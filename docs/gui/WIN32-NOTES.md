# fer-gui 项目记忆（agent 速览）

一句话定位：**fer（File Express Retriever）的 Windows 原生 GUI 前端** —— 纯 Rust + winsafe（手写 Win32 绑定），
零 C 依赖、零第三方运行时依赖；数据面走常驻 fer serve 的 HTTP API（默认 127.0.0.1:19876）。

选型与踩坑依据：工作区 `Notes\rust-win32-gui.md`（winsafe 选型结论 + manifest/资源坑 + 实测体积）。

## 目录

- `src/main.rs`     —— UI 装配与线程模型（Lead 独占写入）
- `src/json.rs`     —— 零依赖 JSON 解析器（core-io 独占）
- `src/http.rs`     —— 极简 HTTP/1.1 客户端（core-io 独占）
- `src/ferapi.rs`   —— fer API 客户端与数据模型（core-io 独占）
- `src/format.rs`   —— 展示层格式化纯函数（features 独占）
- `src/config.rs`   —— 配置持久化（features 独占）
- `src/actions.rs`  —— 系统集成（explorer 定位 / 剪贴板）（features 独占）
- `src/query.rs`    —— 搜索会话状态机（applogic 独占）
- `src/serve_ctl.rs` —— serve 探测与自动拉起（applogic 独占）
- `src/menu.rs`     —— 菜单栏定义与构建（命令 ID 2001..=2016，applogic 独占）
- `src/history.rs`  —— 搜索历史（`%LOCALAPPDATA%\fer-gui\history.txt`，core-io 独占）
- `tests/`          —— 集成测试（query 状态机 + 真实 serve 联调，后者 #[ignore]）
- `scripts/gui_probe.ps1` —— 实机验收探针（按 pid 找窗口 → WM_SETTEXT 注入查询 → 截屏）
- `scripts/verify.ps1`    —— 构建/接口/视觉/对照验收脚本
- `docs/INTERFACES.md` —— 冻结的模块接口契约（改接口必须先改它）
- `_build/` `_run/` —— 产物与运行数据（前缀 `_` = 不入库）

## 构建（本机无 MSVC 链接器，必须走 GNU 链）

```powershell
$env:HTTPS_PROXY='http://127.0.0.1:3067'   # 首次拉包
& "D:\Kita-Tools\DevEnv\cargo\bin\rustup.exe" run stable-x86_64-pc-windows-gnu cargo build --release
```

产物 `target-gnu\release\fer-gui.exe`。测试：`cargo test`（纯逻辑单测，不开窗口）；
静态检查：`cargo clippy --all-targets`（保持 0 警告）。

## 关键坑（勿重蹈）

- **必须自带 manifest**（`app.manifest` + `app.rc` + `build.rs`）—— 否则 comctl32 退回 v5，控件变 Win95 风格。
  验证：`(Get-Process -Id <pid>).Modules | ? ModuleName -eq 'COMCTL32.dll'` 应指向 WinSxS 的 `6.0.*`。
- **嵌资源禁用静态库**：GNU ld 会丢弃无符号引用的 `.a` 成员（资源静默丢失），必须 `cargo:rustc-link-arg` 直传 `.o`。
- **控件必须在 `run_main` 之前构造**；控件句柄必须存进结构体字段（drop 掉即丢失句柄）。
- **跨线程更新 UI 必须切回 UI 线程**：本项目的做法是后台线程只做 HTTP，结果经 mpsc 送回，
  由 `WM_TIMER(40ms)` 在 UI 线程消费 —— 不要用 `Rc<RefCell<..>>` 跨线程（不是 Send，编译期就过不去）。
- **所有 .cmd/.bat 必须 CRLF + 纯 ASCII**（工作区既有教训）；Rust 源码用 LF 无妨。
- **定时器必须在 `wm_create` 里设置**（2026-09-30 实测血案）：`run_main` 之前 `wnd.hwnd()` 仍是 NULL，
  此时 `SetTimer(NULL, ..)` 把定时器绑到**线程**而非窗口，`WM_TIMER` 永远到不了窗口过程 ——
  表现为「后台线程结果收到了但界面永不刷新」（状态栏一直停在初始文案）。同理，`spawn_thread` 的
  `SendMessage(WM_UI_THREAD)` 在窗口创建前发出也会丢（`GetAncestor` 返回 NULL），
  所以启动初始化（拉 stats、恢复上次查询）要放在**首次 WM_TIMER** 里做。
- **ListView 的列要用 `ListViewOpts.columns` 声明**：在 `ListView::new` 之后调 `list.cols().add(..)`
  实测表头为空（列没生效），改 `columns: &[("名称", gui::dpi_x(260)), ..]` 立即正常。
- **ShellExecuteW 不在 windows-sys 的 Win32_UI_Shell 里**（0.61.2 实测）：UI/Shell 的每个条目另被
  `Win32_UI_WindowsAndMessaging` 二次门控，只开 Win32_UI_Shell 会 E0432。本项目按 Win32 文档自声明
  `#[link(name = "shell32")] unsafe extern "system" { fn ShellExecuteW(..) }`（与 GlobalFree 同一先例），
  已实调验证（返回值 > 32 判成功）。
