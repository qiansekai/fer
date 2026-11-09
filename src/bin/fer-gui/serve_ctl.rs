//! fer serve 探测与自动拉起（仅用 std，刻意不依赖 crate::http / crate::ferapi，避免耦合）。
//!
//! 契约（Lead 冻结，清单见 docs/INTERFACES.md）：
//!   ServeStatus { addr, online, fer_path, detail }
//!   probe(addr, timeout) -> ServeStatus
//!   find_fer_exe() -> Option<PathBuf>
//!   find_in_path(name) -> Option<PathBuf>
//!   serve_argv(fer, addr) -> Vec<String>
//!   spawn_serve(fer, addr) -> io::Result<u32>
//!   ensure_serve(addr, timeout, wait_ms) -> ServeStatus
//!
//! 语义要点：
//!   - probe 只做一次 GET /api/health；超时 / 拒绝 / 非 200 / 响应不像 fer 一律 online = false，
//!     并把人类可读原因写进 detail（状态栏直接显示）。
//!   - fer_path 在离线时同样会被填写，UI 据此决定是否给出「一键启动」。
//!   - ensure_serve 先 probe；未在线且有 fer.exe 才拉起，随后轮询到 wait_ms 上限为止。
//!   - 全程不 panic：地址解析、连接、读写失败一律降级为 Result/Option。
//!   - 本模块是 Windows 专用（项目本身即 winsafe/Win32 GUI），spawn 走 CREATE_NO_WINDOW。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 存活探针端点（fer 返回 {"ok":true}）。
const HEALTH_TARGET: &str = "/api/health";

/// fer 可执行文件名。
const FER_EXE_NAME: &str = "fer.exe";

/// fer 的默认程序/索引目录名（%LOCALAPPDATA%\file-engine-rust）。
const FER_DATA_DIR: &str = "file-engine-rust";

/// 环境变量查找顺序：先 GUI 专用，再通用 FER。
const FER_ENV_VARS: [&str; 2] = ["FER_GUI_FER", "FER"];

/// ensure_serve 的轮询间隔（题面要求 200-300ms）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// CREATE_NO_WINDOW：拉起后台 serve 时不闪控制台窗口。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// serve 探测结果（UI 状态栏直接消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeStatus {
	/// 被探测的地址，形如 127.0.0.1:19876。
	pub addr: String,
	/// /api/health 是否确认可用。
	pub online: bool,
	/// 本机 fer.exe 路径（找不到为 None）；离线时用于提示能否一键启动。
	pub fer_path: Option<String>,
	/// 人类可读的状态说明，可直接显示在状态栏。
	pub detail: String,
}

impl ServeStatus {
	/// 统一构造，避免各处字段顺序写错。
	fn new(addr: &str, online: bool, fer_path: Option<String>, detail: String) -> Self {
		Self { addr: addr.to_string(), online, fer_path, detail }
	}
}

/// 探测 addr 上的 fer serve 是否在线（一次 GET /api/health，不重试）。
///
/// timeout 同时用作连接与读写超时；任何失败都返回 online = false 并附带原因。
pub fn probe(addr: &str, timeout: Duration) -> ServeStatus {
	let fer_path = find_fer_exe().map(|path| path.display().to_string());
	match health_body(addr, timeout) {
		Ok(body) if body_looks_healthy(&body) => {
			ServeStatus::new(addr, true, fer_path, format!("serve 在线（{addr}）"))
		}
		Ok(body) => ServeStatus::new(
			addr,
			false,
			fer_path,
			format!("{addr} 有 HTTP 响应，但 /api/health 不含 ok:true：{}", truncate(&body, 80)),
		),
		Err(reason) => ServeStatus::new(addr, false, fer_path, format!("serve 未连接（{reason}）")),
	}
}

/// 在 PATH 的各目录里查找名为 name 的现有文件；找不到返回 None。
pub fn find_in_path(name: &str) -> Option<PathBuf> {
	let path = std::env::var_os("PATH")?;
	for dir in std::env::split_paths(&path) {
		if dir.as_os_str().is_empty() {
			continue;
		}
		let candidate = dir.join(name);
		if candidate.is_file() {
			return Some(candidate);
		}
	}
	None
}

