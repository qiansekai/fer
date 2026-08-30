# 性能优化交接文档（会话中断，2026-08-31）

> 给下一个 agent：本会话在实施「极致性能」优化时额度耗尽。**代码已通过
> `cargo check --all-targets`（零错误），但尚未跑测试、未提交、未做性能实测。**
> 按本文件继续即可。改动前先 `git diff` 总览（5 个文件已改，均未提交）。

## 一、任务背景与已确认决策

用户目标：**优化到极致性能**。已确认的关键决策：

1. **cargo 编译耗时不算性能预算**——`fat LTO`、`codegen-units=1`、`panic=abort`、
   `target-cpu=native` 都可以开（还没改，见 TODO 7）。
2. **`fer index` 建索引时间算性能目标**。
3. 路线是三步走的「dump 优先、SQLite 降级为构建/回退层」：
   - 步骤 1（本次做）：构建收尾写 FERIDX01 dump；serve/CLI 优先加载 dump，
     回退 SQLite——解决 serve 启动 15s 和 CLI 路径子串 ~1s 两个痛点。
   - 步骤 2（本次做一半）：dump 写入已实现，接线（indexer/server/main）未完成。
   - 步骤 3（**明确推迟**，本次不做）：monitor 改写内存索引 + USN 追平 + 低频落盘。
     SQLite 的写路径暂时保留。
4. **不**给内存引擎加 trigram 倒排（~150-300MB 内存换 <10ms 不值得，现状 65ms 已接近
   内存带宽边界）。
5. serve 混合分发逻辑保留：≥3 字子串仍走 SQLite FTS5（12-25ms 更快），其余走内存。

## 二、基线性能数字（优化前，6 卷 · 全盘 ~415 万条）

- 构建 107s = MFT 扫描 43s + SQLite 落库 64s；峰值内存 ~667MB；index.db 3.4GB
- serve 内存引擎加载 15s / 970MB（逐行从 SQLite 搬运 + 6 个排序串行）
- 查询：`*.rs` 16ms(SQL)/0ms(mem)；2 字 CJK 67ms；CLI 路径子串（含 `\` 的裸词）
  **~1s**（instr 全扫描，README 已知限制）；`dm:thisweek` 170 万命中 29-39ms
- 预期优化后：构建 ~35-50s；serve 启动 <2s（见 TODO 3 的 mmap 说明）；
  CLI 慢查询 ~65ms；所有查询 ≤65ms

## 三、已完成改动（git diff 可见，全部未提交）

### src/mft.rs —— MFT 扫描提速（预期 43s → ~10s）
- `read_mft`：原来每 4KB 簇一次 ReadFile（百万次 syscall）+ 中间 buffer 两次拷贝；
  现在把**同一 run 内的连续区间合并成一次大读**（4MB chunk），直接 `read_raw_into`
  读进目标缓冲，零中间拷贝。$MFT 几乎总是单一长 run。
- `apply_fixups_inplace`：USA fixup 原地修补（原来每条记录 `to_vec()` 整块拷贝，
  400 万条 = 4GB 浪费 memcpy）。保留拷贝版 `apply_fixups` 供 record 0 / 单测使用。
- `scan()`：属性解析从两遍（STD/DATA 一遍 + FILE_NAME 一遍）合并为**单遍** match；
  4MB chunk（原 1MB）；新增 `utf16_name()` 用 510 字节对齐栈缓冲解码 UTF-16
  （NTFS 名 ≤255 单元），替代逐字节组装。
- ⚠️ 无害警告：`FileHeader` 的 `usa_off/usa_count` 字段现在只写不读（inplace 版直接
  从字节重读），可留可删。

### src/usn.rs —— TreeBuilder 输出顺序优化
- children 排序键从 `parent FRN` 改为 `(parent FRN, name bytes)`：DFS 输出变成接近
  字典序，SQLite `path UNIQUE` btree 与索引重建的插入局部性大幅改善。
- 新增 `name_bytes()` 私有方法。

### src/lib.rs —— ASCII 快路径
- 新增 `fold_lower(s)`：`is_ascii()` 时走 `to_ascii_lowercase`（否则全 Unicode 小写化
  在 4M 行 × 4 字段上占大头）。
- 新增 `lower_rev(s)`：小写反转名；ASCII 走字节反转（多字节 CJK 仍按 chars 反转）。

### src/store.rs —— 落库提速（预期 64s → ~35-40s）
- **FTS5 从逐行插入改为 commit 时一次性 `INSERT INTO files_fts(files_fts)
  VALUES('rebuild')`**（外部内容表的排序合并路径，比逐行维护 posting 快 2-3 倍）。
  `Rebuild` 结构体删掉 `fts_stmt` 字段；`begin_rebuild` 不再 prepare FTS 语句。
- `Rebuild::insert` 用 `fold_lower`/`lower_rev`；`upsert`/`search` 同样替换。
- rebuild 前设 `PRAGMA temp_store=FILE`（4M 行 trigram 重建防内存爆），
  cache_size 提到 -262144 (256MB)。
- ⚠️ `store.rs:889` warning（测试 helper `seed` 的多余 mut）顺手可修。

### src/mem.rs —— 重写（dump + 并行 finalize）
上一个会话留下的半成品（`finalize` 只有调用没有定义，**原状态编译不过**）已补全：

- **`Entry` 改为 `#[repr(C)]` 56 字节定长**（原来 64B 含填充），字段按 u64×4 → u32×4 →
  u16×3 → u8×2 排列无填充；`const` 断言 size==56。**这个布局是 dump 二进制契约的一部分。**
