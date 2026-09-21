//! Local control channel for a running `fer monitor`.
//!
//! The monitor holds the authoritative in-memory index but only writes it back
//! to the dump every `--flush-secs` (30 minutes in the default deployment — a
//! deliberate SSD-write trade-off). Everything that reads the dump therefore
//! lags behind reality by up to one flush period: `fer du` keeps reporting
//! directories that were deleted minutes ago, and a CLI search misses files
//! created since the last flush. This channel lets a caller ask the monitor to
//! flush or rebuild *now*.
//!
//! Protocol: one line of text per loopback connection (`flush`, `rebuild`,
//! `status`), answered with one line. The monitor executes the command on its
//! own thread, so the answer reflects real work and the dump it writes cannot
//! be clobbered by a later in-memory flush.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

/// Loopback port the monitor listens on. 19876 is `fer serve`, 19877 is the
/// real-time change feed — this is the third, control-only endpoint.
pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:19878";

/// How long a client waits for the monitor to finish a command. A full rebuild
/// of a large volume takes seconds; the dump write adds a second or two.
const REPLY_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Write the pending in-memory changes to the dump immediately.
    Flush,
    /// Re-scan this monitor's volume from $MFT and rewrite the dump. Repairs
    /// the drift that accumulates between flushes and the gaps the USN journal
    /// can no longer replay.
    Rebuild,
    /// Report what the monitor is currently holding.
    Status,
}

impl Cmd {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "flush" => Some(Cmd::Flush),
            "rebuild" | "reindex" => Some(Cmd::Rebuild),
            "status" => Some(Cmd::Status),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Cmd::Flush => "flush",
            Cmd::Rebuild => "rebuild",
            Cmd::Status => "status",
        }
    }
}

/// One command plus the channel its answer must be sent on.
pub struct Request {
    pub cmd: Cmd,
    pub reply: Sender<String>,
}

/// Monitor side: bind the control port and return the receiver its loop polls.
///
/// Returns `None` when the port cannot be bound (already taken, or an unusable
/// address). A monitor without a control channel still watches the journal — it
/// simply cannot be poked — so the failure is reported and not fatal.
pub fn bind(addr: &str) -> Option<Receiver<Request>> {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[monitor] control channel {addr} unavailable ({e}) — \
                 on-demand flush/rebuild disabled"
            );
            return None;
        }
    };
    let (tx, rx) = channel::<Request>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            // One short-lived thread per connection: a rebuild takes seconds and
            // the monitor must stay pokeable meanwhile.
            thread::spawn(move || handle_client(stream, &tx));
        }
    });
    eprintln!("[monitor] control channel on {addr} (flush | rebuild | status)");
    Some(rx)
}

fn handle_client(mut stream: TcpStream, tx: &Sender<Request>) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut line = String::new();
    if BufReader::new(reader).read_line(&mut line).is_err() {
        return;
    }
    let Some(cmd) = Cmd::parse(&line) else {
        let _ = stream.write_all(b"err: unknown command (flush | rebuild | status)\n");
        return;
    };
    let (rtx, rrx) = channel::<String>();
    if tx.send(Request { cmd, reply: rtx }).is_err() {
        let _ = stream.write_all(b"err: monitor loop is gone\n");
        return;
    }
    let msg = match rrx.recv_timeout(REPLY_TIMEOUT) {
        Ok(msg) => msg,
        Err(RecvTimeoutError::Timeout) => "err: monitor did not answer in time".to_string(),
        Err(RecvTimeoutError::Disconnected) => "err: monitor loop is gone".to_string(),
    };
    let _ = stream.write_all(format!("{msg}\n").as_bytes());
}

/// Client side: send one command and wait for the monitor's answer.
pub fn request(addr: &str, cmd: &str) -> Result<String> {
    let mut stream = TcpStream::connect(addr).with_context(|| {
        format!(
            "no `fer monitor` listening on {addr} — start one, or run `fer index` \
             to rebuild the dump directly"
        )
    })?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.write_all(format!("{cmd}\n").as_bytes())?;
    stream.flush()?;
    let mut out = String::new();
    BufReader::new(stream).read_line(&mut out)?;
    let out = out.trim().to_string();
    if out.is_empty() {
        anyhow::bail!("monitor on {addr} closed the connection without answering");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing() {
        assert_eq!(Cmd::parse(" flush\n"), Some(Cmd::Flush));
        assert_eq!(Cmd::parse("REBUILD"), Some(Cmd::Rebuild));
        assert_eq!(Cmd::parse("reindex"), Some(Cmd::Rebuild));
        assert_eq!(Cmd::parse("status"), Some(Cmd::Status));
        assert_eq!(Cmd::parse("nonsense"), None);
        assert_eq!(Cmd::Flush.as_str(), "flush");
        assert_eq!(Cmd::Rebuild.as_str(), "rebuild");
    }

    #[test]
    fn control_round_trip() {
        // Bind on an ephemeral port, answer one command from a fake loop.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        let rx = bind(&addr).expect("bind");
        let server = thread::spawn(move || {
            let req = rx.recv().expect("a request");
            let _ = req.reply.send(format!("ok: {}", req.cmd.as_str()));
        });
        let answer = request(&addr, "status").expect("request");
        assert_eq!(answer, "ok: status");
        server.join().unwrap();
    }
}
