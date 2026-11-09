//! fer HTTP API 的客户端与数据模型（fer serve @ 127.0.0.1:19876）。
//!
//! 契约（冻结；字段名对应 fer README 里声明为稳定的 API）：
//!   FerClient::new(addr: &str, timeout: Duration) -> Self
//!   FerClient::{health, search, stats, feed, du, rebuild, rescan, reveal}
//!
//! 端点：
//!   GET  /api/health
//!   GET  /api/search?q=<enc>&limit=<n>[&sort=<key>&desc=1]
//!   GET  /api/stats
//!   GET  /api/du?path=<enc>[&depth=N][&top=N][&allocated=true]   （只认 true/false）
//!   GET  /api/feed
//!   POST /api/rescan | /api/rebuild
//!   POST /api/reveal?path=<enc>   （explorer /select；不得用于探测不存在的路径）
//!
//! 实现要求：所有响应经 crate::json 解析；字段缺失一律回退默认值而不得 panic；
//! ok:false 时把 error 文本带回上层。
//!
//! 实测字段（本机 serve，见 docs/api-samples.md）：
//! - search 顶层 ok/query/count/total/took_ms/engine/hits；hits[] = path/is_dir/size/
//!   allocated/mtime/ctime/flags（无 name）
//! - stats 只有 ok/entries/files/dirs/dump/dump_mb/mem_bytes；volumes/index_method/
//!   built_at_unix/feed 属 CLI 口径，缺失时回退默认值
//! - du 的 children[] 只有 path/size/allocated/files/depth（无 name，名称从 path 末段派生）

#![allow(dead_code)]

use std::time::Duration;

use crate::http::{HttpClient, HttpError, url_encode};
use crate::json::Json;

/// 一条命中记录。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hit {
	pub path: String,
	pub is_dir: bool,
	pub size: u64,
	pub allocated: u64,
	pub mtime: i64,
	pub ctime: i64,
	pub flags: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
	pub ok: bool,
	pub error: Option<String>,
	pub query: String,
	pub count: usize,
	pub total: u64,
	pub took_ms: u64,
	pub hits: Vec<Hit>,
}

