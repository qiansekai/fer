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
- dump 二进制契约：56B repr(C) Entry + 8 字节对齐段 + 头部 23 段偏移表 + 6 列表计数
  + **by_frn 置换段** + **name_offs/path_offs 加速段** + **trigram 倒排段** +
  **alloc 段**（v6 追加：per-entry 分配簇字节 u64，置末尾——Entry 布局不动，
  v3/v4/v5 零拷贝兼容不破坏）（trigrams 排序去重 u32 / trig_offs 累计**终点**偏移 /
  trig_posts 按 entry 序）。改布局必须 bump FORMAT_VERSION。**v3/v4/v5 兼容加载**：
  v3 内存重建加速段（AuxAccel，Keep::Mapped 持有），v4/v5 直接映射各段；v3/v4
  trigram 段为空 → 查询自动回退 arena 扫描；**v5- 无 alloc 段 → meta_at 回退
  allocated=size**（近似，真实簇数只有 `fer index` 重扫 $MFT 才有）。**`fer upgrade`**
  免管理员格式迁移：load → build_missing_trigrams（trig 三元组挂到 AuxAccel.trig）→
  save 重写最新版（实测 4.14M 条 ~6.6s；老 dump 升级时 alloc 段写 size 近似值）
- **dump 段尺寸校验必须容忍对齐 padding**：u32 段在 n×4 不是 8 的倍数时有 ≤7B padding
  （真实 dump n 为偶数从未触发过，n=3 的测试炸了）——用 `bytes >= logical && bytes-logical < 8`
- monitor：load dump → USN 增量进内存（by_frn 二分 + 删除影子集/删除集/追加列表）→
  默认每 60s 防抖重建成新 dump（flush 走 push_arena 直达复用，零 String 分配）；
  USN 位置存 `*.feridx.usn` 边车，崩溃靠日志回放补齐
- serve：全查询走内存引擎；`/api/rescan` 重建 + 换 dump + 热替换引擎

## 关键路径

- 源码 `src/`：mft.rs（原始 $MFT 扫描，核心）、usn.rs（回退索引 + 变更监控）、
  walk.rs（回退）、store.rs（SQLite oracle，feature `sqlite` 门控）、query.rs（查询语言）、
  indexer.rs（build 编排：auto=纯 MFT，非提权硬拒绝；usn/walk 仅显式降级）、monitor.rs、server.rs、mem.rs（dump 内存引擎：
  **v4 + name_offs/path_offs 加速段 + 整 arena 单遍 SIMD 扫描**）、du.rs（磁盘占用
  聚合：subtree_ids 边界感知枚举 + FRN 去重 + 祖先 roll-up）、dupes.rs、main.rs（CLI）
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
- **trigram 倒排段**：`trig_offs` 存的是每个 trigram posting 的**累计终点**（offs[0]=0，
  换 trigram 时 push 上一个的 end，循环后 push 最后一个的 end）——写成"起点"语义会
  让首个 trigram 的区间变成空（debug 血案）。trigram 只是超集过滤，**必须逐候选
  memmem 校验**（含 trigram 的假阳性）；常见 trigram（如 "con"）posting 巨大，
  校验成本线性于候选数（实测 186K 候选 14ms，可接受）。构建走**三遍计数/填充**
  （HashMap counts → 前缀和 → entry 序填充，posting 天然升序），per-entry 去重用
  256 槽 epoch 开放寻址——零排序零 pair 存储，峰值省 ~270MB
- **MFT 扫描构建层（2026-09）**：`$MFT::$BITMAP` 跳读（1024 记录/块，全零块不读不解析）
  + 8MB 批分片并行解析（scoped 线程，主线程按序 emit）。**血案**：①`$BITMAP` 可能是
  驻留属性（小卷）——只信 `non_resident`，解析失败静默禁用跳读，绝不让 bitmap 问题
  挂掉整卷（曾把 G:/H: 打进 USN 回退、丢硬链接别名）②**卷句柄裸读必须扇区对齐**
  （bitmap 长度不是扇区倍数 → error 87）——读取长度按扇区向上取整再 truncate
  ③MftScanner 持有裸 HANDLE（!Sync），并行闭包只许拷入 record_size/sector_size 等
  几何参数，不许捕获 &self
- `parent:`/`path:` 是**朴素前缀**（`parent:D:\proj` 会误匹配 `D:\proj2`，README 已明示）；
  **边界感知枚举用 `MemIndex::subtree_ids`**（du.rs 使用；前缀需 caller 折叠小写，
  尾分隔符内部 trim）。改 parent: 语义前先看 README 契约与 du 测试
- **du 聚合（v2 并行版）**：FRN 键置顶位（`frn | 1<<63`）与无 FRN 条目的 entry-id 键（<2^32）
  不冲突；目录表 = FNV-1a 折叠哈希预筛 + `ci_eq` 精确校验（哈希碰撞零风险）；增量哈希遍历时
  **目录前缀不含分隔符本身**（查表用 `&raw[..i-1]` 配处理当前字节前的 `before` 哈希——
  血案：把分隔符算进前缀导致与目录键全部失配、children 全空）；聚合 = 连续均分块 scoped
  线程 + 稠密 per-dir 原子数组（无 merge；**按一级目录分桶会失效**——单目录占 87% 文件时
  一个线程单扛全部查找）
- **构建门禁（2026-09）**：`indexer::build` 对 auto/mft/usn 做 `is_elevated()` 硬检查
  （`Win32_Security` OpenProcessToken + TokenElevation），非提权直接 bail——**auto 曾经
  MFT→USN→walk 静默降级**，非提权跑 `fer index` 会悄悄用 walk 覆盖好 dump（丢硬链接/
  大小/时间）；现在降级必须显式 `--method walk`。monitor 同款门禁。`/api/rescan` 走
  build() 自动继承。测试：`elevation_gate`（环境无关）；真实非提权验证 = 普通终端跑
  `fer index` 应瞬间拒绝
- **allocated 口径（v6）**：`$DATA` 非驻留头 allocated@+40 / real@+48（mft.rs）；**驻留文件
  allocated=0 是真实语义**（住在 MFT 记录里不占簇，与"未知"区分靠 dump 版本）；du 双口径
  始终在 JSON 里（`total_bytes`/`total_allocated`、children 的 `size`/`allocated`），
  `--allocated` 只切排序/文本显示口径
- 本仓库有 git（commit 节点：基线/测试全绿/真实卷全绿/性能优化/内存引擎/dupes/极致性能），
  改动前先看 `git log`

## 常用命令

```powershell
.\target-gnu\release\fer.exe search "ext:rs size:>1mb" --json
.\target-gnu\release\fer.exe search "foo" --count-only
.\target-gnu\release\fer.exe index --volumes D
.\target-gnu\release\fer.exe serve
.\target-gnu\release\fer.exe upgrade   # 老 dump 免管理员迁移到最新版（重建 trigram 段）
.\target-gnu\release\fer.exe du "D:\Kita-Tools" --top 20        # 磁盘占用聚合
.\target-gnu\release\fer.exe du "D:\" --depth 1 --top 10 --json # 整卷顶层
.\target-gnu\release\fer.exe du "D:\" --top 10 --allocated      # 按磁盘占用（需 fer index 重建后才有真实簇数）
```

查询语言、HTTP API 契约、性能数据：见 README.md（以 README 为准，本文件只是速查）。
