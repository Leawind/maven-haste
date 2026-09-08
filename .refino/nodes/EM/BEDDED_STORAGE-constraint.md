---
grounds:
  - SINGLE_BINARY
rationale: 自托管部署成本最小化：下载一个文件即可运行。
summary: 存储自包含：SQLite 编译进二进制，迁移编译期嵌入，无外部进程
---

SQLite 以 bundled 特性编译进二进制；迁移由 build.rs 编译期嵌入（src/db.rs），运行时不读目录；单入口单进程交付。
