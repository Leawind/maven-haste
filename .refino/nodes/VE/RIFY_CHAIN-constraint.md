---
grounds:
  - TRUSTED_BYTES
rationale: 把「不可信字节」落成可逐段检查的工程判据，而非凭直觉。
summary: 内容可信链路：下载即哈希、长度校验、上游校验和交叉验证、原子安装、服务前复核
---

五段可信链路：下载即哈希并校验长度（src/cache/io.rs）→ 上游校验和交叉验证（src/cache/download.rs）→ 先记录后原子 rename（src/cache/install.rs）→ 服务前大小复核（src/cache/serve.rs）→ 仅从缓存文件读取（src/server.rs）；`cache verify` 全量核查。
