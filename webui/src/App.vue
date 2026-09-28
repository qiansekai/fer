<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from 'vue'

// All requests are same-origin: fer serve hosts this page itself. During
// "npm run dev" the Vite proxy forwards /api to the locally running serve.
// No absolute URL, port or path is baked in here on purpose.
const q = ref('')
const limit = ref(200)
const hits = ref([])
const total = ref(0)
const tookMs = ref(0)
const loading = ref(false)
const error = ref('')
const stats = ref(null)
const feed = ref(null)

// --- sorting ---------------------------------------------------------------
// Click cycles: engine order -> asc -> desc -> engine order. The sort is pushed
// to the server (sort= / desc=) so the whole index is ordered, not just the
// page that happened to be fetched. Old servers ignore the extra params; the
// same comparator is applied to the returned page as a fallback, which is a
// no-op once the server honours the parameters.
const sortKey = ref('')   // '' = engine order; else name|path|size|allocated|mtime
const sortDir = ref(1)    // 1 asc, -1 desc
const DESC_FIRST = ['size', 'allocated', 'mtime']
const COLUMNS = [
  { key: 'name', label: '名称', cls: 'col-name' },
  { key: 'path', label: '目录', cls: 'col-dir' },
  { key: 'size', label: '大小', cls: 'col-size' },
  { key: 'allocated', label: '占用', cls: 'col-alloc' },
  { key: 'mtime', label: '修改时间', cls: 'col-time' },
]
const copiedPath = ref('')

// --- rebuild ---------------------------------------------------------------
// POST /api/rebuild forwards to the elevated monitor, which re-scans the volume
// (5-20 s). It must never fire on a single click.
const rebuildPhase = ref('idle')  // idle | confirm | running
const rebuildMsg = ref('')
const rebuildErr = ref('')
const rebuildElapsed = ref(0)

// --- du panel --------------------------------------------------------------
const duOpen = ref(false)
const duPath = ref('')
const duDepth = ref(1)
const duTop = ref(20)
const duAllocated = ref(true)
const du = ref(null)
const duLoading = ref(false)
const duError = ref('')
const duTookMs = ref(0)
const DU_STORE_KEY = 'fer.du.path'

let debounce = null
let feedTimer = null
let searchSeq = 0

const FILTERS = [
  { label: '文件', add: 'type:file' },
  { label: '目录', add: 'type:dir' },
  { label: '隐藏', add: 'hidden:true' },
  { label: '>1MB', add: 'size:>1mb' },
  { label: '今天', add: 'dm:today' },
  { label: '本周', add: 'dm:thisweek' },
]

function baseName(p) { return p.split('\\').pop() }
function dirName(p) { const i = p.lastIndexOf('\\'); return i > 0 ? p.slice(0, i) : '' }

function sortValue(h, k) {
  if (k === 'name') return baseName(h.path).toLowerCase()
  if (k === 'path') return h.path.toLowerCase()
  return h[k] ?? 0
}

// Client-side mirror of the server ordering. Harmless when the server already
// sorted globally: re-sorting a prefix by the same key keeps it identical.
const shown = computed(() => {
  const list = hits.value.slice()
  const k = sortKey.value
  if (!k) return list
  const d = sortDir.value
  list.sort((a, b) => {
    const x = sortValue(a, k)
    const y = sortValue(b, k)
    return x < y ? -d : x > y ? d : 0
  })
  return list
})

function toggleSort(key) {
  if (sortKey.value !== key) {
    sortKey.value = key
    sortDir.value = DESC_FIRST.includes(key) ? -1 : 1
  } else if (sortDir.value === (DESC_FIRST.includes(key) ? -1 : 1)) {
    sortDir.value = -sortDir.value
  } else {
    sortKey.value = ''
    sortDir.value = 1
  }
  if (q.value.trim()) search()
}

function arrow(key) {
  if (sortKey.value !== key) return ''
  return sortDir.value === 1 ? ' \u25b2' : ' \u25bc'
}

