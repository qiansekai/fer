//! Index-quality metadata sidecar (`<dump>.meta`).
//!
//! Kept in its own file instead of in the dump header: the dump stays a frozen
//! binary contract, older `fer` builds keep reading new dumps, and this can
//! evolve freely. It records which method produced the index — so a degraded
//! rebuild cannot silently replace a full-fidelity one — and when it was built.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What produced the index and when. Absent for dumps written before this
/// sidecar existed: callers must read that as "unknown", never as "degraded".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub method: String,
    pub volumes: Vec<String>,
    pub files: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub elapsed_ms: u64,
    pub built_at_unix: u64,
}

impl IndexMeta {
    /// Now, as the sidecar's `built_at_unix`.
    pub fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

pub fn meta_path(dump: &Path) -> PathBuf {
    let mut s = dump.as_os_str().to_os_string();
    s.push(".meta");
    PathBuf::from(s)
}

pub fn write_index_meta(dump: &Path, meta: &IndexMeta) -> Result<()> {
    let path = meta_path(dump);
    std::fs::write(&path, serde_json::to_vec_pretty(meta)?)
        .with_context(|| format!("writing index metadata {}", path.display()))
}

/// Read the sidecar, or `None` when it is missing or unparseable. A missing
/// sidecar is the normal state for dumps written by older builds.
pub fn read_index_meta(dump: &Path) -> Option<IndexMeta> {
    serde_json::from_slice(&std::fs::read(meta_path(dump)).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trip() {
        let dir = std::env::temp_dir().join(format!("fer-meta-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("index.db.feridx");
        assert!(read_index_meta(&dump).is_none());
        let meta = IndexMeta {
            method: "mft".into(),
            volumes: vec!["D:".into()],
            files: 3,
            dirs: 1,
            skipped: 0,
            elapsed_ms: 42,
            built_at_unix: 1_700_000_000,
        };
        write_index_meta(&dump, &meta).unwrap();
        let back = read_index_meta(&dump).expect("round trip");
        assert_eq!(back.method, "mft");
        assert_eq!(back.files, 3);
        assert_eq!(back.built_at_unix, 1_700_000_000);
        assert!(meta_path(&dump).to_string_lossy().ends_with(".feridx.meta"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