- **`finalize()`**：6 个排序置换（by_path/by_name/by_rev/by_size/by_mtime/by_ctime）+
  6 个属性 id 列表（dir/file/hidden/system/readonly/reparse）用
  `std::thread::scope` **并行排序**（4M 条 ~0.5s，原串行 15s 的大头之一）。
  ⚠️ 注意 `let e: &[Entry] = &entries;` 必须在 scope 外声明（曾因 scope 内局部引用
  报 E0373）。
- **`save(path)`**：把成品 MemIndex 原样序列化（sections 见下），写 `.tmp` 后 rename
  原子替换，`sync_all` 后落盘。
- **`load_dump(path)`**：顺序读回（单个 `read_pod` 每段一次 read_exact），校验 magic/
  version/offset 布局。**注意：当前实现是读进堆，不是 mmap**——4M 条 ~1GB 从页缓存
  读 ~0.5s，冷盘 ~1-2s；真零拷贝 mmap 需要把 MemIndex 改成借用 mmap 缓冲，留作升级。
- **`dump_path(db)`**：`index.db` → `index.db.feridx`；**`dump_is_fresh(db)`**：dump
  mtime ≥ max(db, db-wal) mtime 才算新鲜（monitor 写入经 WAL 会更新 mtime）。
- `MemBuilder::push`/`MemIndex::load`（SQL 路径，保留作回退）改用 fold_lower/lower_rev。
- 顶部模块文档注释里 "~40 bytes/entry" 已过时（实际 56B），顺手可改。

## 四、FERIDX01 dump 二进制格式（v1，勿破坏）

```
Header 168B: MAGIC "FERIDX01"(8) | version u32(4) | n_entries u32(4) |
             created i64(8) | reserved i64(8) | 17×u64 section offsets
Sections 16 个，顺序固定，offset[16] == 文件长度:
  0 entries   n×56B (repr(C) Entry 原样)
  1 paths     字节场（原大小写全路径）
  2 names     字节场（小写名）
  3 revs      字节场（小写反转名）
  4-9  by_path / by_name / by_rev / by_size / by_mtime / by_ctime  各 n×u32
  10-15 dir/file/hidden/system/readonly/reparse_ids  各 k×u32（k 由相邻 offset 差算出）
```
所有多字节值 little-endian。`pod_bytes()`/`read_pod()` 在 mem.rs 底部（unsafe 注释
已写明 POD 依据）。改 Entry 布局必须 bump FORMAT_VERSION。

## 五、剩余 TODO（按优先级，接续做）

1. **`cargo test` 全绿**（check 已过，测试未跑）。若有失败优先修 mem.rs/store.rs 的新逻辑
   （FTS5 rebuild 时序、dump 往返一致性——可加一个 save→load_dump→search 对比单测）。
