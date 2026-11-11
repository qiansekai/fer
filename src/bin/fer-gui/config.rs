//! 配置持久化（窗口几何 + 搜索偏好）。
//!
//! 位置：%LOCALAPPDATA%\fer-gui\config.json（本机专属数据，不入库）。
//! 契约（冻结）：
//!   Config::load() -> Config                 缺失/损坏一律回退默认值，绝不 panic
//!   Config::save(&self) -> io::Result<()>    自动建父目录
//!   Config::path() -> PathBuf
//!
//! 字段：limit / last_query / sort_key / sort_desc / win_x / win_y / win_w / win_h。
//! 自带单元测试：往返序列化 + 损坏输入回退 + 缺失文件回退。
//!
//! 为什么自带一份极简 JSON 读写：写入是手写文本（不引 serde）；读取若依赖 crate::json
//! 会把「配置能否恢复」绑死在另一个模块的实现与改动上。此处的解析器只处理本文件写出
//! 的扁平对象（字符串转义 + 整数 + 布尔），语法不合规整体回退 Default、字段缺失或类型
//! 不符逐字段回退，保证 load() 在任何输入下都不 panic。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
	pub limit: usize,
	pub last_query: String,
	pub sort_key: String,
	pub sort_desc: bool,
	pub win_x: i32,
	pub win_y: i32,
	pub win_w: i32,
	pub win_h: i32,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			limit: 2000,
			last_query: String::new(),
			sort_key: String::new(),
			sort_desc: false,
			win_x: i32::MIN, // MIN = 未记录，交给系统默认
			win_y: i32::MIN,
			win_w: 1180,
			win_h: 720,
		}
	}
}

impl Config {
	/// 配置目录：%LOCALAPPDATA%\fer-gui（环境变量缺失时退回当前目录）。
	pub fn dir() -> PathBuf {
		let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
		PathBuf::from(base).join("fer-gui")
	}

	pub fn path() -> PathBuf {
		Self::dir().join("config.json")
	}

	/// 读取配置；任何异常回退 Default。
	pub fn load() -> Config {
		Self::load_from(&Self::path())
	}

	/// 写盘；自动创建父目录。
	pub fn save(&self) -> io::Result<()> {
		self.save_to(&Self::path())
	}

	/// 显式路径版本（单测与「另存」用）。
	pub fn load_from(path: &Path) -> Config {
		match fs::read_to_string(path) {
			Ok(text) => Self::from_json_text(&text),
			Err(_) => Config::default(),
		}
	}

	/// 显式路径版本；父目录不存在则递归创建。
	pub fn save_to(&self, path: &Path) -> io::Result<()> {
		if let Some(parent) = path.parent()
			&& !parent.as_os_str().is_empty()
		{
			fs::create_dir_all(parent)?;
		}
		fs::write(path, self.to_json_text())
	}

	/// 文本 -> 配置；语法损坏整体回退 Default，字段缺失/类型不符逐字段回退。
	pub fn from_json_text(text: &str) -> Config {
		match parse_flat_object(text) {
			Some(fields) => Self::from_fields(&fields),
			None => Config::default(),
		}
	}

	fn from_fields(fields: &[(String, Raw)]) -> Config {
		let d = Config::default();
		let find = |key: &str| fields.iter().find(|(name, _)| name == key).map(|(_, v)| v);
		let string_of = |key: &str, fallback: &str| match find(key) {
			Some(Raw::Str(s)) => s.clone(),
			_ => fallback.to_string(),
		};
		let bool_of = |key: &str, fallback: bool| match find(key) {
			Some(Raw::Bool(b)) => *b,
			_ => fallback,
		};
		let i32_of = |key: &str, fallback: i32| match find(key) {
			Some(Raw::Int(v)) => i32::try_from(*v).unwrap_or(fallback),
			_ => fallback,
		};
		let positive_of = |key: &str, fallback: i32| {
			let v = i32_of(key, fallback);
			if v > 0 { v } else { fallback }
		};
		let limit = match find("limit") {
			Some(Raw::Int(v)) => usize::try_from(*v).ok().filter(|n| *n > 0).unwrap_or(d.limit),
			_ => d.limit,
		};
		Config {
			limit,
			last_query: string_of("last_query", &d.last_query),
			sort_key: string_of("sort_key", &d.sort_key),
			sort_desc: bool_of("sort_desc", d.sort_desc),
			win_x: i32_of("win_x", d.win_x),
			win_y: i32_of("win_y", d.win_y),
			win_w: positive_of("win_w", d.win_w),
			win_h: positive_of("win_h", d.win_h),
		}
	}

