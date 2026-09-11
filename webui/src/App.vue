<script setup>
import { ref, computed, watch, onMounted, onUnmounted } from 'vue'

// All requests are same-origin: `fer serve` hosts this page itself. During
// `npm run dev` the Vite proxy forwards /api to 127.0.0.1:19876.
const q = ref('')
const limit = ref(200)
const hits = ref([])
const total = ref(0)
const tookMs = ref(0)
const loading = ref(false)
const error = ref('')
const stats = ref(null)
const feed = ref(null)

const sortKey = ref('')      // '' = engine order; else path|size|mtime
const sortDir = ref(1)       // 1 asc, -1 desc
const copiedPath = ref('')

let debounce = null
let feedTimer = null

const FILTERS = [
  { label: '文件', add: 'type:file' },
  { label: '目录', add: 'type:dir' },
  { label: '隐藏', add: 'hidden:true' },
  { label: '>1MB', add: 'size:>1mb' },
  { label: '今天', add: 'dm:today' },
  { label: '本周', add: 'dm:thisweek' },
]

const shown = computed(() => {
  const list = hits.value.slice()
  if (!sortKey.value) return list
  const k = sortKey.value
  const d = sortDir.value
  list.sort((a, b) => {
    let x = a[k], y = b[k]
    if (k === 'path') {
      // Compare the file name first: that is what a search result is "about",
      // and full-path ordering would group by directory instead.
      const an = x.split('\\').pop().toLowerCase()
      const bn = y.split('\\').pop().toLowerCase()
      return an < bn ? -d : an > bn ? d : 0
    }
    return (x - y) * d
  })
  return list
})

function toggleSort(key) {
  if (sortKey.value === key) {
    if (sortDir.value === 1) sortDir.value = -1
    else { sortKey.value = ''; sortDir.value = 1 }
  } else {
    sortKey.value = key
    sortDir.value = key === 'size' || key === 'mtime' ? -1 : 1
  }
}

function addFilter(f) {
  const cur = q.value.trim()
  if (cur.includes(f.add)) return
  q.value = cur ? cur + ' ' + f.add : f.add
}

async function search() {
  const query = q.value.trim()
  if (!query) { hits.value = []; total.value = 0; return }
  loading.value = true
  error.value = ''
  try {
    const r = await fetch(`/api/search?q=${encodeURIComponent(query)}&limit=${limit.value}`)
    const j = await r.json()
    if (!j.ok) { error.value = j.error || '查询失败'; hits.value = []; total.value = 0; return }
    hits.value = j.hits || []
    total.value = j.total || 0
    tookMs.value = j.took_ms || 0
  } catch (e) {
    error.value = String(e)
  } finally {
    loading.value = false
  }
}

function onInput() {
  clearTimeout(debounce)
  debounce = setTimeout(search, 300)
}

watch(q, onInput)

async function refreshStatus() {
  try {
    stats.value = await (await fetch('/api/stats')).json()
  } catch { /* serve 未就绪时静默 */ }
  try {
    feed.value = await (await fetch('/api/feed')).json()
  } catch { /* 未启用变更推送 */ }
}

async function copyPath(p) {
  try {
    await navigator.clipboard.writeText(p)
    copiedPath.value = p
    setTimeout(() => { if (copiedPath.value === p) copiedPath.value = '' }, 1200)
  } catch { error.value = '剪贴板不可用（需要 https 或 localhost）' }
}

function fmtSize(n) {
  if (n === null || n === undefined) return '-'
  if (n < 1024) return n + ' B'
  const u = ['KB', 'MB', 'GB', 'TB']
  let v = n / 1024, i = 0
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++ }
  return v.toFixed(v < 10 ? 1 : 0) + ' ' + u[i]
}

