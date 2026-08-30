//! USN journal change monitoring — keeps the dump live in memory and flushes
//! it back to disk (Everything-style: in-memory index + debounced save).
//!
//! Polls `FSCTL_READ_USN_JOURNAL` (admin) and applies create/delete/rename
//! events to a working copy of the index. Deletions are applied by FRN so
//! they work even after the MFT record has been recycled. A crash between
//! flushes loses nothing: the USN position sidecar is updated with the dump,
//! and the journal replays the gap on the next start.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use windows_sys::Win32::System::Ioctl::{
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
};

use crate::EntryMeta;
use crate::mem::{MemBuilder, MemIndex};
use crate::usn::{UsnVolume, resolve_path};

const MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_RENAME_OLD_NAME;

/// Watch one volume forever, applying journal events every `interval` and
/// flushing the index to `dump` every `flush_every` seconds or after a large
/// batch. The in-memory index is authoritative between flushes.
pub fn run(
    mut mem: MemIndex,
    drive: char,
    dump: PathBuf,
    interval: Duration,
    flush_every: Duration,
) -> Result<()> {
    let vol = UsnVolume::open(drive)?;
    let usn_sidecar = usn_sidecar_path(&dump);
    let mut start = read_usn(&usn_sidecar, drive).unwrap_or_else(|| sync_to_now(&vol));
    eprintln!("[monitor] watching {drive}: from USN {start} (dump: {})", dump.display());
    let mut cache: HashMap<u64, Option<String>> = HashMap::new();
    let mut frn: HashMap<u64, u32> = mem.frn_map();
    let mut removed: HashSet<u32> = HashSet::new();
    let mut appended: Vec<(String, EntryMeta)> = Vec::new();
    let mut last_flush = std::time::Instant::now();
    loop {
        let (next, records) = vol.read_journal(start, MASK)?;
        if !records.is_empty() && next < start {
            bail!(
                "USN journal on {drive}: wrapped (next={next} < start={start}) — \
                 run `fer index` again to rebuild"
            );
        }
        let mut applied = 0usize;
        for r in &records {
            if r.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0 {
                if let Some(idx) = frn.remove(&r.frn) {
                    removed.insert(idx);
                    applied += 1;
                } else if let Some(k) = appended
                    .iter()
                    .position(|(_, m)| m.frn == Some(r.frn))
                {
                    // created-then-deleted within one flush window: drop it
                    // from the pending list instead of marking an index
                    // (indices shift as removals accumulate).
                    appended.swap_remove(k);
                    applied += 1;
                }
            }
            if r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
                if let Some(parent) = resolve_path(&vol, drive, r.parent_frn, &mut cache) {
                    let path = if parent.is_empty() {
                        format!("{drive}:\\{}", r.name)
                    } else {
                        format!("{parent}\\{}", r.name)
                    };
                    // Rename-into-place / case change: retire any entry that
                    // already occupies this path.
                    if let Some(idx) = mem.find_path_idx(&path) {
                        let old_frn = mem.meta_at(idx).frn.unwrap_or(0);
                        frn.remove(&old_frn);
                        removed.insert(idx as u32);
                    }
                    let meta = EntryMeta { is_dir: r.is_dir, frn: Some(r.frn), ..Default::default() };
                    if let Some(k) = appended
                        .iter()
                        .position(|(p, _)| p.eq_ignore_ascii_case(&path))
                    {
                        appended.swap_remove(k); // same path re-created this window
                    }
                    appended.push((path, meta));
                    applied += 1;
                }
            }
        }
        if next != start {
            start = next;
        }
        if applied > 0 {
            eprintln!("[monitor] applied {applied} changes (usn={start})");
        }
        if cache.len() > 1_000_000 {
            cache.clear();
        }
        let due = !appended.is_empty() && last_flush.elapsed() >= flush_every;
        if due {
            let kept = mem.len() - removed.len() + appended.len();
            mem = flush(&mem, &removed, &appended, &dump)?;
            write_usn(&usn_sidecar, drive, start)?;
            removed.clear();
            appended.clear();
            frn = mem.frn_map();
            last_flush = std::time::Instant::now();
            eprintln!("[monitor] flushed: {kept} entries -> {}", dump.display());
        }
        thread::sleep(interval);
    }
}

/// Rebuild the index (drop `removed` indices, append new entries) and write it
/// to `dump` atomically. Returns the new authoritative index.
fn flush(
    mem: &MemIndex,
    removed: &HashSet<u32>,
    appended: &[(String, EntryMeta)],
    dump: &Path,
) -> Result<MemIndex> {
    let mut b = MemBuilder::default();
    for i in 0..mem.len() {
        if removed.contains(&(i as u32)) {
            continue;
        }
        b.push(&mem.path_at(i), mem.meta_at(i));
    }
    for (path, meta) in appended {
        b.push(path, *meta);
    }
    let new = b.finish();
    new.save(dump)?;
    Ok(new)
}

/// Sidecar holding the last-applied USN per volume ("C: 123456" lines).
fn usn_sidecar_path(dump: &Path) -> PathBuf {
    let mut p = std::ffi::OsString::from(dump.as_os_str());
    p.push(".usn");
    PathBuf::from(p)
}

fn read_usn(sidecar: &Path, drive: char) -> Option<i64> {
    let text = std::fs::read_to_string(sidecar).ok()?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(&format!("{drive}:")) {
            return parts.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn write_usn(sidecar: &Path, drive: char, usn: i64) -> Result<()> {
    let mut out: String = read_all_usns(sidecar);
    let entry = format!("{drive}: {usn}");
    let mut found = false;
    let mut lines: Vec<String> = out.lines().map(str::to_string).collect();
    for line in lines.iter_mut() {
        if line.starts_with(&format!("{drive}:")) {
            *line = entry.clone();
            found = true;
            break;
        }
    }
    if !found {
        lines.push(entry);
    }
    out = lines.join("\n") + "\n";
    let mut f = std::fs::File::create(sidecar)?;
    f.write_all(out.as_bytes())?;
    Ok(())
}

fn read_all_usns(sidecar: &Path) -> String {
    std::fs::read_to_string(sidecar).unwrap_or_default()
}

/// Start from the journal's current position (QUERY_USN_JOURNAL.NextUsn) so a
/// fresh monitor applies only future changes instead of replaying history.
fn sync_to_now(vol: &UsnVolume) -> i64 {
    match vol.query_journal() {
        Ok((_id, next)) => {
            eprintln!("[monitor] no stored USN — syncing to current journal position ({next})");
            next
        }
        Err(_) => 0,
    }
}
