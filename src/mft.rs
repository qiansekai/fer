//! Raw NTFS $MFT scanner — the "full parity" indexing path.
//!
//! `FSCTL_ENUM_USN_DATA` only exposes each file's *primary* name. Everything's
//! engine instead parses the MFT directly, which additionally yields:
//! * every hard-link alias (additional `$FILE_NAME` attributes),
//! * real/allocated size and timestamps,
//! * DOS attribute flags (hidden/system/read-only/reparse).
//!
//! This module reads the $MFT data runs from the raw volume (via the runs
//! declared in $MFT's own record 0), applies UPDATE_SEQUENCE_ARRAY fixups and
//! walks FILE records. If anything looks unsupported (e.g. a fragmented $MFT
//! behind an attribute list), [`crate::indexer`] falls back to the USN path.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_BEGIN, ReadFile, SetFilePointerEx,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::usn::UsnVolume;
use crate::FRN_MASK;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

/// Mirror of NTFS_VOLUME_DATA_BUFFER (kept local to avoid feature churn).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NtfsVolumeData {
    volume_serial: i64,
    number_sectors: i64,
    total_clusters: i64,
    free_clusters: i64,
    total_reserved: i64,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    bytes_per_file_record: u32,
    clusters_per_file_record: u32,
    mft_valid_data_length: i64,
    mft_start_lcn: i64,
    mft2_start_lcn: i64,
    mft_zone_start: i64,
    mft_zone_end: i64,
}

/// One emitted entry: a single `$FILE_NAME` occurrence (hard links yield
/// several entries with the same FRN).
#[derive(Debug, Clone)]
pub struct MftEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Allocated clusters in bytes (0 for resident streams).
    pub allocated: u64,
    pub mtime: i64, // unix seconds
    pub ctime: i64, // unix seconds
    pub hidden: bool,
    pub system: bool,
    pub readonly: bool,
    pub reparse: bool,
    /// Set when this file's default `$DATA` stream lives in a **different**
    /// MFT record (spilled via `$ATTRIBUTE_LIST` — common for heavily
    /// fragmented large files). Holds that record's number; the scanner
    /// resolves size/allocated from it after the main sweep.
    pub fixup_record: Option<u64>,
    /// The `$ATTRIBUTE_LIST` itself is non-resident (very fragmented file, e.g.
    /// tens of thousands of extents): its data runs + logical size, resolved
    /// by the scanner after the sweep.
    pub(crate) fixup_list: Option<(Vec<Run>, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    vcn: u64,
    lcn: i64,
    len: u64, // clusters
}

pub struct MftScanner {
    vol: UsnVolume,
    sector_size: u32,
    record_size: u32,
    bytes_per_cluster: u32,
    runs: Vec<Run>,
    data_size: u64,
    // true when $MFT's record 0 contains an attribute list (unsupported):
    fragmented: bool,
    // $MFT::$BITMAP data runs + size: 1 bit per FILE record, marking in-use.
    // Empty when unavailable — the scan then reads every record (no skip).
    bitmap_runs: Vec<Run>,
    bitmap_size: u64,
}

/// Windows FILETIME (100ns since 1601) → unix seconds.
///
/// `pub(crate)` so the monitor's create/rename metadata fill (which stats a
/// path with `std::fs`, not the raw `$MFT`) converts timestamps exactly the
/// same way instead of carrying a second copy of the epoch constant.
pub(crate) fn filetime_to_unix(ft: u64) -> i64 {
    (ft / 10_000_000).saturating_sub(11_644_473_600) as i64
}

impl MftScanner {
    pub fn open(drive: char) -> Result<Self> {
        let vol = UsnVolume::open(drive)?;
        let handle = vol.raw_handle();

        let mut vd: NtfsVolumeData = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                null_mut(),
                0,
                &mut vd as *mut _ as *mut c_void,
                size_of::<NtfsVolumeData>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            bail!(
                "FSCTL_GET_NTFS_VOLUME_DATA failed on {drive}: (error {})",
                unsafe { GetLastError() }
            );
        }
        if vd.mft_start_lcn < 0 {
            bail!("$MFT start LCN not available on {drive}: (fragmented?)");
        }
        let sector_size = vd.bytes_per_sector;
        let record_size = vd.bytes_per_file_record;
        if sector_size == 0 || record_size < 48 {
            bail!("implausible NTFS geometry on {drive}: sector={sector_size} record={record_size}");
        }

