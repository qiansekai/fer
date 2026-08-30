//! Duplicate-file finder: same-size groups from the index, then content hash
//! (FNV-1a streaming) + byte-verification to confirm true duplicates.

use anyhow::Result;
use serde::Serialize;

use crate::store::Store;

#[derive(Debug, Clone, Serialize)]
pub struct DupGroup {
    pub size: u64,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DupReport {
    pub groups: Vec<DupGroup>,
    pub wasted_bytes: u64,
    pub files_hashed: u64,
    pub skipped: u64,
}

/// Find duplicate files (identical content, same size) among files at least
/// `min_size` bytes, optionally narrowed to a name substring. Groups are
/// returned most-wasteful first, capped at `limit`.
pub fn find(
    store: &Store,
    min_size: u64,
    name_filter: Option<&str>,
    limit: usize,
    mut progress: impl FnMut(u64),
) -> Result<DupReport> {
    let mut report = DupReport::default();
    let conn = store.conn();
    // Stream in size order so each same-size group is processed with bounded
    // memory (group size is small in practice).
    let sql = match name_filter {
        Some(f) => format!(
            "SELECT path, size FROM files WHERE is_dir = 0 AND size >= ?1 AND instr(name_l, '{}') > 0 ORDER BY size, path",
            crate::store::sql_literal(&f.to_lowercase())
        ),
        None => "SELECT path, size FROM files WHERE is_dir = 0 AND size >= ?1 ORDER BY size, path"
            .to_string(),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([min_size as i64])?;

    let mut group: Vec<(String, u64)> = Vec::new();
    let mut cur_size: u64 = u64::MAX;
    let flush = |report: &mut DupReport,
                 group: &mut Vec<(String, u64)>,
                 limit: usize,
                 progress: &mut dyn FnMut(u64)| {
        if group.len() < 2 {
            group.clear();
            return;
        }
        let size = group[0].1;
        let mut hashed: Vec<(String, u64)> = Vec::with_capacity(group.len());
        for (path, _) in group.drain(..) {
            match hash_file(&path) {
                Ok(h) => hashed.push((path, h)),
                Err(_) => report.skipped += 1,
            }
            report.files_hashed += 1;
            progress(report.files_hashed);
        }
        hashed.sort_unstable_by_key(|(_, h)| *h);
        let mut i = 0;
        while i < hashed.len() {
            let h = hashed[i].1;
            let mut j = i + 1;
            while j < hashed.len() && hashed[j].1 == h {
                j += 1;
            }
            if j - i >= 2 {
                // Byte-verify against the first candidate (streaming compare).
                let mut paths: Vec<String> = Vec::with_capacity(j - i);
                for (path, _) in &hashed[i..j] {
                    if paths.is_empty() || files_equal(&hashed[i].0, path) {
                        paths.push(path.clone());
                    }
                }
                if paths.len() >= 2 {
                    let wasted = size * (paths.len() as u64 - 1);
                    report.wasted_bytes += wasted;
                    report.groups.push(DupGroup { size, paths });
                }
            }
            i = j;
        }
        report.groups.sort_unstable_by(|a, b| {
            let wa = b.size * (b.paths.len() as u64 - 1);
            let wb = a.size * (a.paths.len() as u64 - 1);
            wa.cmp(&wb)
        });
        report.groups.truncate(limit);
    };

    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let size: i64 = row.get(1)?;
        let size = size as u64;
        if size != cur_size {
            flush(&mut report, &mut group, limit, &mut progress);
            cur_size = size;
        }
        group.push((path, size));
    }
    flush(&mut report, &mut group, limit, &mut progress);
    Ok(report)
}

/// FNV-1a 64-bit streaming hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_file(path: &str) -> Result<u64> {
    let mut file = std::fs::File::open(path)?;
    use std::io::Read;
    let mut buf = vec![0u8; 1 << 20];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(h)
}

/// Streaming byte-compare of two files (returns false on any difference/error).
fn files_equal(a: &str, b: &str) -> bool {
    use std::io::Read;
    let (mut fa, mut fb) = match (std::fs::File::open(a), std::fs::File::open(b)) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return false,
    };
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        let na = fa.read(&mut ba).unwrap_or(0);
        let nb = fb.read(&mut bb).unwrap_or(0);
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryMeta;
    use crate::walk::scan_tree;

    #[test]
    fn finds_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.path().join("b.txt"), vec![7u8; 4096]).unwrap(); // duplicate of a
        std::fs::write(dir.path().join("c.txt"), vec![9u8; 4096]).unwrap(); // same size, different
        std::fs::write(dir.path().join("d.txt"), vec![7u8; 8192]).unwrap(); // different size

        let dbdir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dbdir.path().join("t.db")).unwrap();
        let mut rb = store.begin_rebuild().unwrap();
        scan_tree(dir.path().to_str().unwrap(), |p, m: EntryMeta| {
            rb.insert(p, m).unwrap();
        });
        rb.commit().unwrap();

        let report = find(&store, 0, None, 10, |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].size, 4096);
        assert_eq!(report.groups[0].paths.len(), 2);
        assert_eq!(report.wasted_bytes, 4096);
        // only same-size groups of >=2 are hashed; d.txt's group is a singleton
        assert_eq!(report.files_hashed, 3);

        // name filter narrows to nothing
        let report = find(&store, 0, Some("zzz"), 10, |_| {}).unwrap();
        assert_eq!(report.groups.len(), 0);
    }

    #[test]
    fn fnv_basics() {
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_eq!(fnv1a(b"same"), fnv1a(b"same"));
    }
}
