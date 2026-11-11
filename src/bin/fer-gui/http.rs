//! 极简 HTTP/1.1 客户端（零第三方依赖，仅 std::net::TcpStream）。
//!
//! 只服务一个用途：访问本机回环上的 fer serve（GET/POST、短连接、无 TLS）。
//! 接口契约（冻结）：
//!   HttpClient::new(addr: &str, timeout: Duration) -> Self
//!   HttpClient::get(&self, target: &str) -> Result<HttpResponse, HttpError>
//!   HttpClient::post(&self, target: &str) -> Result<HttpResponse, HttpError>
//!   url_encode(&str) -> String
//!
//! 实现要求：
//! - addr 形如 127.0.0.1:19876；target 形如 /api/search?q=...（已编码）。
//! - 必须设连接/读/写超时（否则 serve 挂掉时 GUI 会卡死）。
//! - 必须处理 Content-Length 与 Transfer-Encoding: chunked 两种响应体；必须
//!   处理响应头大小写混合（axum 默认发小写）。
//! - 非 2xx 返回 HttpError::Status(code, body)。
//! - 不得 panic；所有错误走 Result。
//! - 自带单元测试：url_encode 边界 + 用 std::net::TcpListener 起桩服务器验证
//!   GET/POST/chunked/超时/错误状态码。

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 单次 read 的缓冲区大小。
const READ_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct HttpResponse {
	/// 状态码随 `HttpError::Status` 一起返回；这里保留完整响应供测试断言。
	#[allow(dead_code)]
	pub status: u16,
	pub body: String,
}

#[derive(Debug, Clone)]
pub enum HttpError {
	Io(String),
	Protocol(String),
	Status(u16, String),
}

impl fmt::Display for HttpError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			HttpError::Io(e) => write!(f, "网络错误: {e}"),
			HttpError::Protocol(e) => write!(f, "协议错误: {e}"),
			HttpError::Status(code, body) => write!(f, "HTTP {code}: {body}"),
		}
	}
}

impl std::error::Error for HttpError {}

pub struct HttpClient {
	addr: String,
	timeout: Duration,
}

impl HttpClient {
	pub fn new(addr: &str, timeout: Duration) -> Self {
		Self { addr: addr.to_string(), timeout }
	}

	pub fn addr(&self) -> &str {
		&self.addr
	}

	/// GET target（形如 /api/health）。
	pub fn get(&self, target: &str) -> Result<HttpResponse, HttpError> {
		self.request("GET", target)
	}

	/// POST target（无请求体；fer 的写端点参数都在查询串上）。
	pub fn post(&self, target: &str) -> Result<HttpResponse, HttpError> {
		self.request("POST", target)
	}

	fn request(&self, method: &str, target: &str) -> Result<HttpResponse, HttpError> {
		let mut stream = self.connect()?;
		stream
			.set_read_timeout(Some(self.timeout))
			.map_err(map_io_error)?;
		stream
			.set_write_timeout(Some(self.timeout))
			.map_err(map_io_error)?;
		let request = format!(
			"{method} {target} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
			self.addr
		);
		stream.write_all(request.as_bytes()).map_err(map_io_error)?;
		stream.flush().map_err(map_io_error)?;
		read_response(stream)
	}

	fn connect(&self) -> Result<TcpStream, HttpError> {
		let addrs = self
			.addr
			.to_socket_addrs()
			.map_err(|e| HttpError::Io(format!("地址解析失败 {}: {e}", self.addr)))?;
		let mut last_error: Option<std::io::Error> = None;
		for sock_addr in addrs {
			match TcpStream::connect_timeout(&sock_addr, self.timeout) {
				Ok(stream) => return Ok(stream),
				Err(e) => last_error = Some(e),
			}
		}
		match last_error {
			Some(e) => Err(HttpError::Io(format!("连接 {} 失败: {e}", self.addr))),
			None => Err(HttpError::Io(format!("地址 {} 未解析出可用地址", self.addr))),
		}
	}
}

