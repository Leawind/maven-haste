---
grounds:
  - TRUSTED_BYTES
rationale: 安全边界与可变性判定单点集中，是 SWR_POLICY 的直接输入。
summary: 请求路径校验与缓存策略分类：先验安全边界，可变性判定驱动刷新行为
---

请求路径段级校验：拒绝遍历、嵌入分隔符、控制字符与 Windows 保留名，首段保留 `.maven-haste` 命名空间；按文件名分类 Permanent/Mutable（metadata 与 SNAPSHOT 别名为 Mutable），驱动新鲜度与刷新（src/request_path.rs）。
