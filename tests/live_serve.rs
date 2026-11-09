//! fer serve 真实联调测试（默认 `#[ignore]`，需要 serve 在线）。
//!
//! 运行：
//!   cargo test --offline --test live_serve -- --ignored --nocapture
//!
//! 服务地址取环境变量 `FER_GUI_SERVE_ADDR`，缺省 `127.0.0.1:19876`（与 GUI 默认一致）。
//! 本文件刻意只用 std（TcpStream + 文本扫描），不 include src/http.rs / src/ferapi.rs：
//! 目的是让联调测试与 core-io 模块的内部改动解耦，同时仍以真实 serve 响应为准
//! （端点事实见 docs/INTERFACES.md）。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 与 GUI 默认一致的 serve 地址；可用环境变量覆盖。
const DEFAULT_ADDR: &str = "127.0.0.1:19876";
/// 单次请求的读写超时。
const TIMEOUT: Duration = Duration::from_secs(5);

fn serve_addr() -> String {
	std::env::var("FER_GUI_SERVE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string())
}

/// 裸 HTTP/1.1 GET（Connection: close，读到 EOF），返回 (状态码, 响应体)。
fn http_get(addr: &str, target: &str) -> (u16, String) {
	let sock = addr
		.to_socket_addrs()
		.unwrap_or_else(|e| panic!("serve 地址 {addr} 无法解析：{e}"))
		.next()
		.unwrap_or_else(|| panic!("serve 地址 {addr} 解析结果为空"));
	let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT)
		.unwrap_or_else(|e| panic!("连接 {addr} 失败（serve 是否在线？）：{e}"));
	stream.set_read_timeout(Some(TIMEOUT)).expect("设置读超时失败");
	stream.set_write_timeout(Some(TIMEOUT)).expect("设置写超时失败");
	let request = format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).expect("发送请求失败");
	let mut raw = String::new();
	stream.read_to_string(&mut raw).expect("读取响应失败");
	split_response(&raw)
}

/// 拆分响应头与响应体，并抽出状态码。
fn split_response(raw: &str) -> (u16, String) {
	let (head, body) = raw.split_once("\r\n\r\n").unwrap_or_else(|| panic!("响应缺少 header/body 分隔：{raw}"));
	let status = head
		.lines()
		.next()
		.and_then(|line| line.split_whitespace().nth(1))
		.and_then(|code| code.parse().ok())
		.unwrap_or_else(|| panic!("无法解析状态行：{head}"));
	(status, body.to_string())
}

/// 取 JSON 里 `"key":` 之后的词元（数字 / true / false / 字符串内容）；找不到返回 None。
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
	let pattern = format!("\"{key}\":");
	let start = json.find(&pattern)? + pattern.len();
	let rest = json[start..].trim_start();
	if let Some(quoted) = rest.strip_prefix('"') {
		let end = quoted.find('"')?;
		Some(&quoted[..end])
	} else {
		let end = rest.find([',', '}']).unwrap_or(rest.len());
		Some(rest[..end].trim())
	}
}

/// 取 JSON 数字字段，缺失或非数字直接失败。
fn number(json: &str, key: &str) -> u64 {
	let value = field(json, key).unwrap_or_else(|| panic!("响应缺少字段 {key}：{}", head(json)));
	value.parse().unwrap_or_else(|_| panic!("字段 {key} 不是整数：{value}"))
}

/// 失败信息里只带响应前缀，避免把整个 hits 数组刷进日志。
fn head(json: &str) -> &str {
	&json[..json.len().min(300)]
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test live_serve -- --ignored --nocapture"]
fn search_returns_hits_with_expected_shape() {
	let addr = serve_addr();
	let (status, body) = http_get(&addr, "/api/search?q=&limit=3");
	assert_eq!(status, 200, "GET /api/search 状态码异常：{}", head(&body));
	assert_eq!(field(&body, "ok"), Some("true"), "ok 应为 true：{}", head(&body));
	assert!(number(&body, "total") > 0, "索引应有命中：{}", head(&body));
	assert_eq!(number(&body, "count"), 3, "count 应等于请求的 limit：{}", head(&body));
	assert!(body.contains("\"hits\":["), "缺少 hits 数组：{}", head(&body));
	assert!(body.contains("\"path\":"), "hits 元素缺少 path 字段：{}", head(&body));
	assert!(field(&body, "query").is_some(), "缺少 query 回显：{}", head(&body));
	assert!(body.contains("\"size\":") && body.contains("\"mtime\":"), "hits 元素字段不全：{}", head(&body));
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test live_serve -- --ignored --nocapture"]
fn search_respects_limit_parameter() {
	let addr = serve_addr();
	let (status, body) = http_get(&addr, "/api/search?q=&limit=1");
	assert_eq!(status, 200, "GET /api/search?limit=1 状态码异常：{}", head(&body));
	assert_eq!(number(&body, "count"), 1, "count 应受 limit 约束：{}", head(&body));
	assert!(number(&body, "total") >= number(&body, "count"), "total 应不小于 count：{}", head(&body));
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test live_serve -- --ignored --nocapture"]
fn stats_reports_a_non_empty_index() {
	let addr = serve_addr();
	let (status, body) = http_get(&addr, "/api/stats");
	assert_eq!(status, 200, "GET /api/stats 状态码异常：{}", head(&body));
	assert_eq!(field(&body, "ok"), Some("true"), "ok 应为 true：{}", head(&body));
	assert!(number(&body, "entries") > 0, "索引条目应为正：{}", head(&body));
	assert!(number(&body, "files") > 0, "文件数应为正：{}", head(&body));
	assert!(field(&body, "dump").is_some_and(|v| !v.is_empty()), "缺少 dump 路径：{}", head(&body));
}
