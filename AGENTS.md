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
- dump 二进制契约：56B repr(C) Entry + 8 字节对齐段 + 头部 17 偏移 + 6 列表计数
  （v2）；改布局必须 bump FORMAT_VERSION
- monitor：load dump → USN 增量进内存（frn 映射/删除集/追加列表）→ 默认每 60s
  防抖重建成新 dump；USN 位置存 `*.feridx.usn` 边车，崩溃靠日志回放补齐
- serve：全查询走内存引擎；`/api/rescan` 重建 + 换 dump + 热替换引擎

## 关键路径

- 源码 `src/`：mft.rs（原始 $MFT 扫描，核心）、usn.rs（回退索引 + 变更监控）、
  walk.rs（回退）、store.rs（SQLite oracle，feature `sqlite` 门控）、query.rs（查询语言）、
  indexer.rs（mft→usn→walk 编排）、monitor.rs、server.rs、mem.rs（dump 内存引擎）、
  dupes.rs、main.rs（CLI）
- 产物 `target-gnu\release\fer.exe`；默认索引库 `%LOCALAPPDATA%\file-engine-rust\index.db`
- 真实卷测试 `tests/live_volume.rs`（`#[ignore]`，需管理员）

## 构建（本机特有）

本机无 MSVC 链接器、Git 的 GNU `link.exe` 会遮蔽；必须走 GNU 工具链：

```powershell
$env:CARGO_TARGET_DIR = 'D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu'
& 'D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe' run stable-x86_64-pc-windows-gnu cargo build --release
```

`rust-toolchain.toml` 已指 GNU；`.cargo/config.toml` 固定 target-gnu。

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
- monitor 追加项**不要**塞进 frn 映射（删除集偏移会失真）——追加后未落盘即删的
  直接从追加列表 swap_remove；同窗口同路径多次创建同理去重
- release 开了 `fat LTO + codegen-units=1 + panic=abort + target-cpu=native`（本机专用）；
  另有 `--profile min-size`（`opt-level="z"`）体积最精简构建；clippy 保持 0 警告
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
