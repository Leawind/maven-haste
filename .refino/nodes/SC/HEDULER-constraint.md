---
grounds:
  - SERVE_FIRST
  - STALE_SERVE
rationale: 前台低延迟与后台新鲜度补偿的执行机制。
summary: 全局与每仓库并发上限，前台优先但不饿死后台刷新
---

全局与每仓库双重并发上限；前台优先，但连续放行 `foreground_priority_burst` 个前台后放行一个后台，刷新不被饿死；取消的等待者不泄漏配额（src/upstream.rs）。