function addFilter(f) {
  const cur = q.value.trim()
  if (cur.includes(f.add)) return
  q.value = cur ? cur + ' ' + f.add : f.add
}

// --- virtual list ----------------------------------------------------------
// Rows are a fixed height, so the visible window is pure arithmetic: render
// only what the viewport can show plus a small overscan. This keeps 2000-5000
// rows as cheap as 20.
const ROW_H = 28
const OVERSCAN = 6
const scrollerEl = ref(null)
const scrollTop = ref(0)
const viewH = ref(400)
let resizeObs = null
let observedEl = null

const canvasH = computed(() => shown.value.length * ROW_H)
const firstIdx = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_H) - OVERSCAN))
const lastIdx = computed(() => Math.min(shown.value.length, firstIdx.value + Math.ceil(viewH.value / ROW_H) + OVERSCAN * 2))
const visible = computed(() => {
  const out = []
  for (let i = firstIdx.value; i < lastIdx.value; i++) out.push({ h: shown.value[i], top: i * ROW_H, i })
  return out
})

function onScroll(e) { scrollTop.value = e.target.scrollTop }

// Client height of the scroller, retried across frames: on the very first
// render (fresh page load, list appearing right after a query) the element can
// still report 0 before the browser has laid the flex column out. Keeping a
// stale smaller value would leave unrendered rows below the last one.
let measureTries = 0
function measure() {
  const el = scrollerEl.value
  if (!el) return
  const h = el.clientHeight
  if (h > 0) { viewH.value = h; measureTries = 0; return }
  if (measureTries < 10) { measureTries++; requestAnimationFrame(measure) }
}

// The list only exists once there are results, so the observer cannot be bound
// at mount time: bind it whenever the element appears and measure immediately.
// Without this the window keeps the 400px default and a tall viewport would be
// left with unrendered (blank) rows below the last rendered one.
function attachObserver() {
  if (!resizeObs || !scrollerEl.value) return
  if (observedEl !== scrollerEl.value) {
    if (observedEl) resizeObs.unobserve(observedEl)
    resizeObs.observe(scrollerEl.value)
    observedEl = scrollerEl.value
  }
  measure()
  requestAnimationFrame(measure)
}

function resetScroll() {
  if (scrollerEl.value) scrollerEl.value.scrollTop = 0
  scrollTop.value = 0
}

async function search() {
  const query = q.value.trim()
  const seq = ++searchSeq
  if (!query) { hits.value = []; total.value = 0; tookMs.value = 0; return }
  loading.value = true
  error.value = ''
  try {
    const params = new URLSearchParams({ q: query, limit: String(limit.value) })
    if (sortKey.value) {
      params.set('sort', sortKey.value)
      params.set('desc', sortDir.value === -1 ? '1' : '0')
    }
    const r = await fetch('/api/search?' + params.toString())
    const j = await r.json()
    if (seq !== searchSeq) return          // a newer query already answered
    if (!j.ok) { error.value = j.error || '查询失败'; hits.value = []; total.value = 0; return }
    hits.value = j.hits || []
    total.value = j.total || 0
    tookMs.value = j.took_ms || 0
    await nextTick()
    attachObserver()
    resetScroll()
  } catch (e) {
    if (seq === searchSeq) error.value = String(e)
  } finally {
    if (seq === searchSeq) loading.value = false
  }
}

function onInput() {
  clearTimeout(debounce)
  debounce = setTimeout(search, 300)
}

watch(q, onInput)
watch(limit, () => { if (q.value.trim()) search() })

async function refreshStatus() {
  try {
    stats.value = await (await fetch('/api/stats')).json()
  } catch { /* serve not ready yet: stay quiet */ }
  try {
    feed.value = await (await fetch('/api/feed')).json()
  } catch { /* change feed disabled */ }
}