        // Read record 0 ($MFT) and pull its $DATA run list.
        // MftStartLcn is a *cluster* number — multiply by bytes per cluster,
        // not by the sector size.
        let rec0_off = vd.mft_start_lcn as u64 * vd.bytes_per_cluster as u64;
        let raw0 = read_raw(handle, rec0_off, record_size)
            .with_context(|| format!("reading $MFT record 0 on {drive}"))?;
        let rec0 = apply_fixups(&raw0, sector_size)?;
        let hdr = parse_file_header(&rec0)?;
        let mut runs: Vec<Run> = Vec::new();
        let mut data_size: u64 = 0;
        let mut has_attr_list = false;
        let mut bitmap_runs: Vec<Run> = Vec::new();
        let mut bitmap_size: u64 = 0;
        for attr in iterate_attributes(&rec0, hdr.attr_off, hdr.bytes_in_use) {
            match attr.attr_type {
                0x20 => has_attr_list = true,
                0x80 if attr.name_len != 0 => {} // named stream, not $MFT's data
                0x80 if !attr.non_resident => {}
                0x80 => {
                    data_size = attr.real_size;
                    runs = parse_runlist(&rec0[attr.mapping_pairs_off..attr.end])?;
                }
                // $MFT::$BITMAP: the in-use record map (enables deleted-record
                // skipping — 40-55% less I/O on typical volumes). Non-resident
                // only; a resident bitmap (tiny MFT) or a malformed run list
                // simply disables skipping — never fail the scan over it.
                0xB0 => {
                    if attr.non_resident
                        && let Ok(r) = parse_runlist(&rec0[attr.mapping_pairs_off..attr.end])
                        && !r.is_empty()
                        && attr.real_size > 0
                    {
                        bitmap_runs = r;
                        bitmap_size = attr.real_size;
                    }
                }
                _ => {}
            }
        }
        if runs.is_empty() || data_size == 0 {
            bail!("$MFT on {drive}: no usable $DATA run list");
        }
        Ok(MftScanner {
            vol,
            sector_size,
            record_size,
            bytes_per_cluster: vd.bytes_per_cluster,
            runs,
            data_size,
            fragmented: has_attr_list,
            bitmap_runs,
            bitmap_size,
        })
    }

    pub fn is_fragmented(&self) -> bool {
        self.fragmented
    }

    /// Scan the whole $MFT, emitting one entry per `$FILE_NAME`.
    /// Returns the number of FILE records processed.
    ///
    /// Build-path optimizations (memory-conscious):
    /// * `$MFT::$BITMAP` skip — whole 1024-record blocks with no in-use bit
    ///   are never read nor parsed (40-55% I/O saved on typical volumes);
    ///   the bitmap itself is ~1 bit per record (~520 KB at 4.17M records).
    /// * parallel parsing — each batch's records are parsed by scoped threads
    ///   over disjoint slices (one `Vec<MftEntry>` per thread, ~1 MB total
    ///   intermediate state per batch); emission stays sequential on the
    ///   calling thread, so entry order is deterministic.
    pub fn scan(&self, mut on_entry: impl FnMut(&MftEntry)) -> Result<u64> {
        const BATCH: usize = 8 << 20; // bytes read per batch
        const BLOCK_RECS: u64 = 1024; // bitmap-skip granularity (record-aligned)
        let rec_size = self.record_size as u64;
        let file_records = self.data_size / rec_size;

        // Load the in-use bitmap once (tiny). Raw-volume reads must be
        // sector-aligned, and a bitmap's byte length usually isn't — round the
        // read up to the sector size (extra bytes are harmless: block checks
        // are clamped to the record count). Bitmap trouble (unreadable runs,
        // size mismatch) degrades to "read everything" — indexing correctness
        // must never depend on it.
        let bitmap: Vec<u8> = if !self.bitmap_runs.is_empty() && self.bitmap_size > 0 {
            let bpc = self.bytes_per_cluster.max(512) as u64;
            let cap = self.bitmap_runs.iter().map(|r| r.len).sum::<u64>() * bpc;
            let sec = self.sector_size.max(512) as usize;
            let want = ((self.bitmap_size as usize).div_ceil(sec) * sec) as u64;
            let want = want.min(cap) as usize;
            let mut b = vec![0u8; want];
            match self.read_runs(&self.bitmap_runs, 0, want, &mut b) {
                Ok(()) => {
                    b.truncate(self.bitmap_size as usize);
                    b
                }
                Err(_) => {
                    eprintln!("[mft] $MFT::$BITMAP unreadable — scanning without skip");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let block_used = |r0: u64, r1: u64| -> bool {
            if bitmap.is_empty() {
                return true; // no bitmap → never skip
            }
            let b0 = (r0 / 8) as usize;
            let b1 = r1.div_ceil(8) as usize;
            bitmap.get(b0..b1).is_none_or(|b| b.iter().any(|&x| x != 0))
        };

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        let mut records = 0u64;
        let mut pending: Vec<MftEntry> = Vec::new();
        let mut buf: Vec<u8> = Vec::with_capacity(BATCH);
        let mut r = 0u64; // record cursor over the whole $MFT
        while r < file_records {
            // 1) Collect read ranges up to BATCH bytes, skipping unused blocks.
            let mut ranges: Vec<(u64, usize)> = Vec::new();
            let mut bytes = 0usize;
            while r < file_records && bytes < BATCH {
                let r1 = (r + BLOCK_RECS).min(file_records);
                if block_used(r, r1) {
                    let off = r * rec_size;
                    let len = ((r1 - r) * rec_size) as usize;
                    if let Some(last) = ranges.last_mut()
                        && last.0 + last.1 as u64 == off
                    {
                        last.1 += len;
                    } else {
                        ranges.push((off, len));
                    }
                    bytes += len;
                }
                r = r1;
            }
            // 2) Read the ranges into the batch buffer (contiguous view of
            //    used blocks; record boundaries stay aligned).
            buf.clear();
            for &(off, len) in &ranges {
                let start = buf.len();
                buf.resize(start + len, 0);
                self.read_runs(&self.runs, off, len, &mut buf[start..])?;
            }
            let n_rec = buf.len() / self.record_size as usize;
            if n_rec == 0 {
                continue;
            }
            // 3) Parallel parse over disjoint record slices; the main thread
            //    emits in order afterwards (on_entry stays single-threaded).
            //    Copy the geometry out first: the scanner holds a raw volume
            //    HANDLE (!Sync) and must not cross into the scoped threads.
            let rec_size = self.record_size as usize;
            let sec_size = self.sector_size;
            let per_thread = n_rec.div_ceil(threads);
            let mut rest = &mut buf[..];
            let mut chunks: Vec<&mut [u8]> = Vec::with_capacity(threads);
            for t in 0..threads {
                let s = t * per_thread;
                let e = ((t + 1) * per_thread).min(n_rec);
                if s >= e {
                    break;
                }
                let (head, tail) = rest.split_at_mut((e - s) * rec_size);
                chunks.push(head);
                rest = tail;
            }
            let mut parsed: Vec<(Vec<MftEntry>, u64)> = Vec::with_capacity(chunks.len());
            std::thread::scope(|sc| {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| sc.spawn(move || parse_slice(chunk, rec_size, sec_size)))
                    .collect();
                for h in handles {
                    parsed.push(h.join().expect("parse thread panicked"));
                }
            });
            for (entries, recs) in parsed {
                records += recs;
                for entry in entries {
                    // Entries whose default $DATA was spilled to another record
                    // are held back until the extra records have been read.
                    if entry.fixup_record.is_some() || entry.fixup_list.is_some() {
                        pending.push(entry);
                    } else {
                        on_entry(&entry);
                    }
                }
            }
        }
        if !pending.is_empty() {
            // Non-resident $ATTRIBUTE_LIST: read each list once to learn the
            // record that owns the stream's first segment.
            for e in pending.iter_mut() {
                if e.fixup_record.is_none()
                    && let Some((runs, size)) = e.fixup_list.take()
                {
                    e.fixup_record = self.read_attr_list_target(&runs, size);
                }
            }
            let sizes = self.read_spilled_data_sizes(&pending);
            let patched = pending
                .iter()
                .filter(|e| e.fixup_record.is_some_and(|t| sizes.contains_key(&t)))
                .count();
            eprintln!(
                "[mft] patched {patched}/{} entries whose $DATA was spilled via $ATTRIBUTE_LIST",
                pending.len()
            );
            for mut entry in pending {
                if let Some(target) = entry.fixup_record
                    && let Some(&(size, allocated)) = sizes.get(&target)
                {
                    entry.size = size;
                    entry.allocated = allocated;
                }
                on_entry(&entry);
            }
        }
        Ok(records)
    }

    /// Read a non-resident `$ATTRIBUTE_LIST` and return the MFT record holding
    /// the default `$DATA` stream's first segment (`lowest_vcn == 0`).
    fn read_attr_list_target(&self, runs: &[Run], size: u64) -> Option<u64> {
        // A 50k-extent file's list is ~1.5 MB; anything past 64 MB is a
        // corrupt/absurd list and not worth an allocation.
        const MAX_LIST: u64 = 64 << 20;
        if size == 0 || size > MAX_LIST {
            return None;
        }
        // Raw volume reads must be sector-aligned in *length* too, and an
        // attribute list's logical size rarely is (640 B, 1056 B, ...): round
        // the read up and truncate afterwards. `cap` keeps the rounded read
        // inside the stream's allocated clusters.
        let want = size as usize;
        let sec = self.sector_size.max(512) as usize;
        let cap = runs.iter().map(|r| r.len).sum::<u64>() * self.bytes_per_cluster.max(512) as u64;
        let read_len = ((want.div_ceil(sec) * sec) as u64).min(cap) as usize;
        if read_len < want {
            return None; // stream shorter than its declared size
        }
        let mut buf = vec![0u8; read_len];
        if let Err(e) = self.read_runs(runs, 0, read_len, &mut buf) {
            eprintln!(
                "[mft] attribute list unreadable ({} runs, {size} B): {e}",
                runs.len()
            );
            return None;
        }
        buf.truncate(want);
        attr_list_data_record(&buf)
    }

    /// Read the extra MFT records referenced by held-back entries and return
    /// `target record -> (real_size, allocated)` for the ones carrying a usable
    /// unnamed `$DATA` segment. Unreadable records are simply left out (the
    /// entry keeps its `$FILE_NAME` fallback rather than failing the scan).
    fn read_spilled_data_sizes(&self, pending: &[MftEntry]) -> HashMap<u64, (u64, u64)> {
        let mut targets: Vec<u64> = pending.iter().filter_map(|e| e.fixup_record).collect();
        targets.sort_unstable();
        targets.dedup();
        let mut out: HashMap<u64, (u64, u64)> = HashMap::with_capacity(targets.len());
        let mut rec = vec![0u8; self.record_size as usize];
        for t in targets {
            let off = t * self.record_size as u64;
            if self.read_runs(&self.runs, off, rec.len(), &mut rec).is_err() {
                continue;
            }
            apply_fixups_inplace(&mut rec, self.sector_size);
            if let Some(pair) = record_default_data_size(&rec) {
                out.insert(t, pair);
            }
        }
        out
    }

    /// Read `len` bytes at `offset` within a set of data runs, crossing run
    /// boundaries as needed. Contiguous stretches inside one run are fetched
    /// with a single ReadFile straight into `out` — the $MFT is almost always
    /// one long run, so this turns millions of per-cluster syscalls into a
    /// handful of megabyte-sized reads.
    fn read_runs(&self, runs: &[Run], offset: u64, len: usize, out: &mut [u8]) -> Result<()> {
        let bytes_per_cluster = self.bytes_per_cluster.max(512) as u64;
        let mut done = 0usize;
        while done < len {
            let off = offset + done as u64;
            let cluster = off / bytes_per_cluster;
            let run = runs
                .iter()
                .find(|r| cluster >= r.vcn && cluster < r.vcn + r.len)
                .with_context(|| format!("byte {off} outside run list"))?;
            if run.lcn < 0 {
                bail!("run at VCN {} is sparse — unsupported", run.vcn);
            }
            let run_end = (run.vcn + run.len) * bytes_per_cluster;
            let take = (run_end - off).min((len - done) as u64) as usize;
            let device_off =
                (run.lcn as u64 + (cluster - run.vcn)) * bytes_per_cluster + off % bytes_per_cluster;
            read_raw_into(self.vol.raw_handle(), device_off, &mut out[done..done + take])
                .with_context(|| format!("reading at device offset {device_off}"))?;
            done += take;
        }
        Ok(())
    }
}

/// Parse one FILE record (applies USA fixups in place) into 0..n entries —
/// one `MftEntry` per `$FILE_NAME` (hard links yield several). Returns `None`
/// when the record is not a valid in-use FILE record.
fn parse_record(rec: &mut [u8], sector_size: u32) -> Option<Vec<MftEntry>> {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return None;
    }
    let flags = u16::from_le_bytes([rec[22], rec[23]]);
    if flags & 0x01 == 0 {
        return None; // not in use (bitmap skip should prevent most of these)
    }
    apply_fixups_inplace(rec, sector_size);
    let hdr = parse_file_header(rec).ok()?;
    // FRNs are normalized to the plain record index (no sequence bits):
    // $FILE_NAME parent references only carry the index, and this also keeps
    // MFT/USN/monitor FRNs mutually consistent.
    let frn = if hdr.base_frn != 0 {
        hdr.base_frn & FRN_MASK
    } else {
        hdr.record_number as u64 & FRN_MASK
    };
    let is_dir = flags & 0x02 != 0;
    // One pass over the attributes. Per-record metadata comes from the
    // authoritative attributes:
    // * size — the $DATA attribute's real size ($FILE_NAME's size fields are
    //   stale directory-entry caches NTFS no longer maintains; they read 0
    //   for most user files)
    // * mtime/ctime — $STANDARD_INFORMATION (same staleness issue)
    // plus every $FILE_NAME — hard-linked files carry one attribute per link.
    let mut std_mtime = 0i64;
    let mut std_ctime = 0i64;
    let mut data_size: Option<u64> = None;
    let mut data_allocated: Option<u64> = None;
    let mut attr_list: Option<(usize, usize)> = None;
    let mut attr_list_runs: Option<(Vec<Run>, u64)> = None;
    let mut names: Vec<(u64, String, bool, bool, bool, bool)> = Vec::new();
    let mut max_size = 0u64;
    for attr in iterate_attributes(rec, hdr.attr_off, hdr.bytes_in_use) {
        match attr.attr_type {
            0x10 if !attr.non_resident && attr.value_len >= 24 => {
                let v = &rec[attr.value_off..attr.value_off + attr.value_len as usize];
                std_ctime = filetime_to_unix(u64::from_le_bytes(v[0..8].try_into().unwrap()));
                std_mtime = filetime_to_unix(u64::from_le_bytes(v[8..16].try_into().unwrap()));
            }
            // Only the *unnamed* $DATA stream is the file's content. Named
            // streams ($BadClus:$Bad spans the whole volume, file:Zone.Identifier,
            // ...) would otherwise be reported as the file's size.
            0x80 if attr.name_len == 0 => {
                if attr.non_resident {
                    // A stream split across records by $ATTRIBUTE_LIST carries
                    // its authoritative size only in the lowest_vcn == 0 segment.
                    if attr.lowest_vcn == 0 {
                        data_size = Some(attr.real_size);
                        // 0 clusters for resident streams (stored in the record)
                        data_allocated = Some(attr.allocated);
                    }
                } else {
                    data_size = Some(attr.value_len as u64);
                    data_allocated = Some(0);
                }
            }
            // Resident $ATTRIBUTE_LIST: keep its bytes so the spilled $DATA
            // record can be located after the attribute sweep.
            0x20 if !attr.non_resident => {
                let end = attr.value_off.saturating_add(attr.value_len as usize);
                if attr.value_off >= hdr.attr_off && end <= rec.len() {
                    attr_list = Some((attr.value_off, end));
                }
            }
            // Non-resident $ATTRIBUTE_LIST (the list outgrew the record — very
            // fragmented files): keep its runs, resolved after the sweep.
            0x20 => {
                if attr.mapping_pairs_off < attr.end
                    && attr.end <= rec.len()
                    && let Ok(runs) = parse_runlist(&rec[attr.mapping_pairs_off..attr.end])
                    && !runs.is_empty()
                    && attr.real_size > 0
                {
                    attr_list_runs = Some((runs, attr.real_size));
                }
            }
            0x30 if !attr.non_resident && attr.value_len >= 66 => {
                let v = &rec[attr.value_off..attr.value_off + attr.value_len as usize];
                let parent_frn = u64::from_le_bytes(v[0..8].try_into().unwrap()) & FRN_MASK;
                let size = u64::from_le_bytes(v[48..56].try_into().unwrap());
                let dos_flags = u32::from_le_bytes(v[56..60].try_into().unwrap());
                let name_len = v[64] as usize;
                let namespace = v[65];
                if namespace == 2 {
                    continue; // pure DOS (8.3) alias — skip clutter
                }
                if name_len == 0 || 66 + name_len * 2 > v.len() {
                    continue;
                }
                let name = utf16_name(&v[66..66 + name_len * 2]);
                max_size = max_size.max(size);
                names.push((
                    parent_frn,
                    name,
                    dos_flags & 0x02 != 0,
                    dos_flags & 0x04 != 0,
                    dos_flags & 0x01 != 0,
                    dos_flags & 0x0400 != 0,
                ));
            }
            _ => {}
        }
    }
    let size = data_size.unwrap_or(max_size);
    let allocated = data_allocated.unwrap_or(0);
    // The default $DATA stream was spilled to another MFT record: remember
    // how to find it so the scanner can read the real size/allocated
    // afterwards. ($FILE_NAME's own size field is a stale directory cache —
    // 0 for most user files — so the fallback above cannot stand in for it.)
    let (fixup_record, fixup_list) = if data_size.is_some() {
        (None, None)
    } else {
        match (attr_list, attr_list_runs) {
            (Some((s, e)), _) => (attr_list_data_record(&rec[s..e]), None),
            (None, Some((runs, size))) => (None, Some((runs, size))),
            _ => (None, None),
        }
    };
    let mut out = Vec::with_capacity(names.len());
    for (parent_frn, name, hidden, system, readonly, reparse) in names {
        out.push(MftEntry {
            frn,
            parent_frn,
            name,
            is_dir,
            size,
            allocated,
            mtime: std_mtime.max(0),
            ctime: std_ctime.max(0),
            hidden,
            system,
            readonly,
            reparse,
            fixup_record,
            fixup_list: fixup_list.clone(),
        });
    }
    Some(out)
}

