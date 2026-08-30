# File-Engine-Rust

> Everything 级「秒定位全局文件」搜索引擎，纯 Rust。
> 索引走 **原始 $MFT 扫描**（与 Everything 同款内核路径），搜索走 **FERIDX01 dump 内存引擎**
> （mmap 零拷贝，毫秒级），支持硬链接别名、文件大小/时间/属性过滤，CLI + HTTP API 双通道，agent 友好。

## 特性

- ⚡ **完整 MFT 解析**：直读 NTFS `$MFT`（runlist + USA fixup + `$FILE_NAME` 全属性），
  一次拿到：**全部硬链接别名**（`System32\ntdll.dll` → WinSxS 本体也能搜到）、
  真实大小、修改/创建时间、hidden/system/readonly/reparse 标志；
  分片 `$MFT` 自动回退 USN 枚举，无管理员回退目录遍历
- 🧠 **低内存**：紧凑数组 + 流式 DFS 构建，全盘 400 万条峰值内存 ~170 MB
- 🚀 **毫秒级搜索**：全查询走内存引擎——子串（memchr SIMD 扫描）、`*.rs` 后缀（反向列二分）、
  通配符、大小写不敏感、CJK
- 🔍 **过滤查询语言**：`ext: size: dm: dc: type: hidden: parent: path: name:` + 取反（`!`）
- 🔄 **实时监控**：`fer monitor` 轮询 USN 日志增量更新（删除按 FRN 直删）
- 🌐 **HTTP API + 网页 UI** + **CLI --json**（稳定 JSON 输出，面向 agent）
- 🧠 **Everything 架构**：`fer index` 一次性把全盘扫进内存引擎并写成
  **FERIDX01 dump**（~1GB）；查询全靠内存的排序数组 + SIMD 扫描，dump 用
  **mmap 零拷贝加载**（serve 启动 ~135ms，CLI 全查询 17-336ms）
- 🔄 **USN 实时监控**（需管理员）：monitor 常驻内存增量追平 + 防抖写回 dump，
  崩溃后从 USN 日志回放，无需重建
- ✅ 测试闭环：单元 + 端到端 + 真实卷（`#[ignore]`）+ SQLite 交叉验证
  （store.rs 保留为测试参照 oracle，不进生产路径）

## 构建

