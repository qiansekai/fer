# File-Engine-Rust 项目记忆（agent 速览）

Everything 级文件搜索引擎的 Rust 重写。本文件是给后续 agent 会话的速查卡。

## 一句话

`fer`（本仓库产物）用原始 NTFS `$MFT` 扫描建索引（硬链接别名/大小/时间/属性全量），
**FERIDX01 dump 内存引擎**毫秒查询（SQLite 仅 feature `sqlite` 测试 oracle），
CLI `--json` 与 HTTP API 双通道。

## 数据流（重要）

- **SQLite 已退出生产路径**（store.rs 仅作测试交叉验证 oracle，`sqlite` feature
  门控——默认构建/check/clippy 完全不编译 SQLite 的 bundled C）：索引 =
  `fer index` 全盘扫描 → 内存引擎 → 原子写 **FERIDX01 dump**（`index.db.feridx`，
  ~1GB）；构建热缓存 ~11s / 冷盘 ~40-50s
- dump **mmap 零拷贝加载**（裸指针 View + `Keep` 所有权锚点，加载 ~1ms，页按需缺页）：
  serve 启动 ~135ms，CLI 全查询 17-336ms（含进程启动）
- dump 二进制契约：56B repr(C) Entry + 8 字节对齐段 + 头部 19 段偏移表 + 6 列表计数
  + **by_frn 置换段** + **name_offs/path_offs 加速段**（v4；子串/正则整 arena 扫描的
  offset→entry 游标映射）；改布局必须 bump FORMAT_VERSION。**v3 兼容加载**：老 dump
  启动时从 entries 内存重建两个加速段（AuxAccel，Keep::Mapped 持有），stderr 提示
  `fer index` 升级；v3/v4 头部长不同（200B/216B），测试里伪造 v3 要重建头部而非改版本字节
- monitor：load dump → USN 增量进内存（by_frn 二分 + 删除影子集/删除集/追加列表）→
  默认每 60s 防抖重建成新 dump（flush 走 push_arena 直达复用，零 String 分配）；
  USN 位置存 `*.feridx.usn` 边车，崩溃靠日志回放补齐
- serve：全查询走内存引擎；`/api/rescan` 重建 + 换 dump + 热替换引擎

## 关键路径

- 源码 `src/`：mft.rs（原始 $MFT 扫描，核心）、usn.rs（回退索引 + 变更监控）、
  walk.rs（回退）、store.rs（SQLite oracle，feature `sqlite` 门控）、query.rs（查询语言）、
  indexer.rs（mft→usn→walk 编排）、monitor.rs、server.rs、mem.rs（dump 内存引擎：
  **v4 + name_offs/path_offs 加速段 + 整 arena 单遍 SIMD 扫描**）、dupes.rs、main.rs（CLI）
- 产物 `target-gnu\release\fer.exe`；默认索引库 `%LOCALAPPDATA%\file-engine-rust\index.db`
- **在役稳定备份** `stable\fer.exe`（gitignored 二进制 + `stable\README.md` 记录来源
  commit/hash/dump 版本）；大改前先更新备份再动代码
- 真实卷测试 `tests/live_volume.rs`（`#[ignore]`，需管理员）

## 构建（本机特有）

本机无 MSVC 链接器、Git 的 GNU `link.exe` 会遮蔽；必须走 GNU 工具链：

```powershell
$env:CARGO_TARGET_DIR = 'D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu'
& 'D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe' run stable-x86_64-pc-windows-gnu cargo build --release
```

`rust-toolchain.toml` 已指 GNU；`.cargo/config.toml` 固定 target-gnu。
crates.io 偶尔 SSL reset：加新依赖先看 `DevEnv\cargo\registry\cache` 是否有缓存，
有则加 `--offline` 可过。

## 测试

```powershell
cargo test                                                   # 单元 + 端到端（默认不编 SQLite）
cargo test --features sqlite                                 # 追加 mem-vs-SQL 交叉验证 oracle
cargo test --test live_volume -- --ignored --nocapture       # 真实卷（管理员）
```

## 踩过的坑（勿重蹈）

- `FSCTL_GET_NTFS_VOLUME_DATA` 的 `MftStartLcn` 是**簇号**（×bytes_per_cluster），不是扇区号
- USN_RECORD_V3 的 FRN 是 16 字节 FILE_ID_128（父 FRN 在偏移 24）；V2 才是 8 字节
- `$FILE_NAME` 父引用含序列位，统一 `& 0x0000_FFFF_FFFF_FFFF` 归一化为纯记录号
- 硬链接 = 同一条 FILE 记录里的多个 `$FILE_NAME`
- **大小必须读 `$DATA` 属性的 real_size**，mtime/ctime 读 `$STANDARD_INFORMATION`(0x10)——
  `$FILE_NAME` 里的 size/时间字段是目录项缓存，NTFS 已不维护，多数用户文件为 0
