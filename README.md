# File-Engine-Rust

> Everything 级「秒定位全局文件」搜索引擎，纯 Rust。
> 索引走 **原始 $MFT 扫描**（与 Everything 同款内核路径），搜索走 SQLite + FTS5 trigram（毫秒级），
> 支持硬链接别名、文件大小/时间/属性过滤，CLI + HTTP API 双通道，agent 友好。

## 特性

- ⚡ **完整 MFT 解析**：直读 NTFS `$MFT`（runlist + USA fixup + `$FILE_NAME` 全属性），
  一次拿到：**全部硬链接别名**（`System32\ntdll.dll` → WinSxS 本体也能搜到）、
  真实大小、修改/创建时间、hidden/system/readonly/reparse 标志；
  分片 `$MFT` 自动回退 USN 枚举，无管理员回退目录遍历
- 🧠 **低内存**：紧凑数组 + 流式 DFS 落库，全盘 400 万条峰值内存 ~170 MB
- 🚀 **毫秒级搜索**：子串（FTS5 trigram ≥3 字符）、`*.rs` 后缀（反向列区间查询）、
  通配符、大小写不敏感、CJK
- 🔍 **过滤查询语言**：`ext: size: dm: dc: type: hidden: parent: path: name:` + 取反（`!`）
- 🔄 **实时监控**：`fer monitor` 轮询 USN 日志增量更新（删除按 FRN 直删）
- 🌐 **HTTP API + 网页 UI** + **CLI --json**（稳定 JSON 输出，面向 agent）
- 🧠 **serve 模式内存索引**：启动时加载 415 万名字的紧凑内存索引（156MB），
  1-2 字短查询单次 SIMD 扫描 ~70ms（Everything 同款方案）；`--no-mem-index` 关闭
- ✅ 测试闭环：单元 + 端到端 + 真实卷（`#[ignore]`）+ 与 Everything(es) 交叉验证

## 构建

```bash
# 本机（Tairitsu）无 MSVC 链接器，走 rustup GNU 工具链（rust-toolchain.toml 已配置）：
rustup run stable-x86_64-pc-windows-gnu cargo build --release
# 产物：target-gnu/release/fer.exe（独立 exe，无运行时依赖）
```

## CLI 使用

```bash
fer volumes                          # 列出固定 NTFS 卷（--json 输出 JSON）
fer index                            # 全盘建索引（auto：MFT → USN → walk 逐级回退）
fer index --volumes D --method mft   # 指定卷 / 强制 MFT 路径
fer search "AGENTS.md"               # 秒搜（子串）
fer search "*.rs"                    # 通配符
fer search "ext:mp4 size:>1gb dm:thisweek"   # 过滤查询语言
fer search "foo" --limit 50 --count-only     # 只看命中数
fer serve --addr 127.0.0.1:9876      # HTTP API + 网页 UI
fer monitor --volume D               # USN 实时增量（需管理员）
fer stats                            # 索引统计
fer --db <path> <cmd>                # 自定义索引库（默认 %LOCALAPPDATA%\file-engine-rust\index.db）
```

## 查询语言（CLI 与 HTTP 共用）

空白分隔的 term，全部大小写不敏感；`!term` 取反。示例：
`ext:rs,png size:>1mb dm:thisweek parent:D:\proj !hidden:true`

| 语法 | 含义 |
|------|------|
| `foo` | 文件名子串（basename contains） |
| `*.rs` `main*` `a?c*` | 通配符（`*` 任意串、`?` 单字符；`*.rs` 为精确后缀） |
| `D:\proj\src`（含分隔符的裸词） | 全路径子串 |
| `name:foo` | 显式文件名匹配（同裸词） |
| `ext:rs,png` | 扩展名（逗号列表，不带点） |
| `size:>1mb` `size:<10kb` `size:1kb-5mb` `size:1024` | 大小（kb=1024, mb=1024², gb=1024³；`>` `<` 区间；裸值为精确字节） |
| `dm:today` `dm:yesterday` `dm:thisweek` `dm:thismonth` | 修改时间（近 7 天/30 天） |
| `dm:2026-01-01` `dm:>2026-01-01` `dm:2026-01-01..2026-01-31` | 修改时间绝对日期（本地时区，含当日） |
| `dc:...` | 创建时间（同上语法） |
| `type:file` / `type:dir` | 只文件 / 只目录 |
| `hidden:` `system:` `readonly:` `reparse:` | 属性过滤（后接 true/false/1/0） |
| `parent:D:\proj` | 该目录**子树**（前缀匹配） |
| `path:D:\proj\src` | 全路径前缀 |
| `!term` | 取反（与其它 term 为 AND 关系） |

## HTTP API

```
GET /api/health                     → {"ok":true}
GET /api/search?q=<query>&limit=<n> → 命中列表（带 size/mtime/ctime/flags）
GET /api/stats                      → 索引统计 + 卷列表
POST /api/rescan                    → 后台全量重建
GET /                              → 网页搜索 UI
```

搜索响应（hits 为数组，`total` 为未截断的总命中数）：

