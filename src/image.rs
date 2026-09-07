//! `fer image` — volume → dynamic VHDX forensic image (used clusters only).
//!
//! Pipeline: open the volume handle (`\\.\X:`) → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`
//! maps the volume onto its physical disk → `FSCTL_GET_VOLUME_BITMAP` returns the
//! NTFS used-cluster bitmap (no NTFS structure parsing needed) → worker threads
//! read only the used sectors straight from `\\.\PhysicalDriveN` → a minimal
//! dynamic-VHDX writer appends the payload blocks → a streaming SHA-256 covers
//! the whole *logical* volume contents (used sectors as-is, unused sectors as
//! zero), which matches the hash a full-dd of a trimmed volume would produce.
//!
//! The VHDX layout follows MS-VHDX (cross-checked against the open-source
//! `vhdx-rs` create path): file identifier, two headers, two region tables, a
//! dormant log region, the BAT, the metadata table + items, then payload blocks.
//! Sector bitmaps are not emitted: every payload block is written in full (tail
//! padded with zeros), so all sectors are valid by default.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, SetFilePointerEx,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_VOLUME_BITMAP};

/// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` — `CTL_CODE('V', 0, METHOD_BUFFERED,
/// FILE_ANY_ACCESS)`; absent from windows-sys 0.61, so defined locally (same
/// style as `DRIVE_FIXED` in usn.rs).
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

// ---------------------------------------------------------------------------
// VHDX layout constants (MS-VHDX)
// ---------------------------------------------------------------------------

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const HEADER1_OFFSET: u64 = 64 * KIB;
const HEADER2_OFFSET: u64 = 128 * KIB;
const REGION1_OFFSET: u64 = 192 * KIB;
const REGION2_OFFSET: u64 = 256 * KIB;
const LOG_OFFSET: u64 = MIB;
const LOG_LENGTH: u64 = MIB;
const BAT_REGION_OFFSET: u64 = 2 * MIB;
const METADATA_REGION_SIZE: u64 = MIB;
const HEADER_SIZE: usize = 4096;
const REGION_TABLE_SIZE: usize = 64 * KIB as usize;
const METADATA_TABLE_SIZE: usize = 64 * KIB as usize;

/// BAT entry state: PAYLOAD_BLOCK_FULLY_PRESENT (bits 0..3 = 6).
const BAT_STATE_PRESENT: u64 = 6;

// Region GUIDs (MS-VHDX §2.2; byte order = little-endian storage).
const BAT_REGION_GUID: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A,
    0x08,
];
const METADATA_REGION_GUID: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88,
    0x6E,
];
// Metadata item GUIDs (MS-VHDX §2.6).
const FILE_PARAMETERS_GUID: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7,
    0x6B,
];
const VIRTUAL_DISK_SIZE_GUID: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4,
    0xB8,
];
const VIRTUAL_DISK_ID_GUID: [u8; 16] = [
    0xAB, 0x12, 0xCA, 0xBE, 0xE6, 0xB2, 0x23, 0x45, 0x93, 0xEF, 0xC3, 0x09, 0xE0, 0x00, 0xC7,
    0x46,
];
const LOGICAL_SECTOR_SIZE_GUID: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB,
    0x5F,
];
const PHYSICAL_SECTOR_SIZE_GUID: [u8; 16] = [
    0xC7, 0x48, 0xA3, 0xCD, 0x5D, 0x44, 0x71, 0x44, 0x9C, 0xC9, 0xE9, 0x88, 0x52, 0x51, 0xC5,
    0x56,
];

// ---------------------------------------------------------------------------
// Win32 structures (repr(C), fixed layouts from MS-FSCC)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct DiskExtent {
    disk_number: u32,
    starting_offset: i64,
    extent_length: i64,
}

#[repr(C)]
struct DiskExtents {
    number_of_extents: u32,
    _reserved: u32,
    extents: [DiskExtent; 1],
}

#[repr(C)]
struct StartingLcnInput {
    starting_lcn: i64,
}

#[repr(C)]
struct NtfsVolumeData {
    _volume_serial: i64,
    _number_sectors: i64,
    total_clusters: i64,
    _free_clusters: i64,
    _total_reserved: i64,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    _bytes_per_file_record: u32,
    _clusters_per_file_record: u32,
    _mft_valid_data_length: i64,
    _mft_start_lcn: i64,
}

// ---------------------------------------------------------------------------
// Raw device helpers
// ---------------------------------------------------------------------------

/// Open a raw device (`\\.\X:` or `\\.\PhysicalDriveN`) for reading.
fn open_device(path: &str) -> Result<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        bail!(
            "cannot open {path} (error {}) — requires an elevated shell",
            unsafe { GetLastError() }
        );
    }
    Ok(handle)
}

