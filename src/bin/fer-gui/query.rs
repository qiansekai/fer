//! 搜索会话状态机（UI 无关的纯逻辑，全部可单测）。
//!
//! 契约（冻结，见 docs/INTERFACES.md 的 src/query.rs 一节）：
//!   SortKey::{Name, Path, Size, Allocated, Mtime, Ctime}（库侧 mem::SortKey 的 re-export）
//!   sort_label(SortKey) / sort_from_column(usize)
//!   SearchState::new(limit) -> Self
//!   SearchState::begin(&mut self, query: &str) -> SearchRequest   （递增代际）
//!   SearchState::accept(&self, generation: u64) -> bool           （丢弃过期响应）
//!   SearchState::cycle_sort(&mut self, key: SortKey) -> (SortKey, bool)  三态循环
//!   SearchState::{generation, sort, set_sort, limit, set_limit, last_query}
//!   now_ms() -> u64
//!
//! 不变式（实现必须始终维持，src/query.rs 单测与 tests/query_state.rs 逐条覆盖）：
//!   1. 代际号从 0 起步、只增不减；begin 每调用一次加一，真实请求的 generation 恒 >= 1，
//!      0 只表示「本次会话尚未发起过搜索」。
//!   2. accept(g) 仅接受「g != 0 且 g 等于当前代际」；一旦有新 begin，更早的代际立即失效，
//!      避免旧查询的响应覆盖新查询的结果（UI 侧连按回车时的主要竞态）。
//!   3. begin 返回的是请求时刻的参数快照；此后改排序/条数不影响已发出的请求。
//!   4. cycle_sort 同列三态：升 -> 降 -> 回 Name 默认（Name 升序）；换列一律从升序开始。
//!   5. limit 恒被钳制在 LIMIT_MIN..=LIMIT_MAX；new 与 set_limit 走同一钳制函数。

use std::time::{SystemTime, UNIX_EPOCH};

/// 排序键的唯一来源在库侧（`mem::SortKey`）：CLI 的 `--sort`、服务端的
/// `sort=` 参数与 GUI 共用同一份字符串表。这里只做 re-export，不再维护副本。
pub use file_engine_rust::mem::SortKey;

/// limit 下限：至少 1 条，避免发出「要 0 条」这种无意义请求。
pub const LIMIT_MIN: usize = 1;

/// limit 上限：一次返回量的现实上界，防止误配的超大值拖垮网络与结果表。
pub const LIMIT_MAX: usize = 100_000;

/// 把任意值收敛到 [LIMIT_MIN, LIMIT_MAX]（0 与 usize::MAX 都安全）。
fn clamp_limit(n: usize) -> usize {
	n.clamp(LIMIT_MIN, LIMIT_MAX)
}

/// 列头与排序提示上显示的中文名（列头当前用固定文案，保留单一来源供 UI 复用）。
#[allow(dead_code)]
pub fn sort_label(key: SortKey) -> &'static str {
	match key {
		SortKey::Name => "名称",
		SortKey::Path => "路径",
		SortKey::Size => "大小",
		SortKey::Allocated => "占用",
		SortKey::Mtime => "修改时间",
		SortKey::Ctime => "创建时间",
	}
}

/// 结果表列索引 -> 排序键（0 名称 / 1 路径 / 2 大小 / 3 修改时间）；
/// 越界或不可排序列（4 起）返回 None，调用方直接忽略该次点击。
pub fn sort_from_column(index: usize) -> Option<SortKey> {
	match index {
		0 => Some(SortKey::Name),
		1 => Some(SortKey::Path),
		2 => Some(SortKey::Size),
		3 => Some(SortKey::Mtime),
		_ => None,
	}
}

/// 一次搜索请求的全部参数（快照，交给后台线程后不再变化）。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
	/// 本次请求的代际号，回调时原样带回给 `accept` 判定。
	pub generation: u64,
	pub query: String,
	pub limit: usize,
	pub sort: SortKey,
	pub desc: bool,
}

