//! 共享 API 模型的契约测试（默认 `#[ignore]`，需要 serve 在线）。
//!
//! 运行：
//!   cargo test --offline --test api_contract -- --ignored --nocapture
//!
//! `live_serve.rs` 只逐字段扫文本；本文件把真实响应直接反序列化成
//! `file_engine_rust::api` 的结构体。服务端与 GUI 共用同一份定义，
//! 所以字段改名、类型变化或缺字段在这里会立刻变成解析失败，
//! 而不是 GUI 侧静默回落默认值。

mod common;

use common::{http_get, serve_addr};
use file_engine_rust::api::{DuReport, FeedStatus, IndexStats, SearchResult};

fn get_json<T: serde::de::DeserializeOwned>(target: &str) -> T {
	let (status, body) = http_get(&serve_addr(), target);
	assert_eq!(status, 200, "GET {target} 状态码异常：{body}");
	serde_json::from_str(&body).unwrap_or_else(|e| panic!("{target} 不符合共享模型：{e}\n{body}"))
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test api_contract -- --ignored --nocapture"]
fn search_matches_shared_model() {
	let r: SearchResult = get_json("/api/search?q=&limit=3");
	assert!(r.ok, "ok 应为 true");
	assert!(r.error.is_none());
	assert_eq!(r.engine, "mem", "引擎标识变了？");
	assert_eq!(r.count, r.hits.len(), "count 必须等于 hits.len()");
	assert!(r.total >= r.count as u64, "total 不应小于 count");
	let hit = r.hits.first().expect("索引应有命中");
	assert!(!hit.path.is_empty(), "hit.path 为空");
	assert!(hit.flags < 32, "flags 只有低 5 位有效：{}", hit.flags);
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test api_contract -- --ignored --nocapture"]
fn stats_matches_shared_model() {
	let s: IndexStats = get_json("/api/stats");
	assert!(s.ok);
	assert!(s.entries > 0, "索引条目为 0");
	assert_eq!(s.entries, s.files + s.dirs, "entries 必须等于 files + dirs");
	assert!(!s.dump.is_empty(), "dump 路径为空");
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test api_contract -- --ignored --nocapture"]
fn feed_matches_shared_model() {
	let f: FeedStatus = get_json("/api/feed");
	assert!(f.ok);
	// 诊断字段可以全零（monitor 未推送），但字段本身必须存在且类型正确；
	// 反序列化已经保证了这一点，这里只做一次抽样。
	assert!(f.sample_append.len() <= 4, "serve 只采样 4 条 append");
}

#[test]
#[ignore = "需要 fer serve 在线：cargo test --offline --test api_contract -- --ignored --nocapture"]
fn du_matches_shared_model() {
	let d: DuReport = get_json("/api/du?path=D%3A%5CKita-Tools%5CCoding%5Cfer&depth=1&top=3");
	assert_eq!(d.root, r"D:\Kita-Tools\Coding\fer");
	assert!(d.total_bytes > 0, "du 合计为 0");
	assert!(!d.children.is_empty(), "fer 目录下应有子目录");
	let child = &d.children[0];
	assert!(!child.path.is_empty());
	assert!(child.depth >= 1);
}
