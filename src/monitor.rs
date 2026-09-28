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
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SYSTEM, GetCompressedFileSizeW,
};
use windows_sys::Win32::System::Ioctl::{
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
};

use crate::EntryMeta;
use crate::mem::{MemBuilder, MemIndex};
use crate::usn::{UsnRecord, UsnVolume, resolve_path};

/// Reason bits the monitor reacts to.
///
/// USN_REASON_HARD_LINK_CHANGE is what Windows raises when a hard link is added
/// *or* removed — unlinking one alias of a multi-link record is NOT a
/// FILE_DELETE. Without it in the mask those records never even reach us, so
/// the removed alias kept its index entry until the next full `fer index`
/// (node_modules is entirely hard links, so this is a common case, not a corner
/// one).
const MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_HARD_LINK_CHANGE;

/// How far the journal may sit ahead of the applied position before the round
/// stops sleeping out `interval` and starts replaying back to back. Below it
/// the monitor is effectively live and the normal poll cadence applies.
const CATCHUP_LAG: i64 = 200_000;
/// Back-to-back catch-up rounds before a breather. The journal is drained 64 KB
/// per ioctl inside `read_journal`, so a round is real work; the budget only
/// exists so a permanently lagging journal cannot spin the CPU for hours.
const CATCHUP_BURST: u32 = 64;
/// Pause between catch-up bursts — still far faster than `interval`, and the
/// control channel is polled while waiting on it, so `fer flush` / `fer rebuild`
/// cannot be starved by a long catch-up.
const CATCHUP_BREATHER: Duration = Duration::from_millis(200);
/// How often catch-up progress (lag, replay rate, ETA) is logged.
const CATCHUP_LOG_EVERY: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Pending change set
// ---------------------------------------------------------------------------

/// Changes accumulated since the last flush: entries retired from the loaded
/// index, the FRNs they belonged to, and entries that are not in the index yet.
#[derive(Default)]
struct Pending {
    /// FRNs retired in this window. Diagnostic only since the retire decisions
    /// became stat-verified: it is what the stats line and `fer status` report,
    /// but nothing reads it to shadow a lookup any more (re-marking an already
    /// retired entry is a no-op, and skipping it could miss a surviving alias
    /// that later goes away).
    removed_frns: HashSet<u64>,
    /// Entries of the *current* index retired in this window (indices into
    /// mem, valid only until the next flush rebuilds it).
    removed: HashSet<u32>,
    /// Created/renamed entries; not in the loaded index until the flush.
    appended: Vec<(String, EntryMeta)>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.appended.is_empty() && self.removed.is_empty()
    }
    fn clear(&mut self) {
        self.appended.clear();
        self.removed.clear();
        self.removed_frns.clear();
    }
}

/// What one round of apply_records did; feeds the periodic stats line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ApplyStats {
    /// Events that changed the pending set.
    applied: usize,
    /// Create/rename events whose parent chain could not be resolved — the
    /// record is dropped (its parent is gone or unreadable). Counted because a
    /// silent drop is indistinguishable from "the event never arrived".
    resolve_fail: usize,
    /// Create/rename events whose path could not be stat'ed, i.e. the file was
    /// already gone by the time the event was applied. They fall back to
    /// zeroed metadata: exactly the 0-byte / 1970-01-01 pollution this fill
    /// exists to prevent, so they are never silent.
    stat_fail: usize,
    /// GetCompressedFileSizeW failures: allocated fell back to 0, which is
    /// also the legitimate value for a resident file, so the two are only
    /// distinguishable through this counter.
    alloc_fail: usize,
}

/// Metadata fill for one create/rename event.
struct StatMeta {
    meta: EntryMeta,
    /// The allocated-size query failed, so meta.allocated is the 0 fallback.
    alloc_failed: bool,
}

/// Fill an entry's metadata from the file system, for a create/rename event.
///
/// Cost per event: one std::fs::metadata plus (files only) one
/// GetCompressedFileSizeW. Both are needed — the USN record itself carries no
/// size and no timestamps, which is why entries created here used to land in
/// the index as size 0 / mtime 0 and stayed that way through the next flush
/// (measured: 482,481 entries at dm:1970-01-01, ~12% of the whole index).
///
/// The two sizes are deliberately different and must not be conflated:
/// * size is the *logical* length — what a raw $MFT scan reports as the $DATA
///   real size, and what size: queries and du total_bytes use. A compressed or
///   sparse file reports its full logical length here.
/// * allocated is the cluster bytes actually occupied. It is legitimately
///   *smaller* than size for compressed/sparse files, and 0 for a resident
///   file that lives inside its MFT record (a real value, not "unknown" — see
///   the allocated contract on EntryMeta).
///
/// None means the path no longer exists (created and deleted between two
/// journal reads, or renamed away before we got to it).
fn stat_meta(path: &str, is_dir: bool, frn: u64) -> Option<StatMeta> {
    // symlink_metadata (lstat) and not metadata (stat): a reparse point has to
    // report its *own* record, exactly as the raw $MFT scan does. Following the
    // link would record the target's size, timestamps and flags, so a junction
    // or symlink created while the monitor is running would disagree with the
    // same entry after the next fer index. For any non-reparse path the two
    // calls are identical.
    let md = std::fs::symlink_metadata(path).ok()?;
    let attrs = md.file_attributes();
    let flags = flags_from_attributes(attrs);
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    // Directories take this path too: their logical size is meaningless and
    // they allocate no data clusters for the entry itself, so both stay 0 (and
    // GetCompressedFileSizeW is not called for them). Their timestamps are real
    // and worth keeping - a directory created or renamed right now would
    // otherwise show up as 1970 exactly like the file entries used to.
    let size = if is_dir { 0 } else { md.len() };
    let mut alloc_failed = false;
    // A reparse point stores its target in resident $REPARSE_POINT data, so it
    // occupies no data clusters of its own - and GetCompressedFileSizeW would
    // follow the link and report the *target's* allocation, which is what $MFT
    // does not do. 0 here is the aligned value, not a failure.
    let allocated = if is_dir || reparse {
        0
    } else {
        match compressed_size(path) {
            Some(bytes) => bytes,
            None => {
                alloc_failed = true;
                0
            }
        }
    };
    Some(StatMeta {
        meta: EntryMeta {
            is_dir,
            size,
            allocated,
            mtime: crate::mft::filetime_to_unix(md.last_write_time()),
            ctime: crate::mft::filetime_to_unix(md.creation_time()),
            flags,
            frn: Some(frn),
        },
        alloc_failed,
    })
}

/// Map Win32 FILE_ATTRIBUTE_* bits onto EntryMeta's flag bits — the same
/// mapping the raw $MFT scan applies from $STANDARD_INFORMATION, so an entry
/// created while the monitor runs carries the same flags as one from a rebuild.
fn flags_from_attributes(attrs: u32) -> u8 {
    let mut flags = 0u8;
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        flags |= EntryMeta::FLAG_HIDDEN;
    }
    if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
        flags |= EntryMeta::FLAG_SYSTEM;
    }
    if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        flags |= EntryMeta::FLAG_READONLY;
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        flags |= EntryMeta::FLAG_REPARSE;
    }
    flags
}

