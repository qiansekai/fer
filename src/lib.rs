//! File-Engine-Rust: an Everything-grade instant file search engine, rewritten in Rust.
//!
//! * `mft`     — raw $MFT scanner: hard-link aliases, size, timestamps, flags
//! * `usn`     — NTFS USN/MFT enumeration (fallback + change journal)
//! * `walk`    — plain directory-walk fallback (no admin required)
//! * `query`   — filter query language (`ext: size: dm: parent:` …)
//! * `mem`     — FERIDX01 dump engine: mmap zero-copy, all queries in memory
//! * `du`      — directory size aggregation (WizTree-style totals from the dump)
//! * `monitor` — USN journal polling to keep the index live
//! * `server`  — HTTP API (axum) with a minimal web UI
//! * `store`   — SQLite + FTS5 (feature `sqlite`, dev/test oracle only —
//!   production queries never touch it)

pub mod du;
pub mod dupes;
pub mod indexer;
pub mod mem;
pub mod mft;
pub mod monitor;
pub mod query;
pub mod server;
#[cfg(feature = "sqlite")]
pub mod store;
pub mod usn;
pub mod walk;

/// NTFS root directory file reference number (MFT record 5).
pub const ROOT_FRN: u64 = 5;
/// Mask that strips the sequence number from a 64-bit file reference number.
pub const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Whether the current process token is elevated (administrator). Drives the
/// hard gates on `fer index` / `fer monitor`: raw $MFT / USN journal access
/// requires it, and silently degrading to the metadata-less walk path would
/// overwrite a good dump with a worse one.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TokenElevation, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle; TOKEN_QUERY is
    // a minimal access mask; the out handle is initialized above.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return false;
    }
    let mut elevated = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    // SAFETY: `token` is a valid query handle; the buffer is a correctly
    // sized TOKEN_ELEVATION; TokenElevation is the matching info class.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevated as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    // SAFETY: `token` came from OpenProcessToken and is used exactly once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevated.TokenIsElevated != 0
}

/// Relaunch this process elevated via the UAC consent prompt (ShellExecuteW
/// "runas", same arguments and working directory). On approval the elevated
/// child takes over and this call never returns (the original process exits
/// 0). It returns Ok only when elevation was declined or unavailable, so the
/// caller can fall back to an instructive error. Requires an interactive
/// desktop — from a service context (e.g. `fer serve` rescan) this fails and
/// the caller reports it instead of prompting.
pub fn try_self_elevate() -> anyhow::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide0 = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let mut args = std::env::args_os();
    let exe = args.next().unwrap_or_default();
    let exe_w = wide0(&exe.to_string_lossy());
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| wide0(&p.to_string_lossy()))
        .unwrap_or_default();
    let mut params: Vec<u16> = Vec::new();
    for (i, a) in args.enumerate() {
        if i > 0 {
            params.push(b' ' as u16);
        }
        let w: Vec<u16> = a.to_string_lossy().encode_utf16().collect();
        params.extend(quote_win_arg(&w));
    }
    params.push(0);
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide0("runas").as_ptr(),
            exe_w.as_ptr(),
            if params.len() > 1 { params.as_ptr() } else { std::ptr::null() },
            if cwd.is_empty() { std::ptr::null() } else { cwd.as_ptr() },
            SW_SHOWNORMAL,
        )
    } as u32;
    if rc > 32 {
        // The elevated child re-runs this command line; it is the new owner.
        std::process::exit(0);
    }
    if rc == 1223 {
        anyhow::bail!(
            "elevation request cancelled — re-run from an elevated terminal, or pass \
             --method walk explicitly to accept a degraded index"
        );
    }
    anyhow::bail!(
        "could not request elevation (ShellExecuteW error {rc}) — re-run from an elevated \
         terminal, or pass --method walk explicitly to accept a degraded index"
    );
}

