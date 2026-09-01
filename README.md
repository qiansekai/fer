# File-Engine-Rust

> Everything 级「秒定位全局文件」搜索引擎，纯 Rust。
> 索引走 **原始 $MFT 扫描**（与 Everything 同款内核路径），搜索走 **FERIDX01 dump 内存引擎**
> （mmap 零拷贝，毫秒级），支持硬链接别名、文件大小/时间/属性过滤，CLI + HTTP API 双通道，agent 友好。

## 特性

- ⚡ **完整 MFT 解析**：直读 NTFS `$MFT`（runlist + USA fixup + `$FILE_NAME` 全属性），
  一次拿到：**全部硬链接别名**（`System32\ntdll.dll` → WinSxS 本体也能搜到）、
  真实大小、修改/创建时间、hidden/system/readonly/reparse 标志；
  **$MFT::$BITMAP 跳读**（已删除记录区段不读不解析，40-55% I/O 削减）+
  **分片并行解析**（scoped 线程池 + 按序提交，构建 12.3s → 7.9s）；
  **降级必须显式**：auto/mft/usn 非提权直接拒绝（`--method walk` 才是显式降级选项）
- 🧠 **低内存**：紧凑数组 + 流式 DFS 构建，全盘 400 万条峰值内存 ~170 MB
- 🚀 **毫秒级搜索**：全查询走内存引擎——子串（memchr SIMD 扫描）、`*.rs` 后缀（反向列二分）、
  通配符、大小写不敏感、CJK
- 🔍 **过滤查询语言**：`ext: size: dm: dc: type: hidden: parent: path: name:` + 取反（`!`）
- 🔄 **实时监控**：`fer monitor` 轮询 USN 日志增量更新（删除按 FRN 直删）
- 🌐 **HTTP API + 网页 UI** + **CLI --json**（稳定 JSON 输出，面向 agent）
- 🧠 **Everything 架构**：`fer index` 一次性把全盘扫进内存引擎并写成
  **FERIDX01 dump**（~1.2GB）；查询全靠内存的排序数组 + **整 arena 单遍 SIMD 扫描**
  （子串/正则走 memchr 大缓冲扫 + `name_offs`/`path_offs` 游标映射）+
  **trigram 倒排段**（≥3 字节子串 O(posting) 候选），dump 用
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
fer index                            # 全盘建索引（auto = 纯 MFT；非提权直接拒绝）
fer index --volumes D --method mft   # 指定卷 / 强制 MFT 路径
fer index --method walk              # 显式降级（无硬链接/大小/时间，仅应急）
fer search "AGENTS.md"               # 秒搜（子串）
fer search "*.rs"                    # 通配符
fer search "ext:mp4 size:>1gb dm:thisweek"   # 过滤查询语言
fer search "foo" --limit 50 --count-only     # 只看命中数
fer serve --addr 127.0.0.1:9876      # HTTP API + 网页 UI
fer upgrade                          # 格式迁移：老 dump 就地重建 trigram 段并写为最新版（免管理员）
fer monitor --volume D               # USN 实时增量（需管理员）
fer stats                            # 索引统计
fer dupes --min-size 1kb --limit 50  # 找重复文件（同大小分组 + 内容哈希 + 字节校验）
fer dupes --name adb.exe             # 只看文件名含 adb.exe 的重复组
fer du "D:\Kita-Tools" --top 20      # 磁盘占用聚合（WizTree 式 du，见下节）
fer du "D:\" --depth 1 --top 10 --json  # 整卷顶层占用，JSON 输出
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
| `regex:^foo.*bar$` | 正则匹配文件名（大小写不敏感，全名扫描档耗时） |
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

> 注意：`parent:`/`path:` 是**朴素前缀**——`parent:D:\proj` 也会匹配 `D:\proj2`。
> 需要组件边界语义（`d:\proj` 绝不匹配 `d:\proj2`）请用 `fer du`，其底层
> `subtree_ids` 是边界感知的。

## fer du — 磁盘占用聚合（WizTree 式）