/// Locate the MFT record holding the *unnamed* `$DATA` stream's first segment
/// (`lowest_vcn == 0`) inside a resident `$ATTRIBUTE_LIST`.
///
/// ATTR_LIST_ENTRY layout (NTFS 3.1, entries 8-byte aligned):
/// `type u32 @0 | length u16 @4 | name_len u8 @6 | name_off u8 @7 |
///  lowest_vcn u64 @8 | file_reference u64 @16 | instance u16 @24 | name @26`.
fn attr_list_data_record(list: &[u8]) -> Option<u64> {
    let mut off = 0usize;
    while off + 26 <= list.len() {
        let ty = u32::from_le_bytes(list[off..off + 4].try_into().unwrap());
        let len = u16::from_le_bytes([list[off + 4], list[off + 5]]) as usize;
        if len < 26 || off + len > list.len() {
            break;
        }
        let name_len = list[off + 6];
        let lowest_vcn = u64::from_le_bytes(list[off + 8..off + 16].try_into().unwrap());
        let file_ref = u64::from_le_bytes(list[off + 16..off + 24].try_into().unwrap());
        if ty == 0x80 && name_len == 0 && lowest_vcn == 0 {
            return Some(file_ref & FRN_MASK);
        }
        off += len;
    }
    None
}