/// Bytes of clusters the path actually occupies (GetCompressedFileSizeW, which
/// is compressed/sparse aware, unlike the $DATA logical size).
///
/// The return value of that API cannot decide success on its own:
/// INVALID_FILE_SIZE (0xFFFF_FFFF) is also a perfectly legal low half, so a
/// file whose size ends in those bits would look like a failure. Only the
/// last-error code distinguishes them, and it is cleared first because Win32
/// only promises that last-error is meaningful *after* a failure — without the
/// reset a stale code from an unrelated call would reject a valid size.
fn compressed_size(path: &str) -> Option<u64> {
    let wide = wide_verbatim(path);
    let mut high = 0u32;
    // SAFETY: 'wide' is a NUL-terminated UTF-16 path and 'high' outlives the
    // call; both are the documented contract of GetCompressedFileSizeW.
    unsafe { SetLastError(0) };
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if unsafe { GetLastError() } != 0 {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

/// NUL-terminated UTF-16 form of a drive-letter path, with the Win32 verbatim
/// prefix (\\?\) when it is missing.
///
/// The API behind this call goes through the Win32 path layer, which refuses
/// paths longer than MAX_PATH unless they are verbatim. Rust's std adds that
/// prefix internally, so metadata() succeeds on a deep build path while the
/// allocation query would fail without it and silently report 0 — a >260-char
/// path is ordinary in a deep build tree.
fn wide_verbatim(path: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(path.len() + 8);
    if path.as_bytes().get(1) == Some(&b':') && !path.starts_with(r"\\?\") {
        out.extend(r"\\?\".encode_utf16());
    }
    out.extend(path.encode_utf16());
    out.push(0);
    out
}

/// ASCII-CI comparison of a path's last component against a USN record name.
fn name_matches_path(path: &[u8], name: &str) -> bool {
    let base = path
        .rsplit(|&b| b == b'\\' || b == b'/')
        .next()
        .unwrap_or(path);
    base.eq_ignore_ascii_case(name.as_bytes())
}

/// Apply one round of USN records to the pending change set.
///
/// Split out of run() so the create/delete/rename rules can be unit-tested
/// without a real volume, a real journal or an elevated token: path resolution
/// and the metadata stat are injected as closures.
///
/// The appearance and disappearance decisions never guess from the reason bits
/// alone: every "this path may be gone" event is settled by stat'ing the
/// candidate paths, so the file system is the judge. That is what lets "one of
/// several hard links was removed" (keep the survivors) and "the FILE record
/// was deleted" (retire everything) share one code path.
fn apply_records(
    mem: &MemIndex,
    drive: char,
    records: &[UsnRecord],
    pending: &mut Pending,
    resolve: &mut impl FnMut(u64) -> Option<String>,
    stat: &mut impl FnMut(&str, bool, u64) -> Option<StatMeta>,
) -> ApplyStats {
    let mut stats = ApplyStats::default();
    for r in records {
        let hard_link = r.reason & USN_REASON_HARD_LINK_CHANGE != 0;
        // Disappearance side: delete, rename-away and hard-link change.
        if hard_link || r.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0 {
            stats.applied += retire_aliases(mem, r, pending, stat);
        }
        // Appearance side: create, rename-into-place and hard-link change - the
        // last one is either a link appearing or a link disappearing, and only
        // the stat below can tell which.
        if hard_link || r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
            match resolve(r.parent_frn) {
                Some(parent) => {
                    let path = if parent.is_empty() {
                        format!("{drive}:\\{}", r.name)
                    } else {
                        format!("{parent}\\{}", r.name)
                    };
                    match stat(&path, r.is_dir, r.frn) {
                        Some(s) => {
                            if s.alloc_failed {
                                stats.alloc_fail += 1;
                            }
                            // Upsert: retire whatever still occupies this path
                            // (rename-into-place, case change, a hard link added
                            // again) and take its place with fresh metadata.
                            retire_path(mem, &path, pending);
                            drop_pending_path(pending, &path);
                            pending.appended.push((path, s.meta));
                            stats.applied += 1;
                        }
                        // A HARD_LINK_CHANGE whose link is not there any more is
                        // a link *removal*: retire the path instead of adding a
                        // zero-metadata entry. Without this the alias stayed in
                        // the index forever - every unlinked pnpm store link was
                        // a ghost (node_modules is nothing but hard links).
                        None if hard_link => {
                            stats.applied += retire_path(mem, &path, pending);
                            stats.applied += drop_pending_path(pending, &path);
                        }
                        None => {
                            stats.stat_fail += 1;
                            retire_path(mem, &path, pending);
                            drop_pending_path(pending, &path);
                            pending.appended.push((
                                path,
                                EntryMeta {
                                    is_dir: r.is_dir,
                                    frn: Some(r.frn),
                                    ..Default::default()
                                },
                            ));
                            stats.applied += 1;
                        }
                    }
                }
                None => stats.resolve_fail += 1,
            }
        }
    }
    stats
}

/// Retire the index entry at the given path, if any. Returns 1 when it was
/// actually retired - re-marking an entry an earlier event already took out is
/// a no-op, not a second event.
fn retire_path(mem: &MemIndex, path: &str, pending: &mut Pending) -> usize {
    match mem.find_path_idx(path) {
        Some(idx) => {
            let old_frn = mem.meta_at(idx).frn.unwrap_or(0);
            if old_frn != 0 {
                pending.removed_frns.insert(old_frn);
            }
            usize::from(pending.removed.insert(idx as u32))
        }
        None => 0,
    }
}

/// Drop a pending append at the given path (ASCII-CI). Returns 1 when one was
/// removed. Pending appends are not in the index yet, so they need their own
/// lookup.
fn drop_pending_path(pending: &mut Pending, path: &str) -> usize {
    match pending
        .appended
        .iter()
        .position(|(p, _)| p.eq_ignore_ascii_case(path))
    {
        Some(k) => {
            pending.appended.swap_remove(k);
            1
        }
        None => 0,
    }
}

/// Retire the entries a "this may be gone" event refers to, after checking each
/// candidate path against the file system. Returns how many were retired.
///
/// * FILE_DELETE / HARD_LINK_CHANGE - candidates are *every* alias of the
///   record, in the index and in the pending appends. find_frn alone reaches
///   only the first, which left the rest of a deleted hard-link set in the
///   index forever: measured with two links deleted, the second was still
///   searchable 30 s later and was written into the next dump.
/// * RENAME_OLD_NAME - only the alias whose name this record carries can have
///   moved; retiring the others would take out hard links that were never
///   renamed. When no name matches it falls back to the first hit (the
///   pre-existing behaviour) so the event is never dropped.
///
/// The stat is what resolves the ambiguity the reason bits cannot express: a
/// delete of one link of a multi-link record leaves the other paths on disk, so
/// they are kept; a rename that only changed case leaves the old spelling
/// stat-able (Windows paths are case-insensitive), and the RENAME_NEW_NAME
/// upsert then replaces it. Nothing is retired on a guess.
fn retire_aliases(
    mem: &MemIndex,
    r: &UsnRecord,
    pending: &mut Pending,
    stat: &mut impl FnMut(&str, bool, u64) -> Option<StatMeta>,
) -> usize {
    let rename_old = r.reason & USN_REASON_RENAME_OLD_NAME != 0;
    let matching = |path: &[u8]| !rename_old || name_matches_path(path, &r.name);
    let mut retired = 0usize;

    let all_idxs = mem.find_frn_all(r.frn);
    // Candidates in the loaded index ...
    let mut idxs: Vec<u32> = all_idxs
        .iter()
        .copied()
        .filter(|&i| matching(mem.path_bytes(i as usize)))
        .collect();
    // ... and in the pending append list (not in the index until the flush).
    let mut apps: Vec<usize> = pending
        .appended
        .iter()
        .enumerate()
        .filter(|(_, (p, m))| m.frn == Some(r.frn) && matching(p.as_bytes()))
        .map(|(k, _)| k)
        .collect();

    // A rename whose old name matches nothing keeps the legacy behaviour of
    // retiring a single candidate instead of dropping the event.
    if rename_old && idxs.is_empty() && apps.is_empty() {
        match all_idxs.first() {
            Some(&i) => idxs.push(i),
            None => {
                apps.extend(
                    pending
                        .appended
                        .iter()
                        .enumerate()
                        .find(|(_, (_, m))| m.frn == Some(r.frn))
                        .map(|(k, _)| k),
                );
            }
        }
    }

    for idx in idxs {
        let i = idx as usize;
        let meta = mem.meta_at(i);
        // Still on disk: a surviving alias (or a case-only rename), not a
        // disappearance.
        if stat(&mem.path_at(i), meta.is_dir, meta.frn.unwrap_or(r.frn)).is_some() {
            continue;
        }
        if pending.removed.insert(idx) {
            retired += 1;
        }
    }
    // Descending, so removing a higher index cannot move a lower victim.
    for k in apps.into_iter().rev() {
        let (path, meta) = &pending.appended[k];
        if stat(path, meta.is_dir, meta.frn.unwrap_or(r.frn)).is_some() {
            continue;
        }
        pending.appended.swap_remove(k);
        retired += 1;
    }

    if retired > 0 {
        pending.removed_frns.insert(r.frn);
    }
    retired
}

/// Watch one volume forever, applying journal events every `interval` and
/// flushing the index to `dump` every `flush_every` seconds whenever changes
/// are pending. The in-memory index is authoritative between flushes.
///
/// When `push_addr` is set a [`crate::push::Broadcaster`] is bound there and
/// every applied batch is broadcast to connected `fer serve` receivers, so a
/// long-lived server can show newly created files without waiting for a flush.
pub fn run(
    mut mem: MemIndex,
    drive: char,
    dump: PathBuf,
    interval: Duration,
    flush_every: Duration,
    push_addr: Option<String>,
    control_addr: Option<String>,
) -> Result<()> {
    // Hard gate: the USN journal needs an elevated token; failing 10 minutes
    // into a watch (or worse, flushing a broken index) is worse than refusing
    // up front.
    if !crate::is_elevated() {
        bail!("fer monitor needs an elevated process (USN journal access)");
    }
    let feed = push_addr.as_deref().and_then(crate::push::Broadcaster::bind);
    // Control channel: lets `fer flush` / `fer rebuild` poke this loop instead of
    // waiting out `--flush-secs`. Optional — a monitor without it still watches
    // the journal.
    let control = control_addr.as_deref().and_then(crate::control::bind);
    let mut vol = UsnVolume::open(drive)?;
    let usn_sidecar = usn_sidecar_path(&dump);
    let mut start = read_usn(&usn_sidecar, drive).unwrap_or_else(|| sync_to_now(&vol));
    eprintln!("[monitor] watching {drive}: from USN {start} (dump: {})", dump.display());
    let mut pending = Pending::default();
    // A restart replays from the last *flush* position, which on this machine is
    // up to --flush-secs (1800 s) of disk churn — about 8.6M USN units measured.
    // Say so up front: "my new file is not searchable" otherwise looks like a
    // broken feed instead of a queue that is still draining.
    let mut lag = journal_lag(&vol, start);
    if lag > CATCHUP_LAG {
        eprintln!(
            "[monitor] {lag} USN units behind the journal — replaying the backlog first; \
             change push stays off until caught up"
        );
    }
    let mut cache: HashMap<u64, Option<String>> = HashMap::new();
    let mut last_flush = std::time::Instant::now();
    // Periodic self-report. The monitor once ballooned to 20 GB of private
    // commit within a minute of starting while `applied` counts and the push
    // batch both looked normal, and nothing in the logs said which structure
    // was growing. These are the only per-round collections that can; printing
    // their sizes makes the culprit identifiable the next time it happens.
    let mut last_report = std::time::Instant::now();
    let mut catching_up = lag > CATCHUP_LAG;
    let mut catchup_rounds: u32 = 0;
    let mut last_catchup_log = std::time::Instant::now() - CATCHUP_LOG_EVERY;
    // Set when a round had something to broadcast but could not (catch-up), so
    // the first live round pushes the accumulated set even if it applied
    // nothing itself.
    let mut push_stale = false;
    loop {
        // Wait out the poll interval, or wake immediately for a control command:
        // `recv_timeout` replaces the plain sleep so `fer rebuild` is served at
        // once rather than on the next tick.
        //
        // While the journal is far behind, skip the wait so the backlog drains
        // at back-to-back round rate instead of one round per interval (the
        // 5-minute catch-up measured on this machine). A burst budget plus a
        // short breather keep a permanently lagging journal from spinning the
        // CPU for hours, and the breather still polls the control channel, so
        // neither `fer flush` nor `fer rebuild` can be starved by catch-up.
        let wait = poll_wait(catching_up, &mut catchup_rounds, interval);
        let mut command: Option<crate::control::Request> = match &control {
            Some(rx) => match rx.recv_timeout(wait) {
                Ok(req) => Some(req),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    thread::sleep(wait);
                    None
                }
            },
            None => {
                thread::sleep(wait);
                None
            }
        };
        // Round timing for the catch-up estimate: taken after the wait so the
        // leading sleep does not dilute the measured replay rate.
        let round_t0 = std::time::Instant::now();
        let round_from = start;
        // A monitor that was down long enough for the journal to be recycled
        // past the saved position fails here with ERROR_JOURNAL_DELETE_IN_
        // PROGRESS (1181) — resuming from that USN is impossible. Sync to the
        // current position instead of dying in a restart loop; the gap is
        // covered by `fer index` (a rebuild), which the message says.
        let (next, records) = match vol.read_journal(start, MASK) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "[monitor] reading the USN journal from {start} failed ({e}) — it was \
                     recycled while the monitor was down. Syncing to the current position; \
                     changes in the gap are NOT in the index (run `fer index` to rebuild)."
                );
                start = sync_to_now(&vol);
                vol.read_journal(start, MASK)?
            }
        };
        if !records.is_empty() && next < start {
            bail!(
                "USN journal on {drive}: wrapped (next={next} < start={start}) — \
                 run `fer index` again to rebuild"
            );
        }
        let stats;
        {
            // Path resolution and the metadata stat are the only things that
            // need the volume, so they are injected here and apply_records
            // itself stays a pure function of the records + the index.
            let mut resolve = |frn: u64| resolve_path(&mut vol, drive, frn, &mut cache);
            let mut stat = |path: &str, is_dir: bool, frn: u64| stat_meta(path, is_dir, frn);
            stats = apply_records(&mem, drive, &records, &mut pending, &mut resolve, &mut stat);
        }
        if next != start {
            start = next;
        }
        if stats.applied > 0 {
            eprintln!("[monitor] applied {} changes (usn={start})", stats.applied);
        }
        // Measure the backlog *after* applying: NextUsn minus the position just
        // applied is exactly what is left to replay. While it stays above the
        // threshold the round does not wait out `interval` and the change feed is
        // paused — re-serializing the whole pending set every round is O(n^2)
        // work for a receiver that cannot show the backlog anyway. The push is a
        // full-replacement snapshot, so the round that finally catches up sends
        // everything again; nothing is lost.
        lag = journal_lag(&vol, start);
        let was_catching_up = catching_up;
        catching_up = lag > CATCHUP_LAG;
        if catching_up {
            if !was_catching_up {
                last_catchup_log = std::time::Instant::now() - CATCHUP_LOG_EVERY;
            }
            catchup_rounds = catchup_rounds.saturating_add(1);
            if last_catchup_log.elapsed() >= CATCHUP_LOG_EVERY {
                // Replay rate of the round that just finished, so the estimate
                // tracks the machine instead of a hardcoded guess.
                let secs = round_t0.elapsed().as_secs_f64().max(0.001);
                let rate = ((start - round_from).max(0) as f64 / secs).max(1.0);
                eprintln!(
                    "[monitor] catching up: {lag} USN units behind ({rate:.0}/s) — ETA ~{:.0}s",
                    lag as f64 / rate
                );
                last_catchup_log = std::time::Instant::now();
            }
        } else {
            catchup_rounds = 0;
            if was_catching_up {
                eprintln!("[monitor] caught up with the journal (usn={start})");
            }
        }
        if last_report.elapsed() >= Duration::from_secs(60) {
            eprintln!(
                "[monitor] stats: mem={} appended={} removed={} frns={} cache={} \
                 resolve_fail={} stat_fail={} alloc_fail={} lag={}",
                mem.len(),
                pending.appended.len(),
                pending.removed.len(),
                pending.removed_frns.len(),
                cache.len(),
                stats.resolve_fail,
                stats.stat_fail,
                stats.alloc_fail,
                lag
            );
            last_report = std::time::Instant::now();
        }
        if cache.len() > 1_000_000 {
            cache.clear();
        }
        // Broadcast the pending set BEFORE the flush decision: `removed` holds
        // indices into the *current* index, so the paths must be resolved while
        // that index is still the authoritative one (flush rebuilds it).
        //
        // A set held back during catch-up is carried into the first live round
        // by `push_stale`, so catching up cannot leave the receiver without the
        // changes accumulated while the feed was off.
        if stats.applied > 0 && catching_up {
            push_stale = true;
        }
        if !catching_up
            && (stats.applied > 0 || push_stale)
            && let Some(f) = &feed
        {
            let batch = build_batch(&mem, &pending);
            if !batch.is_empty() {
                f.send(&batch);
                push_stale = false;
            }
        }
        let pending_changes = !pending.is_empty();
        // A `fer flush` on the control channel overrides the debounce window.
        let force_flush = matches!(
            command.as_ref().map(|r| r.cmd),
            Some(crate::control::Cmd::Flush)
        );
        let due = pending_changes && (last_flush.elapsed() >= flush_every || force_flush);
        let mut flushed = false;
        if due {
            let kept = mem.len() - pending.removed.len() + pending.appended.len();
            let n_removed = pending.removed.len();
            let n_appended = pending.appended.len();
            // `flush` returns the index it just built — a heap `Owned` copy of the
            // whole volume (~1.4 GB here). The file it wrote is byte-identical, so
            // re-map the dump instead of keeping that copy alive: the old mmap is
            // dropped anyway, the new one costs ~1 ms, and its pages come straight
            // from the page cache the writer just populated. Without this the
            // monitor sits on 1.4 GB of committed private memory that nothing ever
            // touches (measured: 1,700 MB private / 12 MB resident after one flush).
            // On the (unlikely) reload failure keep the owned copy — correctness
            // first, memory second.
            let owned = flush(&mem, &pending, &dump)?;
            // `kept` is what the loop above *intended* to write; `owned.len()` is
            // what the builder actually produced. They diverge when the source
            // index contains entries the arena writes cannot round-trip (an
            // inflated or span-corrupt dump), so log both plus the pending-set
            // sizes: a shrinking `mem` across flushes with a small `removed` is
            // the signature of that, and it was invisible before this line.
            let written = owned.len();
            mem = MemIndex::load_dump(&dump).unwrap_or(owned);
            write_usn(&usn_sidecar, drive, start)?;
            pending.clear();
            // The dump now carries everything that was pending, so a set held
            // back during catch-up has nothing left to push.
            push_stale = false;
            last_flush = std::time::Instant::now();
            eprintln!(
                "[monitor] flushed: kept={kept} written={written} \
                 (mem={} removed={n_removed} appended={n_appended}) -> {}",
                mem.len(),
                dump.display()
            );
            flushed = true;
        }
        if let Some(req) = command.take() {
            let msg = match req.cmd {
                crate::control::Cmd::Flush => {
                    if flushed {
                        format!("ok: flushed {} entries to {}", mem.len(), dump.display())
                    } else {
                        "ok: no pending changes (the dump already matches the journal)".to_string()
                    }
                }
                crate::control::Cmd::Status => format!(
                    "ok: mem={} appended={} removed={} retired_frns={} usn={start} \
                     last_flush={}s ago",
                    mem.len(),
                    pending.appended.len(),
                    pending.removed.len(),
                    pending.removed_frns.len(),
                    last_flush.elapsed().as_secs()
                ),
                crate::control::Cmd::Rebuild => {
                    let t0 = std::time::Instant::now();
                    // Journal position BEFORE the scan: changes made while the
                    // rebuild runs are replayed from here on the next iteration,
                    // so a rebuild cannot lose them.
                    let before = vol.query_journal().map(|(_, n)| n).unwrap_or(start);
                    let vols = crate::indexer::resolve_volumes(&drive.to_string());
                    let outcome = crate::indexer::build(&vols, crate::indexer::Method::Mft);
                    match outcome {
                        Ok((report, fresh)) => {
                            // This monitor owns one volume; entries already
                            // indexed on the others are carried over verbatim,
                            // otherwise a rebuild would silently shrink the dump.
                            let mut b = MemBuilder::default();
                            let mut files = 0u64;
                            let mut dirs = 0u64;
                            for i in 0..mem.len() {
                                if !path_on_drive(mem.path_bytes(i), drive) {
                                    let meta = mem.meta_at(i);
                                    if meta.is_dir {
                                        dirs += 1;
                                    } else {
                                        files += 1;
                                    }
                                    b.push_arena(
                                        mem.path_bytes(i),
                                        mem.name_l_bytes(i),
                                        mem.rev_bytes(i),
                                        meta,
                                    );
                                }
                            }
                            for i in 0..fresh.len() {
                                let meta = fresh.meta_at(i);
                                if meta.is_dir {
                                    dirs += 1;
                                } else {
                                    files += 1;
                                }
                                b.push_arena(
                                    fresh.path_bytes(i),
                                    fresh.name_l_bytes(i),
                                    fresh.rev_bytes(i),
                                    meta,
                                );
                            }
                            let new = b.finish();
                            let entries = new.len();
                            match new.save(&dump) {
                                Ok(()) => {
                                    mem = MemIndex::load_dump(&dump).unwrap_or(new);
                                    cache.clear();
                                    pending.clear();
                                    // The dump now carries everything; nothing is
                                    // left over from a paused feed either.
                                    push_stale = false;
                                    start = before;
                                    last_flush = std::time::Instant::now();
                                    let _ = write_usn(&usn_sidecar, drive, start);
                                    // Keep the quality sidecar honest: `fer stats`
                                    // reports built_at_unix from it, so a rebuild that
                                    // does not stamp it leaves a freshly rebuilt index
                                    // looking stale. The volume list is carried over —
                                    // this dump still covers every volume, only `drive`
                                    // was re-scanned.
                                    let volumes = crate::meta::read_index_meta(&dump)
                                        .map(|m| m.volumes)
                                        .unwrap_or_else(|| vec![format!("{drive}:")]);
                                    let _ = crate::meta::write_index_meta(
                                        &dump,
                                        &crate::meta::IndexMeta {
                                            method: "mft".to_string(),
                                            volumes,
                                            files,
                                            dirs,
                                            skipped: report.skipped,
                                            elapsed_ms: t0.elapsed().as_millis() as u64,
                                            built_at_unix: crate::meta::IndexMeta::now_unix(),
                                        },
                                    );
                                    let msg = format!(
                                        "ok: rebuilt {drive}: in {} ms — {entries} entries \
                                         ({} files + {} dirs scanned) -> {}",
                                        t0.elapsed().as_millis(),
                                        report.files,
                                        report.dirs,
                                        dump.display()
                                    );
                                    eprintln!("[monitor] {msg}");
                                    msg
                                }
                                Err(e) => format!(
                                    "err: rebuild scanned {drive}: but writing the dump failed: {e}"
                                ),
                            }
                        }
                        Err(e) => format!("err: rebuild failed: {e}"),
                    }
                }
            };
            let _ = req.reply.send(msg);
        }
    }
}

