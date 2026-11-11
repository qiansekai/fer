//! Who holds a path open: the handle table **plus** the working directories.
//!
//! Two independent mechanisms stop a rename or a delete, and only one of them
//! appears in the handle table:
//!
//! 1. An **open handle**. NtQuerySystemInformation(SystemExtendedHandleInformation)
//!    lists every handle in the system; filter by the File object type index and
//!    duplicate the survivors into this process to read their paths.
//! 2. A **working directory inside the path**. This is not a handle at all. The
//!    most common cause of "the directory will not rename while every
//!    subdirectory inside it renames fine" is a GUI (Photos, Paint, a shell)
//!    launched with its cwd there, and it is invisible to every handle-only
//!    tool. Reading it out of the target PEB is the only way to see it.
//!
//! Load-bearing design notes:
//!
//! * NtQueryObject is deliberately NOT used. It answers handle-name queries by
//!   deadlocking on synchronous objects (pipes), whose usual workaround is a
//!   watchdog thread. GetFinalPathNameByHandleW returns exactly the file path
//!   this module needs, safely, for the only handle type it looks at.
//! * The File object **type index is probed at runtime** (see
//!   probe_file_type_index), never assumed: it differs across Windows builds,
//!   and a stale constant makes the whole scan report "nothing is holding this
//!   path" - the exact failure this module exists to avoid.
//! * The --probe switch (off by default) asks the target for DELETE access and
//!   reports the Win32 error. It never deletes, renames, or writes anything.
//! * Handle paths compare case-insensitively on **component boundaries**, so
//!   D:\proj does not match D:\proj2.
//! * When the target exists, hits are also matched by NTFS file record number,
//!   so a handle opened through a hard-link alias is attributed to the target
//!   even though the paths differ.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;

use anyhow::{Context, Result};
use serde::Serialize;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, ERROR_SHARING_VIOLATION, GetLastError, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    GetFileInformationByHandle, GetFileType, GetFinalPathNameByHandleW, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcessModules, EnumProcesses, GetModuleFileNameExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW,
};

use crate::{FRN_MASK, basename, fold_lower};

// ---------------------------------------------------------------------------
// Un-symbolised ntdll entry points.
//
// Declared here rather than pulled in through a Wdk feature: fer already
// declares the few Win32 exports it needs this way (see fer-gui: ShellExecuteW
// and GlobalFree), and the Wdk module paths have moved between windows-sys
// releases, so a feature there would break the build on every crate bump.
// ---------------------------------------------------------------------------
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

/// SystemExtendedHandleInformation - the 64-bit-safe handle table class.
const SYSTEM_EXTENDED_HANDLE_INFORMATION: u32 = 0x40;
/// ProcessBasicInformation - yields the target PEB address.
const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

const PROCESS_DUP_HANDLE: u32 = 0x0040;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_VM_READ: u32 = 0x0010;
const DUPLICATE_SAME_ACCESS: u32 = 0x0002;
/// DELETE is the access right a rename/delete needs; a handle held without
/// FILE_SHARE_DELETE is what actually blocks the operation.
const DELETE_ACCESS: u32 = 0x0001_0000;

/// Offsets into the target address space: PEB.ProcessParameters at +0x20 and
/// RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath at +0x38. These are the
/// stable x64 offsets and are not exported symbols.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
const PARAMS_CURRENT_DIRECTORY: usize = 0x38;

/// Byte count of the extended handle-information header (two ULONG_PTRs).
const HANDLE_TABLE_HEADER: usize = 16;

/// One row of the system handle table, as the kernel lays it out.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SystemHandleEntry {
    object: *mut c_void,
    unique_process_id: usize,
    handle_value: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

/// PROCESS_BASIC_INFORMATION, x64 layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ProcessBasicInformation {
    exit_status: i32,
    _pad0: u32,
    peb_base_address: *mut c_void,
    affinity_mask: usize,
    base_priority: i32,
    _pad1: u32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

/// Which halves of the scan to run.
#[derive(Debug, Clone, Copy, Default)]
pub struct LockOptions {
    /// Ask the target for DELETE access and report the Win32 error. Off by
    /// default: it is a real open (still zero mutation) and the caller opts in.
    pub probe: bool,
    /// Skip the handle-table half.
    pub cwd_only: bool,
    /// Skip the working-directory half.
    pub no_cwd: bool,
    /// Also walk every process's full module list, so a loaded .dll that locks
    /// the target is reported. Off by default: EnumProcessModules plus one
    /// GetModuleFileNameExW per module costs ~0.1 ms per module system-wide
    /// (measured ~1.4 s here) and the common case — "the program is still
    /// running, so its .exe will not delete" — is already covered by the
    /// per-process image path, which costs one call per process.
    pub modules: bool,
}