fn map_io_error(e: std::io::Error) -> HttpError {
	match e.kind() {
		std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
			HttpError::Io(format!("连接或读取超时: {e}"))
		}
		_ => HttpError::Io(e.to_string()),
	}
}

fn read_response(stream: TcpStream) -> Result<HttpResponse, HttpError> {
	let mut reader = BodyReader::new(stream);
	let head_bytes = reader.read_until(b"\r\n\r\n")?;
	let head = String::from_utf8_lossy(&head_bytes).into_owned();

	let mut lines = head.split("\r\n");
	let status_line = lines.next().unwrap_or_default();
	let mut parts = status_line.splitn(3, ' ');
	let proto = parts.next().unwrap_or_default();
	if !proto.starts_with("HTTP/") {
		return Err(HttpError::Protocol(format!("响应状态行非法: {status_line}")));
	}
	let code: u16 = parts
		.next()
		.and_then(|s| s.trim().parse().ok())
		.ok_or_else(|| HttpError::Protocol(format!("状态码非法: {status_line}")))?;

	// 头名一律折叠成小写，查找时大小写不敏感（axum 发小写，代理可能发混合大小写）。
	let mut headers: Vec<(String, String)> = Vec::new();
	for line in lines {
		if line.trim().is_empty() {
			continue;
		}
		let Some((name, value)) = line.split_once(':') else {
			return Err(HttpError::Protocol(format!("响应头非法: {line}")));
		};
		headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
	}

	let chunked = header(&headers, "transfer-encoding")
		.is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
	let body = if chunked {
		reader.read_chunked()?
	} else if let Some(len) = header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok())
	{
		reader.take(len)?
	} else {
		reader.read_to_end()?
	};

	if (200..300).contains(&code) {
		Ok(HttpResponse { status: code, body: String::from_utf8_lossy(&body).into_owned() })
	} else {
		Err(HttpError::Status(code, String::from_utf8_lossy(&body).into_owned()))
	}
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
	headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// 增量读缓冲：把 TcpStream 的字节流切成「按行」与「按长度」两种读取。
struct BodyReader {
	stream: TcpStream,
	buf: Vec<u8>,
	pos: usize,
}

impl BodyReader {
	fn new(stream: TcpStream) -> Self {
		Self { stream, buf: Vec::new(), pos: 0 }
	}

	/// 再读一段；返回 0 表示对端已关闭。
	fn fill(&mut self) -> Result<usize, HttpError> {
		let mut tmp = [0u8; READ_CHUNK];
		let n = self.stream.read(&mut tmp).map_err(map_io_error)?;
		self.buf.extend_from_slice(&tmp[..n]);
		Ok(n)
	}

	fn pending(&self) -> &[u8] {
		&self.buf[self.pos..]
	}

	/// 消费 n 字节并丢掉已消费的前缀（响应体规模有限，拷贝成本可忽略）。
	fn consume(&mut self, n: usize) {
		self.pos += n;
		self.buf.drain(..self.pos);
		self.pos = 0;
	}

	fn read_until(&mut self, needle: &[u8]) -> Result<Vec<u8>, HttpError> {
		loop {
			if let Some(idx) = find(self.pending(), needle) {
				let out = self.pending()[..idx].to_vec();
				self.consume(idx + needle.len());
				return Ok(out);
			}
			if self.fill()? == 0 {
				return Err(HttpError::Protocol("响应在分隔符出现前中断".to_string()));
			}
		}
	}

	fn take(&mut self, n: usize) -> Result<Vec<u8>, HttpError> {
		while self.pending().len() < n {
			if self.fill()? == 0 {
				return Err(HttpError::Protocol(format!(
					"响应体不足：期望 {n} 字节，连接提前结束"
				)));
			}
		}
		let out = self.pending()[..n].to_vec();
		self.consume(n);
		Ok(out)
	}

	fn read_to_end(&mut self) -> Result<Vec<u8>, HttpError> {
		let mut out = self.pending().to_vec();
		let taken = out.len();
		self.consume(taken);
		loop {
			if self.fill()? == 0 {
				return Ok(out);
			}
			let n = self.pending().len();
			out.extend_from_slice(self.pending());
			self.consume(n);
		}
	}

