//! 菜单栏定义与构建（Everything 风格：功能全部从菜单/工具栏/右键进入，搜索框只做搜索）。
//!
//! 契约（Lead 冻结）：
//!   pub const CMD_*: u16（16 个命令）+ pub const ALL_COMMANDS: [u16; 16]
//!   pub fn build_menu_bar() -> winsafe::SysResult<winsafe::HMENU>
//!   pub fn command_label(id: u16) -> &'static str
//!
//! 菜单结构（助记符用 &）：
//!   文件(&F)：打开 / 在资源管理器中定位 / --- / 退出
//!   编辑(&E)：复制完整路径 / 复制文件名 / 复制所在目录 / --- / 清空结果
//!   视图(&V)：搜索模式 / 占用分析（当前目录）/ 返回上一级
//!   搜索(&S)：立即搜索 / --- / 重建索引 / 全量重扫 / 启动 fer serve
//!   工具(&T)：打开配置目录 / --- / 关于
//!
//! 说明（与任务描述的差异，已同步 Lead）：冻结的命令 id 恰为 16 个，其中**没有** CMD_HELP，
//! 因此「帮助」不单列菜单，用法说明并入 CMD_ABOUT（关于对话框/状态栏文案）；若确需独立的
//! 「用法说明」菜单项，需要新增第 17 个 id 并同步修改 ALL_COMMANDS 的长度（契约变更）。
//!
//! 句柄所有权：winsafe 的 HMENU **没有实现 Drop**，本模块不做自动销毁。
//!   - 成功：返回的 HMENU 交给 main.rs 的 WindowMainOpts.menu 持有，窗口销毁时由系统一并销毁；
//!   - 失败：已挂到菜单栏上的子菜单随 DestroyMenu 一并销毁（仅极端情况下最后那个未挂载的
//!     popup 会泄漏一个 HMENU，此时启动即将失败退出，不额外引入清理状态机）。

use winsafe::{co, BmpPtrStr, HMENU, IdMenu, SysResult, WString};

// ---------------- 命令 ID ----------------
// 取值 2001..=2016：与 main.rs 的结果表右键菜单（101..106）和历史菜单（1000..1999）
// 完全不相交，避免 WM_COMMAND 分发串号。

/// 打开选中项。
pub const CMD_OPEN: u16 = 2001;
/// 在资源管理器中定位选中项。
pub const CMD_REVEAL: u16 = 2002;
/// 复制完整路径。
pub const CMD_COPY_PATH: u16 = 2003;
/// 复制文件名。
pub const CMD_COPY_NAME: u16 = 2004;
/// 复制所在目录。
pub const CMD_COPY_DIR: u16 = 2005;
/// 对选中目录做占用分析。
pub const CMD_DU_ANALYZE: u16 = 2006;
/// 占用视图：返回上一级。
pub const CMD_DU_UP: u16 = 2007;
/// 回到搜索模式。
pub const CMD_MODE_SEARCH: u16 = 2008;
/// 重建索引（POST /api/rebuild）。
pub const CMD_REBUILD: u16 = 2009;
/// 全量重扫（POST /api/rescan）。
pub const CMD_RESCAN: u16 = 2010;
/// 启动 fer serve。
pub const CMD_START_SERVE: u16 = 2011;
/// 打开配置目录。
pub const CMD_OPEN_CONFIG_DIR: u16 = 2012;
/// 清空结果。
pub const CMD_CLEAR: u16 = 2013;
/// 立即搜索。
pub const CMD_SEARCH_NOW: u16 = 2014;
/// 关于（含用法说明入口）。
pub const CMD_ABOUT: u16 = 2015;
/// 退出。
pub const CMD_EXIT: u16 = 2016;

/// 全部菜单命令，顺序与 id 升序一致（唯一性与「菜单覆盖完整性」由单测保证）。
pub const ALL_COMMANDS: [u16; 16] = [
	CMD_OPEN,
	CMD_REVEAL,
	CMD_COPY_PATH,
	CMD_COPY_NAME,
	CMD_COPY_DIR,
	CMD_DU_ANALYZE,
	CMD_DU_UP,
	CMD_MODE_SEARCH,
	CMD_REBUILD,
	CMD_RESCAN,
	CMD_START_SERVE,
	CMD_OPEN_CONFIG_DIR,
	CMD_CLEAR,
	CMD_SEARCH_NOW,
	CMD_ABOUT,
	CMD_EXIT,
];