/// 搜索会话状态：代际号、上次查询、排序状态与条数上限。
#[derive(Debug)]
pub struct SearchState {
	generation: u64,
	last_query: String,
	sort: SortKey,
	desc: bool,
	limit: usize,
}

impl SearchState {
	/// 新建会话：代际 0（尚未发起搜索）、Name 升序；limit 先钳制再存入。
	pub fn new(limit: usize) -> Self {
		Self {
			generation: 0,
			last_query: String::new(),
			sort: SortKey::Name,
			desc: false,
			limit: clamp_limit(limit),
		}
	}

	/// 开始一次新搜索：递增代际、记录查询串，并返回包含当前排序/条数的请求快照。
	///
	/// 代际用饱和加法：u64 上限现实中搜不出来，真到了也只退化（代际不再变化）而不会 panic。
	pub fn begin(&mut self, query: &str) -> SearchRequest {
		self.generation = self.generation.saturating_add(1);
		self.last_query = query.to_string();
		SearchRequest {
			generation: self.generation,
			query: query.to_string(),
			limit: self.limit,
			sort: self.sort,
			desc: self.desc,
		}
	}

	/// 当前代际（0 = 尚未发起过搜索）。单测用它验证单调性。
	#[allow(dead_code)]
	pub fn generation(&self) -> u64 {
		self.generation
	}

	/// 代际匹配才接受（否则是被新查询取代的过期响应）。
	///
	/// 额外严格化：0 不是任何真实请求的代际，`accept(0)` 恒为 false，
	/// 防止「尚未 begin」时把占位回调当成有效结果。
	pub fn accept(&self, generation: u64) -> bool {
		generation != 0 && generation == self.generation
	}

	/// 点击列头：同列三态循环 升 -> 降 -> 回 Name 默认（Name 升序）；换列一律升序。
	///
	/// 返回点击后的 (排序键, 是否降序)，调用方据此刷新列头箭头并重发查询。
	pub fn cycle_sort(&mut self, key: SortKey) -> (SortKey, bool) {
		if key == self.sort {
			if self.desc {
				// 第二态（降序）-> 第三态：回到 Name 默认，而不是继续停在本列。
				self.sort = SortKey::Name;
				self.desc = false;
			} else {
				// 第一态（升序）-> 第二态（降序）。
				self.desc = true;
			}
		} else {
			// 换列：从升序开始。
			self.sort = key;
			self.desc = false;
		}
		(self.sort, self.desc)
	}

	/// 当前排序状态 (排序键, 是否降序)。
	pub fn sort(&self) -> (SortKey, bool) {
		(self.sort, self.desc)
	}

	/// 直接设定排序状态（用于从配置恢复），不做三态推导。
	pub fn set_sort(&mut self, key: SortKey, desc: bool) {
		self.sort = key;
		self.desc = desc;
	}

	/// 当前条数上限（恒在 LIMIT_MIN..=LIMIT_MAX 内）。
	pub fn limit(&self) -> usize {
		self.limit
	}

	/// 设定条数上限：钳制到 [LIMIT_MIN, LIMIT_MAX]；不影响已发出的请求快照。
	pub fn set_limit(&mut self, n: usize) {
		self.limit = clamp_limit(n);
	}

	/// 最近一次 `begin` 使用的查询串（用于重启后恢复输入框）。单测覆盖。
	#[allow(dead_code)]
	pub fn last_query(&self) -> &str {
		&self.last_query
	}
}

