# fer-gui 验收汇总（Lead 侧实测）

> 独立验证者的逐项证据在 [verify-report.md](verify-report.md)（R1 判定 FAIL-1 → 修复后 R2 判定 PASS）。
> 本文只记 Lead 侧的门禁与实机证据，时间 2026-09-30。

## 1. 交付物

| 项 | 值 |
|---|---|
| 产物 | `target-gnu\\release\\fer-gui.exe` |
| 体积 | **505,856 字节（494 KB）**（含 1 KB manifest 资源） |
| 运行时依赖 | 无（无 MSVC CRT、无 WebView2、无任何 DLL） |
| 代码规模 | `src/` 8 个模块（含约 6k 行自有代码）+ `tests/` 2 个集成测试文件 |

## 2. 门禁（GNU 链，`cargo --offline`）

| 命令 | 结果 |
|---|---|
| `cargo test --offline --no-fail-fast` | **103 passed / 0 failed / 3 ignored**（bin 单测）+ **26 passed**（tests/query_state.rs）；3 ignored = 真实 serve 联调，`-- --ignored` 时全 pass |
| `cargo clippy --all-targets --offline -- -D warnings` | exit 0，**0 警告** |
| `cargo build --release --offline` | exit 0 |

## 3. 实机证据（截图在 `_build/lead-verify/`，均为 1300x800 真实窗口截图）

| 场景 | 证据 | 观测 |
|---|---|---|
| 启动与视觉 | （Release 实例）COMCTL32 = `WinSxS\\...common-controls_..._6.0.19041.6456` | v6 现代样式，控件为圆角按钮/表头 |
| 搜索 | `shot2-search.png` | 查询 `AGENTS.md parent:D:\\Kita-Tools\\Coding\\fer-gui` → 命中 1 条、size 4.40 KB、类型「文本」；状态栏「命中 1 条（显示 1）· 引擎 0 ms」 |
| 服务端排序 | `shot5-size-desc.png` | 点「大小」列头两次 → 首行 686.70 KB 递减，命中 2,500 条（显示 2000）· 引擎 4 ms |
| du 占用模式 | `shot9-du-final.png` | `du D:\\Kita-Tools\\Coding` → fer 4.55 GB / 11,630 文件 / 占用 4.57 GB；列头自动切为 名称\|路径\|大小\|文件数\|占用；状态栏「合计 14.65 GB（分配 14.79 GB）· 94,197 文件 · 16,509 目录 · 已截断」 |
| serve 离线 | `shot10-offline.png` | 指向死端口 → 状态栏「请求失败：…os error 10061（fer serve 是否在运行？）」+「serve 未连接（127.0.0.1:19）」；进程不崩不卡 |
| 一键拉起 serve | `shot13-serve-up.png` | 点「启动服务」→ 真实拉起 `fer serve --no-warm --addr 127.0.0.1:25000`（pid 42044）、`/api/health` 返回 `{"ok":true}`、界面转为「索引 4,276,741 条 · 1,482 MB · 127.0.0.1:25000」且搜索出 56 条（= `ext:rs size:>1mb` 的三方一致值） |
| 窗口关闭（修复后） | 见 verify-report R2.1 | WM_CLOSE 与 SC_CLOSE 两路径均 66/64 ms 内退出、退出码 0、config.json 落盘 |
| 配置持久化 | `%LOCALAPPDATA%\\fer-gui\\config.json` | 窗口几何 + last_query + 排序态落盘；重启后 GetWindowRect 与查询框完全恢复 |

## 4. 三方一致性（GUI ↔ /api/search ↔ `fer --count-only`）

`scripts\\verify.ps1` 全绿（0 失败）：

```
PASS  对照 ext:rs        api=32409  cli=32409
PASS  对照 main*.rs      api=147    cli=147
PASS  对照 size:>1mb     api=49755  cli=49755
PASS  对照 type:dir      api=528132 cli=528132
PASS  COMCTL32.dll 为 v6
```

## 5. 未覆盖 / 已知边界

- **右键菜单点击**：合成鼠标消息无法让 comctl32 ListView 产生 NM_RCLICK（verifier 实测），
  需真实硬件输入才能端到端验证；其底层动作（`actions::*`）已由 features 的真机冒烟覆盖（explorer 选中、剪贴板往返）。
- **Ctrl+C**：逻辑与右键「复制完整路径」共用 `copy_to_clipboard`，未单独实机验证。
- **非 100% DPI 缩放**下的布局、小时级 soak：未测。
- **端口选择**：`FER_GUI_ADDR` 必须避开 WinNAT 保留段（19899 实测失败于 os error 10013，25000 通过），
  已写入项目 AGENTS.md 坑列表。

## 6. Everything 式改造（2026-09-30，用户反馈后）

**背景**：初版把「目录占用分析」做成 `du <目录>` 查询子命令 —— 被用户否掉（CLI 思维，不该让用户在搜索框里打命令）。
**改动**（commit a4f4078）：菜单栏（文件/编辑/视图/搜索/工具）+ 工具栏按钮；搜索框彻底去子命令；
占用视图改由按钮/菜单/右键进入并支持面包屑与「返回上级」/Backspace；新增搜索历史（Ctrl+H 最近查询）。

