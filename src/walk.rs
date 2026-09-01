//! Plain directory-walk indexing fallback (no admin required, slower than MFT).

use walkdir::WalkDir;

use crate::EntryMeta;

/// Scan `root` recursively, calling `on_entry(path, meta)` for every file and
/// directory. Returns the number of unreadable entries skipped.
pub fn scan_tree(root: &str, mut on_entry: impl FnMut(&str, EntryMeta)) -> u64 {
    let mut skipped = 0u64;
    for item in WalkDir::new(root).follow_links(false) {
        match item {
            Ok(de) => {
                if de.depth() == 0 {
                    continue;
                }
                let is_dir = de.file_type().is_dir();
                // On Windows both come from the find-data already held by the
                // iterator — no extra syscalls per entry.
                let md = de.metadata().ok();
                let size = if is_dir { 0 } else { md.as_ref().map(|m| m.len()).unwrap_or(0) };
                let mtime = md
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let ctime = md
                    .as_ref()
                    .and_then(|m| m.created().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let path = de.path().to_string_lossy();
                on_entry(
                    path.as_ref(),
                    EntryMeta { is_dir, size, allocated: 0, mtime, ctime, flags: 0, frn: None },
                );
            }
            Err(_) => skipped += 1,
        }
    }
    skipped
}
