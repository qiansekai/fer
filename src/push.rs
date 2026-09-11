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
use std::time::{Duration, Instant};

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

/// Broadcasts batches to every connected receiver. Connections are tracked in a
/// small vector of write handles; a receiver that has gone away is dropped on
/// the first failed write. `send` never blocks on a slow peer (writes go to a
/// socket with the default send buffer and a short write timeout).
pub struct Broadcaster {
    clients: Arc<Mutex<Vec<TcpStream>>>,
    addr: String,
    /// When the last *large* batch actually went out (throttling, see below).
    last_big_send: Mutex<Instant>,
}

/// Throttling policy for the change feed.
///
/// The monitor re-sends its **entire** pending set every round — that is what
/// makes the receiver idempotent — and the pending set only clears on flush
/// (1800 s by default). Small sets are cheap and must go out immediately (that
/// is the whole point of the feed), but a large one is expensive to re-serialize
/// every `--interval-secs`: `serde_json` allocates a fresh multi-megabyte
/// `String` each round. So send eagerly below `BIG_BATCH` and throttle above it.
/// The receiver stays correct either way — it is fed a full snapshot, just less
/// often.
const BIG_BATCH: usize = 2_000;
/// Minimum gap between two large broadcasts.
const BIG_GAP: Duration = Duration::from_secs(30);

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
        let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = clients.clone();
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
                        sink.lock().unwrap().push(s);
                    }
                    Err(e) => eprintln!("[monitor] change feed accept failed: {e}"),
                }
            }
        });
        eprintln!("[monitor] change feed listening on {addr}");
        Some(Self {
            clients,
            addr: addr.to_string(),
            last_big_send: Mutex::new(Instant::now() - BIG_GAP),
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn receivers(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Broadcast one batch. Skipped entirely when nobody is listening, so an
    /// unconnected monitor pays nothing per round. Large batches are throttled
    /// (see `BIG_BATCH` / `BIG_GAP`).
    pub fn send(&self, batch: &Batch) {
        let n = batch.append.len() + batch.remove.len();
        if n == 0 {
            return;
        }
        let mut clients = match self.clients.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if clients.is_empty() {
            return;
        }
        if n >= BIG_BATCH {
            match self.last_big_send.lock() {
                Ok(mut last) => {
                    if last.elapsed() < BIG_GAP {
                        return; // too soon for an expensive batch; a later round carries it
                    }
                    *last = Instant::now();
                }
                Err(_) => return,
            }
        }
        let mut line = match serde_json::to_string(batch) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[monitor] change feed: serialize failed ({e})");
                return;
            }
        };
        line.push('\n');
        clients.retain_mut(|s| match s.write_all(line.as_bytes()) {
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
