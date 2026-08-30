//! Windows NTFS USN / MFT enumeration.
//!
//! The USN path reads the MFT directly via `FSCTL_ENUM_USN_DATA` — the same
//! mechanism Everything uses — so a full volume is indexed in seconds.
//! Requires an elevated (admin) process; without admin, use [`crate::walk`].

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V1,
    READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};

use crate::{EntryMeta, FRN_MASK, ROOT_FRN};

/// GetDriveTypeW result: fixed disk. (windows-sys puts it in
/// Win32::System::WindowsProgramming; defining it locally avoids another feature.)
const DRIVE_FIXED: u32 = 3;

/// One parsed USN / MFT record.
#[derive(Debug, Clone)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub is_dir: bool,
    pub usn: i64,
    pub reason: u32,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub drive: char,
    pub label: String,
    pub fs: String,
}

/// List fixed local NTFS volumes.
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    let drives = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }
        let drive = (b'A' + i as u8) as char;
        let root_wide: Vec<u16> = format!("{drive}:\\").encode_utf16().chain(std::iter::once(0)).collect();
        if unsafe { GetDriveTypeW(root_wide.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 32];
        let ok = unsafe {
            GetVolumeInformationW(
                root_wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue;
        }
        let fs_name = String::from_utf16_lossy(&fs).trim_end_matches('\0').to_string();
        let label_name = String::from_utf16_lossy(&label).trim_end_matches('\0').to_string();
        out.push(VolumeInfo {
            drive,
            label: label_name,
            fs: fs_name,
        });
    }
    out
}

pub struct UsnVolume {
    handle: HANDLE,
    pub drive: char,
    /// Reused FSCTL_ENUM_USN_DATA buffer for [`UsnVolume::lookup`] (the
    /// monitor resolves a parent path per event — allocating 64 KB per call
    /// would churn the allocator for nothing).
    lookup_buf: Vec<u8>,
}

impl UsnVolume {
    pub fn open(drive: char) -> Result<Self> {        let wide: Vec<u16> = format!(r"\\.\{drive}:")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!(
                "cannot open volume {drive}: (error {}) — requires an elevated shell",
                unsafe { GetLastError() }
            );
        }
        Ok(UsnVolume {
            handle,
            drive,
            lookup_buf: Vec::with_capacity(64 * 1024),
        })
    }

    /// Raw volume handle (for advanced ioctl calls; `pub(crate)`-adjacent API).
    pub fn raw_handle(&self) -> HANDLE {
        self.handle
    }

    fn ioctl(
        &self,
        code: u32,
        input: *const c_void,
        in_size: u32,
        out: *mut u8,
        out_size: u32,
        returned: &mut u32,
    ) -> bool {
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                input,
                in_size,
                out as *mut c_void,
                out_size,
                returned,
                null_mut(),
            ) != 0
        }
    }

    /// Enumerate every MFT record on the volume (the fast, Everything-grade path).
    /// Returns `(record_count, max_usn)`.
    pub fn enumerate(&self, mut on_record: impl FnMut(&UsnRecord)) -> Result<(u64, i64)> {
        let mut mft = MFT_ENUM_DATA_V1 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: i64::MAX,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut records = 0u64;
        let mut max_usn = 0i64;
        loop {
            let mut returned = 0u32;
            let ok = self.ioctl(
                FSCTL_ENUM_USN_DATA,
                &mut mft as *mut _ as *const c_void,
                size_of::<MFT_ENUM_DATA_V1>() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut returned,
            );
            if !ok {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    break;
                }
                bail!("FSCTL_ENUM_USN_DATA failed on {}: (error {err})", self.drive);
            }
            if returned < 8 {
                break;
            }
            let next_frn = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            if next_frn == 0 {
                break;
            }
            let parsed = parse_usn_buffer(&buf[..returned as usize]);
            if parsed.is_empty() {
                break; // header-only: nothing left to enumerate
            }
            for r in &parsed {
                max_usn = max_usn.max(r.usn);
                on_record(r);
            }
            records += parsed.len() as u64;
            mft.StartFileReferenceNumber = next_frn;
        }
        Ok((records, max_usn))
    }

    /// Look up one MFT record by file reference number (used to resolve parent paths).
    pub fn lookup(&mut self, frn: u64) -> Option<(u64, String, bool)> {
        let mut mft = MFT_ENUM_DATA_V1 {
            StartFileReferenceNumber: frn.saturating_sub(1),
            LowUsn: 0,
            HighUsn: i64::MAX,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        self.lookup_buf.clear();
        self.lookup_buf.resize(64 * 1024, 0);
        let (buf_ptr, buf_len) = (self.lookup_buf.as_mut_ptr(), self.lookup_buf.len() as u32);
        let mut returned = 0u32;
        let ok = self.ioctl(
            FSCTL_ENUM_USN_DATA,
            &mut mft as *mut _ as *const c_void,
            size_of::<MFT_ENUM_DATA_V1>() as u32,
            buf_ptr,
            buf_len,
            &mut returned,
        );
        if !ok || returned < 8 {
            return None;
        }
        parse_usn_buffer(&self.lookup_buf[..returned as usize])
            .into_iter()
            .find(|r| r.frn == frn)
            .map(|r| (r.parent_frn, r.name, r.is_dir))
    }

    /// Query the USN change journal: returns `(journal_id, next_usn)`.
    /// `FSCTL_READ_USN_JOURNAL` requires the real journal ID — passing 0
    /// fails with ERROR_INVALID_PARAMETER (87).
    pub fn query_journal(&self) -> Result<(u64, i64)> {
        let mut jd: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        let ok = self.ioctl(
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut jd as *mut _ as *mut u8,
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut returned,
        );
        if !ok {
            bail!(
                "FSCTL_QUERY_USN_JOURNAL failed on {}: (error {})",
                self.drive,
                unsafe { GetLastError() }
            );
        }
        Ok((jd.UsnJournalID, jd.NextUsn))
    }

    /// Non-blocking poll of the USN journal for changes since `start_usn`.
    /// Returns the next USN to poll from and the collected records.
    pub fn read_journal(&self, start_usn: i64, mask: u32) -> Result<(i64, Vec<UsnRecord>)> {
        let (journal_id, _next) = self.query_journal()?;
        let mut rjd = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start_usn,
            ReasonMask: mask,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut all = Vec::new();
        let mut current = start_usn;
        loop {
            let mut returned = 0u32;
            let ok = self.ioctl(
                FSCTL_READ_USN_JOURNAL,
                &mut rjd as *mut _ as *const c_void,
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut returned,
            );
            if !ok {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    break;
                }
                bail!("FSCTL_READ_USN_JOURNAL failed on {}: (error {err})", self.drive);
            }
            if returned < 8 {
                break;
            }
            let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
            let parsed = parse_usn_buffer(&buf[..returned as usize]);
            let got = !parsed.is_empty();
            all.extend(parsed);
            current = next;
            if !got {
                break; // header-only: nothing new right now
            }
            if (returned as usize) < buf.len() {
                break; // drained everything currently available
            }
        }
        Ok((current, all))
    }
}

