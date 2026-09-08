---
grounds:
  - NO_OTHER_NET
rationale: Maven 仓库生态的常规行为（CDN 跳转）；收紧为拒绝跨主机跳转会破坏部分仓库可用性。
summary: 跟随上游重定向是对约束「仅访问显式配置地址」的有意解释
---

上游客户端跟随至多 5 次重定向（src/upstream.rs `build_http_client`）；由配置上游发起的跳转视为上游行为的一部分，不算额外外联。