async function copyPath(p) {
  try {
    await navigator.clipboard.writeText(p)
    copiedPath.value = p
    setTimeout(() => { if (copiedPath.value === p) copiedPath.value = '' }, 1200)
  } catch { error.value = '剪贴板不可用（需要 https 或 localhost）' }
}

// Double-click opens Explorer with the entry selected. POST (not GET) because
// it has a side effect -- see /api/reveal in src/server.rs.
async function reveal(h) {
  error.value = ''
  try {
    const r = await fetch('/api/reveal?path=' + encodeURIComponent(h.path), { method: 'POST' })
    const j = await r.json()
    if (!j.ok) error.value = j.error || '定位失败'
  } catch (e) { error.value = String(e) }
}

// --- rebuild ---------------------------------------------------------------
async function runRebuild() {
  rebuildPhase.value = 'running'
  rebuildMsg.value = ''
  rebuildErr.value = ''
  rebuildElapsed.value = 0
  const t0 = performance.now()
  try {
    const r = await fetch('/api/rebuild', { method: 'POST' })
    let j = null
    try { j = await r.json() } catch { /* 404 / non-JSON from an older server */ }
    if (j && j.ok) {
      rebuildMsg.value = j.message || '重建完成'
    } else if (j && j.error) {
      rebuildErr.value = j.error
    } else {
      rebuildErr.value = '服务端不支持重建（HTTP ' + r.status + '），需要带 /api/rebuild 的 fer serve'
    }
  } catch (e) {
    rebuildErr.value = String(e)
  } finally {
    rebuildElapsed.value = Math.round(performance.now() - t0)
    rebuildPhase.value = 'idle'
    refreshStatus()
  }
}

// --- du --------------------------------------------------------------------
function pathFromQuery() {
  const m = q.value.match(/(?:^|\s)(?:parent|path):("[^"]+"|\S+)/)
  if (!m) return ''
  let v = m[1]
  if (v.startsWith('"') && v.endsWith('"')) v = v.slice(1, -1)
  return v
}

function syncDuPath() {
  if (duPath.value.trim()) return
  duPath.value = pathFromQuery() || localStorage.getItem(DU_STORE_KEY) || ''
}

function toggleDu() {
  duOpen.value = !duOpen.value
  if (duOpen.value) {
    syncDuPath()
    nextTick(attachObserver)
  }
}

async function runDu(p) {
  if (typeof p === 'string') duPath.value = p
  const root = duPath.value.trim()
  if (!root) { duError.value = '请先填写目录绝对路径'; return }
  duLoading.value = true
  duError.value = ''
  try {
    const params = new URLSearchParams({
      path: root,
      depth: String(duDepth.value),
      top: String(duTop.value),
      allocated: duAllocated.value ? '1' : '0',
    })
    const r = await fetch('/api/du?' + params.toString())
    const j = await r.json()
    if (!j.ok) { duError.value = j.error || '统计失败'; du.value = null; return }
    du.value = j
    duTookMs.value = j.took_ms || 0
    localStorage.setItem(DU_STORE_KEY, root)
  } catch (e) {
    duError.value = String(e)
  } finally {
    duLoading.value = false
    nextTick(attachObserver)
  }
}

function duDrill(e) { runDu(e.path) }

function duUp() {
  const p = duPath.value.trim().replace(/[\\/]+$/, '')
  const i = p.lastIndexOf('\\')
  if (i > 0) { duPath.value = p.slice(0, i); runDu() }
}

// --- formatting ------------------------------------------------------------
function fmtSize(n) {
  if (n === null || n === undefined) return '-'
  if (n === 0) return '0 B'
  if (n < 1024) return n + ' B'
  const u = ['KB', 'MB', 'GB', 'TB', 'PB']
  let v = n / 1024, i = 0
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++ }
  return v.toFixed(v < 10 ? 1 : 0) + ' ' + u[i]
}

function fmtTime(sec) {
  if (!sec) return '-'
  const d = new Date(sec * 1000)
  const p = (x) => String(x).padStart(2, '0')
  return d.getFullYear() + '-' + p(d.getMonth() + 1) + '-' + p(d.getDate()) + ' ' + p(d.getHours()) + ':' + p(d.getMinutes())
}