	/// 序列化为 JSON 文本（手写，字段顺序固定，便于人工查看与 diff）。
	pub fn to_json_text(&self) -> String {
		let mut s = String::with_capacity(256);
		s.push_str("{\n");
		s.push_str(&format!("\t\"limit\": {},\n", self.limit));
		s.push_str(&format!("\t\"last_query\": {},\n", json_string(&self.last_query)));
		s.push_str(&format!("\t\"sort_key\": {},\n", json_string(&self.sort_key)));
		s.push_str(&format!("\t\"sort_desc\": {},\n", self.sort_desc));
		s.push_str(&format!("\t\"win_x\": {},\n", self.win_x));
		s.push_str(&format!("\t\"win_y\": {},\n", self.win_y));
		s.push_str(&format!("\t\"win_w\": {},\n", self.win_w));
		s.push_str(&format!("\t\"win_h\": {}\n", self.win_h));
		s.push_str("}\n");
		s
	}
}

/// JSON 字符串字面量（含引号）：转义 ", \\ 与控制字符；其余（含中文）原样直出。
fn json_string(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('"');
	for c in s.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'\u{8}' => out.push_str("\\b"),
			'\u{c}' => out.push_str("\\f"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

/// 解析结果里我们用得到的三种值；数组/对象/浮点/null 统一记为 Other（忽略）。
#[derive(Debug, Clone, PartialEq)]
enum Raw {
	Str(String),
	Int(i64),
	Bool(bool),
	Other,
}

/// 解析一个扁平 JSON 对象；语法问题（含尾部多余内容）一律返回 None。
fn parse_flat_object(text: &str) -> Option<Vec<(String, Raw)>> {
	let chars: Vec<char> = text.chars().collect();
	let mut p = Cursor { chars: &chars, i: 0 };
	p.skip_ws();
	if !p.eat('{') {
		return None;
	}
	let mut fields: Vec<(String, Raw)> = Vec::new();
	p.skip_ws();
	if !p.eat('}') {
		loop {
			p.skip_ws();
			let key = p.parse_string()?;
			p.skip_ws();
			if !p.eat(':') {
				return None;
			}
			p.skip_ws();
			let value = p.parse_value()?;
			fields.push((key, value));
			p.skip_ws();
			if p.eat(',') {
				continue;
			}
			if p.eat('}') {
				break;
			}
			return None;
		}
	}
	p.skip_ws();
	if p.i != chars.len() {
		return None; // 尾部还有内容 = 文件损坏
	}
	Some(fields)
}

struct Cursor<'a> {
	chars: &'a [char],
	i: usize,
}

impl Cursor<'_> {
	fn peek(&self) -> Option<char> {
		self.chars.get(self.i).copied()
	}

	fn next(&mut self) -> Option<char> {
		let c = self.peek()?;
		self.i += 1;
		Some(c)
	}

	fn eat(&mut self, expected: char) -> bool {
		if self.peek() == Some(expected) {
			self.i += 1;
			true
		} else {
			false
		}
	}

	fn skip_ws(&mut self) {
		while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
			self.i += 1;
		}
	}

	fn expect_literal(&mut self, lit: &str) -> Option<()> {
		for c in lit.chars() {
			if !self.eat(c) {
				return None;
			}
		}
		Some(())
	}

	fn parse_value(&mut self) -> Option<Raw> {
		match self.peek()? {
			'"' => Some(Raw::Str(self.parse_string()?)),
			't' => {
				self.expect_literal("true")?;
				Some(Raw::Bool(true))
			}
			'f' => {
				self.expect_literal("false")?;
				Some(Raw::Bool(false))
			}
			'n' => {
				self.expect_literal("null")?;
				Some(Raw::Other)
			}
			'-' | '0'..='9' => self.parse_number(),
			'[' | '{' => {
				self.skip_balanced()?;
				Some(Raw::Other)
			}
			_ => None,
		}
	}

	/// 整数按 i64 收；带小数/指数的记 Other（配置里用不到），溢出判为损坏。
	fn parse_number(&mut self) -> Option<Raw> {
		let start = self.i;
		while matches!(self.peek(), Some('0'..='9' | '-' | '+' | '.' | 'e' | 'E')) {
			self.i += 1;
		}
		let raw: String = self.chars[start..self.i].iter().collect();
		if raw.is_empty() {
			return None;
		}
		if raw.contains(['.', 'e', 'E']) {
			return Some(Raw::Other);
		}
		Some(Raw::Int(raw.parse::<i64>().ok()?))
	}

	fn parse_string(&mut self) -> Option<String> {
		if !self.eat('"') {
			return None;
		}
		let mut out = String::new();
		loop {
			match self.next()? {
				'"' => return Some(out),
				'\\' => match self.next()? {
					'"' => out.push('"'),
					'\\' => out.push('\\'),
					'/' => out.push('/'),
					'b' => out.push('\u{8}'),
					'f' => out.push('\u{c}'),
					'n' => out.push('\n'),
					'r' => out.push('\r'),
					't' => out.push('\t'),
					'u' => out.push(self.parse_unicode_escape()?),
					_ => return None,
				},
				c if (c as u32) < 0x20 => return None, // 未转义控制字符非法
				c => out.push(c),
			}
		}
	}

	/// \uXXXX（含代理对）；孤立代理位判为损坏。
	fn parse_unicode_escape(&mut self) -> Option<char> {
		let hi = self.parse_hex4()?;
		if (0xD800..0xDC00).contains(&hi) {
			if !self.eat('\\') || !self.eat('u') {
				return None;
			}
			let lo = self.parse_hex4()?;
			if !(0xDC00..0xE000).contains(&lo) {
				return None;
			}
			char::from_u32(0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
		} else {
			char::from_u32(hi)
		}
	}

	fn parse_hex4(&mut self) -> Option<u32> {
		let mut v = 0u32;
		for _ in 0..4 {
			v = v * 16 + self.next()?.to_digit(16)?;
		}
		Some(v)
	}

	/// 跳过字符串正文（调用前开引号已被消费），正确处理转义。
	fn skip_string_body(&mut self) -> Option<()> {
		loop {
			match self.next()? {
				'"' => return Some(()),
				'\\' => {
					self.next()?;
				}
				_ => {}
			}
		}
	}

	/// 跳过一段平衡的数组/对象（字符串内的括号不计）。
	fn skip_balanced(&mut self) -> Option<()> {
		let mut depth = 0usize;
		loop {
			match self.next()? {
				'"' => {
					self.skip_string_body()?;
				}
				'{' | '[' => depth += 1,
				'}' | ']' => {
					depth = depth.checked_sub(1)?;
					if depth == 0 {
						return Some(());
					}
				}
				_ => {}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 每个测试用独立临时目录，避免互相踩踏。
	fn temp_dir(tag: &str) -> PathBuf {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_nanos())
			.unwrap_or(0);
		std::env::temp_dir().join(format!("fer-gui-test-{}-{}-{}", std::process::id(), tag, nanos))
	}

	fn sample() -> Config {
		Config {
			limit: 500,
			last_query: "报告 \"2026\"\\最终稿".to_string(),
			sort_key: "size".to_string(),
			sort_desc: true,
			win_x: -1200,
			win_y: 40,
			win_w: 1600,
			win_h: 900,
		}
	}

	#[test]
	fn round_trip_preserves_fields() {
		let c = sample();
		assert_eq!(Config::from_json_text(&c.to_json_text()), c);
		assert_eq!(Config::from_json_text(&Config::default().to_json_text()), Config::default());
	}

	#[test]
	fn escaping_and_cjk_passthrough() {
		let c = Config {
			last_query: "换行\n制表\t引号\"反斜杠\\路径C:\\Users\\中文".to_string(),
			sort_key: "名称".to_string(),
			..Config::default()
		};
		assert_eq!(Config::from_json_text(&c.to_json_text()), c);
		// 中文不转义成 \uXXXX（可读性）
		let text = c.to_json_text();
		assert!(text.contains("中文"));
		assert!(!text.contains("\\u4e2d"));
	}

	#[test]
	fn corrupt_input_falls_back_to_default() {
		for bad in [
			"",
			"   ",
			"不是 JSON",
			"{",
			"}",
			"[1, 2, 3]",
			"\"裸字符串\"",
			"null",
			"{\"limit\": 5",
			"{\"last_query\": \"未闭合}",
			"{\"limit\": 5,}",
			"{\"limit\": 5}{\"limit\": 6}",
			"{\"limit\": 5} 尾巴",
			"{\"limit\": 99999999999999999999999}",
			"{\"last_query\": \"\\ud800\"}",
			"{\"last_query\": \"\\uZZZZ\"}",
		] {
			assert_eq!(Config::from_json_text(bad), Config::default(), "输入: {bad:?}");
		}
	}

	#[test]
	fn wrong_types_fall_back_per_field() {
		let c = Config::from_json_text(
			r#"{"limit": "x", "last_query": 7, "sort_key": false, "sort_desc": "yes",
			    "win_x": 1.5, "win_y": null, "win_w": -3, "win_h": 0}"#,
		);
		assert_eq!(c, Config::default());
		// 合法的仍生效，非法的回退，未知字段忽略
		let c2 = Config::from_json_text(r#"{"limit": 42, "nope": 1, "extra": {"a": [1, 2]}, "sort_desc": true}"#);
		assert_eq!(c2.limit, 42);
		assert!(c2.sort_desc);
		assert_eq!(c2.last_query, Config::default().last_query);
		assert_eq!(c2.win_w, Config::default().win_w);
	}

	#[test]
	fn missing_file_falls_back_to_default() {
		let dir = temp_dir("missing");
		let missing = dir.join("nested").join("config.json");
		assert_eq!(Config::load_from(&missing), Config::default());
		// 传目录而非文件也不 panic
		assert_eq!(Config::load_from(&dir), Config::default());
	}

	#[test]
	fn save_creates_parent_dir_and_reloads() {
		let root = temp_dir("save");
		let file = root.join("deep").join("nested").join("config.json");
		assert!(!file.exists());
		let c = sample();
		c.save_to(&file).expect("save_to 应成功");
		assert!(file.exists(), "父目录应被自动创建");
		assert_eq!(Config::load_from(&file), c);
		// 覆盖写：先写坏内容，再保存后仍可回读
		fs::write(&file, "垃圾内容").unwrap();
		assert_eq!(Config::load_from(&file), Config::default());
		c.save_to(&file).unwrap();
		assert_eq!(Config::load_from(&file), c);
		let _ = fs::remove_dir_all(&root);
	}

	#[test]
	fn path_is_localappdata_fer_gui_config_json() {
		let dir = Config::dir();
		let path = Config::path();
		assert_eq!(path, dir.join("config.json"));
		assert_eq!(path.file_name().and_then(|s| s.to_str()), Some("config.json"));
		assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("fer-gui"));
	}
}