#[derive(Debug, Clone, Default)]
pub struct FeedStatus {
	pub delta_entries: u64,
	pub pending_remove: u64,
	pub batches_applied: u64,
	pub saturated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
	pub ok: bool,
	pub entries: u64,
	pub files: u64,
	pub dirs: u64,
	/// dump 文件路径（serve 报的原始大小写）。
	pub dump: String,
	/// dump 文件大小（MB，serve 侧整除后的整数）。
	pub dump_mb: u64,
	/// 引擎常驻内存字节数。
	pub mem_bytes: u64,
	/// 索引覆盖的卷列表（本机 serve 不返回，缺失即空）。
	pub volumes: Vec<String>,
	/// 索引方法（CLI 口径；缺失即空串）。
	pub index_method: String,
	/// dump 构建时间（Unix 秒；缺失即 0）。
	pub built_at_unix: i64,
	/// 实时覆盖层状态（serve 的 /api/stats 不含，缺失即 None）。
	pub feed: Option<FeedStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct DuNode {
	pub name: String,
	pub path: String,
	pub size: u64,
	pub allocated: u64,
	pub files: u64,
	pub dirs: u64,
	pub depth: u32,
	pub children: Vec<DuNode>,
	pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DuResult {
	pub root: String,
	pub total_bytes: u64,
	pub total_allocated: u64,
	pub files: u64,
	pub dirs: u64,
	pub entries: u64,
	pub children: Vec<DuNode>,
	pub truncated: bool,
}

pub struct FerClient {
	http: HttpClient,
}

impl FerClient {
	pub fn new(addr: &str, timeout: Duration) -> Self {
		Self { http: HttpClient::new(addr, timeout) }
	}

	pub fn addr(&self) -> &str {
		self.http.addr()
	}

	/// GET /api/health —— serve 是否在线。
	pub fn health(&self) -> Result<bool, HttpError> {
		let body = self.http.get("/api/health")?.body;
		Ok(match Json::parse(&body) {
			Ok(v) => v.bool_at("ok").unwrap_or(false),
			// 2xx 但响应不是 JSON：视为未就绪，不当作致命错误
			Err(_) => false,
		})
	}

	/// GET /api/search?q=&limit=&sort=&desc=1
	pub fn search(
		&self,
		q: &str,
		limit: usize,
		sort: Option<&str>,
		desc: bool,
	) -> Result<SearchResult, HttpError> {
		let mut target = format!("/api/search?q={}&limit={}", url_encode(q), limit);
		if let Some(sort) = sort.filter(|s| !s.is_empty()) {
			target.push_str("&sort=");
			target.push_str(&url_encode(sort));
		}
		if desc {
			target.push_str("&desc=1");
		}
		let body = self.http.get(&target)?.body;
		parse_search(&body, q)
	}

	/// GET /api/stats
	pub fn stats(&self) -> Result<IndexStats, HttpError> {
		let body = self.http.get("/api/stats")?.body;
		parse_stats(&body)
	}

	/// GET /api/feed
	pub fn feed(&self) -> Result<FeedStatus, HttpError> {
		let body = self.http.get("/api/feed")?.body;
		parse_feed(&body)
	}

	/// GET /api/du?path=&depth=&top=&allocated=1
	pub fn du(
		&self,
		path: &str,
		depth: Option<u32>,
		top: Option<u32>,
		allocated: bool,
	) -> Result<DuResult, HttpError> {
		let mut target = format!("/api/du?path={}", url_encode(path));
		if let Some(depth) = depth {
			target.push_str(&format!("&depth={depth}"));
		}
		if let Some(top) = top {
			target.push_str(&format!("&top={top}"));
		}
		if allocated {
			// 实测：/api/du 的 allocated 由 serde 直接反序列化 bool，只认 true/false
			//（与 /api/search 的 desc 不同，后者走 parse_flag，1/0/yes/on 都行）
			target.push_str("&allocated=true");
		}
		let body = self.http.get(&target)?.body;
		parse_du(&body)
	}

	/// POST /api/rebuild（serve 转交给已提权的 monitor）。
	pub fn rebuild(&self) -> Result<(), HttpError> {
		let body = self.http.post("/api/rebuild")?.body;
		check_ack(&body)
	}

	/// POST /api/rescan（serve 进程内重建，通常需要管理员）。
	pub fn rescan(&self) -> Result<(), HttpError> {
		let body = self.http.post("/api/rescan")?.body;
		check_ack(&body)
	}

	/// POST /api/reveal —— 资源管理器定位。
	pub fn reveal(&self, path: &str) -> Result<(), HttpError> {
		let target = format!("/api/reveal?path={}", url_encode(path));
		let body = self.http.post(&target)?.body;
		check_ack(&body)
	}
}

// ---------------------------------------------------------------------------
// 解析：全部容忍缺字段，类型不符按缺失处理。

fn parse_json(body: &str, what: &str) -> Result<Json, HttpError> {
	Json::parse(body).map_err(|e| HttpError::Protocol(format!("{what} 响应解析失败: {e}")))
}

/// POST 类端点的应答（{"ok":true,...}）；ok:false 时把 error 文本转成 Protocol 错误。
fn check_ack(body: &str) -> Result<(), HttpError> {
	match Json::parse(body) {
		Ok(v) => {
			if v.bool_at("ok").unwrap_or(true) {
				Ok(())
			} else {
				Err(HttpError::Protocol(v.str_or("error", "fer 拒绝了该操作")))
			}
		}
		// 2xx 但不是 JSON：不当作失败（容忍中间层返回空体或纯文本）
		Err(_) => Ok(()),
	}
}

fn parse_search(body: &str, requested_query: &str) -> Result<SearchResult, HttpError> {
	let v = parse_json(body, "search")?;
	let hits: Vec<Hit> = v
		.arr_at("hits")
		.map(|items| items.iter().map(parse_hit).collect())
		.unwrap_or_default();
	let hit_count = hits.len();
	Ok(SearchResult {
		ok: v.bool_at("ok").unwrap_or(true),
		error: v.str_at("error").map(|s| s.to_string()),
		query: v.str_or("query", requested_query),
		count: v.u64_at("count").map_or(hit_count, |n| n as usize),
		total: v.u64_at("total").unwrap_or(0),
		took_ms: v.u64_at("took_ms").unwrap_or(0),
		hits,
	})
}

fn parse_hit(item: &Json) -> Hit {
	Hit {
		path: item.str_or("path", ""),
		is_dir: item.bool_at("is_dir").unwrap_or(false),
		size: item.u64_at("size").unwrap_or(0),
		allocated: item.u64_at("allocated").unwrap_or(0),
		mtime: item.i64_at("mtime").unwrap_or(0),
		ctime: item.i64_at("ctime").unwrap_or(0),
		flags: item.u64_at("flags").unwrap_or(0) as u32,
	}
}

fn parse_feed(body: &str) -> Result<FeedStatus, HttpError> {
	let v = parse_json(body, "feed")?;
	Ok(feed_from(&v))
}

fn feed_from(v: &Json) -> FeedStatus {
	FeedStatus {
		delta_entries: v.u64_at("delta_entries").unwrap_or(0),
		pending_remove: v.u64_at("pending_remove").unwrap_or(0),
		batches_applied: v.u64_at("batches_applied").unwrap_or(0),
		saturated: v.bool_at("saturated").unwrap_or(false),
	}
}

fn parse_stats(body: &str) -> Result<IndexStats, HttpError> {
	let v = parse_json(body, "stats")?;
	Ok(IndexStats {
		ok: v.bool_at("ok").unwrap_or(true),
		entries: v.u64_at("entries").unwrap_or(0),
		files: v.u64_at("files").unwrap_or(0),
		dirs: v.u64_at("dirs").unwrap_or(0),
		dump: v.str_or("dump", ""),
		dump_mb: v.u64_at("dump_mb").unwrap_or(0),
		mem_bytes: v.u64_at("mem_bytes").unwrap_or(0),
		volumes: v
			.arr_at("volumes")
			.map(|items| items.iter().filter_map(|s| s.as_str().map(|x| x.to_string())).collect())
			.unwrap_or_default(),
		// serve 不返回该字段；兼容 CLI 口径的别名 method
		index_method: v.str_or("index_method", &v.str_or("method", "")),
		built_at_unix: v.i64_at("built_at_unix").unwrap_or(0),
		feed: v.get("feed").map(feed_from),
	})
}

fn parse_du(body: &str) -> Result<DuResult, HttpError> {
	let v = parse_json(body, "du")?;
	Ok(DuResult {
		root: v.str_or("root", ""),
		total_bytes: v.u64_at("total_bytes").unwrap_or(0),
		total_allocated: v.u64_at("total_allocated").unwrap_or(0),
		files: v.u64_at("files").unwrap_or(0),
		dirs: v.u64_at("dirs").unwrap_or(0),
		entries: v.u64_at("entries").unwrap_or(0),
		children: v
			.arr_at("children")
			.map(|items| items.iter().map(parse_du_node).collect())
			.unwrap_or_default(),
		truncated: v.bool_at("truncated").unwrap_or(false),
	})
}

fn parse_du_node(item: &Json) -> DuNode {
	let path = item.str_or("path", "");
	// 实测 du 的 children[] 没有 name 字段：从路径末段派生（兼容 \ 与 / 分隔符）
	let name = match item.str_at("name") {
		Some(name) => name.to_string(),
		None => last_segment(&path),
	};
	DuNode {
		name,
		path,
		size: item.u64_at("size").unwrap_or(0),
		allocated: item.u64_at("allocated").unwrap_or(0),
		files: item.u64_at("files").unwrap_or(0),
		dirs: item.u64_at("dirs").unwrap_or(0),
		depth: item.u64_at("depth").unwrap_or(0) as u32,
		children: item
			.arr_at("children")
			.map(|items| items.iter().map(parse_du_node).collect())
			.unwrap_or_default(),
		truncated: item.bool_at("truncated").unwrap_or(false),
	}
}

/// 取路径末段（忽略结尾分隔符）；空路径返回空串。
fn last_segment(path: &str) -> String {
	let trimmed = path.trim_end_matches(['\\', '/']);
	trimmed
		.rsplit(['\\', '/'])
		.next()
		.unwrap_or(trimmed)
		.to_string()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Read, Write};
	use std::net::TcpListener;
	use std::thread;

	/// 桩服务器：接受一次连接、回写固定响应；返回 (地址, 收到的请求文本)。
	fn stub_raw(response: String) -> (String, thread::JoinHandle<String>) {
		let listener = TcpListener::bind("127.0.0.1:0").expect("绑定桩端口");
		let addr = listener.local_addr().expect("取本地地址").to_string();
		let handle = thread::spawn(move || {
			let (mut sock, _) = listener.accept().expect("接受连接");
			let mut req = Vec::new();
			let mut tmp = [0u8; 1024];
			loop {
				let n = sock.read(&mut tmp).expect("读请求");
				if n == 0 {
					break;
				}
				req.extend_from_slice(&tmp[..n]);
				if req.windows(4).any(|w| w == b"\r\n\r\n") {
					break;
				}
			}
			sock.write_all(response.as_bytes()).expect("写响应");
			sock.flush().expect("冲刷响应");
			String::from_utf8_lossy(&req).into_owned()
		});
		(addr, handle)
	}

	/// 桩服务器：200 + JSON 体。
	fn stub_json(body: &str) -> (String, thread::JoinHandle<String>) {
		let len = body.len();
		stub_raw(format!(
			"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {len}\r\n\r\n{body}"
		))
	}

	fn client(addr: &str) -> FerClient {
		FerClient::new(addr, Duration::from_secs(5))
	}

	// ---- 纯解析 ----

	#[test]
	fn parses_live_search_shape() {
		let body = r#"{"count":2,"engine":"mem","hits":[{"allocated":7,"ctime":1790729897,"flags":0,"is_dir":false,"mtime":1153704088,"path":"D:\\Kita-Tools\\a.rs","size":8011},{"allocated":0,"is_dir":true,"path":"D:\\Kita-Tools\\sub"}],"ok":true,"query":"ext:rs","took_ms":18,"total":32406}"#;
		let r = parse_search(body, "fallback").expect("解析应成功");
		assert!(r.ok);
		assert_eq!(r.error, None);
		assert_eq!(r.query, "ext:rs");
		assert_eq!(r.count, 2);
		assert_eq!(r.total, 32406);
		assert_eq!(r.took_ms, 18);
		assert_eq!(r.hits.len(), 2);
		assert_eq!(
			r.hits[0],
			Hit {
				path: r"D:\Kita-Tools\a.rs".to_string(),
				is_dir: false,
				size: 8011,
				allocated: 7,
				mtime: 1_153_704_088,
				ctime: 1_790_729_897,
				flags: 0,
			}
		);
		// 第二个 hit 缺 size/allocated/mtime/ctime/flags：回退默认值
		assert_eq!(r.hits[1], Hit { path: r"D:\Kita-Tools\sub".to_string(), is_dir: true, ..Default::default() });
	}

	#[test]
	fn search_missing_fields_fall_back() {
		let r = parse_search(r#"{"hits":[{},null]}"#, "req").expect("解析应成功");
		assert!(r.ok, "ok 缺失时按成功处理（HTTP 已是 2xx）");
		assert_eq!(r.query, "req", "query 缺失时回退请求词");
		assert_eq!(r.count, 2, "count 缺失时回退 hits.len()");
		assert_eq!(r.total, 0);
		assert_eq!(r.took_ms, 0);
		assert_eq!(r.hits[0], Hit::default());
		assert_eq!(r.hits[1], Hit::default());
	}

	#[test]
	fn search_error_is_carried_through() {
		let r = parse_search(r#"{"ok":false,"error":"unknown sort key x"}"#, "q").expect("解析应成功");
		assert!(!r.ok);
		assert_eq!(r.error.as_deref(), Some("unknown sort key x"));
		assert!(r.hits.is_empty());
		assert_eq!(r.count, 0);
	}

	#[test]
	fn parses_live_stats_shape() {
		let body = r#"{"dirs":527617,"dump":"C:\\Users\\x\\AppData\\Local\\file-engine-rust\\index.db.feridx","dump_mb":1474,"entries":4270030,"files":3742413,"mem_bytes":1546156084,"ok":true}"#;
		let s = parse_stats(body).expect("解析应成功");
		assert!(s.ok);
		assert_eq!(s.entries, 4_270_030);
		assert_eq!(s.files, 3_742_413);
		assert_eq!(s.dirs, 527_617);
		assert_eq!(s.dump, r"C:\Users\x\AppData\Local\file-engine-rust\index.db.feridx");
		assert_eq!(s.dump_mb, 1474);
		assert_eq!(s.mem_bytes, 1_546_156_084);
		// serve 实际不返回的字段一律回退默认值
		assert!(s.volumes.is_empty());
		assert_eq!(s.index_method, "");
		assert_eq!(s.built_at_unix, 0);
		assert!(s.feed.is_none());
	}

	#[test]
	fn parses_cli_style_stats_optional_fields() {
		let body = r#"{"ok":true,"entries":5,"volumes":["C:","D:"],"index_method":"mft","built_at_unix":1790000000,"feed":{"delta_entries":3,"pending_remove":1,"batches_applied":7,"saturated":true}}"#;
		let s = parse_stats(body).expect("解析应成功");
		assert_eq!(s.volumes, vec!["C:".to_string(), "D:".to_string()]);
		assert_eq!(s.index_method, "mft");
		assert_eq!(s.built_at_unix, 1_790_000_000);
		let feed = s.feed.expect("feed 应解析出来");
		assert_eq!(feed.delta_entries, 3);
		assert_eq!(feed.pending_remove, 1);
		assert_eq!(feed.batches_applied, 7);
		assert!(feed.saturated);
	}

	#[test]
	fn parses_live_feed_shape() {
		let body = r#"{"batches_applied":6986,"delta_entries":1259,"last_apply_age_ms":2227,"ok":true,"overlay_generation":47,"pending_append":1259,"pending_remove":148,"removed_in_dump":148,"sample_append":["D:\\a"],"sample_remove":["d:\\b"],"saturated":false}"#;
		let f = parse_feed(body).expect("解析应成功");
		assert_eq!(f.batches_applied, 6986);
		assert_eq!(f.delta_entries, 1259);
		assert_eq!(f.pending_remove, 148);
		assert!(!f.saturated);

		let empty = parse_feed("{}").expect("解析应成功");
		assert_eq!(empty.batches_applied, 0);
		assert!(!empty.saturated);
	}

	#[test]
	fn parses_live_du_shape_and_derives_name() {
		let body = r#"{"children":[{"allocated":4902397412,"depth":1,"files":11630,"path":"D:\\Kita-Tools\\Coding\\fer","size":4887028896}],"dirs":16097,"entries":108268,"files":89758,"ok":true,"root":"D:\\Kita-Tools\\Coding","took_ms":151,"total_allocated":14312089559,"total_bytes":14178791197,"truncated":true}"#;
		let d = parse_du(body).expect("解析应成功");
		assert_eq!(d.root, r"D:\Kita-Tools\Coding");
		assert_eq!(d.total_bytes, 14_178_791_197);
		assert_eq!(d.total_allocated, 14_312_089_559);
		assert_eq!(d.files, 89_758);
		assert_eq!(d.dirs, 16_097);
		assert_eq!(d.entries, 108_268);
		assert!(d.truncated);
		assert_eq!(d.children.len(), 1);
		let child = &d.children[0];
		assert_eq!(child.name, "fer", "无 name 字段时从路径末段派生");
		assert_eq!(child.path, r"D:\Kita-Tools\Coding\fer");
		assert_eq!(child.size, 4_887_028_896);
		assert_eq!(child.allocated, 4_902_397_412);
		assert_eq!(child.files, 11_630);
		assert_eq!(child.depth, 1);
		assert!(child.children.is_empty());
		assert!(!child.truncated);
	}

	#[test]
	fn last_segment_derives_names() {
		assert_eq!(last_segment(r"D:\Kita\Coding\fer"), "fer");
		assert_eq!(last_segment("D:/Kita/Coding/fer"), "fer");
		assert_eq!(last_segment("D:\\Kita\\fer\\"), "fer");
		assert_eq!(last_segment("D:\\"), "D:");
		assert_eq!(last_segment("name"), "name");
		assert_eq!(last_segment(""), "");
	}

	#[test]
	fn malformed_json_is_protocol_error() {
		assert!(matches!(parse_search("<html>", "q"), Err(HttpError::Protocol(_))));
		assert!(matches!(parse_stats(""), Err(HttpError::Protocol(_))));
		assert!(matches!(parse_feed("{oops"), Err(HttpError::Protocol(_))));
		assert!(matches!(parse_du("{"), Err(HttpError::Protocol(_))));
		// 合法 JSON 但不是对象（例如 null）：容忍，全部回退默认值
		let d = parse_du("null").expect("非对象应容忍");
		assert_eq!(d.root, "");
		assert_eq!(d.total_bytes, 0);
		assert!(d.children.is_empty());
	}

	// ---- 桩服务器：请求拼装与错误映射 ----

	#[test]
	fn search_builds_encoded_query_string() {
		let (addr, handle) = stub_json(r#"{"ok":true,"query":"中文 空格&x","count":0,"total":0,"took_ms":1,"hits":[]}"#);
		let res = client(&addr).search("中文 空格&x", 7, Some("size"), true).expect("search 应成功");
		let req = handle.join().expect("桩线程");
		assert!(
			req.starts_with("GET /api/search?q=%E4%B8%AD%E6%96%87%20%E7%A9%BA%E6%A0%BC%26x&limit=7&sort=size&desc=1 HTTP/1.1\r\n"),
			"{req}"
		);
		assert!(res.ok);
		assert_eq!(res.query, "中文 空格&x");
	}

	#[test]
	fn search_without_sort_omits_sort_and_desc() {
		let (addr, handle) = stub_json(r#"{"ok":true,"hits":[]}"#);
		let res = client(&addr).search("a b", 3, None, false).expect("search 应成功");
		let req = handle.join().expect("桩线程");
		assert!(req.starts_with("GET /api/search?q=a%20b&limit=3 HTTP/1.1\r\n"), "{req}");
		assert!(!req.contains("sort="), "{req}");
		assert!(!req.contains("desc="), "{req}");
		assert_eq!(res.query, "a b");
	}

	#[test]
	fn search_empty_sort_string_is_omitted() {
		let (addr, handle) = stub_json(r#"{"ok":true}"#);
		client(&addr).search("x", 1, Some(""), false).expect("search 应成功");
		let req = handle.join().expect("桩线程");
		assert!(!req.contains("sort="), "{req}");
	}

	#[test]
	fn stats_hits_stats_endpoint() {
		let (addr, handle) = stub_json(r#"{"ok":true,"entries":1}"#);
		let s = client(&addr).stats().expect("stats 应成功");
		assert_eq!(s.entries, 1);
		assert!(handle.join().expect("桩线程").starts_with("GET /api/stats HTTP/1.1\r\n"));
	}

	#[test]
	fn feed_hits_feed_endpoint() {
		let (addr, handle) = stub_json(r#"{"ok":true,"delta_entries":42,"saturated":true}"#);
		let f = client(&addr).feed().expect("feed 应成功");
		assert_eq!(f.delta_entries, 42);
		assert!(f.saturated);
		assert!(handle.join().expect("桩线程").starts_with("GET /api/feed HTTP/1.1\r\n"));
	}

	#[test]
	fn du_builds_query_string() {
		let (addr, handle) = stub_json(r#"{"ok":true,"root":"D:\\Kita-Tools\\Coding"}"#);
		let d = client(&addr)
			.du("D:\\Kita-Tools\\Coding", Some(1), Some(3), false)
			.expect("du 应成功");
		let req = handle.join().expect("桩线程");
		assert!(
			req.starts_with("GET /api/du?path=D%3A%5CKita-Tools%5CCoding&depth=1&top=3 HTTP/1.1\r\n"),
			"{req}"
		);
		assert!(!req.contains("allocated="), "{req}");
		assert_eq!(d.root, r"D:\Kita-Tools\Coding");
	}

	#[test]
	fn du_optional_params() {
		let (addr, handle) = stub_json(r#"{"ok":true,"allocated":true}"#);
		client(&addr).du("D:\\", None, None, true).expect("du 应成功");
		let req = handle.join().expect("桩线程");
		assert!(
			req.starts_with("GET /api/du?path=D%3A%5C&allocated=true HTTP/1.1\r\n"),
			"{req}"
		);
	}

	#[test]
	fn rebuild_and_rescan_post() {
		let (addr, handle) = stub_json(r#"{"ok":true,"message":"queued"}"#);
		client(&addr).rebuild().expect("rebuild 应成功");
		assert!(handle.join().expect("桩线程").starts_with("POST /api/rebuild HTTP/1.1\r\n"));

		let (addr, handle) = stub_json(r#"{"ok":true,"result":{"files":1}}"#);
		client(&addr).rescan().expect("rescan 应成功");
		assert!(handle.join().expect("桩线程").starts_with("POST /api/rescan HTTP/1.1\r\n"));
	}

	#[test]
	fn ack_failure_becomes_protocol_error() {
		let (addr, handle) = stub_json(r#"{"ok":false,"error":"路径不存在"}"#);
		match client(&addr).rescan() {
			Err(HttpError::Protocol(msg)) => assert!(msg.contains("路径不存在"), "{msg}"),
			other => panic!("期望 Protocol 错误，实际 {other:?}"),
		}
		handle.join().expect("桩线程");
	}

	#[test]
	fn ack_without_json_is_tolerated() {
		let (addr, handle) = stub_json("done");
		client(&addr).rebuild().expect("2xx 非 JSON 视为成功");
		handle.join().expect("桩线程");
	}

	#[test]
	fn reveal_posts_encoded_path() {
		let (addr, handle) = stub_json(r#"{"ok":true}"#);
		client(&addr).reveal("D:\\Kita-Tools\\a b.txt").expect("reveal 应成功");
		let req = handle.join().expect("桩线程");
		assert!(
			req.starts_with("POST /api/reveal?path=D%3A%5CKita-Tools%5Ca%20b.txt HTTP/1.1\r\n"),
			"{req}"
		);
	}

	#[test]
	fn health_reads_ok_flag() {
		let (addr, handle) = stub_json(r#"{"ok":true}"#);
		assert!(client(&addr).health().expect("health 应成功"));
		assert!(handle.join().expect("桩线程").starts_with("GET /api/health HTTP/1.1\r\n"));

		let (addr, handle) = stub_json(r#"{"ok":false}"#);
		assert!(!client(&addr).health().expect("health 应成功"));
		handle.join().expect("桩线程");

		// 非 JSON 的 2xx：视为未就绪而非报错
		let (addr, handle) = stub_json("up");
		assert!(!client(&addr).health().expect("health 应成功"));
		handle.join().expect("桩线程");
	}

	#[test]
	fn non_2xx_maps_to_status_error() {
		let (addr, handle) = stub_raw(
			"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\n\r\nboom".to_string(),
		);
		match client(&addr).stats() {
			Err(HttpError::Status(code, body)) => {
				assert_eq!(code, 500);
				assert_eq!(body, "boom");
			}
			other => panic!("期望 Status 错误，实际 {other:?}"),
		}
		handle.join().expect("桩线程");
	}

	// ---- 真实 serve 联调（Lead 验收时执行） ----

	/// 需要本机 fer serve 常驻在 127.0.0.1:19876：
	///   cargo test --offline -- --ignored --nocapture
	#[test]
	#[ignore = "需要本机 fer serve (127.0.0.1:19876)"]
	fn live_serve_health_search_stats_feed_du() {
		let c = FerClient::new("127.0.0.1:19876", Duration::from_secs(10));
		assert!(c.health().expect("health 请求失败"), "serve 未在线: {}", c.addr());

		let stats = c.stats().expect("stats 请求失败");
		assert!(stats.ok, "stats ok=false");
		assert!(stats.entries > 0, "索引条目为 0");
		println!(
			"[live] stats: entries={} files={} dirs={} dump_mb={} mem_bytes={} dump={}",
			stats.entries, stats.files, stats.dirs, stats.dump_mb, stats.mem_bytes, stats.dump
		);

		let feed = c.feed().expect("feed 请求失败");
		println!(
			"[live] feed: delta_entries={} pending_remove={} batches_applied={} saturated={}",
			feed.delta_entries, feed.pending_remove, feed.batches_applied, feed.saturated
		);
		assert!(!feed.saturated, "本机 feed 不应处于饱和状态");

		let search = c.search("ext:rs", 3, None, false).expect("search 请求失败");
		assert!(search.ok, "search ok=false: {:?}", search.error);
		assert!(search.total > 0, "ext:rs 应命中");
		assert_eq!(search.count, search.hits.len());
		println!(
			"[live] search ext:rs: total={} count={} took_ms={}",
			search.total, search.count, search.took_ms
		);
		if let Some(hit) = search.hits.first() {
			println!("[live] first hit: {} ({} bytes, dir={})", hit.path, hit.size, hit.is_dir);
		}

		let sorted = c.search("ext:rs", 3, Some("size"), true).expect("search(sort) 请求失败");
		assert!(sorted.ok, "sort search ok=false: {:?}", sorted.error);
		println!("[live] search ext:rs sort=size desc=1: count={}", sorted.count);

		let du = c
			.du("D:\\Kita-Tools\\Coding\\fer-gui", Some(1), Some(5), true)
			.expect("du 请求失败");
		println!(
			"[live] du {}: total_bytes={} total_allocated={} files={} dirs={} children={} truncated={}",
			du.root, du.total_bytes, du.total_allocated, du.files, du.dirs, du.children.len(), du.truncated
		);
	}
}
