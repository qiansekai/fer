# fer HTTP API 真实响应样本（2026-09-30 实测，本机 serve @ 127.0.0.1:19876）

> 用途：解析器与客户端的对齐依据。**以本节实测为准**（README 的表述比实际响应更宽泛）。
> 抓取方式：`Invoke-WebRequest "http://127.0.0.1:19876/api/..."`

## GET /api/health
```json
{"ok":true}
```

## GET /api/search?q=ext%3Ars&limit=3
```json
{"count":3,"engine":"mem","hits":[{"allocated":8011,"ctime":1790729897,"flags":0,"is_dir":false,"mtime":1153704088,"path":"D:\\Kita-Tools\\...\\vts.rs","size":8011}],"ok":true,"query":"ext:rs","took_ms":18,"total":32406}
```

要点：
- 顶层字段：`ok / query / count / total / took_ms / engine / hits`（`engine` 未在文档列出，忽略即可）
- `hits[]` 字段：`path / is_dir / size / allocated / mtime / ctime / flags`（**无 name 字段**，名称需从 path 派生）
- `count` = 本次返回条数；`total` = 未截断总命中数
- 排序请求 `&sort=size&desc=1` 生效（实测返回最大者在前）；`took_ms` 含排序成本（277ms）
- 查询出错时顶层 `ok:false` + `error` 文本（字段名以实际为准，解析侧需容错）

## GET /api/stats
```json
{"dirs":527617,"dump":"C:\\Users\\<user>\\AppData\\Local\\file-engine-rust\\index.db.feridx","dump_mb":1474,"entries":4270030,"files":3742413,"mem_bytes":1546156084,"ok":true}
```

要点：**实际没有 volumes / index_method / built_at_unix / feed 字段**（与 README 的 CLI `fer stats` 输出不同）。
客户端的 IndexStats 需为这些字段提供回退默认值，不得因缺字段报错。

## GET /api/feed
```json
{"batches_applied":6986,"delta_entries":1259,"last_apply_age_ms":2227,"ok":true,"overlay_generation":47,"pending_append":1259,"pending_remove":148,"removed_in_dump":148,"sample_append":["D:\\..."],"sample_remove":["d:\\..."],"saturated":false}
```

要点：overlay 实时状态；`saturated=true` 表示 pending 超过上限（需 flush 或 `fer index`）。

## GET /api/du?path=D%3A%5CKita-Tools%5CCoding&depth=1&top=3
```json
{"children":[{"allocated":4902397412,"depth":1,"files":11630,"path":"D:\\Kita-Tools\\Coding\\fer","size":4887028896}],"dirs":16097,"entries":108268,"files":89758,"ok":true,"root":"D:\\Kita-Tools\\Coding","took_ms":151,"total_allocated":14312089559,"total_bytes":14178791197,"truncated":true}
```

要点：`children[]` 只有 `path / size / allocated / files / depth`（**无 name**，名称从 path 末段派生）；
顶层有 `total_bytes / total_allocated / files / dirs / entries / truncated / took_ms`。
## 契约陷阱：`/api/du` 的 allocated 只认 true/false

```
GET /api/du?path=<p>&allocated=true     -> 200
GET /api/du?path=<p>&allocated=1        -> 400 provided string was not `true` or `false`
```

原因：du 的 `allocated` 由 axum 的 `Query<Option<bool>>` 直接反序列化（serde 原生 bool），
而 `/api/search` 的 `desc` 走 fer 自己的 `parse_flag`（认 `1/0/yes/on/true/false`）。
**两个相邻布尔旋钮行为不一致，GUI 侧一律按各自的实测语义发送**（2026-09-30 core-io 实联调发现）。