fn ioctl_read(handle: HANDLE, code: u32, input: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            input.as_ptr() as *const core::ffi::c_void,
            input.len() as u32,
            out.as_mut_ptr() as *mut core::ffi::c_void,
            out_len as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        bail!("DeviceIoControl failed (error {})", unsafe { GetLastError() });
    }
    out.truncate(returned as usize);
    Ok(out)
}

/// Synchronous positioned read from a raw device handle (no OVERLAPPED: the
/// handle is opened without FILE_FLAG_OVERLAPPED and each thread owns its own).
fn read_device(h: HANDLE, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0usize;
    let mut pos = offset;
    while filled < buf.len() {
        let ok = unsafe { SetFilePointerEx(h, pos as i64, std::ptr::null_mut(), 0 /* FILE_BEGIN */) };
        if ok == 0 {
            bail!("seek to {pos} failed (error {})", unsafe { GetLastError() });
        }
        let mut got = 0u32;
        let ok = unsafe {
            ReadFile(
                h,
                buf[filled..].as_mut_ptr(),
                (buf.len() - filled) as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            bail!("read at {pos} failed (error {})", unsafe { GetLastError() });
        }
        if got == 0 {
            bail!("read at {pos} returned 0 bytes (device shrank?)");
        }
        filled += got as usize;
        pos += u64::from(got);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Volume geometry + used-cluster bitmap
// ---------------------------------------------------------------------------

struct VolumeGeom {
    /// Physical disk number the volume lives on.
    disk_number: u32,
    /// Byte offset of the volume start on the physical disk.
    extent_offset: u64,
    /// Volume length in bytes.
    extent_length: u64,
    bytes_per_cluster: u64,
    total_clusters: u64,
}

fn query_geometry(vol: HANDLE) -> Result<VolumeGeom> {
    let ext_raw = ioctl_read(
        vol,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        std::mem::size_of::<DiskExtents>(),
    )?;
    if ext_raw.len() < 8 {
        bail!("volume extents query returned a truncated buffer");
    }
    let ext = unsafe { &*(ext_raw.as_ptr() as *const DiskExtents) };
    if ext.number_of_extents != 1 {
        bail!(
            "volume spans {} extents — multi-extent (spanned/striped) volumes are not \
             supported yet",
            ext.number_of_extents
        );
    }
    let e = ext.extents[0];
    if e.extent_length <= 0 || e.starting_offset < 0 {
        bail!("volume extent has an invalid offset/length");
    }

    let nd_raw = ioctl_read(
        vol,
        FSCTL_GET_NTFS_VOLUME_DATA,
        &[],
        std::mem::size_of::<NtfsVolumeData>(),
    )?;
    let nd = unsafe { &*(nd_raw.as_ptr() as *const NtfsVolumeData) };
    if nd.bytes_per_cluster == 0 {
        bail!("volume reports zero bytes per cluster (not NTFS?)");
    }

    Ok(VolumeGeom {
        disk_number: e.disk_number,
        extent_offset: e.starting_offset as u64,
        extent_length: e.extent_length as u64,
        bytes_per_cluster: u64::from(nd.bytes_per_cluster),
        total_clusters: nd.total_clusters.max(0) as u64,
    })
}

/// Query the NTFS used-cluster bitmap starting at cluster 0.
fn query_bitmap(vol: HANDLE, geom: &VolumeGeom) -> Result<Vec<u8>> {
    let input = StartingLcnInput { starting_lcn: 0 };
    let input_bytes = unsafe {
        std::slice::from_raw_parts((&input as *const StartingLcnInput).cast::<u8>(), 8)
    };
    // Worst case: 8-byte fixed header + one bit per cluster.
    let need = 8usize
        .saturating_add(geom.total_clusters.div_ceil(8) as usize)
        .max(16);
    let raw = ioctl_read(vol, FSCTL_GET_VOLUME_BITMAP, input_bytes, need)?;
    if raw.len() < 8 {
        bail!("FSCTL_GET_VOLUME_BITMAP returned a truncated buffer");
    }
    Ok(raw[8..].to_vec())
}

/// Expand the cluster bitmap into merged, sorted `(phys_start, phys_end)` byte
/// ranges on the *physical disk* (end-exclusive, cluster-aligned).
fn used_ranges(bitmap: &[u8], geom: &VolumeGeom) -> Vec<(u64, u64)> {
    let bpc = geom.bytes_per_cluster;
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut run_start: Option<u64> = None;
    let clusters = (bitmap.len() as u64 * 8).min(geom.total_clusters);
    for c in 0..clusters {
        let used = bitmap[(c / 8) as usize] & (1 << (c % 8)) != 0;
        if used {
            if run_start.is_none() {
                run_start = Some(c);
            }
        } else if let Some(s) = run_start.take() {
            out.push((geom.extent_offset + s * bpc, geom.extent_offset + c * bpc));
        }
    }
    if let Some(s) = run_start {
        out.push((geom.extent_offset + s * bpc, geom.extent_offset + clusters * bpc));
    }
    out
}

// ---------------------------------------------------------------------------
// Dynamic VHDX writer
// ---------------------------------------------------------------------------

/// Minimal dynamic-VHDX writer: headers + region tables + dormant log + BAT +
/// metadata, then sequentially appended payload blocks.
struct VhdxWriter {
    file: BufWriter<File>,
    block_size: u64,
    /// Payload blocks per sector-bitmap block (MS-VHDX §2.5.1.1).
    chunk_ratio: u64,
    /// Total BAT entries (payload + sector-bitmap slots).
    total_entries: u64,
    /// Next payload block file offset.
    next_payload_offset: u64,
}

impl VhdxWriter {
    fn create(
        path: &Path, virtual_size: u64, block_size: u32, logical_sector_size: u32,
    ) -> Result<Self> {
        let bs = u64::from(block_size);
        let num_payload = virtual_size.div_ceil(bs);
        let chunk_ratio = (1u64 << 23) * u64::from(logical_sector_size) / bs;
        let num_sb = num_payload.div_ceil(chunk_ratio);
        let total_entries = num_payload + num_sb;
        let bat_bytes = (total_entries * 8).div_ceil(MIB).max(1) * MIB;
        let metadata_offset = BAT_REGION_OFFSET + bat_bytes;
        let end = metadata_offset + METADATA_REGION_SIZE;
        // First payload block sits 1 MiB-aligned after the metadata region.
        let first_payload_offset = end.div_ceil(MIB) * MIB;

        let f = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        let mut w = BufWriter::new(f);

        // 1. File type identifier.
        let mut buf = [0u8; 64 * KIB as usize];
        buf[..8].copy_from_slice(b"vhdxfile");
        let creator = "fer".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        buf[8..8 + creator.len()].copy_from_slice(&creator);
        w.write_all(&buf)?;

        // 2. Headers 1 + 2.
        let file_write_guid = guid_v4();
        let data_write_guid = guid_v4();
        write_all_at(&mut w, HEADER1_OFFSET, &build_header(0, &file_write_guid, &data_write_guid))?;
        write_all_at(&mut w, HEADER2_OFFSET, &build_header(1, &file_write_guid, &data_write_guid))?;

        // 3. Region tables 1 + 2.
        let region = build_region_table(bat_bytes, metadata_offset);
        write_all_at(&mut w, REGION1_OFFSET, &region)?;
        write_all_at(&mut w, REGION2_OFFSET, &region)?;

        // 4. Log + BAT + metadata region: zero-filled by set_len (dynamic disk
        //    BAT = all NOT_PRESENT).
        w.flush()?;
        w.get_ref().set_len(end)?;

        // 5. Metadata table + items.
        let (table, items) = build_metadata(virtual_size, block_size, logical_sector_size);
        write_all_at(&mut w, metadata_offset, &table)?;
        write_all_at(&mut w, metadata_offset + METADATA_TABLE_SIZE as u64, &items)?;
        w.flush()?;

        Ok(Self {
            file: w,
            block_size: bs,
            chunk_ratio,
            total_entries,
            next_payload_offset: first_payload_offset,
        })
    }

    /// Append one full payload block (must be exactly `block_size` bytes) and
    /// update its BAT entry. `idx` is the payload block index (skipped
    /// all-zero blocks leave their BAT entries NOT_PRESENT, so `idx` is not
    /// the number of written blocks).
    fn write_block(&mut self, idx: u64, data: &[u8]) -> Result<()> {
        if data.len() as u64 != self.block_size {
            bail!("payload block must be exactly {} bytes", self.block_size);
        }
        let off_mb = self.next_payload_offset / MIB;
        if off_mb >= (1u64 << 44) {
            bail!("payload offset exceeds BAT FileOffsetMB range");
        }
        write_all_at(&mut self.file, self.next_payload_offset, data)?;

        let entry_idx = idx + idx / self.chunk_ratio;
        if entry_idx >= self.total_entries {
            bail!("BAT entry {entry_idx} out of range");
        }
        let entry = BAT_STATE_PRESENT | (off_mb << 20);
        write_all_at(&mut self.file, BAT_REGION_OFFSET + entry_idx * 8, &entry.to_le_bytes())?;

        self.next_payload_offset += self.block_size;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(())
    }
}

fn write_all_at(w: &mut (impl Write + Seek), offset: u64, data: &[u8]) -> Result<()> {
    w.seek(SeekFrom::Start(offset))?;
    w.write_all(data)?;
    Ok(())
}

/// A v4 GUID from the OS randomness source (version/variant bits set).
fn guid_v4() -> [u8; 16] {
    let mut g = [0u8; 16];
    getrandom::fill(&mut g).expect("os randomness unavailable");
    g[6] = (g[6] & 0x0F) | 0x40; // version 4
    g[8] = (g[8] & 0x3F) | 0x80; // variant 10
    g
}

fn build_header(
    sequence: u64, file_write_guid: &[u8; 16], data_write_guid: &[u8; 16],
) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[..4].copy_from_slice(b"head");
    buf[8..16].copy_from_slice(&sequence.to_le_bytes());
    buf[16..32].copy_from_slice(file_write_guid);
    buf[32..48].copy_from_slice(data_write_guid);
    // LogGuid: all zero = no active log.
    buf[64..66].copy_from_slice(&0u16.to_le_bytes()); // LogVersion
    buf[66..68].copy_from_slice(&1u16.to_le_bytes()); // Version
    buf[68..72].copy_from_slice(&(LOG_LENGTH as u32).to_le_bytes());
    buf[72..80].copy_from_slice(&LOG_OFFSET.to_le_bytes());
    let checksum = crc32c::crc32c(&buf);
    buf[4..8].copy_from_slice(&checksum.to_le_bytes());
    buf
}

fn build_region_table(bat_size: u64, metadata_offset: u64) -> [u8; REGION_TABLE_SIZE] {
    let mut buf = [0u8; REGION_TABLE_SIZE];
    buf[..4].copy_from_slice(b"regi");
    buf[8..12].copy_from_slice(&2u32.to_le_bytes()); // entry count
    // Entry 0: BAT region.
    buf[16..32].copy_from_slice(&BAT_REGION_GUID);
    buf[32..40].copy_from_slice(&BAT_REGION_OFFSET.to_le_bytes());
    buf[40..44].copy_from_slice(&(bat_size as u32).to_le_bytes());
    buf[44..48].copy_from_slice(&1u32.to_le_bytes()); // Required
    // Entry 1: metadata region.
    buf[48..64].copy_from_slice(&METADATA_REGION_GUID);
    buf[64..72].copy_from_slice(&metadata_offset.to_le_bytes());
    buf[72..76].copy_from_slice(&(METADATA_REGION_SIZE as u32).to_le_bytes());
    buf[76..80].copy_from_slice(&1u32.to_le_bytes()); // Required
    let checksum = crc32c::crc32c(&buf);
    buf[4..8].copy_from_slice(&checksum.to_le_bytes());
    buf
}

/// Metadata table (64 KiB) + packed items: File Parameters, Virtual Disk Size,
/// Virtual Disk ID, Logical Sector Size, Physical Sector Size.
fn build_metadata(
    virtual_size: u64, block_size: u32, logical_sector_size: u32,
) -> ([u8; METADATA_TABLE_SIZE], Vec<u8>) {
    // File Parameters: BlockSize (u32) | bit32 = IsFixed (false) |
    // bit33 = HasParent (false).
    let mut file_params = [0u8; 8];
    file_params[..4].copy_from_slice(&block_size.to_le_bytes());
    let virtual_disk_id = guid_v4();

    let mut items: Vec<u8> = Vec::new();
    let mut entries: Vec<(u16, u32, u32, u32)> = Vec::new();
    let push = |items: &mut Vec<u8>, entries: &mut Vec<(u16, u32, u32, u32)>, guid_idx: u16, bytes: &[u8], is_virtual_disk: bool| {
        let offset = METADATA_TABLE_SIZE as u32 + items.len() as u32;
        items.extend_from_slice(bytes);
        // Flags: bit 0 = IsUser, bit 1 = IsVirtualDisk, bit 2 = IsRequired.
        let flags = 0b101u32 | (u32::from(is_virtual_disk) << 1);
        entries.push((guid_idx, offset, bytes.len() as u32, flags));
    };
    push(&mut items, &mut entries, 0, &file_params, false);
    push(&mut items, &mut entries, 1, &virtual_size.to_le_bytes(), true);
    push(&mut items, &mut entries, 2, &virtual_disk_id, true);
    push(&mut items, &mut entries, 3, &logical_sector_size.to_le_bytes(), true);
    push(&mut items, &mut entries, 4, &logical_sector_size.to_le_bytes(), true);

    let guids: [[u8; 16]; 5] = [
        FILE_PARAMETERS_GUID,
        VIRTUAL_DISK_SIZE_GUID,
        VIRTUAL_DISK_ID_GUID,
        LOGICAL_SECTOR_SIZE_GUID,
        PHYSICAL_SECTOR_SIZE_GUID,
    ];
    let mut table = [0u8; METADATA_TABLE_SIZE];
    table[..8].copy_from_slice(b"metadata");
    table[10..12].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    for (i, (guid_idx, offset, len, flags)) in entries.iter().enumerate() {
        let base = 32 + i * 32;
        table[base..base + 16].copy_from_slice(&guids[*guid_idx as usize]);
        table[base + 16..base + 20].copy_from_slice(&offset.to_le_bytes());
        table[base + 20..base + 24].copy_from_slice(&len.to_le_bytes());
        table[base + 24..base + 28].copy_from_slice(&flags.to_le_bytes());
    }
    (table, items)
}

// ---------------------------------------------------------------------------
// Imaging pipeline
// ---------------------------------------------------------------------------

/// Options for one `fer image` run.
pub struct ImageOptions<'a> {
    pub volume: char,
    pub output: &'a Path,
    pub block_size_mb: u32,
    pub threads: Option<usize>,
    pub verify: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct ImageReport {
    pub volume: String,
    pub output: String,
    pub volume_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: f64,
    pub image_bytes: u64,
    pub sha256: String,
    pub blocks: u64,
    pub elapsed_ms: u128,
    pub verified: bool,
}

/// Run the full imaging pipeline.
pub fn run(opts: &ImageOptions) -> Result<ImageReport> {
    use std::time::Instant;
    let start = Instant::now();

    let block_size = u64::from(opts.block_size_mb) * MIB;
    if !(MIB..=256 * MIB).contains(&block_size) || !opts.block_size_mb.is_power_of_two() {
        bail!("--block-size-mb must be a power of two between 1 and 256");
    }

    // 1. Volume handle + geometry + bitmap.
    let vol_path = format!(r"\\.\{}:", opts.volume.to_ascii_uppercase());
    let vol = open_device(&vol_path)?;
    let geom = query_geometry(vol)?;
    let bitmap = query_bitmap(vol, &geom)?;
    unsafe { CloseHandle(vol) };

    // 2. Used-sector ranges on the physical disk.
    let ranges = used_ranges(&bitmap, &geom);
    let used_bytes: u64 = ranges.iter().map(|(s, e)| e - s).sum();
    let volume_bytes = geom.extent_length;

    // 3. Block grid over the volume.
    let num_blocks = volume_bytes.div_ceil(block_size);
    let threads = opts
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16));

    // 4. Writer.
    let mut writer =
        VhdxWriter::create(opts.output, volume_bytes, block_size as u32, 4096)?;

    // 5. Parallel readers + ordered writer. Readers pull block indices from a
    //    shared counter (out-of-order completion), the main thread re-orders
    //    by index and writes sequentially. Both live inside the same scope so
    //    the inflight window can never deadlock (the consumer is the scope's
    //    main thread); a writer failure drops the sender side, readers observe
    //    the closed channel and exit before the scope joins them.
    let disk_path = format!(r"\\.\PhysicalDrive{}", geom.disk_number);
    let next_block = AtomicUsize::new(0);
    let inflight = AtomicUsize::new(0);
    let max_inflight = threads * 2;
    let (tx, rx) = mpsc::channel::<(u64, Vec<u8>, bool)>();
    // `mpsc::Receiver` is !Sync, so the quit signal is an atomic flag instead.
    // A Drop guard flips it on every exit path out of the scope (success or
    // error): readers blocked on the inflight window then observe it and exit
    // before `scope()` joins them — no deadlock window.
    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct QuitOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for QuitOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let _quit_guard = QuitOnDrop(quit.clone());

    let mut hasher = Sha256::new();
    let mut pending: BTreeMap<u64, (Vec<u8>, bool)> = BTreeMap::new();
    let mut next_out = 0u64;
    let mut blocks_stored = 0u64;

    let scope_result: Result<()> = std::thread::scope(|scope| -> Result<()> {
        // Shared state is borrowed by the reader closures (scoped threads
        // permit borrowing); each reader also owns a private disk handle.
        let disk_path = &disk_path;
        let next_block = &next_block;
        let inflight = &inflight;
        let ranges = &ranges;
        let geom = &geom;
        let mut handles = Vec::new();
        for _ in 0..threads {
            let tx = tx.clone();
            let quit = quit.clone();
            handles.push(scope.spawn(move || -> Result<()> {
                let h = open_device(disk_path)?;
                let mut buf = vec![0u8; block_size as usize];
                loop {
                    let idx = next_block.fetch_add(1, Ordering::Relaxed) as u64;
                    if idx >= num_blocks {
                        break;
                    }
                    // Backpressure: bounded number of blocks in flight.
                    while inflight.load(Ordering::Acquire) >= max_inflight {
                        if quit.load(Ordering::Acquire) {
                            unsafe { CloseHandle(h) };
                            return Ok(()); // writer bailed
                        }
                        std::thread::yield_now();
                    }
                    inflight.fetch_add(1, Ordering::AcqRel);
                    let touched = fill_block(h, ranges, geom, idx, block_size, &mut buf)?;
                    if tx.send((idx, std::mem::take(&mut buf), touched)).is_err() {
                        unsafe { CloseHandle(h) };
                        return Ok(()); // writer bailed: receiver dropped
                    }
                    buf = vec![0u8; block_size as usize];
                }
                unsafe { CloseHandle(h) };
                Ok(())
            }));
        }
        drop(tx);

        // Ordered consume + write + hash (the scope's main thread). All-zero
        // blocks feed the hash but are skipped on disk (BAT stays
        // NOT_PRESENT), keeping the dynamic image at used-cluster size.
        for (idx, buf, touched) in rx {
            pending.insert(idx, (buf, touched));
            while let Some((&i, _)) = pending.first_key_value() {
                if i != next_out {
                    break;
                }
                let (data, touched) = pending.remove(&i).expect("first key present");
                let valid = (volume_bytes.saturating_sub(i * block_size)).min(block_size);
                if touched {
                    writer.write_block(i, &data)?;
                    blocks_stored += 1;
                }
                hasher.update(&data[..valid as usize]);
                inflight.fetch_sub(1, Ordering::AcqRel);
                next_out += 1;
                if next_out.is_multiple_of(32) || next_out == num_blocks {
                    eprintln!(
                        "\r[{}/{} blocks] {:.1}%",
                        next_out,
                        num_blocks,
                        next_out as f64 / num_blocks as f64 * 100.0
                    );
                }
            }
        }
        // Scope exit drops quit_tx -> readers waiting on backpressure see the
        // disconnected channel and exit; scope() then joins them all.
        for h in handles {
            match h.join() {
                Ok(r) => r?,
                Err(_) => bail!("a reader thread panicked"),
            }
        }
        Ok(())
    });
    scope_result?;

    if next_out != num_blocks {
        bail!("internal error: {next_out}/{num_blocks} blocks consumed");
    }
    writer.finish()?;

    let sha256 = hex_encode(&hasher.finalize());
    let image_bytes = std::fs::metadata(opts.output)?.len();

    // 7. Optional self-verification: re-open the finished image, re-read the
    //    payload blocks, and compare the recomputed hash.
    let verified = if opts.verify {
        verify_image(opts.output, volume_bytes, block_size, &sha256)?
    } else {
        false
    };

    Ok(ImageReport {
        volume: format!("{}:", opts.volume.to_ascii_uppercase()),
        output: opts.output.display().to_string(),
        volume_bytes,
        used_bytes,
        used_percent: used_bytes as f64 / volume_bytes.max(1) as f64 * 100.0,
        image_bytes,
        sha256,
        blocks: blocks_stored,
        elapsed_ms: start.elapsed().as_millis(),
        verified,
    })
}

