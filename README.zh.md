| 中文 | [English](README.md) |
| ---- | -------------------- |

# Maven Haste

一个轻量、可自托管的本地 Maven 仓库代理缓存。

它运行在构建工具和上游 Maven 仓库之间：依赖第一次请求时从上游下载并保存到本地，之后优先由本地缓存响应。适合个人开发机、局域网构建机和 CI，用来减少重复下载，并提高网络不稳定时的构建成功率。

- 多上游仓库回退，以及按请求路径路由
- 固定版本和时间戳快照持久缓存
- 可变文件的过期后台刷新
- 可变文件 404 负缓存、上游熔断和并发控制

## 开始

maven-haste 以单个命令行工具的形式发布。

- 从 [crates.io](https://crates.io/crates/maven-haste) 构建并安装
  ```bash
  cargo install maven-haste
  ```
- 从 [源码](https://github.com/Leawind/maven-haste) 构建并安装
  ```bash
  git clone https://github.com/Leawind/maven-haste
  cd maven-haste
  ```
  ```bash
  cargo install --path .
  ```
- 从 [Github Release](https://github.com/Leawind/maven-haste/releases) 下载

## 跑起来

在当前工作目录生成配置模板（不会覆盖已有文件）

```bash
maven-haste config init [PATH]
```

使用当前工作目录下的配置启动；`maven-haste.json`、`maven-haste.toml`、`maven-haste.yaml` 会被自动发现

```bash
maven-haste run
```

可以通过命令行参数 `-c --config <PATH>` 指定配置文件路径。

## 配置

> [!TIP]
>
> 运行 `maven-haste config init [PATH]` 生成只含必需键的最小配置；语言（TOML/YAML/JSON）随 `PATH` 的扩展名决定，默认生成 `maven-haste.toml`。各配置项的字段说明见下方列出的 JSON Schema。

推荐步骤：

1. 在 `[[repositories]]` 中配置上游 Maven 仓库。
2. 根据需要调整 `[server]` 的监听地址和本地 endpoint 前缀。
3. 设置 `[storage]` 的存储根目录。相对路径视为相对于配置文件所在目录。
4. 按网络环境调整缓存 TTL、上游超时、并发和熔断参数。
5. 用 `maven-haste config check -c <配置文件>` 验证配置和存储目录，再启动服务。

程序未指定 `-c/--config` 时，会依次查找当前目录和系统用户配置目录中的 `maven-haste.json`、`maven-haste.toml`、`maven-haste.yaml`；若同时存在多个格式则报错停止，而不是选择其中一个。解析器由文件扩展名决定（`json`、`yaml`、`yml`、`toml`）；无法识别或缺失的扩展名会直接报错，不再回退为 TOML。启动时可以用全局参数临时覆盖监听地址或启用调试日志；长期配置应写入配置文件。

### 借助 Schema 编辑

描述整个配置的 JSON Schema 位于 [maven-haste.schema.json](maven-haste.schema.json)，也可用 `maven-haste config schema` 输出（`-o <PATH>` 写入文件）。`config init` 按目标路径扩展名生成对应语言（TOML/YAML/JSON）的最小配置，其 `$schema` 键已默认启用且 URL 固定指向当前发布版本，因此编辑器或 AI 工具无需任何编辑与 IDE 设置即可获得补全、字段说明、默认值和校验提示：

```toml
"$schema" = "https://raw.githubusercontent.com/Leawind/maven-haste/vX.Y.Z/maven-haste.schema.json"
```

其中 `X.Y.Z` 为 `maven-haste --version` 显示的版本；`config init` 会自动填入。手工引用时照此替换版本号即可。

URL 指向当前发布版本的 tag 而非 `main`，因此按发行版保持有效（从包含 schema 的发布 tag 起可访问，之后的每个发行版 tag 都能精确匹配）。离线环境可在配置旁生成一次本地副本 `maven-haste config schema -o ./maven-haste.schema.json`，改为引用 `./maven-haste.schema.json`。

在配置文件中写入引用：

- JSON：顶层 `"$schema": "<引用>"`
- YAML：顶层 `$schema: <引用>`，或首行 `# yaml-language-server: $schema=<引用>`
- TOML：顶层 `"$schema" = "<引用>"`；TOML 语法要求带引号，Even Better TOML 插件识别该键

任何格式下 maven-haste 都会接受并忽略 `$schema` 键，带标注的配置仍能通过 `config check`。schema 由 Rust 配置类型自动生成（schemars）：修改配置结构后，用 `maven-haste config schema -o maven-haste.schema.json` 重新生成并随改动一起提交；单元测试会在提交的 schema 过期时失败。

### 上游路由

仓库按配置顺序参与请求。`rules` 是按 Maven 相对请求路径匹配的有序 glob：第一条匹配规则决定该仓库是否参与，`!` 表示排除，`*` 仅匹配单个路径段，`**` 可以跨 `/` 匹配多级目录。省略 `rules` 的仓库作为全局 fallback。

仓库 id 用于日志、统计和熔断状态识别，只能包含小写 ASCII 字母、数字、下划线和连字符（例如 `central`、`kikugie-releases`、`gradle-plugin`）。

当某个上游没有文件、请求失败，或反复返回无法通过校验且内容不同的文件时，服务会在适用的情况下继续尝试后续上游。

## 接入构建工具

Maven Haste 不修改构建工具或项目文件。请复制并按自己的环境修改相应示例：

- [Gradle init script](config-examples/gradle.init.gradle)，覆盖插件管理、buildscript 依赖和项目依赖。
- [Maven settings](config-examples/maven-settings.xml)，将外部仓库镜像到 Maven Haste。
- [Minecraft 上游配置](config-examples/minecraft.toml)，为常用加载器仓库提供路径路由。

Gradle 示例会把 Maven Haste 添加到项目声明的仓库之前，并保留原有仓库作为 fallback。项目可以实施自己的仓库策略，
因此应先检查和调整脚本，再将其安装为全局配置。

## 细节

### 缓存语义

- 固定版本、时间戳快照及其校验和成功下载后会持久保存。
- 将仓库的 `cache_writes` 设为 `false` 可停止写入来自该仓库的新产物、校验和与负缓存条目。之前已缓存的内容仍会继续提供，且不会删除任何文件；先前确认的 404 在负缓存 TTL 过期前仍会直接返回。
- `maven-metadata.xml` 和 `-SNAPSHOT` 别名属于可变内容。缓存过期后，已有内容可以立即返回，刷新在后台进行。
- 启用 stale-on-error 时，后台刷新遇到上游故障仍可继续提供旧内容。
- 仅可变文件的上游 404 会被短暂记忆，减少对不存在文件的重复请求。
- 上游校验和与下载内容不一致时，服务会通过重试区分不稳定下载和错误校验和。稳定内容会在记录警告后被接受；反复变化且无法验证的内容会被丢弃并尝试其他来源。缓存的 `.sha1`、`.sha256` 和 `.sha512` 始终根据实际保存的字节计算。

上游请求受全局并发上限和单仓库并发上限共同约束；调度器会让首次下载的请求优先，同时定期为缓存刷新留出机会。

### HTTP 接口

本地 Maven endpoint 由 `[server].base_path` 决定，默认是 `/maven`。根路径和 `/api` 保留给服务自身，不可配置为 Maven endpoint。

- `GET /api/v1/health`：检查 SQLite、缓存目录和临时目录；健康时返回 `200 OK`，否则返回服务不可用。
- `GET /api/v1/cache/stats`：返回缓存文件数量、总大小、命中率、负缓存数量和各上游熔断状态。

这两个接口适合接入容器或进程管理器的存活/就绪检查。缓存统计适合用于低频监控，不建议在每个请求中轮询。

Maven endpoint 支持 `GET`、`HEAD`、单段字节范围、客户端条件验证和缓存控制响应头。固定构件允许客户端按不可变内容
缓存；可变元数据需要向本地代理重新验证。

### 日志

使用 `RUST_LOG` 调整日志级别，例如：

```bash
RUST_LOG=maven_haste=debug maven-haste run -c ./maven-haste.toml
```

也可以使用 `--verbose` 快速启用调试日志；若同时设置 `RUST_LOG`，环境变量优先。

文件日志默认关闭。在 `[logging]` 中设置 `enabled = true` 开启；默认目录为 `<root>/.maven-haste/logs`。

`logging.filter` 使用 [`tracing-subscriber` EnvFilter 指令语法][env-filter-directives]。

## 兼容性项目

[test-projects](test-projects/README.md) 中包含最小 Maven、Gradle、Fabric、Forge 和 NeoForge 项目。它们用于人工兼容性
及性能检查，不属于离线 Rust 测试套件。普通 Gradle 项目组成一个根多项目构建，Stonecutter 项目则保持独立并管理自己的版本子项目。
所有 Gradle 项目共享 `test-projects/` 下的一份 Wrapper。其中的代表性 Fabric 项目参考了
[TerraformersMC/ModMenu](https://github.com/TerraformersMC/ModMenu) 的构建结构。

[env-filter-directives]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