/// One process standing in the way, and by which mechanism.
#[derive(Debug, Clone, Serialize)]
pub struct Holder {
    pub pid: u32,
    /// Executable base name when readable, else "<unknown>".
    pub process: String,
    /// "handle" or "cwd".
    pub kind: String,
    /// The path the process has open (handle) or its working directory (cwd),
    /// normalised: no long-path prefix, no trailing separator.
    pub path: String,
    /// Granted access mask (handle holders only; 0 for cwd holders).
    pub access: u32,
    /// Whether the mask carries access rights that typically block a
    /// rename/delete. This is an indicator, not a verdict: the real blocker is
    /// an open handle that omits FILE_SHARE_DELETE, and the handle table does
    /// not report share modes.
    pub hard: bool,
    /// True when the holder file record number equals the target, i.e. the
    /// handle was opened through a hard-link alias and the paths differ.
    pub hard_link_alias: bool,
    /// True when the holder is this very process (querying an executable always
    /// finds the fer that is running the query).
    pub is_self: bool,
}

/// One executable listening on the requested TCP port.
#[derive(Debug, Clone, Serialize)]
pub struct PortOwner {
    pub pid: u32,
    pub process: String,
    pub local_addr: String,
    pub family: String,
}

/// Outcome of the opt-in DELETE-access probe.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    /// would_succeed | sharing_violation | access_denied | not_found | error
    pub verdict: String,
    pub win32_error: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockReport {
    /// The path as asked for, normalised to an absolute form.
    pub target: String,
    /// file | dir | missing
    pub kind: String,
    /// NTFS file record number (sequence included) when the target exists.
    pub frn: Option<u64>,
    pub probe: Option<ProbeResult>,
    pub holders: Vec<Holder>,
    pub handles_scanned: u64,
    pub processes_seen: u64,
    /// Processes whose handle table or PEB could not be opened (protected
    /// processes; an un-elevated caller sees more of these).
    pub processes_unreadable: u64,
    pub elevated: bool,
    pub elapsed_ms: u128,
}

/// The system handle table, captured once and reduced to the fields the scan
/// needs (a full parse costs ~6 MB of buffer for ~160k rows).
pub struct HandleSnapshot {
    /// (pid, raw handle, granted access, object type index)
    entries: Vec<(u32, usize, u32, u16)>,
    file_type_index: Option<u16>,
}

enum HandleQueryError {
    NeedMore(usize),
    Status(i32),
}

impl HandleSnapshot {
    /// Capture the handle table. The File object type index is measured in the
    /// same snapshot: a file handle this process opens is guaranteed to appear
    /// in it, so finding that one row names the type.
    pub fn capture() -> Result<Self> {
        // Open the marker FIRST, before the snapshot. The handle has to exist in
        // the table already, otherwise the probe finds nothing and the caller
        // falls back to scanning every object type in the system - which is how
        // the first version ended up duplicating 164k handles, half of them
        // pipes that block when asked for a path.
        let marker = std::env::current_exe()
            .ok()
            .and_then(|exe| std::fs::File::open(exe).ok());
        let marker_handle = marker.as_ref().map(|f| f.as_raw_handle() as usize);
        let me = std::process::id();

        let mut size = 1usize << 20;
        let raw = loop {
            match query_system_handles(size) {
                Ok(raw) => break raw,
                Err(HandleQueryError::NeedMore(want)) if want > size => size = want,
                Err(HandleQueryError::NeedMore(_)) => {
                    anyhow::bail!("the handle table did not grow; refusing to spin")
                }
                Err(HandleQueryError::Status(status)) => anyhow::bail!(
                    "NtQuerySystemInformation(SystemExtendedHandleInformation) failed with \
                     NTSTATUS {status:#010X}"
                ),
            }
        };

        if raw.len() < HANDLE_TABLE_HEADER {
            anyhow::bail!("handle table shorter than its header ({} bytes)", raw.len());
        }
        let count = u64::from_ne_bytes(raw[0..8].try_into().unwrap()) as usize;
        let stride = size_of::<SystemHandleEntry>();
        let needed = HANDLE_TABLE_HEADER + count.saturating_mul(stride);
        if raw.len() < needed {
            anyhow::bail!(
                "handle table truncated: {count} rows need {needed} bytes, got {}",
                raw.len()
            );
        }

        let mut entries = Vec::with_capacity(count);
        let mut file_type_index = None;
        for i in 0..count {
            let offset = HANDLE_TABLE_HEADER + i * stride;
            // SAFETY: offset + stride was bounds-checked against raw.len()
            // above, and SystemHandleEntry is a plain repr(C) POD read
            // unaligned (the kernel packs rows back to back).
            let row = unsafe {
                std::ptr::read_unaligned(raw.as_ptr().add(offset) as *const SystemHandleEntry)
            };
            let pid = row.unique_process_id as u32;
            if let Some(handle) = marker_handle
                && pid == me
                && row.handle_value == handle
            {
                file_type_index = Some(row.object_type_index);
            }
            entries.push((pid, row.handle_value, row.granted_access, row.object_type_index));
        }

        Ok(Self {
            entries,
            file_type_index,
        })
    }

    /// The measured index of the File object type. None means the marker
    /// executable could not be opened, so the caller must treat every type as a
    /// candidate rather than silently scanning nothing.
    pub fn probe_file_type_index(&self) -> Option<u16> {
        self.file_type_index
    }
}