| 场景 | 证据（_build/lead-verify/） | 观测 |
|---|---|---|
| 菜单栏出现 | shot30-menu.png | 顶部 文件(F) 编辑(E) 视图(V) 搜索(S) 工具(T) 五项 |
| 菜单命令 → 占用分析 | shot31-menu-du.png | WM_COMMAND 2006 → 标题加「- 占用分析」、列头切为 大小\|文件数\|占用、状态栏面包屑 `占用 D: › Kita-Tools › Coding › fer-gui［…］ —— 合计 3.02 GB（分配 3.03 GB）· 8,506 文件 · 696 目录` |
| 关于对话框 | shot32-about.png | WM_COMMAND 2015 → 搜索语法 + 快捷键 + 配置/历史路径 |
| 工具栏导航 | shot21-du-view.png / shot22-du-up.png | 「占用分析」→ 当前目录占用；「返回上级」→ 上一层（面包屑同步缩短） |
| 门禁 | `cargo test/clippy --target-dir _build\lead-verify` | **128 + 26 passed / 0 failed**（4 ignored 为真实 serve 联调）、clippy `-D warnings` **0 警告** |

未覆盖：菜单里 2009 重建索引 / 2010 全量重扫 / 2011 启动 serve 三项未实触发（会改索引或起进程，会干扰其它 agent 的验证；
其中「启动服务」已在 §3 用独立端口 25000 验证过成功分支）。

## 7. 逐轮修复链（独立验证者驱动，2026-09-30）

| 轮次 | 发现 | 处置 | 复验 |
|---|---|---|---|
| R1 | **FAIL-1**：点 X / Alt+F4 关不掉窗口（winsafe 有用户 WM_CLOSE 回调即短路 DefWindowProc） | `wm_close` 里显式 `DestroyWindow` | R2.1 PASS（两路径 66/64 ms 退出、退出码 0、config 落盘） |
| R3 | **R3-D1**：查询框为空时退出占用视图，标题/列头不复位 | `exit_du` 里无条件复位标题 + 搜索列头 | R4 PASS（清空查询 + 搜索模式 / 连点到盘根 两条路径） |
| R4 | **R4-D1**：Esc 退不出占用视图（comctl32 的 ListView 吞 VK_ESCAPE、不发 LVN_KEYDOWN；同一时刻 Backspace 正常） | 改用**窗口加速键表**注册 Esc / Backspace / Ctrl+H / Ctrl+C，不依赖焦点 | R5 PASS（主窗口投 Esc 即复位；Ctrl+H 在搜索框焦点下也生效；Ctrl+C 内容与 API 首行逐字一致） |

全部修复均提交并有独立复验，未发现回归（每轮都重跑门禁 + 三方对照 + WM_CLOSE）。

## 8. 真实输入验证（kra 宿主机 GUI 自动化，2026-09-30 补做）

**背景**：R1/R3/R5 的实机证据全部来自 PostMessage/UIA 合成注入，把「真人键盘 / 真实右键点击」记为 [UNKNOWN]。
本轮改用 **kita-remote-agent（kra）真实输入通道**补齐（agent 起在 127.0.0.1:25001；本地 token 已备份，验证后恢复）。

| 项 | 手法 | 实测 |
|---|---|---|
| 真实键盘打字 | `kra input type "ext:rs size:>1mb"` | 输入框被逐字键入（`uia getValue` 读回一致），结果 **56 条**，与 `/api/search` total=56 一致 |
| 真实右键点击 | `kra input right_click`（SendInput，回执 `dispatch=foreground:sendinput`） | 结果行右键 → 上下文菜单 **6 项完整显示**（截图 `shot61-rightclick.png`、`shot51-menu-live.png`） |
| 真实菜单项选择 | 菜单内真实键盘 ↓×3 + Enter；另一次 ↓×4 + Enter | 第一次选中「分析此目录占用」→ 进入占用视图（标题加后缀）；第二次 → 剪贴板 = `D:\\Kita-Tools\\Coding\\fer-gui\\Cargo.toml`，状态栏「已复制：…」 |
| 真实 Esc | `kra input key esc` | 占用视图标题复位 `fer GUI - File Express Retriever` |
| 剪贴板核验 | `kra clipboard` 读回 | 复制前后比对；测试用剪贴板已恢复原值 |

**结论**：R1/R3/R5 遗留的 [UNKNOWN]（真实键盘、真实右键与菜单点击）**全部升级为 [MEASURED]**，未发现新缺陷。
此前「合成右键不触发 NM_RCLICK」是**合成注入方式**的盲区，不是程序缺陷 —— 教训：实机验证优先用 kra 的真实输入通道。

**仍未覆盖**：非 100% DPI 缩放布局、小时级 soak、菜单里 2009/2010/2011 三项的实触发。
