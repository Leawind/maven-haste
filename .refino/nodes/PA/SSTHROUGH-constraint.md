---
grounds:
  - TRUSTED_BYTES
rationale: 「不写入」是存储策略而非信任策略，直通不得成为绕过校验的后门。
summary: cache_writes=false 的仓库仍走完整验证，内容以共享临时文件直通
---

`cache_writes=false` 的仓库不落库，但下载走同一验证链，内容以共享临时文件服务并在响应结束清理；已有缓存照常服务。