function fmtTime(sec) {
  if (!sec) return '-'
  const d = new Date(sec * 1000)
  const p = (x) => String(x).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

function shortPath(p) {
  // Keep the tail (file name + a couple of parents) — the head is usually the
  // same for every row and just pushes the interesting part out of view.
  const parts = p.split('\\')
  if (parts.length <= 4) return p
  return parts[0] + '\\…\\' + parts.slice(-3).join('\\')
}

onMounted(() => {
  refreshStatus()
  feedTimer = setInterval(refreshStatus, 3000)
})
onUnmounted(() => { clearInterval(feedTimer); clearTimeout(debounce) })
</script>

<template>
  <div class="wrap">
    <header>
      <h1>fer</h1>
      <span class="sub">File Express Retriever</span>
      <span class="spacer"></span>
      <span v-if="stats" class="stat" :title="stats.dump">
        {{ stats.entries.toLocaleString() }} 条 · {{ stats.dump_mb }} MB
      </span>
      <span v-if="feed" class="stat" :class="{ live: feed.batches_applied > 0 }">
        实时 {{ feed.delta_entries }} 条
      </span>
    </header>

    <div class="bar">
      <input
        v-model="q"
        class="search"
        type="text"
        placeholder="文件名 / 通配符 / 查询语言：ext:rs size:>1mb dm:today parent:D:\proj …"
        autofocus
      />
      <select v-model.number="limit" class="limit">
        <option :value="100">100</option>
        <option :value="200">200</option>
        <option :value="500">500</option>
        <option :value="2000">2000</option>
      </select>
    </div>

    <div class="chips">
      <button v-for="f in FILTERS" :key="f.add" class="chip" @click="addFilter(f)">{{ f.label }}</button>
      <span class="spacer"></span>
      <span v-if="loading" class="muted">查询中…</span>
      <span v-else-if="error" class="err">{{ error }}</span>
      <span v-else-if="q.trim()" class="muted">
        显示 {{ hits.length }} / 共 {{ total.toLocaleString() }} 条 · {{ tookMs }} ms
      </span>
    </div>

    <table v-if="hits.length">
      <thead>
        <tr>
          <th class="col-path" @click="toggleSort('path')">
            路径<span v-if="sortKey === 'path'">{{ sortDir === 1 ? ' ▲' : ' ▼' }}</span>
          </th>
          <th class="col-size" @click="toggleSort('size')">
            大小<span v-if="sortKey === 'size'">{{ sortDir === 1 ? ' ▲' : ' ▼' }}</span>
          </th>
          <th class="col-time" @click="toggleSort('mtime')">
            修改时间<span v-if="sortKey === 'mtime'">{{ sortDir === 1 ? ' ▲' : ' ▼' }}</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="h in shown"
          :key="h.path"
          :class="{ copied: copiedPath === h.path }"
          @click="copyPath(h.path)"
          :title="h.path"
        >
          <td class="col-path">
            <span class="badge" :class="h.is_dir ? 'dir' : 'file'">{{ h.is_dir ? 'D' : 'F' }}</span>
            <span class="pname">{{ h.path.split('\\').pop() }}</span>
            <span class="pdir">{{ shortPath(h.path).replace(/\\[^\\]*$/, '') }}</span>
          </td>
          <td class="col-size">{{ h.is_dir ? '-' : fmtSize(h.size) }}</td>
          <td class="col-time">{{ fmtTime(h.mtime) }}</td>
        </tr>
      </tbody>
    </table>

    <p v-else-if="q.trim() && !loading" class="empty">没有匹配结果</p>
    <p v-else-if="!q.trim()" class="empty">
      输入关键词开始搜索。点击任意行可复制完整路径。
    </p>
  </div>
</template>

<style>
:root {
  --bg: #16181d;
  --panel: #1e2128;
  --line: #2b3038;
  --fg: #d6dae0;
  --dim: #868e9b;
  --accent: #4c9aff;
  --dir: #c9a227;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font: 13px/1.5 "Cascadia Mono", Consolas, "Microsoft YaHei", monospace;
}
.wrap { max-width: 1400px; margin: 0 auto; padding: 14px 18px 40px; }

header { display: flex; align-items: baseline; gap: 10px; margin-bottom: 12px; }
header h1 { font-size: 20px; margin: 0; color: var(--accent); letter-spacing: .5px; }
header .sub { color: var(--dim); font-size: 12px; }
.spacer { flex: 1; }
.stat { color: var(--dim); font-size: 12px; margin-left: 12px; }
.stat.live { color: #4caf7d; }

.bar { display: flex; gap: 8px; margin-bottom: 8px; }
.search {
  flex: 1; padding: 9px 12px; font: inherit; color: var(--fg);
  background: var(--panel); border: 1px solid var(--line); border-radius: 6px;
}
.search:focus { outline: none; border-color: var(--accent); }
.limit {
  padding: 0 8px; font: inherit; color: var(--fg);
  background: var(--panel); border: 1px solid var(--line); border-radius: 6px;
}

.chips { display: flex; align-items: center; gap: 6px; margin-bottom: 12px; min-height: 26px; }
.chip {
  padding: 3px 10px; font: inherit; font-size: 12px; color: var(--dim);
  background: var(--panel); border: 1px solid var(--line); border-radius: 20px; cursor: pointer;
}
.chip:hover { color: var(--fg); border-color: var(--accent); }
.muted { color: var(--dim); font-size: 12px; }
.err { color: #e5534b; font-size: 12px; }

table { width: 100%; border-collapse: collapse; }
th {
  text-align: left; padding: 6px 8px; font-weight: normal; font-size: 12px;
  color: var(--dim); border-bottom: 1px solid var(--line); cursor: pointer; user-select: none;
}
th:hover { color: var(--fg); }
td { padding: 4px 8px; border-bottom: 1px solid #23262d; white-space: nowrap; }
tbody tr { cursor: pointer; }
tbody tr:hover { background: #232833; }
tbody tr.copied { background: #1f3a2a; }
.col-size { width: 90px; text-align: right; }
.col-time { width: 150px; }
.col-path { overflow: hidden; text-overflow: ellipsis; }

.badge {
  display: inline-block; width: 16px; text-align: center; margin-right: 8px;
  font-size: 11px; border-radius: 3px; color: #111;
}
.badge.file { background: #5a6b83; color: #dfe6ef; }
.badge.dir { background: var(--dir); }
.pname { color: var(--fg); }
.pdir { color: var(--dim); font-size: 12px; margin-left: 10px; }

.empty { color: var(--dim); margin-top: 40px; text-align: center; }
</style>
