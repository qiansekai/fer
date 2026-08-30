//! Duplicate-file finder: same-size groups from the in-memory index, then
//! content hash (FNV-1a streaming) + byte-verification to confirm duplicates.

use anyhow::Result;
use serde::Serialize;

use crate::mem::MemIndex;

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
    mem: &MemIndex,
    min_size: u64,
    name_filter: Option<&str>,
    limit: usize,
    mut progress: impl FnMut(u64),
) -> Result<DupReport> {
    let mut report = DupReport::default();
    let filter_l = name_filter.map(|f| f.to_lowercase());

    // Collect candidates (path, size), sorted by size so each same-size group
    // is processed with bounded memory.
    let mut group: Vec<(String, u64)> = Vec::new();
    for i in 0..mem.len() {
        let meta = mem.meta_at(i);
        if meta.is_dir || meta.size < min_size {
            continue;
        }
        if let Some(f) = &filter_l {
            let name = crate::basename(mem.path_at(i).as_str()).to_lowercase();
            if !name.contains(f.as_str()) {
                continue;
            }
        }
        group.push((mem.path_at(i), meta.size));
    }
    group.sort_unstable_by_key(|(_, s)| *s);

    let mut i = 0;
    while i < group.len() {
        let size = group[i].1;
        let mut j = i + 1;
        while j < group.len() && group[j].1 == size {
            j += 1;
        }
        if j - i >= 2 {
            let mut hashed: Vec<(String, u64)> = Vec::with_capacity(j - i);
            for (path, _) in &group[i..j] {
                match hash_file(path) {
                    Ok(h) => hashed.push((path.clone(), h)),
                    Err(_) => report.skipped += 1,
                }
                report.files_hashed += 1;
                progress(report.files_hashed);
            }
            hashed.sort_unstable_by_key(|(_, h)| *h);
            let mut a = 0;
            while a < hashed.len() {
                let h = hashed[a].1;
                let mut b = a + 1;
                while b < hashed.len() && hashed[b].1 == h {
                    b += 1;
                }
                if b - a >= 2 {
                    // Byte-verify against the first candidate.
                    let mut paths: Vec<String> = Vec::with_capacity(b - a);
                    for (path, _) in &hashed[a..b] {
                        if paths.is_empty() || files_equal(&hashed[a].0, path) {
                            paths.push(path.clone());
                        }
                    }
                    if paths.len() >= 2 {
                        let wasted = size * (paths.len() as u64 - 1);
                        report.wasted_bytes += wasted;
                        report.groups.push(DupGroup { size, paths });
                    }
                }
                a = b;
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
    Ok(report)
}

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit hash step over one chunk, folded into the running state
/// (streaming-friendly: call per read buffer).
fn fnv1a_update(mut h: u64, bytes: &[u8]) -> u64 {
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
    let mut h = FNV_OFFSET;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h = fnv1a_update(h, &buf[..n]);
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
    use crate::mem::MemBuilder;

    fn build_mem(dir: &std::path::Path) -> MemIndex {
        let mut b = MemBuilder::default();
        b.push(
            &dir.join("a.txt").to_string_lossy(),
            EntryMeta { size: 4096, ..Default::default() },
        );
        b.push(
            &dir.join("b.txt").to_string_lossy(),
            EntryMeta { size: 4096, ..Default::default() },
        );
        b.push(
            &dir.join("c.txt").to_string_lossy(),
            EntryMeta { size: 4096, ..Default::default() },
        );
        b.push(
            &dir.join("d.txt").to_string_lossy(),
            EntryMeta { size: 8192, ..Default::default() },
        );
        b.finish()
    }

    #[test]
    fn finds_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.path().join("b.txt"), vec![7u8; 4096]).unwrap(); // duplicate of a
        std::fs::write(dir.path().join("c.txt"), vec![9u8; 4096]).unwrap(); // same size, different
        std::fs::write(dir.path().join("d.txt"), vec![7u8; 8192]).unwrap(); // different size

        let mem = build_mem(dir.path());
        let report = find(&mem, 0, None, 10, |_| {}).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].size, 4096);
        assert_eq!(report.groups[0].paths.len(), 2);
        assert_eq!(report.wasted_bytes, 4096);
        // only same-size groups of >=2 are hashed; d.txt's group is a singleton
        assert_eq!(report.files_hashed, 3);

        // name filter narrows to nothing
        let report = find(&mem, 0, Some("zzz"), 10, |_| {}).unwrap();
        assert_eq!(report.groups.len(), 0);
    }

    #[test]
    fn fnv_update_basics() {
        let a = fnv1a_update(FNV_OFFSET, b"a");
        let b = fnv1a_update(FNV_OFFSET, b"b");
        assert_ne!(a, b);
        // streaming chunks fold to the same value as one buffer
        let ab = fnv1a_update(FNV_OFFSET, b"ab");
        assert_eq!(fnv1a_update(a, b"b"), ab);
    }
}