/// Extract `(real_size, allocated)` of a record's unnamed non-resident
/// `$DATA` first segment — used to patch entries whose stream was spilled.
fn record_default_data_size(rec: &[u8]) -> Option<(u64, u64)> {
    let hdr = parse_file_header(rec).ok()?;
    for attr in iterate_attributes(rec, hdr.attr_off, hdr.bytes_in_use) {
        if attr.attr_type == 0x80
            && attr.name_len == 0
            && attr.non_resident
            && attr.lowest_vcn == 0
        {
            return Some((attr.real_size, attr.allocated));
        }
    }
    None
}

/// Parse a slice of back-to-back FILE records (parallel-scan worker body).
/// Returns the flattened entries and the number of valid records processed.
fn parse_slice(recs: &mut [u8], record_size: usize, sector_size: u32) -> (Vec<MftEntry>, u64) {
    let mut out = Vec::new();
    let mut n = 0u64;
    for rec in recs.chunks_mut(record_size) {
        if let Some(entries) = parse_record(rec, sector_size) {
            n += 1;
            out.extend(entries);
        }
    }
    (out, n)
}

/// Decode a little-endian UTF-16 name via an aligned stack buffer (an NTFS
/// name is at most 255 UTF-16 units). Avoids per-element byte assembly.
fn utf16_name(bytes: &[u8]) -> String {
    let mut tmp = [0u8; 510];
    tmp[..bytes.len()].copy_from_slice(bytes);
    // SAFETY: `tmp` is a stack array (2-aligned); u16 has no invalid bit
    // patterns; the slice length is halved to match.
    let units: &[u16] =
        unsafe { std::slice::from_raw_parts(tmp.as_ptr() as *const u16, bytes.len() / 2) };
    String::from_utf16_lossy(units)
}

