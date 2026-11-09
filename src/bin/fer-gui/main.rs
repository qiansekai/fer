#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! fer GUI —— fer（File Express Retriever）的 Windows 原生前端。
//!
//! 纯 Rust + winsafe（手写 Win32 绑定，零 C 依赖、零第三方运行时依赖）；数据面走
//! 常驻 fer serve 的 HTTP API（默认 127.0.0.1:19876）。
//!
//! 线程模型：UI 线程只碰控件；后台线程只做 HTTP；结果经 mpsc 回到 UI 线程，由
//! WM_TIMER 驱动消费（跨线程直接操作控件会死锁，winsafe 文档明确要求走 UI 线程）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use winsafe::{self as w, co, gui, prelude::*};

mod actions;
mod config;
mod ferapi;
mod format;
mod history;
mod http;
mod json;
mod menu;
mod query;
mod serve_ctl;
mod theme;

use config::Config;
use ferapi::{FerClient, Hit, SearchResult};
use query::{SearchState, SortKey};

const PAD: i32 = 8;
/// 自绘菜单栏高度（逻辑像素）：Win10 的系统菜单栏无法跟随深色，改在客户区自绘。
const MENU_H: i32 = 22;
const TITLE_BASE: &str = "fer GUI - File Express Retriever";
const TIMER_PUMP: usize = 1;
/// 输入防抖期间的泵间隔。
const TIMER_PUMP_MS: u32 = 40;
/// 空闲心跳间隔：只用来兜底处理迟到的回包，不做事时开销可忽略。
const TIMER_IDLE_MS: u32 = 200;
const TIMER_STATS: usize = 2;
const TIMER_STATS_MS: u32 = 4000;
const DEBOUNCE_MS: u64 = 150;
/// 快捷筛选 chip：(显示文本, 查询 term, 互斥组)。
///
/// 语义是**开关**：点一次加入、再点一次移除；互斥组内加入新值会先剔除同组旧值
/// （文件 vs 目录、今天 vs 本周）。组为 `None` 的 chip 可自由叠加。
const CHIP_DEFS: [(&str, &str, Option<&str>); 6] = [
	("文件", "type:file", Some("type")),
	("目录", "type:dir", Some("type")),
	("今天", "dm:today", Some("dm")),
	("本周", "dm:thisweek", Some("dm")),
	(">1MB", "size:>1mb", None),
	("隐藏", "hidden:true", None),
];
const HTTP_TIMEOUT: Duration = Duration::from_millis(4000);
/// 结果表右键菜单（复用菜单栏命令 ID，两者走同一个 menu_command 分派）。
const CONTEXT_MENU_IDS: [u16; 6] = [
	menu::CMD_OPEN,
	menu::CMD_REVEAL,
	menu::CMD_DU_ANALYZE,
	menu::CMD_COPY_PATH,
	menu::CMD_COPY_NAME,
	menu::CMD_COPY_DIR,
];
/// 动态「最近查询」菜单项 ID 基址（ID = 基址 + 历史序号）。
const MENU_HISTORY_BASE: u16 = 1000;
const MENU_HISTORY_CLEAR: u16 = 1999;
/// 加速键专用命令（不属菜单栏，只给窗口消息循环用）。
const CMD_HISTORY_OPEN: u16 = 1998;
/// Enter 加速键：焦点在搜索框=立即搜索，焦点在结果表=打开/下钻（用焦点判断，不改列表语义）。
const CMD_ENTER_KEY: u16 = 1997;
fn main() {
	theme::prefer_dark_app_mode();
	if let Err(err) = App::create_and_run() {
		w::HWND::NULL
			.MessageBox(&err.to_string(), "fer GUI - uncaught error", co::MB::ICONERROR)
			.unwrap();
	}
}

/// 后台线程 -> UI 线程的消息。
enum TaskMsg {
	Search { generation: u64, result: Result<SearchResult, String> },
	Du { generation: u64, result: Result<ferapi::DuResult, String> },
	ServeUp(serve_ctl::ServeStatus),
	IndexInfo(String),
}

/// 面包屑："D:\\a\\b" -> "D: › a › b"。
/// 注：features 的 `format::path_segments`（task-10）合入后可替换为它（届时可按段做可点击导航）。
fn breadcrumb(path: &str) -> String {
	format::path_segments(path)
		.iter()
		.map(|(disp, _)| disp.as_str())
		.collect::<Vec<_>>()
		.join(" › ")
}

/// 结果表当前承载的内容类型。
#[derive(Debug, Clone, PartialEq)]
enum Mode {
	/// 普通文件搜索（双击=资源管理器定位）。
	Search,
	/// 目录占用聚合（双击=下钻该子目录）。
	Du { root: String },
}

struct AppState {
	search: SearchState,
	cfg: Config,
	hits: Vec<Hit>,
	/// 输入防抖：待发起的查询与其最后编辑时间（0 = 无待发起）。
	pending_query: String,
	pending_since_ms: u64,
	index_line: String,
	/// 首次 WM_TIMER 才做启动初始化（定时器绑定在 wm_create 之后）。
	bootstrapped: bool,
	/// 右键菜单作用的行号（菜单命令回调是窗口级，无法带参）。
	context_row: Option<usize>,
	/// 当前结果表模式。
	mode: Mode,
	/// 占用请求的独立代际号（与搜索代际互不干扰）。
	du_generation: u64,
	/// 最近查询（Everything 式搜索历史）。
	history: history::History,
}

#[derive(Clone)]
struct App {
	wnd: gui::WindowMain,
	/// 自绘菜单栏按钮（文件/编辑/视图/搜索/工具）。
	menu_buttons: Vec<gui::Button>,
	txt_query: gui::Edit,
	btn_search: gui::Button,
	/// 行负载 = 结果序号（供键盘/右键按行定位，不依赖 ListView 的 uid）。
	list: gui::ListView<usize>,
	chips: Vec<gui::CheckBox>,
	btn_du: gui::Button,
	btn_up: gui::Button,
	btn_rebuild: gui::Button,
	btn_serve: gui::Button,
	lbl_status: gui::Label,
	lbl_index: gui::Label,
	/// 深色画刷（裸句柄；进程退出由系统回收）。
	brush_bg: isize,
	brush_edit: isize,
	/// 结果表小图标列表（裸句柄）与「文件类型 -> 图标序号」缓存。
	icon_list: isize,
	icons: Rc<RefCell<std::collections::HashMap<String, i32>>>,
	client: Arc<FerClient>,
	tx: Sender<TaskMsg>,
	rx: Rc<RefCell<Receiver<TaskMsg>>>,
	state: Rc<RefCell<AppState>>,
}

/// 把窗口位置夹回虚拟屏幕。
///
/// 多显示器下负坐标是合法的（左侧副屏），所以不能用「x < 0 就重置」的粗暴规则；
/// 判据是窗口矩形与虚拟屏幕的交集面积是否达到 40%，不够就居中。
fn clamp_window_pos(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
		SM_YVIRTUALSCREEN,
	};
	unsafe {
		let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
		let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
		let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
		let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
		let iw = (x + w).min(vx + vw) - x.max(vx);
		let ih = (y + h).min(vy + vh) - y.max(vy);
		let visible =
			iw > 0 && ih > 0 && (iw as i64 * ih as i64) * 5 >= (w as i64 * h as i64) * 2;
		if visible {
			(x, y)
		} else {
			(vx + (vw - w) / 2, vy + (vh - h) / 2)
		}
	}
}

