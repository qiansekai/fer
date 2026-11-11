//! fer serve 真实联调测试（默认 `#[ignore]`，需要 serve 在线）。
//!
//! 运行：
//!   cargo test --offline --test live_serve -- --ignored --nocapture
//!
//! 服务地址取环境变量 `FER_GUI_SERVE_ADDR`，缺省 `127.0.0.1:19876`（与 GUI 默认一致）。
//! 本文件刻意只用 std，不 include src/http.rs / src/ferapi.rs：
//! 目的是让联调测试与 core-io 模块的内部改动解耦，同时仍以真实 serve 响应为准
//! （端点事实见 docs/gui/INTERFACES.md）。

mod common;

use common::{http_get, serve_addr};

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
	assert!(field(&body, "query").is_some(), "缺少 query 回显：{}", head(&body));
	// hit 字段集是 GUI 解析器的输入契约：任一改名都会让 GUI 静默回落默认值。
	for key in ["path", "is_dir", "size", "allocated", "mtime", "ctime", "flags"] {
		assert!(
			body.contains(&format!("\"{key}\":")),
			"hits 元素缺少字段 {key}：{}",
			head(&body)
		);
	}
	for key in ["engine", "count", "total", "took_ms"] {
		assert!(field(&body, key).is_some(), "顶层缺少字段 {key}：{}", head(&body));
	}
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

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test live_serve -- --ignored --nocapture"]
fn feed_reports_overlay_shape() {
	let addr = serve_addr();
	let (status, body) = http_get(&addr, "/api/feed");
	assert_eq!(status, 200, "GET /api/feed 状态码异常：{}", head(&body));
	assert_eq!(field(&body, "ok"), Some("true"), "ok 应为 true：{}", head(&body));
	// GUI 状态栏与 /api/feed 的四个字段一一对应，缺一个就会永远显示 0。
	for key in ["delta_entries", "pending_remove", "batches_applied", "saturated"] {
		assert!(field(&body, key).is_some(), "feed 缺少字段 {key}：{}", head(&body));
	}
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test live_serve -- --ignored --nocapture"]
fn du_returns_children_with_expected_shape() {
	let addr = serve_addr();
	let target = "/api/du?path=D%3A%5CKita-Tools%5CCoding%5Cfer&depth=1&top=3";
	let (status, body) = http_get(&addr, target);
	assert_eq!(status, 200, "GET /api/du 状态码异常：{}", head(&body));
	assert_eq!(field(&body, "ok"), Some("true"), "ok 应为 true：{}", head(&body));
	// field() 返回 JSON 原文（未反转义），所以期望值也写成转义形式。
	assert_eq!(
		field(&body, "root"),
		Some(r"D:\\Kita-Tools\\Coding\\fer"),
		"root 应为请求目录：{}",
		head(&body)
	);
	for key in ["total_bytes", "total_allocated", "files", "dirs", "entries", "truncated"] {
		assert!(field(&body, key).is_some(), "du 顶层缺少字段 {key}：{}", head(&body));
	}
	assert!(body.contains("\"children\":["), "缺少 children 数组：{}", head(&body));
	// children 无 name 字段，GUI 从 path 末段派生；这里只钉住实际存在的五个字段。
	for key in ["path", "size", "allocated", "files", "depth"] {
		assert!(
			body.contains(&format!("\"{key}\":")),
			"children 元素缺少字段 {key}：{}",
			head(&body)
		);
	}
}
