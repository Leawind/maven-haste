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
- 使用 GitHub Packages 的容器镜像运行
  ```bash
  docker run -d -p 8080:8080 \
      -v maven-haste-cache:/var/lib/maven-haste \
      -v ./config:/etc/maven-haste \
      ghcr.io/leawind/maven-haste
  ```

  镜像期望在挂载的 `/etc/maven-haste` 目录中放置 `maven-haste.toml`（或 `.json`/`.yaml`/`.yml`），缓存产物保存在 `/var/lib/maven-haste` 下；请在配置中将 `[storage].root` 指向该目录。镜像提供 `linux/amd64` 与 `linux/arm64` 版本，以非特权用户运行，并通过 HTTP 报告容器健康状态。容器内需设置 `[server].bind = "0.0.0.0:8080"`，否则映射出的端口不可访问；内置健康检查使用 8080 端口。发布压缩包同时提供 SHA-256 校验文件。

## 跑起来

在当前工作目录生成配置模板（不会覆盖已有文件）

```bash
maven-haste config init [--format json|yaml|toml] [PATH]
```

使用当前工作目录下的配置启动；`maven-haste.json`、`maven-haste.toml`、`maven-haste.yaml`、`maven-haste.yml` 会被自动发现

```bash
maven-haste run
```

可以通过命令行参数 `-c --config <PATH>` 指定配置文件路径。

启动前验证配置和存储目录：

```bash
maven-haste config check
```

维护本地缓存：重新校验每个缓存产物的尺寸与校验和，删除某个路径前缀下的全部缓存内容，打印缓存总量，按清单预取，或从损坏的数据库中恢复

```bash
maven-haste cache verify
maven-haste cache remove-prefix com/example
maven-haste cache stats
maven-haste cache stats --json
maven-haste cache warm ./dependencies.txt
maven-haste cache repair
```

`cache verify` 发现问题时会逐条列出并以错误码退出。`cache warm` 的清单每行一个 Maven 相对路径，空行和以 `#` 开头的行会被忽略；任一条目获取失败时命令返回错误。`cache repair` 会把损坏的 SQLite 数据库隔离保存并新建数据库；已有内容对象仍保留在磁盘上，需要时重新获取。

> [!WARNING]
>
> Maven endpoint 不认证客户端。请保持默认的回环监听地址，或将服务放在限制访问的反向代理或防火墙之后。不要把私有产物的缓存暴露到不可信网络。

设置 `[upstream].offline = true` 后只提供缓存内容；未缓存的路径会直接返回 `503 Service Unavailable`，不会访问上游。

## 配置

> [!TIP]
>
> 运行 `maven-haste config init [PATH]` 生成只含必需键的最小配置；语言（TOML/YAML/JSON）随 `PATH` 的扩展名决定，默认生成 YAML（`./maven-haste.yaml`），也可以用 `--format json|yaml|toml` 显式指定。各配置项的字段说明见下方列出的 JSON Schema。

推荐步骤：

1. 在 `[[repositories]]` 中配置上游 Maven 仓库，需要时设置认证或自定义请求头。
2. 根据需要调整 `[server]` 的监听地址和本地 endpoint 前缀。
3. 设置 `[storage]` 的存储根目录。相对路径视为相对于配置文件所在目录。
4. 按网络环境调整缓存 TTL、上游超时、并发和熔断参数。
5. 用 `maven-haste config check -c <配置文件>` 验证配置、存储目录和数据库，再启动服务。

程序未指定 `-c/--config` 时，会依次查找当前目录和系统用户配置目录中的 `maven-haste.json`、`maven-haste.toml`、`maven-haste.yaml`、`maven-haste.yml`。同一目录下存在多个配置，或两个目录中存在不同格式的配置时，程序会报错并要求用 `--config` 明确指定，而不是自行选择；两个目录中可以存在相同格式的配置，此时优先使用当前目录。解析器由文件扩展名决定（`json`、`yaml`、`yml`、`toml`）；无法识别或缺失的扩展名会直接报错，不再回退为 TOML。启动时可以用全局参数临时覆盖监听地址或启用调试日志；长期配置应写入配置文件。

### 热重载

`maven-haste run` 会持续监视启动时使用的配置文件。文件变化后，程序按扩展名重新解析新内容，并在约一秒内应用：

