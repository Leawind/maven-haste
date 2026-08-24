# 仓库指南

## 项目结构与模块组织

- 除非用户明确要求，否则不要查看 docs-dev 中的文档
- `deno.json` 用于格式化 Markdown、JSON 和 YAML
- 当前项目未发布正式版本，可以进行任意破坏性更改，不要试图兼容旧实现
- 修改文档、代码注释时仅正向描述当前实现，不出现对旧的实现的负面描述

## 构建、测试和开发命令

```bash
cargo fmt --check   # 验证 Rust 格式
cargo test          # 运行单元测试和集成测试
cargo clippy -- -D warnings  # 拒绝 lint 警告
cargo run -- --check -c .\maven-haste.toml  # 验证本地配置
deno fmt --check    # 验证 Markdown/JSON/YAML 格式
```

在提交文档或配置更改之前运行 `deno fmt`。

运行会写入构建目录的 Cargo 命令前，先通过 `cargo metadata` 确认 `target_directory`；若它位于工作区外，首次执行就使用可写该目录的本机权限。

## 编码风格与命名约定

使用稳定的、符合惯用法的 Rust：4 空格；函数和模块使用 `snake_case`；类型使用 `PascalCase`；常量使用 `SCREAMING_SNAKE_CASE`。围绕配置、路由、缓存、上游客户端和 HTTP 服务器组织小型模块。保持 I/O 在 Tokio 上运行，避免阻塞请求路径。

对于散文/配置，Deno 强制使用 4 空格、LF 换行、120 字符行和适用时使用单引号。

## 测试指南

将单元测试放在模块旁边，将跨组件行为放在 `tests/` 中。以可观察行为命名测试，例如 `returns_stale_metadata_while_refreshing`。覆盖解析、路由优先级、缓存、原子写入和上游故障。使用临时目录和本地模拟 HTTP 服务器；绝不依赖公共仓库或用户配置。

## 提交与拉取请求指南

历史记录使用简短的祈使句主题，包括 `init` 和 `docs: add design.md`；保持这种风格，并在有用时使用范围，例如 `cache: handle missing files`。保持提交聚焦。拉取请求应描述行为、链接问题或设计章节、列出验证步骤，并包含用户可见的 CLI 或 HTTP 变更示例。

## Git 工作流

- 提交信息使用英语和 Conventional Commits 风格。
- 复杂任务应在形成可构建、职责完整的阶段时及时提交，不必等到所有规划功能全部完成。
- 在受限执行环境中创建本地提交时，先使用可写入仓库 `.git` 目录的本机权限执行 `git add` 与 `git commit`。`index.lock` 的“只读文件系统”错误表示环境限制，不是残留锁；不要删除该文件来规避错误。

## 发布流程

- 发布前由开发者手动更新 `CHANGELOG.md`，并添加与标签 `vX.Y.Z` 对应的 `## [X.Y.Z]` 小节。
- 推送 `v*` 标签会验证代码，并构建 Linux、macOS 与 Windows 的压缩二进制包。
- 构建成功后创建含压缩包和 CHANGELOG 发布说明的 GitHub Release 草稿，再发布到 crates.io，最后发布该草稿。

## 配置与安全

绝不提交本地缓存内容、SQLite 数据库、凭据或用户配置文件。将仓库 URL 和认证头视为敏感信息。保留设计需求：相对存储路径应从配置文件的目录解析。