function allocTitle(h) {
  return '逻辑大小 ' + fmtSize(h.size) + '\n磁盘占用 ' + fmtSize(h.allocated)
}

function diffAlloc(h) {
  return !h.is_dir && h.allocated !== h.size
}

function shortPath(p) {
  const parts = p.split('\\')
  if (parts.length <= 3) return p
  return parts[0] + '\\…\\' + parts.slice(-2).join('\\')
}

onMounted(() => {
  refreshStatus()
  feedTimer = setInterval(refreshStatus, 3000)
  if (typeof ResizeObserver !== 'undefined') {
    resizeObs = new ResizeObserver(measure)
    attachObserver()
  } else {
    window.addEventListener('resize', measure)
  }
})
onUnmounted(() => {
  clearInterval(feedTimer)
  clearTimeout(debounce)
  if (resizeObs) resizeObs.disconnect()
  else window.removeEventListener('resize', measure)
})
</script>

<template>
  <div class="wrap">
    <header>
      <h1>fer</h1>
      <span class="sub">File Express Retriever</span>
      <span class="spacer"></span>
      <span v-if="stats" class="stat" :title="stats.dump">
        {{ (stats.entries || 0).toLocaleString() }} 条 · {{ stats.dump_mb }} MB
      </span>
      <span v-if="feed" class="stat" :class="{ live: feed.batches_applied > 0 }">
        实时 {{ feed.delta_entries }} 条
      </span>
      <span class="rebuild">
        <template v-if="rebuildPhase === 'idle'">
          <button class="btn ghost" @click="rebuildPhase = 'confirm'">重建索引</button>
        </template>
        <template v-else-if="rebuildPhase === 'confirm'">
          <span class="confirm-text">重扫本卷索引？</span>
          <button class="btn danger" @click="runRebuild()">确认</button>
          <button class="btn ghost" @click="rebuildPhase = 'idle'">取消</button>
        </template>
        <template v-else>
          <button class="btn" disabled>重建中…</button>
        </template>
      </span>
    </header>

    <p v-if="rebuildMsg || rebuildErr" class="banner" :class="{ bad: !!rebuildErr }">
      <span v-if="rebuildErr">{{ rebuildErr }}</span>
      <span v-else>{{ rebuildMsg }}</span>
      <span v-if="rebuildElapsed" class="muted"> · {{ rebuildElapsed }} ms</span>
      <button class="x" @click="rebuildMsg = ''; rebuildErr = ''">×</button>
    </p>

    <div class="bar">
      <input
        v-model="q"
        class="search"
        type="text"
        placeholder="文件名 / 通配符 / 查询语言：ext:rs size:>1mb dm:today parent:D:\proj …"
        autofocus
      />
      <select v-model.number="limit" class="limit" title="结果条数上限">
        <option :value="100">100</option>
        <option :value="200">200</option>
        <option :value="500">500</option>
        <option :value="1000">1000</option>
        <option :value="2000">2000</option>
        <option :value="5000">5000</option>
      </select>
    </div>

    <div class="chips">
      <button v-for="f in FILTERS" :key="f.add" class="chip" @click="addFilter(f)">{{ f.label }}</button>
      <span class="spacer"></span>
      <span v-if="loading" class="muted">查询中…</span>
      <span v-else-if="error" class="err">{{ error }}</span>
      <span v-else-if="q.trim()" class="muted">
        显示 {{ hits.length }} / 共 {{ total.toLocaleString() }} 条 · {{ tookMs }} ms
        <template v-if="sortKey"> · 排序：{{ (COLUMNS.find(c => c.key === sortKey) || {}).label }}{{ sortDir === 1 ? ' 升序' : ' 降序' }}</template>
      </span>
    </div>

    <section class="panel" :class="{ open: duOpen }">
      <button class="panel-head" @click="toggleDu()">
        <span class="caret">{{ duOpen ? '▾' : '▸' }}</span>
        <span>目录占用</span>
        <span v-if="du" class="muted">
          根 {{ fmtSize(du.total_bytes) }} · 占用 {{ fmtSize(du.total_allocated) }} · {{ du.files.toLocaleString() }} 文件
        </span>
      </button>
      <div v-if="duOpen" class="panel-body">
        <div class="du-bar">
          <button class="btn ghost" @click="duUp()" :disabled="duPath.trim().replace(/[\\/]+$/, '').lastIndexOf('\\') <= 0" title="上一级">↑</button>
          <input
            v-model="duPath"
            class="du-path"
            type="text"
            placeholder="目录绝对路径（留空则继承查询里的 parent: / 上次使用）"
            @keyup.enter="runDu()"
          />
          <label class="lbl">深度
            <select v-model.number="duDepth">
              <option :value="1">1</option>
              <option :value="2">2</option>
              <option :value="3">3</option>
              <option :value="0">全部</option>
            </select>
          </label>
          <label class="lbl">条目
            <select v-model.number="duTop">
              <option :value="10">10</option>
              <option :value="20">20</option>
              <option :value="50">50</option>
              <option :value="100">100</option>
            </select>
          </label>
          <label class="lbl chk"><input type="checkbox" v-model="duAllocated" /> 按占用排序</label>
          <button class="btn" :disabled="duLoading" @click="runDu()">{{ duLoading ? '统计中…' : '统计' }}</button>
        </div>
        <p v-if="duError" class="err">{{ duError }}</p>
        <template v-else-if="du">
          <div class="du-head">
            <span class="du-root" :title="du.root">{{ shortPath(du.root) }}</span>
            <span class="muted">{{ du.dirs.toLocaleString() }} 子目录 · {{ duTookMs }} ms<template v-if="du.truncated"> · 仅显示前 {{ du.children.length }} 项</template></span>
          </div>
          <table class="du-table">
            <thead>
              <tr>
                <th class="du-name">条目</th>
                <th class="du-num">大小</th>
                <th class="du-num">占用</th>
                <th class="du-num">文件</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="c in du.children" :key="c.path" @click="duDrill(c)" :title="c.path + '\n单击下钻'">
                <td class="du-name">
                  <span class="pname">{{ baseName(c.path) }}</span>
                  <span class="pdir">{{ shortPath(c.path).replace(/\\[^\\]*$/, '') }}</span>
                </td>
                <td class="du-num">{{ fmtSize(c.size) }}</td>
                <td class="du-num" :class="{ hi: c.allocated !== c.size }">{{ fmtSize(c.allocated) }}</td>
                <td class="du-num muted">{{ c.files.toLocaleString() }}</td>
              </tr>
              <tr v-if="!du.children.length"><td colspan="4" class="muted">没有子目录</td></tr>
            </tbody>
          </table>
        </template>
        <p v-else class="muted">按「统计」查看该目录下各级占用；点击任意条目可继续下钻。</p>
      </div>
    </section>

    <div v-if="shown.length" class="results">
      <div class="thead grid">
        <div
          v-for="c in COLUMNS"
          :key="c.key"
          class="th"
          :class="c.cls"
          @click="toggleSort(c.key)"
          :title="'按' + c.label + '排序（新版服务端对全库排序；第三次点击恢复引擎顺序）'"
        >{{ c.label }}{{ arrow(c.key) }}</div>
      </div>
      <div class="vbody" ref="scrollerEl" @scroll="onScroll">
        <div class="vcanvas" :style="{ height: canvasH + 'px' }">
          <div
            v-for="r in visible"
            :key="r.i + '|' + r.h.path"
            class="vrow grid"
            :class="{ copied: copiedPath === r.h.path }"
            :style="{ top: r.top + 'px' }"
            @click="copyPath(r.h.path)"
            @dblclick="reveal(r.h)"
            :title="r.h.path + '\n单击复制路径 · 双击在资源管理器中定位'"
          >
            <div class="cell col-name">
              <span class="badge" :class="r.h.is_dir ? 'dir' : 'file'">{{ r.h.is_dir ? 'D' : 'F' }}</span>
              <span class="pname">{{ baseName(r.h.path) }}</span>
              <span class="pdir narrow-only">{{ shortPath(r.h.path).replace(/\\[^\\]*$/, '') }}</span>
            </div>
            <div class="cell col-dir" :title="r.h.path">{{ dirName(r.h.path) }}</div>
            <div class="cell col-size">{{ r.h.is_dir ? '-' : fmtSize(r.h.size) }}</div>
            <div class="cell col-alloc" :class="{ hi: diffAlloc(r.h) }" :title="allocTitle(r.h)">
              {{ r.h.is_dir ? '-' : fmtSize(r.h.allocated) }}
            </div>
            <div class="cell col-time">{{ fmtTime(r.h.mtime) }}</div>
          </div>
        </div>
      </div>
    </div>

    <p v-else-if="q.trim() && !loading" class="empty">没有匹配结果</p>
    <p v-else-if="!q.trim()" class="empty">
      输入关键词开始搜索。<br />
      单击任意行复制完整路径，<b>双击在资源管理器中定位</b>。
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
  --hi: #c9a227;
  --bad: #e5534b;
  --ok: #4caf7d;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font: 13px/1.5 "Cascadia Mono", Consolas, "Microsoft YaHei", monospace;
}
.wrap {
  max-width: 1400px; margin: 0 auto; padding: 12px 16px 16px;
  height: 100vh; display: flex; flex-direction: column;
}