	/// chunked 传输：<十六进制长度>[;扩展]\r\n<数据>\r\n ... 0\r\n[trailer]\r\n
	fn read_chunked(&mut self) -> Result<Vec<u8>, HttpError> {
		let mut body = Vec::new();
		loop {
			let line = self.read_until(b"\r\n")?;
			let text = String::from_utf8_lossy(&line).into_owned();
			let size_text = text.split(';').next().unwrap_or_default().trim().to_string();
			let size = usize::from_str_radix(&size_text, 16)
				.map_err(|_| HttpError::Protocol(format!("chunked 分块长度非法: {size_text}")))?;
			if size == 0 {
				// 末尾 trailer：可能直接是空行，也可能有若干 header 行
				loop {
					let trailer = self.read_until(b"\r\n")?;
					if trailer.is_empty() {
						break;
					}
				}
				return Ok(body);
			}
			body.extend_from_slice(&self.take(size)?);
			let crlf = self.take(2)?;
			if crlf != b"\r\n" {
				return Err(HttpError::Protocol(
					"chunked 分块数据未以 CRLF 结束".to_string(),
				));
			}
		}
	}
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	if needle.is_empty() || haystack.len() < needle.len() {
		return None;
	}
	haystack.windows(needle.len()).position(|w| w == needle)
}