impl Drop for UsnVolume {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// Parse a raw USN/MFT buffer. The first 8 bytes are the "next FRN/USN" header;
/// records start at offset 8.
///
/// Two layouts are supported (both share RecordLength/MajorVersion at 0/4):
/// * V2 — 64-bit file reference numbers, name at offset 60
/// * V3 — `FILE_ID_128` reference numbers (16 bytes each, use the low 64 bits
///   as the FRN), name at offset 76
pub fn parse_usn_buffer(buf: &[u8]) -> Vec<UsnRecord> {
    let mut out = Vec::new();
    let mut off = 8usize;
    while off + 8 <= buf.len() {
        let rec_len =
            u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        if rec_len < 8 || off + rec_len > buf.len() {
            break;
        }
        let major = u16::from_le_bytes([buf[off + 4], buf[off + 5]]);
        let (frn, parent_frn, usn, reason, attrs, name_len, name_off) = if major == 2 {
            (
                u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()) & FRN_MASK,
                u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap()) & FRN_MASK,
                i64::from_le_bytes(buf[off + 24..off + 32].try_into().unwrap()),
                u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap()),
                u32::from_le_bytes(buf[off + 52..off + 56].try_into().unwrap()),
                u16::from_le_bytes([buf[off + 56], buf[off + 57]]) as usize,
                u16::from_le_bytes([buf[off + 58], buf[off + 59]]) as usize,
            )
        } else if major == 3 {
            (
                u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()) & FRN_MASK,
                u64::from_le_bytes(buf[off + 24..off + 32].try_into().unwrap()) & FRN_MASK,
                i64::from_le_bytes(buf[off + 40..off + 48].try_into().unwrap()),
                u32::from_le_bytes(buf[off + 56..off + 60].try_into().unwrap()),
                u32::from_le_bytes(buf[off + 68..off + 72].try_into().unwrap()),
                u16::from_le_bytes([buf[off + 72], buf[off + 73]]) as usize,
                u16::from_le_bytes([buf[off + 74], buf[off + 75]]) as usize,
            )
        } else {
            off += rec_len;
            continue;
        };
        let mut name = String::new();
        if name_len > 0 && name_off < rec_len {
            let start = off + name_off;
            let end = (start + name_len).min(off + rec_len);
            if end >= start + 2 {
                let mut utf16 = Vec::with_capacity((end - start) / 2);
                for i in (start..end).step_by(2) {
                    utf16.push(u16::from_le_bytes([buf[i], buf[i + 1]]));
                }
                name = String::from_utf16_lossy(&utf16);
            }
        }
        out.push(UsnRecord {
            frn,
            parent_frn,
            name,
            is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            usn,
            reason,
        });
        off += rec_len;
    }
    out
}