- SQLite：LIKE 的索引优化要求 RHS 是字面量（不能绑定参数）；FTS5 `detail=none` 禁 phrase/column 查询
- FTS5 特殊命令 `delete-all` 只允许 contentless 表 → 重建时 DROP/CREATE 虚拟表
- 内存引擎 `partition_point` 的谓词必须单调（`starts_with` 是假-真-假会二分出垃圾值，
  用 `小于或前缀` 的假→真→假形式）；区间结果要排序后才能进交集
- **dump 视图生命周期**：mmap/owned 内存靠 `Keep` 锚点保活，Sections 是裸指针 View
  （读-only 契约）；改这个结构前先想清楚 Send/Sync 与析构顺序
- **路径子串扫描必须 CI**（paths arena 存原始大小写，memchr 定位首字节折叠变体 +
  `ci_eq_at` 校验；byte-exact memmem 会漏掉全部大写路径）
- **dump Entry 布局是二进制契约**（56B repr(C) 无填充），改字段必须 bump FORMAT_VERSION
- monitor 删除事件先用 `removed_frns` 影子集挡掉本窗口已退役的 FRN（否则
  删除-重建-删除会把旧条目重复标记删除）；追加项 flush 前不在索引里（find_frn
  查不到），同窗口删除走 `appended.swap_remove`；同窗口同路径多次创建同理去重
- 多 term 查询的 eval 在 scoped 线程上并行求值；`ScopedJoinHandle` 的 join 必须
  写在 `thread::scope` 闭包内（handle 的 env 生命周期出不了 scope）
- regex 走 `regex::bytes` 且**关 unicode feature**（名字已小写，二进制省 400KB）；
  `\p{...}` 报 "Unicode property not found"；模式 parse 时小写化 + 预校验
- release 开了 `fat LTO + codegen-units=1 + panic=abort + target-cpu=native`（本机专用）；
  另有 `--profile min-size`（`opt-level="z"`）体积最精简构建；clippy 保持 0 警告
- **整 arena 扫描必须用重叠语义手动循环**（命中后 `from=abs+1` 续扫）：non-overlap 的
  `find_iter` 会让跨 entry 边界的伪命中遮蔽与其重叠的真实命中（"xab"+"bbc" 拼成
  "xabbbcab" 时 needle "bb" 先伪后真，迭代器会跳过真的）。伪命中 +1、真命中跳到
  entry end（contains 布尔语义，每 entry 至多一次）
- **arena 命中→entry 映射用单调游标**（`while offs[e_idx+1] <= abs { e_idx += 1 }`），
  因为 `from` 只增、abs 严格递增——不要用 partition_point（每命中 22 次二分比摊还 O(1) 慢一个量级）
- 单字节 needle 无跨边界问题（abs+1 ≤ end 恒成立），memchr 单遍即可
- **regex 不能直接 arena 级 find**：贪婪匹配跨边界时（`a.*` 吃掉邻接 entry 字节）
  会遮蔽同起点的短真匹配——正确做法是 regex-syntax 抽字面量 → 整 arena 预筛候选
  （每 match 必含至少一个 extracted literal，超集过滤安全）→ 候选逐 entry 用完整
  span 校验。无字面量的模式（`[a-z]+`）回退 per-entry 扫
- **v3 兼容加载**：v3/v4 头部尺寸不同（200B/216B、偏移表 18/20 项），测试里伪造
  v3 必须重算偏移重建头部，只改版本字节会被 layout 校验拒掉
- **glob 位并行（GlobProg）**：每查询编译一次（段 ≤64 token 走 2KB 栈上 Shift-And 掩码表，
  >64 走 naive），逐候选零分配。**结尾锚定契约**：无尾随 `*` 时末段必须结束于 hay 尾
  （`*.rs` 不匹配 "main.rss"）；段间贪心 earliest-match 是安全的（更早 end 给后续段
  超集起点）。**1 字节前缀的 glob 是 contains 语义**（by_name 前缀快路径只对 ≥2 字节
  前缀生效——老 DP 遗留契约，与 SQL LIKE oracle 一致，别"顺手修正"它）。`?` 是字节级
  语义（`报?` = 4 字节模式，对 6 字节的"报告"不锚定匹配，要 `报???`）。旧 DP 保留在
  测试作 oracle（`glob_match_dp`）+ 800 例伪随机交叉验证，改匹配器必须跑它
- **glob 预筛**：最长字面量 run 作 SIMD seed（glob 无 alternation，任何 run 必现，超集
  安全）；纯星模式特判 `all_ids()` 免全扫；无字面量模式（`???`）退全扫 + 位并行验证
- 本仓库有 git（commit 节点：基线/测试全绿/真实卷全绿/性能优化/内存引擎/dupes/极致性能），
  改动前先看 `git log`

## 常用命令

```powershell
.\target-gnu\release\fer.exe search "ext:rs size:>1mb" --json
.\target-gnu\release\fer.exe search "foo" --count-only
.\target-gnu\release\fer.exe index --volumes D
.\target-gnu\release\fer.exe serve
```

查询语言、HTTP API 契约、性能数据：见 README.md（以 README 为准，本文件只是速查）。
