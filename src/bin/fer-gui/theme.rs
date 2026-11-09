//! 深色模式支持。
//!
//! Windows 没有官方的 Win32 深色 API，业界通行做法是三层叠加，缺任何一层都会「花」：
//!   1. uxtheme 序号导出 `AllowDarkModeForWindow`(133) / `SetPreferredAppMode`(135)
//!   2. `SetWindowTheme` 把控件主题切到 `DarkMode_Explorer` / `DarkMode_ItemsView`
//!   3. `DwmSetWindowAttribute`(20) 让标题栏变深
//!
//! 颜色本身还要靠 `WM_CTLCOLOR*`（返回深色画刷）与 ListView 自绘（NM_CUSTOMDRAW）。
//! 两个序号导出没有符号名，只能 `GetProcAddress` 按序号取；老系统上取不到时静默降级为浅色。

use std::ffi::c_void;

use winsafe::{self as w};
use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

// COLORREF 布局是 0x00BBGGRR（不是 RGB 顺序）。
// 取值依据 WCAG 2.1 深色模式规范：
//   - 背景不用纯黑，主文字不用纯白（避免 halation 光晕）
//   - 正文对比度 ≥ 4.5:1（#1E1E1E + #E0E0E0 ≈ 11.5:1）
//   - 次要文字 ≥ 4.5:1（#1E1E1E + #A9A9AB ≈ 6.5:1）
/// 客户区背景 #1E1E1E
pub const BG: u32 = 0x001E_1E1E;
/// 列表背景 #252526
pub const LIST_BG: u32 = 0x0026_2525;
/// 输入框背景 #3C3C3C
pub const EDIT_BG: u32 = 0x003C_3C3C;
/// 主文字 #E0E0E0（规范推荐值，对比度约 11.5:1）
pub const FG: u32 = 0x00E0_E0E0;
/// 次要文字 / 表头 / 状态栏 #A9A9AB（对比度约 6.5:1）
pub const FG_DIM: u32 = 0x00AB_A9A9;
/// 选中行背景 #264F78
pub const SELECT: u32 = 0x0078_4F26;

#[link(name = "uxtheme")]
unsafe extern "system" {
	fn SetWindowTheme(hwnd: *mut c_void, sub_app_name: *const u16, sub_id_list: *const u16) -> i32;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
	fn DwmSetWindowAttribute(
		hwnd: *mut c_void,
		attribute: u32,
		pv_attribute: *const c_void,
		cb_attribute: u32,
	) -> i32;
}

/// `uxtheme!AllowDarkModeForWindow` 序号。
const ORD_ALLOW_DARK_MODE_FOR_WINDOW: usize = 133;
/// `uxtheme!SetPreferredAppMode` 序号。
const ORD_SET_PREFERRED_APP_MODE: usize = 135;
/// `SetPreferredAppMode` 的 ForceDark。
const PREFERRED_MODE_FORCE_DARK: i32 = 2;
/// `DWMWA_USE_IMMERSIVE_DARK_MODE`（Win10 1903+ 为 20）。
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

/// 取 uxtheme 的序号导出；取不到返回 None（老系统降级）。
unsafe fn uxtheme_proc(
	ordinal: usize,
) -> Option<unsafe extern "system" fn() -> isize> {
	let module = unsafe { GetModuleHandleW(w::WString::from_str("uxtheme.dll").as_ptr()) };
	if module.is_null() {
		return None;
	}
	unsafe { GetProcAddress(module, ordinal as *const u8) }
}

/// 进程级：声明偏好深色。必须在创建窗口之前调用一次。
pub fn prefer_dark_app_mode() {
	unsafe {
		let Some(proc) = uxtheme_proc(ORD_SET_PREFERRED_APP_MODE) else {
			return;
		};
		let set_preferred: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(proc);
		set_preferred(PREFERRED_MODE_FORCE_DARK);
	}
}

/// 窗口级：允许深色、切窗口主题、刷深标题栏。
pub fn apply_window(hwnd: *mut c_void) {
	unsafe {
		if let Some(proc) = uxtheme_proc(ORD_ALLOW_DARK_MODE_FOR_WINDOW) {
			let allow: unsafe extern "system" fn(*mut c_void, i32) -> i32 =
				std::mem::transmute(proc);
			allow(hwnd, 1);
		}
		let theme = w::WString::from_str("DarkMode_Explorer");
		SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
		let on: i32 = 1;
		DwmSetWindowAttribute(
			hwnd,
			DWMWA_USE_IMMERSIVE_DARK_MODE,
			&on as *const i32 as *const c_void,
			std::mem::size_of::<i32>() as u32,
		);
	}
}

/// 控件级：切到指定的深色控件主题（`DarkMode_Explorer` / `DarkMode_ItemsView`）。
pub fn apply_control(hwnd: *mut c_void, theme_name: &str) {
	unsafe {
		if let Some(proc) = uxtheme_proc(ORD_ALLOW_DARK_MODE_FOR_WINDOW) {
			let allow: unsafe extern "system" fn(*mut c_void, i32) -> i32 =
				std::mem::transmute(proc);
			allow(hwnd, 1);
		}
		let theme = w::WString::from_str(theme_name);
		SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
	}
}

/// 创建一个实心画刷（进程退出时由系统回收；窗口类画刷不显式删除是常规做法）。
pub fn solid(color: u32) -> *mut c_void {
	unsafe { CreateSolidBrush(color) }
}