fn query_system_handles(size: usize) -> std::result::Result<Vec<u8>, HandleQueryError> {
    let mut buf = vec![0u8; size];
    let mut needed = 0u32;
    // SAFETY: buf is size bytes long and size is passed verbatim; the
    // out-parameter is a valid u32.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_EXTENDED_HANDLE_INFORMATION,
            buf.as_mut_ptr() as *mut c_void,
            size as u32,
            &mut needed,
        )
    };
    if status == STATUS_INFO_LENGTH_MISMATCH {
        return Err(HandleQueryError::NeedMore(needed as usize + (64 << 10)));
    }
    if status != 0 {
        return Err(HandleQueryError::Status(status));
    }
    Ok(buf)
}

/// A Win32 handle closed on drop.
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(raw: HANDLE) -> Option<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self(raw))
        }
    }
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open_process(pid: u32, access: u32) -> Option<OwnedHandle> {
    if pid == 0 || pid == 4 {
        // The idle and System pseudo-processes have no handle table to walk.
        return None;
    }
    // SAFETY: OpenProcess takes no buffer; a null return is the documented
    // failure signal and is filtered by OwnedHandle::new.
    OwnedHandle::new(unsafe { OpenProcess(access, 0, pid) })
}

/// Full path of the executable backing a process.
fn process_image(process: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    // SAFETY: buf is size u16s and size describes it exactly.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut size) };
    if ok == 0 || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

fn process_name(process: HANDLE) -> Option<String> {
    process_image(process).map(|full| basename(&full).to_string())
}

/// Full paths of the modules loaded into a process (the main executable first).
///
/// A loaded image locks its file without holding a handle: the kernel keeps a
/// Section and the file handle is closed at load time. That is why every
/// handle-only tool reports "no matching handles" for a running .exe, and why
/// this pass exists.
fn process_modules(process: HANDLE) -> Vec<String> {
    const MAX_MODULES: usize = 1024;
    let mut modules: Vec<HMODULE> = vec![null_mut(); MAX_MODULES];
    let mut needed = 0u32;
    let bytes = (modules.len() * size_of::<HMODULE>()) as u32;
    // SAFETY: modules holds exactly bytes bytes and bytes says so.
    let ok = unsafe { EnumProcessModules(process, modules.as_mut_ptr(), bytes, &mut needed) };
    if ok == 0 {
        return Vec::new();
    }
    let count = (needed as usize / size_of::<HMODULE>()).min(modules.len());
    let mut out = Vec::with_capacity(count);
    for module in &modules[..count] {
        let mut buf = [0u16; 512];
        // SAFETY: buf is buf.len() u16s; the call is bounded by that length.
        let len =
            unsafe { GetModuleFileNameExW(process, *module, buf.as_mut_ptr(), buf.len() as u32) };
        if len > 0 && (len as usize) < buf.len() {
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
    }
    out
}

/// Whether two paths name the same object, compared case-insensitively after
/// normalising both.
fn path_eq(a: &str, b: &str) -> bool {
    fold_lower(&normalize_nt_path(a)) == fold_lower(&normalize_nt_path(b))
}

/// NTFS file record number of an open handle (the full 64-bit nFileIndex, which
/// carries the sequence number in its high 16 bits).
fn handle_file_index(handle: HANDLE) -> Option<u64> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: info is the exact struct the call fills in.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    // Reading two already-filled fields, so eager evaluation is harmless.
    (ok != 0).then_some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}

/// File record number of a path, opened briefly with FILE_READ_ATTRIBUTES.
///
/// std's MetadataExt::file_index would say the same thing, but it is behind the
/// unstable windows_by_handle feature on stable Rust. Failure (a file held
/// without share-read, a protected path) is not fatal: it only downgrades
/// hard-link alias detection for that one run.
fn path_file_index(target: &str, is_dir: bool) -> Option<u64> {
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let flags = if is_dir { FILE_FLAG_BACKUP_SEMANTICS } else { 0 };
    // SAFETY: wide is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    let owned = OwnedHandle::new(handle)?;
    handle_file_index(owned.raw())
}

/// Duplicate one handle and read both facts the scan needs from the copy: its
/// path and its file record number.
fn duplicate_and_inspect(process: HANDLE, raw_handle: usize) -> Option<(String, Option<u64>)> {
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle that must not be
    // closed; it is only ever passed as a target here.
    let self_process = unsafe { GetCurrentProcess() };
    let mut duplicated: HANDLE = null_mut();
    // SAFETY: process carries PROCESS_DUP_HANDLE; the out handle starts null and
    // is only trusted after a non-zero return.
    let ok = unsafe {
        DuplicateHandle(
            process,
            raw_handle as HANDLE,
            self_process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return None;
    }
    let owned = OwnedHandle::new(duplicated)?;
    // Named pipes and character devices are File-typed handles too, and asking
    // one of those for a path can sit there waiting for a peer that never
    // answers. Only disks carry a path this module can use.
    //
    // SAFETY: the duplicated handle is open and this call takes no buffer.
    if unsafe { GetFileType(owned.raw()) } != FILE_TYPE_DISK {
        return None;
    }
    let index = handle_file_index(owned.raw());
    let mut buf = [0u16; 1024];
    // SAFETY: buf holds buf.len() u16s; the call is bounded by that length.
    let len = unsafe { GetFinalPathNameByHandleW(owned.raw(), buf.as_mut_ptr(), buf.len() as u32, 0) };
    if len == 0 || len as usize >= buf.len() {
        // The path is unreadable, but the record number still identifies a
        // hard-link alias, so do not throw that away.
        return index.map(|idx| (String::new(), Some(idx)));
    }
    let path = normalize_nt_path(&String::from_utf16_lossy(&buf[..len as usize]));
    Some((path, index))
}

/// Drop the long-path prefix so caller-supplied paths compare equal to handle
/// paths.
fn normalize_nt_path(raw: &str) -> String {
    // The verbatim prefixes are built from the platform separator so this file
    // never needs an escaped backslash literal in a doc comment.
    let sep = std::path::MAIN_SEPARATOR;
    let verbatim = [sep, sep, '?', sep].iter().collect::<String>();
    let verbatim_unc = [sep, sep, '?', sep, 'U', 'N', 'C', sep]
        .iter()
        .collect::<String>();
    let stripped = if let Some(rest) = raw.strip_prefix(&verbatim_unc) {
        format!("{sep}{sep}{rest}")
    } else if let Some(rest) = raw.strip_prefix(&verbatim) {
        rest.to_string()
    } else {
        raw.to_string()
    };
    strip_trailing_sep(&stripped)
}

/// Trim trailing separators, keeping a bare volume root (D:\) intact.
fn strip_trailing_sep(path: &str) -> String {
    let mut out = path;
    while out.len() > 3 && (out.ends_with('\\') || out.ends_with('/')) {
        out = &out[..out.len() - 1];
    }
    out.to_string()
}

/// Read one fixed-size integer out of another process.
fn read_process_memory<T: Copy>(process: HANDLE, address: usize) -> Option<usize> {
    let mut value: T = unsafe { std::mem::zeroed() };
    let mut read = 0usize;
    // SAFETY: value is size_of::<T>() bytes and lives across the call.
    let ok = unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            &mut value as *mut T as *mut c_void,
            size_of::<T>(),
            &mut read,
        )
    };
    if ok == 0 || read != size_of::<T>() {
        return None;
    }
    // Widen through a byte copy: T may be u16 or usize.
    let mut widened = 0usize;
    let count = size_of::<T>().min(size_of::<usize>());
    // SAFETY: source has size_of::<T>() bytes, destination size_of::<usize>(),
    // and count is the smaller of the two.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &value as *const T as *const u8,
            &mut widened as *mut usize as *mut u8,
            count,
        )
    };
    Some(widened)
}

