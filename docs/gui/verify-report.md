# fer-gui 独立验收报告（verifier / task-6）

- 项目：`D:\Kita-Tools\Coding\fer-gui`（独立 git 仓库）
- 验收对象：工作树 HEAD 状态的源码 + 本轮自建产物（未修改任何 src/*.rs、Cargo.toml、build.rs、app.manifest、app.rc）
- 构建/运行环境：Windows，GNU 工具链（`rustup run stable-x86_64-pc-windows-gnu`），fer serve 常驻 `127.0.0.1:19876`
- 证据脚本（本人撰写，全部位于写作用域 `_build/verify/`）：
  `inspect.ps1`（注入查询 + 跨进程读回控件文本/ListView 全部单元格）、`sort_probe.ps1`（单进程原子：注入→快照→点列头→快照）、
  `click_header.ps1`、`compare.ps1`（GUI vs HTTP API vs fer CLI 三方对照）、`wait_status.ps1`（状态栏时序）、
  `config_probe.ps1`、`mem_probe.ps1`、`dblclick_reveal.ps1`、`serve_button.ps1`、`stub_serve.ps1`（受控 fer 桩）、`gui_ctl.ps1`。
  说明：不依赖截图 OCR —— 用 `SendMessage(WM_GETTEXT)` 取控件文本、用 `VirtualAllocEx + LVM_GETITEMTEXT/HDM_GETITEM` 取表格单元格，得到**逐字**证据；截图用于视觉项佐证。

## 0. 判定

**FAIL —— 存在 1 个阻塞级缺陷：主窗口无法关闭（点 X / Alt+F4 均无效，进程永久驻留）。**

除此之外，数据面与交互面的 9 项验收**全部 PASS**（含四组查询三方口径一致、三态排序、du 聚合、6 类边界查询、断链自愈、20 连查询无泄漏、配置持久化、双击资源管理器定位到文件）。
未覆盖项见 §13。

## 1. 构建门禁 —— PASS [MEASURED]

命令（均为独立 target dir，避免与其它 agent 抢锁）：

```powershell
& "D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe" run stable-x86_64-pc-windows-gnu cargo test    --offline --target-dir _build\verify
& "D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe" run stable-x86_64-pc-windows-gnu cargo clippy  --all-targets --offline --target-dir _build\verify -- -D warnings
& "D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe" run stable-x86_64-pc-windows-gnu cargo build   --release --offline --target-dir _build\verify-release
```

实测：

| 门禁 | 结果 | 证据 |
|---|---|---|
| cargo test | `TEST_EXIT=0`；`103 passed; 0 failed; 3 ignored`（单测）+ `26 passed`（tests/query_state.rs）+ `3 ignored`（tests/live_serve.rs） | `_build/verify/test.log` |
| cargo clippy --all-targets -- -D warnings | `CLIPPY_EXIT=0`，日志中 `^warning` 计数 = **0** | `_build/verify/clippy.log` |
| cargo build --release | `RELEASE_EXIT=0`，产物 `_build/verify-release/release/fer-gui.exe` = **505,344 字节**（< 3 MB） | `_build/verify/build-release.log` |
| cargo build (debug，用于实机) | `Finished dev profile`，`_build/verify/debug/fer-gui.exe` = 20,317,262 字节 | `_build/verify/build-debug.log` |

## 2. 视觉 / 资源清单 —— PASS [MEASURED]

- 窗口出现且标题为 `fer GUI - File Express Retriever`：截图 `_build/verify/shot-search1.png`、`shot-du.png`。
- **COMCTL32.dll 解析到 WinSxS v6**：
  `C:\Windows\WinSxS\amd64_microsoft.windows.common-controls_6595b64144ccf1df_6.0.19041.6456_none_60b8a6cb71f64256\comctl32.dll`
  （命令：`(Get-Process -Id <pid>).Modules | ? ModuleName -eq 'COMCTL32.dll'`）
- **列头 5 列齐全**（HDM_GETITEMCOUNT=5，逐列文本由 HDM_GETITEMW 读回）：
  `名称 | 路径 | 大小 | 修改时间 | 类型`
- 控件为现代样式（v6 视觉样式 + `LVS_EX_FULLROWSELECT|DOUBLEBUFFER|GRIDLINES`）：见截图，按钮/表头为扁平现代绘制，非 Win95 经典样式。

## 3. 正确性对照（GUI vs HTTP API vs fer CLI）—— PASS [MEASURED]

命令：`pwsh -File _build\verify\compare.ps1 -TargetPid <pid> -Query "<q>"`
对照口径：GUI 实际发出的请求 = `/api/search?q=<enc>&limit=2000&sort=name`（limit 来自 config 默认 2000；升序时省略 `desc`）。

| 查询 | /api/search total | `fer search "<q>" --count-only` | GUI 状态栏 | GUI 首行 path == API hits[0].path |
|---|---|---|---|---|
| `ext:rs size:>1mb` | 56 | 56 | 命中 56 条（显示 56） | 是（avx512f.rs 全路径逐字一致） |
| `main*.rs` | 147 | 147 | 命中 147 条（显示 147） | 是 |
| `type:dir` | 528,112（count=2000 截断） | 528,112 | 命中 528,112 条（显示 2000） | 是 |
| `报告` | 84 | 84 | 命中 84 条（显示 84） | 是 |

另：**受控桩渲染**（`stub_serve.ps1` 返回构造好的 JSON）验证了「字段 -> 界面」映射：total=999 → `命中 999 条（显示 2）`；`took_ms=7` → `引擎 7 ms`；size=1234 → `1.21 KB`；mtime=1750000000 → `2025-06-15 23:06`（UTC+8 正确，且与 API 的 unix 秒一致）；is_dir=true → 大小列为空、时间为 `-`、类型为「文件夹」。

## 4. 排序三态 —— PASS [MEASURED]

方法：`sort_probe.ps1` 单进程内用 `HDM_GETITEMRECT` 取「大小」列矩形（rect=(800,0,890,24)），向 SysHeader32 投递真实 WM_LBUTTONDOWN/UP，触发 LVN_COLUMNCLICK；每次点击后立刻读取全部单元格。
基线：config 预置 `sort_key=name, sort_desc=false`。

| 步骤 | GUI 首行 path | 对应 API | 一致 |
|---|---|---|---|
| 注入后（name 升序） | `...\stdarch\...\x86\avx512f.rs` | `sort=name` 首条 = 同一路径 | 是 |
| 点「大小」第 1 次（size 升序） | `...\windows-0.58.0\...\Certificates\impl.rs` | `sort=size` 首条 = 同一路径 | 是 |
| 点「大小」第 2 次（size 降序） | `...\windows-0.62.2\...\Debug\Extensions\mod.rs`（3.90 MB） | `sort=size&desc=1` 首条 = 同一路径 | 是 |
| 点「大小」第 3 次（回 Name 默认） | 回到 avx512f.rs | `sort=name` 首条 = 同一路径 | 是 |

三态「升 -> 降 -> 回 Name 默认」与服务端 `--sort` 结果逐步一致。

## 5. du 模式 —— PASS（数据面）/ 见 §7 展示层疑点 [MEASURED]

注入 `du D:\Kita-Tools\Coding`，GUI 走 `/api/du?path=...&depth=1&top=300&allocated=false`。

| 项 | API | GUI |
|---|---|---|
| 合计 | total_bytes=15,734,144,882 | `合计 14.65 GB`（=15,734,144,882/1024^3，一致） |
| 分配 | total_allocated=15,875,715,072 | `分配 14.79 GB`（一致） |
| 文件/目录 | 94,197 / 16,509 | `94,197 文件 · 16,509 目录` |
| truncated | true | `· 已截断` |
| children | 57 条 | LISTROWS=57；前 5 条 path/大小逐字一致（fer 4.55 GB / kita-console 2.72 GB / fer-gui 1.40 GB / tg-channel-harvest 1.37 GB / nemophila-avatar 832.44 MB） |

截图：`_build/verify/shot-du.png`。

## 6. 边界查询 —— PASS（6/6 不崩不卡）[MEASURED]

每种用例：注入 → 等 3 s → 读状态栏与行数 → 检查进程存活/响应。

| 用例 | GUI 状态栏 | 进程 | 与 API 对照 |
|---|---|---|---|
| 空查询 `` | `就绪`（清空结果，不发请求） | 存活/响应 | n/a（设计如此） |
| 超长 267 字符 | `命中 0 条（显示 0）` | 存活/响应 | API total=0 |
| 非法 `size:>abc` | `查询出错：bad size 'abc' (use e.g. 1kb 500mb 2gb, with >, < or a-b)` | 存活/响应 | 与 API `error` 文本逐字一致 |
| CJK `报告` | `命中 84 条` | 存活/响应 | API total=84 |
| 取反 `!hidden:true` | `命中 4,276,101 条（显示 2000）` | 存活/响应 | API total=4,276,101 |
| 含引号/反斜杠 `parent:"D:\Kita-Tools\Coding"` | `命中 0 条` | 存活/响应 | API total=0（引号语义属 fer，GUI 原样透传） |
| 裸反斜杠路径 `D:\Kita-Tools\Coding\fer-gui\src` | `命中 9 条（显示 9）` | 存活/响应 | API total=9 |

## 7. 断链行为与自愈 —— PASS [MEASURED]

用 `FER_GUI_ADDR` 指到未监听端口（先 `127.0.0.1:19`，后 `127.0.0.1:28877`）：

- 启动后 **1,462 ms / 1,555 ms** 状态栏出现 `serve 未连接（<addr>）`（远小于 4 s；轮询周期 TIMER_STATS_MS=4000）。
- 断链下搜索：`请求失败：网络错误: 连接 127.0.0.1:19 失败: 由于目标计算机积极拒绝，无法连接。 (os error 10061)（fer serve 是否在运行？）`，进程 `Responding=True`，未崩溃、未卡死。截图：`_build/verify/shot-offline.png`。
- **自愈**：在该实例指向的死端口上启动受控 fer 桩（`stub_serve.ps1 -Port 28877`）后，**不重启 GUI**，4 s 轮询周期内右侧索引标签自动从 `serve 未连接` 变为 `索引 123,456 条 · 42 MB · 实时 Δ1 / 待删 2 · 127.0.0.1:28877`；随后搜索正常出结果（截图 `shot-stub.png`）。
- 「启动服务」按钮（`BM_CLICK` 等价真实点击）：本次为了让会话在 8 s 内结束，把 `FER_GUI_FER` 指向 `cmd.exe`（**确保不会真的拉起 fer serve**），实测状态机：`+436ms 正在启动 fer serve…` → `+11060ms 已启动 fer serve（pid 8692），但 8000 ms 内未就绪：serve 未连接（连接 127.0.0.1:28877 失败：connection timed out）`，进程存活。**注意：本项未验证「真起 fer serve 并连上」的成功分支**（会与本机常驻索引实例共存，风险不可控，故刻意规避），该分支记 [UNKNOWN]。
- 恢复链的另一半（真 serve 在线）：主验收实例全程 4 个查询 + 20 连查询均正常，等价于「恢复后可用」。

## 8. 进程资源（20 连查询）—— PASS（无泄漏迹象）[MEASURED]

命令：`pwsh -File _build\verify\mem_probe.ps1 -TargetPid <pid> -Rounds 20 -PerQueryWaitMs 700 -SampleEvery 2`

| 采样点 | WorkingSet | WorkingSet Private | Handles |
|---|---|---|---|
| baseline | 17,014,784 | 57,360,384 | 205 |
| after#6（峰值） | 20,480,000 | 61,751,296 | 205 |
| after#20 | 17,485,824 | 58,617,856 | 205 |
| settled+3s | 17,485,824 | 58,617,856 | 205 |

结论：私有内存 54.7 → 58.9 → 55.9 MB，**非单调增长**，20 次查询后回落到 baseline +2.4%；句柄数恒定 205。无泄漏迹象。

## 9. 配置持久化 —— PASS [MEASURED]

命令：`pwsh -File _build\verify\config_probe.ps1 -TargetPid <pid> -Query "main*.rs" -NewX 200 -NewY 150 -NewW 1100 -NewH 700`

1. 改几何 + 输入查询后发 `WM_CLOSE`（触发 `wm_close` 里的 `save_config`），`%LOCALAPPDATA%\fer-gui\config.json` 实测：
   `{"limit":2000,"last_query":"main*.rs","sort_key":"name","sort_desc":false,"win_x":200,"win_y":150,"win_w":1100,"win_h":700}` —— 几何与查询逐字段精确落盘。
2. 重启后：`GetWindowRect = (200,150,1100,700)`（与配置一致），查询框恢复为 `main*.rs`，并自动触发搜索 `命中 147 条`（= API/CLI 的 total）。
3. 早前一轮也验证过 `sort_key/sort_desc` 参与恢复（把 config 预置 `name/false` 后启动，首行即 name 升序首条）。

## 10. FAIL 清单

### FAIL-1（阻塞）主窗口无法关闭，进程永久驻留

**最小复现**（二选一，均实测）：

```powershell
# A. 用等价于「点 X / Alt+F4」的系统命令
$hwnd = <fer-gui 主窗口句柄>
[user32]::PostMessage($hwnd, 0x0112, [IntPtr]0xF060, [IntPtr]::Zero)   # WM_SYSCOMMAND / SC_CLOSE
Start-Sleep 4; Get-Process -Id <pid>            # 仍然存活
# B. 直接发 WM_CLOSE
[user32]::PostMessage($hwnd, 0x0010, 0, 0)
```

实测（pid 16204，2026-09-30）：
- 发 `SC_CLOSE` 4 s 后：`进程存活=True 窗口 IsWindow=True Visible=True`；再发一次 `WM_CLOSE`，4 s 后依旧 `IsWindow=True`。
- 窗口仍在**正常工作**（4 s 索引轮询继续刷新：状态栏 `实时 Δ83 → Δ84` 有变化），不是假死。
- 副作用：配置**确实**被写盘（说明 `wm_close` 回调执行了），但窗口没被销毁 —— 即「关不掉、但每次尝试关闭都会写一次配置」。
- 因 `run_main` 永不返回，进程只能被 `Stop-Process` 结束；本轮我自己起的 4 个实例全部靠强杀清理（`scripts\verify.ps1` 同样只能用 `Stop-Process -Force`）。

**根因（源码级，构建无关）**：
1. `src/main.rs:295` 注册了 `wm_close` 事件，回调体只有 `app.save_config()`，**没有** `DestroyWindow`。
2. winsafe 的 `wm_close` 由宏 `fn_wm_noparm_noret!` 生成（`winsafe-0.0.29/src/macros/gui_events.rs:4-19`）：回调返回 `Ok(self.wnd_ty().def_proc_val())`；
   非对话框窗口是 `WndTy::Raw → def_proc_val() = 0`（`src/gui/globals_priv.rs:29-34`）。
3. `raw_base.rs:180-186`：`if let Some(user_ret) = user_ret { Ok(user_ret) } else { DefWindowProc(p) }` —— 一旦有用户回调就**短路 DefWindowProc**；而 `WM_CLOSE` 的默认处理（`DefWindowProc`）才是 `DestroyWindow`。
4. winsafe 自己的文档也写明「handle 该事件会覆盖默认处理」，且默认处理只列了 dialog `WindowMain` / `WindowModal`（`base_wnd_events.rs:271-277`）—— 本项目的非 dialog `WindowMain::new`（`main.rs:123`）不在其中。
5. 因此 `WindowMain::close()`（内部只是 `PostMessage(WM_CLOSE)`）在本项目里同样无效。

**修复建议（交给 owner，我不动源码）**：在 `wm_close` 回调里保存配置后显式 `self.wnd.hwnd().DestroyWindow()?;`（winsafe 已有该绑定），或改为 `WM_DESTROY` 里落盘 + 不注册 `wm_close`。

## 11. 非阻塞疑点（不影响判定，建议顺手处理）

1. **[MEASURED] du 模式表头语义错位**：`render_du`（`main.rs:490-524`）复用同一张 5 列表，但依次填入 `名称/路径/大小/文件数/子目录数` —— 于是**「修改时间」列显示的是文件数、「类型」列显示的是子目录数**。实测行：`fer || D:\...\fer || 4.55 GB || 11,630 || 0`（截图 `shot-du.png` 同样可见）。
2. **[MEASURED] du 模式「类型」列恒为 0**：`/api/du` 的 `children[]` 实测只有 `path/size/allocated/files/depth`（无 `dirs`），`parse_du_node` 回退默认值 0，故该列永远 0。建议该列在 du 模式下改显 `depth` 或隐藏。
3. **[静态] 右键菜单行定位用视图行号索引 `hits`**：`on_context_menu` 把 `p.iItem`（视图行号）直接当 `state.hits` 下标（`main.rs:707-717` + `selected_path`）。正常路径下两者一一对应；但 `render` 中 `items().add()` 失败时 `shown` 不递增，视图行号会与 `hits` 下标错位，可能对**错误的文件**执行「打开/定位/复制」。建议改用行 lParam（已是结果序号）反查。
4. **[静态] `du` 前缀劫持**：`start_search` 用 `strip_prefix("du ")/strip_prefix("du:")` 判定 du 模式（`main.rs:440`）；若 fer 查询语法里出现以 `du` 开头的合法 term 会被误判。当前 fer 词表无此 term，属潜在耦合。
5. **[静态] `save_config` 只在 `wm_close` 落盘**：因 FAIL-1，窗口关不掉时配置靠「点一次 X」写盘；若用户直接杀进程，窗口几何/最后查询不会保存。
6. **[环境，非 GUI] 索引缺口**：验收期间 Lead 实测 `AGENTS.md`/`src\main.rs` 等本仓库文件在 rebuild 前查不到（entries 4,272,588 → 4,276,741 后回归），判定为 fer 「monitor 停机窗口变更永久缺席」（TODO T-08）。本轮所有对照均在 rebuild 后索引上进行，**与 GUI 无关**。
7. **[环境，过程]** 验收期间存在并行干扰（按窗口标题定位的探针会把查询注入到别人的实例；为 release 链接批量 `Stop-Process fer-gui`）。已在 Lead 侧处置（探针加 `-Pid`、只按具体 pid 杀）。本报告全部结论均来自**按 pid 锁定**的原子探针，或已复现两次以上。

## 12. INTERFACES「完全可用」验收标准逐条对照

| # | 标准 | 结论 | 证据 |
|---|---|---|---|
| 1 | build/test/clippy 全绿 | PASS | §1 |
| 2 | 启动即出窗口；COMCTL32 为 WinSxS 6.0.* | PASS | §2 |
| 3 | 输入查询 → 与 `fer search --json` 的 total/首条一致 | PASS | §3（4 组查询，逐字比对） |
| 4 | 列头三态排序与服务端 `--sort` 一致 | PASS | §4 |
| 5 | 双击行 → 资源管理器选中该文件；状态栏显示命中总数与索引规模 | PASS（定位）/ PASS（状态栏） | §12.1 |
| 6 | serve 停掉 → 状态栏明示未连接、不崩不卡（超时生效） | PASS | §7 |
| 7 | 关闭/重开 → 上次查询与窗口尺寸恢复 | **FAIL** | 落盘/恢复 PASS（§9），但「关闭」本身不可用（§10） |

### 12.1 双击定位的逐字证据

向 ListView 第 0 行投递双击消息（`WM_LBUTTONDOWN/UP + WM_LBUTTONDBLCLK/UP`，行矩形由 `LVM_GETITEMRECT` 取得），然后用 Shell COM 读资源管理器真实选中项：

```
TARGET row0 path = D:\Kita-Tools\Coding\fer-gui\Cargo.toml
NEWWINDOW loc=file:///D:/Kita-Tools/Coding/fer-gui  selected=[D:\Kita-Tools\Coding\fer-gui\Cargo.toml]
```

即：资源管理器打开到该文件所在目录，且**选中项正是该文件**。（附带观察：目标位于 `D:\$RECYCLE.BIN` 内时，Shell 只打开到 `$RECYCLE.BIN\<SID>` 而不选中 —— 属 Windows 对回收站路径的 Shell 行为，非 GUI 缺陷；正常路径 100% 复现选中。）

## 13. 未覆盖 / [UNKNOWN] 清单

- **[UNKNOWN] 「启动服务」的成功分支**：未在真实死端口上让 GUI 拉起真正的 `fer serve` 并连接成功（会与本机常驻索引实例共存，风险不可控）。已验证失败分支与状态机文案（§7）。
- **[UNKNOWN] Ctrl+C 复制路径 / 右键菜单 4 项动作**：不在 task-6 的 9 项清单内，本轮未做端到端实机验证（`copy_to_clipboard` 只有单测覆盖「空串直接报错」分支；剪贴板写入会改动用户剪贴板，故未主动触发）。
- **[UNKNOWN] 高 DPI 缩放下的布局**：本机为 100% 缩放；未在 125%/150% 下复验列宽与窗口几何的换算（winsafe 侧已 `SetProcessDPIAware()`，但未实测）。
- **[UNKNOWN] 长时间稳定性**：仅 20 连查询 / 约 4 分钟观察，未做小时级 soak。
- **未验证 rebuild/rescan 相关按钮**：GUI 未暴露这两个入口（`FerClient::rebuild/rescan` 有实现但无 UI 调用点），故不在验收面内。

## 14. 复现命令速查

```powershell
cd D:\Kita-Tools\Coding\fer-gui
# 构建（独立 target dir）
& "D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe" run stable-x86_64-pc-windows-gnu cargo build --offline --target-dir _build\verify
# 起实例并记录 pid
$p = (Start-Process -FilePath (Resolve-Path "_build\verify\debug\fer-gui.exe") -PassThru).Id
# 注入 + 读回（原子）
pwsh -File _build\verify\sort_probe.ps1 -TargetPid $p -Query "ext:rs size:>1mb" -ClickSeq "2,2,2" -Rows 3 -WaitMs 3000
# 三方对照
pwsh -File _build\verify\compare.ps1  -TargetPid $p -Query "main*.rs"
# 断链 + 自愈（另起实例）
$env:FER_GUI_ADDR='127.0.0.1:28877'; Start-Process _build\verify\debug\fer-gui.exe
pwsh -File _build\verify\stub_serve.ps1 -Port 28877        # 另一个窗口
# 关不掉（FAIL-1 最小复现）：向主窗口 PostMessage 0x0112/0xF060 或 0x0010，进程仍在
```

---

# 增量复验（R2）—— task-7

- 复验对象：HEAD = `f3011a1`（fix: WM_CLOSE 显式 DestroyWindow + 右键行负载反查），其上一条 `6591053` 引入了 du 动态列等改动。
- 本轮所有实机证据均由**当前 HEAD 重新构建**的 `_build/verify/debug/fer-gui.exe` 产生。
- **重要说明（R1 证据的时效性）**：R1 的实机测试用的是 09:31 构建的 debug 产物，而 `6591053`（09:39:46，含 du 动态列/状态栏构成变更/README/install.ps1）在其后落地 —— 也就是说 R1 的运行时证据对应的是 `6591053` 之前的 main.rs。本轮 R2 除复验修复项外，还**重新抽查了核心链路**，以覆盖 `6591053` 可能带来的回归。
- 写作用域不变：仅 `docs/verify-report.md` 与 `_build/verify/`（`git status` 仍只有 `?? docs/verify-report.md`）。

## R2.1 FAIL-1 修复复验 —— PASS [MEASURED]

脚本：`_build/verify/close_probe.ps1`（起实例 → SetWindowPos(260,170,1050,680) → 注入查询 main*.rs → 发关闭消息 → 50ms 粒度轮询退出 → 校验 IsWindow/退出码/config）。两条路径各起一个新实例，且每次都先删除 config.json。

| 关闭路径 | 退出 | 耗时 | IsWindow | 退出码 | config.json |
|---|---|---|---|---|---|
| A：`WM_CLOSE (0x0010)` | 是 | **66 ms** | False | **0** | 已落盘 |
| B：`WM_SYSCOMMAND / SC_CLOSE (0xF060)`（≡ 点 X / Alt+F4） | 是 | **64 ms** | False | **0** | 已落盘 |

两条路径落盘的 config 内容逐字一致：

```json
{"limit":2000,"last_query":"main*.rs","sort_key":"name","sort_desc":false,"win_x":260,"win_y":170,"win_w":1050,"win_h":680}
```

（上表为格式化后的实测文件，字段与取值逐字一致。）

补充检查：
- 关闭时刻前后 1 分钟内，`Application` 事件日志中 **无任何含 fer-gui 的记录**（无崩溃、无 WER、无 MessageBox 相关事件）→ 不是「被错误路径杀掉」，而是 `run_main` 正常返回后进程自行退出（退出码 0）。
- 关闭后 `Get-Process fer-gui` 无残留（我的两个测试实例都不需要 Stop-Process）。
- 根因注释与实现一致：`src/main.rs:295-306` 在 `save_config()` 后调用 `app.wnd.hwnd().DestroyWindow()`；销毁后由 winsafe 的 `WM_NCDESTROY` 默认处理 `PostQuitMessage`，`run_main` 得以返回。

## R2.2 右键行号修复（row_at）—— 静态复核 PASS / 菜单点击实测 [UNKNOWN]

静态复核（`src/main.rs` HEAD）：

```rust
fn row_at(&self, view_index: i32) -> Option<usize> {
    if view_index < 0 { return None; }
    let item = self.list.items().iter().nth(view_index as usize)?;
    let data = item.data();
    Some(*data.borrow())          // 行负载 = 结果序号（render/render_du 写入）
}
fn on_context_menu(&self, view_index: i32, pt: w::POINT) {
    let Some(row) = self.row_at(view_index) else { return };
    self.state.borrow_mut().context_row = Some(row);   // 存的是结果序号，不再是视图行号
    ...
}
```

- `render` 与 `render_du` 都按 `add(..., lParam)` 写入**结果序号**（`render` 用成功计数 `shown`，`render_du` 用 `hits.len()`），`row_at` 读回的正是该序号，`menu_action` 再用它索引 `state.hits` —— 语义闭合，修复方向正确。
- `row_at` 对 `view_index < 0`、行不存在（`nth` 越界）都返回 `None` 并提前 return，不 panic；比原来「`p.iItem` 直接当 hits 下标」严格更安全。
- 结论：**静态复核 PASS**。（残余理论风险：若 `render` 中途 `add` 失败，ListView 行与 `hits` 的下标关系由 lParam 承载，仍一一对应，不再错位。）

**实测尝试（未成功，记为 [UNKNOWN]）**：`_build/verify/ctxmenu_probe.ps1` / `ctxmenu_probe2.ps1` / `ctxmenu_diag.ps1` 向 ListView 投递 `WM_MOUSEMOVE + WM_RBUTTONDOWN + WM_RBUTTONUP`（坐标为 `LVM_GETITEMRECT` 算出的行中心），随后：
- 枚举该进程全部顶层窗口（含 `#32768` 菜单类）：**右键前后都没有菜单窗口出现**，说明 `NM_RCLICK` 未到达应用（或 `TrackPopupMenu` 未建菜单）；
- 随后 `PostMessage(main, WM_COMMAND, 103, 0)`（复制完整路径）→ 剪贴板仍是预先放置的哨兵 `R2-SENTINEL-NOT-COPIED`，状态栏无「已执行」文案，与「`context_row` 未被设置」一致。
- 判断：合成 post 的鼠标消息无法让 comctl32 ListView 产生 `NM_RCLICK`（更可能与真实光标位置/输入队列有关）；要实测菜单项只能注入**真实**硬件输入（移动用户物理光标并点击），本轮判断其侵入性不值得，**保持 [UNKNOWN]**，未升级为 MEASURED。

## R2.3 du 列语义修复 —— PASS [MEASURED]

同一次运行内先搜后 du，逐字读回表头（HDM_GETITEMCOUNT / HDM_GETITEMW）与单元格：

| 模式 | 表头（逐字） | 首行单元格 |
|---|---|---|
| 搜索 `ext:rs size:>1mb` | `名称 | 路径 | 大小 | 修改时间 | 类型` | `avx512f.rs | D:\...\avx512f.rs | 2.73 MB | 2025-11-11 20:00 | 代码` |
| du `du D:\Kita-Tools\Coding` | `名称 | 路径 | 大小 | 文件数 | 占用` | `fer | D:\Kita-Tools\Coding\fer | 4.55 GB | 11,630 | 4.57 GB` |

数值与 `/api/du` 逐条对照（前 3 行）：

| 行 | GUI 大小 | API size | GUI 文件数 | API files | GUI 占用 | API allocated |
|---|---|---|---|---|---|---|
| fer | 4.55 GB | 4,887,683,230 | 11,630 | 11,630 | 4.57 GB | 4,903,153,664 |
| kita-console | 2.72 GB | 2,922,812,726 | 15,322 | 15,322 | 2.74 GB | 2,942,558,208 |
| fer-gui | 1.40 GB | 1,497,998,945 | 4,340 | 4,340 | 1.40 GB | 1,505,869,824 |

（换算核对：4,887,683,230/1024³ = 4.552 → 4.55 GB；4,903,153,664/1024³ = 4.567 → 4.57 GB；其余同理。第 5 列由「恒为 0 的子目录数」改为「分配占用」，与 API 的 `allocated` 一致。）

截图（窗口 1300×800，5 列全部可见）：`_build/verify/r2-shot-du.png`、`_build/verify/r2-shot-search.png`。

## R2.4 回归抽查 —— PASS [MEASURED]

| 项 | 结果 | 证据 |
|---|---|---|
| `cargo test --offline` | `R2_TEST_EXIT=0`；103 单测 + 26 集成，0 failed，3 ignored | `_build/verify/r2-test.log` |
| `cargo clippy --all-targets -- -D warnings` | `R2_CLIPPY_EXIT=0`，`^warning` 计数 **0** | `_build/verify/r2-clippy.log` |
| `cargo build --release` | `R2_RELEASE_EXIT=0`，**505,856 字节**（R1 为 505,344，增量即本次修复） | `_build/verify/r2-build-release.log` |
| debug build | `EXIT=0` | `_build/verify/r2-build-debug.log` |
| 四组查询三方对照 | `ext:rs size:>1mb` 56/56/56、`main*.rs` 147/147/147、`type:dir` 528,250/528,250、`报告` 84/84/84；GUI 首行 path 与 API `hits[0].path` 逐字一致 | 本轮 `compare.ps1` 输出 |
| 三态排序 | name↑ → size↑ → size↓ → name↑，每态首行与对应 `sort=` 首条逐字一致 | 本轮 `sort_probe.ps1 -ClickSeq 2,2,2` 输出 |
| 双击定位（复核） | 未重跑（代码未改动该路径，R1 已 MEASURED） | 见 §12.1 |

注：`type:dir` 的 total 由 R1 的 528,112 变为 528,250，是 fer 实时 monitor 在两次运行间新增条目所致 —— GUI 与 API/CLI 三方始终一致，非回归。

## R2.5 本轮新发现（非阻塞）

1. **[静态] `render_du` 的文档注释已过时**：`src/main.rs:494` 仍写「复用同一张表：名称/路径/大小/文件数/子目录数」，实际第 5 列已是 `human_size(allocated)` 且表头为「占用」。建议同步注释。
2. **[实测/构成变更] 左状态栏不再附带索引行**：`render` 的文案现为 `命中 N 条（显示 M）· 引擎 K ms`，索引规模只出现在右侧标签（`索引 4,276,741 条 · 1,482 MB · 实时 Δ… `）。功能无影响，但 R1 报告里「状态栏含索引规模」的表述在 R2 下应理解为「右侧标签承担」。
3. **[实测] 窄窗口下第 5 列会被挤出可视区**：列宽合计 1120 px，窗口宽 1050 时出现水平滚动条、表头第 5 列不可见（`r2-shot-du.png` 第一版即如此）。属既定的固定列宽取舍，非缺陷；仅提示验收截图需 ≥1120 宽窗口。

## R2 判定

**PASS —— 阻塞缺陷已消除，未发现回归。**

- FAIL-1（窗口关不掉）→ 两条关闭路径均 4 s 内退出、`IsWindow=False`、退出码 0、config 正确落盘、无崩溃事件 → **已修复**。
- du 列语义 → 表头与数值均与 `/api/du` 一致 → **已修复**。
- 右键行号 → 静态复核 PASS；菜单点击实测 **[UNKNOWN]**（原因见 R2.2）。
- 构建门禁与核心链路抽查 → 全部维持 PASS。

**当前「完全可用」判定：PASS**（未覆盖项继承 R1 §13，并新增「右键菜单真实点击」一项 [UNKNOWN]）。

---

# R3：Everything 式交互改造复验（task-11，commit a4f4078）

复验对象：`a4f4078`（菜单栏 + 工具栏、搜索框去 du 子命令、占用视图改由命令进入并支持面包屑/返回上级、搜索历史 Ctrl+H）。所有实机证据由当前 HEAD 重新构建的 `_build/verify/debug/fer-gui.exe` 产生；构建门禁见 R3.6。

**判定：PASS（用户点名的核心问题已解决，6 项必验全通过）**；同时报 **1 个非阻塞缺陷 R3-D1**（占用视图退出时若查询框为空，标题/列头不复位），建议修但不阻塞交付。

R2 遗留项状态：`render_du` 的过时注释已在本轮前被修正为「名称/路径/大小/文件数/占用 —— 第 5 列是分配簇字节」（`src/main.rs:796-797`）。

## R3.1 核心：搜索框不再有 du 子命令 —— PASS [MEASURED]

注入方式：`WM_SETTEXT` 写入搜索框（触发 EN_CHANGE → 150ms 防抖 → 自动搜索），逐字读回标题/列头/状态栏/单元格。

| 断言项 | 实测 | 结论 |
|---|---|---|
| 窗口标题 | `fer GUI - File Express Retriever`（**无** ` - 占用分析` 后缀） | PASS |
| 列头 | `名称|路径|大小|修改时间|类型`（搜索列，非 du 列） | PASS |
| 结果 | `命中 969 条（显示 969）· 引擎 310 ms`，普通文件结果 | PASS |
| 与 API 对照 | `/api/search?q=du%20D%3A%5CKita-Tools%5CCoding` → `ok=True total=969 count=969`，首条 `D:\Kita-Tools\Coding\crossdesk\.gitmodules` 与 GUI 首行逐字一致 | PASS |
| 查询框内容 | 原样保留 `du D:\Kita-Tools\Coding`（不被改写） | PASS |

源码佐证：`src/main.rs:539` 的 `start_search` 已无任何 du 前缀解析，只有空查询短路 + 历史入栈 + 普通 search 请求。

## R3.2 菜单命令分派 —— PASS [MEASURED]（2009/2010/2011 除外）

注入方式：`PostMessage(hwnd, WM_COMMAND=0x0111, MAKEWPARAM(id,0), 0)`（winsafe 的 `wm_command_acc_menu` 同时挂 Menu/Accel 通知码）。

| 命令 | 效果（实测） | 结论 |
|---|---|---|
| 2006 占用分析 | 标题变 `… - 占用分析`；面包屑 `占用 D: › Kita-Tools › Coding › crossdesk ［D:\Kita-Tools\Coding\crossdesk］ —— 合计 400.09 MB（分配 402.04 MB）· 1,208 文件 · 869 目录 · 已截断`；列头变 `名称|路径|大小|文件数|占用`；查询框不动 | PASS |
| 2007 返回上级 | 面包屑上移到 `… › Coding`，合计 16.28 GB、57 行 | PASS |
| 2008 搜索模式 | 标题复位为 `fer GUI - File Express Retriever`；列头回搜索列；自动重发搜索 `命中 969 条` | PASS |
| 2013 清空结果 | `LISTROWS=0`、状态栏 `就绪` | PASS |
| 2014 立即搜索 | 重新出结果 `命中 969 条`，首行不变 | PASS |
| 2015 关于 | 出现顶层对话框 `#32770\|\|关于 fer GUI\|\|visible=True`（截图 `_build/verify/r3-about.png`：含搜索语法、快捷键、配置文件/历史文件路径）；`WM_CLOSE` 可关闭，应用存活 | PASS |
| 2016 退出 | 进程退出（`进程存活=False`），`config.json` 落盘（几何 412,157,1300,800 + last_query） | PASS |
| 2009 重建索引 / 2010 全量重扫 / 2011 启动 fer serve | **未实触发**（按任务要求：会改索引或起进程）。仅有源码级核对：三者分别 → `rebuild_index`（先 YESNO 二次确认再 POST /api/rebuild）、`rescan_index`（先确认再 POST /api/rescan）、`start_serve`（`serve_ctl::ensure_serve`，8s 上限）；菜单项标签 `重建索引(&B)` / `全量重扫(&R)` / `启动 fer serve(&V)` 由 `menu.rs` 单测覆盖 | **[UNKNOWN]**（实触发未验） |

补充：`menu.rs` 的 16 个命令 id 为 2001..=2016 连续块，与右键菜单（复用同一批 id）和历史菜单（1000..1999）不相交 —— 该不相交性由 `menu.rs` 单测 `command_ids_live_in_a_block_disjoint_from_context_menus` 保证。

## R3.3 工具栏按钮 —— PASS [MEASURED]

注入方式：对按控件文本定位到的 `Button` 发 `BM_CLICK(0x00F5)`（等价真实点击）。

- `占用分析`：进入占用视图（标题带后缀、du 列头、面包屑 `D: › Kita-Tools › Coding › crossdesk`），截图 `_build/verify/r3-du-view.png`。
- `返回上级`：逐级上移（见 R3.4）。
- `重建索引`：会弹 `YESNO` 确认框（源码 `rebuild_index` 已确认该分支），**未点确定**，故只验到「弹框」这一步；取消分支未实跑。

## R3.4 面包屑与到盘根自动退出 —— PASS [MEASURED]

从 `D:\Kita-Tools\Coding\crossdesk` 连续点 `返回上级`：

| 次数 | 面包屑 | 列头 | 标题 |
|---|---|---|---|
| 1 | `D: › Kita-Tools › Coding` | du 列 | 带后缀 |
| 2 | `D: › Kita-Tools`（407.57 GB / 2,171,385 文件） | du 列 | 带后缀 |
| 3 | `D:`（603.31 GB / 2,685,449 文件） | du 列 | 带后缀 |
| 4 | —— | **回搜索列** `名称|路径|大小|修改时间|类型` | **复位** `fer GUI - File Express Retriever` |

第 4 次即盘根（`parent_of("D:") = None`）→ 自动退出占用视图并重发搜索（`命中 969 条`）。与任务描述一致。

## R3.5 搜索历史 —— PASS [MEASURED]

1. **落盘**：连续做 `du D:\Kita-Tools\Coding` / `ext:rs size:>1mb` / `main*.rs` 多次查询后走 2016 退出，`%LOCALAPPDATA%\fer-gui\history.txt` 实测：

```
[1] main*.rs
[2] ext:rs size:>1mb
[3] du D:\Kita-Tools\Coding
[4] search deepseek
```

→ 最新在前；`du D:\Kita-Tools\Coding` 被搜过多次但**只出现一次**（去重生效）；启动时从 config 恢复的 `search deepseek` 也入栈。

2. **加载 + Ctrl+H**：重启后（未再落盘）按真实按键 `Ctrl+H`，进程顶层窗口出现 `#32768` 菜单窗口，截图为 `_build/verify/r3-history.png`，菜单内容与 history.txt **逐条一致**（`main*.rs` / `ext:rs size:>1mb` / `du D:\Kita-Tools\Coding` / `search deepseek` / 分隔线 / `清除历史`）。

3. **选中回填并搜索**：菜单打开后按 `↓↓Enter` 选第 2 项 → 查询框由 `main*.rs` 变为 `ext:rs size:>1mb`，结果由 148 条变为 **56 条**（= `/api/search?q=ext%3Ars%20size%3A%3E1mb` 的 total 56）。

（`Ctrl+H` 的处理挂在 ListView 的 `lvn_key_down`（`src/main.rs:412`），要求列表有焦点；本次用合成左键点列表 + 真实 Ctrl+H 按键完成。焦点检查 API 返回 0，但菜单确实弹出且内容/选中均正确，判定以「#32768 出现 + 内容正确 + 选中生效」为准。）

## R3.6 回归 —— PASS [MEASURED]

| 项 | 结果 | 证据 |
|---|---|---|
| `cargo test --offline` | `R3_TEST_EXIT=0`；**128 passed**（较 R2 的 103 增加 menu/history 单测）+ 26 集成，0 failed，4 ignored | `_build/verify/r3-test.log` |
| `cargo clippy --all-targets -- -D warnings` | `R3_CLIPPY_EXIT=0`，`^warning` 计数 **0** | `_build/verify/r3-clippy.log` |
| `cargo build --release` | `R3_RELEASE_EXIT=0`，**534,016 字节**（R2 为 505,856） | `_build/verify/r3-build-release.log` |
| 三方对照 | `main*.rs` 148/148/148、`报告` 84/84/84，GUI 首行 path 与 API 首条逐字一致 | `compare.ps1` 输出 |
| 关闭行为 | `WM_CLOSE` 仍 70 ms 内退出（`IsWindow=False`、退出码 0、config 落盘）→ **R2.1 修复未被破坏** | `close_probe.ps1` 输出 |
| 视觉 | COMCTL32 仍为 WinSxS `6.0.19041.6456`；工具栏/菜单渲染见 `r3-du-view.png`、`r3-about.png` | 截图 |

## R3-D1（新发现，非阻塞）占用视图退出时若查询框为空，标题与列头不复位

**最小复现**（实测）：
1. 搜一个非空查询（例 `du D:\Kita-Tools\Coding`，969 条）。
2. 菜单 视图 → 占用分析（当前目录）或工具栏「占用分析」→ 进入占用视图（标题带后缀、du 列头）。
3. 把搜索框内容清空（空查询 → `clear_results`，此时仍处于占用视图）。
4. 菜单 视图 → 搜索模式（或按 Esc）→ 退出占用视图。
5. **观察**：标题仍是 `fer GUI - File Express Retriever - 占用分析`，列头仍是 `名称|路径|大小|文件数|占用`，列表为空、状态栏 `就绪`。

实测输出：退出后 `TITLE=fer GUI - File Express Retriever - 占用分析`、`HEADERS=名称|路径|大小|文件数|占用 LISTROWS=0`。

**根因**：`exit_du()`（`src/main.rs:765`）把 mode 置回 Search 后只调 `trigger_search_now()`；查询框为空时 `start_search("")` 走 `clear_results()`，**不会**经过 `render()`，而复位标题（`main.rs:950`）与搜索列头（`main.rs:951`）这两件事只写在 `render()` 里。同理，「返回上级」到盘根的自动退出（`du_up → exit_du`）在查询框为空时也会留下同样的残留状态。

**影响**：状态机本身正确（mode 已回到 Search，下一次非空搜索会经 `render()` 自愈），但界面会短暂「说谎」——标题与列头仍停留在占用视图。严重度：低-中（非阻塞）。

**修复建议**：把「复位标题 + 搜索列头」从 `render()` 抽成 `reset_view_chrome()`，在 `exit_du()` 与 `clear_results()` 里一并调用（或让 `exit_du` 显式复位）。

## R3 未覆盖 / [UNKNOWN]

- **[UNKNOWN]** 2009 重建索引 / 2010 全量重扫 / 2011 启动 fer serve 的**实触发**（按任务要求规避：会改索引或起进程）；仅源码与菜单标签核对。
- **[UNKNOWN]** 「重建索引 / 全量重扫」确认框的**取消/确定分支**未实跑（只验到弹框路径存在）。
- **[UNKNOWN]** 菜单栏的**真实鼠标点击**与助记符（Alt+F 等）未触发 —— 命令分派改用 `WM_COMMAND` 注入验证（与真实点击同一条 `wm_command_acc_menu` 通路），菜单标签由 `menu.rs` 单测覆盖。
- 继承 R1 §13 的其余未覆盖项（右键菜单真实点击、非 100% DPI、长时间 soak）。

---

# R4：R3-D1 修复复验（task-12，commit e20f1fe）

复验对象：`e20f1fe`「退出占用视图时无条件复位标题与搜索列头」。改动仅 4 行（`src/main.rs` 的 `exit_du()`）：

```rust
self.state.borrow_mut().mode = Mode::Search;
let _ = self.wnd.hwnd().SetWindowText(TITLE_BASE);
self.set_column_titles(["名称", "路径", "大小", "修改时间", "类型"]);
self.trigger_search_now();
```

静态复核：修复点正是 R3-D1 的根因位置（复位动作原先只写在 `render()` 里）；`du_up` 到盘根的自动退出走 `exit_du()`，同一条路径被覆盖。
所有实机证据来自**全新 target dir** 的构建（`cargo build --offline --target-dir _build\verify-r4`，`R4_BUILD_EXIT=0`），确保产物来自当前源码。

## R4.1 复现路径 A（清空查询框 + 菜单「搜索模式」）—— 已修复 [MEASURED]

| 步骤 | TITLE | HEADERS |
|---|---|---|
| 搜 `du D:\Kita-Tools\Coding`（819 条） | 无后缀 | 搜索列 |
| 2006 进占用视图 | `… - 占用分析` | `名称\|路径\|大小\|文件数\|占用` |
| 清空搜索框（空查询 → clear_results，仍在占用视图） | `… - 占用分析` | du 列，LISTROWS=0 |
| **2008 搜索模式** | **`fer GUI - File Express Retriever`（无后缀）** | **`名称\|路径\|大小\|修改时间\|类型`** |

状态栏同时回到 `就绪`。断言 `TITLE 含「 - 占用分析」= False`。

## R4.2 复现路径 B（清空查询框 + 「返回上级」连点到盘根）—— 已修复 [MEASURED]

从 `D:\Kita-Tools\Reversing\ln-c-image-forensics\data\doc_text` 起（先清空查询框），连点工具栏「返回上级」：

| 次数 | 面包屑 | 标题/列头 |
|---|---|---|
| 1 | `… Reversing › ln-c-image-forensics › data` | 带后缀 / du 列 |
| 2 | `… Reversing › ln-c-image-forensics` | 带后缀 / du 列 |
| 3 | `… Reversing` | 带后缀 / du 列 |
| 4 | `D: › Kita-Tools` | 带后缀 / du 列 |
| 5 | `D:` | 带后缀 / du 列 |
| 6 | ——（盘根 `parent_of("D:") = None` → 自动 `exit_du`） | **无后缀 / 搜索列** |

最终 `TITLE=fer GUI - File Express Retriever`、`HEADERS=名称\|路径\|大小\|修改时间\|类型`、`STATIC=就绪`。

## R4.3 回归 —— PASS [MEASURED]

| 项 | 结果 | 证据 |
|---|---|---|
| `cargo test --offline` | `R4_TEST_EXIT=0`；128 passed + 26 集成，0 failed，4 ignored | `_build/verify/r4-test.log` |
| `cargo clippy --all-targets -- -D warnings` | `R4_CLIPPY_EXIT=0`（0 警告） | `_build/verify/r4-clippy.log` |
| `WM_CLOSE` 退出 | 70 ms 内 `EXITED=True`、`IsWindow=False`、退出码 0、config 落盘（220,140,1150,720 + `报告`） | `close_probe.ps1` 输出 |
| 全新 target dir 构建 | `R4_BUILD_EXIT=0` | `_build/verify/r4-clean-build.log` |

## R4-D1（本轮新发现，非阻塞，**与本次修复无关**）Esc 无法退出占用视图

task-12 第 1 项把「Esc」列为「搜索模式」菜单的替代退出方式，实测 **Esc 无效**：

**最小复现**：
1. 搜非空查询 → 菜单 视图→占用分析（当前目录）进入占用视图（标题带后缀、du 列头）。
2. 把焦点给结果表（点一行），按真实 `Esc`（`keybd_event` 注入，非 PostMessage 合成键）。
3. **观察**：标题仍 `… - 占用分析`、列头仍 du 列；连按两次仍无变化。

**同一状态下的对照实验（关键证据）**：紧接着按真实 `Backspace` → 面包屑立刻由 `…\data\doc_text` 上移到 `…\data`（LISTROWS 0→15），说明该窗口/焦点下键盘事件确实到达了 ListView，且 `lvn_key_down` 分支链在跑（`VK_BACK` 分支生效）。

源码对照：`src/main.rs:418-421` 的同一 if 链里 `VK::BACK → du_up()`、`VK::ESCAPE → exit_du()`；`co::VK::ESCAPE` 在 winsafe 里确为 `0x1b`（`winsafe-0.0.29/src/user/co/consts.rs:2928`），常量值无误。

**结论分层**：
- [MEASURED] 真实 Esc 按键在占用视图下不产生任何效果；同一时刻真实 Backspace 生效。
- [假设] 最可能的原因是 comctl32 的 ListView 对 `VK_ESCAPE` 不发送（或不在此时机发送）`LVN_KEYDOWN` 通知，因此挂在 `lvn_key_down` 上的 Esc 分支永远不会命中；由于无法在不插桩的前提下直接观测 comctl32 内部，此条标为假设而非定论。
- [影响] 「关于」对话框把「Esc 退出占用视图」写成了快捷键说明（`show_about` 文案），与实际行为不符；退出占用视图仍有 3 条可用路径（视图菜单「搜索模式」、逐级「返回上级」到盘根、双击/回车下钻不是退出）。严重度：低（文档与实现不一致 + 少一条快捷方式）。
- [建议] ① 把 Esc 处理从 `lvn_key_down` 挪到窗口级（`WindowMain::on().wm_key_down` 或注册 Esc 为加速键），使其不依赖结果表焦点；或 ② 若确认 comctl32 行为不可绕，则把「关于」里的 Esc 说明删掉，只保留 Backspace。

## R4 判定

**PASS —— R3-D1 已修复**（两条复现路径 A/B 均逐字断言通过），回归全绿，未见新回归。

未覆盖项：实现 R4-D1 建议方案后的复验、以及继承 R1/R3 的 [UNKNOWN] 清单（2009/2010/2011 实触发、右键菜单真实点击、非 100% DPI、长时间 soak）。

---

# R5：R4-D1 修复复验 —— 窗口加速键表（task-13，commit 0e57f16）

复验对象：`0e57f16`「用加速键表处理 Esc/Backspace/Ctrl+H/Ctrl+C」。实现：`WindowMainOpts.accel_table` 注册 4 条 `ACCEL`（Esc→CMD_MODE_SEARCH、Backspace→CMD_DU_UP、Ctrl+H→新增的 CMD_HISTORY_OPEN=1998、Ctrl+C→CMD_COPY_PATH），命令经 `wm_command_acc_menu` 回到 `menu_command`；原 `lvn_key_down` 分支保留。

winsafe 侧机制核对（`winsafe-0.0.29/src/gui/windows/base_wnd.rs:158-204`）：消息循环先 `hwnd_top_level.TranslateAccelerator(haccel, &mut msg)`，`is_ok()`（= 已翻译）则 `continue`，否则继续 `IsDialogMessage` / `TranslateMessage` + `DispatchMessage`。**TranslateAccelerator 只看消息与加速键表，不看焦点在哪个子控件** —— 这正是 R4-D1 要的属性。

证据产物：`_build/verify-r5/debug/fer-gui.exe`（全新 target dir 构建，`R5_BUILD_EXIT=0`）。

## R5.1 Esc 退出占用视图 —— 已修复 [MEASURED]

| 步骤 | TITLE | HEADERS |
|---|---|---|
| 搜 `报告`（84 条）→ 2006 进占用视图 | `… - 占用分析` | `名称\|路径\|大小\|文件数\|占用` |
| 向**主窗口**投递 `WM_KEYDOWN/WM_KEYUP VK_ESCAPE` | **`fer GUI - File Express Retriever`** | **`名称\|路径\|大小\|修改时间\|类型`**（LISTROWS=84，自动重发搜索） |

## R5.2 Backspace 返回上一级 —— PASS [MEASURED]

占用视图停在 `…\data\doc_text`（54.98 MB / 1,820 文件）时投递 `VK_BACK` → 面包屑变为 `… › ln-c-image-forensics › data`（1.31 GB / 2,709 文件，LISTROWS=15）。

## R5.3 Ctrl+H 不再依赖结果表焦点 —— PASS [MEASURED]

把焦点显式放在**搜索框**（不是结果表；`GetGUIThreadInfo` 实测 `focus` = Edit 句柄），按真实 `Ctrl+H` → 进程顶层窗口出现 `#32768` 菜单窗口（`SysShadow` 一并出现），历史菜单弹出。对比 R3：当时 Ctrl+H 只在结果表有焦点时生效。

## R5.4 Ctrl+C 复制聚焦行路径 —— PASS [MEASURED]

结果表点选一行后按真实 `Ctrl+C` → 剪贴板 = `D:\Kita-Tools\DevEnv\sdk\ndk\magisk\toolchains\rust\lib\rustlib\src\rust\library\stdarch\crates\core_arch\src\x86\avx512f.rs`，与 `/api/search` 的 `hits[0].path` 逐字一致；状态栏同时显示 `已复制：<同一路径>`。（测试前剪贴板置哨兵，测后已被覆盖为应用写入的值。）

## R5.5 搜索视图下 Esc 无害 —— PASS [MEASURED]

搜索视图 `命中 56 条` 时投递/按真实 Esc → 结果与状态栏不变（仍 `命中 56 条（显示 56）`、标题无后缀、列头不变），进程存活。

## R5.6 回归 —— PASS [MEASURED]

| 项 | 结果 | 证据 |
|---|---|---|
| 干净 target dir 构建 | `R5_BUILD_EXIT=0` | `_build/verify/r5-clean-build.log` |
| `cargo test --offline` | `R5_TEST_EXIT=0`；128 passed + 26 集成，0 failed，4 ignored | `_build/verify/r5-test.log` |
| `cargo clippy --all-targets -- -D warnings` | `R5_CLIPPY_EXIT=0`（0 警告） | `_build/verify/r5-clippy.log` |
| 三方对照 | `main*.rs` 146/146/146，GUI 首行 path 与 API 首条逐字一致 | `compare.ps1` 输出 |
| `WM_CLOSE` | 69 ms 内退出、`IsWindow=False`、退出码 0、config 落盘（240,160,1180,740 + `报告`） | `close_probe.ps1` 输出 |

（`main*.rs` 的 total 由 R3 的 148 变为 146，是 fer monitor 期间的索引变化；三方始终一致。）

## R5.7 复验方法与一处 [UNKNOWN]（重要，避免误报）

本环境的**合成键盘输入**存在一段明确的失效区间，必须说明，否则容易把注入问题误判成应用缺陷：

| 注入方式 | 结果 | 说明 |
|---|---|---|
| 真实鼠标点击（`SetCursorPos` + `mouse_event`） | **有效** | 能把焦点切到搜索框（`GetGUIThreadInfo` 的 `focus` 从别的控件变为 Edit 句柄） |
| 真实修饰键组合（`keybd_event` 带扫描码）Ctrl+H / Ctrl+C | **有效** | 走 `TranslateAccelerator` 命中加速键 |
| `PostMessage(main, WM_KEYDOWN)` 的 Esc / Backspace | **有效** | 同上（加速键匹配不看目标窗口） |
| `PostMessage(edit, WM_CHAR)` | **有效** | 文本被插入（`abcdef` → `xyzabcdef`），证明 Edit 可编辑、未被置只读（style=0x50030080，无 ES_READONLY） |
| 真实**普通字符**按键（`keybd_event` VK_X，扫描码由 `MapVirtualKey` 填，前台/焦点均已用 API 校验为 True/命中 Edit） | **无效** | 期望插入 `x`，实测文本不变（多轮复现） |

A/B 排除回归：用**加速键之前**的产物 `_build/verify-r4/debug/fer-gui.exe` 做同样实验，`abcdef` + 真实 `x` + 真实 Backspace 后文本仍为 `abcdef` —— **与带加速键表的 r5 产物行为完全一致**，故这不是本次改动引入的问题；加速键表本身也已被上表前三种方式验证有效。

结论：本环境无法用合成输入验证「**真人在搜索框打字**」这一路径，故该项记 **[UNKNOWN]**（未纳入判定）。补充依据：用户此前的反馈（在搜索框里输入过 `du <目录>` 子命令）本身就说明真实键盘输入在他们那边是通的。

## R5 判定

**PASS —— R4-D1 已修复**：Esc 退出占用视图不再依赖结果表焦点（投递主窗口 Esc 即复位标题/列头/重发搜索）；Backspace 返回上级正常；Ctrl+H 在焦点位于搜索框时也能弹出历史菜单；Ctrl+C 复制路径正确；搜索视图下 Esc 无害；回归全绿。

未覆盖：真人键盘打字路径（R5.7 的 [UNKNOWN]）、以及继承 R1/R3 的其余 [UNKNOWN] 清单（2009/2010/2011 实触发、右键菜单真实点击、非 100% DPI、长时间 soak）。

---

# R6：加速键与输入框冲突修复复验（task-14，commit 0968aa1）

**先认账**：R5 时我把「Backspace / Ctrl+C 也放进窗口加速键表」当作改进予以通过，实际引入了用户可见的回归 —— `TranslateAccelerator` 在消息循环层面无条件拦截，Edit 再也收不到这两个键（输入框不能退格、框内不能复制文本）。R5 报告没有覆盖「真人在输入框打字/编辑」这条路径（当时记为 [UNKNOWN]），这正是漏网的缺口。本轮用 **kra 真实输入通道**补齐。

## R6.0 修复核对与验证方法

源码（`src/main.rs` HEAD）：加速键表现在只有两条 —— `ACCEL{VIRTKEY, ESCAPE, CMD_MODE_SEARCH}`（第 161 行）与 `ACCEL{VIRTKEY|CONTROL, H, CMD_HISTORY_OPEN}`（162-166 行）；Backspace / Ctrl+C 回到 ListView 的 `lvn_key_down` 分支。

方法：`kita-remote`（agent @ `127.0.0.1:25001`）真实输入 —— `window -action focus` 置前台 → `input click`（`dispatch=foreground:sendinput`，真实光标落点 `(774,227)`）→ `input combo ctrl a` / `input type` / `input key backspace` / `input combo ctrl h` → 用 `WM_GETTEXT` 逐字读回 → 剪贴板用 `kra clipboard` 核验。这同时把 R5.7 的 [UNKNOWN] 升级为 MEASURED。

## R6.1 输入框可退格 —— PASS [MEASURED]

| 步骤 | 查询框实测值 |
|---|---|
| 真实点击搜索框 → `Ctrl+A` → 真实键入 `ext:rs` | `ext:rs` |
| 真实 `Backspace` ×2 | **`ext:`**（与任务字面断言一致） |
| （另一轮）`ext:rs` → 真实 `Backspace` ×3 | `ext`（3 次退格正确删掉 `s`/`r`/`:` 三个字符；任务描述里「×3 → `ext:`」少算了一位） |

## R6.2 输入框内 Ctrl+C 是文本复制 —— PASS [MEASURED]

光标在搜索框、内容为 `ext` 时按真实 `Ctrl+A` + `Ctrl+C` → `kra clipboard` 回读 `length=3` / `ext`（**输入框文本**，不是任何文件路径）→ 与旧行为（复制聚焦行路径）相反，说明 Ctrl+C 已回到 Edit 的正常语义。

剪贴板卫生：测试前先备份用户剪贴板（原值 `length=6` / `ext:rs`，存 `_build/verify/clipboard-backup.txt`），测试后 `kra clipboard -set` 恢复并回读确认 `length=6` / `ext:rs`，备份文件已删除。

## R6.3 结果表焦点下 Backspace 仍返回上级 —— PASS [MEASURED]

进入占用视图（`…\Reversing\ln-c-image-forensics\data\doc_text`，54.98 MB / 1,820 文件）→ 真实点击结果表给焦点 → 真实 `Backspace` → 面包屑上移到 `… › data`（1.31 GB / 2,709 文件，LISTROWS=15）。

## R6.4 Esc 仍退出占用视图 —— PASS [MEASURED]

同一占用视图内按真实 `Esc` → `TITLE=fer GUI - File Express Retriever`（后缀消失）、列头回 `名称|路径|大小|修改时间|类型`、自动重发搜索 `命中 84 条`。

## R6.5 Ctrl+H 在搜索框焦点下仍全局可用 —— PASS [MEASURED]

焦点在搜索框（非结果表）时按真实 `Ctrl+H` → 进程顶层出现 `#32768` 菜单窗口（历史菜单）；按 `Esc` 可关闭。

## R6.6 回归 —— PASS [MEASURED]

| 项 | 结果 | 证据 |
|---|---|---|
| 干净 target dir 构建 | `R6_BUILD_EXIT=0` | `_build/verify/r6-clean-build.log` |
| `cargo test --offline` | `R6_TEST_EXIT=0`；128 passed + 26 集成，0 failed，4 ignored | `_build/verify/r6-test.log` |
| `cargo clippy --all-targets -- -D warnings` | `R6_CLIPPY_EXIT=0`（0 警告） | `_build/verify/r6-clippy.log` |
| 三方对照 | `main*.rs` 147/147/147，GUI 首行 path 与 API 首条逐字一致 | `compare.ps1` 输出 |
| `WM_CLOSE` | 68 ms 内退出、`IsWindow=False`、退出码 0、config 落盘（260,170,1160,730 + `报告`） | `close_probe.ps1` 输出 |

## R6 判定

**PASS —— 回归已修复。** 输入框退格、框内 Ctrl+C 文本复制、结果表 Backspace 返回上级、Esc 退出占用视图、Ctrl+H 全局可用，全部用 kra 真实输入复现通过；构建门禁与核心链路回归全绿。

教训（已由 Lead 写入 AGENTS.md，这里只做佐证）：**只把「在输入框里没有默认语义」的键放进窗口加速键表**；Backspace / Ctrl+C 这类 Edit 原生语义键一旦进表，`TranslateAccelerator` 会在消息循环层面无条件吞掉它们。R5 的 [UNKNOWN]（真人键盘路径）就是这次漏网的原因 —— 后续涉及键盘交互的改动，应默认用 kra 真实输入而不是合成注入。

未覆盖：继承 R1/R3 的其余 [UNKNOWN]（2009/2010/2011 实触发、右键菜单真实点击、非 100% DPI、长时间 soak）。