/// 取一个文件类型的小图标并加入 ImageList，返回图标序号（失败 -1）。
///
/// 用 `SHGFI_USEFILEATTRIBUTES`：按扩展名取系统通用图标，不访问磁盘，毫秒级返回。
fn icon_index_for(icon_list: isize, path: &str, is_dir: bool) -> i32 {
	use windows_sys::Win32::Storage::FileSystem::{
		FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
	};
	use windows_sys::Win32::UI::Shell::{
		SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;

	let mut shfi = unsafe { std::mem::zeroed::<SHFILEINFOW>() };
	let attrs = if is_dir {
		FILE_ATTRIBUTE_DIRECTORY
	} else {
		FILE_ATTRIBUTE_NORMAL
	};
	let wide = w::WString::from_str(path);
	let ok = unsafe {
		SHGetFileInfoW(
			wide.as_ptr(),
			attrs,
			&mut shfi,
			std::mem::size_of::<SHFILEINFOW>() as u32,
			SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
		)
	};
	if ok == 0 || shfi.hIcon.is_null() {
		return -1;
	}
	let himgl = unsafe { w::HIMAGELIST::from_ptr(icon_list as *mut std::ffi::c_void) };
	let hicon = unsafe { w::HICON::from_ptr(shfi.hIcon) };
	let idx = himgl.AddIcon(&hicon).map(|i| i as i32).unwrap_or(-1);
	unsafe { DestroyIcon(shfi.hIcon) };
	idx
}

/// 后台线程回包后唤醒 UI 泵。
///
/// 从后台线程 `SetTimer` 且传入有效 hwnd 时，定时器绑定到窗口所在的 UI 线程；
/// 1ms 只是「尽快醒一次」，pump 处理完会自行 KillTimer。
fn wake_pump(hwnd_raw: isize) {
	unsafe {
		windows_sys::Win32::UI::WindowsAndMessaging::SetTimer(
			hwnd_raw as *mut std::ffi::c_void,
			TIMER_PUMP,
			1,
			None,
		);
	}
}

/// Windows 的 MEASUREITEMSTRUCT（winsafe 未封装，按官方定义自声明）。
#[repr(C)]
struct MeasureItemStruct {
	ctl_type: u32,
	ctl_id: u32,
	item_id: u32,
	item_width: u32,
	item_height: u32,
	item_data: usize,
}

impl App {
	fn create_and_run() -> w::AnyResult<i32> {
		let cfg = Config::load();
		let (tx, rx) = mpsc::channel::<TaskMsg>();

		// 结果表的小图标列表；生命周期跟进程（forget 掉 guard，避免离开作用域即销毁）。
		let icon_list = {
			let himgl = w::HIMAGELIST::Create(w::SIZE::with(16, 16), co::ILC::COLOR32, 32, 32)
				.map_err(|e| e.to_string())?;
			let raw = himgl.ptr() as isize;
			std::mem::forget(himgl);
			raw
		};

		// 控件布局按默认逻辑尺寸计算；窗口几何（物理像素）在 bootstrap 里用 SetWindowPos 恢复。
		let win_w = 1180;
		let win_h = 720;

		// 加速键表：由消息循环的 TranslateAccelerator 处理，不依赖焦点。
		// 起因（R4-D1 实测）：comctl32 的 ListView 会吞掉 VK_ESCAPE、不产生 LVN_KEYDOWN，
		// 挂在结果表上的 Esc 分支永不命中；Backspace 则正常。
		// 只放「在输入框里没有默认语义」的键：Esc（取消/退出占用）与 Ctrl+H（历史）。
		// 教训（2026-09-30 用户报障）：加速键由 TranslateAccelerator 在消息循环层面**无条件**拦截，
		// 一旦把 Backspace / Ctrl+C 放进来，搜索框就再也收不到它们 —— 表现为「输入框不能退格删除」
		// 与「输入框里 Ctrl+C 复制不了文本」。这两个键只在结果表有焦点时才有意义，
		// 因此留在 ListView 的 lvn_key_down（见 events()）里，不进加速键表。
		let accel = w::HACCEL::CreateAcceleratorTable(&[
			w::ACCEL { fVirt: co::ACCELF::VIRTKEY, key: co::VK::ESCAPE, cmd: menu::CMD_MODE_SEARCH },
			w::ACCEL {
				fVirt: co::ACCELF::VIRTKEY | co::ACCELF::CONTROL,
				key: unsafe { co::VK::from_raw(0x48) },
				cmd: CMD_HISTORY_OPEN,
			},
			// Enter 在输入框里没有默认语义（非对话框窗口无默认按钮），放进来可让「打完字回车即搜」；
			// 结果表里的 Enter 语义由回调内的焦点判断保持（打开/下钻）。
			w::ACCEL { fVirt: co::ACCELF::VIRTKEY, key: co::VK::RETURN, cmd: CMD_ENTER_KEY },
		])
		.ok();

		let bg_brush = unsafe { w::HBRUSH::from_ptr(theme::solid(theme::BG)) };
		let wnd = gui::WindowMain::new(gui::WindowMainOpts {
			title: TITLE_BASE,
			size: gui::dpi(win_w, win_h),
			// 系统菜单栏在 Win10 无法变深，改在客户区顶部自绘（见 menu_buttons）。
			accel_table: accel,
			class_icon: gui::Icon::Id(1),
			class_bg_brush: gui::Brush::Handle(bg_brush),
			..Default::default()
		});

		// 自绘菜单栏：扁平按钮，点击弹出 owner-draw 深色下拉菜单。
		let menu_buttons: Vec<gui::Button> = menu::MENUS
			.iter()
			.enumerate()
			.map(|(i, (title, _))| {
				gui::Button::new(&wnd, gui::ButtonOpts {
					text: title,
					position: gui::dpi(PAD + i as i32 * 62, 0),
					width: gui::dpi_x(58),
					height: gui::dpi_y(MENU_H),
					control_style: co::BS::FLAT,
					..Default::default()
				})
			})
			.collect();

		let txt_query = gui::Edit::new(&wnd, gui::EditOpts {
			position: gui::dpi(PAD, PAD + MENU_H),
			width: gui::dpi_x(win_w - PAD * 2 - 110),
			height: gui::dpi_y(26),
			control_style: co::ES::AUTOHSCROLL,
			resize_behavior: (gui::Horz::Resize, gui::Vert::None),
			..Default::default()
		});

		let btn_search = gui::Button::new(&wnd, gui::ButtonOpts {
			text: "搜索",
			position: gui::dpi(win_w - PAD - 100, PAD + MENU_H),
			width: gui::dpi_x(100),
			height: gui::dpi_y(26),
			resize_behavior: (gui::Horz::Repos, gui::Vert::None),
			..Default::default()
		});

		let chips = Self::build_chips(&wnd);

		// 工具栏（Everything 式：功能从按钮/菜单进入，搜索框只做搜索）
		let tools_x = PAD + 6 * 70 + 14;
		let btn_du = gui::Button::new(&wnd, gui::ButtonOpts {
			text: "占用分析",
			position: gui::dpi(tools_x, PAD + MENU_H + 32),
			width: gui::dpi_x(80),
			height: gui::dpi_y(24),
			..Default::default()
		});
		let btn_up = gui::Button::new(&wnd, gui::ButtonOpts {
			text: "返回上级",
			position: gui::dpi(tools_x + 84, PAD + MENU_H + 32),
			width: gui::dpi_x(80),
			height: gui::dpi_y(24),
			..Default::default()
		});
		let btn_rebuild = gui::Button::new(&wnd, gui::ButtonOpts {
			text: "重建索引",
			position: gui::dpi(tools_x + 168, PAD + MENU_H + 32),
			width: gui::dpi_x(80),
			height: gui::dpi_y(24),
			..Default::default()
		});
		let btn_serve = gui::Button::new(&wnd, gui::ButtonOpts {
			text: "启动服务",
			position: gui::dpi(tools_x + 252, PAD + MENU_H + 32),
			width: gui::dpi_x(80),
			height: gui::dpi_y(24),
			..Default::default()
		});

		let list = gui::ListView::<usize>::new(&wnd, gui::ListViewOpts {
			position: gui::dpi(PAD, PAD + MENU_H + 34 + 30),
			size: gui::dpi(win_w - PAD * 2, win_h - PAD * 2 - MENU_H - 34 - 30 - 22),
			control_style: co::LVS::REPORT | co::LVS::SHOWSELALWAYS,
			control_ex_style: co::LVS_EX::FULLROWSELECT
				| co::LVS_EX::DOUBLEBUFFER,
			resize_behavior: (gui::Horz::Resize, gui::Vert::Resize),
			columns: &[
				("名称", gui::dpi_x(300)),
				("所在目录", gui::dpi_x(430)),
				("大小", gui::dpi_x(80)),
				("修改时间", gui::dpi_x(130)),
				("类型", gui::dpi_x(60)),
			],
			..Default::default()
		});

		let lbl_status = gui::Label::new(&wnd, gui::LabelOpts {
			text: "就绪",
			position: gui::dpi(PAD, win_h - PAD - 18),
			size: gui::dpi(win_w - PAD * 2 - 430, 18),
			resize_behavior: (gui::Horz::Resize, gui::Vert::Repos),
			..Default::default()
		});

		// 右半区独立显示索引/服务状态，避免 4s 一次的轮询把搜索状态覆盖掉。
		let lbl_index = gui::Label::new(&wnd, gui::LabelOpts {
			text: "",
			position: gui::dpi(win_w - PAD - 424, win_h - PAD - 18),
			size: gui::dpi(424, 18),
			control_style: co::SS::RIGHT,
			resize_behavior: (gui::Horz::Repos, gui::Vert::Repos),
			..Default::default()
		});

		let state = Rc::new(RefCell::new(AppState {
			search: SearchState::new(cfg.limit),
			cfg,
			hits: Vec::new(),
			pending_query: String::new(),
			pending_since_ms: 0,
			index_line: String::new(),
			bootstrapped: false,
			context_row: None,
			mode: Mode::Search,
			du_generation: 0,
			history: history::History::load(20),
		}));

		let client = Arc::new(FerClient::new(
			&std::env::var("FER_GUI_ADDR").unwrap_or_else(|_| "127.0.0.1:19876".to_string()),
			HTTP_TIMEOUT,
		));

		let app = Self {
			wnd,
			menu_buttons,
			txt_query,
			btn_search,
			list,
			chips,
			btn_du,
			btn_up,
			btn_rebuild,
			btn_serve,
			lbl_status,
			lbl_index,
			brush_bg: theme::solid(theme::BG) as isize,
			brush_edit: theme::solid(theme::EDIT_BG) as isize,
			icon_list,
			icons: Rc::new(RefCell::new(std::collections::HashMap::new())),
			client,
			tx,
			rx: Rc::new(RefCell::new(rx)),
			state,
		};

		{
			let (sort, desc, limit) = {
				let st = app.state.borrow();
				(
					SortKey::from_api(&st.cfg.sort_key).unwrap_or(SortKey::Name),
					st.cfg.sort_desc,
					st.cfg.limit,
				)
			};
			let mut st = app.state.borrow_mut();
			st.search.set_sort(sort, desc);
			st.search.set_limit(limit);
		}

		app.events();
		app.wnd.run_main(None)
	}

	fn build_chips(wnd: &gui::WindowMain) -> Vec<gui::CheckBox> {
		CHIP_DEFS
			.iter()
			.enumerate()
			.map(|(i, (label, _, _))| {
				gui::CheckBox::new(wnd, gui::CheckBoxOpts {
					text: label,
					// Everything 式：筛选 chip 横排在搜索框下方。
					position: gui::dpi(PAD + i as i32 * 70, PAD + MENU_H + 32),
					size: gui::dpi(66, 24),
					// PUSHLIKE：AutoCheckBox 呈现为可按下的按钮，按下态 = 该条件已启用。
					control_style: co::BS::AUTOCHECKBOX | co::BS::PUSHLIKE,
					..Default::default()
				})
			})
			.collect()
	}

	/// 弹出第 index 个下拉菜单（owner-draw，配色由 WM_DRAWITEM 处理）。
	fn show_menu(&self, index: usize) {
		let Ok(mut popup) = menu::build_popup_at(index) else {
			return;
		};
		let Some(btn) = self.menu_buttons.get(index) else {
			return;
		};
		let Ok(rc) = btn.hwnd().GetWindowRect() else {
			let _ = popup.DestroyMenu();
			return;
		};
		let pt = w::POINT { x: rc.left, y: rc.bottom };
		let _ = popup.track_popup_menu_at_point(pt, self.wnd.hwnd(), self.wnd.hwnd());
		let _ = popup.DestroyMenu();
	}

	fn events(&self) {
		self.btn_search.on().bn_clicked({
			let app = self.clone();
			move || {
				app.trigger_search_now();
				Ok(())
			}
		});

		for (i, btn) in self.menu_buttons.iter().enumerate() {
			btn.on().bn_clicked({
				let app = self.clone();
				move || {
					app.show_menu(i);
					Ok(())
				}
			});
		}

		self.txt_query.on().en_change({
			let app = self.clone();
			move || {
				app.on_query_changed();
				Ok(())
			}
		});

		// 菜单栏命令（ID 见 src/menu.rs；与右键菜单、历史菜单 ID 段互不相交）。
		for id in menu::ALL_COMMANDS {
			self.wnd.on().wm_command_acc_menu(id, {
				let app = self.clone();
				move || {
					app.menu_command(id);
					Ok(())
				}
			});
		}

		self.btn_du.on().bn_clicked({
			let app = self.clone();
			move || {
				app.analyze_selection();
				Ok(())
			}
		});

		self.btn_up.on().bn_clicked({
			let app = self.clone();
			move || {
				app.du_up();
				Ok(())
			}
		});

		self.btn_rebuild.on().bn_clicked({
			let app = self.clone();
			move || {
				app.rebuild_index();
				Ok(())
			}
		});

		self.btn_serve.on().bn_clicked({
			let app = self.clone();
			move || {
				app.start_serve();
				Ok(())
			}
		});

		for (i, chip) in self.chips.iter().enumerate() {
			chip.on().bn_clicked({
				let app = self.clone();
				move || {
					app.apply_chip(CHIP_DEFS[i].1, CHIP_DEFS[i].2);
					Ok(())
				}
			});
		}

		self.wnd.on().wm_close({
			let app = self.clone();
			move || {
				app.save_config();
				// 必须显式销毁窗口：winsafe 一旦有用户 WM_CLOSE 回调就短路 DefWindowProc
				// （而 WM_CLOSE 的默认处理正是 DestroyWindow），回调返回 0 不足以关窗 ——
				// 缺这一行的后果是点 X / Alt+F4 全都关不掉，进程永久驻留。
				// 销毁后由 winsafe 的 WM_NCDESTROY 默认处理 PostQuitMessage，run_main 才返回。
				let _ = app.wnd.hwnd().DestroyWindow();
				Ok(())
			}
		});

		self.list.on().lvn_item_activate({
			let app = self.clone();
			move |p: &w::NMITEMACTIVATE| {
				app.on_item_activate(p.iItem as usize);
				Ok(())
			}
		});

		self.list.on().lvn_key_down({
			let app = self.clone();
			move |p: &w::NMLVKEYDOWN| {
				let ctrl = w::GetKeyState(co::VK::CONTROL).0;
				if ctrl && p.wVKey == unsafe { co::VK::from_raw(0x43) } {
					app.copy_focused_path();
				} else if ctrl && p.wVKey == unsafe { co::VK::from_raw(0x48) } {
					app.show_history_menu(); // Ctrl+H：最近查询（菜单栏的「搜索 → 最近查询」同一入口）
				} else if p.wVKey == co::VK::RETURN {
					if let Some(idx) = app.focused_index() {
						app.on_item_activate(idx);
					}
				} else if p.wVKey == co::VK::BACK {
					app.du_up();
				} else if p.wVKey == co::VK::ESCAPE {
					app.exit_du();
				}
				Ok(())
			}
		});

		self.list.on().nm_r_click({
			let app = self.clone();
			move |p: &w::NMITEMACTIVATE| -> w::AnyResult<i32> {
				app.on_context_menu(p.iItem, p.ptAction);
				Ok(0)
			}
		});

		// 加速键命令（Ctrl+H 历史、Enter 搜索/打开）
		self.wnd.on().wm_command_acc_menu(CMD_HISTORY_OPEN, {
			let app = self.clone();
			move || {
				app.show_history_menu();
				Ok(())
			}
		});

		self.wnd.on().wm_command_acc_menu(CMD_ENTER_KEY, {
			let app = self.clone();
			move || {
				let in_edit = w::HWND::GetFocus()
					.map(|h| h == *app.txt_query.hwnd())
					.unwrap_or(false);
				if in_edit {
					app.trigger_search_now(); // 输入框回车：跳过防抖立即搜索
				} else if let Some(idx) = app.focused_index() {
					app.on_item_activate(idx); // 结果表回车：打开/下钻（与原来一致）
				}
				Ok(())
			}
		});

		for id in CONTEXT_MENU_IDS {
			self.wnd.on().wm_command_acc_menu(id, {
				let app = self.clone();
				move || {
					app.menu_command(id);
					Ok(())
				}
			});
		}

		// 深色：表头文字用自绘 HDC 文字色（控件主题只换背景，不换表头字色）。
		if let Some(header) = self.list.header() {
			header.on().nm_custom_draw(|p| {
				use winsafe::co::{CDDS, CDRF};
				if p.dwDrawStage == CDDS::PREPAINT {
					return Ok(CDRF::NOTIFYITEMDRAW);
				}
				if p.dwDrawStage == CDDS::ITEMPREPAINT {
					unsafe {
						use windows_sys::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
						SetTextColor(p.hdc.ptr(), theme::FG_DIM);
						SetBkColor(p.hdc.ptr(), theme::BG);
					}
					return Ok(CDRF::NEWFONT);
				}
				Ok(CDRF::DODEFAULT)
			});
		}

		self.list.on().lvn_column_click({
			let app = self.clone();
			move |p: &w::NMLISTVIEW| {
				app.on_column_click(p.iSubItem as usize);
				Ok(())
			}
		});

		// 定时器必须在窗口真正创建之后才能设置：run_main 之前 hwnd 还是 NULL，
		// 此时 SetTimer(NULL, ..) 会把定时器绑到线程而非窗口，WM_TIMER 永远到不了窗口过程。
		// owner-draw 下拉菜单：度量与绘制（Win10 系统菜单不支持深色，必须自绘）。
		self.wnd.on().wm(unsafe { co::WM::from_raw(0x002C) }, |p| {
			unsafe {
				let mis = &mut *(p.lparam as *mut MeasureItemStruct);
				if mis.ctl_type == 1 {
					mis.item_width = 220;
					mis.item_height = 26;
				}
			}
			Ok(1)
		});

		self.wnd.on().wm(unsafe { co::WM::from_raw(0x002B) }, |p| {
			unsafe {
				use windows_sys::Win32::Graphics::Gdi::{
					DrawTextW, FillRect, SetBkMode, SetTextColor,
				};
				let dis = &mut *(p.lparam as *mut w::DRAWITEMSTRUCT);
				if dis.CtlType == co::ODT::MENU {
					// winsafe 的 RECT 与 windows-sys 的是不同类型，需逐字段转换。
					let mut rc = windows_sys::Win32::Foundation::RECT {
						left: dis.rcItem.left,
						top: dis.rcItem.top,
						right: dis.rcItem.right,
						bottom: dis.rcItem.bottom,
					};
					let bg = theme::solid(theme::LIST_BG);
					FillRect(dis.hDC.ptr(), &rc, bg);
					if dis.itemID != u32::MAX && dis.itemData != 0 {
						if dis.itemState.raw() & co::ODS::SELECTED.raw() != 0 {
							let sel = theme::solid(theme::SELECT);
							FillRect(dis.hDC.ptr(), &rc, sel);
						}
						rc.left += 12;
						SetBkMode(dis.hDC.ptr(), 1);
						SetTextColor(dis.hDC.ptr(), theme::FG);
						DrawTextW(dis.hDC.ptr(), dis.itemData as *const u16, -1, &mut rc, 0x0024);
					}
				}
			}
			Ok(1)
		});

		// 深色：编辑框与标签的底色/文字色（控件主题不覆盖这两项）。
		self.wnd.on().wm_ctl_color_edit({
			let app = self.clone();
			move |p: w::msg::WmCtlColorEdit| {
				unsafe {
					use windows_sys::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
					SetTextColor(p.hdc.ptr(), theme::FG);
					SetBkColor(p.hdc.ptr(), theme::EDIT_BG);
				}
				Ok(unsafe { w::HBRUSH::from_ptr(app.brush_edit as *mut std::ffi::c_void) })
			}
		});

		self.wnd.on().wm_ctl_color_static({
			let app = self.clone();
			move |p: w::msg::WmCtlColorStatic| {
				unsafe {
					use windows_sys::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
					SetTextColor(p.hdc.ptr(), theme::FG_DIM);
					SetBkColor(p.hdc.ptr(), theme::BG);
				}
				Ok(unsafe { w::HBRUSH::from_ptr(app.brush_bg as *mut std::ffi::c_void) })
			}
		});

		self.wnd.on().wm_create({
			let app = self.clone();
			move |_p: w::msg::WmCreate| -> w::AnyResult<i32> {
				let wnd_raw = app.wnd.hwnd().ptr();
				theme::apply_window(wnd_raw);
				theme::apply_control(app.list.hwnd().ptr(), "DarkMode_ItemsView");
				if let Some(header) = app.list.header() {
					theme::apply_control(header.hwnd().ptr(), "DarkMode_ItemsView");
				}
				// 列表内部滚动条是独立子窗口，主题不自动继承，单独刷一次。
				unsafe {
					use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowLongW};
					let mut child = FindWindowExW(app.list.hwnd().ptr(), std::ptr::null_mut(), w::WString::from_str("ScrollBar").as_ptr(), std::ptr::null());
					while !child.is_null() {
						theme::apply_control(child, "DarkMode_Explorer");
						let style = GetWindowLongW(child, -16);
						if style & 0x0000_0001 != 0 {
							theme::apply_control(child, "DarkMode_Explorer");
						}
						child = FindWindowExW(app.list.hwnd().ptr(), child, w::WString::from_str("ScrollBar").as_ptr(), std::ptr::null());
					}
				}
				theme::apply_control(app.txt_query.hwnd().ptr(), "DarkMode_Explorer");
				for b in [
					&app.btn_search,
					&app.btn_du,
					&app.btn_up,
					&app.btn_rebuild,
					&app.btn_serve,
				] {
					theme::apply_control(b.hwnd().ptr(), "DarkMode_Explorer");
				}
				for c in &app.chips {
					theme::apply_control(c.hwnd().ptr(), "DarkMode_Explorer");
				}
				// 列表背景/文字色不在主题覆盖范围内，需显式设定。
				use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
				const LVM_SETBKCOLOR: u32 = 0x1000 + 1;
				const LVM_SETTEXTCOLOR: u32 = 0x1000 + 36;
				const LVM_SETTEXTBKCOLOR: u32 = 0x1000 + 38;
				const LVM_SETIMAGELIST: u32 = 0x1000 + 3;
				const LVSIL_SMALL: usize = 1;
				unsafe {
					SendMessageW(app.list.hwnd().ptr(), LVM_SETBKCOLOR, 0, theme::LIST_BG as isize);
					SendMessageW(app.list.hwnd().ptr(), LVM_SETTEXTCOLOR, 0, theme::FG as isize);
					SendMessageW(app.list.hwnd().ptr(), LVM_SETTEXTBKCOLOR, 0, theme::LIST_BG as isize);
					SendMessageW(app.list.hwnd().ptr(), LVM_SETIMAGELIST, LVSIL_SMALL, app.icon_list);
				}
				app.start_timers();
				Ok(0)
			}
		});

		self.wnd.on().wm_timer(TIMER_PUMP, {
			let app = self.clone();
			move || {
				app.pump();
				Ok(())
			}
		});

		self.wnd.on().wm_timer(TIMER_STATS, {
			let app = self.clone();
			move || {
				app.refresh_index_info();
				Ok(())
			}
		});
	}

	// ---------------- 搜索流程 ----------------

	/// 快捷 chip：把 term 追加到查询串末尾并立即搜索。
	/// 应用/取消一个快捷筛选 chip。
	///
	/// 语义是**开关**而非追加：已存在则移除；不存在则加入，并先剔除同组互斥项。
	/// 互斥剔除只针对本组已定义的 chip 值，用户手写的同前缀条件（如 `dm:2020-01-01`）不动。
	fn apply_chip(&self, term: &str, group: Option<&str>) {
		let cur = self.txt_query.text().unwrap_or_default();
		let mut tokens: Vec<String> =
			cur.split_whitespace().map(|t| t.to_string()).collect();

		let present = tokens.iter().any(|t| t.eq_ignore_ascii_case(term));

		if present {
			tokens.retain(|t| !t.eq_ignore_ascii_case(term));
		} else {
			if let Some(group) = group {
				let group_terms: Vec<&str> = CHIP_DEFS
					.iter()
					.filter(|(_, _, g)| *g == Some(group))
					.map(|(_, t, _)| *t)
					.collect();
				tokens.retain(|t| !group_terms.iter().any(|gt| t.eq_ignore_ascii_case(gt)));
			}
			tokens.push(term.to_string());
		}

		let next = tokens.join(" ");
		let _ = self.txt_query.set_text(&next);
		self.sync_chip_states();
		self.trigger_search_now();
	}

	/// 让 chip 的按下态与查询串保持一致（用户手改查询串也会反映）。
	fn sync_chip_states(&self) {
		let cur = self.txt_query.text().unwrap_or_default();
		let tokens: Vec<&str> = cur.split_whitespace().collect();
		for (chip, (_, term, _)) in self.chips.iter().zip(CHIP_DEFS.iter()) {
			chip.set_check(tokens.iter().any(|t| t.eq_ignore_ascii_case(term)));
		}
	}

	/// 关闭时落盘：上次查询 / 排序 / limit（窗口几何在下一批补）。
	fn save_config(&self) {
		let mut cfg = self.state.borrow().cfg.clone();
		cfg.last_query = self.txt_query.text().unwrap_or_default();
		{
			let st = self.state.borrow();
			let (sort, desc) = st.search.sort();
			cfg.sort_key = sort.as_api().to_string();
			cfg.sort_desc = desc;
			cfg.limit = st.search.limit();
		}
		if let Ok(rc) = self.wnd.hwnd().GetWindowRect() {
			cfg.win_x = rc.left;
			cfg.win_y = rc.top;
			cfg.win_w = rc.right - rc.left;
			cfg.win_h = rc.bottom - rc.top;
		}
		{
			let st = self.state.borrow();
			let _ = st.history.save();
		}
		self.state.borrow_mut().cfg = cfg.clone();
		let _ = cfg.save();
	}

	/// 输入变化：登记待发起（由 WM_TIMER 做防抖）。
	fn on_query_changed(&self) {
		let text = self.txt_query.text().unwrap_or_default();
		{
			let mut st = self.state.borrow_mut();
			st.pending_query = text;
			st.pending_since_ms = query::now_ms();
		}
		self.sync_chip_states();
		// 拉起泵，直到防抖到期并处理完（pump 空闲时自行 KillTimer）。
		let _ = self.wnd.hwnd().SetTimer(TIMER_PUMP, TIMER_PUMP_MS, None);
	}

	/// 立即搜索（按钮 / Enter）。
	fn trigger_search_now(&self) {
		let text = self.txt_query.text().unwrap_or_default();
		{
			let mut st = self.state.borrow_mut();
			st.pending_since_ms = 0;
			st.pending_query = text.clone();
		}
		self.start_search(text.trim());
	}

	fn start_search(&self, query: &str) {
		if query.is_empty() {
			self.clear_results();
			return;
		}
		self.state.borrow_mut().history.push(query);
		self.state.borrow_mut().mode = Mode::Search;
		self.set_status(&format!("搜索中… {query}"));
		let req = self.state.borrow_mut().search.begin(query);
		let tx = self.tx.clone();
		let client = self.client.clone();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let result = client
				.search(&req.query, req.limit, Some(req.sort.as_api()), req.desc)
				.map_err(|e| e.to_string());
			let _ = tx.send(TaskMsg::Search { generation: req.generation, result });
			wake_pump(hwnd_raw);
			Ok(())
		});
	}

	/// 当前操作目标（路径, 是否目录）：优先聚焦行，其次右键记录行，最后首行。
	fn target_path(&self) -> Option<(String, bool)> {
		let idx = {
			let focused = self.list.items().focused().map(|it| {
				let d = it.data();
				*d.borrow()
			});
			let st = self.state.borrow();
			focused
				.or(st.context_row)
				.or_else(|| if st.hits.is_empty() { None } else { Some(0) })?
		};
		let st = self.state.borrow();
		let h = st.hits.get(idx)?;
		Some((h.path.clone(), h.is_dir))
	}

	/// 菜单栏与右键菜单共用的命令分派。
	fn menu_command(&self, id: u16) {
		match id {
			menu::CMD_OPEN | menu::CMD_REVEAL => {
				let Some((path, is_dir)) = self.target_path() else {
					self.set_status("请先选中一行");
					return;
				};
				let out = if id == menu::CMD_OPEN && !is_dir {
					actions::open_file(&path).map_err(|e| e.to_string())
				} else {
					actions::reveal_in_explorer(&path).map_err(|e| e.to_string())
				};
				match out {
					Ok(()) => self.set_status(&format!("{}：{path}", menu::command_label(id))),
					Err(e) => self.set_status(&format!("操作失败：{e}")),
				}
			}
			menu::CMD_COPY_PATH | menu::CMD_COPY_NAME | menu::CMD_COPY_DIR => self.act_copy(id),
			menu::CMD_DU_ANALYZE => self.analyze_selection(),
			menu::CMD_DU_UP => self.du_up(),
			menu::CMD_MODE_SEARCH => self.exit_du(),
			menu::CMD_REBUILD => self.rebuild_index(),
			menu::CMD_RESCAN => self.rescan_index(),
			menu::CMD_START_SERVE => self.start_serve(),
			menu::CMD_OPEN_CONFIG_DIR => self.open_config_dir(),
			menu::CMD_CLEAR => self.clear_results(),
			menu::CMD_SEARCH_NOW => self.trigger_search_now(),
			menu::CMD_ABOUT => self.show_about(),
			menu::CMD_EXIT => {
				self.save_config();
				let _ = self.wnd.hwnd().DestroyWindow();
			}
			_ => {}
		}
	}

	fn act_copy(&self, id: u16) {
		let Some((path, _)) = self.target_path() else {
			self.set_status("请先选中一行");
			return;
		};
		let text = match id {
			menu::CMD_COPY_NAME => format::file_name(&path).to_string(),
			menu::CMD_COPY_DIR => format::parent_dir(&path).to_string(),
			_ => path.clone(),
		};
		match actions::copy_to_clipboard(&text) {
			Ok(()) => self.set_status(&format!("已复制：{text}")),
			Err(e) => self.set_status(&format!("复制失败：{e}")),
		}
	}

	/// 全量重扫（比 rebuild 更重：serve 进程内重建，通常需要管理员）。
	fn rescan_index(&self) {
		let answer = self
			.wnd
			.hwnd()
			.MessageBox(
				"全量重扫由 serve 进程执行（通常需要管理员权限），耗时更长。\n\n确定继续？",
				"fer GUI - 全量重扫",
				co::MB::YESNO | co::MB::ICONQUESTION,
			)
			.unwrap_or(co::DLGID::NO);
		if answer != co::DLGID::YES {
			return;
		}
		self.set_status("已请求全量重扫…");
		let tx = self.tx.clone();
		let client = self.client.clone();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let line = match client.rescan() {
				Ok(()) => "全量重扫请求已提交".to_string(),
				Err(e) => format!("全量重扫失败：{e}"),
			};
			let _ = tx.send(TaskMsg::IndexInfo(line));
			wake_pump(hwnd_raw);
			Ok(())
		});
	}

	fn open_config_dir(&self) {
		let dir = config::Config::dir();
		let _ = std::fs::create_dir_all(&dir);
		let _ = std::process::Command::new("explorer").arg(&dir).spawn();
		self.set_status(&format!("已打开配置目录：{}", dir.display()));
	}

	fn show_about(&self) {
		let text = format!(
			"fer GUI —— fer（File Express Retriever）的 Windows 原生前端\n\n\
			 搜索语法（与 fer 一致）：\n\
			   foo              文件名子串\n\
			   *.rs             通配符\n\
			   ext:rs,png       扩展名\n\
			   size:>1mb        大小\n\
			   dm:thisweek      修改时间\n\
			   parent:D:\\proj  该目录子树\n\
			   !hidden:true     取反\n\n\
			 快捷键：Enter 打开/下钻 · Ctrl+C 复制路径 · Ctrl+H 最近查询\n\
			         Backspace 返回上级 · Esc 退出占用视图\n\n\
			 配置：{}\n历史：{}",
			config::Config::path().display(),
			history::history_path().display(),
		);
		let _ = self
			.wnd
			.hwnd()
			.MessageBox(&text, "关于 fer GUI", co::MB::OK | co::MB::ICONINFORMATION);
	}

	/// 重建索引：先两步确认，再 POST /api/rebuild（serve 转交给已提权的 monitor）。
	fn rebuild_index(&self) {
		let answer = self
			.wnd
			.hwnd()
			.MessageBox(
				"重建索引会重扫本卷 $MFT（约 20 秒），期间搜索结果可能短暂不完整。\n\n确定继续？",
				"fer GUI - 重建索引",
				co::MB::YESNO | co::MB::ICONQUESTION,
			)
			.unwrap_or(co::DLGID::NO);
		if answer != co::DLGID::YES {
			return;
		}
		self.set_status("已请求重建索引（后台执行，完成后结果会自动刷新）…");
		let tx = self.tx.clone();
		let client = self.client.clone();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let line = match client.rebuild() {
				Ok(()) => "重建请求已提交（monitor 正在重扫本卷）".to_string(),
				Err(e) => format!("重建失败：{e}"),
			};
			let _ = tx.send(TaskMsg::IndexInfo(line));
			wake_pump(hwnd_raw);
			Ok(())
		});
	}

	/// 一键拉起 fer serve（后台线程；ensure_serve 最长阻塞 8s）。
	fn start_serve(&self) {
		self.set_status("正在启动 fer serve…");
		let tx = self.tx.clone();
		let addr = self.client.addr().to_string();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let st = serve_ctl::ensure_serve(&addr, Duration::from_secs(2), 8000);
			let _ = tx.send(TaskMsg::ServeUp(st));
			wake_pump(hwnd_raw);
			Ok(())
		});
	}

	/// 进入占用视图（Everything 式：由工具栏按钮/右键菜单/视图菜单触发，**不改查询框**）。
	fn enter_du(&self, root: &str) {
		let root = format::normalize_dir(root);
		if root.is_empty() {
			self.set_status("占用分析：请先在结果里选中一个文件夹");
			return;
		}
		let generation = {
			let mut st = self.state.borrow_mut();
			st.du_generation = st.du_generation.wrapping_add(1);
			st.mode = Mode::Du { root: root.clone() };
			st.du_generation
		};
		let _ = self.btn_up.hwnd().EnableWindow(true);
		let _ = self.wnd.hwnd().SetWindowText(&format!("{TITLE_BASE} - 占用分析"));
		self.set_status(&format!("正在分析占用：{root} …"));
		let tx = self.tx.clone();
		let client = self.client.clone();
		let path = root.clone();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let result = client
				.du(&path, Some(1), Some(300), false)
				.map_err(|e| e.to_string());
			let _ = tx.send(TaskMsg::Du { generation, result });
			wake_pump(hwnd_raw);
			Ok(())
		});
	}

	/// 占用视图返回上一级（到根则退出占用视图）。
	fn du_up(&self) {
		let root = match self.state.borrow().mode.clone() {
			Mode::Du { root } => root,
			Mode::Search => return,
		};
		match format::parent_of(&root) {
			Some(parent) => self.enter_du(&parent),
			None => self.exit_du(),
		}
	}

	/// 退出占用视图，回到搜索模式（查询框内容不动）。
	fn exit_du(&self) {
		if matches!(self.state.borrow().mode, Mode::Search) {
			return;
		}
		self.state.borrow_mut().mode = Mode::Search;
		let _ = self.btn_up.hwnd().EnableWindow(false);
		// 即使查询框为空（此时 trigger_search_now 会走 clear_results 提前返回，不经过 render），
		// 也要把标题与列头切回搜索态 —— 否则退出占用视图后界面外观停在占用模式（R3-D1）。
		let _ = self.wnd.hwnd().SetWindowText(TITLE_BASE);
		self.set_column_titles(["名称", "所在目录", "大小", "修改时间", "类型"]);
		self.trigger_search_now();
	}

	/// 对「选中行的目录」做占用分析：选中的是目录就用它，是文件就用它所在目录；无选中则提示。
	fn analyze_selection(&self) {
		let Some(idx) = self.focused_index().or_else(|| {
			let st = self.state.borrow();
			if st.hits.is_empty() { None } else { Some(0) }
		}) else {
			self.set_status("占用分析：请先选中一个文件夹");
			return;
		};
		let (path, is_dir) = {
			let st = self.state.borrow();
			match st.hits.get(idx) {
				Some(h) => (h.path.clone(), h.is_dir),
				None => return,
			}
		};
		if is_dir {
			self.enter_du(&path);
		} else {
			self.enter_du(format::parent_dir(&path));
		}
	}

	/// 渲染目录占用结果（复用同一张表：名称/路径/大小/文件数/占用 —— 第 5 列是分配簇字节，
	/// /api/du 的 children 不提供子目录数）。
	fn render_du(&self, res: &ferapi::DuResult) {
		self.set_column_titles(["名称", "路径", "大小", "文件数", "占用"]);
		self.list.set_redraw(false);
		let _ = self.list.items().delete_all();
		let mut hits: Vec<Hit> = Vec::with_capacity(res.children.len());
		for node in &res.children {
			let name = format::file_name(&node.path).to_string();
			let size = format::human_size(node.size);
			let files = format::human_count(node.files);
			let allocated = format::human_size(node.allocated);
			let _ = self
				.list
				.items()
				.add(&[name, node.path.clone(), size, files, allocated], None, hits.len());
			hits.push(Hit {
				path: node.path.clone(),
				is_dir: true,
				size: node.size,
				allocated: node.allocated,
				mtime: 0,
				ctime: 0,
				flags: 0,
			});
		}
		self.list.set_redraw(true);
		self.state.borrow_mut().hits = hits;
		self.set_status(&format!(
			"占用 {} ［{}］ —— 合计 {}（分配 {}）· {} 文件 · {} 目录{}",
			breadcrumb(&res.root),
			res.root,
			format::human_size(res.total_bytes),
			format::human_size(res.total_allocated),
			format::human_count(res.files),
			format::human_count(res.dirs),
			if res.truncated { " · 已截断" } else { "" },
		));
	}

	fn clear_results(&self) {
		let _ = self.list.items().delete_all();
		self.state.borrow_mut().hits.clear();
		self.set_status("就绪");
	}

	fn start_timers(&self) {
		// 泵按「空闲低频 + 按需加速」运行：空闲 5Hz 兜底，输入防抖期间 40ms，
		// 后台回包时 wake_pump 拉到 1ms 立即处理。常驻 25Hz 空泵实测占 0.94% 单核。
		let _ = self.wnd.hwnd().SetTimer(TIMER_PUMP, TIMER_IDLE_MS, None);
		let _ = self.wnd.hwnd().SetTimer(TIMER_STATS, TIMER_STATS_MS, None);
	}

	/// 窗口创建并进入消息循环后的首次初始化（此时 hwnd 有效，spawn_thread 的回包才送得到）。
	fn bootstrap(&self) {
		// 恢复上次窗口位置与尺寸（存的是物理像素，直接喂 SetWindowPos）。
		let cfg = self.state.borrow().cfg.clone();
		if cfg.win_x != i32::MIN && cfg.win_w >= 400 && cfg.win_h >= 300 {
			// 显示器布局可能已变（拔掉副屏/改分辨率），旧坐标可能整窗落在屏幕外。
			let (px, py) = clamp_window_pos(cfg.win_x, cfg.win_y, cfg.win_w, cfg.win_h);
			let _ = self.wnd.hwnd().SetWindowPos(
				w::HwndPlace::None,
				w::POINT { x: px, y: py },
				w::SIZE { cx: cfg.win_w, cy: cfg.win_h },
				co::SWP::NOZORDER | co::SWP::NOACTIVATE,
			);
		}
		let _ = self.btn_up.hwnd().EnableWindow(false);
		self.refresh_index_info();
		self.update_sort_arrow();
		self.layout_columns();
		let initial = self.state.borrow().cfg.last_query.clone();
		if !initial.is_empty() {
			let _ = self.txt_query.set_text(&initial);
			self.on_query_changed();
		}
		self.sync_chip_states();
	}

	/// WM_TIMER：消费后台消息 + 防抖到期发起搜索。
	fn pump(&self) {
		let first = {
			let mut st = self.state.borrow_mut();
			if st.bootstrapped {
				false
			} else {
				st.bootstrapped = true;
				true
			}
		};
		if first {
			self.bootstrap();
		}

		let now = query::now_ms();
		let due = {
			let mut st = self.state.borrow_mut();
			if st.pending_since_ms != 0 && now.saturating_sub(st.pending_since_ms) >= DEBOUNCE_MS {
				let q = std::mem::take(&mut st.pending_query);
				st.pending_since_ms = 0;
				Some(q)
			} else {
				None
			}
		};
		if let Some(q) = due {
			let q = q.trim().to_string();
			self.start_search(&q);
		}

		loop {
			let msg = self.rx.borrow().try_recv();
			match msg {
				Ok(m) => self.handle(m),
				Err(_) => break,
			}
		}

		// 按当前是否有防抖待办决定下一拍频率；SetTimer 对已存在的 id 会重置间隔。
		let interval = if self.state.borrow().pending_since_ms != 0 {
			TIMER_PUMP_MS
		} else {
			TIMER_IDLE_MS
		};
		let _ = self.wnd.hwnd().SetTimer(TIMER_PUMP, interval, None);
	}

	fn handle(&self, msg: TaskMsg) {
		match msg {
			TaskMsg::Search { generation, result } => {
				if !self.state.borrow().search.accept(generation) {
					return; // 过期响应：输入已变化，丢弃
				}
				match result {
					Ok(res) => self.render(&res),
					Err(e) => self.set_status(&format!("请求失败：{e}（fer serve 是否在运行？）")),
				}
			}
			TaskMsg::Du { generation, result } => {
				if generation != self.state.borrow().du_generation {
					return; // 过期占用响应（用户已切换目录）
				}
				match result {
					Ok(res) => self.render_du(&res),
					Err(e) => self.set_status(&format!("占用查询失败：{e}")),
				}
			}
			TaskMsg::ServeUp(st) => {
				self.set_status(&st.detail);
				self.refresh_index_info();
			}
			TaskMsg::IndexInfo(line) => {
				self.state.borrow_mut().index_line = line.clone();
				// serve 在线时「启动服务」无意义，置灰避免重复拉起。
				let online = !line.starts_with("serve 未连接");
				let _ = self.btn_serve.hwnd().EnableWindow(!online);
				let _ = self.lbl_index.set_text_and_resize(&line);
			}
		}
	}

	/// 切换表头列标题（搜索模式 / du 模式语义不同）。
	fn set_column_titles(&self, titles: [&str; 5]) {
		if let Ok(cols) = self.list.cols().iter() {
			for (i, col) in cols.enumerate() {
				if let Some(t) = titles.get(i) {
					let _ = col.set_title(t);
				}
			}
		}
	}

	/// 按「文件类型」缓存图标序号；同一扩展名只取一次。
	fn icon_for(&self, key: &str, path: &str, is_dir: bool) -> i32 {
		if let Some(v) = self.icons.borrow().get(key) {
			return *v;
		}
		let idx = icon_index_for(self.icon_list, path, is_dir);
		self.icons.borrow_mut().insert(key.to_string(), idx);
		idx
	}

	/// 让「所在目录」列吃掉除固定列以外的全部宽度，避免右侧留下一条空白带。
	fn layout_columns(&self) {
		let Ok(rc) = self.list.hwnd().GetClientRect() else {
			return;
		};
		let total_w = rc.right - rc.left;
		// 名称 / 大小 / 修改时间 / 类型 固定；-24 给垂直滚动条留位。
		let fixed = gui::dpi_x(300) + gui::dpi_x(80) + gui::dpi_x(130) + gui::dpi_x(60) + 24;
		let dir_w = (total_w - fixed).max(gui::dpi_x(180));
		if let Ok(cols) = self.list.cols().iter() {
			let v: Vec<_> = cols.collect();
			if let Some(col) = v.get(1) {
				let _ = col.set_width(dir_w);
			}
		}
	}

	fn render(&self, res: &SearchResult) {
		self.layout_columns();
		let _ = self.wnd.hwnd().SetWindowText(TITLE_BASE);
		self.set_column_titles(["名称", "所在目录", "大小", "修改时间", "类型"]);
		self.list.set_redraw(false);
		let _ = self.list.items().delete_all();
		let mut shown = 0usize;
		for hit in &res.hits {
			let name = format::file_name(&hit.path).to_string();
			// 名称已含路径最后一段；这里只放所在目录，避免两列重复且都被截断。
			let dir = format::parent_of(&hit.path).unwrap_or_default();
			let size = if hit.is_dir { String::new() } else { format::human_size(hit.size) };
			let mtime = format::format_time(hit.mtime);
			let kind = if hit.is_dir {
				"文件夹".to_string()
			} else {
				std::path::Path::new(&hit.path)
					.extension()
					.and_then(|e| e.to_str())
					.map(|e| e.to_ascii_lowercase())
					.unwrap_or_default()
			};
			let icon = self.icon_for(&kind, &hit.path, hit.is_dir);
			let icon_opt = if icon >= 0 { Some(icon as u32) } else { None };
			if self.list.items().add(&[name, dir, size, mtime, kind], icon_opt, shown).is_ok() {
				shown += 1;
			}
		}
		self.list.set_redraw(true);
		self.state.borrow_mut().hits = res.hits.clone();

		let status = match &res.error {
			Some(err) => format!("查询出错：{err}"),
			None => format!(
				"命中 {} 条（显示 {}）· 引擎 {} ms",
				format::human_count(res.total),
				shown,
				res.took_ms,
			),
		};
		self.set_status(&status);
		self.update_sort_arrow();
	}

	fn set_status(&self, text: &str) {
		let _ = self.lbl_status.set_text_and_resize(text);
	}

	// ---------------- 行交互 ----------------

	/// 当前聚焦行号（行负载即结果序号）。
	fn focused_index(&self) -> Option<usize> {
		let item = self.list.items().focused()?;
		let data = item.data();
		let idx = *data.borrow();
		Some(idx)
	}

	/// Ctrl+C：复制聚焦行的完整路径。
	fn copy_focused_path(&self) {
		let Some(idx) = self.focused_index() else {
			return;
		};
		let Some(path) = self.selected_path(idx) else {
			return;
		};
		match actions::copy_to_clipboard(&path) {
			Ok(()) => self.set_status(&format!("已复制路径：{path}")),
			Err(e) => self.set_status(&format!("复制失败：{e}")),
		}
	}

	fn selected_path(&self, index: usize) -> Option<String> {
		let st = self.state.borrow();
		st.hits.get(index).map(|h| h.path.clone())
	}

	fn on_item_activate(&self, index: usize) {
		let Some(path) = self.selected_path(index) else {
			return;
		};
		if matches!(self.state.borrow().mode, Mode::Du { .. }) {
			// 下钻该子目录（不再把子命令写回查询框）。
			self.enter_du(&path);
			return;
		}
		if let Err(e) = actions::reveal_in_explorer(&path) {
			self.set_status(&format!("定位失败：{e}"));
		}
	}

	/// 视图行号 -> 结果序号：用行负载（lParam）反查，避免 add 失败导致行号与 hits 错位。
	fn row_at(&self, view_index: i32) -> Option<usize> {
		if view_index < 0 {
			return None;
		}
		let item = self.list.items().iter().nth(view_index as usize)?;
		let data = item.data();
		let idx = *data.borrow();
		Some(idx)
	}

	/// 右键：记录目标行、选中它，然后弹出上下文菜单。
	fn on_context_menu(&self, view_index: i32, pt: w::POINT) {
		let Some(row) = self.row_at(view_index) else {
			return;
		};
		self.state.borrow_mut().context_row = Some(row);
		if let Some(item) = self.list.items().iter().nth(view_index as usize) {
			let _ = item.select(true);
			_ = item.focus();
		}
		self.show_context_menu(pt);
	}

	/// 弹出「最近查询」菜单（动态项；用 TPM::RETURNCMD 拿回选中 ID）。
	fn show_history_menu(&self) {
		let history = self.state.borrow().history.items().to_vec();
		if history.is_empty() {
			self.set_status("暂无搜索历史");
			return;
		}
		let Ok(mut menu) = w::HMENU::CreatePopupMenu() else {
			return;
		};
		for (i, q) in history.iter().enumerate() {
			let label = if q.chars().count() > 60 {
				format!("{}…", q.chars().take(60).collect::<String>())
			} else {
				q.clone()
			};
			let _ = menu.AppendMenu(
				co::MF::STRING,
				w::IdMenu::Id(MENU_HISTORY_BASE + i as u16),
				w::BmpPtrStr::Str(w::WString::from_str(&label)),
			);
		}
		let _ = menu.AppendMenu(co::MF::SEPARATOR, w::IdMenu::None, w::BmpPtrStr::None);
		let _ = menu.AppendMenu(
			co::MF::STRING,
			w::IdMenu::Id(MENU_HISTORY_CLEAR),
			w::BmpPtrStr::Str(w::WString::from_str("清除历史")),
		);
		if let Ok(pos) = w::GetCursorPos()
			&& let Ok(Some(cmd)) = menu.TrackPopupMenu(
				co::TPM::RETURNCMD | co::TPM::LEFTBUTTON,
				pos,
				self.wnd.hwnd(),
			)
		{
			if cmd == MENU_HISTORY_CLEAR as i32 {
				{
					let mut st = self.state.borrow_mut();
					st.history.clear();
					let _ = st.history.save();
				}
				self.set_status("搜索历史已清除");
			} else if cmd >= MENU_HISTORY_BASE as i32 {
				let idx = (cmd - MENU_HISTORY_BASE as i32) as usize;
				if let Some(q) = history.get(idx) {
					let _ = self.txt_query.set_text(q);
					self.trigger_search_now();
				}
			}
		}
		let _ = menu.DestroyMenu();
	}

	fn show_context_menu(&self, pt: w::POINT) {
		let Ok(mut menu) = w::HMENU::CreatePopupMenu() else {
			return;
		};
		let items: [(u16, &str); 6] = [
			(menu::CMD_OPEN, "打开"),
			(menu::CMD_REVEAL, "在资源管理器中定位"),
			(menu::CMD_DU_ANALYZE, "分析此目录占用"),
			(menu::CMD_COPY_PATH, "复制完整路径"),
			(menu::CMD_COPY_NAME, "复制文件名"),
			(menu::CMD_COPY_DIR, "复制所在目录"),
		];
		for (i, (id, label)) in items.iter().enumerate() {
			if i == 2 {
				let _ = menu.AppendMenu(co::MF::SEPARATOR, w::IdMenu::None, w::BmpPtrStr::None);
			}
			let _ = menu.AppendMenu(
				co::MF::STRING,
				w::IdMenu::Id(*id),
				w::BmpPtrStr::Str(w::WString::from_str(label)),
			);
		}
		let _ = menu.track_popup_menu_at_point(pt, self.wnd.hwnd(), self.list.hwnd());
		let _ = menu.DestroyMenu();
	}

	fn on_column_click(&self, index: usize) {
		let Some(key) = SortKey::from_column(index) else {
			return;
		};
		{
			let mut st = self.state.borrow_mut();
			st.search.cycle_sort(key);
		}
		self.update_sort_arrow();
		self.trigger_search_now();
	}

	/// 在表头显示当前排序方向（缺它用户不知道按什么排、是升还是降）。
	fn update_sort_arrow(&self) {
		let (sort, desc) = self.state.borrow().search.sort();
		let active = match sort {
			SortKey::Name => Some(0usize),
			SortKey::Path => Some(1),
			SortKey::Size => Some(2),
			SortKey::Mtime => Some(3),
			_ => None,
		};
		let Some(header) = self.list.header() else {
			return;
		};
		let Ok(items) = header.items().iter() else {
			return;
		};
		for (i, item) in items.enumerate() {
			let arrow = if Some(i) == active {
				if desc {
					gui::HeaderArrow::Desc
				} else {
					gui::HeaderArrow::Asc
				}
			} else {
				gui::HeaderArrow::None
			};
			let _ = item.set_arrow(arrow);
		}
	}

	// ---------------- 索引状态 ----------------

	fn refresh_index_info(&self) {
		let tx = self.tx.clone();
		let client = self.client.clone();
		let hwnd_raw = self.wnd.hwnd().ptr() as isize;
		self.wnd.spawn_thread(move || {
			let line = match client.stats() {
				Ok(s) => {
					let mut out = format!(
						"索引 {} 条 · {} MB",
						format::human_count(s.entries),
						format::human_count(s.dump_mb),
					);
					if let Ok(f) = client.feed() {
						out.push_str(&format!(
							" · 实时 +{}{}",
							format::human_count(f.delta_entries),
							if f.saturated { " · 已饱和" } else { "" },
						));
					}
					out.push_str(&format!(" · {}", client.addr()));
					out
				}
				Err(_) => format!("serve 未连接（{}）", client.addr()),
			};
			let _ = tx.send(TaskMsg::IndexInfo(line));
			wake_pump(hwnd_raw);
			Ok(())
		});
	}
}