/// Read a process current working directory out of its PEB.
///
/// None means the read failed (protected process, or a 32-bit process whose PEB
/// layout differs) - never "the process has no working directory".
fn read_cwd(process: HANDLE) -> Option<String> {
    let mut pbi: ProcessBasicInformation = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    // SAFETY: pbi is the exact struct ProcessBasicInformation fills.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            PROCESS_BASIC_INFORMATION_CLASS,
            &mut pbi as *mut ProcessBasicInformation as *mut c_void,
            size_of::<ProcessBasicInformation>() as u32,
            &mut ret,
        )
    };
    if status != 0 || pbi.peb_base_address.is_null() {
        return None;
    }
    let params = read_process_memory::<usize>(
        process,
        pbi.peb_base_address as usize + PEB_PROCESS_PARAMETERS,
    )?;
    if params == 0 {
        return None;
    }
    // UNICODE_STRING { Length: u16, MaximumLength: u16, padding, Buffer: ptr }
    let length = read_process_memory::<u16>(process, params + PARAMS_CURRENT_DIRECTORY)?;
    let buffer = read_process_memory::<usize>(process, params + PARAMS_CURRENT_DIRECTORY + 8)?;
    if length == 0 || buffer == 0 || length > 32 * 1024 {
        return None;
    }
    let mut chars = vec![0u16; length as usize / 2];
    let mut read = 0usize;
    // SAFETY: chars holds exactly length bytes and process carries VM_READ.
    let ok = unsafe {
        ReadProcessMemory(
            process,
            buffer as *const c_void,
            chars.as_mut_ptr() as *mut c_void,
            length as usize,
            &mut read,
        )
    };
    if ok == 0 || read != length as usize {
        return None;
    }
    Some(strip_trailing_sep(&String::from_utf16_lossy(&chars)))
}

/// Whether candidate is target itself or lives inside it, compared
/// case-insensitively on path-component boundaries.
fn path_within(candidate: &str, target: &str) -> bool {
    let candidate = fold_lower(&strip_trailing_sep(candidate));
    let target = fold_lower(&strip_trailing_sep(target));
    if candidate == target {
        return true;
    }
    candidate.len() > target.len()
        && candidate.starts_with(&target)
        && matches!(candidate.as_bytes()[target.len()], b'\\' | b'/')
}

/// Access bits that typically mean the holder is doing something the target
/// cannot be renamed or deleted underneath.
fn access_is_hard(access: u32) -> bool {
    const FILE_WRITE_DATA: u32 = 0x0000_0002;
    const FILE_APPEND_DATA: u32 = 0x0000_0004;
    const FILE_WRITE_EA: u32 = 0x0000_0010;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    access
        & (FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | FILE_WRITE_EA
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | GENERIC_WRITE
            | GENERIC_ALL)
        != 0
}

/// Normalise a user-supplied path to an absolute one so it compares against
/// handle paths (which are always absolute).
pub fn absolute_target(target: &str) -> Result<String> {
    let path = std::path::Path::new(target);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading the current directory")?
            .join(path)
    };
    Ok(strip_trailing_sep(&absolute.to_string_lossy()))
}