/// 命令的中文短标签（状态栏提示、日志用；不带助记符 &）。
///
/// 未知 id 返回「未知命令」而不 panic —— main.rs 收到非预期 id 时也能安全显示。
pub fn command_label(id: u16) -> &'static str {
	match id {
		CMD_OPEN => "打开",
		CMD_REVEAL => "在资源管理器中定位",
		CMD_COPY_PATH => "复制完整路径",
		CMD_COPY_NAME => "复制文件名",
		CMD_COPY_DIR => "复制所在目录",
		CMD_DU_ANALYZE => "占用分析",
		CMD_DU_UP => "返回上一级",
		CMD_MODE_SEARCH => "搜索模式",
		CMD_REBUILD => "重建索引",
		CMD_RESCAN => "全量重扫",
		CMD_START_SERVE => "启动 fer serve",
		CMD_OPEN_CONFIG_DIR => "打开配置目录",
		CMD_CLEAR => "清空结果",
		CMD_SEARCH_NOW => "立即搜索",
		CMD_ABOUT => "关于",
		CMD_EXIT => "退出",
		_ => "未知命令",
	}
}

// ---------------- 菜单结构表 ----------------

/// 一个菜单项：命令项（文字 + id）或分隔线。
pub enum ItemDef {
	/// 可点击命令项：(显示文字, 命令 id)。
	Cmd(&'static str, u16),
	/// 分隔线。
	Sep,
}

/// 文件(&F)。
const FILE_ITEMS: &[ItemDef] = &[
	ItemDef::Cmd("打开(&O)", CMD_OPEN),
	ItemDef::Cmd("在资源管理器中定位(&R)", CMD_REVEAL),
	ItemDef::Sep,
	ItemDef::Cmd("退出(&X)", CMD_EXIT),
];

/// 编辑(&E)。
const EDIT_ITEMS: &[ItemDef] = &[
	ItemDef::Cmd("复制完整路径(&C)", CMD_COPY_PATH),
	ItemDef::Cmd("复制文件名(&N)", CMD_COPY_NAME),
	ItemDef::Cmd("复制所在目录(&D)", CMD_COPY_DIR),
	ItemDef::Sep,
	ItemDef::Cmd("清空结果(&L)", CMD_CLEAR),
];

/// 视图(&V)。
const VIEW_ITEMS: &[ItemDef] = &[
	ItemDef::Cmd("搜索模式(&M)", CMD_MODE_SEARCH),
	ItemDef::Cmd("占用分析（当前目录）(&A)", CMD_DU_ANALYZE),
	ItemDef::Cmd("返回上一级(&U)", CMD_DU_UP),
];

/// 搜索(&S)。
const SEARCH_ITEMS: &[ItemDef] = &[
	ItemDef::Cmd("立即搜索(&G)", CMD_SEARCH_NOW),
	ItemDef::Sep,
	ItemDef::Cmd("重建索引(&B)", CMD_REBUILD),
	ItemDef::Cmd("全量重扫(&R)", CMD_RESCAN),
	ItemDef::Cmd("启动 fer serve(&V)", CMD_START_SERVE),
];

/// 工具(&T)。
const TOOLS_ITEMS: &[ItemDef] = &[
	ItemDef::Cmd("打开配置目录(&O)", CMD_OPEN_CONFIG_DIR),
	ItemDef::Sep,
	ItemDef::Cmd("关于(&A)", CMD_ABOUT),
];

/// 菜单栏：(标题, 下拉项)。
pub const MENUS: [(&str, &[ItemDef]); 5] = [
	("文件(&F)", FILE_ITEMS),
	("编辑(&E)", EDIT_ITEMS),
	("视图(&V)", VIEW_ITEMS),
	("搜索(&S)", SEARCH_ITEMS),
	("工具(&T)", TOOLS_ITEMS),
];

/// 按序号构建单个下拉菜单（自绘菜单栏用：按钮点击时现建现弹）。
pub fn build_popup_at(index: usize) -> SysResult<HMENU> {
	let (_, items) = MENUS[index];
	build_popup(items)
}

// ---------------- 构建 ----------------

/// 构建菜单栏并返回其句柄（交由 main.rs 的 WindowMainOpts.menu 持有）。
///
/// 任何一步失败都返回 Err，并尽量销毁已建好的部分菜单；不 panic。
/// 构建完整菜单栏（自绘菜单栏后不再使用，保留给未来的系统菜单栏方案）。
#[allow(dead_code)]
pub fn build_menu_bar() -> SysResult<HMENU> {
	validate_tables()?;
	let bar = HMENU::CreateMenu()?;
	for (title, items) in MENUS {
		let popup = match build_popup(items) {
			Ok(popup) => popup,
			Err(err) => return Err(destroy_bar(bar, err)),
		};
		let appended = bar.AppendMenu(
			co::MF::POPUP,
			IdMenu::Menu(&popup),
			BmpPtrStr::Str(WString::from_str(title)),
		);
		if let Err(err) = appended {
			// 这个 popup 尚未挂到父菜单上，必须显式销毁，否则句柄泄漏。
			let mut orphan = popup;
			let _ = orphan.DestroyMenu();
			return Err(destroy_bar(bar, err));
		}
	}
	Ok(bar)
}

/// 构建前自检：菜单结构表里出现的每个命令 id 都必须在冻结清单 ALL_COMMANDS 内。
///
/// 让「清单唯一事实源」在运行期也生效：以后往菜单表里加项时若写错 id，启动即失败，
/// 而不是让 WM_COMMAND 收到一个没人处理的未知命令。
#[allow(dead_code)]
fn validate_tables() -> SysResult<()> {
	for (_, items) in MENUS {
		for item in items {
			if let ItemDef::Cmd(_, id) = item
				&& !ALL_COMMANDS.contains(id)
			{
				return Err(co::ERROR::BAD_ARGUMENTS);
			}
		}
	}
	Ok(())
}

/// 构建一个下拉菜单；失败时销毁半成品并返回错误。
fn build_popup(items: &[ItemDef]) -> SysResult<HMENU> {
	let popup = HMENU::CreatePopupMenu()?;
	for item in items {
		let appended = match item {
			ItemDef::Cmd(text, id) => {
				// owner-draw：itemData 持有文本指针（WString 故意泄漏，进程生命周期）。
				let ws: Box<WString> = Box::new(WString::from_str(text));
				let ptr = Box::into_raw(ws) as *const std::ffi::c_void;
				popup.AppendMenu(co::MF::OWNERDRAW, IdMenu::Id(*id), BmpPtrStr::Ptr(ptr))
			}
			ItemDef::Sep => popup.AppendMenu(co::MF::SEPARATOR, IdMenu::None, BmpPtrStr::None),
		};
		if let Err(err) = appended {
			let mut orphan = popup;
			let _ = orphan.DestroyMenu();
			return Err(err);
		}
	}
	Ok(popup)
}

/// 销毁已建好的菜单栏（已挂载的子菜单随之销毁），原样返回错误。
#[allow(dead_code)]
fn destroy_bar(mut bar: HMENU, err: co::ERROR) -> co::ERROR {
	let _ = bar.DestroyMenu();
	err
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 全部 CMD_* 常量（与 ALL_COMMANDS 一一对应，供覆盖性测试使用）。
	const ALL_CONSTANTS: [u16; 16] = [
		CMD_OPEN,
		CMD_REVEAL,
		CMD_COPY_PATH,
		CMD_COPY_NAME,
		CMD_COPY_DIR,
		CMD_DU_ANALYZE,
		CMD_DU_UP,
		CMD_MODE_SEARCH,
		CMD_REBUILD,
		CMD_RESCAN,
		CMD_START_SERVE,
		CMD_OPEN_CONFIG_DIR,
		CMD_CLEAR,
		CMD_SEARCH_NOW,
		CMD_ABOUT,
		CMD_EXIT,
	];

	/// 从菜单结构表里收集所有命令项 id（顺序即菜单出现顺序）。
	fn menu_command_ids() -> Vec<u16> {
		let mut ids = Vec::new();
		for (_, items) in MENUS {
			for item in items {
				if let ItemDef::Cmd(_, id) = item {
					ids.push(*id);
				}
			}
		}
		ids
	}

	#[test]
	fn all_commands_has_no_duplicates() {
		for (i, a) in ALL_COMMANDS.iter().enumerate() {
			for b in ALL_COMMANDS.iter().skip(i + 1) {
				assert_ne!(a, b, "命令 id 重复：{a}");
			}
		}
	}

	#[test]
	fn all_commands_matches_the_frozen_constants() {
		let mut expected = ALL_CONSTANTS;
		expected.sort_unstable();
		let mut actual = ALL_COMMANDS;
		actual.sort_unstable();
		assert_eq!(actual, expected, "ALL_COMMANDS 必须与 16 个 CMD_* 常量一一对应");
	}

	#[test]
	fn command_ids_live_in_a_block_disjoint_from_context_menus() {
		// main.rs 的右键菜单 101..106、历史菜单 1000..1999 必须与本模块完全不重叠。
		const BASE: u16 = 2001;
		for (i, id) in ALL_COMMANDS.iter().enumerate() {
			assert_eq!(*id, BASE + i as u16, "命令 id 应为 2001 起的连续块");
			assert!(!(101..=106).contains(id), "与右键菜单 id 冲突：{id}");
			assert!(!(1000..=1999).contains(id), "与历史菜单 id 冲突：{id}");
		}
	}

	#[test]
	fn every_command_has_a_distinct_nonempty_label() {
		for (i, id) in ALL_COMMANDS.iter().enumerate() {
			let label = command_label(*id);
			assert!(!label.is_empty(), "命令 {id} 缺少标签");
			assert_ne!(label, command_label(u16::MAX), "命令 {id} 落到了兜底标签");
			for other in ALL_COMMANDS.iter().skip(i + 1) {
				assert_ne!(label, command_label(*other), "命令 {id} 与 {other} 标签重复");
			}
		}
		assert_eq!(command_label(u16::MAX), "未知命令", "未知 id 必须走兜底而不 panic");
	}

	#[test]
	fn menu_bar_covers_every_command_exactly_once() {
		let mut from_menu = menu_command_ids();
		assert_eq!(from_menu.len(), ALL_COMMANDS.len(), "菜单项数量与命令数不一致");
		from_menu.sort_unstable();
		let mut expected = ALL_COMMANDS;
		expected.sort_unstable();
		assert_eq!(from_menu, expected, "每个命令都必须且只能出现在菜单里一次");
	}

	#[test]
	fn top_level_titles_carry_mnemonics() {
		for (title, items) in MENUS {
			assert!(title.ends_with(')'), "顶级菜单标题缺少助记符：{title}");
			assert!(title.contains("(&"), "顶级菜单标题缺少助记符：{title}");
			assert!(!items.is_empty(), "顶级菜单 {title} 没有下拉项");
		}
	}

	#[test]
	fn separators_are_never_leading_trailing_or_adjacent() {
		for (title, items) in MENUS {
			assert!(!matches!(items.first(), Some(ItemDef::Sep)), "{title} 以分隔线开头");
			assert!(!matches!(items.last(), Some(ItemDef::Sep)), "{title} 以分隔线结尾");
			for pair in items.windows(2) {
				let both_sep = matches!(pair[0], ItemDef::Sep) && matches!(pair[1], ItemDef::Sep);
				assert!(!both_sep, "{title} 出现连续分隔线");
			}
		}
	}

	#[test]
	fn command_items_all_carry_mnemonics() {
		for (title, items) in MENUS {
			for item in items {
				if let ItemDef::Cmd(text, id) = item {
					assert!(text.contains("(&"), "{title} 的「{text}」缺少助记符");
					assert!(
						ALL_COMMANDS.contains(id),
						"{title} 的「{text}」使用了不在 ALL_COMMANDS 里的 id {id}"
					);
				}
			}
		}
	}

	#[test]
	#[ignore = "会真的创建菜单资源（user32），手动跑：cargo test -- --ignored"]
	fn build_menu_bar_creates_a_real_menu() {
		let mut bar = build_menu_bar().expect("构建菜单栏失败");
		let _ = bar.DestroyMenu();
	}
}
