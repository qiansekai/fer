//! Real-time change push: `fer monitor` → `fer serve`.
//!
//! ## Why this exists
//!
//! `monitor` keeps the live index in memory and only writes it back to the dump
//! every `--flush-secs` (1800 s by default, because a flush rewrites the whole
//! 1.2+ GB dump — shortening it would mean terabytes/day of writes). `serve`
//! reads that dump, so a file created now only becomes searchable at the next
//! flush: measured **3 minutes to never** in practice. Everything shows new
//! files within seconds, which is the one thing fer cannot match.
//!
//! Rebuilding the whole `MemIndex` every few seconds is not an option (4.5M
//! entries, seconds per rebuild). Instead `monitor` broadcasts the *pending*
//! changes it has already applied in memory, and `serve` keeps them in a tiny
//! overlay that is consulted on top of the dump snapshot.
//!
//! ## Wire format
//!
//! TCP on loopback, one JSON object per line (`\n`-terminated):
//!
//! ```json
//! {"append":[{"p":"D:\\new.txt","d":false,"s":12,"a":4096,"m":0,"c":0,"f":0,"frn":123}],
//!  "remove":["D:\\gone.txt"]}
//! ```
//!
//! `monitor` re-sends the *entire* pending set each round (not just the delta),
//! which makes the receiver idempotent by construction: re-applying a path is a
//! no-op, and a missed message self-heals on the next round. Batches stay small
//! because the pending set is cleared on every flush.
//!
//! ## Failure behaviour
//!
//! Every path here degrades to "no overlay": with no client connected the
//! sender drops the batch, and a receiver that cannot connect just keeps
//! retrying in the background. Neither side ever blocks the other, and `serve`
//! without a live connection behaves exactly as it did before this module
//! existed.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::EntryMeta;

/// Default port for the change feed. 19876 is the HTTP API, so the feed sits
/// next to it; both are loopback-only.
pub const DEFAULT_PUSH_ADDR: &str = "127.0.0.1:19877";

/// One entry in a batch. Short field names keep the wire format compact —
/// these messages are sent every `--interval-secs` (5 s) while changes trickle
/// in, and a busy build tree can produce thousands of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntry {
    /// Full path (drive-letter form, as stored in the index).
    pub p: String,
    pub d: bool,
    pub s: u64,
    pub a: u64,
    pub m: i64,
    pub c: i64,
    pub f: u8,
    #[serde(default)]
    pub frn: Option<u64>,
}

impl AppendEntry {
    pub fn new(path: impl Into<String>, meta: EntryMeta) -> Self {
        Self {
            p: path.into(),
            d: meta.is_dir,
            s: meta.size,
            a: meta.allocated,
            m: meta.mtime,
            c: meta.ctime,
            f: meta.flags,
            frn: meta.frn,
        }
    }

    pub fn meta(&self) -> EntryMeta {
        EntryMeta {
            is_dir: self.d,
            size: self.s,
            allocated: self.a,
            mtime: self.m,
            ctime: self.c,
            flags: self.f,
            frn: self.frn,
        }
    }
}

/// A batch of changes. `append` is the full pending append set, `remove` the
/// full pending removal set — not deltas.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Batch {
    #[serde(default)]
    pub append: Vec<AppendEntry>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.append.is_empty() && self.remove.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Sender side (monitor)
// ---------------------------------------------------------------------------

/// Connected receivers plus the memo of the last broadcast line (the sender
/// side; `FeedState` below is the receiver-side state, a different struct).
///
/// Both live under one mutex on purpose: the accept thread has to invalidate the
/// memo exactly when it registers a receiver, and two locks taken in opposite
/// orders would be a deadlock waiting to happen.
#[derive(Default)]
struct BroadcastState {
    /// Live receiver sockets; a failed write drops its entry.
    clients: Vec<TcpStream>,
    /// The exact JSON line that went out last. The monitor re-sends its whole
    /// pending set every round, so without this an idle monitor would keep
    /// re-serializing and re-sending an identical (potentially multi-megabyte)
    /// batch forever. Comparing against it makes an idle feed silent while a
    /// busy one still goes out every round — the old 30 s large-batch gate
    /// delayed a live overlay by up to half a minute.
    last_sent: Option<String>,
}

/// Broadcasts batches to every connected receiver. Connections are tracked in a
/// small vector of write handles; a receiver that has gone away is dropped on
/// the first failed write. `send` never blocks on a slow peer (writes go to a
/// socket with the default send buffer and a short write timeout).
pub struct Broadcaster {
    state: Arc<Mutex<BroadcastState>>,
    addr: String,
}

