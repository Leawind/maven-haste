---
confirmed: 2026-09-06T17:09:09.321Z
summary: Maven 仓库语义：release 构件不可变，可变内容仅限 metadata 与 SNAPSHOT 别名
---

Maven 仓库中 release 构件（含校验和）发布后不可变；可变的仅 maven-metadata.xml（含校验和）与 SNAPSHOT 别名路径。过期的 release 缓存仍与上游一致，是缓存优先策略的安全性依据。