/// Streaming, compact MFT tree builder.
///
/// Names live in one arena buffer and records are flat parallel arrays
/// (~24 bytes/record), so a 2.8M-record volume costs ~220 MB instead of the
/// gigabytes a `Vec<String>` + `HashMap` layout would. Paths are materialized
/// into a single reusable buffer during a DFS and handed to `on_entry`
/// immediately (the store binds them into SQLite without keeping them).
pub struct TreeBuilder {
    drive: char,
    frn: Vec<u64>,
    parent: Vec<u64>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    meta: Vec<EntryMeta>,
    names: Vec<u8>,
}

impl TreeBuilder {
    pub fn new(drive: char) -> Self {
        TreeBuilder {
            drive,
            frn: Vec::new(),
            parent: Vec::new(),
            name_off: Vec::new(),
            name_len: Vec::new(),
            meta: Vec::new(),
            names: Vec::new(),
        }
    }

    pub fn push(&mut self, frn: u64, parent_frn: u64, name: &str, meta: EntryMeta) {
        self.frn.push(frn);
        self.parent.push(parent_frn);
        self.name_off.push(self.names.len() as u32);
        self.name_len.push(name.len() as u16);
        self.names.extend_from_slice(name.as_bytes());
        self.meta.push(meta);
    }

    fn name(&self, i: usize) -> &str {
        let off = self.name_off[i] as usize;
        let end = off + self.name_len[i] as usize;
        std::str::from_utf8(&self.names[off..end]).unwrap_or("")
    }

    fn name_bytes(&self, i: usize) -> &[u8] {
        let off = self.name_off[i] as usize;
        let end = off + self.name_len[i] as usize;
        &self.names[off..end]
    }

    /// DFS from the volume root, emitting `on_entry(full_path, meta)` for
    /// every reachable record. Returns the number of unreachable records
    /// (the volume root record itself plus orphans).
    pub fn build(self, mut on_entry: impl FnMut(&str, EntryMeta)) -> u64 {
        let total = self.frn.len() as u64;
        // Permutation sorted by parent FRN so children of X are a contiguous
        // range found by two binary searches — no children HashMap needed.
        // Children additionally sorted by name: the DFS then emits paths in
        // near-lexicographic order, which keeps the path UNIQUE/`path_l`
        // b-tree inserts (and the index rebuilds) sequential-ish instead of
        // random within every directory.
        let mut order: Vec<u32> = (0..self.frn.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            self.parent[a]
                .cmp(&self.parent[b])
                .then_with(|| self.name_bytes(a).cmp(self.name_bytes(b)))
        });

        // Reusable path buffer: "D:" + pushed segments. Each recursion frame
        // truncates only its own suffix, so the prefix bytes are never touched
        // and truncation is always at a char boundary.
        let mut path = format!("{}:", self.drive);
        let mut emitted = 0u64;
        visit(&self, &order, ROOT_FRN, &mut path, &mut emitted, &mut on_entry);
        total.saturating_sub(emitted)
    }
}

/// Recursive DFS visitor over the parent-sorted permutation.
fn visit(
    tb: &TreeBuilder,
    order: &[u32],
    frn: u64,
    path: &mut String,
    emitted: &mut u64,
    on_entry: &mut impl FnMut(&str, EntryMeta),
) {
    let lo = order.partition_point(|&i| tb.parent[i as usize] < frn);
    let hi = order.partition_point(|&i| tb.parent[i as usize] <= frn);
    for &i in &order[lo..hi] {
        let i = i as usize;
        if tb.frn[i] == ROOT_FRN {
            continue; // the root record itself (empty name)
        }
        let base = path.len();
        path.push('\\');
        path.push_str(tb.name(i));
        if tb.meta[i].is_dir {
            visit(tb, order, tb.frn[i], path, emitted, on_entry);
        }
        on_entry(path, tb.meta[i]);
        *emitted += 1;
        path.truncate(base);
    }
}