```json
{
  "ok": true, "query": "ext:rs", "count": 100, "total": 40039, "took_ms": 5,
  "hits": [
    {"path": "D:\\proj\\main.rs", "is_dir": false, "size": 1234,
     "mtime": 1750000000, "ctime": 1749000000, "flags": 0}
  ]
}
```

curl 示例：

```bash
curl "http://127.0.0.1:9876/api/search?q=ext%3Ars%20size%3A%3E1mb&limit=10"
curl -X POST http://127.0.0.1:9876/api/rescan
```

## Agent 使用指南

- **CLI**：所有子命令支持 `--json`；错误时退出码非 0。查询语言参数**带引号传入**：
  `fer search "ext:rs dm:thisweek" --json`（注意 shell 里 `!` 需转义或使用单引号）。
- **HTTP**：长驻 `fer serve` 后直接 `GET /api/search?q=...`，URL 编码查询串。
- **稳定契约**：`--json` 输出与 `/api/*` 的字段名视为稳定 API；`took_ms`/`total` 仅供观测。
- 已知边界：2 字短查询（尤其中文）走全扫描约 1s 量级；硬链接别名均已收录。

## 架构

```
src/
├── mft.rs      原始 $MFT 扫描：FSCTL_GET_NTFS_VOLUME_DATA → record0 $DATA runlist
│               → 分块读 + USA fixup → FILE 记录 → 全部 $FILE_NAME（硬链接）+
│               大小/时间/DOS 标志；纯函数解析器全部有单测
├── usn.rs      FSCTL_ENUM_USN_DATA / READ_USN_JOURNAL（回退索引 + 变更监控）
├── walk.rs     walkdir 回退（Windows 上 metadata 零额外 syscall）
├── store.rs    SQLite(WAL+mmap) + FTS5 trigram；反向列区间查询；查询语言→SQL 翻译
├── query.rs    查询语言解析（纯函数 + 单测）
├── indexer.rs  mft → usn → walk 逐级回退编排
├── monitor.rs  USN 日志轮询 → delete_by_frn / upsert
├── server.rs   axum HTTP API + 内嵌网页
└── main.rs     fer CLI（clap，--json 全局开关）
```

## 测试

```bash
cargo test                                        # 单元 + 端到端（33+ 用例）
cargo test --test live_volume -- --ignored --nocapture   # 真实卷（需管理员）
# 自定义盘符：$env:FER_TEST_DRIVE='D'
```

真实卷测试断言：MFT 扫描 >10 万文件、`hosts` 命中、**硬链接别名 System32\ntdll.dll 命中**、
元数据 size 真实、搜索 <1s。与 Everything(es) 交叉验证见提交历史中的验证记录。

## 实测性能（本机 6 卷 · 全盘 358 万文件）

| 查询 | 结果数 | 耗时 |
|------|-------:|-----:|
| `*.rs`（后缀） | 40,067 | 16 ms |
| `*.jpg`（后缀） | 109,726 | 11 ms |
| `Cargo.toml`（子串） | 2,603 | ~20 ms |
| `ntdll.dll`（含硬链接别名） | 61 | ~10 ms |
| `dm:thisweek`（mtime 索引） | 1,701,682 | 39 ms |
| `hidden:true`（部分索引） | 583 | ~0 ms |
| `ext:mp4 size:>100mb` | 42 | 9 ms |
| `parent:D:\Kita-Tools\Coding` | 77,899 | 57 ms |
| `报告`（2 字 CJK） | 41 | 436 ms（SQL）/ **71 ms（serve 内存索引）** |
| `rs`（2 字，18 万命中） | 183,505 | 465 ms（SQL）/ **78 ms（serve 内存索引）** |

索引构建：全盘 ~415 万条（MFT 路径含硬链接别名），峰值内存 ~309 MB。
serve 模式常驻 ~530MB 工作集（内存索引 156MB + mmap 页缓存，OS 可按需回收）。

## 已知限制 / TODO

- 2 字短查询：serve 模式走内存索引 ~70ms；CLI 一次性查询仍走 SQL ~0.4s
  （不在单次 CLI 查询里加载 156MB 索引）
- 单个 `$FILE_NAME` 残留 0 大小（NTFS 自身行为，Everything 同样显示未知），不影响搜索
- 8.3 短名（namespace=2）不入索引（避免噪音）；分片 `$MFT` 回退 USN 路径会丢失硬链接别名
- FAT/exFAT 卷不支持（监控需 ReadDirectoryChangesW，列为 TODO）
- 多卷监控需逐个 `fer monitor`；USN 日志回卷会报错提示重建

## 与上游对照

| | File-Engine-Core (Java) | Everything | File-Engine-Rust |
|---|---|---|---|
| 索引 | C++ JNI 读 USN | 完整 MFT 解析 | **完整 MFT 解析（mft.rs）** |
| 硬链接别名 | ❌ | ✅ | ✅ |
| 大小/时间/属性 | 部分 | ✅ | ✅ |
| 存储 | SQLite | 内存索引 | SQLite + FTS5 trigram |
| 搜索 | HTTP API | GUI + IPC | CLI + HTTP + 网页 |
| 监控 | fileMonitor | USN + RDCW | USN 轮询 |
| 依赖 | JDK21+VS+GraalVM | 闭源 | 单一 exe |
