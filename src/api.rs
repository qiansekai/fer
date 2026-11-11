//! HTTP API 的响应模型：服务端与 GUI 共用的唯一字段定义。
//!
//! 合并 fer-gui 时两边各写了一份同名结构体，`Hit::flags` 甚至漂移成
//! `u8` / `u32` 两种类型；字段改名后只会表现为客户端静默回落到默认值。
//! 这里把定义收口到库侧：
//!
//! * GUI 的解析器以这些结构体为构造目标 —— 字段改名会直接编译失败；
//! * 服务端能用结构体构造的端点（`/api/stats`、`/api/feed`、`/api/du`）
//!   改为结构体构造 —— 序列化字段由编译器保证；
//! * `/api/search` 仍用 `json!` 构造（缓存与错误分支交织），
//!   由 `tests/api_contract.rs` 把真实响应反序列化回来兜底。
//!
//! 只收录「线上 JSON 的实际形状」。CLI 专属口径（`index_method` /
//! `built_at_unix` / `volumes`）从未出现在 serve 响应里，客户端不再维护。

use serde::{Deserialize, Serialize};

pub use crate::Hit;
pub use crate::du::{DuEntry, DuReport};

/// `GET /api/search` 的成功响应。错误响应只有 `ok:false` + `error`，
/// 其余字段回退默认值。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SearchResult {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub query: String,
    /// 引擎标识（当前恒为 `mem`）；GUI 不展示，保留以对齐线上响应。
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub count: usize,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub took_ms: u64,
    #[serde(default)]
    pub hits: Vec<Hit>,
}

/// `GET /api/stats`。serve 只报这七项；CLI 的 `fer stats` 是另一套口径。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub ok: bool,
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    pub dump: String,
    pub dump_mb: u64,
    pub mem_bytes: u64,
}

/// `GET /api/feed` 的实时覆盖层状态。
///
/// `sample_*` / `removed_in_dump` / `overlay_generation` 是 serve 自带的
/// 诊断字段（排查「文件进了 overlay 却搜不到」时唯一可见的证据），
/// GUI 不使用但必须原样保留在响应里。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedStatus {
    pub ok: bool,
    pub pending_append: u64,
    pub pending_remove: u64,
    pub delta_entries: u64,
    pub batches_applied: u64,
    pub saturated: bool,
    pub removed_in_dump: u64,
    pub overlay_generation: u64,
    #[serde(default)]
    pub last_apply_age_ms: Option<u64>,
    #[serde(default)]
    pub sample_append: Vec<String>,
    #[serde(default)]
    pub sample_remove: Vec<String>,
}