/// Resolve an FRN to a full path by walking parent records up to the root.
pub fn resolve_path(
    vol: &mut UsnVolume,
    drive: char,
    frn: u64,
    cache: &mut HashMap<u64, Option<String>>,
) -> Option<String> {
    if frn == ROOT_FRN {
        return Some(String::new());
    }
    if let Some(cached) = cache.get(&frn) {
        return cached.clone();
    }
    let (parent, name, _is_dir) = vol.lookup(frn)?;
    let parent_path = resolve_path(vol, drive, parent, cache)?;
    let full = if parent_path.is_empty() {
        format!("{drive}:\\{name}")
    } else {
        format!("{parent_path}\\{name}")
    };
    cache.insert(frn, Some(full.clone()));
    Some(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn put_i64(buf: &mut [u8], off: usize, v: i64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Build a synthetic USN record buffer. `v3` selects the 16-byte
    /// FILE_ID_128 layout; otherwise the 8-byte V2 layout is used.
    fn make_record(name: &str, frn: u64, parent: u64, attrs: u32, v3: bool) -> Vec<u8> {
        let name_bytes: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let name_off = if v3 { 76 } else { 60 };
        let rec_len = (name_off + name_bytes.len()).div_ceil(8) * 8;
        let mut buf = vec![0u8; rec_len];
        put_u32(&mut buf, 0, rec_len as u32);
        put_u16(&mut buf, 4, if v3 { 3 } else { 2 });
        put_u64(&mut buf, 8, frn);
        if v3 {
            put_u64(&mut buf, 16, 0); // high 64 bits of 128-bit id
            put_u64(&mut buf, 24, parent);
            put_u64(&mut buf, 32, 0); // high 64 bits of parent id
            put_i64(&mut buf, 40, frn as i64); // usn
            put_u32(&mut buf, 56, 0); // reason
            put_u32(&mut buf, 68, attrs);
        } else {
            put_u64(&mut buf, 16, parent);
            put_i64(&mut buf, 24, frn as i64); // usn
            put_u32(&mut buf, 40, 0); // reason
            put_u32(&mut buf, 52, attrs);
        }
        match v3 {
            true => {
                put_u16(&mut buf, 72, name_bytes.len() as u16);
                put_u16(&mut buf, 74, 76);
            }
            false => {
                put_u16(&mut buf, 56, name_bytes.len() as u16);
                put_u16(&mut buf, 58, 60);
            }
        }
        buf[name_off..name_off + name_bytes.len()].copy_from_slice(&name_bytes);
        buf
    }

    #[test]
    fn parse_v2_buffer() {
        let mut buf = vec![0u8; 8]; // next-FRN header
        let r1 = make_record("README.md", 42, 5, 0, false);
        buf.extend_from_slice(&r1);
        let parsed = parse_usn_buffer(&buf);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "README.md");
        assert_eq!(parsed[0].frn, 42);
        assert_eq!(parsed[0].parent_frn, 5);
        assert!(!parsed[0].is_dir);
    }

    #[test]
    fn parse_v3_buffer() {
        let mut buf = vec![0u8; 8]; // next-FRN header
        let r1 = make_record("README.md", 42, 5, 0, true);
        let r2 = make_record("目录", 43, 5, FILE_ATTRIBUTE_DIRECTORY, true);
        buf.extend_from_slice(&r1);
        buf.extend_from_slice(&r2);
        let parsed = parse_usn_buffer(&buf);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "README.md");
        assert_eq!(parsed[0].frn, 42);
        assert_eq!(parsed[0].parent_frn, 5);
        assert!(!parsed[0].is_dir);
        assert_eq!(parsed[1].name, "目录");
        assert!(parsed[1].is_dir);
    }

    #[test]
    fn tree_builder_reconstructs_paths() {
        let mut tb = TreeBuilder::new('D');
        let recs = [
            (5u64, 5u64, "", true),
            (10, 5, "dev", true),
            (11, 10, "main.rs", false),
            (12, 10, "lib.rs", false),
            (99, 666, "orphan.bin", false),
        ];
        for (frn, parent, name, is_dir) in recs {
            tb.push(frn, parent, name, EntryMeta { is_dir, frn: Some(frn), ..Default::default() });
        }
        let mut out: Vec<(String, EntryMeta)> = Vec::new();
        let skipped = tb.build(|p, m| out.push((p.to_string(), m)));
        assert_eq!(out.len(), 3);
        assert_eq!(skipped, 2); // root record + orphan
        let mut got: Vec<&str> = out.iter().map(|(p, _)| p.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["D:\\dev", "D:\\dev\\lib.rs", "D:\\dev\\main.rs"]);
        let main = out.iter().find(|(p, _)| p == "D:\\dev\\main.rs").unwrap();
        assert_eq!(main.1.frn, Some(11));
    }
}
