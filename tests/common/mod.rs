//! 联调测试共享的极简 HTTP 客户端（仅 std）。
//!
//! 只服务一个用途：访问本机回环上的 fer serve。刻意不 include GUI 的
//! `http.rs` / `ferapi.rs`，让联调测试与客户端内部改动解耦。

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 与 GUI 默认一致的 serve 地址；可用环境变量覆盖。
pub const DEFAULT_ADDR: &str = "127.0.0.1:19876";
/// 单次请求的读写超时。
pub const TIMEOUT: Duration = Duration::from_secs(5);

pub fn serve_addr() -> String {
	std::env::var("FER_GUI_SERVE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string())
}

/// 裸 HTTP/1.1 请求（Connection: close，读到 EOF），返回 (状态码, 响应体)。
pub fn http_request(addr: &str, method: &str, target: &str) -> (u16, String) {
	let sock = addr
		.to_socket_addrs()
		.unwrap_or_else(|e| panic!("serve 地址 {addr} 无法解析：{e}"))
		.next()
		.unwrap_or_else(|| panic!("serve 地址 {addr} 解析结果为空"));
	let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT)
		.unwrap_or_else(|e| panic!("连接 {addr} 失败（serve 是否在线？）：{e}"));
	stream.set_read_timeout(Some(TIMEOUT)).expect("设置读超时失败");
	stream.set_write_timeout(Some(TIMEOUT)).expect("设置写超时失败");
	let request = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).expect("发送请求失败");
	let mut raw = String::new();
	stream.read_to_string(&mut raw).expect("读取响应失败");
	split_response(&raw)
}

pub fn http_get(addr: &str, target: &str) -> (u16, String) {
	http_request(addr, "GET", target)
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
