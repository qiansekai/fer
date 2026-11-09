//! 搜索历史（Everything 式「搜索」菜单里的最近查询）。
//!
//! 位置：%LOCALAPPDATA%\fer-gui\history.txt（本机专属数据，不入库）。
//! 契约（冻结）：
//!   History::load(max) -> History        缺失/损坏一律回退空历史，绝不 panic
//!   History::save(&self) -> io::Result<()>   自动建父目录
//!   History::push(&mut self, query: &str)    去重（大小写不敏感、保留最新写法）、最新在前、超 max 截断
//!   History::{items, len, is_empty, clear}
//!   history_path() -> PathBuf
//!
//! 文件格式：一行一条，UTF-8，**最新在最前**。
//! 刻意不依赖 crate::config 与 crate::json：历史文件坏了不该影响配置恢复，反之亦然
//!（与 config.rs 自带极简 JSON 读写是同一个「解耦」理由）。零第三方依赖。

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 默认保留的历史条数（main.rs 可自行传其它上限）。
pub const DEFAULT_MAX: usize = 20;

#[derive(Debug, Clone)]
pub struct History {
	/// 最新查询在 items[0]。
	items: Vec<String>,
	/// 上限；至少为 1（load 时钳制，避免 max=0 让历史永远为空）。
	max: usize,
}

impl Default for History {
	/// 空历史 + 默认上限（不能让 max 落在 0 上，否则 push 会被截成空）。
	fn default() -> Self {
		Self { items: Vec::new(), max: DEFAULT_MAX }
	}
}

impl History {
	/// 读默认位置的历史；缺失/损坏 -> 空历史。
	pub fn load(max: usize) -> Self {
		Self::load_from(&history_path(), max)
	}

	/// 写默认位置；自动建父目录。
	pub fn save(&self) -> io::Result<()> {
		self.save_to(&history_path())
	}

	/// 记一条查询：空/纯空白忽略；换行等控制字符清理成单行；
	/// 大小写不敏感去重（保留本次写法）；最新在前；超上限截断最旧。
	pub fn push(&mut self, query: &str) {
		let cleaned = clean_query(query);
		if cleaned.is_empty() {
			return;
		}
		let key = cleaned.to_lowercase();
		self.items.retain(|item| item.to_lowercase() != key);
		self.items.insert(0, cleaned);
		if self.items.len() > self.max {
			self.items.truncate(self.max);
		}
	}

	pub fn items(&self) -> &[String] {
		&self.items
	}

	pub fn len(&self) -> usize {
		self.items.len()
	}

	pub fn is_empty(&self) -> bool {
		self.items.is_empty()
	}

	pub fn clear(&mut self) {
		self.items.clear();
	}

	/// 显式路径版本（单测与「另存」用）；任何读取异常都回退空历史。
	pub fn load_from(path: &Path, max: usize) -> Self {
		let mut history = Self { items: Vec::new(), max: max.max(1) };
		let Ok(text) = fs::read_to_string(path) else {
			// 文件缺失、无权限、非 UTF-8（损坏）：一律当空历史
			return history;
		};
		// 文件按「最新在前」保存，倒序 push 才能让顺序在往返后保持不变；
		// 文件内的重复项由 push 的去重收敛（保留更靠前 = 更新的那条写法）。
		for line in text.lines().rev() {
			history.push(line);
		}
		history
	}

	/// 显式路径版本；父目录不存在则递归创建。
	pub fn save_to(&self, path: &Path) -> io::Result<()> {
		if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
			fs::create_dir_all(parent)?;
		}
		let mut text = String::with_capacity(self.items.iter().map(|s| s.len() + 1).sum());
		for item in &self.items {
			text.push_str(item);
			text.push('\n');
		}
		fs::write(path, text)
	}
}

/// 历史文件路径：%LOCALAPPDATA%\fer-gui\history.txt（环境变量缺失时退回当前目录，
/// 与 config.rs 的 dir() 保持同一约定）。
pub fn history_path() -> PathBuf {
	let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
	PathBuf::from(base).join("fer-gui").join("history.txt")
}