/// 定位本机 fer.exe，顺序：FER_GUI_FER -> FER -> PATH -> %LOCALAPPDATA%\file-engine-rust\fer.exe。
///
/// 环境变量指向目录时，会在该目录下继续找 fer.exe；指向不存在的文件则视为未命中，继续下一条。
pub fn find_fer_exe() -> Option<PathBuf> {
	for var in FER_ENV_VARS {
		if let Some(value) = std::env::var_os(var) {
			let candidate = PathBuf::from(value);
			if candidate.is_file() {
				return Some(candidate);
			}
			if candidate.is_dir() {
				let nested = candidate.join(FER_EXE_NAME);
				if nested.is_file() {
					return Some(nested);
				}
			}
		}
	}
	if let Some(found) = find_in_path(FER_EXE_NAME) {
		return Some(found);
	}
	local_default_fer_path()
}

/// %LOCALAPPDATA%\file-engine-rust\fer.exe（fer 的默认数据目录）。
fn local_default_fer_path() -> Option<PathBuf> {
	let base = std::env::var_os("LOCALAPPDATA")?;
	let candidate = PathBuf::from(base).join(FER_DATA_DIR).join(FER_EXE_NAME);
	if candidate.is_file() { Some(candidate) } else { None }
}

/// 构造 serve 子命令的参数（不含 argv[0]：可执行文件由 Command::new(fer) 提供）。
///
/// 形参 fer 保留在签名中，让调用方在一个地方拿到「程序 + 参数」的对应关系；
/// 单测可直接断言本函数返回值，从而在不起进程的前提下验证命令行。
pub fn serve_argv(_fer: &Path, addr: &str) -> Vec<String> {
	vec!["serve".to_string(), "--no-warm".to_string(), "--addr".to_string(), addr.to_string()]
}

/// 以不弹窗方式拉起 fer serve，返回子进程 pid（stdout/stderr 丢弃，标准输入置空）。
pub fn spawn_serve(fer: &Path, addr: &str) -> std::io::Result<u32> {
	let child = Command::new(fer)
		.args(serve_argv(fer, addr))
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.creation_flags(CREATE_NO_WINDOW)
		.spawn()?;
	Ok(child.id())
}

/// 确保 serve 可用：先探测，离线且有 fer.exe 时拉起并轮询等待（总等待不超过 wait_ms）。
///
/// 本函数会阻塞（最长约 wait_ms），应由后台线程调用；无论如何都返回最终状态而不 panic。
pub fn ensure_serve(addr: &str, timeout: Duration, wait_ms: u64) -> ServeStatus {
	let first = probe(addr, timeout);
	if first.online {
		return first;
	}

	let Some(fer) = find_fer_exe() else {
		return ServeStatus::new(
			addr,
			false,
			None,
			format!(
				"serve 未连接，且未找到 fer.exe（已查 FER_GUI_FER / FER / PATH / %LOCALAPPDATA%\\{FER_DATA_DIR}）：{}",
				first.detail
			),
		);
	};
	let fer_path = Some(fer.display().to_string());

	let pid = match spawn_serve(&fer, addr) {
		Ok(pid) => pid,
		Err(err) => {
			return ServeStatus::new(
				addr,
				false,
				fer_path,
				format!("serve 未连接，拉起 fer serve 失败：{err}"),
			)
		}
	};

	let deadline = Instant::now() + Duration::from_millis(wait_ms);
	loop {
		let status = probe(addr, timeout);
		if status.online {
			return ServeStatus::new(
				addr,
				true,
				fer_path,
				format!("已拉起 fer serve（pid {pid}）并连接成功"),
			);
		}
		let now = Instant::now();
		if now >= deadline {
			return ServeStatus::new(
				addr,
				false,
				fer_path,
				format!("已启动 fer serve（pid {pid}），但 {wait_ms} ms 内未就绪：{}", status.detail),
			);
		}
		// 睡到下一个轮询点，但不超过剩余等待时间。
		std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
	}
}

