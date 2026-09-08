---
grounds:
  - TRUSTED_BYTES
rationale: 构建工具强依赖校验和端点；本地生成是同时满足可用性与可信性的唯一方式。
summary: 上游缺失校验和文件时从已验证内容生成，绝不透传未验证的校验和
---

上游缺失校验和文件时，从已验证的主文件内容本地生成（src/cache/download.rs `build_checksum`）；绝不透传未经交叉验证的校验和。
