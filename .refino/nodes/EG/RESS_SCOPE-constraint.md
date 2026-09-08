---
grounds:
  - NO_OTHER_NET
rationale: 出站收敛为单一代码出口，「除配置外零外联」才可被审计。
summary: 出站网络唯一出口：upstream.rs 的 reqwest 客户端，仅访问配置的仓库与代理
---

全库唯一出站客户端在 src/upstream.rs，URL 仅由配置的 repository.url 拼接；显式 `.no_proxy()` 不读环境代理，仅配置 `[upstream].proxy` 时挂代理；无遥测、无版本检查。