// ---------------------------------------------------------------------------
// pure parsing helpers (unit-tested)

struct FileHeader {
    attr_off: usize,
    bytes_in_use: usize,
    base_frn: u64,
    record_number: u32,
}

fn parse_file_header(rec: &[u8]) -> Result<FileHeader> {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        bail!("not a FILE record");
    }
    Ok(FileHeader {
        attr_off: u16::from_le_bytes([rec[20], rec[21]]) as usize,
        bytes_in_use: u32::from_le_bytes(rec[24..28].try_into().unwrap()) as usize,
        base_frn: u64::from_le_bytes(rec[32..40].try_into().unwrap()),
        record_number: u32::from_le_bytes(rec[44..48].try_into().unwrap()),
    })
}

/// Apply the UPDATE_SEQUENCE_ARRAY fixups in place. Guards all offsets; on
/// anything malformed the record is left untouched (parse will then skip it).
/// Idempotent with respect to the USA table itself.
fn apply_fixups_inplace(rec: &mut [u8], sector_size: u32) {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return;
    }
    let usa_off = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let usa_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    if usa_count < 2 || usa_off + usa_count * 2 > rec.len() {
        return; // nothing to fix
    }
    let sector = sector_size as usize;
    for s in 1..usa_count {
        let end = s * sector;
        if end + 2 > rec.len() {
            break;
        }
        let v = u16::from_le_bytes([rec[usa_off + s * 2], rec[usa_off + s * 2 + 1]]);
        rec[end - 2] = v as u8;
        rec[end - 1] = (v >> 8) as u8;
    }
}

/// Copying variant used for one-shot reads (record 0) and unit tests.
fn apply_fixups(rec: &[u8], sector_size: u32) -> Result<Vec<u8>> {
    parse_file_header(rec)?;
    let mut out = rec.to_vec();
    apply_fixups_inplace(&mut out, sector_size);
    Ok(out)
}

struct AttrRef {
    attr_type: u32,
    end: usize,
    non_resident: bool,
    /// Attribute name length in UTF-16 units. **Zero means the unnamed
    /// (default) stream** — only that one describes the file itself; named
    /// streams (`$BadClus:$Bad`, `file:Zone.Identifier`, ...) must be ignored
    /// or `du`/`size:` report the stream, not the file.
    name_len: u8,
    value_len: u32,
    value_off: usize,
    real_size: u64,
    /// Allocated clusters in bytes ($DATA only; 0 for resident attributes,
    /// which live inside the FILE record and occupy no clusters).
    allocated: u64,
    /// First VCN of a non-resident stream segment. A stream split across
    /// `$ATTRIBUTE_LIST` records carries its authoritative size only in the
    /// segment whose `lowest_vcn == 0`.
    lowest_vcn: u64,
    mapping_pairs_off: usize,
}