2. **indexer.rs 接线（核心剩余工作）**：
   - `indexer::build` 里让 DFS 的 on_entry 回调**同时**喂 `MemBuilder::push(path, meta)`
     （与 `rb.insert` 并行喂；index_mft/index_usn/index_walk 三处都要）。
   - `rb.commit()` 后 `builder.finish()` → `mem.save(&mem::dump_path(db_path))`，
     eprintln 计时。注意：MemIndex 需要 `db_path()`——Store 已有该方法。
   - **内存权衡**：构建期间多持 ~970MB（TreeBuilder ~220MB/卷 + MemBuilder 堆），
     峰值 ~1.2-1.5GB，可接受；替代方案（commit 后 `MemIndex::load(conn)` + save）会
     给每次构建 +17s，不要。
   - **多卷并行扫描**（原计划）可降级为可选项：收益 = 各卷扫描时间取 max；但 auto 的
     mft→usn→walk 回退逻辑让并行化复杂。建议先把串行版跑通测数，够快就不做。
3. **server.rs**：`serve()` 加载 mem index 时先试 `dump_is_fresh(db_path)` &&
   `MemIndex::load_dump(dump_path(db_path))`，失败/不新鲜回退现有 `load_mem`（SQL 路径）。
   `search` handler 里的 `mem.search()` 包进 `tokio::task::spawn_blocking`（70ms 扫描
   不该占 executor 线程）。
4. **main.rs**：CLI search 的 dump 使用要**按查询类别门控**，不要无脑加载：
   - 快查询（FTS5 可覆盖：≥3 字子串、后缀、parent/path 前缀、size/dm 区间）仍走
     SQLite（12-25ms），加载 1GB dump 反而更慢；
   - 慢扫描查询（2 字子串、路径子串、复杂通配）才加载 dump 走内存 SIMD（~65ms）。
   - 门控可用 `mem::MemIndex::prefers_sql(q)` 的反函数思路 + PathSubstr/短子串特判；
     注意冷启动首次加载 dump 的缺页成本，实测后定阈值。
5. **Cargo.toml**：
   ```toml
   [profile.release]
   lto = "fat"
   codegen-units = 1
   panic = "abort"
   ```
   `target-cpu=native` 放 `.cargo/config.toml` 的 `[build] rustflags = ["-C",
   "target-cpu=native"]`（自用机器 OK，exe 将不可分发——用户已知情同意）。
6. **README/AGENTS.md 文档同步**：性能数字、已知限制（CLI 路径子串 1s 一条可删）、
   dump 机制说明。
7. **实测**：`fer index` 前后对比（目标 <50s）、`fer serve` 启动时间、查询表重测；
   真实卷测试需管理员：`cargo test --test live_volume -- --ignored --nocapture`。
8. 全部完成后 git commit（信息风格参考 `git log`，中文短行）。

## 六、构建/测试命令（本机特有，bash 里 rustup 不在 PATH）

```bash
'/d/Kita-Tools/DevEnv/cargo/bin/rustup.exe' run stable-x86_64-pc-windows-gnu cargo check --all-targets
'/d/Kita-Tools/DevEnv/cargo/bin/rustup.exe' run stable-x86_64-pc-windows-gnu cargo test
'/d/Kita-Tools/DevEnv/cargo/bin/rustup.exe' run stable-x86_64-pc-windows-gnu cargo build --release
# 产物 target-gnu/release/fer.exe
```
（PowerShell 写法见 AGENTS.md。rust-toolchain.toml 已锁 GNU 工具链。）

## 七、本会话踩过的坑（新增）

- `str::bytes().rev().collect::<String>()` 编不过（String 不实现 FromIterator<u8>）——
  用 `as_bytes().to_vec()` + `reverse()` + `from_utf8`。
- `thread::scope` 的 spawn 闭包捕获 **scope 闭包体内声明的局部引用** 会报 E0373——
  把共享切片引用提到 scope 外声明。
- `usize.min(u64)` 类型不匹配——字面量可推断，`const CHUNK: usize` 不行，先 `as u64`。
- FTS5 'rebuild' 命令仅用于外部内容表（本仓库就是 content='files'）；
  AGENTS.md 里旧坑「delete-all 只许 contentless」不冲突。
- dump 新鲜度判断必须把 `index.db-wal` 的 mtime 一起算（monitor 的写会先进 WAL）。
