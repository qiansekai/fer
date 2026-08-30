//! File-Engine-Rust: an Everything-grade instant file search engine, rewritten in Rust.
//!
//! * `mft`     — raw $MFT scanner: hard-link aliases, size, timestamps, flags
//! * `usn`     — NTFS USN/MFT enumeration (fallback + change journal)
//! * `walk`    — plain directory-walk fallback (no admin required)
//! * `store`   — SQLite + FTS5 trigram persistent index, millisecond queries
//! * `query`   — filter query language (`ext: size: dm: parent:` …)
//! * `matcher` — case-insensitive substring / wildcard matching semantics
//! * `monitor` — USN journal polling to keep the index live
//! * `server`  — HTTP API (axum) with a minimal web UI

pub mod indexer;
pub mod matcher;
pub mod mem;
pub mod mft;
pub mod monitor;
pub mod query;
pub mod server;
pub mod store;
pub mod usn;
pub mod walk;

/// NTFS root directory file reference number (MFT record 5).
pub const ROOT_FRN: u64 = 5;
/// Mask that strips the sequence number from a 64-bit file reference number.
pub const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Per-entry metadata flowing through the index pipeline.
#[derive(Debug, Clone, Copy, Default)]
pub struct EntryMeta {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64, // unix seconds (0 = unknown)
    pub ctime: i64, // unix seconds (0 = unknown)
    /// bit0 hidden, bit1 system, bit2 readonly, bit3 reparse
    pub flags: u8,
    /// NTFS file reference number (USN/MFT paths; used by the monitor).
    pub frn: Option<u64>,
}

impl EntryMeta {
    pub const FLAG_HIDDEN: u8 = 1;
    pub const FLAG_SYSTEM: u8 = 2;
    pub const FLAG_READONLY: u8 = 4;
    pub const FLAG_REPARSE: u8 = 8;

    pub fn hidden(&self) -> bool {
        self.flags & Self::FLAG_HIDDEN != 0
    }
    pub fn system(&self) -> bool {
        self.flags & Self::FLAG_SYSTEM != 0
    }
    pub fn readonly(&self) -> bool {
        self.flags & Self::FLAG_READONLY != 0
    }
    pub fn reparse(&self) -> bool {
        self.flags & Self::FLAG_REPARSE != 0
    }
}

/// Result of a full index build.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BuildReport {
    pub method: String,
    pub volumes: Vec<String>,
    pub files: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub elapsed_ms: u128,
    pub max_usn: i64,
}

/// Basename of a Windows/Linux style path.
pub fn basename(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}