零磁盘 IO：直接从 dump 聚合，和 WizTree 从 MFT 出树同源。

```bash
fer du <root> [--depth N] [--top N] [--allocated] [--json]
```

- `<root>`：目录、卷根（`D:\`）、或单个文件；大小写不敏感，尾部分隔符可省。
- `--depth N`：只报告 root 以下 N 层的子目录（0 = 只出总数；缺省 = 不限层）。
- `--top N`：按占用降序最多报 N 个子目录（缺省 20；`truncated` 标记是否截断）。
- `--allocated`：按**分配簇字节**（磁盘实际占用，WizTree 的 "size on disk"）排序/显示；
  缺省按逻辑大小。JSON 里两种口径始终同时输出。
- `--json`：稳定字段 `root/total_bytes/total_allocated/files/dirs/entries/children[]/truncated`，
  `children[].depth` = 相对 root 的层数（root 直接子目录 = 1），`children[]` 同时带
  `size` 与 `allocated`。

语义：只统计文件（NTFS 目录记录自身无有效大小）；**硬链接按 FRN 去重只计一次**；
每个文件计入其全部祖先目录（父目录 = 自身文件 + 子目录之和）。

两种口径：`size` = 逻辑大小（`$DATA` real_size，压缩文件为解压后大小）；
`allocated` = 分配簇字节（驻留文件为 0——它们住在 MFT 记录里不占簇）。
**allocated 需要 dump v6**（`fer index` 重扫后生效）；旧 dump（v3-v5）加载时
`allocated` 回退为 `size` 近似值，`fer upgrade` 只能迁移格式、无法补回真实簇数。

## HTTP API

```
GET /api/health                     → {"ok":true}
GET /api/search?q=<query>&limit=<n> → 命中列表（带 size/mtime/ctime/flags）
GET /api/du?path=<p>&depth=<n>&top=<n>&allocated=<bool> → 目录占用聚合（WizTree 式，字段同 `fer du --json`）
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
│               （含 by_frn，monitor 二分删改）+ name_offs/path_offs 加速段 +
│               trigram 倒排段（trigrams/trig_offs/trig_posts），mmap 零拷贝
│               加载，多 term 并行求值；子串/正则走整 arena 单遍 SIMD 扫描
│               （跨 entry 边界伪命中过滤 + 命中后跳到 entry 尾），≥3 字节
│               子串先走 trigram posting 交集（候选超集，逐候选 memmem 校验）
├── store.rs    SQLite + FTS5 oracle（feature `sqlite`，仅测试交叉验证，不进生产路径）
├── query.rs    查询语言解析（纯函数 + 单测）
├── indexer.rs  build 编排：auto = 纯 MFT；usn/walk 仅显式可选（非提权硬拒绝）
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

索引构建：全盘 416.8 万条（MFT 路径含硬链接别名），**热缓存 7.9s（含 dump 写出）/ 冷盘 ~30-40s**
（bitmap 跳读 + 分片并行解析；峰值 RSS 1.4GB，`--json` 输出 `peak_rss`），收尾原子写
**FERIDX01 dump**（1418MB，写出 ~0.9s）。
- **serve**：mmap 零拷贝加载，**进程启动→listening 86ms**；HTTP 查询（热页）：
  `ext:rs` 2ms、`type:dir` 1ms、`dm:thisweek type:file` 34ms；逻辑内存 1021MB /
  RSS 350MB（mmap 按需缺页，OS 可回收）