```bash
# 本机（Tairitsu）无 MSVC 链接器，走 rustup GNU 工具链（rust-toolchain.toml 已配置）：
rustup run stable-x86_64-pc-windows-gnu cargo build --release
# 产物：target-gnu/release/fer.exe（独立 exe，无运行时依赖）
# 精简编译：cargo build --profile min-size（opt-level="z"，体积优先；默认构建不编译 SQLite）
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
fer dupes --min-size 1kb --limit 50  # 找重复文件（同大小分组 + 内容哈希 + 字节校验）
fer dupes --name adb.exe             # 只看文件名含 adb.exe 的重复组
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
- 已知边界：2 字短查询（尤其中文）走全名扫描，~60-150 ms；硬链接别名均已收录。

## 架构

```
src/
├── mft.rs      原始 $MFT 扫描：FSCTL_GET_NTFS_VOLUME_DATA → record0 $DATA runlist
│               → 分块读 + USA fixup → FILE 记录 → 全部 $FILE_NAME（硬链接）+
│               大小/时间/DOS 标志；纯函数解析器全部有单测
├── usn.rs      FSCTL_ENUM_USN_DATA / READ_USN_JOURNAL（回退索引 + 变更监控）
├── walk.rs     walkdir 回退（Windows 上 metadata 零额外 syscall）
├── mem.rs      FERIDX01 dump 内存引擎：56B 紧凑 Entry + arena + 7 排序置换
│               （含 by_frn，monitor 二分删改），mmap 零拷贝加载，多 term 并行求值
├── store.rs    SQLite + FTS5 oracle（feature `sqlite`，仅测试交叉验证，不进生产路径）
├── query.rs    查询语言解析（纯函数 + 单测）
├── indexer.rs  mft → usn → walk 逐级回退编排
├── monitor.rs  USN 日志轮询 → 内存增量（FRN 直删）+ 防抖写回 dump
├── dupes.rs    重复文件查找（同大小分组 + FNV 哈希 + 字节校验）
├── server.rs   axum HTTP API + 内嵌网页
└── main.rs     fer CLI（clap，--json 全局开关）
```

## 测试

```bash
cargo test                                        # 单元 + 端到端（默认不含 SQLite）
cargo test --features sqlite                      # 追加 SQL 交叉验证 oracle（store.rs / mem 一致性）
cargo test --test live_volume -- --ignored --nocapture   # 真实卷（需管理员）
# 自定义盘符：$env:FER_TEST_DRIVE='D'
```

真实卷测试断言：MFT 扫描 >10 万文件、`hosts` 命中、**硬链接别名 System32\ntdll.dll 命中**、
元数据 size 真实、搜索 <1s。与 Everything(es) 交叉验证见提交历史中的验证记录。

## 实测性能（本机 6 卷 · 全盘 416.8 万条 · dump v3 · 2026-09 基准）

CLI 全查询 3 轮取 min/median（每项均含进程启动 + dump mmap 加载）：

| 类别 | 查询 | 结果数 | min | median |
|------|------|-------:|----:|-------:|
| 后缀 | `*.rs` | 40,076 | 2 ms | 2 ms |
| 后缀 | `*.jpg` | 109,730 | 4 ms | 5 ms |
| 扩展名 | `ext:rs` | 40,076 | 2 ms | 2 ms |
| 通配符 | `main*.rs` | 161 | 6 ms | 6 ms |
| 大小 | `size:>1mb` | 52,327 | 10 ms | 10 ms |
| mtime | `dm:thisweek` | 1,273,577 | 43 ms | 47 ms |
| 目录 | `type:dir` | 573,127 | **1 ms** | 1 ms |
| 文件 | `type:file` | 3,595,317 | 6 ms | 6 ms |
| 隐藏 | `hidden:true` | 583 | **0 ms** | 0 ms |
| 取反 | `!hidden:true` | 4,167,861 | 63 ms | 73 ms |
| 父目录 | `parent:D:\Kita-Tools` | 2,546,957 | 72 ms | 81 ms |
| 交集 | `ext:rs size:>1mb` | 98 | 9 ms | 10 ms |
| 混合 | `dm:thisweek type:file` | 1,028,236 | 53 ms | 57 ms |
| 2 字子串 | `rs` | 183,817 | 158 ms | 168 ms（全名扫描） |
| 2 字 CJK | `报告` | 40 | 152 ms | 158 ms（全名扫描） |
| 长子串 | `report` | 7,440 | 167 ms | 171 ms（全名扫描） |
| 路径子串 | `Kita-Tools\Coding`（含 `\`） | 86,189 | 281 ms | 294 ms |
| 并行 | `report 报告`（两全扫合取） | 0 | 158 ms | 166 ms（≈单扫描） |

索引构建：全盘 416.8 万条（MFT 路径含硬链接别名），**热缓存 12.3s / 冷盘 ~40-50s**
（MFT 扫描是磁盘物理下限），收尾原子写 **FERIDX01 dump**（1021MB，写出 2.4s）。
- **serve**：mmap 零拷贝加载，**进程启动→listening 86ms**；HTTP 查询（热页）：
  `ext:rs` 2ms、`type:dir` 1ms、`dm:thisweek type:file` 34ms；逻辑内存 1021MB /
  RSS 350MB（mmap 按需缺页，OS 可回收）
- **CLI**：全查询 **0-294ms**（含进程启动 + dump 加载），无 SQLite、无门控
- **monitor**：USN 增量进内存（by_frn 二分 + 删除影子集），默认每 60s 防抖写回 dump
  （`--flush-secs` 可调）；flush 走 arena 直达复用（零 String 分配）
- **编译**：`cargo check` 6.9s（不编 SQLite）；release 1.81MB / `--profile min-size`
  1.58MB；clippy 0 警告

## 已知限制 / TODO

- 索引 = dump 快照 + monitor 增量；monitor 不在线时文件变动不反映（下次
  `fer index` 或 monitor 启动回放 USN 日志补齐）
- 内存引擎的路径排序只折叠 ASCII 大小写（非 ASCII 大小写字母如 É/Ö 按字节比较）
  ——Windows 路径中极少见
- 8.3 短名（namespace=2）不入索引（避免噪音）；分片 `$MFT` 回退 USN 路径会丢失
  硬链接别名与大小/时间元数据
- FAT/exFAT 卷不支持（监控需 ReadDirectoryChangesW，列为 TODO）
- 多卷监控需逐个 `fer monitor`；USN 日志回卷会报错提示重建
- release 构建开 `target-cpu=native`（本机专用，exe 不可分发）

## 与上游对照

| | File-Engine-Core (Java) | Everything | File-Engine-Rust |
|---|---|---|---|
| 索引 | C++ JNI 读 USN | 完整 MFT 解析 | **完整 MFT 解析（mft.rs）** |
| 硬链接别名 | ❌ | ✅ | ✅ |
| 大小/时间/属性 | 部分 | ✅ | ✅ |
| 存储 | SQLite | 内存索引 | **FERIDX01 dump 内存引擎（mmap 零拷贝）** |
| 搜索 | HTTP API | GUI + IPC | CLI + HTTP + 网页 |
| 监控 | fileMonitor | USN + RDCW | USN 轮询 |
| 依赖 | JDK21+VS+GraalVM | 闭源 | 单一 exe |