/// Send policy for the change feed.
///
/// The monitor re-sends its **entire** pending set every round — that is what
/// makes the receiver idempotent — and the pending set only clears on flush
/// (1800 s by default). Every round the batch is therefore serialized once and
/// compared against the line that went out last: identical content is dropped,
/// and anything new goes out immediately. There is deliberately no size-based
/// throttling: a large pending set is exactly when a live overlay matters most,
/// and the serialization cost is bounded by the same change volume that built
/// the set. A fresh receiver clears the memo, so a reconnect always gets a full
/// snapshot.
impl Broadcaster {
    /// Bind the feed port and start accepting receivers. Returns `None` (with a
    /// log line) when the port is unavailable — the monitor then runs exactly as
    /// before, just without real-time push.
    pub fn bind(addr: &str) -> Option<Self> {
        let listener = match TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[monitor] change feed disabled: cannot bind {addr} ({e})");
                return None;
            }
        };
        let state: Arc<Mutex<BroadcastState>> = Arc::new(Mutex::new(BroadcastState::default()));
        let sink = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let _ = s.set_nodelay(true);
                        let peer = s
                            .peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "?".into());
                        eprintln!("[monitor] change feed: receiver connected from {peer}");
                        // A new receiver has seen nothing yet: forget the memo so
                        // the next round sends it the full snapshot even when the
                        // pending set has not changed since the last broadcast.
                        match sink.lock() {
                            Ok(mut g) => {
                                g.last_sent = None;
                                g.clients.push(s);
                            }
                            Err(_) => continue,
                        }
                    }
                    Err(e) => eprintln!("[monitor] change feed accept failed: {e}"),
                }
            }
        });
        eprintln!("[monitor] change feed listening on {addr}");
        Some(Self { state, addr: addr.to_string() })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn receivers(&self) -> usize {
        self.state.lock().map(|c| c.clients.len()).unwrap_or(0)
    }

    /// Broadcast one batch. Skipped entirely when nobody is listening (an
    /// unconnected monitor pays nothing per round) and when the batch is
    /// byte-identical to the last one sent (an idle monitor must not re-serialize
    /// a large pending set every round).
    pub fn send(&self, batch: &Batch) {
        let n = batch.append.len() + batch.remove.len();
        if n == 0 {
            return;
        }
        let mut state = match self.state.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if state.clients.is_empty() {
            return;
        }
        let mut line = match serde_json::to_string(batch) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[monitor] change feed: serialize failed ({e})");
                return;
            }
        };
        line.push('\n');
        if state.last_sent.as_deref() == Some(line.as_str()) {
            return; // nothing changed since the last broadcast
        }
        state.last_sent = Some(line.clone());
        state.clients.retain_mut(|s| match s.write_all(line.as_bytes()) {
            Ok(()) => true,
            Err(_) => {
                let _ = s.shutdown(Shutdown::Both);
                false
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Receiver side (serve)
// ---------------------------------------------------------------------------

/// Background connector: keeps a TCP session to the monitor's feed and forwards
/// parsed batches over a channel. Reconnects every `RETRY` on failure, so the
/// order in which monitor and serve start does not matter.
pub struct Feed {
    rx: Receiver<Batch>,
    state: Arc<Mutex<FeedState>>,
}

#[derive(Default)]
pub struct FeedState {
    pub connected: bool,
    pub batches: u64,
    pub last_error: Option<String>,
}

const RETRY: Duration = Duration::from_secs(3);

impl Feed {
    /// Start connecting to `addr` in the background. Never fails: an unreachable
    /// monitor is retried forever, and `try_recv` simply yields nothing.
    pub fn connect(addr: &str) -> Self {
        let (tx, rx): (Sender<Batch>, Receiver<Batch>) = channel();
        let state = Arc::new(Mutex::new(FeedState::default()));
        let st = state.clone();
        let addr = addr.to_string();
        thread::spawn(move || loop {
            match TcpStream::connect(&addr) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    if let Ok(mut g) = st.lock() {
                        g.connected = true;
                        g.last_error = None;
                    }
                    eprintln!("[server] change feed connected to {addr}");
                    let reader = BufReader::new(match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => {
                            if let Ok(mut g) = st.lock() {
                                g.connected = false;
                                g.last_error = Some(e.to_string());
                            }
                            thread::sleep(RETRY);
                            continue;
                        }
                    });
                    for line in reader.lines() {
                        match line {
                            Ok(l) if l.trim().is_empty() => continue,
                            Ok(l) => match serde_json::from_str::<Batch>(&l) {
                                Ok(b) => {
                                    if let Ok(mut g) = st.lock() {
                                        g.batches += 1;
                                    }
                                    // A closed channel means the server is gone.
                                    if tx.send(b).is_err() {
                                        return;
                                    }
                                }
                                Err(e) => eprintln!("[server] change feed: bad batch ({e})"),
                            },
                            Err(_) => break,
                        }
                    }
                    if let Ok(mut g) = st.lock() {
                        g.connected = false;
                        g.last_error = Some("connection closed".into());
                    }
                    eprintln!("[server] change feed disconnected; retrying in {RETRY:?}");
                }
                Err(e) => {
                    if let Ok(mut g) = st.lock() {
                        g.connected = false;
                        g.last_error = Some(e.to_string());
                    }
                }
            }
            thread::sleep(RETRY);
        });
        Self { rx, state }
    }

    /// Non-blocking drain of every batch that arrived since the last call.
    pub fn drain(&self) -> Vec<Batch> {
        let mut out = Vec::new();
        while let Ok(b) = self.rx.try_recv() {
            out.push(b);
        }
        out
    }

    pub fn status(&self) -> (bool, u64, Option<String>) {
        match self.state.lock() {
            Ok(g) => (g.connected, g.batches, g.last_error.clone()),
            Err(_) => (false, 0, Some("feed state poisoned".into())),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    /// Bind the feed on a free loopback port and attach one client with a
    /// short read timeout, so "nothing was sent" is observable as a timeout.
    fn feed_and_client() -> (Broadcaster, TcpStream) {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let addr = format!("127.0.0.1:{port}");
        let feed = Broadcaster::bind(&addr).expect("bind change feed");
        let client = TcpStream::connect(&addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while feed.receivers() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(feed.receivers(), 1, "client was not registered");
        (feed, client)
    }

    fn batch_of(names: &[&str]) -> Batch {
        Batch {
            append: names
                .iter()
                .map(|n| AppendEntry::new(*n, EntryMeta::default()))
                .collect(),
            remove: Vec::new(),
        }
    }

    fn wait_for_receivers(feed: &Broadcaster, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while feed.receivers() < want && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(feed.receivers(), want, "receiver was not registered");
    }

    /// The whole point of the feed: a changed pending set goes out at once,
    /// with no size- or time-based throttle. The old policy held a batch of
    /// more than 2000 entries back for up to 30 s — exactly when a live
    /// overlay matters most.
    #[test]
    fn a_changed_batch_is_sent_at_once() {
        let (feed, mut client) = feed_and_client();
        let mut buf = [0u8; 8192];
        feed.send(&batch_of(&[r"D:\a.txt"]));
        assert!(client.read(&mut buf).unwrap() > 0, "the first batch must be sent");
        let big: Vec<String> = (0..2500).map(|i| format!(r"D:\big\{i}.bin")).collect();
        let refs: Vec<&str> = big.iter().map(String::as_str).collect();
        feed.send(&batch_of(&refs));
        let n = client.read(&mut buf).unwrap();
        assert!(n > 0, "a large changed batch must be sent immediately");
    }

    /// An idle monitor must not re-serialize and re-send an identical
    /// snapshot every round; the pending set only clears on flush (up to
    /// --flush-secs, 1800 s on this machine).
    #[test]
    fn an_unchanged_batch_is_not_resent() {
        let (feed, mut client) = feed_and_client();
        let mut buf = [0u8; 8192];
        let batch = batch_of(&[r"D:\a.txt", r"D:\b.txt"]);
        feed.send(&batch);
        assert!(client.read(&mut buf).unwrap() > 0);
        feed.send(&batch);
        let err = client.read(&mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "an unchanged batch must stay on the sender, got {err:?}"
        );
        // ... but a real change still goes out at once.
        feed.send(&batch_of(&[r"D:\a.txt", r"D:\b.txt", r"D:\c.txt"]));
        assert!(client.read(&mut buf).unwrap() > 0);
    }

    /// A receiver that connects after the last broadcast has seen nothing, so
    /// the memo must be dropped on accept — otherwise an unchanged pending
    /// set would never reach it.
    #[test]
    fn a_new_receiver_gets_the_snapshot_even_when_nothing_changed() {
        let (feed, mut first) = feed_and_client();
        let batch = batch_of(&[r"D:\a.txt"]);
        let mut buf = [0u8; 8192];
        feed.send(&batch);
        assert!(first.read(&mut buf).unwrap() > 0);

        let addr = feed.addr().to_string();
        let mut second = TcpStream::connect(&addr).unwrap();
        second
            .set_read_timeout(Some(Duration::from_millis(2000)))
            .unwrap();
        wait_for_receivers(&feed, 2);
        feed.send(&batch);
        assert!(
            second.read(&mut buf).unwrap() > 0,
            "the new receiver must get the snapshot"
        );
    }
}
