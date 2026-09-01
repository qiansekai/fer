//! Directory size aggregation (`du`): sums unique file sizes per directory
//! subtree straight from the in-memory index. No disk access — everything is
//! derived from the dump the same way WizTree derives its tree from the MFT.
//!
//! Semantics:
//! * only file entries contribute size (NTFS directory records carry no
//!   meaningful size of their own);
//! * hard links are counted once, by FRN (`by_frn` aliases dedupe);
//! * every file contributes to *all* its ancestor directories, so a parent's
//!   total always equals its own files plus its children;
//! * subtree membership is boundary-aware (`d:\proj` never matches
//!   `d:\proj2`), case-insensitive like the rest of the engine.
//!
//! Performance design (whole volume ≈ 3M entries):
//! * directory lookup is allocation-free: dirs are keyed by an FNV-1a hash
//!   over ASCII-folded path bytes (hash = prefilter, `ci_eq` = exact verify),
//!   and the per-file ancestor walk maintains the hash incrementally in one
//!   byte pass;
//! * aggregation runs on scoped threads over contiguous, evenly sized file
//!   chunks and accumulates into dense per-dir atomic counters (one slot per
//!   directory id): no per-thread maps, no merge phase, balanced load even
//!   when one top-level directory dominates the volume.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::fold_lower;
use crate::mem::MemIndex;

