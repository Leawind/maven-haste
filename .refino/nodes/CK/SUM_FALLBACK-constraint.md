---
grounds:
  - TRUSTED_BYTES
rationale: 上游校验和元数据可能陈旧而内容稳定；字节仍完整来自上游。有意取舍，不是 bug。
summary: 校验和验证失败时的两条有意的兜底路径，不是缺陷
---

校验和交叉验证的两条有意兜底（src/cache/download.rs `select_stable_download`）：上游重复返回相同字节但校验和对不上时，接受内容并告警；复验重试失败时保留首个完整下载（已过长度校验）。测试：`stable_metadata_with_inconsistent_checksums_returns_success`、`checksum_retry_failure_keeps_first_complete_download`。
