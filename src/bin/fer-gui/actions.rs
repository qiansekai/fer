//! 系统集成动作（唯一会拉起外部进程 / 操作剪贴板的地方）。
//!
//! 契约（冻结）：
//!   reveal_in_explorer(path: &str) -> io::Result<()>    explorer /select,"<path>"
//!   open_file(path: &str) -> io::Result<()>             系统默认关联打开
//!   copy_to_clipboard(text: &str) -> Result<(), String> CF_UNICODETEXT
//!
//! 约束：
//! - 参数一律作为独立 argv 元素传入（禁止拼命令行字符串），防注入。
//! - explorer 用 /select, 前缀形式；前导 / 或 - 不得被当成开关（先规范成绝对路径）。
//! - 剪贴板失败要返回错误文本而不得 panic；必须成对 OpenClipboard/CloseClipboard。
//! - open_file 主路径 = ShellExecuteW（verb "open"，路径单独传递）；失败回退 cmd /C start。
//! - 自带单元测试：命令参数构造（不实际拉进程、不改用户剪贴板）。

use std::io;
use std::path::Path;
use std::process::Command;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, HINSTANCE, HWND};
use windows_sys::Win32::System::DataExchange::{
	CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// CF_UNICODETEXT：windows-sys 未导出该常量，按 Win32 文档固定值声明（与 winuser.h 一致）。
const CF_UNICODETEXT: u32 = 13;

/// ShellExecuteW 的动词：用系统关联打开。
pub const SHELL_OPEN_VERB: &str = "open";

/// SW_SHOWNORMAL：ShellExecuteW 的 nshowcmd。
const SW_SHOWNORMAL: i32 = 1;

/// ShellExecuteW 返回值 <= 32 表示失败（MSDN：值是错误码而非实例句柄）。
const SHELL_EXECUTE_ERROR_MAX: isize = 32;

/// cmd 的静默标志，避免「关联打开」时闪一个控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// Windows 未导出 GlobalFree（0.61 版只有 GlobalAlloc/GlobalLock/GlobalUnlock/GlobalSize），
// 而「内存已分配但移交失败」时必须自行释放，故按 Win32 文档自行声明；非新增依赖。
#[link(name = "kernel32")]
unsafe extern "system" {
	fn GlobalFree(hmem: HANDLE) -> HANDLE;
}

// ShellExecuteW 在 windows-sys 里位于 Win32_UI_Shell 模块，但每个条目还被
// #[cfg(feature = "Win32_UI_WindowsAndMessaging")] 额外门控；只开 Win32_UI_Shell
// 取不到它，而加那个特性要改 Cargo.toml（Lead 作用域）。这里按 Win32 文档
// 声明同签名的绑定（与 windows-sys 生成的完全一致），不引入新依赖。
#[link(name = "shell32")]
unsafe extern "system" {
	fn ShellExecuteW(
		hwnd: HWND,
		lpoperation: *const u16,
		lpfile: *const u16,
		lpparameters: *const u16,
		lpdirectory: *const u16,
		nshowcmd: i32,
	) -> HINSTANCE;
}

/// explorer 定位用的开关前缀：必须是独立 argv 元素，不能与路径拼成一个字符串。
pub const EXPLORER_SELECT_PREFIX: &str = "/select,";

/// 规范成绝对路径：已是绝对路径则原样返回，否则按当前目录展开。
/// 作用之一：保证传给 explorer / start 的路径不以 / 或 - 开头，不会被当成开关。
fn absolute_path(path: &str) -> String {
	if Path::new(path).is_absolute() {
		return path.to_string();
	}
	match std::path::absolute(path) {
		Ok(p) => p.to_string_lossy().into_owned(),
		Err(_) => path.to_string(),
	}
}

/// 构造 explorer 的定位参数（纯函数，单测不拉进程）：
/// 返回 ["/select,", "<绝对路径>"] 两个独立 argv 元素。
pub fn explorer_argv(path: &str) -> io::Result<Vec<String>> {
	if path.trim().is_empty() {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, "路径为空"));
	}
	Ok(vec![EXPLORER_SELECT_PREFIX.to_string(), absolute_path(path)])
}

/// 构造「系统默认关联打开」的回退命令（纯函数，单测不拉进程）。
/// 仅当 ShellExecuteW 失败时使用：cmd /C start "" <path>，路径同样走独立 argv。
pub fn open_command(path: &str) -> io::Result<(&'static str, Vec<String>)> {
	if path.trim().is_empty() {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, "路径为空"));
	}
	Ok((
		"cmd",
		vec![
			"/C".to_string(),
			"start".to_string(),
			String::new(), // start 的第一个带引号参数是窗口标题，此处显式留空
			absolute_path(path),
		],
	))
}