- 可热重载：`[cache]`、`[upstream]`、`[circuit_breaker]` 和 `[[repositories]]`，包括仓库 URL、路由规则、认证、自定义请求头、代理设置、并发数、`cache_writes` 以及 `[upstream].offline`。
- 需要重启：`[server]`（`bind` 和 `base_path`）、`[storage]` 与 `[logging]`。这些部分的变更会记录警告并被忽略。
- 新文件无法解析、校验失败，或无法根据新配置创建上游客户端时，程序会记录错误并继续使用旧配置。
- 重载不会中断正在进行的下载；新请求使用重载后的配置。
- `${ENV_VAR}` 会在每次重载时重新展开；修改环境变量后，需要保存或 touch 配置文件才会触发重载。

只有 `run` 会监视配置文件；`config` 和 `cache` 命令只读取一次配置然后退出。

### 借助 Schema 编辑

描述整个配置的 JSON Schema 位于 [maven-haste.schema.json](maven-haste.schema.json)，也可用 `maven-haste config schema` 输出（`-o <PATH>` 写入文件）。`config init` 按指定语言（`PATH` 的扩展名、`--format` 参数，默认 YAML）生成最小配置，其 `$schema` 键已默认启用且 URL 固定指向当前发布版本，因此编辑器或 AI 工具无需任何编辑与 IDE 设置即可获得补全、字段说明、默认值和校验提示：

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

当某个上游没有文件、请求失败，或重试后仍无法通过校验时，服务会在适用的情况下继续尝试后续上游。重定向仅允许跳转到已配置仓库 URL 明确列出的来源（协议、主机和端口）。

元数据文档按候选仓库顺序取第一个成功响应并缓存。maven-haste 不会合并多个仓库的 `maven-metadata.xml`；如果动态版本或版本范围必须看到全部版本，请把同一类产物路由到已经聚合了仓库的上游，或在构建工具中保留原始仓库配置。

### 上游认证与 TLS

每个仓库都可以发送自定义请求头、HTTP Basic 凭据或 Bearer token：

```toml
[[repositories]]
id = "company"
url = "https://repo.example.com/maven/"
username = "build-user"
password = "${MAVEN_HASTE_REPO_PASSWORD}"
headers = { "X-Api-Key" = "${MAVEN_HASTE_API_KEY}" }
```

`username`、`password`、`bearer_token` 和请求头的值支持 `${ENV_VAR}` 展开。Basic 凭据和 Bearer token 不能同时设置。控制 HTTP 交换本身的请求头（`Host`、`Content-Length`、`Range`、`If-*`、`Connection`、`Transfer-Encoding` 和 `Proxy-Authorization`）不能被覆盖。`maven-haste config show` 会脱敏凭据和请求头的值，日志中的代理凭据也会脱敏。

如果上游或代理使用私有证书颁发机构，可将 `[upstream].ca_bundle` 指向包含额外根证书的 PEM 或 DER 文件。

### 上游代理

在 `[upstream]` 中设置 `proxy` 可以让所有请求走一个或多个 HTTP 代理；接受单个 URL 或有序列表，代理按列出顺序依次尝试。仓库上的 `use_proxy` 会覆盖该选择：`true` 始终使用代理，`false` 始终直连。某个代理不可达（连接被拒、DNS 失败或连接超时）时，maven-haste 会把请求交给下一个代理，最后才回退为直连，而不是直接失败。连续 `circuit_breaker.failure_threshold` 次失败后，该代理会被跳过 `circuit_breaker.recovery_timeout`，随后再探测一次，因此代理挂掉不会拖慢每一次下载。设置 `proxy_fallback = false` 可以保持必须使用代理。

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
- 后台刷新遇到上游故障时，仍会继续提供旧的缓存内容。
- 仅可变文件的上游 404 会被短暂记忆，减少对不存在文件的重复请求。设置 `cache.negative_cache_permanent = true` 后，固定版本路径也会使用负缓存 TTL。
- 每次缓存响应（包括 `HEAD` 和条件请求）都会先验证文件大小和记录中最强的摘要。损坏的文件会重新下载；无法从上游修复时返回错误。访问计数在内存中批量写入 SQLite。
- 未缓存产物的 `HEAD` 请求会用 `HEAD` 探测上游，而不是下载正文；生成的校验和仍通过源产物解析。
- 上游校验和与下载内容不一致，或因上游故障无法取得校验和时，服务会重试并尝试其他来源。重复得到相同内容或重试失败都不会放行校验冲突。只有明确的校验和 404 才允许生成缺失的校验和。缓存的 `.md5`、`.sha1`、`.sha256` 和 `.sha512` 根据实际保存的字节计算。
- 单个产物包大于 `cache.max_size` 时直接提供而不写入缓存，避免一个超大下载突破容量上限。

