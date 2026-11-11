//! src/query.rs 状态机的集成测试（独立测试 crate）。
//!
//! 本项目是 bin crate，外部无法 `use fer_gui::query`，因此用 `#[path]` 把被测模块直接编进本
//! 测试 crate：验证的是「作为独立编译单元」暴露出来的公开接口行为，与 src/query.rs 内的
//! `#[cfg(test)] mod tests` 互为补充（同一份模块代码会被两个测试二进制各编一次，属预期）。
//!
//! 契约来源：docs/INTERFACES.md 的 src/query.rs 一节（冻结）。

#[path = "../src/bin/fer-gui/query.rs"]
mod query;

use query::{SearchRequest, SearchState, SortKey, sort_from_column, sort_label};

/// 契约里的全部排序键。
const ALL_KEYS: [SortKey; 6] =
	[SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Allocated, SortKey::Mtime, SortKey::Ctime];

#[test]
fn generation_is_monotonic_and_stale_responses_are_rejected() {
	let mut st = SearchState::new(20);
	assert_eq!(st.generation(), 0, "尚未搜索时 0 表示空代际");

	let first = st.begin("alpha");
	let second = st.begin("beta");
	assert_eq!(first.generation, 1);
	assert_eq!(second.generation, 2);
	assert_eq!(st.generation(), 2);

	// 旧响应晚到：必须被拒，否则结果表会被上一个查询覆盖。
	assert!(!st.accept(first.generation), "过期代际必须被拒");
	assert!(st.accept(second.generation), "当前代际必须被接受");
	assert!(!st.accept(0), "0 不是任何真实请求的代际");
	assert!(!st.accept(3), "未来代际（乱序回调）也必须被拒");

	// 第三次搜索后，第二代同样立即失效。
	let third = st.begin("gamma");
	assert!(!st.accept(second.generation));
	assert!(st.accept(third.generation));
	assert_eq!(third.generation, 3);
}

#[test]
fn accept_before_first_begin_rejects_everything() {
	let st = SearchState::new(20);
	for generation in [0_u64, 1, 2, u64::MAX] {
		assert!(!st.accept(generation), "未发起搜索时不应接受代际 {generation}");
	}
}

#[test]
fn cycle_sort_contract_tri_state_for_every_column() {
	// Name 本身就是默认列：默认态即 (Name, false)，第一次点击进入降序，第二次回到默认。
	let mut st = SearchState::new(20);
	assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, true), "默认列第一次点击进降序");
	assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, false), "第二次点击回到默认升序");
	assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, true), "回到默认后重新开始循环");

	// 其它列：升 -> 降 -> 回 Name 默认（第三态不在本列停住）。
	for key in ALL_KEYS.into_iter().filter(|k| *k != SortKey::Name) {
		let mut st = SearchState::new(20);
		assert_eq!(st.cycle_sort(key), (key, false), "第一次点击 {key:?} 应为升序");
		assert_eq!(st.cycle_sort(key), (key, true), "第二次点击 {key:?} 应为降序");
		assert_eq!(st.cycle_sort(key), (SortKey::Name, false), "第三次点击 {key:?} 应回到默认");
		assert_eq!(st.sort(), (SortKey::Name, false));
		// 回到默认后再次点击该列，重新是升序。
		assert_eq!(st.cycle_sort(key), (key, false), "回到默认后 {key:?} 应重新升序");
	}
}

#[test]
fn cycle_sort_switch_column_always_starts_ascending() {
	let mut st = SearchState::new(20);
	st.set_sort(SortKey::Size, true);
	assert_eq!(st.cycle_sort(SortKey::Mtime), (SortKey::Mtime, false));
	st.set_sort(SortKey::Allocated, true);
	assert_eq!(st.cycle_sort(SortKey::Ctime), (SortKey::Ctime, false));
}

#[test]
fn limit_is_clamped_into_contract_range() {
	// 低于下限：钳到 1。
	let below = 0_usize;
	let mut st = SearchState::new(below);
	assert_eq!(st.limit(), 1, "构造时下限应钳制到 1（输入 {below}）");
	st.set_limit(below);
	assert_eq!(st.limit(), 1, "set_limit 下限应钳制到 1（输入 {below}）");
	// 高于上限：钳到 100000。
	for above in [100_001_usize, 1_000_000, usize::MAX] {
		let mut st = SearchState::new(above);
		assert_eq!(st.limit(), 100_000, "构造时上限应钳制到 100000（输入 {above}）");
		st.set_limit(above);
		assert_eq!(st.limit(), 100_000, "set_limit 上限应钳制到 100000（输入 {above}）");
	}
	for good in [1_usize, 2, 500, 100_000] {
		let mut st = SearchState::new(good);
		assert_eq!(st.limit(), good);
		st.set_limit(good);
		assert_eq!(st.limit(), good);
	}
}

#[test]
fn sort_keys_match_frozen_server_vocabulary() {
	let expected =
		[(SortKey::Name, "name"), (SortKey::Path, "path"), (SortKey::Size, "size"),
		 (SortKey::Allocated, "allocated"), (SortKey::Mtime, "mtime"), (SortKey::Ctime, "ctime")];
	for (key, api) in expected {
		assert_eq!(key.as_str(), api);
		assert_eq!(SortKey::parse(api), Some(key));
		assert!(!sort_label(key).is_empty(), "{key:?} 缺少中文列名");
	}
	// 库侧 parse 大小写不敏感（与服务端一致）；写端始终发小写。
	assert_eq!(SortKey::parse("NAME"), Some(SortKey::Name));
	for unknown in ["", "modified", "asc", "0"] {
		assert_eq!(SortKey::parse(unknown), None, "未知取值 {unknown} 不应被识别");
	}
}

#[test]
fn from_column_covers_the_four_sortable_columns() {
	assert_eq!(sort_from_column(0), Some(SortKey::Name));
	assert_eq!(sort_from_column(1), Some(SortKey::Path));
	assert_eq!(sort_from_column(2), Some(SortKey::Size));
	assert_eq!(sort_from_column(3), Some(SortKey::Mtime));
	for index in [4_usize, 5, 100, usize::MAX] {
		assert_eq!(sort_from_column(index), None, "第 {index} 列不应可排序");
	}
}

#[test]
fn begin_snapshot_survives_state_mutation() {
	let mut st = SearchState::new(30);
	st.set_sort(SortKey::Size, true);
	let snapshot = st.begin("深层嵌套");
	let expected =
		SearchRequest { generation: 1, query: "深层嵌套".to_string(), limit: 30, sort: SortKey::Size, desc: true };
	assert_eq!(snapshot, expected);

	// 请求发出后再改状态：快照保持原样，下一次 begin 才带上新参数。
	st.set_sort(SortKey::Name, false);
	st.set_limit(5);
	assert_eq!(snapshot, expected);
	let next = st.begin("另一个查询");
	assert_eq!(next.limit, 5);
	assert_eq!(next.sort, SortKey::Name);
	assert!(!next.desc);
}

#[test]
fn last_query_follows_the_latest_begin() {
	let mut st = SearchState::new(20);
	assert_eq!(st.last_query(), "");
	st.begin("第一个");
	assert_eq!(st.last_query(), "第一个");
	st.begin("");
	assert_eq!(st.last_query(), "", "空查询也要如实记录，便于恢复输入框状态");
	st.begin("最终");
	assert_eq!(st.last_query(), "最终");
}
