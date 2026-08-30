//! Index build orchestration: raw $MFT scan (full parity, admin) with USN and
//! walk fallbacks. The build streams straight into the in-memory engine —
//! SQLite is no longer on the build path (it survives only as a dev-time
//! query oracle in the test suite).

use std::time::Instant;

use anyhow::{Result, bail};

use crate::mft::MftScanner;
use crate::mem::{MemBuilder, MemIndex};
use crate::usn::{self, UsnVolume, VolumeInfo};
use crate::{BuildReport, EntryMeta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Auto,
    Mft,
    Usn,
    Walk,
}

impl Method {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Method::Auto),
            "mft" => Ok(Method::Mft),
            "usn" => Ok(Method::Usn),
            "walk" => Ok(Method::Walk),
            other => bail!("unknown method '{other}' (expected auto|mft|usn|walk)"),
        }
    }
}

/// Resolve a comma-separated drive-letter list against the fixed NTFS volumes;
/// an empty list selects all of them.
pub fn resolve_volumes(volumes: &str) -> Vec<VolumeInfo> {
    let all = usn::list_volumes();
    if volumes.trim().is_empty() {
        return all;
    }
    let wanted: Vec<char> = volumes
        .split(',')
        .filter_map(|s| s.trim().chars().next())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    all.into_iter().filter(|v| wanted.contains(&v.drive)).collect()
}

/// Full rebuild: streams every volume straight into the in-memory engine and
/// returns it (the caller saves the dump). Also returns the per-volume max
/// USN so the monitor can start from the current journal position.
pub fn build(volumes: &[VolumeInfo], method: Method) -> Result<(BuildReport, MemIndex, Vec<(char, i64)>)> {
    let start = Instant::now();
    let mut report = BuildReport {
        volumes: volumes
            .iter()
            .map(|v| format!("{}: ({})", v.drive, v.label.trim()))
            .collect(),
        ..Default::default()
    };
    let mut mb = MemBuilder::default();
    let mut methods: Vec<&str> = Vec::new();
    let mut max_usns: Vec<(char, i64)> = Vec::new();
    for vol in volumes {
        let stats = index_volume(vol, method, &mut mb)?;
        report.files += stats.files;
        report.dirs += stats.dirs;
        report.skipped += stats.skipped;
        report.max_usn = report.max_usn.max(stats.max_usn);
        if stats.max_usn > 0 {
            max_usns.push((vol.drive, stats.max_usn));
        }
        if !methods.contains(&stats.method) {
            methods.push(stats.method);
        }
        eprintln!(
            "[{}] {}:{} — {} files + {} dirs ({} skipped)",
            stats.method, vol.drive, vol.label.trim(), stats.files, stats.dirs, stats.skipped
        );
    }
    report.elapsed_ms = start.elapsed().as_millis();
    report.method = if methods.len() == 1 {
        methods[0].to_string()
    } else {
        "mixed".to_string()
    };
    let mem = mb.finish();
    Ok((report, mem, max_usns))
}

struct VolStats {
    files: u64,
    dirs: u64,
    skipped: u64,
    max_usn: i64,
    method: &'static str,
}

fn index_volume(vol: &VolumeInfo, method: Method, mb: &mut MemBuilder) -> Result<VolStats> {
    match method {
        Method::Walk => index_walk(vol, mb),
        Method::Usn => index_usn(vol, mb),
        Method::Mft => index_mft(vol, mb),
        Method::Auto => {
            // Full-parity MFT scan first; USN enumeration as middle fallback;
            // directory walk as last resort.
            match index_mft(vol, mb) {
                Ok(stats) => Ok(stats),
                Err(e) => {
                    eprintln!(
                        "[warn] raw MFT scan failed for {}: — falling back to USN: {e:#}",
                        vol.drive
                    );
                    match index_usn(vol, mb) {
                        Ok(stats) => Ok(stats),
                        Err(e2) => {
                            eprintln!(
                                "[warn] USN indexing failed for {}: — falling back to walk: {e2:#}",
                                vol.drive
                            );
                            index_walk(vol, mb)
                        }
                    }
                }
            }
        }
    }
}