fn iterate_attributes<'a>(rec: &'a [u8], mut off: usize, limit: usize) -> impl Iterator<Item = AttrRef> + 'a {
    std::iter::from_fn(move || {
        if off + 16 > limit.min(rec.len()) {
            return None;
        }
        let attr_type = u32::from_le_bytes(rec[off..off + 4].try_into().unwrap());
        let len = u32::from_le_bytes(rec[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_type == 0xFFFF_FFFF || len < 16 || off + len > rec.len() {
            return None;
        }
        let non_resident = rec[off + 8] != 0;
        let name_len = rec[off + 9];
        let (value_len, value_off, real_size, allocated, lowest_vcn, mapping_pairs_off) =
            if non_resident {
                let mp = u16::from_le_bytes([rec[off + 32], rec[off + 33]]) as usize;
                // non-resident header: lowest_vcn @ +16, allocated_size @ +40,
                // real_size @ +48
                let lv = u64::from_le_bytes(rec[off + 16..off + 24].try_into().unwrap());
                let as_ = u64::from_le_bytes(rec[off + 40..off + 48].try_into().unwrap());
                let rs = u64::from_le_bytes(rec[off + 48..off + 56].try_into().unwrap());
                (0u32, 0usize, rs, as_, lv, off + mp)
            } else {
                let vl = u32::from_le_bytes(rec[off + 16..off + 20].try_into().unwrap());
                let vo = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
                (vl, off + vo, 0u64, 0u64, 0u64, 0usize)
            };
        let end = off + len;
        off += len;
        Some(AttrRef {
            attr_type,
            end,
            non_resident,
            name_len,
            value_len,
            value_off,
            real_size,
            allocated,
            lowest_vcn,
            mapping_pairs_off,
        })
    })
}

/// Parse NTFS mapping pairs into sorted (vcn, lcn, len) runs.
fn parse_runlist(buf: &[u8]) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut off = 0usize;
    let mut vcn = 0u64;
    let mut lcn = 0i64;
    loop {
        if off >= buf.len() {
            break;
        }
        let header = buf[off];
        off += 1;
        if header == 0 {
            break;
        }
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        if len_bytes == 0 || off + len_bytes + off_bytes > buf.len() {
            bail!("malformed run list");
        }
        let mut run_len = 0u64;
        for i in 0..len_bytes {
            run_len |= (buf[off + i] as u64) << (8 * i);
        }
        off += len_bytes;
        if off_bytes > 0 {
            let mut delta = 0i64;
            for i in 0..off_bytes {
                delta |= (buf[off + i] as i64) << (8 * i);
            }
            // sign-extend
            let shift = 64 - 8 * off_bytes;
            delta = (delta << shift) >> shift;
            lcn += delta;
            off += off_bytes;
        } else {
            // sparse run
        }
        runs.push(Run { vcn, lcn, len: run_len });
        vcn += run_len;
    }
    runs.sort_by_key(|r| r.vcn);
    Ok(runs)
}

/// Raw read at an absolute device offset on the volume handle.
fn read_raw(handle: HANDLE, offset: u64, len: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    read_raw_into(handle, offset, &mut buf)?;
    Ok(buf)
}