/// 在资源管理器中定位（选中）该路径。
pub fn reveal_in_explorer(path: &str) -> io::Result<()> {
	let argv = explorer_argv(path)?;
	let mut cmd = Command::new("explorer");
	cmd.args(&argv);
	// 不 wait：explorer 常把 /select 转交给已运行的实例后立即退出，等它没有意义。
	let _child = cmd.spawn()?;
	Ok(())
}

/// UTF-16 编码 + 结尾 NUL（Win32 宽字符 API 一律要 NUL 结尾）。纯函数，可单测。
pub fn wide_null(s: &str) -> Vec<u16> {
	let mut units: Vec<u16> = s.encode_utf16().collect();
	units.push(0);
	units
}

/// ShellExecuteW 的参数构造（纯函数，单测不真开进程）：
/// verb 与文件路径分属两个参数、永不拼接；路径先规范成绝对路径并转 NUL 结尾 UTF-16。
pub fn shell_open_args(path: &str) -> io::Result<(&'static str, Vec<u16>)> {
	if path.trim().is_empty() {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, "路径为空"));
	}
	Ok((SHELL_OPEN_VERB, wide_null(&absolute_path(path))))
}

/// 用系统关联打开（ShellExecuteW 主路径）。失败返回 Err，由 open_file 决定是否回退。
pub fn open_file_via_shell(path: &str) -> io::Result<()> {
	let (verb, file) = shell_open_args(path)?;
	let verb_wide = wide_null(verb);
	// SAFETY: 两个宽字符串指针都指向本函数持有的 NUL 结尾缓冲，调用期间存活；
	// hwnd / 参数 / 目录均传 NULL（无父窗口、无附加参数、用当前目录）。
	let hinstance = unsafe {
		ShellExecuteW(
			std::ptr::null_mut(),
			verb_wide.as_ptr(),
			file.as_ptr(),
			std::ptr::null(),
			std::ptr::null(),
			SW_SHOWNORMAL,
		)
	};
	let code = hinstance as isize;
	if code <= SHELL_EXECUTE_ERROR_MAX {
		return Err(io::Error::other(format!("ShellExecuteW 失败（返回码 {code}）")));
	}
	Ok(())
}

/// 回退路径：cmd /C start "" <绝对路径>（ShellExecuteW 不可用时）。
pub fn open_file_via_cmd(path: &str) -> io::Result<()> {
	let (program, argv) = open_command(path)?;
	let mut cmd = Command::new(program);
	cmd.args(&argv);
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		cmd.creation_flags(CREATE_NO_WINDOW);
	}
	let _child = cmd.spawn()?;
	Ok(())
}

/// 用系统默认关联打开文件或目录：优先 ShellExecuteW，失败回退 cmd /C start。
pub fn open_file(path: &str) -> io::Result<()> {
	match open_file_via_shell(path) {
		Ok(()) => Ok(()),
		Err(shell_err) => open_file_via_cmd(path).map_err(|cmd_err| {
			io::Error::new(
				cmd_err.kind(),
				format!("ShellExecuteW 与 cmd 回退均失败：{shell_err}；{cmd_err}"),
			)
		}),
	}
}

/// UTF-16 编码 + 结尾 NUL（CF_UNICODETEXT 要求）。纯函数，可单测。
pub fn clipboard_utf16_units(text: &str) -> Vec<u16> {
	wide_null(text)
}

fn last_error_text(api: &str) -> String {
	let code = unsafe { GetLastError() };
	format!("{api} 失败（Win32 错误码 {code}）")
}

/// 复制文本到剪贴板（CF_UNICODETEXT）。空串直接报错，不触碰系统剪贴板。
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
	if text.is_empty() {
		return Err("复制内容为空".to_string());
	}
	let units = clipboard_utf16_units(text);
	// SAFETY: 只在持有剪贴板期间访问剪贴板 API，且下面的分支保证成对 CloseClipboard。
	unsafe {
		if OpenClipboard(std::ptr::null_mut()) == 0 {
			return Err(last_error_text("OpenClipboard"));
		}
		let written = write_clipboard_locked(&units);
		let closed = CloseClipboard() != 0;
		match written {
			Ok(()) if closed => Ok(()),
			Ok(()) => Err(last_error_text("CloseClipboard")),
			Err(e) => Err(e),
		}
	}
}