- **CLI**：全查询 **0-294ms**（含进程启动 + dump 加载），无 SQLite、无门控
- **du**（2026-09 新增，v2 并行化）：子树 `D:\Kita-Tools\Coding`（9.5 万条目/1.8 万目录）
  **35ms**；整卷 `D:\`（300 万条目/57 万目录）聚合 **~1.2s**（v1 顺序版 2.9s）。实现 =
  FRN 去重 → 连续均分块 scoped 线程并行 → 稠密 per-dir 原子计数（无 merge 阶段），
  目录查找走 FNV 折叠哈希预筛 + 精确校验，每文件零分配；serve 内热页同查询 ~35ms
- **du 双口径实测（2026-09-01 v6 重建后）**：`D:\` 逻辑 1354.38 GB vs 分配
  1327.13 GB（压缩/稀疏省 **27.25 GB**）；`Android-Tools` 160.67→143.73 GB（镜像
  稀疏文件大户）、`Games` 78.11→66.26 GB；小文件目录 allocated 略高（簇对齐税）

### serve 稳态查询（2026-09 优化后，dump v5 含 trigram 倒排段）

第二轮热页测量（`took_ms`，引擎侧耗时，不含 HTTP/CLI 进程税）：

| 查询 | 结果数 | 旧（CLI 含启动） | v4 arena 版 | v5 trigram 版 |
|------|-------:|------------------:|------------:|--------------:|
| `rs`（2 字子串，无 trigram 走 arena） | 184,011 | 158-168ms | 22ms | **21-23ms** |
| `报告`（2 字 CJK） | 40 | 152-158ms | 7ms | **0-1ms** |
| `报`（单字 CJK = 3 字节 = 单 trigram） | 66 | ~150ms | ~11ms | **1ms** |
| `report`（长子串） | 7,325 | 167-171ms | 8ms | **0-1ms** |
| `con`（常见 trigram 压力案例） | 186,357 | ~165ms | ~10ms | **14ms** |
| `Kita-Tools\Coding`（路径子串） | 88,965 | 281-294ms | 135ms | 90-135ms |
| `regex:\.rs$`（字面量预筛） | 40,211 | ~170ms 档 | 14ms | **4ms** |
| `a?c`（glob 位并行） | 1,829 | 1,018ms | 118ms | 118-134ms |
| `zzzzzz`（trigram 缺失 → 空） | 0 | ~165ms | ~10ms | **0ms** |
| `*.rs` / `ext:rs` | 40,211 | 2ms | 0-1ms | 0-1ms（持平） |

说明：dump v4 新增 `name_offs`/`path_offs` 加速段（整 arena SIMD 扫描 + 游标映射），
v5 新增 **trigram 倒排段**（85,706 个 distinct trigram，~350MB）——≥3 字节子串先做
posting 交集得候选超集，再逐候选 memmem 校验。glob 匹配编译为 Shift-And 位并行 +
最长字面量 run 预筛。老 dump 兼容加载：v3 自动内存重建加速段，`fer upgrade`
（免管理员、~6.6s @4.14M 条）就地重建 trigram 段并重写为 v5。
首次查询含 mmap 缺页税（首轮比稳态高数十 ms）。
- **monitor**：USN 增量进内存（by_frn 二分 + 删除影子集），默认每 60s 防抖写回 dump
  （`--flush-secs` 可调）；flush 走 arena 直达复用（零 String 分配）
- **编译**：`cargo check` 6.9s（不编 SQLite）；release 1.81MB / `--profile min-size`
  1.58MB；clippy 0 警告

## 已知限制 / TODO

- 索引 = dump 快照 + monitor 增量；monitor 不在线时文件变动不反映（下次
  `fer index` 或 monitor 启动回放 USN 日志补齐）
- v3 dump 兼容加载会在启动时重建加速段（一次性 ~百 ms 级）；`fer upgrade`
  就地重建 trigram 段并重写为最新版（免管理员），`fer index` 则是全量刷新。
  v6 新增 per-entry allocated 段（磁盘占用口径）；v5- 老 dump 加载时
  `allocated` 回退为逻辑大小，真实簇数需 `fer index` 重扫 $MFT
- trigram 段只覆盖文件名（≥3 字节子串）；路径子串与 <3 字节子串走 arena 扫描
- 内存引擎的路径排序只折叠 ASCII 大小写（非 ASCII 大小写字母如 É/Ö 按字节比较）
  ——Windows 路径中极少见
- 8.3 短名（namespace=2）不入索引（避免噪音）；分片 `$MFT` 不支持 attribute-list 布局
  （报错退出，可用 `--method usn|walk` 显式降级，会丢失硬链接别名与大小/时间元数据）
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