/// Quote one command-line argument per the Windows C-runtime rules (quote
/// when it contains spaces/tabs/quotes or is empty; backslashes before a
/// quote or the closing quote are doubled).
fn quote_win_arg(arg: &[u16]) -> Vec<u16> {
    let sp = b' ' as u16;
    let tab = b'\t' as u16;
    let q = b'"' as u16;
    let bs = b'\\' as u16;
    let needs_quote = arg.is_empty() || arg.iter().any(|c| *c == sp || *c == tab || *c == q);
    if !needs_quote {
        return arg.to_vec();
    }
    let mut out = Vec::with_capacity(arg.len() + 2);
    out.push(q);
    let mut backslashes = 0usize;
    for &c in arg {
        if c == bs {
            backslashes += 1;
        } else if c == q {
            out.extend(std::iter::repeat_n(bs, backslashes * 2 + 1));
            out.push(q);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n(bs, backslashes));
            out.push(c);
            backslashes = 0;
        }
    }
    out.extend(std::iter::repeat_n(bs, backslashes * 2));
    out.push(q);
    out
}

#[cfg(test)]
mod tests {
    use super::quote_win_arg;

    fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn win_arg_quoting() {
        assert_eq!(quote_win_arg(&u16s("plain")), u16s("plain"));
        assert_eq!(quote_win_arg(&u16s("has space")), u16s("\"has space\""));
        assert_eq!(quote_win_arg(&u16s("")), u16s("\"\""));
        assert_eq!(quote_win_arg(&u16s(r#"a"b"#)), u16s(r#""a\"b""#));
        // backslashes alone need no quoting…
        assert_eq!(quote_win_arg(&u16s(r"trail\")), u16s(r"trail\"));
        // …but when quoting IS needed, a trailing backslash is doubled
        assert_eq!(quote_win_arg(&u16s(r"sp ace\")), u16s(r#""sp ace\\""#));
    }
}

/// Per-entry metadata flowing through the index pipeline.
#[derive(Debug, Clone, Copy, Default)]
pub struct EntryMeta {
    pub is_dir: bool,
    pub size: u64,
    /// Bytes of clusters actually allocated ($DATA AllocatedSize, 0 for
    /// resident files that live inside the MFT record). `0` can also mean
    /// "unknown" (walk/USN fallback paths); indexes loaded from pre-v6 dumps
    /// fall back to `size` at load time.
    pub allocated: u64,
    pub mtime: i64, // unix seconds (0 = unknown)
    pub ctime: i64, // unix seconds (0 = unknown)
    /// bit0 hidden, bit1 system, bit2 readonly, bit3 reparse
    pub flags: u8,
    /// NTFS file reference number (USN/MFT paths; used by the monitor).
    pub frn: Option<u64>,
}

impl EntryMeta {
    pub const FLAG_HIDDEN: u8 = 1;
    pub const FLAG_SYSTEM: u8 = 2;
    pub const FLAG_READONLY: u8 = 4;
    pub const FLAG_REPARSE: u8 = 8;

    pub fn hidden(&self) -> bool {
        self.flags & Self::FLAG_HIDDEN != 0
    }
    pub fn system(&self) -> bool {
        self.flags & Self::FLAG_SYSTEM != 0
    }
    pub fn readonly(&self) -> bool {
        self.flags & Self::FLAG_READONLY != 0
    }
    pub fn reparse(&self) -> bool {
        self.flags & Self::FLAG_REPARSE != 0
    }
}

/// One search hit (path + metadata), shared by every engine.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub flags: u8,
}

/// Result of a full index build.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BuildReport {
    pub method: String,
    pub volumes: Vec<String>,
    pub files: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub elapsed_ms: u128,
    pub max_usn: i64,
}

/// Basename of a Windows/Linux style path.
pub fn basename(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

/// Lowercase with an ASCII fast path. Windows names are overwhelmingly ASCII;
/// `str::to_lowercase` pays full Unicode processing for every one of them,
/// which dominates build/load time at millions of rows.
#[inline]
pub fn fold_lower(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}

/// Reversed lowercase name (suffix searches run as prefix searches on it).
/// Byte reversal is only valid for ASCII; non-ASCII reverses by chars so
/// multi-byte sequences stay well-formed.
#[inline]
pub fn lower_rev(s: &str) -> String {
    if s.is_ascii() {
        let mut b = s.as_bytes().to_vec();
        b.reverse();
        // Reversed ASCII is still valid UTF-8.
        String::from_utf8(b).expect("ascii is valid utf8")
    } else {
        s.chars().rev().collect()
    }
}