/// 当前 Unix 毫秒；系统时钟早于 1970（不可能，但防御）时返回 0。
pub fn now_ms() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 全部排序键，供遍历测试使用。
	const ALL_KEYS: [SortKey; 6] =
		[SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Allocated, SortKey::Mtime, SortKey::Ctime];

	#[test]
	fn new_state_defaults_are_name_ascending_and_generation_zero() {
		let st = SearchState::new(50);
		assert_eq!(st.generation(), 0);
		assert_eq!(st.sort(), (SortKey::Name, false));
		assert_eq!(st.limit(), 50);
		assert_eq!(st.last_query(), "");
		assert_eq!(SortKey::default(), SortKey::Name);
	}

	#[test]
	fn new_clamps_limit() {
		assert_eq!(SearchState::new(0).limit(), LIMIT_MIN);
		assert_eq!(SearchState::new(1).limit(), 1);
		assert_eq!(SearchState::new(100_000).limit(), 100_000);
		assert_eq!(SearchState::new(100_001).limit(), 100_000);
		assert_eq!(SearchState::new(usize::MAX).limit(), 100_000);
	}

	#[test]
	fn begin_increments_generation_monotonically() {
		let mut st = SearchState::new(10);
		assert_eq!(st.begin("a").generation, 1);
		assert_eq!(st.generation(), 1);
		assert_eq!(st.begin("b").generation, 2);
		assert_eq!(st.begin("").generation, 3);
		assert_eq!(st.generation(), 3);
	}

	#[test]
	fn begin_returns_snapshot_of_current_settings() {
		let mut st = SearchState::new(10);
		st.set_sort(SortKey::Size, true);
		st.set_limit(7);
		let req = st.begin("报告");
		assert_eq!(req, SearchRequest {
			generation: 1,
			query: "报告".to_string(),
			limit: 7,
			sort: SortKey::Size,
			desc: true,
		});
		assert_eq!(st.last_query(), "报告");
	}

	#[test]
	fn begin_snapshot_is_immune_to_later_changes() {
		let mut st = SearchState::new(10);
		let first = st.begin("旧查询");
		st.set_sort(SortKey::Ctime, true);
		st.set_limit(999);
		// 已发出的快照必须原样保留，否则过期响应会与新请求参数混淆。
		assert_eq!(first.limit, 10);
		assert_eq!(first.sort, SortKey::Name);
		assert!(!first.desc);
		assert_eq!(first.query, "旧查询");
		let second = st.begin("新查询");
		assert_eq!(second.limit, 999);
		assert_eq!(second.sort, SortKey::Ctime);
		assert!(second.desc);
	}

	#[test]
	fn accept_only_accepts_current_nonzero_generation() {
		let mut st = SearchState::new(10);
		// 尚未搜索：任何代际都不合法，0 也不例外。
		assert!(!st.accept(0));
		assert!(!st.accept(1));
		st.begin("a");
		assert!(st.accept(1));
		assert!(!st.accept(0));
		assert!(!st.accept(2));
		assert!(!st.accept(u64::MAX));
		// 新查询（连按回车）让上一代立即失效。
		st.begin("ab");
		assert!(!st.accept(1));
		assert!(st.accept(2));
	}

	#[test]
	fn cycle_sort_on_name_is_tri_state() {
		let mut st = SearchState::new(10);
		assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, true));
		assert_eq!(st.sort(), (SortKey::Name, true));
		assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, false));
		assert_eq!(st.sort(), (SortKey::Name, false));
		// 第三态之后重新从升序开始，三态可无限循环。
		assert_eq!(st.cycle_sort(SortKey::Name), (SortKey::Name, true));
	}

	#[test]
	fn cycle_sort_on_other_column_returns_to_name_default() {
		let mut st = SearchState::new(10);
		assert_eq!(st.cycle_sort(SortKey::Size), (SortKey::Size, false));
		assert_eq!(st.cycle_sort(SortKey::Size), (SortKey::Size, true));
		// 第三态：不在本列停住，而是回到 Name 默认。
		assert_eq!(st.cycle_sort(SortKey::Size), (SortKey::Name, false));
		assert_eq!(st.sort(), (SortKey::Name, false));
		// 回到默认后再点该列，重新是升序。
		assert_eq!(st.cycle_sort(SortKey::Size), (SortKey::Size, false));
	}

	#[test]
	fn cycle_sort_switching_column_starts_ascending() {
		let mut st = SearchState::new(10);
		st.set_sort(SortKey::Size, true);
		assert_eq!(st.cycle_sort(SortKey::Mtime), (SortKey::Mtime, false));
		assert_eq!(st.cycle_sort(SortKey::Mtime), (SortKey::Mtime, true));
		assert_eq!(st.cycle_sort(SortKey::Path), (SortKey::Path, false));
		// Name 作为「被切走的列」时同样是换列语义（升序），不受默认列影响。
		st.set_sort(SortKey::Name, true);
		assert_eq!(st.cycle_sort(SortKey::Ctime), (SortKey::Ctime, false));
	}

	#[test]
	fn set_limit_clamps_both_ends() {
		let mut st = SearchState::new(10);
		st.set_limit(0);
		assert_eq!(st.limit(), LIMIT_MIN);
		st.set_limit(1);
		assert_eq!(st.limit(), 1);
		st.set_limit(100_000);
		assert_eq!(st.limit(), 100_000);
		st.set_limit(100_001);
		assert_eq!(st.limit(), 100_000);
		st.set_limit(usize::MAX);
		assert_eq!(st.limit(), 100_000);
	}

	#[test]
	fn set_limit_does_not_retroactively_change_issued_request() {
		let mut st = SearchState::new(10);
		let req = st.begin("x");
		st.set_limit(99);
		assert_eq!(req.limit, 10);
		assert_eq!(st.begin("y").limit, 99);
	}

	#[test]
	fn generation_saturates_instead_of_panicking() {
		let mut st = SearchState::new(10);
		st.generation = u64::MAX;
		assert_eq!(st.begin("a").generation, u64::MAX);
		assert_eq!(st.begin("b").generation, u64::MAX);
		assert!(st.accept(u64::MAX));
	}

	#[test]
	fn sort_key_api_vocabulary_round_trips() {
		for key in ALL_KEYS {
			assert_eq!(SortKey::parse(key.as_str()), Some(key));
			// 服务端契约是小写；库侧 parse 宽松（大小写不敏感），写端始终发小写。
			assert_eq!(key.as_str(), key.as_str().to_lowercase());
			assert!(!sort_label(key).is_empty());
		}
		assert_eq!(SortKey::Name.as_str(), "name");
		assert_eq!(SortKey::Allocated.as_str(), "allocated");
		assert_eq!(SortKey::Mtime.as_str(), "mtime");
		assert_eq!(SortKey::parse(""), None);
		assert_eq!(SortKey::parse("Name"), Some(SortKey::Name));
		assert_eq!(SortKey::parse("modified"), None);
	}

	#[test]
	fn sort_key_labels_are_distinct() {
		for (i, a) in ALL_KEYS.iter().enumerate() {
			for b in ALL_KEYS.iter().skip(i + 1) {
				assert_ne!(sort_label(*a), sort_label(*b), "列名重复会让用户无法区分列头");
			}
		}
	}

	#[test]
	fn from_column_maps_ui_columns_only() {
		assert_eq!(sort_from_column(0), Some(SortKey::Name));
		assert_eq!(sort_from_column(1), Some(SortKey::Path));
		assert_eq!(sort_from_column(2), Some(SortKey::Size));
		assert_eq!(sort_from_column(3), Some(SortKey::Mtime));
		for index in 4..=8 {
			assert_eq!(sort_from_column(index), None, "第 {index} 列不可排序");
		}
		assert_eq!(sort_from_column(usize::MAX), None);
	}

	#[test]
	fn set_sort_overrides_state_without_tri_state() {
		let mut st = SearchState::new(10);
		st.set_sort(SortKey::Allocated, true);
		assert_eq!(st.sort(), (SortKey::Allocated, true));
		st.set_sort(SortKey::Allocated, false);
		assert_eq!(st.sort(), (SortKey::Allocated, false));
		st.set_sort(SortKey::Path, true);
		assert_eq!(st.sort(), (SortKey::Path, true));
	}

	#[test]
	fn now_ms_is_wall_clock_millis() {
		let floor = 1_600_000_000_000_u64; // 2020-09-13
		let now = now_ms();
		assert!(now > floor, "now_ms 应远大于 2020 年：{now}");
		assert!(now < floor * 100, "毫秒量级应远小于 100 倍下限");
	}
}
