# 接口契约（冻结）

> 约定：`main.rs` 只依赖本文件列出的签名。改签名 = 改本文件 + 通知 Lead；新增函数自由。
> 每个模块必须自带 `#[cfg(test)] mod tests`，纯逻辑测试不得启动窗口或真实网络。

## src/json.rs —— 零依赖 JSON 解析器

```rust
pub enum Json { Null, Bool(bool), Num(f64), Str(String), Arr(Vec<Json>), Obj(Vec<(String, Json)>) }
pub struct JsonError { pub message: String, pub pos: usize }
impl Json {
    pub fn parse(text: &str) -> Result<Json, JsonError>;
    pub fn get(&self, key: &str) -> Option<&Json>;
    pub fn as_str / as_u64 / as_i64 / as_f64 / as_bool / as_arr;
    pub fn str_at / u64_at / i64_at / bool_at / arr_at;
    pub fn arr_len(&self) -> usize;  pub fn is_null(&self) -> bool;
    pub fn str_or(&self, key: &str, default: &str) -> String;
}
```

要求：完整 RFC 8259（转义、`\uXXXX` 代理对、数字指数、嵌套、空白）；非法输入返回
`JsonError{message,pos}` 而**不得 panic**；测试覆盖每条。

## src/http.rs —— 极简 HTTP/1.1 客户端

```rust
pub struct HttpResponse { pub status: u16, pub body: String }
pub enum HttpError { Io(String), Protocol(String), Status(u16, String) }
impl HttpClient {
    pub fn new(addr: &str, timeout: Duration) -> Self;
    pub fn addr(&self) -> &str;
    pub fn get(&self, target: &str) -> Result<HttpResponse, HttpError>;
    pub fn post(&self, target: &str) -> Result<HttpResponse, HttpError>;
}
pub fn url_encode(s: &str) -> String;   // UTF-8 按字节 %XX，unreserved 直通
```

要求：连接/读超时；Content-Length 与 chunked 两种响应体；响应头大小写不敏感；
非 2xx -> `Status(code, body)`。测试用 `TcpListener` 起桩服务器。

## src/ferapi.rs —— fer API 客户端

```rust
pub struct Hit { path, is_dir, size, allocated, mtime, ctime, flags }
pub struct SearchResult { ok, error: Option<String>, query, count, total, took_ms, hits }
pub struct FeedStatus { delta_entries, pending_remove, batches_applied, saturated }
pub struct IndexStats { ok, entries, files, dirs, dump, dump_mb, mem_bytes, volumes, index_method, built_at_unix, feed }
pub struct DuNode { name, path, size, allocated, files, dirs, depth, children, truncated }
pub struct DuResult { root, total_bytes, total_allocated, files, dirs, entries, children, truncated }
impl FerClient {
    pub fn new(addr: &str, timeout: Duration) -> Self;
    pub fn addr(&self) -> &str;
    pub fn health(&self) -> Result<bool, HttpError>;
    pub fn search(&self, q: &str, limit: usize, sort: Option<&str>, desc: bool) -> Result<SearchResult, HttpError>;
    pub fn stats(&self) -> Result<IndexStats, HttpError>;
    pub fn feed(&self) -> Result<FeedStatus, HttpError>;
    pub fn du(&self, path: &str, depth: Option<u32>, top: Option<u32>, allocated: bool) -> Result<DuResult, HttpError>;
    pub fn rebuild(&self) -> Result<(), HttpError>;
    pub fn rescan(&self) -> Result<(), HttpError>;
    pub fn reveal(&self, path: &str) -> Result<(), HttpError>;
}
```

实测响应样本见 `docs/api-samples.md`（字段以此为准：search 顶层 ok/query/count/total/took_ms/engine/hits；
stats 只有 ok/entries/files/dirs/dump/dump_mb/mem_bytes；du 的 children[] 只有 path/size/allocated/files/depth，**无 name 字段**）。

端点事实（fer README 声明为稳定契约）：`/api/search?q=&limit=&sort=&desc=`、
`/api/stats`、`/api/feed`、`/api/du?path=&depth=&top=&allocated=`、
`POST /api/rebuild|/api/rescan`、`POST /api/reveal?path=`。