新缓存文件按 SHA-256 保存为 `<root>/.maven-haste/objects` 下的内容对象。整组文件保存后，通过一次 SQLite 事务发布主文件和校验和记录；发布失败时仍可使用旧的一组内容。刷新期间，正在发送的响应继续使用已经验证的文件句柄。旧版 Maven 目录布局缓存仍可读取，无需数据库迁移。维护会在宽限期后清理未引用的对象及已被替代的旧版文件；正在响应中使用的对象按哈希跟踪。临时目录中的未完成下载只有在 24 小时后才会被清理，因此 CLI 命令或第二个服务进程不会删掉正在进行的下载。每次命中先完整读取文件进行校验，再流式发送，因此会增加本地磁盘 I/O。

上游请求受全局并发上限和单仓库并发上限共同约束；调度器会让首次下载的请求优先，同时定期为缓存刷新留出机会。校验和文件与主下载并行获取。下载中断时，仅在存在强 ETag 的情况下使用 `Range` 和 `If-Range` 续传。缺少验证器、验证器为弱 ETag、ETag 改变或上游忽略范围请求时，会从头下载；不一致的部分范围响应会被拒绝。最多尝试三次正文传输，仍失败才跳过该仓库。刷新生成的校验和时，会刷新主文件及整组校验和。

缓存大小上限在每次写入后执行，同时由周期性的后台维护（`cache.maintenance_interval`，默认 1 分钟）执行；该维护还会清理过期的负缓存条目和长期未完成的临时下载。`cache stats` 同时报告逻辑产物大小和去重后的内容对象数量。

### HTTP 接口

本地 Maven endpoint 由 `[server].base_path` 决定，默认是 `/maven`。根路径和 `/api` 保留给服务自身，不可配置为 Maven endpoint。

- `GET /api/v1/health`：检查 SQLite、缓存目录和临时目录；健康时返回 `200 OK`，否则返回服务不可用。
- `GET /api/v1/cache/stats`：返回缓存文件数量、逻辑大小与内容对象大小、累计请求数、命中率、负缓存数量和各上游熔断状态。
- `GET /api/v1/metrics`：以 Prometheus 文本格式返回缓存总量、内容对象数量、请求与命中计数、发送字节数、中断响应数、响应与上游延迟直方图，以及各上游熔断状态。每个指标族只有一行 `HELP` 和一行 `TYPE`；计数指标使用 counter 类型。

这些接口适合接入容器或进程管理器的存活/就绪检查与指标抓取。缓存统计和指标适合用于低频监控，不建议在每个请求中轮询。

Maven endpoint 支持 `GET`、`HEAD`、单段字节范围、客户端条件验证和缓存控制响应头。固定构件允许客户端按不可变内容
缓存；可变元数据需要向本地代理重新验证。

### 日志

使用 `RUST_LOG` 调整日志级别，例如：

```bash
RUST_LOG=maven_haste=debug maven-haste run -c ./maven-haste.toml
```

也可以使用 `--verbose` 快速启用调试日志；若同时设置 `RUST_LOG`，环境变量优先。

文件日志默认关闭。在 `[logging]` 中设置 `enabled = true` 开启；默认目录为 `<root>/.maven-haste/logs`。

`logging.filter` 使用 [`tracing-subscriber` EnvFilter 指令语法][env-filter-directives]。日志中的代理 URL 会隐藏凭据。

## 兼容性项目

[test-projects](test-projects/README.md) 中包含最小 Maven、Gradle、Fabric、Forge 和 NeoForge 项目。它们用于人工兼容性
及性能检查，不属于离线 Rust 测试套件。普通 Gradle 项目组成一个根多项目构建，Stonecutter 项目则保持独立并管理自己的版本子项目。
所有 Gradle 项目共享 `test-projects/` 下的一份 Wrapper。其中的代表性 Fabric 项目参考了
[TerraformersMC/ModMenu](https://github.com/TerraformersMC/ModMenu) 的构建结构。

[env-filter-directives]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