/// Raw $MFT scan: full parity — hard-link aliases, size, mtime, flags.
fn index_mft(vol: &VolumeInfo, mb: &mut MemBuilder) -> Result<VolStats> {
    let t = Instant::now();
    eprintln!("[mft] indexing {}: via raw $MFT ...", vol.drive);
    let scanner = MftScanner::open(vol.drive)?;
    if scanner.is_fragmented() {
        bail!("$MFT behind an attribute list — unsupported layout");
    }
    let mut builder = usn::TreeBuilder::new(vol.drive);
    let records = scanner.scan(|e| {
        let mut meta = EntryMeta {
            is_dir: e.is_dir,
            size: e.size,
            mtime: e.mtime,
            ctime: e.ctime,
            frn: Some(e.frn),
            flags: 0,
        };
        if e.hidden {
            meta.flags |= EntryMeta::FLAG_HIDDEN;
        }
        if e.system {
            meta.flags |= EntryMeta::FLAG_SYSTEM;
        }
        if e.readonly {
            meta.flags |= EntryMeta::FLAG_READONLY;
        }
        if e.reparse {
            meta.flags |= EntryMeta::FLAG_REPARSE;
        }
        builder.push(e.frn, e.parent_frn, &e.name, meta);
    })?;
    eprintln!(
        "[mft] {}: scanned {records} FILE records in {} ms",
        vol.drive,
        t.elapsed().as_millis()
    );
    let mut stats = VolStats { files: 0, dirs: 0, skipped: 0, max_usn: 0, method: "mft" };
    let mut processed = 0u64;
    stats.skipped = builder.build(|path, meta| {
        if meta.is_dir {
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        processed += 1;
        if processed % 200_000 == 0 {
            eprintln!("[mft] {}: {processed} entries inserted ...", vol.drive);
        }
        mb.push(path, meta);
    });
    Ok(stats)
}

/// USN/MFT enumeration fallback: primary names only, no size/timestamps.
fn index_usn(vol: &VolumeInfo, mb: &mut MemBuilder) -> Result<VolStats> {
    let t = Instant::now();
    eprintln!("[usn] indexing {}: via NTFS USN ...", vol.drive);
    let handle = UsnVolume::open(vol.drive)?;
    // Compact pool + streaming DFS: peak memory stays ~24 bytes/record, no
    // per-entry path strings are ever stored.
    let mut builder = usn::TreeBuilder::new(vol.drive);
    let (count, max_usn) = handle.enumerate(|r| {
        builder.push(
            r.frn,
            r.parent_frn,
            &r.name,
            EntryMeta { is_dir: r.is_dir, frn: Some(r.frn), ..Default::default() },
        )
    })?;
    eprintln!(
        "[usn] {}: enumerated {count} MFT records in {} ms, max_usn={max_usn}",
        vol.drive,
        t.elapsed().as_millis()
    );
    let mut stats = VolStats { files: 0, dirs: 0, skipped: 0, max_usn, method: "usn" };
    let mut processed = 0u64;
    stats.skipped = builder.build(|path, meta| {
        if meta.is_dir {
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        processed += 1;
        if processed % 200_000 == 0 {
            eprintln!("[usn] {}: {processed} entries inserted ...", vol.drive);
        }
        mb.push(path, meta);
    });
    Ok(stats)
}

fn index_walk(vol: &VolumeInfo, mb: &mut MemBuilder) -> Result<VolStats> {
    let root = format!("{}:\\", vol.drive);
    eprintln!("[walk] indexing {root} ...");
    let mut stats = VolStats { files: 0, dirs: 0, skipped: 0, max_usn: 0, method: "walk" };
    let mut processed = 0u64;
    stats.skipped = crate::walk::scan_tree(&root, |path: &str, meta: EntryMeta| {
        if meta.is_dir {
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        processed += 1;
        if processed % 200_000 == 0 {
            eprintln!("[walk] {}: {processed} entries inserted ...", vol.drive);
        }
        mb.push(path, meta);
    });
    Ok(stats)
}
