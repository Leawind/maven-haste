---
grounds:
  - CROSS_PLATFORM
  - TRUSTED_BYTES
rationale: 两个大小写变体指向同一磁盘文件，不清理则数据库与磁盘真相分叉。
summary: 大小写不敏感文件系统上，新大小写变体替换旧条目并清理冲突
---

大小写不敏感文件系统上，安装前删除仅大小写不同的旧记录与旧文件再装新变体（src/cache/install.rs）。