/// 写入剪贴板（调用前必须已持有剪贴板）。
fn write_clipboard_locked(units: &[u16]) -> Result<(), String> {
	// SAFETY: 全局内存按 CF_UNICODETEXT 约定分配；成功移交后所有权归系统，失败则自行释放。
	unsafe {
		if EmptyClipboard() == 0 {
			return Err(last_error_text("EmptyClipboard"));
		}
		let bytes = std::mem::size_of_val(units);
		let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
		if hmem.is_null() {
			return Err(last_error_text("GlobalAlloc"));
		}
		let dst = GlobalLock(hmem) as *mut u16;
		if dst.is_null() {
			GlobalFree(hmem);
			return Err(last_error_text("GlobalLock"));
		}
		std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
		let _ = GlobalUnlock(hmem);
		if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
			GlobalFree(hmem); // 移交失败：所有权仍在本进程，必须释放
			return Err(last_error_text("SetClipboardData"));
		}
	}
	Ok(()) // 移交成功：内存归系统所有，不得再释放
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn explorer_argv_前缀与路径分离() {
		let argv = explorer_argv(r"D:\Kita-Tools\Coding\fer-gui\目标 文件.txt").expect("应构造成功");
		assert_eq!(argv.len(), 2, "必须是独立的开关与路径两个元素");
		assert_eq!(argv[0], "/select,");
		assert!(argv[1].ends_with(r"目标 文件.txt"));
		// 绝不出现「开关与路径拼在一起」的形态
		assert!(!argv[0].contains("目标"));
		assert!(!argv[1].starts_with("/select"));
	}

	#[test]
	fn explorer_argv_相对路径被绝对化() {
		let argv = explorer_argv("sub/子目录/.-开头.txt").expect("应构造成功");
		let p = Path::new(&argv[1]);
		assert!(p.is_absolute(), "相对路径必须规范成绝对路径: {}", argv[1]);
		assert!(argv[1].ends_with(".-开头.txt"));
		// 绝对化之后不可能再以 / 或 - 开头（防被当成开关）
		assert!(!argv[1].starts_with('/') && !argv[1].starts_with('-'));
	}

	#[test]
	fn explorer_argv_拒绝空路径() {
		for bad in ["", "   ", "\t"] {
			let err = explorer_argv(bad).expect_err("空路径应报错");
			assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
		}
	}

	#[test]
	fn open_command_参数形态() {
		let (program, argv) = open_command(r"D:\a b\文档.txt").expect("应构造成功");
		assert_eq!(program, "cmd");
		assert_eq!(argv.len(), 4);
		assert_eq!(argv[0], "/C");
		assert_eq!(argv[1], "start");
		assert_eq!(argv[2], "", "start 的窗口标题占位必须存在");
		assert!(argv[3].ends_with(r"文档.txt"));
		assert!(open_command("").is_err());
	}

	#[test]
	fn shell_open_args_verb与路径分离() {
		let (verb, file) = shell_open_args(r"D:a b中文 文档.txt").expect("应构造成功");
		assert_eq!(verb, SHELL_OPEN_VERB);
		assert_eq!(verb, "open");
		// verb 里不得混入路径（防命令行拼接）
		assert!(!verb.contains('\\'));
		// NUL 结尾，解码后能还原路径且是绝对路径
		assert_eq!(*file.last().unwrap(), 0);
		let decoded = String::from_utf16(&file[..file.len() - 1]).expect("应是合法 UTF-16");
		assert!(decoded.ends_with(r"中文 文档.txt"));
		assert!(Path::new(&decoded).is_absolute(), "应规范成绝对路径: {decoded}");
		assert!(shell_open_args("").is_err());
		assert!(shell_open_args("   ").is_err());
	}

	#[test]
	fn wide_null_编码边界() {
		assert_eq!(wide_null(""), vec![0]);
		assert_eq!(wide_null("ab"), vec![97, 98, 0]);
		assert_eq!(wide_null("中"), vec![0x4E2D, 0]);
		// 路径里的空格与反斜杠原样保留（不做任何 shell 解析）
		let s = r"C:\a b\c  d.txt";
		let w = wide_null(s);
		assert_eq!(w.len(), s.chars().count() + 1);
		assert_eq!(*w.last().unwrap(), 0);
	}

	#[test]
	fn clipboard_utf16_编码() {
		assert_eq!(clipboard_utf16_units(""), vec![0]);
		assert_eq!(clipboard_utf16_units("abc"), vec![97, 98, 99, 0]);
		assert_eq!(clipboard_utf16_units("中"), vec![0x4E2D, 0]);
		// 非 BMP 字符 -> 代理对（2 个 u16）+ NUL
		let emoji_like = clipboard_utf16_units("𝄞");
		assert_eq!(emoji_like.len(), 3);
		assert_eq!(*emoji_like.last().unwrap(), 0);
	}

	#[test]
	fn copy_to_clipboard_空串直接报错() {
		// 该分支在 OpenClipboard 之前返回，测试不会改动用户剪贴板
		let err = copy_to_clipboard("").expect_err("空串应报错");
		assert!(err.contains("空"), "错误文本应说明原因: {err}");
	}
}
