//! USN journal change monitoring — keeps the dump live in memory and flushes
//! it back to disk (Everything-style: in-memory index + debounced save).
//!
//! Polls FSCTL_READ_USN_JOURNAL (admin) and applies create/delete/rename events
//! to a working copy of the index. Deletions are applied by FRN so they work
//! even after the MFT record has been recycled. A crash between flushes loses
//! nothing: the USN position sidecar is updated with the dump, and the journal
//! replays the gap on the next start.
//!
//! Several volumes are watched concurrently (monitor --volume D,H). Each volume
//! keeps its own journal handle, replay position, parent-path cache and pending
//! change set; the index and the change feed stay cross-volume, so a flush or a
//! broadcast consumes the per-volume sets as one merged batch. A volume that
//! cannot be opened (unplugged stick, unusable journal) is logged, retried in
//! the background and left out of that round instead of taking the whole monitor
//! — or the volumes that are fine — down with it.

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
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, GetCompressedFileSizeW,
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

/// How often a volume that could not be opened (unplugged stick, unusable
/// journal) is retried. Cheap — a failed CreateFile on a missing drive returns
/// immediately — but not every round, so the log stays quiet.
const OPEN_RETRY: Duration = Duration::from_secs(60);
/// How long a change batch may be held back while some volume is still
/// replaying a backlog. Without a cap one permanently lagging volume would keep
/// every live volume out of the change feed; with it, the feed is at worst this
/// stale during a catch-up.
const PUSH_STALE_FORCE: Duration = Duration::from_secs(60);

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

/// Read-only union of the per-volume pending sets.
///
/// The index and the wire protocol are cross-volume by construction (removed
/// holds indices into the shared index, appended holds absolute paths), so the
/// flush and the broadcast consume the per-volume sets as one batch. Borrowed,
/// never merged into an owned copy: this is re-read every round and can hold
/// millions of paths.
struct MergedPending<'a> {
    sets: &'a [&'a Pending],
}

impl<'a> MergedPending<'a> {
    fn new(sets: &'a [&'a Pending]) -> Self {
        Self { sets }
    }

    fn is_empty(&self) -> bool {
        self.sets.iter().all(|s| s.is_empty())
    }

    fn appended(&self) -> impl Iterator<Item = &'a (String, EntryMeta)> {
        self.sets.iter().flat_map(|s| s.appended.iter())
    }

    fn removed(&self) -> impl Iterator<Item = u32> + 'a {
        self.sets.iter().flat_map(|s| s.removed.iter().copied())
    }

    fn appended_len(&self) -> usize {
        self.sets.iter().map(|s| s.appended.len()).sum()
    }

    fn removed_len(&self) -> usize {
        self.sets.iter().map(|s| s.removed.len()).sum()
    }

    fn frns_len(&self) -> usize {
        self.sets.iter().map(|s| s.removed_frns.len()).sum()
    }

    /// Every retired index in one set. The flush walks the whole index, and a
    /// per-entry lookup across N volumes would be N hash probes each.
    fn removed_union(&self) -> HashSet<u32> {
        self.sets.iter().flat_map(|s| s.removed.iter().copied()).collect()
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
    /// Create/rename events skipped because the path was already gone
    /// (ERROR_FILE_NOT_FOUND 2 / ERROR_PATH_NOT_FOUND 3) when the event was
    /// applied. Indexing one of those would only ever add a hit for a path that
    /// is not on disk: measured in the live overlay as 1400+ "ghost" entries — a
    /// real file's path with a relative-path tail glued on, all size 0 / mtime 0.
    stat_gone: usize,
    /// Create/rename events whose stat failed for some *other* reason (locked,
    /// access denied, IO error). The file may well exist, so the entry is kept
    /// with zeroed metadata — losing a real file is worse than a bad size — and
    /// counted, because that is the 0-byte / 1970-01-01 pollution this fill
    /// exists to prevent.
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

/// Why a create/rename event's path could not be stat'ed.
///
/// The two cases lead to opposite decisions and must not be collapsed into one
/// "no metadata" answer: a create whose file is already gone is a stale (or
/// virtual) event whose entry could only ever produce a search hit for a path
/// that does not exist, while a locked or denied file is a real file whose entry
/// must be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatMiss {
    /// The path is not there (ERROR_FILE_NOT_FOUND 2 / ERROR_PATH_NOT_FOUND 3,
    /// both mapped to ErrorKind::NotFound by std).
    Gone,
    /// Anything else: locked, access denied, IO error, ...
    Other,
}

/// Outcome of one metadata fill: the metadata, or why it could not be read.
type StatResult = Result<StatMeta, StatMiss>;