- **交互约定（2026-09-30 用户反馈后定调）**：功能一律从菜单/工具栏/右键进入，**搜索框只做搜索**。
  早期版本曾用 `du <目录>` 子命令切占用视图，被用户否掉（CLI 思维）；占用分析现在是工具栏/菜单/右键命令，
  视图内用面包屑 + 「返回上级」/Backspace 导航，查询框内容在任何模式下都不被改写。
- **winsafe 菜单**：`WindowMainOpts.menu` 接 `HMENU`（`HMENU` 无 `Drop`，交给窗口持有即可）；
  命令走 `WM_COMMAND`，用 `wm_command_acc_menu(id, ..)` 注册（它同时挂 Menu 与 Accel 两个通知码）。
  测试时可用 `PostMessage(hwnd, 0x0111, MAKEWPARAM(id,0), 0)` 直接触发菜单命令。
- **Esc 要用加速键表，别挂在 ListView 的 `lvn_key_down` 上**（R4-D1 实测）：comctl32 的 ListView 会吞掉
  `VK_ESCAPE`、不产生 `LVN_KEYDOWN`（同一时刻 Backspace 却正常触发），所以挂在结果表上的 Esc 分支永不命中。
  正解是 `WindowMainOpts.accel_table`：`HACCEL::CreateAcceleratorTable(&[ACCEL { fVirt: co::ACCELF::VIRTKEY, key, cmd }])`，
  由消息循环的 `TranslateAccelerator` 处理，**不依赖焦点**，命令经 `WM_COMMAND`（Accel 通知码）回到
  `wm_command_acc_menu`；本项目**只放了 Esc 与 Ctrl+H 两条** —— 教训（2026-09-30 用户报障）：
  加速键是 `TranslateAccelerator` 在消息循环层面**无条件**拦截的，一旦把 `Backspace`/`Ctrl+C` 也放进去，
  搜索框就再也收不到它们，直接表现为「输入框不能退格删除」「输入框里 Ctrl+C 复制不了文本」。
  判据：**只把「在输入框里没有默认语义」的键放进加速键表**；Backspace 与 Ctrl+C 留在
  ListView 的 `lvn_key_down`（结果表有焦点时才有意义）。
- **MessageBox 返回 `co::DLGID`**（不是 MBRES）：判 `co::DLGID::YES`；与 `co::MB::YESNO` 配合做两步确认。
- **自定义端口要避开 WinNAT 保留段**：`FER_GUI_ADDR` 落在保留段时，serve 会以
  `os error 10013`（访问套接字权限不允许）退出，界面显示「已启动 fer serve（pid N），但 8000 ms 内未就绪」。
  查保留段用 `netsh interface ipv4 show excludedportrange protocol=tcp`（本机 19879-19978、20935-21134
  等大片保留；实测 25000 可用、19899 不可用）。这与 fer 默认端口从 9876 改到 19876 是同一个坑。
- **实机验证优先走 kra 的真实输入通道，别只用 PostMessage/UIA 合成**（2026-09-30 教训）：合成注入在 comctl32
  控件上有盲区 —— PostMessage 的 WM_RBUTTONDOWN/UP **不产生 NM_RCLICK**，一度被误判成「右键菜单坏了」，而用
  kra 真实右键点击立刻看到菜单。用法：
  `kita-remote-agent.exe --port <port>` 起 agent（**端口要避开 WinNAT 保留段**，8888/18999 本机都不可用，
  实测 25001 可用）→ `kita-remote.exe pair http://127.0.0.1:<port> <配对码>`（配对码在 agent 启动输出里；
  token 落 `~/.kita-remote.json`，**动它前先备份**）→ 之后 `-server <url> a11y`（定位）/
  `input type|right_click|key|combo`（真实输入）/ `screenshot`（取证）/ `clipboard`（回读核验）。
  注意 `input right_click` 无后台语义，会动真实光标（回执里的 `dispatch=foreground:sendinput` 是实况）。
- **构建期间不能有在跑的 fer-gui 实例**：exe 被占用会让链接阶段报「拒绝访问」；
  多人并行编译时用独立 `CARGO_TARGET_DIR`（`.gitignore` 已覆盖 `/target-gnu*`）。
- 本机无 MSVC 链接器；`rustup run stable-x86_64-pc-windows-gnu` 是唯一正确入口（裸 cargo 会捡到 MSVC rustc）。

## 与 fer 的边界

- 只通过 HTTP API 交互（`GET /api/search|stats|du|feed`、`POST /api/rebuild|rescan|reveal`），
  **不改 fer 仓库代码**；GUI 侧不解析 dump、不依赖 dump 版本。
- `/api/reveal` 是唯一会拉起进程的 fer 接口，fer 侧已有 Origin 校验；GUI 侧另有本地 `actions::reveal_in_explorer`。
- serve 未运行时 GUI 必须给出明确状态（状态栏显示「serve 未连接」），不得崩溃。