/// Raw read directly into a caller buffer (no intermediate allocation).
fn read_raw_into(handle: HANDLE, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut pos = offset as i64;
    let ok = unsafe {
        SetFilePointerEx(
            handle,
            pos,
            &mut pos as *mut i64,
            FILE_BEGIN,
        )
    };
    if ok == 0 {
        bail!("SetFilePointerEx failed: error {}", unsafe { GetLastError() });
    }
    let len = buf.len() as u32;
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            len,
            &mut read,
            null_mut(),
        )
    };
    if ok == 0 || read as usize != buf.len() {
        bail!(
            "ReadFile failed at device offset {offset} (read {read}/{len}): error {}",
            unsafe { GetLastError() }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }
    fn put_u32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
    fn put_u64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

    /// Build a synthetic 1024-byte FILE record with `file_names` $FILE_NAME
    /// attributes (and an optional non-resident $DATA attribute), with a valid
    /// USA covering two 512-byte sectors.
    fn make_file_record(record_number: u32, seq: u16, is_dir: bool, file_names: &[(u64, &str, u64, u32)], data_size: Option<u64>) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        put_u16(&mut rec, 4, 48); // usa_off
        put_u16(&mut rec, 6, 2); // usa_count (2 sectors)
        put_u16(&mut rec, 16, seq);
        put_u16(&mut rec, 18, 1); // link count
        put_u16(&mut rec, 20, 56); // attr_off
        put_u16(&mut rec, 22, 0x01 | if is_dir { 0x02 } else { 0 });
        put_u32(&mut rec, 24, 1024); // bytes in use
        put_u32(&mut rec, 28, 1024);
        put_u32(&mut rec, 44, record_number);
        // USA: usn + fixup value for sector 1
        put_u16(&mut rec, 48, 0x1234);
        put_u16(&mut rec, 50, 0x0000);
        put_u16(&mut rec, 510, 0x1234);
        put_u16(&mut rec, 1022, 0x1234);

        let mut off = 56usize;
        for &(parent, name, size, dos_flags) in file_names {
            let name16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let attr_len = (24 + 66 + name16.len()).div_ceil(8) * 8;
            put_u32(&mut rec, off, 0x30);
            put_u32(&mut rec, off + 4, attr_len as u32);
            rec[off + 8] = 0; // resident
            put_u32(&mut rec, off + 16, (66 + name16.len()) as u32);
            put_u16(&mut rec, off + 20, 24);
            let v = off + 24;
            put_u64(&mut rec, v, parent);
            put_u64(&mut rec, v + 16, 133_000_000_000_000); // mtime filetime
            put_u64(&mut rec, v + 48, size);
            put_u32(&mut rec, v + 56, dos_flags);
            rec[v + 64] = name.encode_utf16().count() as u8;
            rec[v + 65] = 1; // Win32 namespace
            rec[v + 66..v + 66 + name16.len()].copy_from_slice(&name16);
            off += attr_len;
        }
        if let Some(ds) = data_size {
            // non-resident $DATA attribute (no runlist): allocated = real = ds
            put_u32(&mut rec, off, 0x80);
            put_u32(&mut rec, off + 4, 64);
            rec[off + 8] = 1;
            put_u64(&mut rec, off + 40, ds); // allocated size
            put_u64(&mut rec, off + 48, ds); // real size
            off += 64;
        }
        put_u32(&mut rec, off, 0xFFFF_FFFF);
        rec
    }

    #[test]
    fn parse_file_record_with_hardlinks() {
        let rec = make_file_record(
            100,
            3,
            false,
            &[
                (5, "target.txt", 1234, 0x02), // hidden
                (77, "alias.txt", 1234, 0x02), // second $FILE_NAME = hard link
            ],
            None,
        );
        let fixed = apply_fixups(&rec, 512).unwrap();
        let hdr = parse_file_header(&fixed).unwrap();
        let entries: Vec<(u64, String, u64, u64, bool)> = iterate_attributes(&fixed, hdr.attr_off, hdr.bytes_in_use)
            .filter(|a| a.attr_type == 0x30 && !a.non_resident)
            .map(|a| {
                let v = &fixed[a.value_off..a.value_off + a.value_len as usize];
                (
                    u64::from_le_bytes(v[0..8].try_into().unwrap()) & FRN_MASK,
                    String::from_utf16_lossy(
                        &(0..v[64] as usize)
                            .map(|i| u16::from_le_bytes([v[66 + i * 2], v[67 + i * 2]]))
                            .collect::<Vec<_>>(),
                    ),
                    u64::from_le_bytes(v[48..56].try_into().unwrap()),
                    u64::from_le_bytes(v[16..24].try_into().unwrap()),
                    u32::from_le_bytes(v[56..60].try_into().unwrap()) & 0x02 != 0,
                )
            })
            .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, "target.txt");
        assert_eq!(entries[1].1, "alias.txt");
        assert_eq!(entries[0].0, 5);
        assert_eq!(entries[1].0, 77);
        assert_eq!(entries[0].2, 1234);
        assert!(entries[0].4); // hidden
        assert_eq!(hdr.record_number, 100);
    }

    #[test]
    fn parse_runlist_contiguous_and_fragmented() {
        // [len=0x10, lcn=0x100], [len=0x08, lcn=+0x40], terminator
        let buf: Vec<u8> = vec![0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x40, 0x00, 0x00];
        let runs = parse_runlist(&buf).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], Run { vcn: 0, lcn: 0x100, len: 0x10 });
        assert_eq!(runs[1], Run { vcn: 0x10, lcn: 0x140, len: 0x08 });
    }

    #[test]
    fn data_attribute_size_parsing() {
        let rec = make_file_record(201, 1, false, &[(5, "big.bin", 0, 0)], Some(7777));
        let fixed = apply_fixups(&rec, 512).unwrap();
        let hdr = parse_file_header(&fixed).unwrap();
        let data: Vec<(bool, u64, u64, u32)> = iterate_attributes(&fixed, hdr.attr_off, hdr.bytes_in_use)
            .filter(|a| a.attr_type == 0x80)
            .map(|a| (a.non_resident, a.real_size, a.allocated, a.value_len))
            .collect();
        assert_eq!(data.len(), 1);
        assert!(data[0].0); // non-resident
        assert_eq!(data[0].1, 7777); // real_size read from offset 48
        assert_eq!(data[0].2, 7777); // allocated read from offset 40
    }

    #[test]
    fn record_allocated_flows_into_entry() {
        // End-to-end: parse_record surfaces the $DATA allocated size on the
        // emitted entry; resident records (no $DATA) get 0.
        let mut rec = make_file_record(202, 1, false, &[(5, "big.bin", 0, 0)], Some(9000));
        let entries = parse_record(&mut rec, 512).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 9000);
        assert_eq!(entries[0].allocated, 9000);

        let mut resident = make_file_record(203, 1, false, &[(5, "tiny.txt", 7, 0)], None);
        let entries = parse_record(&mut resident, 512).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 7); // falls back to $FILE_NAME's field
        assert_eq!(entries[0].allocated, 0);
    }

    /// Append a non-resident $DATA attribute (optionally a *named* stream).
    /// Returns the offset just past it.
    fn put_data_attr(rec: &mut [u8], off: usize, name: Option<&str>, real: u64, allocated: u64) -> usize {
        let name16: Vec<u8> = match name {
            Some(n) => n.encode_utf16().flat_map(|u| u.to_le_bytes()).collect(),
            None => Vec::new(),
        };
        let head = 64usize;
        let attr_len = (head + name16.len()).div_ceil(8) * 8;
        put_u32(rec, off, 0x80);
        put_u32(rec, off + 4, attr_len as u32);
        rec[off + 8] = 1; // non-resident
        rec[off + 9] = match name {
            Some(n) => n.encode_utf16().count() as u8,
            None => 0,
        };
        put_u16(rec, off + 10, if name.is_some() { head as u16 } else { 0 });
        put_u16(rec, off + 32, (head + name16.len()) as u16); // mapping pairs offset
        put_u64(rec, off + 40, allocated);
        put_u64(rec, off + 48, real);
        put_u64(rec, off + 56, real);
        rec[off + head..off + head + name16.len()].copy_from_slice(&name16);
        off + attr_len
    }

    /// Resident $ATTRIBUTE_LIST with a single entry pointing the unnamed
    /// $DATA stream (lowest VCN 0) at `target_record`.
    fn put_attr_list(rec: &mut [u8], off: usize, target_record: u64) -> usize {
        const ENTRY: usize = 26;
        let attr_len = (24 + ENTRY).div_ceil(8) * 8;
        put_u32(rec, off, 0x20);
        put_u32(rec, off + 4, attr_len as u32);
        rec[off + 8] = 0; // resident
        put_u32(rec, off + 16, ENTRY as u32);
        put_u16(rec, off + 20, 24);
        let v = off + 24;
        put_u32(rec, v, 0x80); // $DATA
        put_u16(rec, v + 4, ENTRY as u16);
        rec[v + 6] = 0; // unnamed
        rec[v + 7] = ENTRY as u8;
        put_u64(rec, v + 8, 0); // lowest_vcn
        put_u64(rec, v + 16, target_record);
        off + attr_len
    }

    /// FILE record with one $FILE_NAME (size field 0, like real volumes) plus
    /// explicit non-resident $DATA streams. Returns (record, offset of the
    /// terminator) so callers can append more attributes.
    fn make_record_with_data(
        record_number: u32,
        name: &str,
        datas: &[(Option<&str>, u64, u64)],
    ) -> (Vec<u8>, usize) {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        put_u16(&mut rec, 4, 48);
        put_u16(&mut rec, 6, 2);
        put_u16(&mut rec, 16, 1);
        put_u16(&mut rec, 18, 1);
        put_u16(&mut rec, 20, 56);
        put_u16(&mut rec, 22, 0x01);
        put_u32(&mut rec, 24, 1024);
        put_u32(&mut rec, 28, 1024);
        put_u32(&mut rec, 44, record_number);
        put_u16(&mut rec, 48, 0x1234);
        put_u16(&mut rec, 50, 0x0000);
        put_u16(&mut rec, 510, 0x1234);
        put_u16(&mut rec, 1022, 0x1234);
        let mut off = 56usize;
        let name16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let attr_len = (24 + 66 + name16.len()).div_ceil(8) * 8;
        put_u32(&mut rec, off, 0x30);
        put_u32(&mut rec, off + 4, attr_len as u32);
        rec[off + 8] = 0;
        put_u32(&mut rec, off + 16, (66 + name16.len()) as u32);
        put_u16(&mut rec, off + 20, 24);
        let v = off + 24;
        put_u64(&mut rec, v, 5); // parent FRN
        put_u64(&mut rec, v + 48, 0); // $FILE_NAME size cache: 0, as on disk
        rec[v + 64] = name.encode_utf16().count() as u8;
        rec[v + 65] = 1;
        rec[v + 66..v + 66 + name16.len()].copy_from_slice(&name16);
        off += attr_len;
        for &(n, real, alloc) in datas {
            off = put_data_attr(&mut rec, off, n, real, alloc);
        }
        put_u32(&mut rec, off, 0xFFFF_FFFF);
        (rec, off)
    }

    #[test]
    fn named_data_stream_is_ignored() {
        // $BadClus-style: the unnamed stream is empty, a named stream spans
        // the whole volume. The file size must come from the unnamed one.
        let (mut rec, _) =
            make_record_with_data(300, "bad", &[(None, 0, 0), (Some("$Bad"), 999_999, 0)]);
        let entries = parse_record(&mut rec, 512).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[0].allocated, 0);
    }

    #[test]
    fn unnamed_stream_wins_over_named() {
        let (mut rec, _) = make_record_with_data(
            303,
            "mixed.bin",
            &[(Some("Zone.Identifier"), 26, 0), (None, 4096, 8192)],
        );
        let entries = parse_record(&mut rec, 512).unwrap();
        assert_eq!(entries[0].size, 4096);
        assert_eq!(entries[0].allocated, 8192);
    }

    #[test]
    fn spilled_data_records_fixup_target() {
        // No $DATA in the base record; a resident $ATTRIBUTE_LIST points at
        // record 999 instead (heavily fragmented large file).
        let (mut rec, off) = make_record_with_data(301, "big.img", &[]);
        let off = put_attr_list(&mut rec, off, 999);
        put_u32(&mut rec, off, 0xFFFF_FFFF);
        let entries = parse_record(&mut rec, 512).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fixup_record, Some(999));
    }

    #[test]
    fn attr_list_data_record_picks_unnamed_first_segment() {
        let mut list = vec![0u8; 26];
        put_u32(&mut list, 0, 0x80);
        put_u16(&mut list, 4, 26);
        list[6] = 0;
        list[7] = 26;
        put_u64(&mut list, 8, 0);
        put_u64(&mut list, 16, 4242);
        assert_eq!(attr_list_data_record(&list), Some(4242));

        // A named stream entry must not be picked.
        let mut named = vec![0u8; 26];
        put_u32(&mut named, 0, 0x80);
        put_u16(&mut named, 4, 26);
        named[6] = 4;
        put_u64(&mut named, 16, 777);
        assert_eq!(attr_list_data_record(&named), None);

        // Continuation segment (lowest_vcn != 0) carries no authoritative size.
        let mut cont = vec![0u8; 26];
        put_u32(&mut cont, 0, 0x80);
        put_u16(&mut cont, 4, 26);
        put_u64(&mut cont, 8, 100);
        put_u64(&mut cont, 16, 888);
        assert_eq!(attr_list_data_record(&cont), None);
    }

    #[test]
    fn record_default_data_size_skips_named_streams() {
        let (rec, _) = make_record_with_data(
            302,
            "x.bin",
            &[(Some("$Bad"), 999_999, 0), (None, 4096, 8192)],
        );
        let fixed = apply_fixups(&rec, 512).unwrap();
        assert_eq!(record_default_data_size(&fixed), Some((4096, 8192)));
    }

    /// Non-resident $ATTRIBUTE_LIST (the list outgrew the record) with the
    /// given mapping-pairs bytes.
    fn put_attr_list_nonresident(
        rec: &mut [u8],
        off: usize,
        real_size: u64,
        runlist: &[u8],
    ) -> usize {
        let head = 64usize;
        let attr_len = (head + runlist.len()).div_ceil(8) * 8;
        put_u32(rec, off, 0x20);
        put_u32(rec, off + 4, attr_len as u32);
        rec[off + 8] = 1; // non-resident
        put_u16(rec, off + 32, head as u16);
        put_u64(rec, off + 40, real_size);
        put_u64(rec, off + 48, real_size);
        put_u64(rec, off + 56, real_size);
        rec[off + head..off + head + runlist.len()].copy_from_slice(runlist);
        off + attr_len
    }

    #[test]
    fn nonresident_attr_list_is_carried_for_later_resolution() {
        // 54k-extent style file: $DATA gone, $ATTRIBUTE_LIST itself spilled.
        let (mut rec, off) = make_record_with_data(304, "huge.iso", &[]);
        // header 0x22 = 2-byte length + 2-byte LCN delta, terminator 0x00
        let runlist = [0x22u8, 0x00, 0x01, 0x40, 0x00, 0x00]; // 0x100 clusters @ LCN 0x40
        let off = put_attr_list_nonresident(&mut rec, off, 4096, &runlist);
        put_u32(&mut rec, off, 0xFFFF_FFFF);
        let entries = parse_record(&mut rec, 512).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fixup_record, None);
        let (runs, size) = entries[0].fixup_list.as_ref().expect("list carried");
        assert_eq!(*size, 4096);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 0x100);
        assert_eq!(runs[0].lcn, 0x40);
    }

    #[test]
    fn filetime_conversion() {
        // 116444736000000000 = 1970-01-01
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000 + 10_000_000), 1);
    }
}