/// Whether an indexed path (raw arena bytes, original case) lives on `drive`.
fn path_on_drive(path: &[u8], drive: char) -> bool {
    path.len() >= 2 && path[1] == b':' && (path[0] as char).eq_ignore_ascii_case(&drive)
}

/// Turn the pending change set into a push batch.
///
/// `pending.removed` stores indices into `mem`, so this must run BEFORE the
/// flush that rebuilds the index. The whole pending set is sent every round
/// (not a delta): re-applying a path is idempotent on the receiver, which makes
/// a dropped message self-heal on the next round.
fn build_batch(mem: &MemIndex, pending: &Pending) -> crate::push::Batch {
    let mut batch = crate::push::Batch::default();
    for &i in &pending.removed {
        let i = i as usize;
        if i >= mem.len() {
            continue;
        }
        batch
            .remove
            .push(String::from_utf8_lossy(mem.path_bytes(i)).into_owned());
    }
    batch.append = pending
        .appended
        .iter()
        .map(|(p, m)| crate::push::AppendEntry::new(p.clone(), *m))
        .collect();
    batch
}

/// How long this round waits for the next poll tick or a control command.
///
/// Live: the full interval. Catching up: nothing, so the backlog drains at
/// back-to-back round rate — but only for `CATCHUP_BURST` rounds, after which
/// the budget resets and one short breather is served. Without that cap a
/// permanently lagging journal would spin the loop as fast as the disk allows;
/// the breather is still far shorter than the interval, and the control channel
/// is polled while waiting on it (the caller passes this to `recv_timeout`), so
/// `fer flush` / `fer rebuild` are never starved by a long catch-up.
fn poll_wait(catching_up: bool, catchup_rounds: &mut u32, interval: Duration) -> Duration {
    if !catching_up {
        return interval;
    }
    if *catchup_rounds >= CATCHUP_BURST {
        *catchup_rounds = 0;
        CATCHUP_BREATHER
    } else {
        Duration::ZERO
    }
}

