# File-Engine-Rust 项目记忆（agent 速览）

Everything 级文件搜索引擎的 Rust 重写。本文件是给后续 agent 会话的速查卡。

> ⚠️ **进行中（2026-08-31）**：性能优化会话中途额度耗尽。5 个文件已改未提交
> （mft/usn/lib/store/mem），`cargo check` 已过但**测试未跑、未实测**。
> 状态、dump 格式契约、剩余 TODO 全在 **[HANDOFF.md](HANDOFF.md)**——先读它再动手。

## 一句话

`fer`（本仓库产物）用原始 NTFS `$MFT` 扫描建索引（硬链接别名/大小/时间/属性全量），
SQLite + FTS5 trigram 毫秒查询，CLI `--json` 与 HTTP API 双通道。

## 关键路径

- 源码 `src/`：mft.rs（原始 $MFT 扫描，核心）、usn.rs（回退索引 + 变更监控）、
  walk.rs（回退）、store.rs（SQLite + 查询翻译）、query.rs（查询语言）、
  indexer.rs（mft→usn→walk 编排）、monitor.rs、server.rs、main.rs（CLI）
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
cargo test                                                   # 单元 + 端到端
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
- 本仓库有 git（commit 节点：基线/测试全绿/真实卷全绿/性能优化/内存引擎/dupes），改动前先看 `git log`

## 常用命令

```powershell
.\target-gnu\release\fer.exe search "ext:rs size:>1mb" --json
.\target-gnu\release\fer.exe search "foo" --count-only
.\target-gnu\release\fer.exe index --volumes D
.\target-gnu\release\fer.exe serve
```

查询语言、HTTP API 契约、性能数据：见 README.md（以 README 为准，本文件只是速查）。