/// 发一次 GET /api/health 并返回响应体；非 200 视为失败。
fn health_body(addr: &str, timeout: Duration) -> Result<String, String> {
	let socket = addr
		.to_socket_addrs()
		.map_err(|err| format!("地址 {addr} 无法解析：{err}"))?
		.next()
		.ok_or_else(|| format!("地址 {addr} 解析结果为空"))?;

	let mut stream = TcpStream::connect_timeout(&socket, timeout)
		.map_err(|err| format!("连接 {addr} 失败：{err}"))?;
	stream
		.set_read_timeout(Some(timeout))
		.map_err(|err| format!("设置读超时失败：{err}"))?;
	stream
		.set_write_timeout(Some(timeout))
		.map_err(|err| format!("设置写超时失败：{err}"))?;

	let request = format!("GET {HEALTH_TARGET} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).map_err(|err| format!("发送请求失败：{err}"))?;

	let mut raw = String::new();
	stream.read_to_string(&mut raw).map_err(|err| format!("读取响应失败：{err}"))?;

	let (status, body) = split_response(&raw)?;
	if status != 200 {
		return Err(format!("HTTP {status}"));
	}
	Ok(body)
}

/// 拆分 HTTP 响应头/体并抽出状态码。
fn split_response(raw: &str) -> Result<(u16, String), String> {
	let (head, body) = raw
		.split_once("\r\n\r\n")
		.ok_or_else(|| "响应缺少 header/body 分隔".to_string())?;
	let status = head
		.lines()
		.next()
		.and_then(|line| line.split_whitespace().nth(1))
		.and_then(|code| code.parse::<u16>().ok())
		.ok_or_else(|| "状态行无法解析".to_string())?;
	Ok((status, body.to_string()))
}

/// 响应体是否像 fer 的存活探针（容忍空白，要求 ok 为 true）。
fn body_looks_healthy(body: &str) -> bool {
	let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
	compact.contains("\"ok\":true")
}

/// 截断长文本用于状态栏提示（超长补省略号）。
fn truncate(text: &str, max_chars: usize) -> String {
	let trimmed = text.trim();
	let chars: Vec<char> = trimmed.chars().collect();
	if chars.len() <= max_chars {
		return trimmed.to_string();
	}
	let mut out: String = chars[..max_chars].iter().collect();
	out.push('…');
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 本机 serve 默认地址（生产侧地址由 main.rs 决定，这里只作联调目标）。
	const DEFAULT_ADDR: &str = "127.0.0.1:19876";

	/// 挑一个「刚刚被释放、当前无人监听」的本地端口。
	fn unlistened_addr() -> String {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口失败");
		let addr = listener.local_addr().expect("读取临时端口失败");
		drop(listener);
		addr.to_string()
	}

	#[test]
	fn serve_argv_matches_frozen_contract() {
		let argv = serve_argv(Path::new("C:\\tools\\fer.exe"), DEFAULT_ADDR);
		let expected: Vec<String> =
			["serve", "--no-warm", "--addr", DEFAULT_ADDR].iter().map(|s| s.to_string()).collect();
		assert_eq!(argv, expected);
		// argv[0] 不在返回值里：可执行文件由 Command::new(fer) 提供。
		assert!(!argv.iter().any(|arg| arg.ends_with("fer.exe")));
	}

	#[test]
	fn serve_argv_carries_the_given_addr() {
		let argv = serve_argv(Path::new("fer.exe"), "127.0.0.1:12345");
		assert_eq!(argv.len(), 4);
		assert_eq!(argv[0], "serve");
		assert_eq!(argv[1], "--no-warm");
		assert_eq!(argv[2], "--addr");
		assert_eq!(argv[3], "127.0.0.1:12345");
	}

	#[test]
	fn find_in_path_returns_none_for_missing_names() {
		assert_eq!(find_in_path("fer-gui-definitely-missing-9f2c.exe"), None);
		assert_eq!(find_in_path(""), None);
		// 带路径分隔符的名字不应命中（join 之后是相对子路径，不是可执行文件）。
		assert_eq!(find_in_path("nested\\fer-gui-missing-9f2c.exe"), None);
	}

	#[test]
	fn find_in_path_returns_existing_file_when_present() {
		// cmd.exe 一般位于 System32；有就验证返回的确实是文件，没有则跳过（不引入环境依赖）。
		if let Some(found) = find_in_path("cmd.exe") {
			assert!(found.is_file());
			assert!(found.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"));
		}
	}

	#[test]
	fn find_fer_exe_only_returns_existing_files() {
		if let Some(found) = find_fer_exe() {
			assert!(found.is_file(), "find_fer_exe 不能返回不存在的路径：{}", found.display());
		}
	}

	#[test]
	fn local_default_fer_path_is_a_file_when_present() {
		if let Some(found) = local_default_fer_path() {
			assert!(found.is_file());
			assert!(found.to_string_lossy().to_ascii_lowercase().ends_with(FER_EXE_NAME));
		}
	}

	#[test]
	fn probe_fails_fast_and_fills_fields_when_port_is_closed() {
		let addr = unlistened_addr();
		let started = Instant::now();
		let status = probe(&addr, Duration::from_millis(500));
		let elapsed = started.elapsed();

		assert!(!status.online, "未监听的端口不应判定在线：{}", status.detail);
		assert_eq!(status.addr, addr, "addr 字段应原样回填");
		assert!(!status.detail.is_empty(), "离线必须给出可读原因");
		assert!(status.detail.contains("serve 未连接"), "detail 应说明未连接：{}", status.detail);
		// 本机拒绝连接是立刻返回的；给 3s 余量以防个别环境慢。
		assert!(elapsed < Duration::from_secs(3), "拒绝连接不应长时间阻塞：{elapsed:?}");
	}

	#[test]
	fn probe_reports_fer_path_consistently_with_find_fer_exe() {
		let addr = unlistened_addr();
		let status = probe(&addr, Duration::from_millis(500));
		assert_eq!(status.fer_path, find_fer_exe().map(|path| path.display().to_string()));
	}

	#[test]
	fn probe_rejects_bad_addr_without_panicking() {
		let status = probe("this-is-not-an-address", Duration::from_millis(200));
		assert!(!status.online);
		assert!(!status.detail.is_empty());
		assert_eq!(status.addr, "this-is-not-an-address");
	}

	#[test]
	fn body_looks_healthy_accepts_fer_health_and_rejects_noise() {
		assert!(body_looks_healthy("{\"ok\":true}"));
		assert!(body_looks_healthy("{\n  \"ok\": true,\n  \"version\": \"1\"\n}"));
		assert!(!body_looks_healthy("{\"ok\":false}"));
		assert!(!body_looks_healthy(""));
		assert!(!body_looks_healthy("<html>hello</html>"));
	}

	#[test]
	fn truncate_keeps_short_text_and_marks_long_text() {
		assert_eq!(truncate("short", 10), "short");
		assert_eq!(truncate("  padded  ", 10), "padded");
		let long = truncate("0123456789abcdef", 4);
		assert_eq!(long, "0123…");
	}

	#[test]
	fn split_response_parses_status_and_body() {
		let (status, body) = split_response("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}").expect("应能解析");
		assert_eq!(status, 200);
		assert_eq!(body, "{}");
		assert!(split_response("garbage").is_err());
	}

	#[test]
	fn spawn_serve_reports_error_without_spawning_anything() {
		// 不存在的可执行文件必须走 Err 分支；这里刻意不构造真实进程，避免副作用。
		let missing = std::env::temp_dir().join("fer-gui-definitely-missing-9f2c.exe");
		let err = spawn_serve(&missing, DEFAULT_ADDR).expect_err("不存在的 fer.exe 应返回 Err");
		assert!(!err.to_string().is_empty(), "错误信息不应为空");
	}

	// 说明：ensure_serve 的「拉起真实进程」分支不做自动化单测 —— 它会启动常驻的 fer serve
	// 并继承调用方的句柄（CI/脚本场景会让管道迟迟不 EOF），副作用大于收益。
	// 该分支由下面的联调用例（serve 已在线时 ensure_serve 必须直接返回探测结果）与
	// Lead 的手工验收共同覆盖。

	#[test]
	#[ignore = "需要 fer serve 在线（127.0.0.1:19876）"]
	fn probe_live_serve_reports_online() {
		let status = probe(DEFAULT_ADDR, Duration::from_secs(3));
		assert!(status.online, "本机 serve 应常驻在线：{}", status.detail);
		assert!(status.detail.contains("在线"), "detail={}", status.detail);
	}

	#[test]
	#[ignore = "需要 fer serve 在线（127.0.0.1:19876）"]
	fn ensure_serve_returns_immediately_when_already_online() {
		let started = Instant::now();
		let status = ensure_serve(DEFAULT_ADDR, Duration::from_secs(3), 5_000);
		let elapsed = started.elapsed();
		assert!(status.online, "serve 已在线时 ensure_serve 应返回在线：{}", status.detail);
		// 在线短路：不应走「拉起 + 轮询 5s」的路径。
		assert!(elapsed < Duration::from_secs(2), "在线时不应等待：{elapsed:?}");
	}
}
