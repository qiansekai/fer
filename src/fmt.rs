//! 人类可读的字节数格式化 —— CLI 与 GUI 共用的唯一实现。
//!
//! 合并 fer-gui 前 CLI 的 `fmt_bytes` 与 GUI 的 `human_size` 各写了一份：
//! 同一套 1024 进制语义，但 GUI 版多了 TB/PB/EB 与「1024.00 进位」修正。
//! 这里取完整的那份，CLI 的旧实现删除。

const SIZE_UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];

/// 人类可读的字节数。1024 进制，保留 2 位小数；
/// 四舍五入后会顶到 1024.00 的（如 1TB-1）自动进到更大单位，避免出现 "1024.00 GB"。
pub fn human_size(bytes: u64) -> String {
	if bytes < 1024 {
		return format!("{bytes} B");
	}
	let mut unit = 0usize;
	let mut scale: u64 = 1;
	while unit + 1 < SIZE_UNITS.len() {
		let next = scale.saturating_mul(1024);
		if bytes < next {
			break;
		}
		scale = next;
		unit += 1;
	}
	let mut value = bytes as f64 / scale as f64;
	if value >= 1023.995 && unit + 1 < SIZE_UNITS.len() {
		unit += 1;
		scale = scale.saturating_mul(1024);
		value = bytes as f64 / scale as f64;
	}
	format!("{value:.2} {}", SIZE_UNITS[unit])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn human_size_边界() {
		assert_eq!(human_size(0), "0 B");
		assert_eq!(human_size(1), "1 B");
		assert_eq!(human_size(512), "512 B");
		assert_eq!(human_size(1023), "1023 B");
		assert_eq!(human_size(1024), "1.00 KB");
		assert_eq!(human_size(1536), "1.50 KB");
		assert_eq!(human_size(1024 * 1024), "1.00 MB");
		assert_eq!(human_size(1_320_000_000), "1.23 GB");
		assert_eq!(human_size(1024u64.pow(4)), "1.00 TB");
		assert_eq!(human_size(1024u64.pow(5)), "1.00 PB");
		// 四舍五入会顶到 1024.00 的边界必须进位，不能输出 "1024.00 TB"。
		assert_eq!(human_size(1024u64.pow(4) - 1), "1.00 TB");
		// 上限：u64::MAX 约 16 EB，不得越界。
		assert_eq!(human_size(u64::MAX), "16.00 EB");
	}
}