/// 百分号编码：非 unreserved 字符全部 %XX（查询串用）。UTF-8 按字节编码。
pub fn url_encode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.as_bytes() {
		let c = *b as char;
		if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
			out.push(c);
		} else {
			out.push_str(&format!("%{:02X}", b));
		}
	}
	out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Read, Write};
	use std::net::TcpListener;
	use std::thread;

	/// 桩服务器：接受一次连接，读完请求头，回写固定响应；返回收到的请求文本。
	fn stub(response: String) -> (String, thread::JoinHandle<String>) {
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
				if find(&req, b"\r\n\r\n").is_some() {
					break;
				}
			}
			sock.write_all(response.as_bytes()).expect("写响应");
			sock.flush().expect("冲刷响应");
			String::from_utf8_lossy(&req).into_owned()
		});
		(addr, handle)
	}

	#[test]
	fn url_encode_covers_boundaries() {
		assert_eq!(url_encode(""), "");
		assert_eq!(url_encode("abcXYZ019"), "abcXYZ019");
		assert_eq!(url_encode("-_.~"), "-_.~");
		assert_eq!(url_encode("a b"), "a%20b");
		assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
		assert_eq!(url_encode("?q=1"), "%3Fq%3D1");
		assert_eq!(url_encode("中文"), "%E4%B8%AD%E6%96%87");
		assert_eq!(url_encode("D:\\Kita"), "D%3A%5CKita");
		assert_eq!(url_encode("😀"), "%F0%9F%98%80");
	}

	#[test]
	fn get_reads_content_length_body() {
		let (addr, handle) = stub(
			"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}"
				.to_string(),
		);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		let res = client.get("/api/health").expect("GET 应成功");
		assert_eq!(res.status, 200);
		assert_eq!(res.body, "{\"ok\":true}");
		assert_eq!(client.addr(), addr);

		let req = handle.join().expect("桩线程");
		assert!(req.starts_with("GET /api/health HTTP/1.1\r\n"), "{req}");
		assert!(req.contains(&format!("Host: {addr}")), "{req}");
	}

	#[test]
	fn post_sends_post_method_with_zero_length_body() {
		let (addr, handle) =
			stub("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_string());
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert_eq!(client.post("/api/rescan").expect("POST 应成功").body, "ok");
		let req = handle.join().expect("桩线程");
		assert!(req.starts_with("POST /api/rescan HTTP/1.1\r\n"), "{req}");
		assert!(req.contains("Content-Length: 0\r\n"), "{req}");
	}

	#[test]
	fn header_lookup_is_case_insensitive() {
		let variants = ["Content-Length: 2", "content-length: 2", "CONTENT-LENGTH: 2", "Content-length: 2"];
		for variant in variants {
			let raw = format!("HTTP/1.1 200 OK\r\n{variant}\r\n\r\nhi");
			let (addr, handle) = stub(raw);
			let client = HttpClient::new(&addr, Duration::from_secs(5));
			assert_eq!(client.get("/x").expect("GET 应成功").body, "hi");
			handle.join().expect("桩线程");
		}
	}

	#[test]
	fn reads_chunked_body() {
		let raw = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\n"
			.to_string();
		let (addr, handle) = stub(raw);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert_eq!(client.get("/api/search").expect("chunked GET 应成功").body, "Wikipedia");
		handle.join().expect("桩线程");
	}

	#[test]
	fn reads_chunked_body_with_trailer() {
		let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nx-trailer: 1\r\n\r\n"
			.to_string();
		let (addr, handle) = stub(raw);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert_eq!(client.get("/x").expect("chunked GET 应成功").body, "abc");
		handle.join().expect("桩线程");
	}

	#[test]
	fn reads_close_delimited_body() {
		let raw = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\nno length here".to_string();
		let (addr, handle) = stub(raw);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert_eq!(client.get("/x").expect("GET 应成功").body, "no length here");
		handle.join().expect("桩线程");
	}

	#[test]
	fn non_2xx_becomes_status_error() {
		let raw = "HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\n\r\nnot found".to_string();
		let (addr, handle) = stub(raw);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		match client.get("/api/nope") {
			Err(HttpError::Status(code, body)) => {
				assert_eq!(code, 404);
				assert_eq!(body, "not found");
			}
			other => panic!("期望 Status 错误，实际 {other:?}"),
		}
		handle.join().expect("桩线程");
	}

	#[test]
	fn malformed_status_line_is_protocol_error() {
		let (addr, handle) = stub("BOGUS\r\n\r\n".to_string());
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert!(matches!(client.get("/x"), Err(HttpError::Protocol(_))));
		handle.join().expect("桩线程");
	}

	#[test]
	fn read_timeout_is_reported_as_io_error() {
		let listener = TcpListener::bind("127.0.0.1:0").expect("绑定桩端口");
		let addr = listener.local_addr().expect("取本地地址").to_string();
		let handle = thread::spawn(move || {
			let (mut sock, _) = listener.accept().expect("接受连接");
			let mut tmp = [0u8; 512];
			let _ = sock.read(&mut tmp);
			thread::sleep(Duration::from_millis(800));
			let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
		});
		let client = HttpClient::new(&addr, Duration::from_millis(150));
		let start = std::time::Instant::now();
		let err = client.get("/slow").expect_err("应超时");
		assert!(matches!(err, HttpError::Io(_)), "{err}");
		assert!(start.elapsed() < Duration::from_millis(700), "超时未生效");
		handle.join().expect("桩线程");
	}

	#[test]
	fn connection_refused_is_io_error() {
		let listener = TcpListener::bind("127.0.0.1:0").expect("绑定桩端口");
		let addr = listener.local_addr().expect("取本地地址").to_string();
		drop(listener); // 立即释放端口，连接必然被拒
		let client = HttpClient::new(&addr, Duration::from_millis(300));
		assert!(matches!(client.get("/x"), Err(HttpError::Io(_))));
	}

	#[test]
	fn bad_address_is_io_error() {
		let client = HttpClient::new("no-such-host.invalid:1", Duration::from_millis(200));
		assert!(matches!(client.get("/x"), Err(HttpError::Io(_))));
	}

	#[test]
	fn truncated_body_is_protocol_error() {
		let raw = "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort".to_string();
		let (addr, handle) = stub(raw);
		let client = HttpClient::new(&addr, Duration::from_secs(5));
		assert!(matches!(client.get("/x"), Err(HttpError::Protocol(_))));
		handle.join().expect("桩线程");
	}
}