/// Fill one block buffer: zeros everywhere, real data where the block overlaps
/// used cluster ranges on the physical disk. Ranges are looked up by binary
/// search (blocks arrive in arbitrary order across threads). Returns whether
/// any used sector touched the block (all-zero blocks are skipped entirely and
/// their BAT entries stay NOT_PRESENT).
fn fill_block(
    h: HANDLE, ranges: &[(u64, u64)], geom: &VolumeGeom, idx: u64, block_size: u64,
    buf: &mut [u8],
) -> Result<bool> {
    buf.fill(0);
    let b_start = geom.extent_offset + idx * block_size;
    let b_end = b_start + block_size;
    let mut ri = ranges.partition_point(|&(_, e)| e <= b_start);
    let mut touched = false;
    while ri < ranges.len() {
        let (rs, re) = ranges[ri];
        if rs >= b_end {
            break;
        }
        let s = rs.max(b_start);
        let e = re.min(b_end);
        let off = (s - b_start) as usize;
        let len = (e - s) as usize;
        read_device(h, s, &mut buf[off..off + len])?;
        touched = true;
        ri += 1;
    }
    Ok(touched)
}

/// Re-open the finished VHDX, walk its BAT, re-read the stored payload blocks
/// (NOT_PRESENT blocks hash as zero), and compare the recomputed volume hash
/// with `expected`.
fn verify_image(path: &Path, volume_bytes: u64, block_size: u64, expected: &str) -> Result<bool> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut sig = [0u8; 8];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut sig)?;
    if &sig != b"vhdxfile" {
        bail!("verify: bad VHDX signature");
    }
    let mut region = [0u8; REGION_TABLE_SIZE];
    f.seek(SeekFrom::Start(REGION1_OFFSET))?;
    f.read_exact(&mut region)?;
    if &region[..4] != b"regi" {
        bail!("verify: bad region table");
    }
    let bat_region_offset =
        u64::from_le_bytes(region[32..40].try_into().expect("8 bytes"));

    // Logical sector size is fixed at 4096 by the writer.
    let chunk_ratio = (1u64 << 23) * 4096 / block_size;
    let num_blocks = volume_bytes.div_ceil(block_size);
    let file_end = std::fs::metadata(path)?.len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; block_size as usize];
    let zeros = vec![0u8; block_size as usize];
    for i in 0..num_blocks {
        let entry_idx = i + i / chunk_ratio;
        let mut entry = [0u8; 8];
        f.seek(SeekFrom::Start(bat_region_offset + entry_idx * 8))?;
        f.read_exact(&mut entry)?;
        let entry = u64::from_le_bytes(entry);
        let valid = (volume_bytes.saturating_sub(i * block_size)).min(block_size);
        if entry & 0b111 == BAT_STATE_PRESENT {
            let off = (entry >> 20) * MIB;
            if off + block_size > file_end {
                bail!("verify: payload block {i} truncated");
            }
            f.seek(SeekFrom::Start(off))?;
            f.read_exact(&mut buf)?;
            hasher.update(&buf[..valid as usize]);
        } else {
            hasher.update(&zeros[..valid as usize]);
        }
    }
    Ok(hex_encode(&hasher.finalize()) == expected)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Tests (no admin required — pure structure/geometry checks)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> VolumeGeom {
        VolumeGeom {
            disk_number: 0,
            extent_offset: 1024 * 1024,
            extent_length: 8 * MIB,
            bytes_per_cluster: 4096,
            total_clusters: 8 * MIB / 4096,
        }
    }

    #[test]
    fn used_ranges_merges_runs() {
        let g = geom();
        // Clusters 0 and 1 used, 2 free, 3..5 used.
        let bitmap = vec![0b0011_1011u8];
        let ranges = used_ranges(&bitmap, &g);
        assert_eq!(
            ranges,
            vec![
                (g.extent_offset, g.extent_offset + 2 * 4096),
                (g.extent_offset + 3 * 4096, g.extent_offset + 6 * 4096),
            ]
        );
    }

    #[test]
    fn used_ranges_empty_bitmap() {
        let g = geom();
        assert!(used_ranges(&[0u8; 4], &g).is_empty());
    }

    #[test]
    fn used_ranges_all_used() {
        let g = geom();
        // Bitmap covering every cluster of the 8 MiB volume (2048 clusters).
        let bitmap = vec![0xFFu8; 256];
        let ranges = used_ranges(&bitmap, &g);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (g.extent_offset, g.extent_offset + g.extent_length));
    }

    #[test]
    fn vhdx_writer_layout_and_bat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.vhdx");
        let virtual_size = 8 * MIB;
        let block_size = 1 * MIB as u32;
        let mut w = VhdxWriter::create(&path, virtual_size, block_size, 4096).expect("create");
        let zeros = vec![0u8; block_size as usize];
        for i in 0..8u64 {
            w.write_block(i, &zeros).expect("write block");
        }
        w.finish().expect("finish");

        let bytes = std::fs::read(&path).expect("read");
        // Signature + creator.
        assert_eq!(&bytes[..8], b"vhdxfile");
        // Header 1 magic + CRC32C (over the whole 4 KiB structure).
        assert_eq!(&bytes[HEADER1_OFFSET as usize..HEADER1_OFFSET as usize + 4], b"head");
        let hdr = &bytes[HEADER1_OFFSET as usize..HEADER1_OFFSET as usize + HEADER_SIZE];
        let stored_crc = u32::from_le_bytes(hdr[4..8].try_into().expect("crc"));
        let mut hdr_nocrc = hdr.to_vec();
        hdr_nocrc[4..8].fill(0);
        assert_eq!(crc32c::crc32c(&hdr_nocrc), stored_crc);
        // Region table.
        let region = &bytes[REGION1_OFFSET as usize..REGION1_OFFSET as usize + REGION_TABLE_SIZE];
        assert_eq!(&region[..4], b"regi");
        assert_eq!(&region[16..32], &BAT_REGION_GUID);
        // Metadata table: signature + 5 entries.
        let bat_bytes = (bat_entries(virtual_size, block_size as u64, 4096) * 8).div_ceil(MIB).max(1) * MIB;
        let meta_off = (BAT_REGION_OFFSET + bat_bytes) as usize;
        assert_eq!(&bytes[meta_off..meta_off + 8], b"metadata");
        let cnt = u16::from_le_bytes(bytes[meta_off + 10..meta_off + 12].try_into().expect("cnt"));
        assert_eq!(cnt, 5);
        // BAT: chunk_ratio for 1 MiB blocks @4096 is 32768, so the first 8
        // payload entries sit at BAT indexes 0..8.
        let first_payload =
            (meta_off + METADATA_REGION_SIZE as usize).div_ceil(MIB as usize) * MIB as usize;
        for i in 0..8usize {
            let e = u64::from_le_bytes(
                bytes[BAT_REGION_OFFSET as usize + i * 8..BAT_REGION_OFFSET as usize + (i + 1) * 8]
                    .try_into()
                    .expect("bat entry"),
            );
            assert_eq!(e & 0b111, BAT_STATE_PRESENT);
            assert_eq!((e >> 20) * MIB, first_payload as u64 + i as u64 * MIB);
        }
    }

    #[test]
    fn bat_entry_mapping_interleaves_sector_bitmaps() {
        // 1 MiB blocks @ 512-byte logical sectors: chunk_ratio = 2^23*512/1MiB = 4096.
        // Write 4097 blocks: block 4096 must land in BAT entry 4097 (one SB slot).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t2.vhdx");
        let virtual_size = 4097 * MIB;
        let block_size = 1 * MIB as u32;
        let mut w = VhdxWriter::create(&path, virtual_size, block_size, 512).expect("create");
        let zeros = vec![0u8; block_size as usize];
        for i in 0..4097u64 {
            w.write_block(i, &zeros).expect("write block");
        }
        w.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read");
        let bat_bytes =
            (bat_entries(virtual_size, block_size as u64, 512) * 8).div_ceil(MIB).max(1) * MIB;
        let e4096 = u64::from_le_bytes(
            bytes[BAT_REGION_OFFSET as usize + 4097 * 8..BAT_REGION_OFFSET as usize + 4098 * 8]
                .try_into()
                .expect("bat entry"),
        );
        assert_eq!(e4096 & 0b111, BAT_STATE_PRESENT);
        // The SB slot at index 4096 stays NOT_PRESENT.
        let sb = u64::from_le_bytes(
            bytes[BAT_REGION_OFFSET as usize + 4096 * 8..BAT_REGION_OFFSET as usize + 4097 * 8]
                .try_into()
                .expect("sb entry"),
        );
        assert_eq!(sb, 0);
        let _ = bat_bytes;
    }

    #[test]
    fn skipped_zero_blocks_stay_not_present() {
        // Write blocks 0 and 2, skip block 1: its BAT entry must remain 0
        // (NOT_PRESENT) and the stored offsets must stay sequential.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t3.vhdx");
        let virtual_size = 8 * MIB;
        let block_size = 1 * MIB as u32;
        let mut w = VhdxWriter::create(&path, virtual_size, block_size, 4096).expect("create");
        let zeros = vec![0u8; block_size as usize];
        w.write_block(0, &zeros).expect("write 0");
        w.write_block(2, &zeros).expect("write 2");
        w.finish().expect("finish");

        let bytes = std::fs::read(&path).expect("read");
        let bat_bytes =
            (bat_entries(virtual_size, block_size as u64, 4096) * 8).div_ceil(MIB).max(1) * MIB;
        let meta_off = BAT_REGION_OFFSET + bat_bytes;
        let first_payload =
            (meta_off + METADATA_REGION_SIZE).div_ceil(MIB) * MIB;
        let e0 = u64::from_le_bytes(
            bytes[BAT_REGION_OFFSET as usize..BAT_REGION_OFFSET as usize + 8]
                .try_into()
                .expect("e0"),
        );
        let e1 = u64::from_le_bytes(
            bytes[BAT_REGION_OFFSET as usize + 8..BAT_REGION_OFFSET as usize + 16]
                .try_into()
                .expect("e1"),
        );
        let e2 = u64::from_le_bytes(
            bytes[BAT_REGION_OFFSET as usize + 16..BAT_REGION_OFFSET as usize + 24]
                .try_into()
                .expect("e2"),
        );
        assert_eq!(e0 & 0b111, BAT_STATE_PRESENT);
        assert_eq!(e1, 0, "skipped block must stay NOT_PRESENT");
        assert_eq!(e2 & 0b111, BAT_STATE_PRESENT);
        assert_eq!((e0 >> 20) * MIB, first_payload);
        assert_eq!((e2 >> 20) * MIB, first_payload + MIB);
        // File size = metadata end + 2 stored blocks (the skipped block costs
        // nothing).
        let expected_len = first_payload + 2 * MIB;
        assert_eq!(bytes.len() as u64, expected_len);
    }

    fn bat_entries(virtual_size: u64, block_size: u64, logical: u32) -> u64 {
        let num_payload = virtual_size.div_ceil(block_size);
        let chunk_ratio = (1u64 << 23) * u64::from(logical) / block_size;
        num_payload + num_payload.div_ceil(chunk_ratio)
    }
}
