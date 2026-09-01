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

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::fold_lower;
use crate::mem::MemIndex;

/// One directory's aggregated size (descendant of the measured root).
#[derive(Debug, Clone, Serialize)]
pub struct DuEntry {
    pub path: String,
    pub size: u64,
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
/// how many are reported (most space first).
pub fn scan(mem: &MemIndex, root: &str, max_depth: Option<usize>, top: usize) -> Result<DuReport> {
    let norm = root.trim_end_matches(['\\', '/']);
    if norm.is_empty() {
        bail!("du: empty root path");
    }
    let root_lc = fold_lower(norm);
    let root_bytes = root_lc.as_bytes();

    let ids = mem.subtree_ids(root_bytes);

    // Directory index: lowercased path → entry id (only within the subtree).
    let mut dir_map: HashMap<String, u32> = HashMap::new();
    let mut dir_display: HashMap<u32, String> = HashMap::new();
    for &id in &ids {
        let meta = mem.meta_at(id as usize);
        if meta.is_dir {
            let display = mem.path_at(id as usize);
            dir_map.insert(fold_lower_bytes(mem.path_bytes(id as usize)), id);
            dir_display.insert(id, display);
        }
    }

    // Per-directory aggregation, keyed by entry id.
    let mut dir_size: HashMap<u32, u64> = HashMap::new();
    let mut dir_files: HashMap<u32, u64> = HashMap::new();
    // FRN dedupe: count each hard-linked file once. FRN keys get the top bit
    // set; entries without an FRN key on their entry id (ids < 2^32 < 2^63),
    // so the two key spaces never collide.
    let mut seen: HashSet<u64> = HashSet::new();
    let mut total: u64 = 0;
    let mut files: u64 = 0;

    for &id in &ids {
        let meta = mem.meta_at(id as usize);
        if meta.is_dir {
            continue;
        }
        let key = meta.frn.map(|f| f | (1 << 63)).unwrap_or(id as u64);
        if !seen.insert(key) {
            continue; // hard-link alias of an already counted file
        }
        total = total.saturating_add(meta.size);
        files += 1;

        // Attribute this file to every ancestor directory (all prefixes
        // ending at a separator; the volume root itself is implicit and the
        // measured root gets the remainder).
        let lc = fold_lower_bytes(mem.path_bytes(id as usize));
        for (pos, &b) in lc.as_bytes().iter().enumerate() {
            if pos > 0
                && (b == b'\\' || b == b'/')
                && let Some(&did) = dir_map.get(&lc[..pos])
            {
                let slot = dir_size.entry(did).or_insert(0);
                *slot = slot.saturating_add(meta.size);
                *dir_files.entry(did).or_insert(0) += 1;
            }
        }
    }

    // Assemble the child list: distinct directories below the root, depth
    // limited, most space first.
    let mut children: Vec<DuEntry> = Vec::with_capacity(dir_size.len());
    for (&did, &size) in &dir_size {
        let files = dir_files[&did];
        if size == 0 && files == 0 {
            continue;
        }
        let Some(display) = dir_display.get(&did) else {
            continue;
        };
        let lc = fold_lower(display);
        let rel = &lc.as_bytes()[root_bytes.len()..];
        let depth = rel
            .iter()
            .filter(|&&b| b == b'\\' || b == b'/')
            .count();
        if depth == 0 {
            continue; // the measured root itself
        }
        children.push(DuEntry {
            path: display.clone(),
            size,
            files,
            depth,
        });
    }
    children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    let truncated = children.len() > top;
    if let Some(max_depth) = max_depth {
        children.retain(|c| c.depth <= max_depth);
    }
    children.truncate(top);

    let root_present = dir_map.contains_key(&root_lc);
    Ok(DuReport {
        root: norm.to_string(),
        total_bytes: total,
        files,
        dirs: dir_map.len() as u64 - root_present as u64,
        entries: ids.len() as u64,
        children,
        truncated,
    })
}

/// Lowercase path bytes without an intermediate UTF-8 round-trip on the
/// (overwhelmingly ASCII) fast path.
fn fold_lower_bytes(b: &[u8]) -> String {
    if b.is_ascii() {
        // ASCII lowercase is byte-length preserving, so the result is valid
        // UTF-8 by construction.
        String::from_utf8(b.iter().map(u8::to_ascii_lowercase).collect())
            .expect("ascii-lowered bytes are valid utf8")
    } else {
        fold_lower(&String::from_utf8_lossy(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryMeta;
    use crate::mem::MemBuilder;

    fn file(size: u64, frn: u64) -> EntryMeta {
        EntryMeta { size, frn: Some(frn), ..Default::default() }
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
        let r = scan(&mem, r"D:\proj", None, 100).unwrap();
        assert_eq!(r.total_bytes, 150); // hard link counted once: 100 + 50
        assert_eq!(r.files, 2);
        assert_eq!(r.entries, 6); // proj, src, main, lib, alias, sub
        assert_eq!(r.dirs, 2); // src + sub (root excluded)
        assert!(!r.truncated);
        // proj2 must not leak in
        assert!(!r.children.iter().any(|c| c.path.to_lowercase().contains("proj2")));
        // src aggregates everything below it
        let src = r.children.iter().find(|c| c.path.ends_with("src")).unwrap();
        assert_eq!(src.size, 150);
        assert_eq!(src.files, 2);
        assert_eq!(src.depth, 1);
    }

    #[test]
    fn case_insensitive_root() {
        let mem = idx();
        let a = scan(&mem, r"D:\PROJ", None, 100).unwrap();
        let b = scan(&mem, r"d:\proj", None, 100).unwrap();
        assert_eq!(a.total_bytes, b.total_bytes);
        assert_eq!(a.files, b.files);
        assert_eq!(a.children.len(), b.children.len());
    }

    #[test]
    fn volume_root_and_depth() {
        let mem = idx();
        let r = scan(&mem, r"D:\", None, 100).unwrap();
        assert_eq!(r.total_bytes, 1149); // 100 + 50 + 999
        assert_eq!(r.files, 3);
        // proj (150) sits at depth 1, src at depth 2
        let proj = r.children.iter().find(|c| c.path.ends_with("proj")).unwrap();
        assert_eq!(proj.size, 150);
        assert_eq!(proj.depth, 1);
        let src = r.children.iter().find(|c| c.path.ends_with("src")).unwrap();
        assert_eq!(src.depth, 2);

        let d1 = scan(&mem, r"D:\", Some(1), 100).unwrap();
        assert!(d1.children.iter().all(|c| c.depth <= 1));
        assert!(d1.children.iter().any(|c| c.path.ends_with("proj")));
        assert!(!d1.children.iter().any(|c| c.path.ends_with("src")));

        let d0 = scan(&mem, r"D:\", Some(0), 100).unwrap();
        assert!(d0.children.is_empty());
        assert_eq!(d0.total_bytes, 1149); // total survives depth 0
    }

    #[test]
    fn top_cap_and_truncation() {
        let mem = idx();
        let r = scan(&mem, r"D:\", None, 1).unwrap();
        assert_eq!(r.children.len(), 1);
        assert!(r.truncated);
        assert_eq!(r.children[0].path.to_lowercase(), "d:\\proj2"); // biggest first
    }

    #[test]
    fn single_file_root() {
        let mem = idx();
        let r = scan(&mem, r"D:\proj\src\main.rs", None, 100).unwrap();
        assert_eq!(r.total_bytes, 100);
        assert_eq!(r.files, 1);
        assert_eq!(r.dirs, 0);
        assert_eq!(r.entries, 1);
        assert!(r.children.is_empty());
    }

    #[test]
    fn missing_root_is_empty() {
        let mem = idx();
        let r = scan(&mem, r"D:\nowhere", None, 100).unwrap();
        assert_eq!(r.total_bytes, 0);
        assert_eq!(r.entries, 0);
        assert!(r.children.is_empty());
    }

    #[test]
    fn trailing_separator_root() {
        let mem = idx();
        let a = scan(&mem, r"D:\proj\", None, 100).unwrap();
        let b = scan(&mem, r"D:\proj", None, 100).unwrap();
        assert_eq!(a.total_bytes, b.total_bytes);
        assert_eq!(a.children.len(), b.children.len());
    }
}