/// Map a stat failure onto the two cases the create path tells apart.
///
/// The raw Win32 code is checked together with the kind: Windows reports a
/// missing file as 2 and a missing parent component as 3, and the code is what
/// actually arrives.
fn classify_stat_error(e: &std::io::Error) -> StatMiss {
    let gone = e.kind() == std::io::ErrorKind::NotFound
        || matches!(e.raw_os_error(), Some(2) | Some(3));
    if gone { StatMiss::Gone } else { StatMiss::Other }
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
/// The failure side is not one "no metadata" answer: a create whose path is
/// gone must be dropped (see StatMiss::Gone), while every other failure keeps
/// the entry. That distinction is what stops stale events from becoming
/// searchable paths that are not on disk.
fn stat_meta(path: &str, _is_dir_hint: bool, frn: u64) -> StatResult {
    // symlink_metadata (lstat) and not metadata (stat): a reparse point has to
    // report its *own* record, exactly as the raw $MFT scan does. Following the
    // link would record the target's size, timestamps and flags, so a junction
    // or symlink created while the monitor is running would disagree with the
    // same entry after the next fer index. For any non-reparse path the two
    // calls are identical.
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) => return Err(classify_stat_error(&e)),
    };
    let attrs = md.file_attributes();
    let flags = flags_from_attributes(attrs);
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    // The file system decides what this is: the USN record's is_dir bit (passed
    // in as a hint, and echoed by the stat stubs) is not authoritative, because
    // the path may have been replaced between the event and now. The parent
    // checks below need the real answer — with the caller's value every existing
    // file would look like a directory and "...\a-file\child" could not be told
    // apart from a real parent.
    //
    // Rust reports a reparse point as a symlink even when it targets a
    // directory, so std's file type alone is wrong for junctions: the Win32
    // FILE_ATTRIBUTE_DIRECTORY bit is what decides, which is exactly the bit the
    // raw $MFT scan reads for the same entry (a directory junction is a
    // directory in the index and can be a parent).
    let is_dir = md.is_dir() || (reparse && attrs & FILE_ATTRIBUTE_DIRECTORY != 0);
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
    Ok(StatMeta {
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

/// Strip a trailing separator from a directory path ("D:\links\" -> "D:\links")
/// so the caller's "parent\name" join cannot produce a doubled separator. The
/// volume root is spelled "D:" and is left alone.
fn trim_dir_sep(mut path: String) -> String {
    while path.ends_with('\\') && path.len() > 2 {
        path.pop();
    }
    path
}

/// Whether the path stats as an existing directory.
///
/// true is passed as the type hint because that is what the test stubs echo;
/// the real stat_meta ignores the hint and asks the file system.
fn is_existing_dir(
    path: &str,
    frn: u64,
    stat: &mut impl FnMut(&str, bool, u64) -> StatResult,
) -> bool {
    matches!(stat(path, true, frn), Ok(s) if s.meta.is_dir)
}

/// Kernel fallback for a parent directory FRN: memoised per volume, and every
/// answer is verified before it is handed out.
///
/// The raw walk behind lookup (UsnVolume::lookup via resolve_path) has been
/// measured to return a record that is *not* the one asked for, which turned
/// every child event of that directory into a path like
/// "...\Nigori.bin\<child>" — Nigori.bin being a file, so nothing on disk ever
/// matched and the children were either stored as zero-metadata ghosts or (with
/// the stat classification) silently dropped. A path from the kernel is
/// therefore only accepted when it stats as an existing directory, and a memo
/// that fails that check is dropped and the walk retried, so one bad answer
/// cannot poison a directory for the rest of the run.
///
/// An empty path is how the walk reports the volume root; it passes through
/// unverified because the caller joins it as "D:\<name>".
fn kernel_parent(
    frn: u64,
    cache: &mut HashMap<u64, Option<String>>,
    lookup: &mut impl FnMut(u64) -> Option<String>,
    stat: &mut impl FnMut(&str, bool, u64) -> StatResult,
) -> Option<String> {
    match cache.get(&frn).cloned() {
        Some(Some(path)) if path.is_empty() => return Some(path),
        // A memo is re-verified on every hit: it costs one stat per directory
        // event, and it is the only way a path that went stale (renamed or
        // deleted since) is not reused for every child of it.
        Some(Some(path)) if is_existing_dir(&path, frn, stat) => return Some(path),
        // Wrong answer (or a memo of one): forget it and resolve again.
        Some(_) => {
            cache.remove(&frn);
        }
        None => {}
    }
    let path = lookup(frn)?;
    if path.is_empty() {
        cache.insert(frn, Some(path.clone()));
        return Some(path);
    }
    if is_existing_dir(&path, frn, stat) {
        cache.insert(frn, Some(path.clone()));
        Some(path)
    } else {
        // Never hand out a path that is not a directory — pasting one in front
        // of a name can only build a path that does not exist. Not memoised, so
        // a later event retries (the parent may be resolvable by then).
        None
    }
}

/// Resolve the parent directory of a create/rename event to a full path.
///
/// Cheapest and most exact source first:
/// 1. the loaded index — every directory the last scan saw is there with its
///    full path, so this is a binary search with no kernel call and no
///    ambiguity (FRNs are volume-local, hence the drive filter);
/// 2. this window's pending appends — a directory created or renamed since the
///    index is not in the index yet, but its append already carries the full
///    path (this is also what finds a *renamed* parent after the stale indexed
///    path failed its check);
/// 3. the kernel walk (kernel_parent), which verifies its own answer.
///
/// Every candidate is stat-verified to be an existing directory before it is
/// used. Membership in the index is not proof it is still there, and a file can
/// never be a parent: refusing here is what keeps "...\a-file\child" out of
/// the index (measured live: 1,497 such ghosts whose parent was a real file).
fn resolve_parent(
    mem: &MemIndex,
    drive: char,
    frn: u64,
    pending: &Pending,
    kernel: &mut impl FnMut(u64) -> Option<String>,
    stat: &mut impl FnMut(&str, bool, u64) -> StatResult,
) -> Option<String> {
    for &idx in &mem.find_frn_all(frn) {
        let i = idx as usize;
        if !path_on_drive(mem.path_bytes(i), drive) || !mem.meta_at(i).is_dir {
            continue;
        }
        let path = trim_dir_sep(mem.path_at(i));
        if is_existing_dir(&path, frn, stat) {
            return Some(path);
        }
    }
    for (path, meta) in &pending.appended {
        if meta.frn != Some(frn) || !meta.is_dir {
            continue;
        }
        let path = trim_dir_sep(path.clone());
        if is_existing_dir(&path, frn, stat) {
            return Some(path);
        }
    }
    kernel(frn)
}

/// Apply one round of USN records to the pending change set.
///
/// Split out of run() so the create/delete/rename rules can be unit-tested
/// without a real volume, a real journal or an elevated token: the kernel
/// fallback of parent resolution and the metadata stat are injected as
/// closures (the index and the pending appends are consulted directly).
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
    kernel: &mut impl FnMut(u64) -> Option<String>,
    stat: &mut impl FnMut(&str, bool, u64) -> StatResult,
) -> ApplyStats {
    let mut stats = ApplyStats::default();
    for r in records {
        let hard_link = r.reason & USN_REASON_HARD_LINK_CHANGE != 0;
        // Disappearance side: delete, rename-away and hard-link change.
        if hard_link || r.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0 {
            stats.applied += retire_aliases(mem, drive, r, pending, stat);
        }
        // Appearance side: create, rename-into-place and hard-link change - the
        // last one is either a link appearing or a link disappearing, and only
        // the stat below can tell which.
        if hard_link || r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
            match resolve_parent(mem, drive, r.parent_frn, pending, kernel, stat) {
                Some(parent) => {
                    let path = if parent.is_empty() {
                        format!("{drive}:\\{}", r.name)
                    } else {
                        format!("{parent}\\{}", r.name)
                    };
                    match stat(&path, r.is_dir, r.frn) {
                        Ok(s) => {
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
                        Err(StatMiss::Gone) if hard_link => {
                            stats.applied += retire_path(mem, &path, pending);
                            stats.applied += drop_pending_path(pending, &path);
                        }
                        // A create/rename whose file is already gone is a stale
                        // event: created and deleted between two journal reads, or
                        // a path that was never really there (measured: 1400+
                        // "<real file>.rmeta\target-gnu\debug\..." ghosts in the
                        // live overlay). Indexing it would add exactly the entry
                        // this monitor must not have — a hit that cannot be
                        // opened — so the event is dropped. Nothing is lost: if
                        // the file was real, its own DELETE would have retired the
                        // same entry.
                        Err(StatMiss::Gone) => {
                            stats.stat_gone += 1;
                            retire_path(mem, &path, pending);
                            drop_pending_path(pending, &path);
                        }
                        // Any other failure (locked, denied, IO): the file may
                        // exist, so the entry is kept with zeroed metadata —
                        // dropping a real file is worse than a bad size.
                        Err(StatMiss::Other) => {
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
/// Index candidates are also filtered by the volume the record came from: FRNs
/// are volume-local, so an entry on another volume that happens to carry the
/// same record number is a different file and must not be stat'ed (or, when its
/// path is gone, retired) by this event.
///
/// The stat is what resolves the ambiguity the reason bits cannot express: a
/// delete of one link of a multi-link record leaves the other paths on disk, so
/// they are kept; a rename that only changed case leaves the old spelling
/// stat-able (Windows paths are case-insensitive), and the RENAME_NEW_NAME
/// upsert then replaces it. Nothing is retired on a guess.
fn retire_aliases(
    mem: &MemIndex,
    drive: char,
    r: &UsnRecord,
    pending: &mut Pending,
    stat: &mut impl FnMut(&str, bool, u64) -> StatResult,
) -> usize {
    let rename_old = r.reason & USN_REASON_RENAME_OLD_NAME != 0;
    let matching = |path: &[u8]| !rename_old || name_matches_path(path, &r.name);
    let mine = |i: u32| path_on_drive(mem.path_bytes(i as usize), drive);
    let mut retired = 0usize;

    let all_idxs = mem.find_frn_all(r.frn);
    // Candidates in the loaded index (this volume only) ...
    let mut idxs: Vec<u32> = all_idxs
        .iter()
        .copied()
        .filter(|&i| mine(i))
        .filter(|&i| matching(mem.path_bytes(i as usize)))
        .collect();
    // ... and in this volume's pending append list (not in the index yet).
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
        match all_idxs.iter().copied().find(|&i| mine(i)) {
            Some(i) => idxs.push(i),
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
        if stat(&mem.path_at(i), meta.is_dir, meta.frn.unwrap_or(r.frn)).is_ok() {
            continue;
        }
        if pending.removed.insert(idx) {
            retired += 1;
        }
    }
    // Descending, so removing a higher index cannot move a lower victim.
    for k in apps.into_iter().rev() {
        let (path, meta) = &pending.appended[k];
        if stat(path, meta.is_dir, meta.frn.unwrap_or(r.frn)).is_ok() {
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

// ---------------------------------------------------------------------------
// Multi-volume watch set
// ---------------------------------------------------------------------------

/// One watched volume: its journal handle, replay position, parent-path cache
/// and the changes applied from it since the last flush.
///
/// Only volume-local state lives here. The change sets stay per-volume because a
/// round is per-volume work; the flush and the broadcast consume them as one
/// batch through MergedPending, because the index itself is cross-volume.
struct Watched {
    drive: char,
    vol: UsnVolume,
    /// Next USN to read from — the position the sidecar stores.
    start: i64,
    /// Parent-FRN -> path memo. Per volume on purpose: FRNs are volume-local, so
    /// a shared cache would hand one volume's path to another.
    cache: HashMap<u64, Option<String>>,
    pending: Pending,
    /// Journal writes ahead of start (0 when the journal cannot be queried).
    lag: i64,
    /// lag > CATCHUP_LAG: this volume is replaying a backlog.
    catching_up: bool,
    /// Rate limiter for the periodic catch-up line.
    last_catchup_log: std::time::Instant,
}

/// A volume that could not be opened. Its entries stay in the dump untouched,
/// the remaining volumes keep being watched, and the open is retried so a
/// re-plugged stick comes back without a restart.
struct Down {
    drive: char,
    reason: String,
    last_try: std::time::Instant,
}

/// Upper-case and de-duplicate the --volume list, preserving its order. An empty
/// result is the caller's problem (run refuses to start with no volume).
pub fn normalize_drives(volumes: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(volumes.len());
    for &v in volumes {
        let d = v.to_ascii_uppercase();
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// "D:,H:" — a drive list as it appears in messages.
fn drive_list(drives: &[char]) -> String {
    drives.iter().map(|d| format!("{d}:")).collect::<Vec<_>>().join(",")
}

/// Per-volume journal positions: "123" for a single volume (the form every log
/// line has always used), "D: 123, H: 456" for several.
fn pos_summary(positions: &[(char, i64)]) -> String {
    match positions {
        [(_, usn)] => usn.to_string(),
        many => many.iter().map(|(d, usn)| format!("{d}: {usn}")).collect::<Vec<_>>().join(", "),
    }
}

/// Per-volume backlog for the periodic stats line: "123" for one volume,
/// "D:123 H:456" for several (compact — this one is printed every 60 s).
fn lag_summary(lags: &[(char, i64)]) -> String {
    match lags {
        [(_, lag)] => lag.to_string(),
        many => many.iter().map(|(d, lag)| format!("{d}:{lag}")).collect::<Vec<_>>().join(" "),
    }
}

/// Startup line: the single-volume form is exactly what it has always been, the
/// multi-volume one lists every position.
fn watch_summary(positions: &[(char, i64)]) -> String {
    match positions {
        [] => "no volumes".to_string(),
        [(d, usn)] => format!("{d}: from USN {usn}"),
        many => {
            let drives: Vec<char> = many.iter().map(|&(d, _)| d).collect();
            format!("{} from USN ({})", drive_list(&drives), pos_summary(many))
        }
    }
}

/// Whether this round broadcasts the merged pending set.
///
/// Live rounds push as soon as anything changed. While any volume is still
/// replaying a backlog the push is held back — the whole pending set is
/// re-serialized every round and the receiver cannot show the backlog anyway —
/// but only for PUSH_STALE_FORCE: a volume that stays behind forever must not
/// keep the volumes that *are* live out of the change feed.
fn should_push(dirty: bool, any_catching_up: bool, held_for: Duration) -> bool {
    dirty && (!any_catching_up || held_for >= PUSH_STALE_FORCE)
}

/// Open one volume and restore its replay position from the sidecar, falling
/// back to the journal's current position when the sidecar has none. Shared by
/// startup and the re-open path, so a volume that comes back behaves exactly
/// like a freshly started monitor.
fn open_watched(drive: char, sidecar: &Path) -> Result<Watched> {
    let vol = UsnVolume::open(drive)?;
    let start = read_usn(sidecar, drive).unwrap_or_else(|| sync_to_now(&vol));
    let lag = journal_lag(&vol, start);
    Ok(Watched {
        drive,
        vol,
        start,
        cache: HashMap::new(),
        pending: Pending::default(),
        lag,
        catching_up: lag > CATCHUP_LAG,
        last_catchup_log: std::time::Instant::now() - CATCHUP_LOG_EVERY,
    })
}

/// Read one volume's journal, recovering from a recycled journal exactly the way
/// the single-volume monitor always has: resuming from a USN the journal no
/// longer holds is impossible (ERROR_JOURNAL_DELETE_IN_PROGRESS, 1181), so sync
/// to the current position and say that the gap needs a rebuild.
///
/// Err means the volume itself is unusable (device gone, handle invalid, or the
/// journal unreadable even at its current position). The caller drops it from
/// the watch set and retries the open later — one dead volume must not take the
/// healthy ones, or the process, down with it.
fn poll_volume(w: &mut Watched) -> Result<(i64, Vec<UsnRecord>)> {
    match w.vol.read_journal(w.start, MASK) {
        Ok(r) => Ok(r),
        Err(e) => {
            eprintln!(
                "[monitor] {}: reading the USN journal from {} failed ({e}) — it was recycled \
                 while the monitor was down. Syncing to the current position; changes in the \
                 gap are NOT in the index (run fer index to rebuild).",
                w.drive, w.start
            );
            w.start = sync_to_now(&w.vol);
            w.vol.read_journal(w.start, MASK)
        }
    }
}

/// Retry the volumes that could not be opened. Rate-limited by OPEN_RETRY and
/// logged only when the reason changes, so a stick that stays out does not spam
/// the log once per round.
fn revive_down(vols: &mut Vec<Watched>, down: &mut Vec<Down>, sidecar: &Path) {
    let mut i = 0;
    while i < down.len() {
        if down[i].last_try.elapsed() < OPEN_RETRY {
            i += 1;
            continue;
        }
        down[i].last_try = std::time::Instant::now();
        match open_watched(down[i].drive, sidecar) {
            Ok(w) => {
                eprintln!("[monitor] {}: watchable again — resuming", w.drive);
                vols.push(w);
                down.remove(i);
            }
            Err(e) => {
                let reason = e.to_string();
                if down[i].reason != reason {
                    eprintln!("[monitor] {}: still cannot be watched ({reason})", down[i].drive);
                    down[i].reason = reason;
                }
                i += 1;
            }
        }
    }
}

/// Watch one or more volumes forever: every interval each volume's journal is
/// polled and applied to the in-memory index, and the index is flushed to dump
/// every flush_every seconds whenever changes are pending. The in-memory index
/// is authoritative between flushes and stays cross-volume — only the journal
/// state is per volume.
///
/// Volumes that cannot be opened are logged and retried rather than aborting the
/// run; only a run with nothing at all to watch fails up front.
///
/// When push_addr is set a [crate::push::Broadcaster] is bound there and every
/// applied batch is broadcast to connected fer serve receivers, so a long-lived
/// server can show newly created files without waiting for a flush.
pub fn run(
    mut mem: MemIndex,
    drives: Vec<char>,
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
    let drives = normalize_drives(&drives);
    if drives.is_empty() {
        bail!("fer monitor needs at least one volume to watch (--volume D,H)");
    }
    let feed = push_addr.as_deref().and_then(crate::push::Broadcaster::bind);
    // Control channel: lets fer flush / fer rebuild poke this loop instead of
    // waiting out --flush-secs. Optional — a monitor without it still watches
    // the journal.
    let control = control_addr.as_deref().and_then(crate::control::bind);
    let usn_sidecar = usn_sidecar_path(&dump);

    // Open every requested volume up front. A volume that cannot be opened is
    // logged and retried in the background instead of aborting the run: one
    // unplugged stick must not cost the other volumes their real-time index.
    // Only a run with nothing to watch at all is an error.
    let mut vols: Vec<Watched> = Vec::new();
    let mut down: Vec<Down> = Vec::new();
    for &drive in &drives {
        match open_watched(drive, &usn_sidecar) {
            Ok(w) => vols.push(w),
            Err(e) => {
                eprintln!(
                    "[monitor] cannot watch {drive}: {e} — continuing with the other volumes \
                     (retrying every {OPEN_RETRY:?})"
                );
                down.push(Down { drive, reason: e.to_string(), last_try: std::time::Instant::now() });
            }
        }
    }
    if vols.is_empty() {
        bail!(
            "cannot watch any of the requested volumes ({}) — {}",
            drive_list(&drives),
            down.iter().map(|d| format!("{}: {}", d.drive, d.reason)).collect::<Vec<_>>().join("; ")
        );
    }
    let positions: Vec<(char, i64)> = vols.iter().map(|v| (v.drive, v.start)).collect();
    eprintln!("[monitor] watching {} (dump: {})", watch_summary(&positions), dump.display());
    // A restart replays from the last *flush* position, which on this machine is
    // up to --flush-secs (1800 s) of disk churn — about 8.6M USN units measured.
    // Say so up front: "my new file is not searchable" otherwise looks like a
    // broken feed instead of a queue that is still draining.
    for v in vols.iter() {
        if v.lag > CATCHUP_LAG {
            eprintln!(
                "[monitor] {}: {} USN units behind the journal — replaying the backlog first; \
                 change push stays off until caught up",
                v.drive, v.lag
            );
        }
    }
    // Periodic self-report. The monitor once ballooned to 20 GB of private
    // commit within a minute of starting while applied counts and the push
    // batch both looked normal, and nothing in the logs said which structure was
    // growing. These are the only per-round collections that can; printing their
    // sizes makes the culprit identifiable the next time it happens.
    let mut last_flush = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();
    let mut any_catching_up = vols.iter().any(|v| v.catching_up);
    let mut catchup_rounds: u32 = 0;
    // Set when a round had something to broadcast but could not (catch-up), so
    // the first live round pushes the accumulated set even if it applied nothing
    // itself. push_stale_since bounds how long that hold-back may last.
    let mut push_stale = false;
    let mut push_stale_since: Option<std::time::Instant> = None;
    loop {
        // Wait out the poll interval, or wake immediately for a control command:
        // recv_timeout replaces the plain sleep so fer rebuild is served at once
        // rather than on the next tick.
        //
        // While any volume is far behind, skip the wait so its backlog drains at
        // back-to-back round rate instead of one round per interval (the
        // 5-minute catch-up measured on this machine). A burst budget plus a
        // short breather keep a permanently lagging journal from spinning the
        // CPU for hours, and the breather still polls the control channel, so
        // neither fer flush nor fer rebuild can be starved by catch-up.
        let wait = poll_wait(any_catching_up, &mut catchup_rounds, interval);
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
        // A volume that was unplugged when the monitor started (or went away
        // since) is retried here, so it rejoins the watch set on its own.
        revive_down(&mut vols, &mut down, &usn_sidecar);

        // One round over every watched volume. The catch-up timers are taken per
        // volume so the estimate measures that volume's replay rate rather than
        // the round as a whole.
        let mut round_stats = ApplyStats::default();
        let mut failed: Vec<(usize, String)> = Vec::new();
        for (i, w) in vols.iter_mut().enumerate() {
            let round_t0 = std::time::Instant::now();
            let round_from = w.start;
            let (next, records) = match poll_volume(w) {
                Ok(x) => x,
                Err(e) => {
                    failed.push((i, e.to_string()));
                    continue;
                }
            };
            // A wrapped journal (NextUsn behind the position we started from) was
            // deleted and recreated: the records just read belong to a different
            // journal generation, so they are dropped and the volume resyncs.
            // The single-volume monitor used to abort here, which in a
            // multi-volume run would take the healthy volumes down with the
            // broken one.
            if !records.is_empty() && next < w.start {
                eprintln!(
                    "[monitor] {}: USN journal wrapped (next={next} < start={}) — syncing to the \
                     current position; the gap needs fer index",
                    w.drive, w.start
                );
                w.start = sync_to_now(&w.vol);
                w.lag = journal_lag(&w.vol, w.start);
                w.catching_up = w.lag > CATCHUP_LAG;
                continue;
            }
            let stats;
            {
                // Path resolution and the metadata stat are the only things that
                // need the volume, so they are injected here and apply_records
                // itself stays a pure function of the records + the index + the
                // change set of *this* volume.
                let Watched { drive, vol, cache, pending, .. } = &mut *w;
                // Only the kernel fallback is injected: the index and the
                // pending appends are consulted first (resolve_parent). The walk
                // gets a throwaway map because kernel_parent owns the validated
                // per-volume memo — a poisoned entry has to be droppable.
                let mut stat_fn = |path: &str, is_dir: bool, frn: u64| stat_meta(path, is_dir, frn);
                let mut kernel = |frn: u64| {
                    let mut walk = |f: u64| resolve_path(vol, *drive, f, &mut HashMap::new());
                    kernel_parent(frn, cache, &mut walk, &mut stat_fn)
                };
                let mut stat = |path: &str, is_dir: bool, frn: u64| stat_meta(path, is_dir, frn);
                stats = apply_records(&mem, *drive, &records, pending, &mut kernel, &mut stat);
            }
            if next != w.start {
                w.start = next;
            }
            round_stats.applied += stats.applied;
            round_stats.resolve_fail += stats.resolve_fail;
            round_stats.stat_fail += stats.stat_fail;
            round_stats.stat_gone += stats.stat_gone;
            round_stats.alloc_fail += stats.alloc_fail;
            if stats.applied > 0 {
                eprintln!(
                    "[monitor] {}: applied {} changes (usn={})",
                    w.drive, stats.applied, w.start
                );
            }
            // Measure the backlog *after* applying: NextUsn minus the position
            // just applied is exactly what is left to replay. While it stays
            // above the threshold this volume (and therefore the round) does not
            // wait out interval and the change feed is paused — see should_push
            // for why that pause is bounded.
            w.lag = journal_lag(&w.vol, w.start);
            let was_catching_up = w.catching_up;
            w.catching_up = w.lag > CATCHUP_LAG;
            if w.catching_up {
                if !was_catching_up {
                    w.last_catchup_log = std::time::Instant::now() - CATCHUP_LOG_EVERY;
                }
                if w.last_catchup_log.elapsed() >= CATCHUP_LOG_EVERY {
                    // Replay rate of the round that just finished, so the
                    // estimate tracks the machine instead of a hardcoded guess.
                    let secs = round_t0.elapsed().as_secs_f64().max(0.001);
                    let rate = ((w.start - round_from).max(0) as f64 / secs).max(1.0);
                    eprintln!(
                        "[monitor] {}: catching up: {} USN units behind ({rate:.0}/s) — ETA ~{:.0}s",
                        w.drive,
                        w.lag,
                        w.lag as f64 / rate
                    );
                    w.last_catchup_log = std::time::Instant::now();
                }
            } else if was_catching_up {
                eprintln!("[monitor] {}: caught up with the journal (usn={})", w.drive, w.start);
            }
            if w.cache.len() > 1_000_000 {
                w.cache.clear();
            }
        }
        // Volumes that could not be read are taken out of the watch set (their
        // dump entries stay untouched) and retried by revive_down.
        for (i, reason) in failed.into_iter().rev() {
            let w = vols.remove(i);
            eprintln!(
                "[monitor] {}: reading the USN journal failed ({reason}) — dropping it from the \
                 watch set; its entries stay indexed and it is retried every {OPEN_RETRY:?}",
                w.drive
            );
            down.push(Down { drive: w.drive, reason, last_try: std::time::Instant::now() });
        }
        any_catching_up = vols.iter().any(|v| v.catching_up);
        if any_catching_up {
            catchup_rounds = catchup_rounds.saturating_add(1);
        } else {
            catchup_rounds = 0;
        }

        let mut flushed = false;
        // kept, written, removed, appended — for the flush log line, which is
        // printed after the borrow of the per-volume pending sets has ended.
        let mut flush_report = (0usize, 0usize, 0usize, 0usize);
        {
            // The flush and the broadcast both treat the per-volume change sets
            // as one batch. Borrowed, never copied — this is re-read every round.
            let refs: Vec<&Pending> = vols.iter().map(|v| &v.pending).collect();
            let merged = MergedPending::new(&refs);
            if last_report.elapsed() >= Duration::from_secs(60) {
                let lags: Vec<(char, i64)> = vols.iter().map(|v| (v.drive, v.lag)).collect();
                let cache_len: usize = vols.iter().map(|v| v.cache.len()).sum();
                eprintln!(
                    "[monitor] stats: mem={} appended={} removed={} frns={} cache={} \
                     resolve_fail={} stat_fail={} stat_gone={} alloc_fail={} lag={}",
                    mem.len(),
                    merged.appended_len(),
                    merged.removed_len(),
                    merged.frns_len(),
                    cache_len,
                    round_stats.resolve_fail,
                    round_stats.stat_fail,
                    round_stats.stat_gone,
                    round_stats.alloc_fail,
                    lag_summary(&lags)
                );
                last_report = std::time::Instant::now();
            }
            // Broadcast the pending set BEFORE the flush decision: removed holds
            // indices into the *current* index, so the paths must be resolved
            // while that index is still the authoritative one (flush rebuilds
            // it).
            if let Some(f) = &feed {
                let dirty = round_stats.applied > 0 || push_stale;
                let held_for = push_stale_since.map_or(Duration::ZERO, |t| t.elapsed());
                if dirty && should_push(dirty, any_catching_up, held_for) {
                    let batch = build_batch(&mem, &merged);
                    if !batch.is_empty() {
                        f.send(&batch);
                        push_stale = false;
                        push_stale_since = None;
                    }
                } else if dirty {
                    // A set held back during catch-up is carried into the first
                    // live round, so catching up cannot leave the receiver
                    // without the changes accumulated while the feed was off.
                    push_stale = true;
                    push_stale_since.get_or_insert_with(std::time::Instant::now);
                }
            }
            let pending_changes = !merged.is_empty();
            // A fer flush on the control channel overrides the debounce window.
            let force_flush = matches!(
                command.as_ref().map(|r| r.cmd),
                Some(crate::control::Cmd::Flush)
            );
            let due = pending_changes && (last_flush.elapsed() >= flush_every || force_flush);
            if due {
                let kept = mem.len() - merged.removed_len() + merged.appended_len();
                let n_removed = merged.removed_len();
                let n_appended = merged.appended_len();
                // flush returns the index it just built — a heap Owned copy of
                // the whole volume (~1.4 GB here). The file it wrote is
                // byte-identical, so re-map the dump instead of keeping that copy
                // alive: the old mmap is dropped anyway, the new one costs ~1 ms,
                // and its pages come straight from the page cache the writer just
                // populated. On the (unlikely) reload failure keep the owned copy
                // — correctness first, memory second.
                let owned = flush(&mem, &merged, &dump)?;
                // kept is what the loop above *intended* to write; owned.len() is
                // what the builder actually produced. They diverge when the source
                // index contains entries the arena writes cannot round-trip (an
                // inflated or span-corrupt dump), so log both plus the pending-set
                // sizes: a shrinking mem across flushes with a small removed is
                // the signature of that, and it was invisible before this line.
                let written = owned.len();
                mem = MemIndex::load_dump(&dump).unwrap_or(owned);
                // Only now — the dump is on disk — may the sidecar move forward:
                // a position ahead of the dump it was replayed from would lose the
                // changes between the two. Every watched volume writes its own
                // line, and none of them overwrites another's.
                let positions: Vec<(char, i64)> = vols.iter().map(|v| (v.drive, v.start)).collect();
                write_usns(&usn_sidecar, &positions)?;
                flush_report = (kept, written, n_removed, n_appended);
                flushed = true;
            }
        }
        if flushed {
            // The dump now carries every volume's pending set.
            for v in vols.iter_mut() {
                v.pending.clear();
            }
            push_stale = false;
            push_stale_since = None;
            last_flush = std::time::Instant::now();
            eprintln!(
                "[monitor] flushed: kept={} written={} (mem={} removed={} appended={}) -> {}",
                flush_report.0,
                flush_report.1,
                mem.len(),
                flush_report.2,
                flush_report.3,
                dump.display()
            );
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
                crate::control::Cmd::Status => {
                    let refs: Vec<&Pending> = vols.iter().map(|v| &v.pending).collect();
                    let merged = MergedPending::new(&refs);
                    let positions: Vec<(char, i64)> =
                        vols.iter().map(|v| (v.drive, v.start)).collect();
                    format!(
                        "ok: mem={} appended={} removed={} retired_frns={} usn={} \
                         last_flush={}s ago",
                        mem.len(),
                        merged.appended_len(),
                        merged.removed_len(),
                        merged.frns_len(),
                        pos_summary(&positions),
                        last_flush.elapsed().as_secs()
                    )
                }
                crate::control::Cmd::Rebuild => {
                    let t0 = std::time::Instant::now();
                    // Journal position BEFORE the scan, taken per volume: changes
                    // made while the rebuild runs are replayed from here on the
                    // next iteration, so a rebuild cannot lose them.
                    let befores: Vec<(char, i64)> = vols
                        .iter()
                        .map(|v| (v.drive, v.vol.query_journal().map(|(_, n)| n).unwrap_or(v.start)))
                        .collect();
                    // Re-scan every volume this monitor can read right now. The
                    // entries of every other volume — and of watched volumes that
                    // are down at the moment — are carried over verbatim,
                    // otherwise a rebuild would silently shrink the cross-volume
                    // dump.
                    let scan = drive_list(&vols.iter().map(|v| v.drive).collect::<Vec<_>>());
                    let outcome = crate::indexer::build(
                        &crate::indexer::resolve_volumes(&scan),
                        crate::indexer::Method::Mft,
                    );
                    match outcome {
                        Ok((report, fresh)) => {
                            let mut b = MemBuilder::default();
                            let mut files = 0u64;
                            let mut dirs = 0u64;
                            for i in 0..mem.len() {
                                if !drives.iter().any(|&d| path_on_drive(mem.path_bytes(i), d)) {
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
                                    for w in vols.iter_mut() {
                                        if let Some(&(_, pos)) =
                                            befores.iter().find(|(d, _)| *d == w.drive)
                                        {
                                            w.start = pos;
                                        }
                                        w.cache.clear();
                                        w.pending.clear();
                                        w.lag = journal_lag(&w.vol, w.start);
                                        w.catching_up = w.lag > CATCHUP_LAG;
                                    }
                                    // The dump now carries everything; nothing is
                                    // left over from a paused feed either.
                                    push_stale = false;
                                    push_stale_since = None;
                                    last_flush = std::time::Instant::now();
                                    let positions: Vec<(char, i64)> =
                                        vols.iter().map(|v| (v.drive, v.start)).collect();
                                    let _ = write_usns(&usn_sidecar, &positions);
                                    // Keep the quality sidecar honest: fer stats
                                    // reports built_at_unix from it, so a rebuild
                                    // that does not stamp it leaves a freshly
                                    // rebuilt index looking stale. The volume list
                                    // is carried over — this dump still covers
                                    // every volume, only the watched ones were
                                    // re-scanned.
                                    let volumes = crate::meta::read_index_meta(&dump)
                                        .map(|m| m.volumes)
                                        .unwrap_or_else(|| {
                                            drives.iter().map(|d| format!("{d}:")).collect()
                                        });
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
                                        "ok: rebuilt {scan} in {} ms — {entries} entries \
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
                                    "err: rebuild scanned {scan} but writing the dump failed: {e}"
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

/// Turn the merged pending change set into a push batch.
///
/// removed stores indices into mem, so this must run BEFORE the flush that
/// rebuilds the index. The whole pending set is sent every round (not a delta):
/// re-applying a path is idempotent on the receiver, which makes a dropped
/// message self-heal on the next round. The per-volume sets are merged into one
/// batch because that is exactly the protocol's shape: a full replacement of
/// absolute paths, which is cross-volume by construction.
fn build_batch(mem: &MemIndex, pending: &MergedPending<'_>) -> crate::push::Batch {
    let mut batch = crate::push::Batch::default();
    for i in pending.removed() {
        let i = i as usize;
        if i >= mem.len() {
            continue;
        }
        batch
            .remove
            .push(String::from_utf8_lossy(mem.path_bytes(i)).into_owned());
    }
    batch.append = pending
        .appended()
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

/// Rebuild the index (drop every volume's removed indices, append every
/// volume's new entries) and write it to dump atomically. Kept entries stream
/// through the arena fast path — no per-entry String allocation or case-fold
/// recomputation. Returns the new authoritative index.
///
/// The index is cross-volume, so one dump write consumes all the pending sets at
/// once and the caller clears every volume's set afterwards.
fn flush(mem: &MemIndex, pending: &MergedPending<'_>, dump: &Path) -> Result<MemIndex> {
    // One merged set: the walk below visits every index and a per-entry lookup
    // across N volumes would be N hash probes each.
    let removed = pending.removed_union();
    let mut b = MemBuilder::default();
    for i in 0..mem.len() {
        if removed.contains(&(i as u32)) {
            continue;
        }
        b.push_arena(
            mem.path_bytes(i),
            mem.name_l_bytes(i),
            mem.rev_bytes(i),
            mem.meta_at(i),
        );
    }
    for (path, meta) in pending.appended() {
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

/// Store the last-applied USN of one or more volumes in the sidecar, one line
/// per volume ("C: 123456").
///
/// Every line is written in a single rewrite and lines for volumes that are not
/// in entries are preserved: losing another volume's line turns its restart into
/// a full sync-to-now (a permanent gap), while a stale line only replays.
fn write_usns(sidecar: &Path, entries: &[(char, i64)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut lines: Vec<String> = read_all_usns(sidecar).lines().map(str::to_string).collect();
    for &(drive, usn) in entries {
        let prefix = format!("{drive}:");
        match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
            Some(line) => *line = format!("{drive}: {usn}"),
            None => lines.push(format!("{drive}: {usn}")),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    let mut f = std::fs::File::create(sidecar)?;
    f.write_all(out.as_bytes())?;
    Ok(())
}

/// Journal positions (drive, NextUsn) of the given volumes, taken *before* an
/// index run so the dump can be stamped with them once it is on disk.
///
/// A volume whose journal cannot be queried (not elevated, journal missing) is
/// skipped: a missing line means "no stored position" and the monitor syncs to
/// now, which is safe, while a wrong position is not.
pub fn journal_positions(drives: &[char]) -> Vec<(char, i64)> {
    let mut out = Vec::with_capacity(drives.len());
    for &drive in drives {
        if let Ok(vol) = UsnVolume::open(drive)
            && let Ok((_id, next)) = vol.query_journal()
        {
            out.push((drive, next));
        }
    }
    out
}

/// Stamp the dump's USN sidecar with the positions an index run recorded before
/// its scan. The dump matches that point, so the monitor's next start replays
/// only the changes made during and after the scan — without this it restarts
/// from the previous flush position and replays up to --flush-secs of
/// already-indexed history, re-creating long-gone creates as zero-metadata
/// entries (measured: a 19-minute replay and 8,900 pending appends).
pub fn write_usn_positions(dump: &Path, positions: &[(char, i64)]) -> Result<()> {
    write_usns(&usn_sidecar_path(dump), positions)
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
    fn stat_ok(_path: &str, is_dir: bool, frn: u64) -> StatResult {
        Ok(StatMeta {
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

    /// Stat stub: the path does not exist any more (the ordinary "the event is
    /// stale" answer).
    fn stat_none(_path: &str, _is_dir: bool, _frn: u64) -> StatResult {
        Err(StatMiss::Gone)
    }

    /// Stat stub for the *other* failure class: locked, denied, IO error. The
    /// entry must survive with zeroed metadata (see stat_fail).
    fn stat_other(_path: &str, _is_dir: bool, _frn: u64) -> StatResult {
        Err(StatMiss::Other)
    }

    /// Stat stub: every path exists, and every path is a *file*. Used to prove
    /// the parent check looks at the real type instead of the caller's hint.
    fn stat_files(_path: &str, _is_dir: bool, frn: u64) -> StatResult {
        Ok(StatMeta {
            meta: EntryMeta { is_dir: false, size: 1, frn: Some(frn), ..Default::default() },
            alloc_failed: false,
        })
    }

    /// Stat stub that reports exactly these paths as still existing and every
    /// other path as gone. The retire rules are stat-verified, so this is what
    /// plays the role of the file system in the tests.
    fn stat_existing<'a>(present: &'a [&'a str]) -> impl FnMut(&str, bool, u64) -> StatResult + 'a {
        move |path: &str, is_dir: bool, frn: u64| {
            if present.iter().any(|p| p.eq_ignore_ascii_case(path)) {
                stat_ok(path, is_dir, frn)
            } else {
                stat_none(path, is_dir, frn)
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
    fn stat_meta_reports_a_vanished_path_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("never-existed.bin");
        assert!(matches!(stat_meta(&gone.to_string_lossy(), false, 1), Err(StatMiss::Gone)));
    }

    #[test]
    fn stat_error_classification_separates_gone_from_other() {
        use std::io::{Error, ErrorKind};
        // Windows' two "it is not there" codes, plus std's mapping.
        assert_eq!(classify_stat_error(&Error::from(ErrorKind::NotFound)), StatMiss::Gone);
        assert_eq!(classify_stat_error(&Error::from_raw_os_error(2)), StatMiss::Gone);
        assert_eq!(classify_stat_error(&Error::from_raw_os_error(3)), StatMiss::Gone);
        // Sharing violation (32) / access denied (5) are "the file may exist".
        assert_eq!(classify_stat_error(&Error::from(ErrorKind::PermissionDenied)), StatMiss::Other);
        assert_eq!(classify_stat_error(&Error::from_raw_os_error(32)), StatMiss::Other);
        assert_eq!(classify_stat_error(&Error::from_raw_os_error(5)), StatMiss::Other);
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
    fn create_counts_an_unreadable_stat_instead_of_hiding_it() {
        // The stat failed for a reason other than "gone" (locked / denied / IO):
        // the file may exist, so the entry is kept with zeroed metadata — but it
        // must be counted, because silent zero-metadata is the defect.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(4242, "locked.bin", USN_REASON_FILE_CREATE, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_other);

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.stat_fail, 1);
        assert_eq!(stats.stat_gone, 0);
        assert_eq!(stats.resolve_fail, 0);
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].1.size, 0);
        assert_eq!(pending.appended[0].1.frn, Some(4242));
    }

    #[test]
    fn a_create_whose_path_is_gone_is_dropped_instead_of_indexed() {
        // The live overlay carried 1400+ entries shaped like a real file's path
        // with a relative-path tail glued on, all size 0 / mtime 0. Their create
        // event could not be stat'ed because the path is not on disk: storing it
        // gives a search hit that can never be opened. Gone is not the same as
        // "unreadable" — this event is dropped.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(4242, "ghost.bin", USN_REASON_FILE_CREATE, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);

        assert_eq!(stats.stat_gone, 1, "the stale create is counted");
        assert_eq!(stats.stat_fail, 0, "a missing path is not an IO failure");
        assert_eq!(stats.applied, 0);
        assert!(pending.is_empty(), "nothing may be indexed: {:?}", pending.appended);
    }

    #[test]
    fn a_stale_create_never_leaves_a_ghost_even_with_its_delete() {
        // create(gone) then delete inside one window: the create is dropped, so
        // there is nothing for the delete to retire and nothing ever reaches the
        // index — the ghost cannot come back through the other branch.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [
            rec(99, "ghost.bin", USN_REASON_FILE_CREATE, false),
            rec(99, "ghost.bin", USN_REASON_FILE_DELETE, false),
        ];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);

        assert_eq!(stats.stat_gone, 1);
        assert_eq!(stats.applied, 0);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_stale_rename_target_is_dropped_too() {
        // RENAME_NEW_NAME onto a path that is not there (renamed away again, or
        // a virtual path): same rule as create.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(77, "ghost.bin", USN_REASON_RENAME_NEW_NAME, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut resolve_links, &mut stat_none);
        assert_eq!(stats.stat_gone, 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn create_counts_unresolvable_parents() {
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        let records = [rec(1, "orphan.bin", USN_REASON_FILE_CREATE, false)];
        let stats =
            apply_records(&mem, 'D', &records, &mut pending, &mut |_| None, &mut stat_ok);
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
            if looks <= 2 { stat_ok(path, is_dir, frn) } else { stat_none(path, is_dir, frn) }
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
        // h1 and h2 exist while their create records are applied; by the time
        // the rename record arrives h1 is gone and h2 stayed. The h1 append can
        // therefore only be found in the pending list, not in the index.
        let mut looks = 0u32;
        let mut stat = |path: &str, is_dir: bool, frn: u64| {
            looks += 1;
            if looks <= 2 { stat_ok(path, is_dir, frn) } else { stat_none(path, is_dir, frn) }
        };
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
            if looks <= 1 { stat_ok(path, is_dir, frn) } else { stat_none(path, is_dir, frn) }
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
        let built = flush(&mem, &MergedPending::new(&[&pending]), &dump).unwrap();
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
        let built = flush(&mem, &MergedPending::new(&[&pending]), &dump).unwrap();
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
        let built = flush(&mem, &MergedPending::new(&[&pending]), &dump).unwrap();
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
        flush(&mem, &MergedPending::new(&[&pending]), &dump).unwrap();
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
        assert!(
            s.meta.is_dir,
            "a directory junction IS a directory: std calls it a symlink, the Win32 \
             DIRECTORY attribute (what the $MFT scan reads) is what decides"
        );
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
    // -- parent resolution --------------------------------------------------

    /// One directory plus one file, as the index holds them after a scan.
    fn dir_index() -> MemIndex {
        let mut b = MemBuilder::default();
        b.push(r"D:\links", EntryMeta { is_dir: true, frn: Some(5), ..Default::default() });
        b.push(r"D:\links\file.bin", meta_file(9, 6));
        b.finish()
    }

    #[test]
    fn a_parent_is_resolved_from_the_index_first() {
        // No kernel call at all: the indexed directory path is exact, and the
        // kernel walk has been measured to return another record for the FRN.
        let mem = dir_index();
        let pending = Pending::default();
        let mut calls = 0u32;
        let mut kernel = |_frn: u64| {
            calls += 1;
            Some(r"D:\Apps\cent\Sync Data\Nigori.bin".to_string())
        };
        let mut stat = stat_existing(&[r"D:\links"]);
        let parent = resolve_parent(&mem, 'D', 5, &pending, &mut kernel, &mut stat);
        assert_eq!(parent.as_deref(), Some(r"D:\links"));
        assert_eq!(calls, 0, "the kernel must not be asked when the index knows");
    }

    #[test]
    fn a_parent_created_in_this_window_comes_from_the_pending_appends() {
        // A directory created since the last index is not in the index yet, but
        // its append already carries the full path.
        let mem = MemBuilder::default().finish();
        let mut pending = Pending::default();
        pending.appended.push((
            r"D:\links\newdir".to_string(),
            EntryMeta { is_dir: true, frn: Some(42), ..Default::default() },
        ));
        let mut calls = 0u32;
        let mut kernel = |_frn: u64| {
            calls += 1;
            Some(r"D:\wrong".to_string())
        };
        let mut stat = stat_existing(&[r"D:\links\newdir"]);
        let parent = resolve_parent(&mem, 'D', 42, &pending, &mut kernel, &mut stat);
        assert_eq!(parent.as_deref(), Some(r"D:\links\newdir"));
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_file_entry_is_never_used_as_a_parent() {
        // FRN 6 is a file in the index. Pasting it in front of a name can only
        // build a path that does not exist, so the hit is skipped and the
        // verified fallback decides (in run() that fallback also stat-checks).
        let mem = dir_index();
        let pending = Pending::default();
        let mut kernel = |_frn: u64| Some(r"D:\links".to_string());
        let mut stat = stat_ok;
        let got = resolve_parent(&mem, 'D', 6, &pending, &mut kernel, &mut stat);
        assert_eq!(got.as_deref(), Some(r"D:\links"), "the file hit must not be used");
        // Another volume's entry of the same number is not this volume's parent
        // either, so the FRN is asked of the kernel instead.
        let mut calls = 0u32;
        let mut kernel = |_frn: u64| {
            calls += 1;
            None
        };
        assert_eq!(resolve_parent(&mem, 'H', 5, &pending, &mut kernel, &mut stat), None);
        assert_eq!(calls, 1, "a foreign-volume hit does not answer for this volume");
    }

    #[test]
    fn a_child_path_uses_the_indexed_parent_not_the_kernel() {
        // Measured defect: the kernel walk handed back another record, the
        // monitor pasted it in front of the name and every child of that
        // directory became "...\Nigori.bin\<child>" (Nigori.bin is a file), so
        // the real path was unreachable. The indexed parent wins.
        let mem = dir_index();
        let mut pending = Pending::default();
        let records = [rec(70, "child.bin", USN_REASON_FILE_CREATE, false)];
        let mut kernel = |_frn: u64| Some(r"D:\Apps\cent\Sync Data\Nigori.bin".to_string());
        let stats = apply_records(&mem, 'D', &records, &mut pending, &mut kernel, &mut stat_ok);
        assert_eq!(stats.applied, 1);
        assert_eq!(stats.resolve_fail, 0);
        assert_eq!(pending.appended.len(), 1);
        assert_eq!(pending.appended[0].0, r"D:\links\child.bin");
    }

    #[test]
    fn a_stale_indexed_parent_falls_through_to_the_pending_appends() {
        // The directory was renamed after the scan: the indexed path no longer
        // exists, so the check rejects it and the window append (the rename's new
        // name, same FRN) answers instead.
        let mem = dir_index();
        let mut pending = Pending::default();
        pending.appended.push((
            r"D:\links\renamed".to_string(),
            EntryMeta { is_dir: true, frn: Some(5), ..Default::default() },
        ));
        let mut stat = stat_existing(&[r"D:\links\renamed"]);
        let mut calls = 0u32;
        let mut kernel = |_frn: u64| {
            calls += 1;
            None
        };
        let parent = resolve_parent(&mem, 'D', 5, &pending, &mut kernel, &mut stat);
        assert_eq!(parent.as_deref(), Some(r"D:\links\renamed"));
        assert_eq!(calls, 0, "the renamed parent is found without the kernel");
    }

    #[test]
    fn a_poisoned_parent_memo_is_dropped_and_the_walk_retried() {
        // A bad answer must not live in the per-volume memo forever: the cached
        // path is re-verified on every hit, and a failed check re-walks.
        let mut cache: HashMap<u64, Option<String>> = HashMap::new();
        cache.insert(5, Some(r"D:\Apps\cent\Sync Data\Nigori.bin".to_string()));
        let mut calls = 0u32;
        let mut walk = |_frn: u64| {
            calls += 1;
            Some(r"D:\links".to_string())
        };
        let mut stat = stat_existing(&[r"D:\links"]);
        let got = kernel_parent(5, &mut cache, &mut walk, &mut stat);
        assert_eq!(got.as_deref(), Some(r"D:\links"), "the poisoned memo must be replaced");
        assert_eq!(calls, 1, "the walk is retried once");
        assert_eq!(cache.get(&5).cloned().flatten().as_deref(), Some(r"D:\links"));

        // A good memo is reused without walking (one stat re-verifies it).
        let mut calls2 = 0u32;
        let mut walk2 = |_frn: u64| {
            calls2 += 1;
            Some(r"D:\other".to_string())
        };
        let got = kernel_parent(5, &mut cache, &mut walk2, &mut stat);
        assert_eq!(got.as_deref(), Some(r"D:\links"));
        assert_eq!(calls2, 0);
    }

    #[test]
    fn the_kernel_fallback_refuses_a_path_that_is_not_a_directory() {
        // A file path can never be a parent, and refusing it must not remember it
        // as a hit — the next event has to be able to try again.
        let mut cache: HashMap<u64, Option<String>> = HashMap::new();
        let mut walk = |_frn: u64| Some(r"D:\Apps\cent\Sync Data\Nigori.bin".to_string());
        let mut stat = stat_existing(&[r"D:\links"]);
        assert_eq!(kernel_parent(5, &mut cache, &mut walk, &mut stat), None);
        assert!(cache.is_empty(), "a refused path must not be memoised as a hit");

        // And a path that exists but is a *file* is refused as well: the type
        // comes from the file system, not from the hint the caller passes.
        let mut cache2: HashMap<u64, Option<String>> = HashMap::new();
        let mut walk = |_frn: u64| Some(r"D:\links".to_string());
        let mut stat_files = stat_files;
        assert_eq!(kernel_parent(5, &mut cache2, &mut walk, &mut stat_files), None);
        assert!(cache2.is_empty());

        // The volume root arrives as an empty path (the caller joins it as
        // "D:\<name>") and passes through untouched.
        let mut walk = |_frn: u64| Some(String::new());
        assert_eq!(kernel_parent(9, &mut cache, &mut walk, &mut stat).as_deref(), Some(""));
    }

    // -- multi-volume -------------------------------------------------------

    /// Parent-path resolver for a test volume: the injected closure plays the
    /// role of that volume's journal, so it must answer with that volume's root.
    fn resolve_on(drive: char) -> impl FnMut(u64) -> Option<String> {
        move |_frn: u64| Some(format!("{drive}:\\links"))
    }

    #[test]
    fn normalize_drives_upper_cases_dedupes_and_keeps_order() {
        assert_eq!(normalize_drives(&['d', 'D', 'h']), vec!['D', 'H']);
        assert_eq!(normalize_drives(&['h', 'd']), vec!['H', 'D']);
        assert!(normalize_drives(&[]).is_empty());
    }

    #[test]
    fn watch_and_lag_summaries_stay_readable_for_one_and_many_volumes() {
        // The single-volume forms are what every log line has always shown.
        assert_eq!(watch_summary(&[('D', 12)]), "D: from USN 12");
        assert_eq!(lag_summary(&[('D', 5)]), "5");
        assert_eq!(pos_summary(&[('D', 5)]), "5");
        // Several volumes name themselves.
        assert_eq!(watch_summary(&[('D', 12), ('H', 34)]), "D:,H: from USN (D: 12, H: 34)");
        assert_eq!(lag_summary(&[('D', 5), ('H', 0)]), "D:5 H:0");
        assert_eq!(pos_summary(&[('D', 5), ('H', 6)]), "D: 5, H: 6");
        assert_eq!(drive_list(&['D', 'H']), "D:,H:");
        assert_eq!(watch_summary(&[]), "no volumes");
    }

    #[test]
    fn push_is_held_back_during_catch_up_but_not_forever() {
        // Live round with changes: goes out at once.
        assert!(should_push(true, false, Duration::ZERO));
        // Some volume is still replaying: held back, so a multi-megabyte pending
        // set is not re-serialized every back-to-back round ...
        assert!(!should_push(true, true, Duration::ZERO));
        assert!(!should_push(true, true, Duration::from_secs(59)));
        // ... but a volume that never catches up must not silence the feed for
        // the volumes that are live.
        assert!(should_push(true, true, PUSH_STALE_FORCE));
        // Nothing changed: nothing to send.
        assert!(!should_push(false, false, Duration::ZERO));
    }

    #[test]
    fn two_volumes_merge_into_one_batch() {
        // D: retires an indexed entry while H: creates one: the receiver applies
        // a full-replacement batch, so both volumes' changes must travel in the
        // same message.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\gone.bin", meta_file(3, 30));
        let mem = b.finish();

        let mut d = Pending::default();
        let mut h = Pending::default();
        apply_records(
            &mem,
            'D',
            &[rec(30, "gone.bin", USN_REASON_FILE_DELETE, false)],
            &mut d,
            &mut resolve_links,
            &mut stat_none,
        );
        apply_records(
            &mem,
            'H',
            &[rec(71, "h-new.bin", USN_REASON_FILE_CREATE, false)],
            &mut h,
            &mut resolve_on('H'),
            &mut stat_ok,
        );

        let sets: [&Pending; 2] = [&d, &h];
        let merged = MergedPending::new(&sets);
        assert!(!merged.is_empty());
        assert_eq!(merged.appended_len(), 1);
        assert_eq!(merged.removed_len(), 1);

        let batch = build_batch(&mem, &merged);
        assert_eq!(batch.remove, vec![r"D:\links\gone.bin".to_string()]);
        assert_eq!(batch.append.len(), 1);
        // The path carries H:, from H:'s resolver — the batch is cross-volume.
        assert_eq!(batch.append[0].p, r"H:\links\h-new.bin");
        assert_eq!(batch.append[0].s, 4096);
        assert!(!batch.append[0].d);
    }

    #[test]
    fn flush_writes_every_volume_into_one_dump() {
        // The flush is cross-volume: one dump holds all volumes, so a single
        // write consumes every pending set. Retaining one volume's set would be
        // unsound anyway — removed holds indices into the index the flush just
        // renumbered.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\gone.bin", meta_file(3, 30));
        b.push(r"H:\links\keep.bin", meta_file(5, 40));
        let mem = b.finish();

        let mut d = Pending::default();
        let mut h = Pending::default();
        apply_records(
            &mem,
            'D',
            &[rec(30, "gone.bin", USN_REASON_FILE_DELETE, false)],
            &mut d,
            &mut resolve_links,
            &mut stat_none,
        );
        apply_records(
            &mem,
            'H',
            &[rec(71, "h-new.bin", USN_REASON_FILE_CREATE, false)],
            &mut h,
            &mut resolve_on('H'),
            &mut stat_ok,
        );

        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("index.db.feridx");
        let built = flush(&mem, &MergedPending::new(&[&d, &h]), &dump).unwrap();
        assert_eq!(built.len(), 2, "one D: entry retired, one H: entry appended");

        let loaded = MemIndex::load_dump(&dump).unwrap();
        assert!(loaded.find_path_idx(r"D:\links\gone.bin").is_none(), "D: removal landed");
        assert!(loaded.find_path_idx(r"H:\links\keep.bin").is_some(), "H: entry survived");
        assert!(loaded.find_path_idx(r"H:\links\h-new.bin").is_some(), "H: append landed");

        // The caller then clears every volume's set (the dump carries them all).
        d.clear();
        h.clear();
        assert!(MergedPending::new(&[&d, &h]).is_empty());
    }

    #[test]
    fn clearing_one_volumes_pending_leaves_the_others() {
        let mut d = Pending::default();
        let mut h = Pending::default();
        d.appended.push((r"D:\links\d.bin".to_string(), meta_file(1, 1)));
        h.appended.push((r"H:\links\h.bin".to_string(), meta_file(2, 2)));
        d.clear();
        assert!(d.is_empty());
        assert_eq!(h.appended.len(), 1, "clearing one volume must not touch another's set");
    }

    #[test]
    fn a_single_volume_merged_set_is_the_plain_pending_set() {
        // Single-volume regression: one set in, the same batch out.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\a.bin", meta_file(1, 60));
        let mem = b.finish();

        let mut pending = Pending::default();
        apply_records(
            &mem,
            'D',
            &[rec(60, "a.bin", USN_REASON_FILE_DELETE, false)],
            &mut pending,
            &mut resolve_links,
            &mut stat_none,
        );
        assert_eq!(pending.removed.len(), 1);

        let sets = [&pending];
        let merged = MergedPending::new(&sets);
        assert_eq!(merged.appended_len(), pending.appended.len());
        assert_eq!(merged.removed_len(), pending.removed.len());
        assert_eq!(merged.frns_len(), pending.removed_frns.len());
        assert!(!merged.is_empty());
        let batch = build_batch(&mem, &merged);
        assert_eq!(batch.remove, vec![r"D:\links\a.bin".to_string()]);
        assert!(batch.append.is_empty());
    }

    #[test]
    fn a_delete_on_one_volume_never_touches_another_volumes_frn() {
        // FRNs are volume-local: record 70 on D: and record 70 on H: are
        // different files. Without the drive filter a delete on D: would stat
        // (and, with the path gone, retire) H:'s entry of the same number.
        let mut b = MemBuilder::default();
        b.push(r"D:\links\h1.bin", meta_file(7, 70));
        b.push(r"H:\links\x.bin", meta_file(9, 70));
        let mem = b.finish();

        let mut pending = Pending::default();
        // Nothing is on disk for either path, so only the drive filter can keep
        // H:'s entry.
        let stats = apply_records(
            &mem,
            'D',
            &[rec(70, "h1.bin", USN_REASON_FILE_DELETE, false)],
            &mut pending,
            &mut resolve_links,
            &mut stat_none,
        );
        assert_eq!(stats.applied, 1);
        assert!(pending.removed.contains(&0), "the D: alias is retired");
        assert!(
            !pending.removed.contains(&1),
            "the H: entry with the same FRN is a different file"
        );
    }

    #[test]
    fn usn_sidecar_keeps_one_position_per_volume() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("index.db.feridx.usn");
        write_usns(&sidecar, &[('D', 100), ('H', 200)]).unwrap();
        assert_eq!(read_usn(&sidecar, 'D'), Some(100));
        assert_eq!(read_usn(&sidecar, 'H'), Some(200));
        assert_eq!(read_usn(&sidecar, 'C'), None);

        // Moving one volume forward must not drop another's line: a missing line
        // means "no stored position" and syncs to now, i.e. a permanent gap.
        write_usns(&sidecar, &[('D', 150)]).unwrap();
        assert_eq!(read_usn(&sidecar, 'D'), Some(150));
        assert_eq!(read_usn(&sidecar, 'H'), Some(200));

        write_usns(&sidecar, &[('H', 250), ('C', 300)]).unwrap();
        assert_eq!(read_usn(&sidecar, 'D'), Some(150));
        assert_eq!(read_usn(&sidecar, 'H'), Some(250));
        assert_eq!(read_usn(&sidecar, 'C'), Some(300));
        let text = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(text.lines().count(), 3, "one line per volume: {text:?}");
    }
}