/// FNV-1a (64-bit) constants.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One directory's aggregated size (descendant of the measured root).
#[derive(Debug, Clone, Serialize)]
pub struct DuEntry {
    pub path: String,
    pub size: u64,
    /// Sum of allocated cluster bytes (v6 dumps; equals `size` on pre-v6
    /// dumps and for resident files).
    pub allocated: u64,
    pub files: u64,
    /// Levels below the root (root's direct children = 1).
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuReport {
    /// Root as measured (display casing).
    pub root: String,
    /// Sum of unique file sizes in the subtree.
    pub total_bytes: u64,
    /// Sum of unique allocated cluster bytes in the subtree.
    pub total_allocated: u64,
    /// Unique files counted (hard links once).
    pub files: u64,
    /// Distinct directories below the root.
    pub dirs: u64,
    /// Raw index entries matched (includes hard-link aliases).
    pub entries: u64,
    /// Subdirectories sorted most-space first, capped at `top`.
    pub children: Vec<DuEntry>,
    /// True when more subdirectories existed than `top` allowed.
    pub truncated: bool,
}

/// Aggregate sizes for `root` (a directory, volume root like `D:\`, or a
/// single file). `max_depth` limits reported subdirectories to that many
/// levels below the root (`None` = unlimited, `0` = total only); `top` caps
/// how many are reported (most space first). With `by_allocated`, children
/// sort by allocated cluster bytes instead of logical size (both totals are
/// always reported).
pub fn scan(
    mem: &MemIndex,
    root: &str,
    max_depth: Option<usize>,
    top: usize,
    by_allocated: bool,
) -> Result<DuReport> {
    let norm = root.trim_end_matches(['\\', '/']);
    if norm.is_empty() {
        bail!("du: empty root path");
    }
    let root_lc = fold_lower(norm);
    let root_bytes = root_lc.as_bytes();

    let ids = mem.subtree_ids(root_bytes);

    // --- Pass A: FRN dedup + directory index. ---
    let mut seen: HashSet<u64> = HashSet::with_capacity(ids.len());
    let mut dir_hash: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut uniq: Vec<(u32, u64, u64)> = Vec::new();
    let mut total: u64 = 0;
    let mut total_allocated: u64 = 0;
    let mut files: u64 = 0;
    let mut dirs_seen: u64 = 0;
    let mut dir_max: u32 = 0;
    let mut root_present = false;

    for &id in &ids {
        let meta = mem.meta_at(id as usize);
        if meta.is_dir {
            let raw = mem.path_bytes(id as usize);
            dir_hash.entry(ci_fnv(raw)).or_default().push(id);
            dirs_seen += 1;
            dir_max = dir_max.max(id);
            root_present |= ci_eq(raw, root_bytes);
            continue;
        }
        // FRN keys get the top bit set; entries without an FRN key on their
        // entry id (ids < 2^32 < 2^63), so the key spaces never collide.
        let key = meta.frn.map(|f| f | (1 << 63)).unwrap_or(id as u64);
        if !seen.insert(key) {
            continue; // hard-link alias of an already counted file
        }
        total = total.saturating_add(meta.size);
        total_allocated = total_allocated.saturating_add(meta.allocated);
        files += 1;
        uniq.push((id, meta.size, meta.allocated));
    }

    // --- Pass B: parallel aggregation into dense per-dir atomic counters. ---
    // One slot per directory id — at most the largest dir id in the subtree,
    // so small subtrees stay small. Contiguous chunks keep every thread busy
    // even when one top-level directory dominates the volume.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);
    let dir_sizes: Vec<AtomicU64> = (0..=dir_max).map(|_| AtomicU64::new(0)).collect();
    let dir_counts: Vec<AtomicU64> = (0..=dir_max).map(|_| AtomicU64::new(0)).collect();
    let dir_allocs: Vec<AtomicU64> = (0..=dir_max).map(|_| AtomicU64::new(0)).collect();
    if uniq.len() < 4096 {
        aggregate(&uniq, &dir_hash, mem, &dir_sizes, &dir_counts, &dir_allocs);
    } else {
        let chunk = uniq.len().div_ceil(threads).max(1);
        std::thread::scope(|s| {
            let handles: Vec<_> = uniq
                .chunks(chunk)
                .map(|c| {
                    s.spawn(|| aggregate(c, &dir_hash, mem, &dir_sizes, &dir_counts, &dir_allocs))
                })
                .collect();
            for h in handles {
                h.join().expect("du aggregation thread panicked");
            }
        });
    }

    // --- Assemble the child list: distinct dirs below the root, depth
    // limited, most space first. ---
    let mut children: Vec<DuEntry> = Vec::new();
    for cands in dir_hash.values() {
        for &did in cands {
            let size = dir_sizes[did as usize].load(Relaxed);
            let allocated = dir_allocs[did as usize].load(Relaxed);
            let files = dir_counts[did as usize].load(Relaxed);
            if size == 0 && allocated == 0 && files == 0 {
                continue;
            }
            let display = mem.path_at(did as usize);
            let raw = display.as_bytes();
            let rel = &raw[root_bytes.len()..];
            let depth = rel
                .iter()
                .filter(|&&b| b == b'\\' || b == b'/')
                .count();
            if depth == 0 {
                continue; // the measured root itself
            }
            children.push(DuEntry {
                path: display,
                size,
                allocated,
                files,
                depth,
            });
        }
    }
    children.sort_unstable_by(|a, b| {
        let (ka, kb) = if by_allocated {
            (a.allocated, b.allocated)
        } else {
            (a.size, b.size)
        };
        kb.cmp(&ka).then_with(|| a.path.cmp(&b.path))
    });
    let truncated = children.len() > top;
    if let Some(max_depth) = max_depth {
        children.retain(|c| c.depth <= max_depth);
    }
    children.truncate(top);

    Ok(DuReport {
        root: norm.to_string(),
        total_bytes: total,
        total_allocated,
        files,
        dirs: dirs_seen - root_present as u64,
        entries: ids.len() as u64,
        children,
        truncated,
    })
}

/// Aggregate one chunk: attribute every file to all its ancestor directories
/// (all prefixes ending at a separator; the volume root is implicit and the
/// measured root is skipped later). The FNV hash is maintained incrementally
/// over the raw path bytes — one pass, zero allocation per file. Accumulation
/// is a relaxed atomic add into the dense per-dir counters (different threads
/// writing the same directory is rare and harmless: add is commutative).
fn aggregate(
    chunk: &[(u32, u64, u64)],
    dir_hash: &HashMap<u64, Vec<u32>>,
    mem: &MemIndex,
    dir_sizes: &[AtomicU64],
    dir_counts: &[AtomicU64],
    dir_allocs: &[AtomicU64],
) {
    for &(id, size, allocated) in chunk {
        let raw = mem.path_bytes(id as usize);
        let mut h = FNV_OFFSET;
        let mut i = 0usize;
        for &b in raw {
            // Hash of raw[..i] — the prefix WITHOUT the current byte. The
            // separator itself is not part of any directory path, so the
            // lookup key must be the hash from before this byte.
            let before = h;
            h ^= b.to_ascii_lowercase() as u64;
            h = h.wrapping_mul(FNV_PRIME);
            i += 1;
            if (b == b'\\' || b == b'/')
                && let Some(did) = lookup_dir(dir_hash, mem, &raw[..i - 1], before)
            {
                dir_sizes[did as usize].fetch_add(size, Relaxed);
                dir_counts[did as usize].fetch_add(1, Relaxed);
                dir_allocs[did as usize].fetch_add(allocated, Relaxed);
            }
        }
    }
}

/// Directory lookup by folded-path hash: the hash is only a prefilter, exact
/// equality re-checks the bytes (ASCII-folded) — hash collisions cannot
/// corrupt results.
fn lookup_dir(map: &HashMap<u64, Vec<u32>>, mem: &MemIndex, prefix: &[u8], hash: u64) -> Option<u32> {
    let cands = map.get(&hash)?;
    cands
        .iter()
        .copied()
        .find(|&did| ci_eq(mem.path_bytes(did as usize), prefix))
}

/// FNV-1a over ASCII-folded bytes (non-ASCII compares raw, matching the
/// engine-wide CI contract).
fn ci_fnv(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b.to_ascii_lowercase() as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// ASCII-case-insensitive byte equality (non-ASCII raw), engine contract.
fn ci_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryMeta;
    use crate::mem::MemBuilder;

    fn file(size: u64, frn: u64) -> EntryMeta {
        EntryMeta { size, allocated: size, frn: Some(frn), ..Default::default() }
    }

    /// A file whose allocated clusters differ from its logical size (sparse
    /// or compressed): allocated = `alloc`.
    fn file_alloc(size: u64, alloc: u64, frn: u64) -> EntryMeta {
        EntryMeta { size, allocated: alloc, frn: Some(frn), ..Default::default() }
    }

    fn dir() -> EntryMeta {
        EntryMeta { is_dir: true, ..Default::default() }
    }

    fn idx() -> MemIndex {
        let mut b = MemBuilder::default();
        b.push(r"D:\proj", dir());
        b.push(r"D:\proj\src", dir());
        b.push(r"D:\proj\src\main.rs", file(100, 1));
        b.push(r"D:\proj\src\lib.rs", file(50, 2));
        b.push(r"D:\proj\alias.rs", file(100, 1)); // hard link to main.rs
        b.push(r"D:\proj2", dir());
        b.push(r"D:\proj2\x.txt", file(999, 3)); // boundary trap
        b.push(r"D:\proj\sub", dir()); // empty dir
        b.finish()
    }

    #[test]
    fn totals_dedupe_and_boundary() {
        let mem = idx();
        let r = scan(&mem, r"D:\proj", None, 100, false).unwrap();
        assert_eq!(r.total_bytes, 150); // hard link counted once: 100 + 50
        assert_eq!(r.total_allocated, 150); // allocated mirrors size in fixtures
        assert_eq!(r.files, 2);
        assert_eq!(r.entries, 6); // proj, src, main, lib, alias, sub
        assert_eq!(r.dirs, 2); // src + sub (root excluded)
        assert!(!r.truncated);
        // proj2 must not leak in
        assert!(!r.children.iter().any(|c| c.path.to_lowercase().contains("proj2")));
        // src aggregates everything below it
        let src = r.children.iter().find(|c| c.path.ends_with("src")).unwrap();
        assert_eq!(src.size, 150);
        assert_eq!(src.allocated, 150);
        assert_eq!(src.files, 2);
        assert_eq!(src.depth, 1);
    }

    #[test]
    fn case_insensitive_root() {
        let mem = idx();
        let a = scan(&mem, r"D:\PROJ", None, 100, false).unwrap();
        let b = scan(&mem, r"d:\proj", None, 100, false).unwrap();
        assert_eq!(a.total_bytes, b.total_bytes);
        assert_eq!(a.files, b.files);
        assert_eq!(a.children.len(), b.children.len());
    }

    #[test]
    fn volume_root_and_depth() {
        let mem = idx();
        let r = scan(&mem, r"D:\", None, 100, false).unwrap();
        assert_eq!(r.total_bytes, 1149); // 100 + 50 + 999
        assert_eq!(r.files, 3);
        // proj (150) sits at depth 1, src at depth 2
        let proj = r.children.iter().find(|c| c.path.ends_with("proj")).unwrap();
        assert_eq!(proj.size, 150);
        assert_eq!(proj.depth, 1);
        let src = r.children.iter().find(|c| c.path.ends_with("src")).unwrap();
        assert_eq!(src.depth, 2);

        let d1 = scan(&mem, r"D:\", Some(1), 100, false).unwrap();
        assert!(d1.children.iter().all(|c| c.depth <= 1));
        assert!(d1.children.iter().any(|c| c.path.ends_with("proj")));
        assert!(!d1.children.iter().any(|c| c.path.ends_with("src")));

        let d0 = scan(&mem, r"D:\", Some(0), 100, false).unwrap();
        assert!(d0.children.is_empty());
        assert_eq!(d0.total_bytes, 1149); // total survives depth 0
    }

    #[test]
    fn top_cap_and_truncation() {
        let mem = idx();
        let r = scan(&mem, r"D:\", None, 1, false).unwrap();
        assert_eq!(r.children.len(), 1);
        assert!(r.truncated);
        assert_eq!(r.children[0].path.to_lowercase(), "d:\\proj2"); // biggest first
    }

    #[test]
    fn allocated_sort_and_resident_files() {
        // A resident file (allocated = 0) and a sparse file (allocated < size)
        // exercise the dual-metric path.
        let mut b = MemBuilder::default();
        b.push(r"D:\s", dir());
        b.push(r"D:\s\resident.txt", EntryMeta { size: 30, allocated: 0, frn: Some(1), ..Default::default() });
        b.push(r"D:\s\sparse.bin", file_alloc(1000, 400, 2));
        b.push(r"D:\t", dir());
        b.push(r"D:\t\plain.bin", file_alloc(500, 500, 3));
        let mem = b.finish();

        let by_size = scan(&mem, r"D:\", None, 100, false).unwrap();
        assert_eq!(by_size.total_bytes, 1530);
        assert_eq!(by_size.total_allocated, 900); // 0 + 400 + 500
        // logical size order: sparse.bin's dir (s) first
        assert_eq!(by_size.children[0].path.to_lowercase(), "d:\\s");

        let by_alloc = scan(&mem, r"D:\", None, 100, true).unwrap();
        // allocated order: t (500) beats s (400)
        assert_eq!(by_alloc.children[0].path.to_lowercase(), "d:\\t");
        assert_eq!(by_alloc.total_allocated, 900);
        assert_eq!(by_alloc.total_bytes, 1530); // totals identical either way
    }

    #[test]
    fn single_file_root() {
        let mem = idx();
        let r = scan(&mem, r"D:\proj\src\main.rs", None, 100, false).unwrap();
        assert_eq!(r.total_bytes, 100);
        assert_eq!(r.total_allocated, 100);
        assert_eq!(r.files, 1);
        assert_eq!(r.dirs, 0);
        assert_eq!(r.entries, 1);
        assert!(r.children.is_empty());
    }

    #[test]
    fn missing_root_is_empty() {
        let mem = idx();
        let r = scan(&mem, r"D:\nowhere", None, 100, false).unwrap();
        assert_eq!(r.total_bytes, 0);
        assert_eq!(r.entries, 0);
        assert!(r.children.is_empty());
    }

    #[test]
    fn trailing_separator_root() {
        let mem = idx();
        let a = scan(&mem, r"D:\proj\", None, 100, false).unwrap();
        let b = scan(&mem, r"D:\proj", None, 100, false).unwrap();
        assert_eq!(a.total_bytes, b.total_bytes);
        assert_eq!(a.children.len(), b.children.len());
    }

    #[test]
    fn hard_link_across_top_dirs() {
        // One FRN aliased under two top-level directories: counted once and
        // attributed to the first-seen alias's chain (id order).
        let mut b = MemBuilder::default();
        b.push(r"D:\a", dir());
        b.push(r"D:\a\f.txt", file(10, 7));
        b.push(r"D:\b", dir());
        b.push(r"D:\b\g.txt", file(10, 7)); // alias
        let mem = b.finish();
        let r = scan(&mem, r"D:\", None, 100, false).unwrap();
        assert_eq!(r.total_bytes, 10);
        assert_eq!(r.files, 1);
        assert_eq!(r.children.len(), 1); // only "a" holds the file
        assert!(r.children[0].path.ends_with("a"));
    }

    #[test]
    fn hashing_and_partition_primitives() {
        // The walk keeps the hash of the prefix WITHOUT the current byte;
        // at each separator that must equal ci_fnv over the path so far.
        let raw = br"d:\proj\src\main.rs";
        let mut h = FNV_OFFSET;
        for (i, &b) in raw.iter().enumerate() {
            let before = h;
            h ^= b.to_ascii_lowercase() as u64;
            h = h.wrapping_mul(FNV_PRIME);
            if i == 7 || i == 11 {
                assert_eq!(before, ci_fnv(&raw[..i]));
            }
        }
        assert!(ci_eq(br"d:\proj", br"D:\PROJ"));
        assert!(!ci_eq(br"d:\proj", br"d:\proj2"));
    }
}
