---
grounds:
  - TRUSTED_BYTES
  - STALE_SERVE
rationale: 淘汰与并发服务的竞争不得产出不可信字节；被误删的缓存由重取恢复。
summary: 容量淘汰跳过正在服务的路径，大小不符的条目重取而非照常服务
---

容量淘汰按最近访问排序，跳过正在服务、下载或刷新的路径；服务前发现文件大小与记录不符即重新回源，不按记录的 Content-Length 硬服务。