## src/format.rs —— 展示层格式化（纯函数）

`human_size`（1024 进制）/ `human_count`（千分位）/ `format_time`（Unix 秒 -> 本地
`YYYY-MM-DD HH:MM`，<=0 -> `-`，可自写 UTC+8 保持零依赖）/ `file_name` / `parent_dir` /
`extension` / `type_label`。

## src/config.rs —— 配置持久化

`Config{limit,last_query,sort_key,sort_desc,win_x,win_y,win_w,win_h}`；`Config::load/save/path/dir`；
位置 `%LOCALAPPDATA%\fer-gui\config.json`；损坏或缺失一律回退 `Default`（不得 panic）。

## src/actions.rs —— 系统集成

`reveal_in_explorer(path)`（explorer `/select,` + 独立 argv）/ `open_file(path)` /
`copy_to_clipboard(text)`（CF_UNICODETEXT，windows-sys）/ `is_valid_existing_path(path)`。

## src/query.rs —— 搜索会话状态机

`SortKey` + `SearchState::{new,begin,generation,accept,cycle_sort,sort,set_sort,limit,set_limit,last_query}`
+ `now_ms()`。代际号用于丢弃过期响应；`cycle_sort` 三态：升 -> 降 -> 回 Name 默认。

## 验收标准（「完全可用」定义）

1. `cargo build --release` 与 `cargo test` 全绿、`cargo clippy --all-targets` 0 警告
2. exe 启动即出窗口；COMCTL32.dll 解析到 WinSxS 6.0.*（v6 视觉样式）
3. serve 在线时：输入查询 -> 结果与 `fer search "<同一查询>" --json` 的 total/首条一致
4. 列头点击三态排序生效（与服务端 `--sort` 结果一致）
5. 双击行 -> 资源管理器选中该文件；状态栏显示命中总数与索引规模
6. serve 停掉 -> 状态栏明示未连接，GUI 不崩、不卡（HTTP 超时生效）
7. 关闭/重开 -> 上次查询与窗口尺寸恢复

## src/history.rs —— 搜索历史（Everything 式最近查询）

```rust
pub struct History { /* items: Vec<String>, max: usize */ }
impl History {
    pub fn load(max: usize) -> Self;             // 缺失/损坏 -> 空历史
    pub fn save(&self) -> std::io::Result<()>;
    pub fn push(&mut self, query: &str);         // 去重（大小写不敏感，保留最新写法）、最新在前、超 max 截断
    pub fn items(&self) -> &[String];
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn clear(&mut self);
}
pub fn history_path() -> std::path::PathBuf;
```

另有手写 `Default`（空 items + `DEFAULT_MAX = 20`）；持久化文件 `%LOCALAPPDATA%\\fer-gui\\history.txt`（一行一条）。

## src/menu.rs —— 菜单栏（Everything 式 GUI 入口）

```rust
pub const CMD_OPEN / CMD_REVEAL / CMD_COPY_PATH / CMD_COPY_NAME / CMD_COPY_DIR / CMD_DU_ANALYZE
        / CMD_DU_UP / CMD_MODE_SEARCH / CMD_REBUILD / CMD_RESCAN / CMD_START_SERVE
        / CMD_OPEN_CONFIG_DIR / CMD_CLEAR / CMD_SEARCH_NOW / CMD_ABOUT / CMD_EXIT: u16;
pub const ALL_COMMANDS: [u16; 16];
pub fn build_menu_bar() -> winsafe::SysResult<winsafe::HMENU>;
pub fn command_label(id: u16) -> &'static str;
```

菜单栏本身不含任何状态；命令一律经窗口 `WM_COMMAND`（`wm_command_acc_menu`）回到 main.rs 分派。

## src/format.rs 追加（占用视图导航）

```rust
pub fn path_segments(path: &str) -> Vec<(String, String)>;  // [("D:", "D:"), ("a", "D:\\a"), ..]
pub fn parent_of(path: &str) -> Option<String>;             // 到根返回 None
pub fn normalize_dir(path: &str) -> String;                 // 去尾分隔符，保留 "D:\\"
```

main.rs 在它们合入前用本地 `norm_dir/up_dir/breadcrumb` 顶替（行为一致，合入后替换调用即可）。