header { display: flex; align-items: baseline; gap: 10px; margin-bottom: 10px; flex-wrap: wrap; }
header h1 { font-size: 20px; margin: 0; color: var(--accent); letter-spacing: .5px; }
header .sub { color: var(--dim); font-size: 12px; }
.spacer { flex: 1; }
.stat { color: var(--dim); font-size: 12px; margin-left: 12px; }
.stat.live { color: var(--ok); }

.rebuild { margin-left: 12px; display: inline-flex; align-items: center; gap: 6px; }
.confirm-text { color: var(--hi); font-size: 12px; }
.btn {
  padding: 3px 10px; font: inherit; font-size: 12px; color: var(--fg);
  background: var(--panel); border: 1px solid var(--line); border-radius: 5px; cursor: pointer;
}
.btn:hover:not(:disabled) { border-color: var(--accent); }
.btn:disabled { opacity: .6; cursor: default; }
.btn.ghost { color: var(--dim); }
.btn.danger { color: #fff; background: #8c2f2a; border-color: #a63a33; }

.banner {
  margin: 0 0 10px; padding: 6px 10px; font-size: 12px;
  background: #1c2b22; border: 1px solid #2c4436; border-radius: 5px; color: var(--fg);
  display: flex; align-items: center; gap: 6px;
}
.banner.bad { background: #2b1d1c; border-color: #4a2c2a; color: #f0b6b1; }
.banner .x { margin-left: auto; background: none; border: none; color: inherit; cursor: pointer; font-size: 14px; }

.bar { display: flex; gap: 8px; margin-bottom: 8px; }
.search {
  flex: 1; padding: 9px 12px; font: inherit; color: var(--fg);
  background: var(--panel); border: 1px solid var(--line); border-radius: 6px;
}
.search:focus { outline: none; border-color: var(--accent); }
.limit, .panel select {
  padding: 0 6px; font: inherit; color: var(--fg);
  background: var(--panel); border: 1px solid var(--line); border-radius: 6px;
}

.chips { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; min-height: 26px; flex-wrap: wrap; }
.chip {
  padding: 3px 10px; font: inherit; font-size: 12px; color: var(--dim);
  background: var(--panel); border: 1px solid var(--line); border-radius: 20px; cursor: pointer;
}
.chip:hover { color: var(--fg); border-color: var(--accent); }
.muted { color: var(--dim); font-size: 12px; }
.err { color: var(--bad); font-size: 12px; }

/* --- du panel (collapsed by default so search stays the focus) --- */
.panel { border: 1px solid var(--line); border-radius: 6px; margin-bottom: 8px; background: #1a1d23; }
.panel.open { background: var(--panel); }
.panel-head {
  display: flex; align-items: center; gap: 8px; width: 100%; text-align: left;
  padding: 5px 10px; font: inherit; font-size: 12px; color: var(--fg);
  background: none; border: none; cursor: pointer;
}
.panel-head .caret { color: var(--dim); width: 10px; }
.panel-body { padding: 0 10px 10px; }
.du-bar { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; flex-wrap: wrap; }
.du-path {
  flex: 1; min-width: 180px; padding: 5px 8px; font: inherit; font-size: 12px; color: var(--fg);
  background: var(--bg); border: 1px solid var(--line); border-radius: 5px;
}
.du-path:focus { outline: none; border-color: var(--accent); }
.du-bar .lbl { color: var(--dim); font-size: 12px; display: inline-flex; align-items: center; gap: 4px; }
.du-head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 4px; }
.du-root { font-size: 12px; color: var(--fg); }
.du-table { width: 100%; border-collapse: collapse; }
.du-table th {
  text-align: left; padding: 4px 8px; font-weight: normal; font-size: 12px;
  color: var(--dim); border-bottom: 1px solid var(--line); user-select: none;
}
.du-table td { padding: 3px 8px; border-bottom: 1px solid #23262d; font-size: 12px; }
.du-table tbody tr { cursor: pointer; }
.du-table tbody tr:hover { background: #232833; }
.du-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.du-num { text-align: right; width: 96px; white-space: nowrap; }
.hi { color: var(--hi); }

/* --- results: fixed-height virtual list --- */
.results { flex: 1 1 auto; min-height: 120px; display: flex; flex-direction: column; }
.grid { display: grid; grid-template-columns: minmax(140px, 1.2fr) minmax(0, 2fr) 100px 110px 140px; align-items: center; }
.thead { border-bottom: 1px solid var(--line); }
.th {
  padding: 6px 8px; font-size: 12px; color: var(--dim);
  cursor: pointer; user-select: none; white-space: nowrap;
}
.th:hover { color: var(--fg); }
.col-size, .col-alloc, .col-time { text-align: right; }
.vbody { flex: 1 1 auto; min-height: 0; overflow-y: auto; overflow-x: hidden; }
.vcanvas { position: relative; }
.vrow {
  position: absolute; left: 0; right: 0; height: 28px;
  border-bottom: 1px solid #23262d; cursor: pointer;
}
.vrow:hover { background: #232833; }
.vrow.copied { background: #1f3a2a; }
.cell { padding: 0 8px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }

.badge {
  display: inline-block; width: 16px; text-align: center; margin-right: 8px;
  font-size: 11px; border-radius: 3px; color: #111;
}
.badge.file { background: #5a6b83; color: #dfe6ef; }
.badge.dir { background: var(--dir); }
.pname { color: var(--fg); }
.pdir { color: var(--dim); font-size: 12px; margin-left: 10px; }
.narrow-only { display: none; }

.empty { color: var(--dim); margin-top: 40px; text-align: center; }

/* Narrow windows: drop the least useful columns instead of squeezing them. */
@media (max-width: 900px) {
  .grid { grid-template-columns: minmax(140px, 1.6fr) 100px 110px 140px; }
  .col-dir { display: none; }
  .narrow-only { display: inline; }
}
@media (max-width: 620px) {
  .grid { grid-template-columns: minmax(120px, 1fr) 96px 130px; }
  .col-alloc { display: none; }
  .wrap { padding: 10px 10px 10px; }
  header .sub { display: none; }
}
</style>
