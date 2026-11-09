//! 零依赖 JSON 解析器 —— fer HTTP API 响应的唯一解析入口。
//!
//! 接口契约（冻结，可内部重构；main.rs 只依赖下列签名）：
//!   Json::parse(&str) -> Result<Json, JsonError>
//!   Json::{get, as_str, as_u64, as_i64, as_f64, as_bool, as_arr}
//!   Json::{str_at, u64_at, i64_at, bool_at, arr_at, arr_len, is_null, str_or}
//!
//! 实现要求：完整覆盖 RFC 8259 —— 字符串转义（引号 / 反斜杠 / 斜杠 / b f n r t /
//! uXXXX 含代理对）、数字（整数/小数/指数/负号）、嵌套数组与对象、空白容忍；
//! 合法输入必须解析成功，非法输入必须返回带偏移量的 JsonError 而不得 panic。
//! 深嵌套有层级上限（MAX_DEPTH），避免递归爆栈 —— 栈溢出不可捕获，等同进程 abort。

#![allow(dead_code)]

use std::fmt;

/// 递归解析的最大嵌套层级。真实 API 响应层级为个位数；上百层的恶意输入
/// 若不加限制会把递归栈打爆。
const MAX_DEPTH: usize = 128;

/// JSON 值。对象用 Vec 保序（fer 的字段顺序稳定，便于断言）。
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
	Null,
	Bool(bool),
	Num(f64),
	Str(String),
	Arr(Vec<Json>),
	Obj(Vec<(String, Json)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonError {
	pub message: String,
	pub pos: usize,
}

impl fmt::Display for JsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "JSON 解析错误 @{}: {}", self.pos, self.message)
	}
}

impl std::error::Error for JsonError {}

impl Json {
	/// 解析一段 JSON 文本；非法输入返回带字节偏移的 JsonError，绝不 panic。
	pub fn parse(text: &str) -> Result<Json, JsonError> {
		let mut p = Parser { text, pos: 0 };
		p.skip_ws();
		if p.eof() {
			return Err(p.err("输入为空"));
		}
		let value = p.parse_value(0)?;
		p.skip_ws();
		if !p.eof() {
			return Err(p.err("值之后存在多余字符"));
		}
		Ok(value)
	}

	pub fn get(&self, key: &str) -> Option<&Json> {
		match self {
			Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
			_ => None,
		}
	}

	pub fn as_str(&self) -> Option<&str> {
		match self {
			Json::Str(s) => Some(s),
			_ => None,
		}
	}

	pub fn as_u64(&self) -> Option<u64> {
		match self {
			Json::Num(n) if n.is_finite() && *n >= 0.0 => Some(*n as u64),
			_ => None,
		}
	}

	pub fn as_i64(&self) -> Option<i64> {
		match self {
			Json::Num(n) if n.is_finite() => Some(*n as i64),
			_ => None,
		}
	}

	pub fn as_f64(&self) -> Option<f64> {
		match self {
			Json::Num(n) => Some(*n),
			_ => None,
		}
	}

	pub fn as_bool(&self) -> Option<bool> {
		match self {
			Json::Bool(b) => Some(*b),
			_ => None,
		}
	}

	pub fn as_arr(&self) -> Option<&[Json]> {
		match self {
			Json::Arr(a) => Some(a),
			_ => None,
		}
	}

	pub fn arr_len(&self) -> usize {
		self.as_arr().map(|a| a.len()).unwrap_or(0)
	}

	pub fn is_null(&self) -> bool {
		matches!(self, Json::Null)
	}

	// ---- 便捷访问：字段缺失/类型不符时返回 None（不 panic） ----

	pub fn str_at(&self, key: &str) -> Option<&str> {
		self.get(key).and_then(|v| v.as_str())
	}

	pub fn u64_at(&self, key: &str) -> Option<u64> {
		self.get(key).and_then(|v| v.as_u64())
	}

