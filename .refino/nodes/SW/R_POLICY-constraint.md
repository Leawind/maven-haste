---
grounds:
  - SERVE_FIRST
  - MAVEN_IMMUTABLE
  - PATH_POLICY
rationale: 前台延迟优先，新鲜度后台补偿；release 不回源的安全性来自前提 MAVEN_IMMUTABLE。
summary: stale-while-revalidate：可变内容立即返回过期值并后台刷新；release 直接信任缓存
---

可变内容命中即返回，过期标记 Stale 并触发至多一个后台刷新；release 永久信任缓存、不回源；并发 miss 由 flight 合并为一次下载（src/cache/serve.rs）。
