# File-Engine-Rust

> Everything 级「秒定位全局文件」搜索引擎，纯 Rust 从零重写。
> 索引走 NTFS MFT（管理员快路径），搜索走 SQLite + FTS5 trigram（毫秒级），
> 可选 USN 日志实时监控 + HTTP API / 网页 UI。

## 为什么有它

`Coding/File-Engine-Core`（Java 版，上游 File-Engine）的 Rust 重写曾烂尾于骨架
（`.claude/worktrees/rust-rewrite`，13 个空模块）。本项目**不**在其上续写，而是
新开目录从零实现，并带完整测试。

## 特性

- ⚡ **秒级全盘索引 + 低内存**：管理员权限下直接读 NTFS MFT（`FSCTL_ENUM_USN_DATA`），
  与 Everything 同机制；紧凑数组 + 流式 DFS 落库，**全盘 409 万条峰值内存 ~170 MB**；
  无管理员时自动回退 `walkdir` 全盘遍历
- 🚀 **毫秒级搜索**：子串（FTS5 trigram ≥3 字符 / instr 兜底）、
  `*`/`?` 通配符（`*.rs` 后缀型走反向列区间查询，纯 LIKE 兜底）、大小写不敏感、
  CJK 友好；默认匹配文件名，含 `\`/`/` 或 `--path` 时匹配全路径
- 🔄 **实时监控**：`fer monitor` 轮询 USN 日志（自动查询日志 ID），增量应用
  创建/删除/重命名（删除按 FRN 直删，MFT 记录被回收也能正确移除）
- 🌐 **HTTP API + 网页 UI**：`fer serve` 后浏览器打开即搜
- ✅ **测试闭环**：单元测试（匹配语义 / SQL 查询 / USN 缓冲解析 / 路径重建）+
  临时目录端到端 + 真实卷集成测试（`#[ignore]`，管理员下跑）；
  与 Everything（es CLI）交叉验证：Cargo.toml/AGENTS.md/*.mp4 等查询 **100% 一致**

## 实测性能（本机 6 卷 · 352 万文件 + 57 万目录）

| 查询 | 结果数 | 耗时 |
|------|-------:|-----:|
| `*.rs`（后缀） | 40,039 | 4 ms |
| `*.jpg`（后缀） | 109,682 | 32 ms |
| `Cargo.toml`（子串） | 2,603 | 16 ms |
| `File-Engine-Rust\src`（路径） | 10 | 38 ms |
| `报告`（2 字 CJK） | 40 | ~1.3 s（全扫描，见限制） |

索引构建：全盘 6 卷 409 万条 ≈ 485 s，**峰值内存 171 MB**。

## 构建

```bash
# 本机（Tairitsu）没有 MSVC 链接器（Git 的 GNU link.exe 会遮蔽 link），
# 用 rustup 的 GNU 工具链构建，产物为独立 exe（无运行时依赖）：
rustup run stable-x86_64-pc-windows-gnu cargo build --release
# 或者直接：cargo build --release   （需 PATH 走 rustup 代理且 rust-toolchain.toml 生效）
# 产物：target-gnu/release/fer.exe（项目配置了 rust-toolchain.toml + .cargo/config.toml 指向 GNU）
```

> 有 MSVC（VS2022 C++ 生成工具）的机器上默认 `cargo build --release` 即可。

## 使用

```bash
fer volumes                          # 列出固定 NTFS 卷
fer index --volumes D                # 索引 D 盘（默认 auto：优先 USN，失败回退 walk）
fer index                            # 索引全部固定 NTFS 卷
fer search "AGENTS.md"               # 秒搜
fer search "*.rs" --limit 50         # 通配符
fer search "src\main" --path         # 全路径搜索
fer search "main" --count-only       # 只看命中总数
fer serve --addr 127.0.0.1:9876      # HTTP API + 网页 UI
fer monitor --volume D               # USN 日志实时增量（需管理员）
fer stats                            # 索引统计
# 自定义数据库位置（默认 %LOCALAPPDATA%\file-engine-rust\index.db）
fer --db D:\data\idx.db search foo
```

## HTTP API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/health` | `{"ok":true}` |
| GET | `/api/search?q=<query>&limit=<n>&path=<bool>` | 命中列表 + 总数 + 耗时 |
| GET | `/api/stats` | 索引统计 + 卷列表 |
| POST | `/api/rescan` | 后台全量重建索引 |
| GET | `/` | 极简网页搜索 UI |

## 测试

```bash
cargo test                                        # 单元 + 临时目录端到端
cargo test --test live_volume -- --ignored --nocapture   # 真实卷（需管理员）
# 自定义盘符：$env:FER_TEST_DRIVE='D'
```

真实卷测试断言：C 盘 MFT 记录数 > 10 万、`ntdll.dll` 可被枚举且搜索命中、
搜索耗时 < 1s。另可用 Everything 的 `es` CLI 交叉验证（本机 Everything 1.5 在役）。

## 架构

```
src/
├── usn.rs      FSCTL_ENUM_USN_DATA / READ_USN_JOURNAL，纯函数解析器 + FRN→路径树重建
├── walk.rs     walkdir 回退（Windows 上 metadata 零额外 syscall）
├── store.rs    SQLite(WAL+mmap) + FTS5 trigram；重建事务（synchronous=OFF 提速）；
│               name_r/path_r 反向列把 `*.rs` 后缀搜索变成索引区间查询
├── matcher.rs  匹配语义参考实现（单测基准）
├── indexer.rs  auto/usn/walk 编排，进度输出
├── monitor.rs  USN 日志轮询 → delete_by_frn / upsert
├── server.rs   axum HTTP API + 内嵌网页
└── main.rs     fer CLI（clap）
```

## 已知限制 / TODO

- **2 字短查询（尤其 CJK）走 instr 全扫描**（FTS5 trigram 要求 ≥3 字符），4M 条目约 1.3 s；
  Everything 靠内存索引做到 ~100 ms。彻底解法 = serve 模式常驻紧凑内存索引（选项化）
- **硬链接别名只显示主名**：`FSCTL_ENUM_USN_DATA` 只暴露 MFT 记录的主文件名。
  `C:\Windows\System32\ntdll.dll` 这类 WinSxS 硬链接别名（Everything 通过解析完整
  MFT 的附加 `$FILE_NAME` 属性能看到）不会出现在索引里，只会显示 WinSxS 主名。
  补齐需 `FSCTL_GET_NTFS_FILE_RECORD` 解析，列为 TODO
- USN 路径不取文件大小（USN_RECORD 无 size 字段；walk 路径有），后续可后补
- 未做拼音 / 模糊排序 / 内容搜索（本项目和 Everything 一样只搜文件名）
- 多卷监控需逐个 `fer monitor`；USN 日志回卷会报错提示重建（而非自动重建）
- 重建索引会阻塞同库的搜索（v1 设计）

## 与上游对照

| | File-Engine-Core (Java) | File-Engine-Rust (本项目) |
|---|---|---|
| 索引 | C++ JNI 读 USN | 纯 Rust windows-sys 读 MFT |
| 存储 | SQLite | SQLite + FTS5 trigram |
| 搜索 | HTTP API | CLI + HTTP API + 网页 |
| 监控 | fileMonitor 线程 | USN 日志轮询 |
| 依赖 | JDK21 + VS2022 + GraalVM | 单一 exe（无运行时依赖） |