/// Inspect who holds target open.
pub fn inspect(target: &str, opts: LockOptions) -> Result<LockReport> {
    let started = std::time::Instant::now();
    let target = absolute_target(target)?;
    let metadata = std::fs::metadata(&target).ok();
    let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let kind = match &metadata {
        Some(m) if m.is_dir() => "dir",
        Some(_) => "file",
        None => "missing",
    };
    let target_frn = path_file_index(&target, is_dir);
    let elevated = crate::is_elevated();
    let me = std::process::id();

    let mut holders: Vec<Holder> = Vec::new();
    let mut handles_scanned = 0u64;
    let mut processes_seen = 0u64;
    let mut processes_unreadable = 0u64;

    if !opts.cwd_only {
        match HandleSnapshot::capture() {
            Ok(snapshot) => {
                let file_type = snapshot.probe_file_type_index();
                // Group by pid so each process handle is opened once.
                let mut per_pid: HashMap<u32, Vec<(usize, u32)>> = HashMap::new();
                for &(pid, handle, access, type_index) in &snapshot.entries {
                    if let Some(want) = file_type
                        && type_index != want
                    {
                        continue;
                    }
                    per_pid.entry(pid).or_default().push((handle, access));
                }
                processes_seen = per_pid.len() as u64;
                for (pid, rows) in per_pid {
                    // PROCESS_DUP_HANDLE is the one right that cannot be
                    // substituted: without it the rows cannot be resolved, so
                    // count the process as unreadable rather than report it as
                    // holding nothing.
                    let Some(process) =
                        open_process(pid, PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION)
                    else {
                        processes_unreadable += 1;
                        continue;
                    };
                    let name = process_name(process.raw()).unwrap_or_else(|| "<unknown>".into());
                    for (raw_handle, access) in rows {
                        handles_scanned += 1;
                        let Some((path, index)) = duplicate_and_inspect(process.raw(), raw_handle)
                        else {
                            continue;
                        };
                        let same_record = match (&target_frn, index) {
                            (Some(want), Some(got)) => *want == got,
                            _ => false,
                        };
                        if path.is_empty() && !same_record {
                            continue;
                        }
                        let by_path = !path.is_empty() && path_within(&path, &target);
                        if !by_path && !same_record {
                            continue;
                        }
                        holders.push(Holder {
                            pid,
                            process: name.clone(),
                            kind: "handle".into(),
                            path,
                            access,
                            hard: access_is_hard(access),
                            hard_link_alias: same_record && !by_path,
                            is_self: pid == me,
                        });
                    }
                }
            }
            Err(err) => {
                if opts.no_cwd {
                    return Err(err);
                }
                // A cwd-only answer is still useful, so surface the loss on
                // stderr rather than failing the whole command.
                eprintln!("[lock] handle table unavailable ({err}); working directories only");
            }
        }
    }

    // The working-directory and loaded-image halves share one pass over the
    // processes. Both are invisible to the handle table:
    //
    // * a cwd inside the target locks the directory, and only from the target
    //   downwards (Windows permits renaming a child of a process cwd);
    // * a loaded image locks its own file through a Section, not a handle.
    if !opts.no_cwd || kind == "file" {
        for pid in all_process_ids() {
            let Some(process) =
                open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
            else {
                processes_unreadable += 1;
                continue;
            };
            if !opts.no_cwd
                && is_dir
                && let Some(cwd) = read_cwd(process.raw())
                && path_within(&cwd, &target)
            {
                holders.push(Holder {
                    pid,
                    process: process_name(process.raw()).unwrap_or_else(|| "<unknown>".into()),
                    kind: "cwd".into(),
                    path: cwd,
                    access: 0,
                    hard: true,
                    hard_link_alias: false,
                    is_self: pid == me,
                });
            }
            if kind != "file" {
                continue;
            }
            // The image path is one call per process and covers "the program I
            // am trying to delete is still running". Loaded dlls need the full
            // module list, which costs an order of magnitude more — opt in.
            let image = process_image(process.raw());
            let image_hit = image
                .as_deref()
                .filter(|path| path_eq(path, &target))
                .map(|path| ("image", path.to_string()));
            let module_hit = if image_hit.is_none() && opts.modules {
                process_modules(process.raw())
                    .into_iter()
                    .find(|module| path_eq(module, &target))
                    .map(|module| ("module", module))
            } else {
                None
            };
            if let Some((kind_name, path)) = image_hit.or(module_hit) {
                holders.push(Holder {
                    pid,
                    process: process_name(process.raw()).unwrap_or_else(|| "<unknown>".into()),
                    kind: kind_name.into(),
                    path,
                    access: 0,
                    hard: true,
                    hard_link_alias: false,
                    is_self: pid == me,
                });
            }
        }
    }

    holders.sort_by(|a, b| a.pid.cmp(&b.pid).then_with(|| a.kind.cmp(&b.kind)));

    // Deliberately an if/else rather than .then(|| ...): the probe opens a real
    // handle, so it must only run when asked for. Clippy's then_some suggestion
    // would evaluate it every time.
    let probe = if opts.probe {
        Some(probe_delete_access(&target, is_dir))
    } else {
        None
    };

    Ok(LockReport {
        target,
        kind: kind.into(),
        frn: target_frn,
        probe,
        holders,
        handles_scanned,
        processes_seen,
        processes_unreadable,
        elevated,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Ask the target for DELETE access and classify the outcome.
///
/// DELETE is the access right a rename/delete actually requires, so an
/// ERROR_SHARING_VIOLATION here pinpoints an open handle that omitted
/// FILE_SHARE_DELETE - the concrete blocker, named by the holder list. Nothing
/// is deleted, renamed, or written: the right is requested on the existing
/// object and the handle is closed again.
fn probe_delete_access(target: &str, is_dir: bool) -> ProbeResult {
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    // Directories can only be opened through CreateFileW with this flag.
    let flags = if is_dir { FILE_FLAG_BACKUP_SEMANTICS } else { 0 };
    // SAFETY: wide is NUL-terminated and outlives the call; the remaining
    // arguments are the documented constants for opening an existing object.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        // SAFETY: opened here, closed exactly once, never used again.
        unsafe { CloseHandle(handle) };
        return ProbeResult {
            verdict: "would_succeed".into(),
            win32_error: 0,
        };
    }
    // SAFETY: GetLastError reads thread-local state and takes no arguments.
    let err = unsafe { GetLastError() };
    let verdict = match err {
        ERROR_SHARING_VIOLATION => "sharing_violation",
        2 | 3 => "not_found",
        5 => "access_denied",
        _ => "error",
    };
    ProbeResult {
        verdict: verdict.into(),
        win32_error: err,
    }
}

/// Every process id on the system (two-call EnumProcesses size probe).
fn all_process_ids() -> Vec<u32> {
    let mut capacity = 1024usize;
    for _ in 0..4 {
        let mut buf = vec![0u32; capacity];
        let mut needed = 0u32;
        let bytes = (capacity * size_of::<u32>()) as u32;
        // SAFETY: buf holds exactly bytes bytes and bytes says so.
        let ok = unsafe { EnumProcesses(buf.as_mut_ptr(), bytes, &mut needed) };
        if ok == 0 {
            return Vec::new();
        }
        let count = needed as usize / size_of::<u32>();
        if count < capacity {
            buf.truncate(count);
            return buf;
        }
        capacity = count + 256;
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// TCP listeners
// ---------------------------------------------------------------------------

/// MIB_TCPROW_OWNER_PID - 24 bytes, no padding.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct MibTcpRowOwnerPid {
    state: u32,
    local_addr: u32,
    local_port: u32,
    remote_addr: u32,
    remote_port: u32,
    owning_pid: u32,
}

#[link(name = "iphlpapi")]
unsafe extern "system" {
    fn GetExtendedTcpTable(
        table: *mut c_void,
        size: *mut u32,
        order: i32,
        af: u32,
        table_class: u32,
        reserved: u32,
    ) -> u32;
}

const AF_INET: u32 = 2;
/// TCP_TABLE_OWNER_PID_LISTENER - listening sockets only, which is what "port
/// already in use" means.
const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// Processes listening on port (IPv4). An empty vec means nothing is bound.
pub fn port_owners(port: u16) -> Result<Vec<PortOwner>> {
    let mut size = 0u32;
    // SAFETY: a null table with size 0 is the documented size query.
    let status = unsafe {
        GetExtendedTcpTable(
            null_mut(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER && status != 0 {
        anyhow::bail!("GetExtendedTcpTable size query failed with win32 error {status}");
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf is size bytes and size describes it.
    let status = unsafe {
        GetExtendedTcpTable(
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if status == ERROR_INSUFFICIENT_BUFFER {
        // The table grew between the size query and the read; retry once with
        // the size the second query reported.
        buf = vec![0u8; size as usize];
        // SAFETY: as above, with the grown buffer.
        let status = unsafe {
            GetExtendedTcpTable(
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                0,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status != 0 {
            anyhow::bail!("GetExtendedTcpTable failed with win32 error {status}");
        }
    } else if status != 0 {
        anyhow::bail!("GetExtendedTcpTable failed with win32 error {status}");
    }

    if buf.len() < 4 {
        return Ok(Vec::new());
    }
    let count = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
    let stride = size_of::<MibTcpRowOwnerPid>();
    let mut out = Vec::new();
    for i in 0..count {
        let offset = 4 + i * stride;
        if offset + stride > buf.len() {
            break;
        }
        // SAFETY: bounds checked just above; the row struct is POD.
        let row = unsafe {
            std::ptr::read_unaligned(buf.as_ptr().add(offset) as *const MibTcpRowOwnerPid)
        };
        if network_port(row.local_port) != port {
            continue;
        }
        let pid = row.owning_pid;
        let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)
            .and_then(|p| process_name(p.raw()))
            .unwrap_or_else(|| "<unknown>".into());
        out.push(PortOwner {
            pid,
            process,
            local_addr: format_ipv4(row.local_addr),
            family: "inet".into(),
        });
    }
    Ok(out)
}

/// dwLocalPort carries the port in network byte order in its low 16 bits, which
/// is what ntohs((u_short)value) reads.
fn network_port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

fn format_ipv4(raw: u32) -> String {
    // dwLocalAddr is stored in network byte order; on a little-endian host the
    // in-memory bytes are already in address order.
    let bytes = raw.to_ne_bytes();
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

/// Human-readable access mask for the text report.
pub fn describe_access(access: u32) -> String {
    const BITS: &[(u32, &str)] = &[
        (0x0000_0001, "READ"),
        (0x0000_0002, "WRITE"),
        (0x0000_0004, "APPEND"),
        (0x0000_0010, "WRITE_EA"),
        (0x0000_0020, "READ_ATTR"),
        (0x0000_0080, "READ_EA"),
        (0x0000_0100, "WRITE_ATTR"),
        (0x0001_0000, "DELETE"),
        (0x0002_0000, "READ_CONTROL"),
        (0x0004_0000, "WRITE_DAC"),
        (0x0008_0000, "WRITE_OWNER"),
        (0x0010_0000, "SYNCHRONIZE"),
        (0x1000_0000, "GENERIC_ALL"),
        (0x4000_0000, "GENERIC_WRITE"),
        (0x8000_0000, "GENERIC_READ"),
    ];
    let mut parts = Vec::new();
    for &(bit, name) in BITS {
        if access & bit != 0 {
            parts.push(name);
        }
    }
    if parts.is_empty() {
        format!("0x{access:08X}")
    } else {
        format!("0x{access:08X} ({})", parts.join("|"))
    }
}

/// File record number with the sequence number masked off, matching the way
/// the fer index keys entries (FRN_MASK).
pub fn frn_display(frn: u64) -> String {
    format!("{frn:#018x} (record {:#x})", frn & FRN_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a file and return its path.
    fn write_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn win32_struct_layouts_are_what_the_kernel_writes() {
        // The handle table arrives as raw bytes; a layout change here would
        // silently mis-parse every row instead of failing to compile.
        assert_eq!(size_of::<SystemHandleEntry>(), 40);
        assert_eq!(size_of::<ProcessBasicInformation>(), 48);
        assert_eq!(size_of::<MibTcpRowOwnerPid>(), 24);
    }

    #[test]
    fn nt_path_prefix_is_stripped() {
        let sep = std::path::MAIN_SEPARATOR;
        let verbatim = format!("{sep}{sep}?{sep}D:{sep}proj");
        assert_eq!(normalize_nt_path(&verbatim), format!("D:{sep}proj"));
        let verbatim_unc = format!("{sep}{sep}?{sep}UNC{sep}server{sep}share{sep}deep");
        assert_eq!(
            normalize_nt_path(&verbatim_unc),
            format!("{sep}{sep}server{sep}share{sep}deep")
        );
        // A path with no verbatim prefix is returned unchanged.
        let plain = format!("D:{sep}plain");
        assert_eq!(normalize_nt_path(&plain), plain);
    }

    #[test]
    fn trailing_separator_is_trimmed_but_volume_roots_are_kept() {
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(strip_trailing_sep(&format!("D:{sep}proj{sep}")), format!("D:{sep}proj"));
        assert_eq!(
            strip_trailing_sep(&format!("D:{sep}proj{sep}{sep}")),
            format!("D:{sep}proj")
        );
        assert_eq!(strip_trailing_sep(&format!("D:{sep}")), format!("D:{sep}"));
        assert_eq!(
            strip_trailing_sep(&format!("D:{sep}a{sep}b")),
            format!("D:{sep}a{sep}b")
        );
    }

    #[test]
    fn path_matching_respects_component_boundaries() {
        let sep = std::path::MAIN_SEPARATOR;
        let p = |s: &str| s.replace('/', &sep.to_string());
        assert!(path_within(&p("D:/proj"), &p("D:/proj")));
        assert!(path_within(&p("D:/proj/sub/a.txt"), &p("D:/proj")));
        assert!(!path_within(&p("D:/proj2"), &p("D:/proj")));
        assert!(!path_within(&p("D:/proj2/a.txt"), &p("D:/proj")));
        assert!(!path_within(&p("D:/pro"), &p("D:/proj")));
        // case-insensitive on both sides
        assert!(path_within(&p("d:/PROJ/Sub"), &p("D:/proj")));
    }

    #[test]
    fn access_masks_classify_write_and_read() {
        assert!(access_is_hard(0x0001_0000)); // DELETE
        assert!(access_is_hard(0x4000_0000)); // GENERIC_WRITE
        assert!(access_is_hard(0x1000_0000)); // GENERIC_ALL
        assert!(access_is_hard(0x0000_0002)); // FILE_WRITE_DATA
        assert!(!access_is_hard(0x0000_0001)); // FILE_READ_DATA
        assert!(!access_is_hard(0x0000_0020)); // FILE_READ_ATTRIBUTES
        assert!(!access_is_hard(0));
    }

    #[test]
    fn local_port_is_network_order_in_low_half() {
        // dwLocalPort stores ntohs(port): the wire bytes land swapped in the low
        // 16 bits, so 80 arrives as 0x5000 and 8899 (0x22C3) as 0xC322.
        assert_eq!(network_port(0x0000_5000), 80);
        assert_eq!(network_port(0x0000_901F), 8080);
        assert_eq!(network_port(0x0000_C322), 8899);
    }

    #[test]
    fn absolute_target_resolves_relative_paths() {
        let resolved = absolute_target(".").unwrap();
        assert!(resolved.contains(':') || resolved.starts_with("\\\\"));
        let plain = format!("D:{}Kita-Tools", std::path::MAIN_SEPARATOR);
        assert_eq!(absolute_target(&plain).unwrap(), plain);
    }

    /// The snapshot must locate a file handle this process is holding. No
    /// elevation needed: the marker handle belongs to the current process.
    #[test]
    fn snapshot_locates_own_open_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_file(temp.path(), "marker.txt").canonicalize().unwrap();
        let kept = std::fs::File::open(&path).unwrap();
        let snapshot = HandleSnapshot::capture().unwrap();
        let file_type = snapshot
            .probe_file_type_index()
            .expect("the running executable is openable, so the File type index is measurable");
        let me = std::process::id();
        let my_handle = kept.as_raw_handle() as usize;
        assert!(
            snapshot
                .entries
                .iter()
                .any(|&(pid, handle, _, ty)| pid == me && handle == my_handle && ty == file_type),
            "the kept file handle must appear as a File-typed row"
        );
    }

    /// End-to-end handle half: a file this process holds open is attributed to
    /// this process.
    #[test]
    fn open_file_is_attributed_to_self() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_file(temp.path(), "held.bin");
        let kept = std::fs::File::open(&path).unwrap();
        let report = inspect(&path.to_string_lossy(), LockOptions::default()).unwrap();
        assert_eq!(report.kind, "file");
        assert!(
            report.holders.iter().any(|h| h.pid == std::process::id()),
            "own pid must be reported for a file it holds open: {:?}",
            report.holders
        );
        drop(kept);
    }

    /// End-to-end cwd half: the directory this process works in is attributed.
    #[test]
    fn working_directory_is_attributed_to_self() {
        let cwd = std::env::current_dir().unwrap();
        let report = inspect(&cwd.to_string_lossy(), LockOptions::default()).unwrap();
        assert_eq!(report.kind, "dir");
        assert!(
            report
                .holders
                .iter()
                .any(|h| h.pid == std::process::id() && h.kind == "cwd"),
            "own pid must be reported as a cwd holder of its working directory: {:?}",
            report.holders
        );
    }

    /// The running test binary is mapped into this process, so a file-target
    /// scan must name this process even though no handle points at the image.
    #[test]
    fn own_executable_image_is_attributed_to_self() {
        let exe = std::env::current_exe().unwrap();
        let report = inspect(&exe.to_string_lossy(), LockOptions::default()).unwrap();
        assert_eq!(report.kind, "file");
        let me = std::process::id();
        assert!(
            report
                .holders
                .iter()
                .any(|h| h.pid == me && h.kind == "image" && h.is_self),
            "the running test binary must be reported as this process image: {:?}",
            report.holders
        );
    }

    /// Opting into the module walk must not change the image-level answer, and
    /// must still find the image (it is module 0).
    #[test]
    fn module_scan_still_names_the_image() {
        let exe = std::env::current_exe().unwrap();
        let report = inspect(
            &exe.to_string_lossy(),
            LockOptions {
                modules: true,
                ..Default::default()
            },
        )
        .unwrap();
        let me = std::process::id();
        assert!(
            report
                .holders
                .iter()
                .any(|h| h.pid == me && (h.kind == "image" || h.kind == "module")),
            "the image must be found with --modules too: {:?}",
            report.holders
        );
    }

    #[test]
    fn reading_own_cwd_matches_std() {
        let process = open_process(
            std::process::id(),
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
        )
        .expect("own process is always openable");
        let read = read_cwd(process.raw()).expect("own PEB is readable");
        let actual = strip_trailing_sep(&std::env::current_dir().unwrap().to_string_lossy());
        assert_eq!(fold_lower(&read), fold_lower(&actual));
    }

    #[test]
    fn port_lookup_finds_own_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let owners = port_owners(port).unwrap();
        assert!(
            owners.iter().any(|o| o.pid == std::process::id()),
            "a bound listener must be attributed to this process: {owners:?}"
        );
    }

    /// The probe must succeed on a file nothing holds without share-delete, and
    /// report sharing_violation once such a handle exists. std File opens with
    /// FILE_SHARE_DELETE, so the blocking handle is created here by hand.
    #[test]
    fn probe_reports_sharing_violation() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_file(temp.path(), "probed.txt");
        let free = probe_delete_access(&path.to_string_lossy(), false);
        assert_eq!(free.verdict, "would_succeed", "{free:?}");

        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide is NUL-terminated; the share mode deliberately omits
        // FILE_SHARE_DELETE, which is exactly what a blocking holder looks like.
        let holder = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x8000_0000, // GENERIC_READ
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        assert_ne!(
            holder, INVALID_HANDLE_VALUE,
            "could not open the probe file for the blocking case"
        );
        let blocked = probe_delete_access(&path.to_string_lossy(), false);
        // SAFETY: opened just above, closed exactly once.
        unsafe { CloseHandle(holder) };
        assert_eq!(blocked.verdict, "sharing_violation", "{blocked:?}");
    }

    #[test]
    fn access_description_is_readable() {
        assert!(describe_access(0x0001_0000).contains("DELETE"));
        assert!(describe_access(0).starts_with("0x"));
    }
}
