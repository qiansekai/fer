//! 展示层格式化（纯函数，全部可单测）。
//!
//! 契约：
//!   human_size(u64) -> String            库侧 src/fmt.rs 的实现（例 "1.23 GB" / "512 B"）
//!   human_count(u64) -> String           千分位：例 "4,223,028"
//!   format_time(i64) -> String           本地时区 "2026-09-30 12:34"；0 或负值 -> "-"
//!   file_name(&str) -> &str              末段（路径以分隔符结尾时返回空串）
//!   parent_dir(&str) -> &str             父目录（无分隔符时返回空串）
//!   path_segments(&str) -> Vec<(String, String)>  面包屑：逐级 (显示段, 该级完整路径)
//!   parent_of(&str) -> Option<String>    上一级目录；已在根/无法解析 -> None
//!   normalize_dir(&str) -> String        目录显示规范化（去多余尾分隔符，保留根形态）
//!
//! 自带单元测试覆盖边界（0 字节、>1TB、中文路径、尾部反斜杠、无扩展名、多点文件名）。
//!
//! 时区：走 Win32 时区 API（unix -> FILETIME -> FileTimeToSystemTime ->
//! SystemTimeToTzSpecificLocalTime），得到真实本地时间并正确处理夏令时（DST）。
//! 只有 API 失败（返回 0、或年份超出 SYSTEMTIME 可表示范围）时才回退到
//! 「UTC + 固定偏移」——默认 +08:00，可用环境变量 FER_GUI_UTC_OFFSET_SECONDS 覆盖。

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{
	FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime, TIME_ZONE_INFORMATION,
};

/// FILETIME 起点（1601-01-01）与 Unix 纪元（1970-01-01）相差的秒数。
const FILETIME_EPOCH_DIFF_SECONDS: i64 = 11_644_473_600;
/// FILETIME 的时间单位 = 100 纳秒。
const FILETIME_INTERVALS_PER_SECOND: i128 = 10_000_000;

/// 默认本地时区偏移：+08:00（中国标准时间，无夏令时）。
pub const DEFAULT_UTC_OFFSET_SECONDS: i64 = 8 * 3600;

/// 取本地时区偏移秒数：优先环境变量 FER_GUI_UTC_OFFSET_SECONDS，
/// 非法值或超出 [-12h, +14h] 一律回退默认。
pub fn local_utc_offset_seconds() -> i64 {
	std::env::var("FER_GUI_UTC_OFFSET_SECONDS")
		.ok()
		.and_then(|s| s.trim().parse::<i64>().ok())
		.filter(|v| (-43_200..=50_400).contains(v)) // [-12h, +14h]
		.unwrap_or(DEFAULT_UTC_OFFSET_SECONDS)
}

// 字节格式化统一由库侧提供（src/fmt.rs）：CLI 与 GUI 共用同一份实现。
pub use file_engine_rust::fmt::human_size;

