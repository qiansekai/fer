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
    pub mtime: i64, // unix seconds
    pub ctime: i64, // unix seconds
    pub hidden: bool,
    pub system: bool,
    pub readonly: bool,
    pub reparse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
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
fn filetime_to_unix(ft: u64) -> i64 {
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
                    on_entry(&entry);
                }
            }
        }
        Ok(records)
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
    let mut names: Vec<(u64, String, bool, bool, bool, bool)> = Vec::new();
    let mut max_size = 0u64;
    for attr in iterate_attributes(rec, hdr.attr_off, hdr.bytes_in_use) {
        match attr.attr_type {
            0x10 if !attr.non_resident && attr.value_len >= 24 => {
                let v = &rec[attr.value_off..attr.value_off + attr.value_len as usize];
                std_ctime = filetime_to_unix(u64::from_le_bytes(v[0..8].try_into().unwrap()));
                std_mtime = filetime_to_unix(u64::from_le_bytes(v[8..16].try_into().unwrap()));
            }
            0x80 => {
                data_size = if attr.non_resident {
                    Some(attr.real_size)
                } else {
                    Some(attr.value_len as u64)
                };
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
    let mut out = Vec::with_capacity(names.len());
    for (parent_frn, name, hidden, system, readonly, reparse) in names {
        out.push(MftEntry {
            frn,
            parent_frn,
            name,
            is_dir,
            size,
            mtime: std_mtime.max(0),
            ctime: std_ctime.max(0),
            hidden,
            system,
            readonly,
            reparse,
        });
    }
    Some(out)
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
    value_len: u32,
    value_off: usize,
    real_size: u64,
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
        let (value_len, value_off, real_size, mapping_pairs_off) = if non_resident {
            let mp = u16::from_le_bytes([rec[off + 32], rec[off + 33]]) as usize;
            let rs = u64::from_le_bytes(rec[off + 48..off + 56].try_into().unwrap());
            (0u32, 0usize, rs, off + mp)
        } else {
            let vl = u32::from_le_bytes(rec[off + 16..off + 20].try_into().unwrap());
            let vo = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            (vl, off + vo, 0u64, 0usize)
        };
        let end = off + len;
        off += len;
        Some(AttrRef {
            attr_type,
            end,
            non_resident,
            value_len,
            value_off,
            real_size,
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
            // non-resident $DATA attribute (no runlist), real size = ds
            put_u32(&mut rec, off, 0x80);
            put_u32(&mut rec, off + 4, 64);
            rec[off + 8] = 1;
            put_u64(&mut rec, off + 48, ds);
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
        let data: Vec<(bool, u64, u32)> = iterate_attributes(&fixed, hdr.attr_off, hdr.bytes_in_use)
            .filter(|a| a.attr_type == 0x80)
            .map(|a| (a.non_resident, a.real_size, a.value_len))
            .collect();
        assert_eq!(data.len(), 1);
        assert!(data[0].0); // non-resident
        assert_eq!(data[0].1, 7777); // real_size read from offset 48
    }

    #[test]
    fn filetime_conversion() {
        // 116444736000000000 = 1970-01-01
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000 + 10_000_000), 1);
    }
}
