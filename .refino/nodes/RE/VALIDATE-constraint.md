---
grounds:
  - SERVE_FIRST
rationale: 让 stale-while-revalidate 的刷新成本趋近于零，避免 TTL 过期即全量重下。
summary: 后台刷新携带条件请求头，304 时零成本保留缓存
---

后台刷新携带存储的 ETag/Last-Modified（src/upstream.rs `refresh`）；304 只更新刷新时间戳并保留缓存，不重新下载。