/// 清洗一条查询，保证它永远是一行：换行/回车/制表符压成空格，其余控制字符丢弃，
/// 首尾空白去掉。
fn clean_query(raw: &str) -> String {
	let mut out = String::with_capacity(raw.len());
	for ch in raw.chars() {
		match ch {
			'\r' | '\n' | '\t' => out.push(' '),
			c if c.is_control() => {}
			c => out.push(c),
		}
	}
	out.trim().to_string()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	/// 临时目录守卫：测试结束（含 panic 展开）时自动删除。
	struct TempDir(PathBuf);

	impl TempDir {
		fn new(tag: &str) -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map(|d| d.as_nanos())
				.unwrap_or(0);
			let dir = std::env::temp_dir()
				.join(format!("fer-gui-history-{tag}-{}-{nanos}", std::process::id()));
			let _ = fs::remove_dir_all(&dir);
			fs::create_dir_all(&dir).expect("建临时目录");
			Self(dir)
		}

		fn file(&self, name: &str) -> PathBuf {
			self.0.join(name)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	#[test]
	fn round_trip_keeps_order() {
		let dir = TempDir::new("roundtrip");
		let path = dir.file("history.txt");

		let mut history = History::load_from(&path, DEFAULT_MAX);
		assert!(history.is_empty(), "缺失文件应为空历史");
		assert_eq!(history.items(), &[] as &[String]);

		history.push("first");
		history.push("second");
		history.push("third");
		history.save_to(&path).expect("写盘");
		assert_eq!(history.items(), &["third".to_string(), "second".to_string(), "first".to_string()]);
		assert_eq!(history.len(), 3);
		assert!(!history.is_empty());

		let reloaded = History::load_from(&path, DEFAULT_MAX);
		assert_eq!(reloaded.items(), history.items(), "往返后顺序应不变");
		assert_eq!(reloaded.len(), 3);
	}

	#[test]
	fn dedupe_is_case_insensitive_and_keeps_newest_spelling() {
		let mut history = History::default();
		history.push("Cargo");
		history.push("readme");
		history.push("CARGO");
		assert_eq!(history.items(), &["CARGO".to_string(), "readme".to_string()]);
		assert_eq!(history.len(), 2);

		history.push("cargo");
		assert_eq!(history.items(), &["cargo".to_string(), "readme".to_string()]);

		// 去重后重新置顶
		history.push("readme");
		assert_eq!(history.items(), &["readme".to_string(), "cargo".to_string()]);
	}

	#[test]
	fn newest_first_and_truncates_to_max() {
		let mut history = History { items: Vec::new(), max: 3 };
		for q in ["a", "b", "c", "d", "e"] {
			history.push(q);
		}
		assert_eq!(history.items(), &["e".to_string(), "d".to_string(), "c".to_string()]);
		assert_eq!(history.len(), 3);
	}

	#[test]
	fn empty_and_whitespace_queries_are_ignored() {
		let mut history = History::default();
		history.push("");
		history.push("   ");
		history.push("\t\r\n ");
		assert!(history.is_empty());
		assert_eq!(history.len(), 0);
	}

	#[test]
	fn queries_with_newlines_never_produce_extra_lines() {
		let dir = TempDir::new("newlines");
		let path = dir.file("history.txt");

		let mut history = History::default();
		history.push("a\r\nb");
		history.push("line1\nline2\tend");
		history.save_to(&path).expect("写盘");

		for item in history.items() {
			assert!(!item.contains('\n') && !item.contains('\r') && !item.contains('\t'), "{item:?}");
		}
		assert_eq!(history.items(), &["line1 line2 end".to_string(), "a  b".to_string()]);

		let text = fs::read_to_string(&path).expect("读回");
		assert_eq!(text.lines().count(), 2, "两条历史就应只有两行：{text:?}");

		let reloaded = History::load_from(&path, DEFAULT_MAX);
		assert_eq!(reloaded.items(), history.items());
	}

	#[test]
	fn corrupt_or_missing_file_yields_empty_history() {
		let dir = TempDir::new("corrupt");

		// 缺失
		let missing = dir.file("nope.txt");
		let history = History::load_from(&missing, DEFAULT_MAX);
		assert!(history.is_empty());

		// 非 UTF-8 字节（损坏）
		let broken = dir.file("broken.txt");
		fs::write(&broken, [0xFF_u8, 0xFE, 0x00, 0x80]).expect("写坏文件");
		let history = History::load_from(&broken, DEFAULT_MAX);
		assert!(history.is_empty(), "损坏文件应回退空历史而不得 panic");

		// 目录当文件读
		let as_dir = dir.file("subdir");
		fs::create_dir_all(&as_dir).expect("建目录");
		assert!(History::load_from(&as_dir, DEFAULT_MAX).is_empty());

		// 空文件
		let empty = dir.file("empty.txt");
		fs::write(&empty, "").expect("写空文件");
		assert!(History::load_from(&empty, DEFAULT_MAX).is_empty());
	}

	#[test]
	fn duplicate_lines_in_file_are_deduped_keeping_newest() {
		let dir = TempDir::new("dupes");
		let path = dir.file("history.txt");
		fs::write(&path, "b\nA\na\nb\n").expect("写文件");

		let history = History::load_from(&path, DEFAULT_MAX);
		// 文件顺序 b,A,a,b（最新在前）：A 比 a 新，故保留 A；两条 b 收敛成一条
		assert_eq!(history.items(), &["b".to_string(), "A".to_string()]);
	}

	#[test]
	fn load_clamps_max_and_truncates_existing_file() {
		let dir = TempDir::new("clamp");
		let path = dir.file("history.txt");
		fs::write(&path, "d\nc\nb\na\n").expect("写文件");

		let history = History::load_from(&path, 2);
		assert_eq!(history.items(), &["d".to_string(), "c".to_string()]);

		// max = 0 被钳制为 1：至少留一条，而不是永远空
		let mut zero = History::load_from(&path, 0);
		assert_eq!(zero.len(), 1);
		zero.push("x");
		assert_eq!(zero.items(), &["x".to_string()]);
	}

	#[test]
	fn save_creates_missing_parent_dirs() {
		let dir = TempDir::new("mkdir");
		let path = dir.file("deep").join("nested").join("history.txt");

		let mut history = History::default();
		history.push("hello");
		history.save_to(&path).expect("写盘应自动建父目录");

		assert!(path.exists(), "文件应已创建");
		assert_eq!(History::load_from(&path, DEFAULT_MAX).items(), &["hello".to_string()]);
	}

	#[test]
	fn save_empty_history_writes_empty_file() {
		let dir = TempDir::new("empty-save");
		let path = dir.file("history.txt");
		let history = History::default();
		history.save_to(&path).expect("写盘");
		assert_eq!(fs::read_to_string(&path).expect("读回"), "");
		assert!(History::load_from(&path, DEFAULT_MAX).is_empty());
	}

	#[test]
	fn clear_empties_items() {
		let mut history = History::default();
		history.push("a");
		history.push("b");
		assert_eq!(history.len(), 2);
		history.clear();
		assert!(history.is_empty());
		assert_eq!(history.items(), &[] as &[String]);

		let dir = TempDir::new("clear-save");
		let path = dir.file("history.txt");
		history.save_to(&path).expect("写盘");
		assert!(History::load_from(&path, DEFAULT_MAX).is_empty());
	}

	#[test]
	fn clean_query_drops_control_characters() {
		assert_eq!(clean_query("  spaced  "), "spaced");
		assert_eq!(clean_query("a\u{0}b\u{7}c"), "abc");
		assert_eq!(clean_query("a\tb"), "a b");
		assert_eq!(clean_query("\r\n"), "");
	}

	#[test]
	fn history_path_is_under_fer_gui() {
		let path = history_path();
		assert!(path.ends_with(Path::new("fer-gui").join("history.txt")), "{path:?}");
		assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("history.txt"));
	}
}