/// 千分位计数（每 3 位一个逗号）。
pub fn human_count(n: u64) -> String {
	let digits = n.to_string();
	let bytes = digits.as_bytes();
	let mut out = String::with_capacity(digits.len() + digits.len() / 3);
	for (i, b) in bytes.iter().enumerate() {
		if i > 0 && (bytes.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(*b as char);
	}
	out
}

/// unix 秒 -> UTC 的 SYSTEMTIME（经 FILETIME，由系统换算日历，不做本地猜测）。
/// 超出 FILETIME / SYSTEMTIME 可表示范围时返回 None（不 panic）。
pub fn utc_system_time(unix: i64) -> Option<SYSTEMTIME> {
	// 用 i128 中转：unix 秒乘 1e7 会溢出 i64；负值（1969 年之前）在这里被排除。
	let intervals =
		(i128::from(unix) + i128::from(FILETIME_EPOCH_DIFF_SECONDS)) * FILETIME_INTERVALS_PER_SECOND;
	let intervals = u64::try_from(intervals).ok()?;
	let file_time = FILETIME {
		dwLowDateTime: (intervals & 0xFFFF_FFFF) as u32,
		dwHighDateTime: (intervals >> 32) as u32,
	};
	let mut utc = SYSTEMTIME::default();
	// SAFETY: 两个指针都指向本函数栈上的有效结构，调用期间存活。
	let ok = unsafe { FileTimeToSystemTime(&file_time, &mut utc) };
	if ok == 0 { None } else { Some(utc) }
}

/// UTC 的 SYSTEMTIME -> 本地 SYSTEMTIME。tz 为 None 表示「用当前时区」（含 DST）；
/// 传显式 TIME_ZONE_INFORMATION 则按该结构里的 StandardDate/DaylightDate 规则换算。
/// 该函数是时区换算的唯一入口，测试用显式时区走的是同一条路径。
///
/// 注：这里用非 Ex 版本（接受 TIME_ZONE_INFORMATION）。SystemTimeToTzSpecificLocalTimeEx
/// 接受 DYNAMIC_TIME_ZONE_INFORMATION，实测在 TimeZoneKeyName 为空时会失败（返回 0），
/// 而显式 TZI 版本只看结构内的规则，正好适合单测构造固定时区。
fn tz_specific_local(utc: &SYSTEMTIME, tz: Option<&TIME_ZONE_INFORMATION>) -> Option<SYSTEMTIME> {
	let mut local = SYSTEMTIME::default();
	let tz_ptr = tz.map_or(std::ptr::null(), |tz| tz as *const TIME_ZONE_INFORMATION);
	// SAFETY: tz_ptr 要么为 NULL（MSDN：使用当前生效时区），要么指向调用方持有的有效结构；
	// utc 与 local 都是本函数可见的有效引用。
	let ok = unsafe { SystemTimeToTzSpecificLocalTime(tz_ptr, utc, &mut local) };
	if ok == 0 { None } else { Some(local) }
}

/// 真实本地时间（系统时区 + 夏令时）的 (年, 月, 日, 时, 分)；
/// Win32 时区 API 不可用时返回 None，由 format_time 走固定偏移回退。
pub fn system_local_time(unix: i64) -> Option<(i64, i64, i64, i64, i64)> {
	let utc = utc_system_time(unix)?;
	let local = tz_specific_local(&utc, None)?;
	Some((
		i64::from(local.wYear),
		i64::from(local.wMonth),
		i64::from(local.wDay),
		i64::from(local.wHour),
		i64::from(local.wMinute),
	))
}

/// Unix 秒 -> 本地时间 "YYYY-MM-DD HH:MM"；0 或负值返回 "-"。
///
/// 主路径 = 系统时区 API（含 DST）；回退路径 = 固定偏移
/// （format_time_with_offset + local_utc_offset_seconds），仅在 API 失败时使用。
pub fn format_time(unix: i64) -> String {
	if unix <= 0 {
		return "-".to_string();
	}
	if let Some((y, m, d, hh, mm)) = system_local_time(unix) {
		return render_local(y, m, d, hh, mm);
	}
	// 回退：时区 API 不可用（年份越界等）时按固定偏移换算
	format_time_with_offset(unix, local_utc_offset_seconds())
}

/// 拼 "YYYY-MM-DD HH:MM"。年份超出 1..=9999 一律给占位符 "-"：
/// 越界的 unix 秒（如 i64::MAX）既可能被自算路径渲染成六位数年份，也不该出现在 UI 里。
fn render_local(y: i64, m: i64, d: i64, hh: i64, mm: i64) -> String {
	if !(1..=9999).contains(&y) {
		return "-".to_string();
	}
	format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// 指定 UTC 偏移（秒）的换算版本，供单测与其它时区复用。
pub fn format_time_with_offset(unix: i64, offset_seconds: i64) -> String {
	if unix <= 0 {
		return "-".to_string();
	}
	let local = unix.saturating_add(offset_seconds);
	let days = local.div_euclid(86_400);
	let secs = local.rem_euclid(86_400);
	let (y, m, d) = civil_from_days(days);
	let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
	render_local(y, m, d, hh, mm)
}

/// 天数（1970-01-01 起，可为负）-> (年, 月, 日)。
/// 算法：Howard Hinnant 的 civil_from_days（proleptic Gregorian，无时区依赖）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
	let z = z + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	(y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// 最后一个路径分隔符（同时接受 Windows 反斜杠与正斜杠）。
fn last_separator(path: &str) -> Option<usize> {
	path.rfind(['\\', '/'])
}

/// 末段文件名；路径以分隔符结尾时返回空串。
pub fn file_name(path: &str) -> &str {
	match last_separator(path) {
		Some(i) => &path[i + 1..],
		None => path,
	}
}

/// 父目录；无分隔符时返回空串。
pub fn parent_dir(path: &str) -> &str {
	match last_separator(path) {
		Some(i) => &path[..i],
		None => "",
	}
}


/// 是否为路径分隔符（同时接受 Windows 反斜杠与正斜杠）。
fn is_sep(c: char) -> bool {
	c == '\\' || c == '/'
}

/// 是否以 UNC 前缀（两个连续分隔符）开头。
fn starts_with_unc(path: &str) -> bool {
	let mut it = path.chars();
	matches!(it.next(), Some(c) if is_sep(c)) && matches!(it.next(), Some(c) if is_sep(c))
}

/// 盘符根前缀判定：字母 + 冒号，且后面结束或紧跟分隔符（"D:" / "D:\" / "D:/"）。
fn is_drive_root_prefix(path: &str) -> bool {
	let b = path.as_bytes();
	b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || is_sep(b[2] as char))
}

/// 根前缀的字节长度（根串本身不含尾分隔符）：
///   "D:"              -> 2
///   "\\server\share"  -> 整个串（UNC 共享根）
///   "\\server"        -> 整个串（不完整的 UNC 也按根处理，保证不 panic）
///   其它              -> None（相对路径、单前导分隔符形式）
fn root_prefix_len(path: &str) -> Option<usize> {
	if starts_with_unc(path) {
		let rest = &path[2..];
		let server_end = rest.find(['\\', '/'])?;
		let after_server = rest[server_end..].trim_start_matches(['\\', '/']);
		let share_start = 2 + (rest.len() - after_server.len());
		if after_server.is_empty() {
			return Some(path.len()); // "\\server"：没有共享名，整体当根
		}
		match after_server.find(['\\', '/']) {
			Some(i) => Some(share_start + i),
			None => Some(path.len()), // 恰好落在共享根
		}
	} else if is_drive_root_prefix(path) {
		Some(2)
	} else {
		None
	}
}

/// 是否已在「根」上（无法再往上一级）：盘符根、UNC 共享根、纯分隔符。
fn is_dir_root(path: &str) -> bool {
	let trimmed = path.trim_end_matches(is_sep);
	if trimmed.is_empty() {
		return !path.is_empty();
	}
	matches!(root_prefix_len(trimmed), Some(len) if len == trimmed.len())
}

/// 规范化目录显示：去掉多余尾分隔符，但保留根形态。
///   "D:\a\" -> "D:\a"；"D:\" -> "D:\"（沿用原分隔符，所以 "D:/" 仍是 "D:/"）；"D:" -> "D:"
///   "\\server\share\" -> "\\server\share"（UNC 共享根不再补尾分隔符）
///   全是分隔符的输入（"\" / "\\" / "/"）原样返回；空串返回空串。
/// 只做字符串层面的整理，不查文件系统，也不改写正斜杠（除非拼根时需要补一个）。
pub fn normalize_dir(path: &str) -> String {
	if path.is_empty() {
		return String::new();
	}
	let trimmed = path.trim_end_matches(is_sep);
	if trimmed.is_empty() {
		return path.to_string(); // 全是分隔符
	}
	// 盘符根（"D:"）原本带尾分隔符 -> 补回一个，且沿用输入用的那个分隔符
	if trimmed.len() == 2 && is_drive_root_prefix(trimmed) && path.len() > trimmed.len() {
		let sep = path[trimmed.len()..].chars().next().unwrap_or('\\');
		return format!("{trimmed}{sep}");
	}
	trimmed.to_string()
}

/// 把目录路径拆成逐级 (显示段, 该级完整路径)，供面包屑使用。
///   "D:\a\b"           -> [("D:", "D:\"), ("a", "D:\a"), ("b", "D:\a\b")]
///   "\\server\share\a" -> [("\\server\share", "\\server\share"), ("a", "\\server\share\a")]
///   "/a/b"             -> [("a", "/a"), ("b", "/a/b")]
///   "a\b"              -> [("a", "a"), ("b", "a\b")]
/// 说明：
/// - 尾分隔符忽略（"D:\a\" 与 "D:\a" 同结果）；连续分隔符折叠；空串、全分隔符 -> 空 Vec。
/// - 显示段用原样的名字（CJK / 空格 / 点开头都不动）；盘符根的完整路径补一个分隔符
///   （"D:" -> "D:\"），这样点面包屑交给 fer 的一定是明确的「盘符根」而不是 D: 这种
///   含义随当前目录变化的驱动相对路径。UNC 共享根本身就是可导航形态，不再补分隔符。
/// - 分隔符风格跟随输入里第一个分隔符（"C:/a/B" 产出 "C:/a/B" 而不是混用）。
pub fn path_segments(path: &str) -> Vec<(String, String)> {
	let normalized = normalize_dir(path);
	if normalized.is_empty() {
		return Vec::new();
	}
	let joiner = normalized.chars().find(|c| is_sep(*c)).unwrap_or('\\');
	let bytes = normalized.as_bytes();
	let mut segments: Vec<(String, String)> = Vec::new();
	// 起点：有根前缀（盘符根 / UNC 共享根）就产出根段并带上它的可导航形态；
	// 否则只保留可能存在的「前导分隔符」（root-relative 的 "/a/b"、退化 UNC 的 "\\server"），
	// 前缀不单独占一段；没有前导分隔符时 lead = 0，等于从零开始。
	let (mut current, mut i) = match root_prefix_len(&normalized) {
		Some(root_len) => {
			let display = normalized[..root_len].to_string();
			// 根的可导航形态：盘符根补一个分隔符（"D:" -> "D:\"），UNC 共享根本身已是根形态
			let root_path = normalize_dir(&format!("{display}{joiner}"));
			segments.push((display, root_path.clone()));
			(root_path, root_len)
		}
		None => {
			let lead = normalized.find(|c| !is_sep(c)).unwrap_or(normalized.len());
			(normalized[..lead].to_string(), lead)
		}
	};
	// 只按 ASCII 分隔符切分，切片边界永远落在 ASCII 上，CJK 段安全
	while i < bytes.len() {
		while i < bytes.len() && is_sep(bytes[i] as char) {
			i += 1;
		}
		if i >= bytes.len() {
			break;
		}
		let start = i;
		while i < bytes.len() && !is_sep(bytes[i] as char) {
			i += 1;
		}
		if !current.is_empty() && !current.ends_with(['\\', '/']) {
			current.push(joiner);
		}
		current.push_str(&normalized[start..i]);
		segments.push((normalized[start..i].to_string(), current.clone()));
	}
	segments
}

/// 上一级目录；已在根（"D:\" / "D:" / "\\server\share" / "\"）或无法解析时返回 None。
///   "D:\a\b" -> Some("D:\a")；"D:\a" -> Some("D:\")；"D:" / "D:\" -> None
///   "\\server\share\a" -> Some("\\server\share")；"\\server\share" -> None
///   "a\b" -> Some("a")；"a"（没有分隔符的相对路径）-> None；"/a" -> Some("/")
/// 返回值与 normalize_dir 同一形态（普通目录不带尾分隔符，根带一个）。
pub fn parent_of(path: &str) -> Option<String> {
	let normalized = normalize_dir(path);
	if normalized.is_empty() {
		return None;
	}
	let trimmed = normalized.trim_end_matches(is_sep);
	if trimmed.is_empty() || is_dir_root(trimmed) {
		return None;
	}
	let idx = trimmed.rfind(['\\', '/'])?;
	let raw_head = &trimmed[..idx];
	if raw_head.is_empty() {
		// "/a" -> 根 "/"
		return Some(trimmed[..1].to_string());
	}
	let head = raw_head.trim_end_matches(is_sep);
	if head.is_empty() || head.chars().all(is_sep) {
		// "\\server" 这类不完整 UNC：再往上没有可导航目录
		return None;
	}
	if head.len() == 2 && is_drive_root_prefix(head) {
		// 盘符根：沿用原分隔符（"C:/a" -> "C:/"）
		let sep = trimmed[idx..].chars().next().unwrap_or('\\');
		return Some(format!("{head}{sep}"));
	}
	Some(head.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn human_count_千分位() {
		assert_eq!(human_count(0), "0");
		assert_eq!(human_count(1), "1");
		assert_eq!(human_count(999), "999");
		assert_eq!(human_count(1000), "1,000");
		assert_eq!(human_count(12345), "12,345");
		assert_eq!(human_count(4_223_028), "4,223,028");
		assert_eq!(human_count(u64::MAX), "18,446,744,073,709,551,615");
	}

	#[test]
	fn format_time_非正值返回占位符() {
		assert_eq!(format_time(0), "-");
		assert_eq!(format_time(-1), "-");
		assert_eq!(format_time(i64::MIN), "-");
		assert_eq!(format_time_with_offset(0, 0), "-");
		assert_eq!(format_time_with_offset(-86_400, 8 * 3600), "-");
	}

	#[test]
	fn format_time_utc8_换算() {
		let cst = 8 * 3600;
		assert_eq!(format_time_with_offset(1, cst), "1970-01-01 08:00");
		assert_eq!(format_time_with_offset(946_684_800, cst), "2000-01-01 08:00");
		assert_eq!(format_time_with_offset(1_000_000_000, cst), "2001-09-09 09:46");
		assert_eq!(format_time_with_offset(1_700_000_000, cst), "2023-11-15 06:13");
		assert_eq!(format_time_with_offset(2_147_483_647, cst), "2038-01-19 11:14");
		assert_eq!(format_time_with_offset(4_102_444_800, cst), "2100-01-01 08:00");
		// 闰年 2 月 29 日；以及跨日边界（UTC 当日 15:59 -> 本地 23:59，UTC 16:00 -> 本地次日 00:00）
		assert_eq!(format_time_with_offset(1_582_934_400, cst), "2020-02-29 08:00");
		assert_eq!(format_time_with_offset(1_700_063_940, cst), "2023-11-15 23:59");
		assert_eq!(format_time_with_offset(1_700_064_000, cst), "2023-11-16 00:00");
		assert_eq!(format_time_with_offset(1_700_006_340, cst), "2023-11-15 07:59");
	}

	#[test]
	fn format_time_其它时区与本地入口() {
		assert_eq!(format_time_with_offset(1, 0), "1970-01-01 00:00");
		assert_eq!(format_time_with_offset(1, -18_000), "1969-12-31 19:00");
		assert_eq!(format_time_with_offset(946_684_800, -18_000), "1999-12-31 19:00");
		// format_time 走环境变量/默认值，结果至少是合法格式且非占位符
		let s = format_time(1_700_000_000);
		assert_eq!(s.len(), 16);
		assert_ne!(s, "-");
	}

	/// 解析 "YYYY-MM-DD HH:MM"（纯 ASCII，按字节切片安全）。
	fn parse_local(s: &str) -> (i64, i64, i64, i64, i64) {
		assert_eq!(s.len(), 16, "格式必须是 YYYY-MM-DD HH:MM: {s}");
		assert_eq!(&s[4..5], "-");
		assert_eq!(&s[7..8], "-");
		assert_eq!(&s[10..11], " ");
		assert_eq!(&s[13..14], ":");
		let num = |a: usize, b: usize| s[a..b].parse::<i64>().expect("数字段");
		(num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16))
	}

	/// Howard Hinnant 的 days_from_civil：与 civil_from_days 互逆，测试里用于把
	/// 本地墙钟反推回「当作 UTC 看的秒数」，从而算出该瞬间的真实偏移。
	fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
		let y = if m <= 2 { y - 1 } else { y };
		let era = if y >= 0 { y } else { y - 399 } / 400;
		let yoe = y - era * 400;
		let mp = (m + 9) % 12;
		let doy = (153 * mp + 2) / 5 + d - 1;
		let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
		era * 146_097 + doe - 719_468
	}

	/// 该瞬间的本地偏移（分钟）。只对整分钟对齐的时刻有意义。
	fn local_offset_minutes(unix: i64) -> i64 {
		assert_eq!(unix % 60, 0, "偏移反推要求整分钟对齐的时刻");
		let (y, m, d, hh, mm) = parse_local(&format_time(unix));
		let wall = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60;
		(wall - unix) / 60
	}

	#[test]
	fn utc_system_time_边界() {
		// 纪元与已知点：FILETIME 换算正确（系统给的日历，不靠自算）
		let epoch = utc_system_time(0).expect("1970-01-01 应可换算");
		assert_eq!((epoch.wYear, epoch.wMonth, epoch.wDay, epoch.wHour, epoch.wMinute), (1970, 1, 1, 0, 0));
		let known = utc_system_time(1_000_000_000).expect("2001-09-09 应可换算");
		assert_eq!((known.wYear, known.wMonth, known.wDay), (2001, 9, 9));
		assert_eq!((known.wHour, known.wMinute, known.wSecond), (1, 46, 40));
		// 纪元之前：1969-12-31 23:59:59
		let before = utc_system_time(-1).expect("1969-12-31 应可换算");
		assert_eq!((before.wYear, before.wMonth, before.wDay), (1969, 12, 31));
		assert_eq!((before.wHour, before.wMinute, before.wSecond), (23, 59, 59));
		// 边界：i64::MAX / i64::MIN 超出 FILETIME 或 SYSTEMTIME 表示范围 -> None，不得 panic
		assert!(utc_system_time(i64::MAX).is_none());
		assert!(utc_system_time(i64::MIN).is_none());
	}

	#[test]
	fn format_time_走系统时区_极值不崩溃() {
		// 系统时区 API 必须成功（失败就退化成固定偏移，本用例会红）
		assert!(system_local_time(1_750_000_000).is_some(), "系统时区 API 应可用");
		assert_eq!(format_time(1_750_000_000).len(), 16);
		// 人工核对用：cargo test -- --nocapture 可直接看到本机时区的换算结果
		println!(
			"format_time(1750000000) = {}；本机当前偏移 {} 分钟",
			format_time(1_750_000_000),
			// 偏移反推要求整点时刻（1_700_006_400 = 2023-11-15 16:00 UTC）
			local_offset_minutes(1_700_006_400)
		);
		// 极值/异常输入只允许返回 "-" 或合法格式，绝不 panic
		for t in [i64::MIN, -1, 0, 1, 1_750_000_000, 4_102_444_800, i64::MAX] {
			let s = format_time(t);
			if t <= 0 || t == i64::MAX {
				// 非正值按契约给占位符；i64::MAX 越出可表示年份，同样给占位符
				assert_eq!(s, "-", "应为占位符: t={t} s={s}");
			} else {
				let _ = parse_local(&s);
			}
		}
		// 真实偏移必须是 15 分钟的整数倍且落在全球合法区间 [-12h, +14h]
		// （用整点时刻，避免「秒余数」把分钟差算出零头）
		for t in [946_684_800, 1_582_934_400, 1_700_006_400, 4_102_444_800] {
			let off = local_offset_minutes(t);
			assert!(off % 15 == 0, "偏移应为 15 分钟整数倍，实测 {off} 分钟 @ {t}");
			assert!((-720..=840).contains(&off), "偏移越界：{off} 分钟 @ {t}"); // [-12h, +14h]
		}
	}

	#[test]
	fn dst_夏令时按真实规则换算() {
		// 合成「UTC-8 + 美国式夏令时」：标准时起点 11 月第一个周日 02:00，
		// 夏令时起点 3 月第二个周日 02:00（Bias=480 => UTC-8，DaylightBias=-60 => UTC-7）。
		// 这样即使本机时区（中国标准时间）没有 DST，也能验证 DST 分支真的被走到。
		let tz = TIME_ZONE_INFORMATION {
			Bias: 480,
			StandardBias: 0,
			DaylightBias: -60,
			StandardDate: SYSTEMTIME {
				wMonth: 11,
				wDayOfWeek: 0, // 周日
				wDay: 1,       // 当月第 1 个周日
				wHour: 2,
				..SYSTEMTIME::default()
			},
			DaylightDate: SYSTEMTIME {
				wMonth: 3,
				wDayOfWeek: 0,
				wDay: 2, // 当月第 2 个周日
				wHour: 2,
				..SYSTEMTIME::default()
			},
			..TIME_ZONE_INFORMATION::default()
		};
		// 2025-01-15 12:00 UTC -> 本地 04:00（标准时，UTC-8）
		let winter = tz_specific_local(&utc_system_time(1_736_942_400).unwrap(), Some(&tz)).unwrap();
		assert_eq!((winter.wYear, winter.wMonth, winter.wDay, winter.wHour), (2025, 1, 15, 4));
		// 2025-07-15 12:00 UTC -> 本地 05:00（夏令时，UTC-7）
		let summer = tz_specific_local(&utc_system_time(1_752_580_800).unwrap(), Some(&tz)).unwrap();
		assert_eq!((summer.wYear, summer.wMonth, summer.wDay, summer.wHour), (2025, 7, 15, 5));
		// 同一时区下的两次换算差 1 小时，正是 DST 生效的直接证据
		let diff = (summer.wHour as i64 - winter.wHour as i64).rem_euclid(24);
		assert_eq!(diff, 1);
	}

	#[test]
	fn file_name_与_parent_dir() {
		assert_eq!(file_name(r"D:\Kita-Tools\Coding\fer-gui\src\format.rs"), "format.rs");
		assert_eq!(parent_dir(r"D:\Kita-Tools\Coding\fer-gui\src\format.rs"), r"D:\Kita-Tools\Coding\fer-gui\src");
		assert_eq!(file_name("format.rs"), "format.rs");
		assert_eq!(parent_dir("format.rs"), "");
		// 尾部反斜杠 -> 末段为空串
		assert_eq!(file_name(r"D:\dir\"), "");
		assert_eq!(parent_dir(r"D:\dir\"), r"D:\dir");
		assert_eq!(file_name(""), "");
		assert_eq!(parent_dir(""), "");
		// 正斜杠与中文路径
		assert_eq!(file_name("D:/中文目录/报告.txt"), "报告.txt");
		assert_eq!(parent_dir("D:/中文目录/报告.txt"), "D:/中文目录");
		// 纯盘符根
		assert_eq!(file_name(r"D:\"), "");
		assert_eq!(parent_dir(r"D:\"), "D:");
	}

	// ---- 目录导航（面包屑 / 上一级 / 规范化）----

	#[test]
	fn normalize_dir_去尾分隔符且保留根() {
		assert_eq!(normalize_dir(""), "");
		// 普通目录：去掉一个或多个尾分隔符
		assert_eq!(normalize_dir(r"D:\a"), r"D:\a");
		assert_eq!(normalize_dir(r"D:\a\"), r"D:\a");
		assert_eq!(normalize_dir("D:\\a\\\\"), r"D:\a");
		assert_eq!(normalize_dir("D:\\a\\b\\/"), r"D:\a\b");
		assert_eq!(normalize_dir("D:/a/"), "D:/a");
		// 盘符根：保留，且沿用输入用的分隔符
		assert_eq!(normalize_dir(r"D:\"), r"D:\");
		assert_eq!(normalize_dir("D:/"), "D:/");
		assert_eq!(normalize_dir("D:\\\\"), r"D:\");
		assert_eq!(normalize_dir("D:"), "D:");
		// UNC：去尾分隔符，但不额外补
		assert_eq!(normalize_dir(r"\\server\share\"), r"\\server\share");
		assert_eq!(normalize_dir(r"\\server\share"), r"\\server\share");
		// 纯分隔符原样返回
		assert_eq!(normalize_dir("/"), "/");
		assert_eq!(normalize_dir(r"\\"), r"\\");
		// CJK / 空格 / 点开头目录名不得被破坏
		assert_eq!(normalize_dir("D:\\中文 目录\\.隐藏\\"), r"D:\中文 目录\.隐藏");
	}

	#[test]
	fn path_segments_逐级拆分() {
		assert!(path_segments("").is_empty());
		assert_eq!(
			path_segments(r"D:\a\b"),
			vec![
				("D:".to_string(), r"D:\".to_string()),
				("a".to_string(), r"D:\a".to_string()),
				("b".to_string(), r"D:\a\b".to_string()),
			]
		);
		// 尾分隔符忽略：与不带尾分隔符完全一致
		assert_eq!(path_segments(r"D:\a\b\"), path_segments(r"D:\a\b"));
		// 只给盘符 / 盘符根
		assert_eq!(path_segments("D:"), vec![("D:".to_string(), r"D:\".to_string())]);
		assert_eq!(path_segments(r"D:\"), path_segments("D:"));
		// 单级
		assert_eq!(path_segments(r"D:\a"), vec![
			("D:".to_string(), r"D:\".to_string()),
			("a".to_string(), r"D:\a".to_string()),
		]);
		// 正斜杠：分隔符风格跟随输入，不混用
		assert_eq!(
			path_segments("C:/a/B"),
			vec![
				("C:".to_string(), "C:/".to_string()),
				("a".to_string(), "C:/a".to_string()),
				("B".to_string(), "C:/a/B".to_string()),
			]
		);
		// 连续分隔符折叠
		assert_eq!(path_segments("D:\\a\\\\\\b"), path_segments(r"D:\a\b"));
		// 相对路径
		assert_eq!(
			path_segments(r"a\b"),
			vec![("a".to_string(), "a".to_string()), ("b".to_string(), r"a\b".to_string())]
		);
		assert_eq!(path_segments("a"), vec![("a".to_string(), "a".to_string())]);
		// root-relative（前导单个分隔符）
		assert_eq!(
			path_segments("/a/b"),
			vec![("a".to_string(), "/a".to_string()), ("b".to_string(), "/a/b".to_string())]
		);
		// CJK / 空格 / 点开头
		assert_eq!(
			path_segments(r"D:\中文 目录\.隐藏"),
			vec![
				("D:".to_string(), r"D:\".to_string()),
				("中文 目录".to_string(), r"D:\中文 目录".to_string()),
				(".隐藏".to_string(), r"D:\中文 目录\.隐藏".to_string()),
			]
		);
		// UNC：共享根占第一段
		assert_eq!(
			path_segments(r"\\server\share\dir"),
			vec![
				(r"\\server\share".to_string(), r"\\server\share".to_string()),
				("dir".to_string(), r"\\server\share\dir".to_string()),
			]
		);
		assert_eq!(
			path_segments(r"\\server\share"),
			vec![(r"\\server\share".to_string(), r"\\server\share".to_string())]
		);
		// 不完整 UNC 不 panic（退化成普通段）
		let degenerate = path_segments(r"\\server");
		assert_eq!(degenerate.len(), 1);
		assert_eq!(degenerate[0].1, r"\\server");
		// 只有分隔符 -> 空
		assert!(path_segments(r"\\").is_empty());
		assert!(path_segments("/").is_empty());
		assert!(path_segments(r"\").is_empty());
	}

	#[test]
	fn parent_of_上一级与到根() {
		assert_eq!(parent_of(""), None);
		assert_eq!(parent_of("D:"), None);
		assert_eq!(parent_of(r"D:\"), None);
		assert_eq!(parent_of(r"D:\a"), Some(r"D:\".to_string()));
		assert_eq!(parent_of(r"D:\a\b"), Some(r"D:\a".to_string()));
		assert_eq!(parent_of(r"D:\a\b\"), Some(r"D:\a".to_string()));
		// 正斜杠风格保留
		assert_eq!(parent_of("C:/a/b"), Some("C:/a".to_string()));
		assert_eq!(parent_of("C:/a"), Some("C:/".to_string()));
		// UNC
		assert_eq!(parent_of(r"\\server\share"), None);
		assert_eq!(parent_of(r"\\server\share\dir"), Some(r"\\server\share".to_string()));
		assert_eq!(parent_of(r"\\server"), None);
		// 相对路径
		assert_eq!(parent_of(r"a\b"), Some("a".to_string()));
		assert_eq!(parent_of("a"), None);
		// root-relative
		assert_eq!(parent_of("/a"), Some("/".to_string()));
		assert_eq!(parent_of("/"), None);
		// CJK
		assert_eq!(parent_of(r"D:\中文 目录\.隐藏"), Some(r"D:\中文 目录".to_string()));
	}

	#[test]
	fn 导航函数自洽_逐级向上收敛到根() {
		let mut cur = r"D:\a\b\c".to_string();
		let mut hops = 0;
		while let Some(parent) = parent_of(&cur) {
			// 每上一级，面包屑恰好少一段
			assert_eq!(path_segments(&cur).len(), path_segments(&parent).len() + 1);
			// 面包屑末段的完整路径 == 当前目录
			assert_eq!(path_segments(&cur).last().unwrap().1, cur);
			cur = parent;
			hops += 1;
			assert!(hops < 10, "parent_of 应逐级收敛");
		}
		assert_eq!(cur, r"D:\");
		assert_eq!(parent_of(&cur), None);
	}
}