	pub fn i64_at(&self, key: &str) -> Option<i64> {
		self.get(key).and_then(|v| v.as_i64())
	}

	pub fn bool_at(&self, key: &str) -> Option<bool> {
		self.get(key).and_then(|v| v.as_bool())
	}

	pub fn arr_at(&self, key: &str) -> Option<&[Json]> {
		self.get(key).and_then(|v| v.as_arr())
	}

	/// 字符串字段，缺失时返回 default。
	pub fn str_or(&self, key: &str, default: &str) -> String {
		self.str_at(key).unwrap_or(default).to_string()
	}
}

// ---------------------------------------------------------------------------
// 递归下降解析器

struct Parser<'a> {
	text: &'a str,
	pos: usize,
}

impl<'a> Parser<'a> {
	fn bytes(&self) -> &'a [u8] {
		self.text.as_bytes()
	}

	fn eof(&self) -> bool {
		self.pos >= self.text.len()
	}

	fn peek(&self) -> Option<u8> {
		self.bytes().get(self.pos).copied()
	}

	fn bump(&mut self) {
		self.pos += 1;
	}

	fn err(&self, message: impl Into<String>) -> JsonError {
		JsonError { message: message.into(), pos: self.pos }
	}

	fn err_at(&self, pos: usize, message: impl Into<String>) -> JsonError {
		JsonError { message: message.into(), pos }
	}

	/// 当前位置的字符，用于错误提示（pos 恒落在字符边界上）。
	fn current(&self) -> String {
		match self.text[self.pos..].chars().next() {
			Some(c) => format!("「{c}」"),
			None => "输入结束".to_string(),
		}
	}

	fn skip_ws(&mut self) {
		while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
			self.bump();
		}
	}

	fn expect(&mut self, byte: u8, what: &str) -> Result<(), JsonError> {
		if self.peek() == Some(byte) {
			self.bump();
			Ok(())
		} else {
			let cur = self.current();
			Err(self.err(format!("期望{what}，实际遇到 {cur}")))
		}
	}

	fn parse_value(&mut self, depth: usize) -> Result<Json, JsonError> {
		self.skip_ws();
		match self.peek() {
			Some(b'{') => self.parse_object(depth),
			Some(b'[') => self.parse_array(depth),
			Some(b'"') => Ok(Json::Str(self.parse_string()?)),
			Some(b't') => self.parse_literal("true", Json::Bool(true)),
			Some(b'f') => self.parse_literal("false", Json::Bool(false)),
			Some(b'n') => self.parse_literal("null", Json::Null),
			Some(c) if c == b'-' || c.is_ascii_digit() => Ok(Json::Num(self.parse_number()?)),
			Some(_) => {
				let cur = self.current();
				Err(self.err(format!("不是合法的 JSON 值起始字符：{cur}")))
			}
			None => Err(self.err("期望一个 JSON 值，实际输入结束")),
		}
	}

	fn parse_literal(&mut self, literal: &str, value: Json) -> Result<Json, JsonError> {
		if self.text[self.pos..].starts_with(literal) {
			self.pos += literal.len();
			Ok(value)
		} else {
			Err(self.err(format!("非法字面量，期望 {literal}")))
		}
	}

	/// RFC 8259 数字：-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?
	fn parse_number(&mut self) -> Result<f64, JsonError> {
		let start = self.pos;
		if self.peek() == Some(b'-') {
			self.bump();
		}
		match self.peek() {
			Some(b'0') => self.bump(),
			Some(c) if c.is_ascii_digit() => {
				while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
					self.bump();
				}
			}
			_ => return Err(self.err("数字缺少整数部分")),
		}
		if self.peek() == Some(b'.') {
			self.bump();
			if !self.eat_digits() {
				return Err(self.err("小数点后缺少数字"));
			}
		}
		if matches!(self.peek(), Some(b'e' | b'E')) {
			self.bump();
			if matches!(self.peek(), Some(b'+' | b'-')) {
				self.bump();
			}
			if !self.eat_digits() {
				return Err(self.err("指数部分缺少数字"));
			}
		}
		let raw = &self.text[start..self.pos];
		raw.parse::<f64>()
			.map_err(|_| self.err_at(start, format!("数字无法解析：{raw}")))
	}

	fn eat_digits(&mut self) -> bool {
		let start = self.pos;
		while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
			self.bump();
		}
		self.pos > start
	}

	fn parse_string(&mut self) -> Result<String, JsonError> {
		self.expect(b'"', "字符串起始引号")?;
		let mut out = String::new();
		loop {
			let Some(byte) = self.peek() else {
				return Err(self.err("字符串未闭合"));
			};
			match byte {
				b'"' => {
					self.bump();
					return Ok(out);
				}
				b'\\' => {
					self.bump();
					self.parse_escape(&mut out)?;
				}
				b if b < 0x20 => return Err(self.err("字符串中出现未转义的控制字符")),
				b if b < 0x80 => {
					out.push(b as char);
					self.bump();
				}
				_ => {
					// 非 ASCII 首字节：按字符边界整体推进（输入是合法 UTF-8 的 &str）。
					let Some(c) = self.text[self.pos..].chars().next() else {
						return Err(self.err("字符串包含非法 UTF-8"));
					};
					out.push(c);
					self.pos += c.len_utf8();
				}
			}
		}
	}

	fn parse_escape(&mut self, out: &mut String) -> Result<(), JsonError> {
		let Some(esc) = self.peek() else {
			return Err(self.err("转义序列未结束"));
		};
		self.bump();
		match esc {
			b'"' => out.push('"'),
			b'\\' => out.push('\\'),
			b'/' => out.push('/'),
			b'b' => out.push('\u{8}'),
			b'f' => out.push('\u{c}'),
			b'n' => out.push('\n'),
			b'r' => out.push('\r'),
			b't' => out.push('\t'),
			b'u' => {
				let hi = self.parse_hex4()?;
				let code = if (0xD800..=0xDBFF).contains(&hi) {
					// 高位代理：必须紧跟 \uXXXX 形式的低位代理（RFC 8259 §7）
					if self.peek() == Some(b'\\') && self.bytes().get(self.pos + 1) == Some(&b'u') {
						self.pos += 2;
						let lo = self.parse_hex4()?;
						if !(0xDC00..=0xDFFF).contains(&lo) {
							return Err(self.err("\\u 高位代理之后的码点不是低位代理"));
						}
						0x1_0000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00)
					} else {
						return Err(self.err("\\u 高位代理缺少配对的低位代理"));
					}
				} else if (0xDC00..=0xDFFF).contains(&hi) {
					return Err(self.err("\\u 出现孤立的低位代理"));
				} else {
					hi as u32
				};
				let Some(c) = char::from_u32(code) else {
					return Err(self.err("\\u 码点非法"));
				};
				out.push(c);
			}
			other => {
				return Err(self.err(format!("未知转义序列 \\{}", other as char)));
			}
		}
		Ok(())
	}

	fn parse_hex4(&mut self) -> Result<u16, JsonError> {
		let start = self.pos;
		let mut value: u16 = 0;
		for _ in 0..4 {
			let Some(byte) = self.peek() else {
				return Err(self.err_at(start, "\\u 转义缺少 4 位十六进制数字"));
			};
			let Some(digit) = (byte as char).to_digit(16) else {
				return Err(self.err("\\u 转义包含非十六进制字符"));
			};
			value = value * 16 + digit as u16;
			self.bump();
		}
		Ok(value)
	}

	fn parse_array(&mut self, depth: usize) -> Result<Json, JsonError> {
		if depth >= MAX_DEPTH {
			return Err(self.err("嵌套层级过深"));
		}
		self.expect(b'[', "数组起始左方括号")?;
		let mut items = Vec::new();
		self.skip_ws();
		if self.peek() == Some(b']') {
			self.bump();
			return Ok(Json::Arr(items));
		}
		loop {
			items.push(self.parse_value(depth + 1)?);
			self.skip_ws();
			match self.peek() {
				Some(b',') => {
					self.bump();
				}
				Some(b']') => {
					self.bump();
					return Ok(Json::Arr(items));
				}
				_ => {
					let cur = self.current();
					return Err(self.err(format!("数组中期望逗号或右方括号，实际遇到 {cur}")));
				}
			}
		}
	}

	fn parse_object(&mut self, depth: usize) -> Result<Json, JsonError> {
		if depth >= MAX_DEPTH {
			return Err(self.err("嵌套层级过深"));
		}
		self.expect(b'{', "对象起始左花括号")?;
		let mut fields: Vec<(String, Json)> = Vec::new();
		self.skip_ws();
		if self.peek() == Some(b'}') {
			self.bump();
			return Ok(Json::Obj(fields));
		}
		loop {
			self.skip_ws();
			let key = self.parse_string()?;
			self.skip_ws();
			self.expect(b':', "键值分隔冒号")?;
			let value = self.parse_value(depth + 1)?;
			fields.push((key, value));
			self.skip_ws();
			match self.peek() {
				Some(b',') => {
					self.bump();
				}
				Some(b'}') => {
					self.bump();
					return Ok(Json::Obj(fields));
				}
				_ => {
					let cur = self.current();
					return Err(self.err(format!("对象中期望逗号或右花括号，实际遇到 {cur}")));
				}
			}
		}
	}
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;

	fn parse_ok(text: &str) -> Json {
		match Json::parse(text) {
			Ok(v) => v,
			Err(e) => panic!("应解析成功: {text:?} -> {e}"),
		}
	}

	fn parse_err(text: &str) -> JsonError {
		match Json::parse(text) {
			Ok(v) => panic!("应解析失败: {text:?} -> {v:?}"),
			Err(e) => e,
		}
	}

	#[test]
	fn parses_scalars() {
		assert_eq!(parse_ok("null"), Json::Null);
		assert_eq!(parse_ok("true"), Json::Bool(true));
		assert_eq!(parse_ok("false"), Json::Bool(false));
		assert_eq!(parse_ok("  null  "), Json::Null);
		assert_eq!(parse_ok("\r\n\ttrue\t"), Json::Bool(true));
	}

	#[test]
	fn parses_numbers() {
		assert_eq!(parse_ok("0"), Json::Num(0.0));
		assert_eq!(parse_ok("-0"), Json::Num(0.0));
		assert_eq!(parse_ok("42"), Json::Num(42.0));
		assert_eq!(parse_ok("-17"), Json::Num(-17.0));
		assert_eq!(parse_ok("3.5"), Json::Num(3.5));
		assert_eq!(parse_ok("-0.25"), Json::Num(-0.25));
		assert_eq!(parse_ok("1e3"), Json::Num(1000.0));
		assert_eq!(parse_ok("1E+3"), Json::Num(1000.0));
		assert_eq!(parse_ok("1e-3"), Json::Num(0.001));
		assert_eq!(parse_ok("2.5e-3"), Json::Num(0.0025));
		assert_eq!(parse_ok("1546156084"), Json::Num(1_546_156_084.0));
	}

	#[test]
	fn parses_strings_and_escapes() {
		assert_eq!(parse_ok("\"\""), Json::Str(String::new()));
		assert_eq!(parse_ok("\"hello\""), Json::Str("hello".to_string()));
		assert_eq!(parse_ok("\"a\\\"b\""), Json::Str("a\"b".to_string()));
		assert_eq!(parse_ok("\"a\\\\b\""), Json::Str("a\\b".to_string()));
		assert_eq!(parse_ok("\"a\\/b\""), Json::Str("a/b".to_string()));
		assert_eq!(parse_ok("\"\\b\\f\\n\\r\\t\""), Json::Str("\u{8}\u{c}\n\r\t".to_string()));
		assert_eq!(parse_ok("\"\\u0041\\u006a\""), Json::Str("Aj".to_string()));
		assert_eq!(parse_ok("\"\\u4e2d\\u6587\""), Json::Str("中文".to_string()));
		assert_eq!(parse_ok("\"直接写中文\""), Json::Str("直接写中文".to_string()));
	}

	#[test]
	fn parses_surrogate_pairs() {
		// U+1F600 的 UTF-16 代理对（大小写十六进制都要认）
		assert_eq!(parse_ok("\"\\uD83D\\uDE00\""), Json::Str("😀".to_string()));
		assert_eq!(parse_ok("\"\\ud83d\\ude00\""), Json::Str("😀".to_string()));
		assert_eq!(parse_ok("\"😀\""), Json::Str("😀".to_string()));
		// 代理对与普通字符混合
		assert_eq!(parse_ok("\"a\\uD83D\\uDE00b\""), Json::Str("a😀b".to_string()));
	}

	#[test]
	fn parses_containers() {
		assert_eq!(parse_ok("[]"), Json::Arr(vec![]));
		assert_eq!(parse_ok("{}"), Json::Obj(vec![]));
		assert_eq!(
			parse_ok("[1,true,null]"),
			Json::Arr(vec![Json::Num(1.0), Json::Bool(true), Json::Null])
		);
		let nested = parse_ok("{\"a\":1,\"b\":[2,{\"c\":\"x\"}]}");
		let expected = Json::Obj(vec![
			("a".to_string(), Json::Num(1.0)),
			(
				"b".to_string(),
				Json::Arr(vec![
					Json::Num(2.0),
					Json::Obj(vec![("c".to_string(), Json::Str("x".to_string()))]),
				]),
			),
		]);
		assert_eq!(nested, expected);
	}

	#[test]
	fn tolerates_whitespace() {
		let v = parse_ok(" { \"a\" : [ 1 , 2 ] } ");
		assert_eq!(v.arr_at("a").map(|a| a.len()), Some(2));
		assert_eq!(parse_ok("\r\n\t {\n}\r\n"), Json::Obj(vec![]));
	}

	#[test]
	fn object_keeps_field_order_and_duplicates() {
		let v = parse_ok("{\"b\":1,\"a\":2,\"b\":3}");
		let keys: Vec<&str> = match &v {
			Json::Obj(fields) => fields.iter().map(|(k, _)| k.as_str()).collect(),
			_ => Vec::new(),
		};
		assert_eq!(keys, vec!["b", "a", "b"]);
		assert_eq!(v.u64_at("b"), Some(1)); // get 取首个匹配
	}

	#[test]
	fn accessors_are_tolerant() {
		let v = parse_ok("{\"s\":\"x\",\"n\":-5,\"u\":7,\"b\":true,\"a\":[1,2,3],\"z\":null,\"f\":1.5}");
		assert_eq!(v.str_at("s"), Some("x"));
		assert_eq!(v.str_at("n"), None); // 类型不符
		assert_eq!(v.str_at("missing"), None);
		assert_eq!(v.u64_at("u"), Some(7));
		assert_eq!(v.u64_at("n"), None); // 负数不是 u64
		assert_eq!(v.i64_at("n"), Some(-5));
		assert_eq!(v.bool_at("b"), Some(true));
		assert_eq!(v.arr_at("a").map(|a| a.len()), Some(3));
		assert_eq!(v.arr_len(), 0); // 非数组
		assert_eq!(v.get("z").map(|j| j.is_null()), Some(true));
		assert!(v.get("missing").is_none());
		assert_eq!(v.str_or("s", "def"), "x");
		assert_eq!(v.str_or("missing", "def"), "def");
		assert_eq!(v.as_f64(), None);
		assert_eq!(parse_ok("2.5").as_f64(), Some(2.5));
		assert_eq!(parse_ok("3").as_i64(), Some(3));
		assert_eq!(parse_ok("\"x\"").as_str(), Some("x"));
		assert_eq!(parse_ok("[]").as_arr().map(|a| a.len()), Some(0));
		assert!(!parse_ok("1").is_null());
		assert!(parse_ok("null").is_null());
	}

	#[test]
	fn rejects_malformed_input() {
		let bad_inputs = [
			"",
			"   ",
			"{",
			"[",
			"[1,",
			"[1,]",
			"{\"a\"}",
			"{\"a\":}",
			"{\"a\":1,}",
			"{a:1}",
			"{\"a\":1 \"b\":2}",
			"tru",
			"nul",
			"True",
			"[1 2]",
			"\"abc",
			"\"a\\qb\"",
			"\"a\\u12\"",
			"\"a\\uZZZZ\"",
			"\"\\uD83D\"",
			"\"\\uDE00\"",
			"01",
			"1.",
			"1e",
			"1e+",
			"-",
			"+1",
			".5",
			"1.2.3",
			"null x",
			"{}\n{}",
			"[1,2",
			"{\"a\":1",
			"\"a\nb\"",
			"\"a\u{1}b\"",
		];
		for bad in bad_inputs {
			let err = parse_err(bad);
			assert!(!err.message.is_empty(), "错误信息不应为空: {bad:?}");
		}
	}

	#[test]
	fn errors_carry_position() {
		let e = parse_err("{\"a\":1,}");
		assert_eq!(e.pos, 7);
		assert!(e.message.contains("字符串起始引号"), "{}", e.message);

		let e2 = parse_err("[1,true,]");
		assert_eq!(e2.pos, 8);

		let e3 = parse_err("{\"a\":1} trailing");
		assert_eq!(e3.pos, 8);
		assert!(e3.message.contains("多余字符"), "{}", e3.message);

		let e4 = parse_err("");
		assert_eq!(e4.pos, 0);

		let e5 = parse_err("{\"a\": 1e}");
		assert!(e5.pos >= 7, "pos={}", e5.pos);
	}

	#[test]
	fn deep_nesting_is_rejected_without_crashing() {
		let too_deep = "[".repeat(MAX_DEPTH + 16);
		let err = parse_err(&too_deep);
		assert!(err.message.contains("嵌套层级过深"), "{}", err.message);

		let nested_objects = "{\"a\":".repeat(MAX_DEPTH + 16);
		assert!(parse_err(&nested_objects).message.contains("嵌套层级过深"));

		// 合法深度照样解析
		let ok_text = "[".repeat(64) + &"]".repeat(64);
		assert!(Json::parse(&ok_text).is_ok());
	}

	#[test]
	fn parses_real_api_sample() {
		let text = r#"{"count":2,"engine":"mem","hits":[{"allocated":7,"ctime":1790729897,"flags":0,"is_dir":false,"mtime":1790729897,"path":"D:\\Kita-Tools\\a.rs","size":7}],"ok":true,"query":"cargo","took_ms":34,"total":5102}"#;
		let v = parse_ok(text);
		assert_eq!(v.bool_at("ok"), Some(true));
		assert_eq!(v.str_at("engine"), Some("mem"));
		assert_eq!(v.u64_at("total"), Some(5102));
		assert_eq!(v.u64_at("count"), Some(2));
		assert_eq!(v.u64_at("took_ms"), Some(34));
		let hits = v.arr_at("hits").expect("hits 是数组");
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].str_at("path"), Some(r"D:\Kita-Tools\a.rs"));
		assert_eq!(hits[0].u64_at("size"), Some(7));
		assert_eq!(hits[0].bool_at("is_dir"), Some(false));
		assert_eq!(hits[0].u64_at("allocated"), Some(7));
		assert!(hits[0].get("missing").is_none());
	}
}