/// How far the journal's writes are ahead of the applied position (0 when the
/// journal cannot be queried). The monitor uses it to decide whether it is live
/// or still replaying a backlog — see `CATCHUP_LAG`.
fn journal_lag(vol: &UsnVolume, start: i64) -> i64 {
    match vol.query_journal() {
        Ok((_id, next)) => next.saturating_sub(start).max(0),
        Err(_) => 0,
    }
}

/// Rebuild the index (drop `removed` indices, append new entries) and write it
/// to `dump` atomically. Kept entries stream through the arena fast path —
/// no per-entry String allocation or case-fold recomputation. Returns the new
/// authoritative index.
fn flush(mem: &MemIndex, pending: &Pending, dump: &Path) -> Result<MemIndex> {
    let mut b = MemBuilder::default();
    for i in 0..mem.len() {
        if pending.removed.contains(&(i as u32)) {
            continue;
        }
        b.push_arena(
            mem.path_bytes(i),
            mem.name_l_bytes(i),
            mem.rev_bytes(i),
            mem.meta_at(i),
        );
    }
    for (path, meta) in &pending.appended {
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

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parent FRN used by every record below ("D:\links").
    const PARENT: u64 = 5;
    const LINKS: &str = "D:\\links";

    fn rec(frn: u64, name: &str, reason: u32, is_dir: bool) -> UsnRecord {
        UsnRecord {
            frn,
            parent_frn: PARENT,
            name: name.to_string(),
            is_dir,
            usn: 0,
            reason,
        }
    }

    fn meta_file(size: u64, frn: u64) -> EntryMeta {
        EntryMeta {
            size,
            mtime: 1_700_000_000,
            ctime: 1_700_000_000,
            frn: Some(frn),
            ..Default::default()
        }
    }

    /// Two hard links to one FILE record plus an unrelated file.
    fn hard_link_index() -> MemIndex {
        let mut b = MemBuilder::default();
        b.push(r"D:\links\h1.bin", meta_file(7, 70));
        b.push(r"D:\links\h2.bin", meta_file(7, 70));
        b.push(r"D:\links\other.bin", meta_file(9, 71));
        b.finish()
    }

    fn resolve_links(_frn: u64) -> Option<String> {
        Some(LINKS.to_string())
    }

    /// Stat stub: every path resolves to the same (real-looking) metadata, so
    /// the tests exercise the piped result instead of the file system.
    fn stat_ok(_path: &str, is_dir: bool, frn: u64) -> Option<StatMeta> {
        Some(StatMeta {
            meta: EntryMeta {
                is_dir,
                size: 4096,
                allocated: 4096,
                mtime: 1_700_000_123,
                ctime: 1_700_000_100,
                flags: 0,
                frn: Some(frn),
            },
            alloc_failed: false,
        })
    }

    fn stat_none(_path: &str, _is_dir: bool, _frn: u64) -> Option<StatMeta> {
        None
    }

    /// Stat stub that reports exactly these paths as still existing and every
    /// other path as gone. The retire rules are stat-verified, so this is what
    /// plays the role of the file system in the tests.
    fn stat_existing<'a>(
        present: &'a [&'a str],
    ) -> impl FnMut(&str, bool, u64) -> Option<StatMeta> + 'a {
        move |path: &str, is_dir: bool, frn: u64| {
            if present.iter().any(|p| p.eq_ignore_ascii_case(path)) {
                stat_ok(path, is_dir, frn)
            } else {
                None
            }
        }
    }

    // -- metadata fill ------------------------------------------------------

    #[test]
    fn attribute_bits_map_to_entry_flags() {
        assert_eq!(flags_from_attributes(0), 0);
        assert_eq!(flags_from_attributes(FILE_ATTRIBUTE_HIDDEN), EntryMeta::FLAG_HIDDEN);
        assert_eq!(flags_from_attributes(FILE_ATTRIBUTE_SYSTEM), EntryMeta::FLAG_SYSTEM);
        assert_eq!(flags_from_attributes(FILE_ATTRIBUTE_READONLY), EntryMeta::FLAG_READONLY);
        assert_eq!(flags_from_attributes(FILE_ATTRIBUTE_REPARSE_POINT), EntryMeta::FLAG_REPARSE);
        assert_eq!(
            flags_from_attributes(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY),
            EntryMeta::FLAG_HIDDEN | EntryMeta::FLAG_READONLY
        );
        // Directory / archive / temp bits are not surfaced as flags.
        assert_eq!(flags_from_attributes(0x10 | 0x20 | 0x100), 0);
    }

    #[test]
    fn stat_meta_reads_real_size_time_and_allocation() {
        // Mirrors the deployment probe: a 4 MiB file must come back with its
        // logical size, its cluster allocation and real timestamps — the defect
        // this replaces wrote size 0 / mtime 0 (the dm:1970-01-01 pollution).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.bin");
        std::fs::write(&path, vec![0xA5u8; 4 << 20]).unwrap();
        let s = stat_meta(&path.to_string_lossy(), false, 4242).expect("file exists");

        assert_eq!(s.meta.size, 4 << 20);
        assert_eq!(s.meta.frn, Some(4242));
        assert!(!s.meta.is_dir);
        assert!(!s.alloc_failed, "a plain file's compressed size must be readable");
        assert!(
            s.meta.allocated >= s.meta.size,
            "allocated {} < logical size {}",
            s.meta.allocated,
            s.meta.size
        );
        assert!(
            s.meta.allocated < s.meta.size + (1 << 20),
            "allocated {} is not cluster-rounded",
            s.meta.allocated
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((now - s.meta.mtime).abs() < 3600, "mtime {} vs now {}", s.meta.mtime, now);
        assert!((now - s.meta.ctime).abs() < 3600, "ctime {} vs now {}", s.meta.ctime, now);
        assert_eq!(s.meta.flags, 0, "an ordinary file carries no attribute flags");
    }

    #[test]
    fn stat_meta_reports_a_hidden_file() {
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hidden.bin");
        std::fs::write(&path, b"x").unwrap();
        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ok = unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
        assert_ne!(ok, 0, "SetFileAttributesW failed ({})", unsafe { GetLastError() });

        let s = stat_meta(&path.to_string_lossy(), false, 1).expect("file exists");
        assert!(s.meta.hidden());
        assert_eq!(s.meta.flags, EntryMeta::FLAG_HIDDEN);
    }

    #[test]
    fn stat_meta_gives_directories_zero_sizes_and_real_times() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let s = stat_meta(&sub.to_string_lossy(), true, 77).expect("directory exists");
        assert!(s.meta.is_dir);
        assert_eq!(s.meta.size, 0);
        assert_eq!(s.meta.allocated, 0);
        assert!(!s.alloc_failed, "directories skip the allocation query");
        assert!(s.meta.mtime > 1_600_000_000, "directory mtime {}", s.meta.mtime);
    }

    #[test]
    fn stat_meta_returns_none_for_a_vanished_path() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("never-existed.bin");
        assert!(stat_meta(&gone.to_string_lossy(), false, 1).is_none());
    }

    #[test]
    fn stat_meta_handles_paths_longer_than_max_path() {
        // Deep build trees exceed MAX_PATH. Rust's std adds the verbatim prefix
        // for metadata(), but the raw Win32 allocation query needs it spelled
        // out — without it a long path would report allocated 0 for every new
        // file, which is the same pollution class this fill fixes.
        let dir = tempfile::tempdir().unwrap();
        let mut p = dir.path().to_path_buf();
        for _ in 0..8 {
            p.push("0123456789012345678901234567890123456789");
        }
        std::fs::create_dir_all(&p).unwrap();
        let file = p.join("deep.bin");
        std::fs::write(&file, vec![1u8; 5000]).unwrap();
        let full = file.to_string_lossy().to_string();
        assert!(full.len() > 260, "the test path must exceed MAX_PATH: {}", full.len());
        let s = stat_meta(&full, false, 5).expect("a long path is stat-able");
        assert_eq!(s.meta.size, 5000);
        assert!(!s.alloc_failed, "verbatim prefix keeps the allocation query working");
        assert!(s.meta.allocated >= 4096, "allocated {}", s.meta.allocated);
        assert_eq!(wide_verbatim(r"D:\a.txt").len(), r"\\?\".len() + r"D:\a.txt".len() + 1);
        // already-verbatim and non-drive paths are passed through untouched
        assert_eq!(wide_verbatim(r"\\?\D:\a.txt").len(), r"\\?\D:\a.txt".len() + 1);
    }

    // -- create -------------------------------------------------------------

    #[test]
    fn create_takes_size_time_and_flags_from_the_stat() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(4242, "probe.bin", USN_REASON_FILE_CREATE, false)];
        let mut seen_frn = None;
        let stats = apply_records(
            &mem,
            'D',
            &records,
            &mut pending,
            &mut resolve_links,
            &mut |path: &str, is_dir: bool, frn: u64| {
                assert_eq!(path, r"D:\links\probe.bin");
                assert!(!is_dir);
                seen_frn = Some(frn);
                stat_ok(path, is_dir, frn)
            },
        );

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.stat_fail, 0);
        assert_eq!(seen_frn, Some(4242));
        assert_eq!(pending.appended.len(), 1);
        let (path, meta) = &pending.appended[0];
        assert_eq!(path, r"D:\links\probe.bin");
        assert_eq!(meta.size, 4096);
        assert_eq!(meta.allocated, 4096);
        assert_eq!(meta.mtime, 1_700_000_123);
        assert_eq!(meta.ctime, 1_700_000_100);
        assert_eq!(meta.frn, Some(4242));
    }

    #[test]
    fn create_counts_a_failed_stat_instead_of_hiding_it() {
        // The file was created and deleted between two journal reads: the
        // metadata cannot be read, so the entry falls back to zeroes — but it
        // must be counted, because silent zero-metadata is the defect.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(4242, "gone.bin", USN_REASON_FILE_CREATE, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.stat_fail, 1);
        assert_eq!(stats.resolve_fail, 0);
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].1.size, 0);
        assert_eq!(pending.appended[0].1.frn, Some(4242));
    }

    #[test]
    fn create_counts_unresolvable_parents() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(1, "orphan.bin", USN_REASON_FILE_CREATE, false)];
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut |_| None, &mut stat_ok);
        assert_eq!(stats.resolve_fail, 1);
        assert_eq!(stats.applied, 0);
        assert!(pending.is_empty());
    }

    #[test]
    fn creation_at_the_volume_root_has_no_double_separator() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(1, "top.bin", USN_REASON_FILE_CREATE, false)];
        let stats = apply_records(
            &mem,
            'D',
            &records,
            &mut pending,
            &mut |_| Some(String::new()),
            &mut stat_ok,
        );
        assert_eq!(stats.applied, 1);
        assert_eq!(pending.appended[0].0, r"D:\top.bin");
    }

    // -- delete / rename ----------------------------------------------------

    #[test]
    fn delete_retires_every_alias_of_the_record() {
        let mem = hard_link_index();
        assert_eq!(mem.find_frn_all(70), vec![0, 1]);

        let mut pending = Pending::default();
        let records = [rec(70, "h1.bin", USN_REASON_FILE_DELETE, false)];
        // Nothing is on disk any more: the record was deleted, so every alias
        // of it goes with it.
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);

        assert_eq!(stats.applied, 2, "both hard-link aliases must be retired");
        assert!(pending.removed.contains(&0));
        assert!(pending.removed.contains(&1));
        assert!(!pending.removed.contains(&2), "an unrelated FRN is untouched");
        assert!(pending.removed_frns.contains(&70));
    }

    #[test]
    fn delete_keeps_the_aliases_that_still_exist() {
        // Deleting ONE hard link is not deleting the record: the other links are
        // still on disk and must stay searchable. Retiring the whole alias set
        // on the first delete record would lose them.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [rec(70, "h1.bin", USN_REASON_FILE_DELETE, false)];
        let mut stat = stat_existing(&[r"D:\links\h2.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);

        assert_eq!(stats.applied, 1);
        assert!(pending.removed.contains(&0), "the deleted link is retired");
        assert!(!pending.removed.contains(&1), "the surviving link stays indexed");
        assert!(!pending.removed.contains(&2));
    }

    #[test]
    fn delete_is_idempotent_across_alias_records() {
        // NTFS emits one delete record per hard-link name; the second one must
        // not report work it did not do.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [
            rec(70, "h1.bin", USN_REASON_FILE_DELETE, false),
            rec(70, "h2.bin", USN_REASON_FILE_DELETE, false),
        ];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);
        assert_eq!(stats.applied, 2);
        assert_eq!(pending.removed.len(), 2);
    }

    #[test]
    fn delete_drops_pending_appends_of_the_record_too() {
        // Both aliases created in one window, then the record deleted: the
        // appends are not in the index yet, so only an FRN match can reach them.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [
            rec(70, "h1.bin", USN_REASON_FILE_CREATE, false),
            rec(70, "h2.bin", USN_REASON_FILE_CREATE, false),
            rec(70, "h1.bin", USN_REASON_FILE_DELETE, false),
        ];
        // Both links exist while their create records are applied and are gone
        // by the time the delete record is verified.
        let mut looks = 0u32;
        let mut stat = |path: &str, is_dir: bool, frn: u64| {
            looks += 1;
            if looks <= 2 { stat_ok(path, is_dir, frn) } else { None }
        };
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 4);
        assert!(pending.appended.is_empty(), "both aliases are gone: {:?}", pending.appended);
        assert!(pending.removed.is_empty());
    }

    // -- hard-link change ---------------------------------------------------

    #[test]
    fn hard_link_change_adds_an_alias_with_real_metadata() {
        // CreateHardLinkW raises HARD_LINK_CHANGE, which the old reason mask
        // dropped entirely: the new link stayed invisible until a full rebuild.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(70, "h3.bin", USN_REASON_HARD_LINK_CHANGE, false)];
        let mut stat = stat_existing(&[r"D:\links\h3.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.stat_fail, 0, "a link that exists is not a failed stat");
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].0, r"D:\links\h3.bin");
        assert_eq!(pending.appended[0].1.size, 4096, "real metadata, not zeroes");
        assert_eq!(pending.appended[0].1.mtime, 1_700_000_123);
        assert_eq!(pending.appended[0].1.frn, Some(70));
    }

    #[test]
    fn hard_link_change_removes_a_deleted_alias() {
        // Unlinking one hard link raises HARD_LINK_CHANGE with the path gone:
        // the alias must be retired. This is the ghost the old code left behind
        // for every pnpm-style store link.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [rec(70, "h1.bin", USN_REASON_HARD_LINK_CHANGE, false)];
        let mut stat = stat_existing(&[r"D:\links\h2.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);

        assert_eq!(stats.applied, 1, "the removed link is retired exactly once");
        assert!(pending.removed.contains(&0));
        assert!(!pending.removed.contains(&1), "the surviving link stays");
        assert!(pending.appended.is_empty(), "a removal must not append an entry");
    }

    #[test]
    fn hard_link_change_refreshes_a_live_alias() {
        // HARD_LINK_CHANGE with the path still there is a link *add*: upsert it
        // with fresh metadata and never leave two entries for the same path.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [rec(70, "h1.bin", USN_REASON_HARD_LINK_CHANGE, false)];
        let mut stat = stat_existing(&[r"D:\links\h1.bin", r"D:\links\h2.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);

        assert_eq!(stats.applied, 1);
        assert!(pending.removed.contains(&0), "the stale entry for that path is replaced");
        assert!(!pending.removed.contains(&1), "the other alias is untouched");
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].0, r"D:\links\h1.bin");
        assert_eq!(pending.appended[0].1.size, 4096);
    }

    // -- rename -------------------------------------------------------------

    #[test]
    fn rename_old_name_retires_only_the_matching_alias() {
        let mem = hard_link_index();
        let mut pending = Pending::default();
        // Windows names compare case-insensitively.
        let records = [rec(70, "H1.BIN", USN_REASON_RENAME_OLD_NAME, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);

        assert_eq!(stats.applied, 1);
        assert_eq!(pending.removed.len(), 1);
        assert!(pending.removed.contains(&0), "h1 (the renamed alias) is retired");
        assert!(!pending.removed.contains(&1), "h2 is a different hard link and survives");
    }

    #[test]
    fn rename_old_name_keeps_a_path_that_is_still_there() {
        // A case-only rename leaves the old spelling stat-able (Windows paths
        // are case-insensitive); the RENAME_NEW_NAME upsert replaces it.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [rec(70, "H1.BIN", USN_REASON_RENAME_OLD_NAME, false)];
        let mut stat = stat_existing(&[r"D:\links\h1.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 0);
        assert!(pending.removed.is_empty());
    }

    #[test]
    fn rename_old_name_falls_back_to_the_first_alias() {
        // No alias carries the recorded name (the entry was renamed twice
        // faster than the journal was read): keep the pre-existing behaviour of
        // retiring the first hit rather than dropping the event.
        let mem = hard_link_index();
        let mut pending = Pending::default();
        let records = [rec(70, "unknown.bin", USN_REASON_RENAME_OLD_NAME, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);
        assert_eq!(stats.applied, 1);
        assert!(pending.removed.contains(&0));
    }

    #[test]
    fn rename_old_name_can_retire_a_pending_append() {
        // Renamed within one window: the old name only exists in the append
        // list, so the name match has to look there as well.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [
            rec(70, "h1.bin", USN_REASON_FILE_CREATE, false),
            rec(70, "h2.bin", USN_REASON_FILE_CREATE, false),
            rec(70, "h1.bin", USN_REASON_RENAME_OLD_NAME, false),
        ];
        // h1 was renamed away, h2 is the link that stayed.
        let mut stat = stat_existing(&[r"D:\links\h2.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 3);
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].0, r"D:\links\h2.bin");
    }

    // -- whole-window behaviour ---------------------------------------------

    #[test]
    fn create_then_delete_in_one_window_leaves_nothing() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [
            rec(99, "tmp.bin", USN_REASON_FILE_CREATE, false),
            rec(99, "tmp.bin", USN_REASON_FILE_DELETE, false),
        ];
        // It exists when the create is applied and is gone by the time the
        // delete is verified - the create+delete-inside-one-round case.
        let mut looks = 0u32;
        let mut stat = |path: &str, is_dir: bool, frn: u64| {
            looks += 1;
            if looks <= 1 { stat_ok(path, is_dir, frn) } else { None }
        };
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 2);
        assert!(pending.appended.is_empty());
        assert!(pending.removed.is_empty());
        assert!(pending.is_empty());
    }

    #[test]
    fn deleted_aliases_do_not_survive_a_flush() {
        // The T-07 ghost regression: a deleted record must be unreachable
        // through both lookups after the dump has been rewritten. Before this
        // fix the second alias was still searchable minutes later.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\h1.bin", meta_file(7, 70));
        b.push(r"D:\links\h2.bin", meta_file(7, 70));
        b.push(r"D:\links\other.bin", meta_file(9, 71));
        let mem = b.finish();

        let mut pending = Pending::default();
        let records = [
            rec(70, "h1.bin", USN_REASON_FILE_DELETE, false),
            rec(70, "h2.bin", USN_REASON_FILE_DELETE, false),
        ];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);
        assert_eq!(stats.applied, 2);

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("index.db.feridx");
        let built = flush(&mem, &pending, &dump).unwrap();
        assert_eq!(built.len(), 1, "only the unrelated entry survives");
        let loaded = MemIndex::load_dump(&dump).unwrap();
        assert!(loaded.find_path_idx(r"D:\links\h1.bin").is_none());
        assert!(loaded.find_path_idx(r"D:\links\h2.bin").is_none());
        assert!(loaded.find_frn(70).is_none());
        assert!(loaded.find_frn_all(70).is_empty());
        assert!(loaded.find_path_idx(r"D:\links\other.bin").is_some());
    }

    #[test]
    fn one_removed_hard_link_survives_a_flush_for_the_other() {
        // End-to-end shape of the pnpm case: h1 is unlinked (HARD_LINK_CHANGE),
        // h2 stays. After the flush exactly one of the two paths is indexed.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\h1.bin", meta_file(7, 70));
        b.push(r"D:\links\h2.bin", meta_file(7, 70));
        let mem = b.finish();

        let mut pending = Pending::default();
        let records = [rec(70, "h1.bin", USN_REASON_HARD_LINK_CHANGE, false)];
        let mut stat = stat_existing(&[r"D:\links\h2.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 1);

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("index.db.feridx");
        let built = flush(&mem, &pending, &dump).unwrap();
        assert_eq!(built.len(), 1, "h1 is gone, h2 stays");
        let loaded = MemIndex::load_dump(&dump).unwrap();
        assert!(loaded.find_path_idx(r"D:\links\h1.bin").is_none(), "the removed link is gone");
        let kept = loaded.find_path_idx(r"D:\links\h2.bin").expect("the surviving link is indexed");
        assert_eq!(loaded.meta_at(kept).size, 7);
    }

    #[test]
    fn rename_leaves_the_new_name_with_real_metadata() {
        let mut b = MemBuilder::default();
        b.push(r"D:\links\h1.bin", meta_file(7, 70));
        b.push(r"D:\links\h2.bin", meta_file(7, 70));
        let mem = b.finish();

        let mut pending = Pending::default();
        let records = [
            rec(70, "h1.bin", USN_REASON_RENAME_OLD_NAME, false),
            rec(70, "renamed.bin", USN_REASON_RENAME_NEW_NAME, false),
        ];
        // The old name is gone, the new one is on disk.
        let mut stat = stat_existing(&[r"D:\links\renamed.bin"]);
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat);
        assert_eq!(stats.applied, 2);
        assert_eq!(pending.removed.len(), 1, "only the old alias is retired");

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("index.db.feridx");
        let built = flush(&mem, &pending, &dump).unwrap();
        let loaded = MemIndex::load_dump(&dump).unwrap();
        assert!(loaded.find_path_idx(r"D:\links\h1.bin").is_none(), "old name is gone");
        assert!(loaded.find_path_idx(r"D:\links\h2.bin").is_some(), "other link survives");
        let new = loaded.find_path_idx(r"D:\links\renamed.bin").expect("new name is indexed");
        let meta = loaded.meta_at(new);
        assert_eq!(meta.size, 4096);
        assert_eq!(meta.allocated, 4096);
        assert_eq!(meta.mtime, 1_700_000_123, "the new name carries real metadata");
        assert_eq!(meta.frn, Some(70));
        assert_eq!(built.len(), 2);
    }

    #[test]
    fn rename_into_an_existing_path_retires_the_old_entry() {
        // Replace-by-rename: the destination already holds an entry, which must
        // not end up as a duplicate of the same path.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\a.bin", meta_file(1, 60));
        b.push(r"D:\links\b.bin", meta_file(2, 61));
        let mem = b.finish();

        let mut pending = Pending::default();
        let records = [rec(61, "B.BIN", USN_REASON_RENAME_NEW_NAME, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_ok);
        assert_eq!(stats.applied, 1);
        // The existing entry for the same path was retired by index.
        assert_eq!(pending.removed.len(), 1);
        assert!(pending.removed.contains(&1));

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("index.db.feridx");
        flush(&mem, &pending, &dump).unwrap();
        let loaded = MemIndex::load_dump(&dump).unwrap();
        let hits = (0..loaded.len())
            .filter(|&i| loaded.path_at(i).eq_ignore_ascii_case(r"D:\links\b.bin"))
            .count();
        assert_eq!(hits, 1, "the path exists exactly once after the flush");
    }

    #[test]
    fn directories_get_metadata_too() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(80, "newdir", USN_REASON_FILE_CREATE, true)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_ok);
        assert_eq!(stats.applied, 1);
        let (_, meta) = &pending.appended[0];
        assert!(meta.is_dir);
        assert_eq!(meta.mtime, 1_700_000_123, "a directory keeps a real mtime");
        assert_eq!(meta.frn, Some(80));
    }

    // -- catch-up pacing ----------------------------------------------------

    #[test]
    fn catchup_skips_the_interval_but_keeps_a_breather() {
        let iv = Duration::from_secs(5);
        // Live: the normal poll interval.
        let mut rounds = 0;
        assert_eq!(poll_wait(false, &mut rounds, iv), iv);
        // Catching up: back-to-back rounds...
        for _ in 0..CATCHUP_BURST {
            assert_eq!(poll_wait(true, &mut rounds, iv), Duration::ZERO);
            rounds += 1;
        }
        // ...until the burst budget is spent: one breather, budget reset, and
        // the next round is immediate again. This is what stops a permanently
        // lagging journal from spinning the loop.
        assert_eq!(poll_wait(true, &mut rounds, iv), CATCHUP_BREATHER);
        assert_eq!(rounds, 0);
        assert_eq!(poll_wait(true, &mut rounds, iv), Duration::ZERO);
        // A live round always gets the interval back, whatever the budget says.
        assert_eq!(poll_wait(false, &mut rounds, iv), iv);
    }

    #[test]
    fn a_zero_wait_poll_returns_at_once() {
        // The catch-up path hands Duration::ZERO to recv_timeout every round:
        // it must not block and must not panic.
        let (_tx, rx) = std::sync::mpsc::channel::<u8>();
        assert!(matches!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn name_match_is_component_wise_and_case_insensitive() {
        assert!(name_matches_path(b"D:\\links\\h1.bin", "h1.bin"));
        assert!(name_matches_path(b"D:\\links\\H1.BIN", "h1.bin"));
        assert!(name_matches_path(b"D:/links/h1.bin", "h1.bin"));
        assert!(!name_matches_path(b"D:\\links\\h1.bin", "h1"));
        assert!(!name_matches_path(b"D:\\links\\h1.bin.bak", "h1.bin"));
        assert!(!name_matches_path(b"D:\\links\\h1.bin", "h2.bin"));
    }

    #[test]
    fn stat_meta_reports_a_junction_as_itself() {
        // The legacy "C:\Users\All Users" junction is on every Windows install
        // and needs no privileges to stat. symlink_metadata must describe the
        // link itself, and the allocation query must not follow it to
        // C:\ProgramData.
        let link = r"C:\Users\All Users";
        if std::fs::symlink_metadata(link).is_err() {
            eprintln!("skipping: {link} is not present on this machine");
            return;
        }
        // is_dir is passed as false on purpose: for a directory the size branch
        // already yields 0, and only a file-shaped reparse point proves the
        // allocation rule is keyed on the reparse attribute rather than on
        // "directory".
        let s = stat_meta(link, false, 3).expect("the junction is stat-able");
        assert!(s.meta.reparse(), "the REPARSE flag must describe the link itself");
        assert_eq!(s.meta.allocated, 0, "the target's allocation must not leak in");
        assert!(!s.alloc_failed, "skipping the query is not a failure");
        assert!(s.meta.hidden() && s.meta.system(), "link attributes, not ProgramData's");
        // Sanity: a plain file still goes through the allocation query.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.bin");
        std::fs::write(&file, vec![0u8; 8192]).unwrap();
        let p = stat_meta(&file.to_string_lossy(), false, 4).expect("file exists");
        assert!(!p.meta.reparse());
        assert!(!p.alloc_failed);
        assert!(p.meta.allocated >= 4096, "allocated {}", p.meta.allocated);
    }
}

