# Maven Haste 设计草图

## 项目概述

Maven Haste 是一个轻量级、无状态的本地 Maven 仓库代理缓存服务，专为 Minecraft 模组开发及跨平台（Windows/Ubuntu 双系统 NTFS 共享）场景设计。对 Gradle/Maven 表现为标准 Maven 仓库，核心特性包括：固定版本永久缓存且永不访问上游；可变文件采用 stale-while-revalidate 策略（立即返回旧缓存并后台静默刷新）；基于 `groupId:artifactId` 的精细 include/exclude 路由规则；上游故障轻量熔断；零项目侵入（通过用户手动配置 Gradle init script 接入）。使用 Rust 编写，编译为单二进制文件 `maven-haste`。

## 技术栈

- **语言**：Rust (stable)
- **异步运行时**：Tokio
- **HTTP 服务器**：Axum
- **HTTP 客户端**：Reqwest (异步、TLS、超时控制)
- **数据库**：rusqlite (存储元数据，禁用 WAL 模式以适配 NTFS 跨系统)
- **配置解析**：Serde + TOML
- **CLI**：Clap
- **日志**：Tracing + Tracing-subscriber
- **并发控制**：DashMap (single-flight 去重 + 内存熔断器)
- **文件系统**：tokio::fs (原子写入)

## 总体架构

Gradle/Maven 通过 HTTP (`127.0.0.1:8080`) 请求 Maven Haste。服务内部由 Axum 路由层接收请求，经路径解析器提取坐标与文件类型后交由缓存管理器处理。缓存管理器以 SQLite 数据库为元数据的唯一权威，结合物理文件系统进行缓存命中判断与数据流转。永久缓存命中直接返回；可变缓存命中则立即返回旧数据并触发后台刷新任务；未命中时由路由引擎根据规则筛选候选上游仓库，经熔断器过滤后由上游客户端拉取。所有构件按标准 Maven 布局存储于本地文件系统，元数据精准记录于 SQLite 数据库。

## CLI 与配置加载机制

采用极简的“单命令 + Flags”结构。

### CLI 接口设计

命令形式为 `maven-haste [OPTIONS]`，默认行为即为加载配置并启动 HTTP 服务。

**核心选项 (Options):**

- `-c, --config <PATH>`：显式指定配置文件路径。若指定但文件不存在或不可读，立即报错退出，不进行任何默认搜索。
- `--bind <ADDR>`：临时覆盖监听地址与端口（如 `0.0.0.0:8080`）。优先级高于配置文件，方便容器化部署或临时调试。
- `--check`：预检模式。仅加载、解析、校验配置并检查目录权限。通过后退出 (Exit 0)，不启动服务。
- `--print-config`：诊断模式。加载配置、填充默认值、将所有相对路径展开为绝对路径后，以 TOML 格式打印最终生效配置并退出。
- `-h, --help`：打印帮助信息。
- `-V, --version`：打印版本号。

注：`--check` 与 `--print-config` 互斥，同时指定视为 CLI 使用错误。

**退出码 (Exit Codes):**

- `0`：成功（服务正常退出，或预检/诊断成功）。
- `1`：运行时错误（如端口被占用、SQLite 崩溃、文件系统只读）。
- `2`：配置错误（如找不到配置文件、TOML 语法错误、缺少必需字段）。

### 配置文件定位策略

1. **显式指定**：`-c` 或 `--config` 提供的路径（相对路径基于当前工作目录 CWD）。
2. **约定路径**：按顺序查找，使用第一个存在的文件，不合并、不叠加：
   - 当前工作目录：`./maven-haste.toml`
   - 用户级配置目录：
     - Linux: `$XDG_CONFIG_HOME/maven-haste/maven-haste.toml` (或 `~/.config/...`)
     - Windows: `%APPDATA%\maven-haste\maven-haste.toml`

**未找到配置文件的处理**：
若所有位置均未找到配置文件，**立即报错退出 (Exit 2)**。错误信息会清晰列出尝试过的路径，并提示用户创建配置文件或使用 `-c` 指定。绝不提供隐式的默认 Fallback 配置，因为本服务的核心价值在于精确的路由规则。

### 内部路径解析基准 (核心亮点)

配置文件中涉及文件系统的路径（`storage.root`, `storage.tmp_dir`, `storage.db_path`）遵循以下解析规则：

- **绝对路径**：直接使用。
- **相对路径**：**基准目录是配置文件所在的物理目录**，而不是进程启动时的当前工作目录 (CWD)。

_示例_：假设配置文件位于 `D:/mcache/maven-haste.toml`，内容配置 `root = "./repository"`。无论用户在哪个系统目录下执行启动命令，`root` 都会被精准解析为 `D:/mcache/repository`。这彻底解决了跨平台、跨目录启动时的路径漂移问题，完美支持配置文件的便携式迁移。

## 启动流程

服务启动时按以下顺序初始化：

1. **CLI 解析与配置定位**：解析单命令 Flags，按策略定位配置文件。若未找到则报错退出。
2. **配置解析与路径展开**：解析 TOML，填充默认值。**以配置文件所在目录为基准**，将所有相对路径展开为绝对路径。
3. **配置校验**：校验语法、必需字段（`storage.root` 与 `[[repositories]]`）及路径合法性。若执行 `--check` 或 `--print-config`，在此步完成后直接退出。
4. **日志与覆盖项初始化**：初始化日志系统，应用 `--bind` 等 CLI 覆盖参数。启动后首行 INFO 日志打印实际加载的配置文件绝对路径。
5. **目录与临时文件清理**：创建或校验 `storage.root`、`storage.tmp_dir` 与 `storage.db_path` 所在目录。清理 `tmp_dir` 中所有遗留的 `.part` 临时文件。
6. **SQLite 初始化**：初始化连接池，执行 `PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;` 并让 SQLite 自动执行崩溃恢复。
7. **文件系统特性探测**：在 `root` 目录下创建两个仅大小写不同的临时文件探测大小写敏感性，记录结果供后续写入冲突处理使用，随后删除临时文件。
8. **启动 HTTP 服务器**。

## 配置设计

配置文件使用 TOML 格式。路由规则完全由有序数组驱动，未匹配任何规则的仓库视为可用（fallback），需排除时显式添加 `!*:*` 兜底。`storage.root` 和至少一个 `[[repositories]]` 为必需字段。

### 完整配置示例

```toml
[server]
bind = "127.0.0.1:8080"
base_path = "/maven"

[storage]
root = "D:/mcache/repository"
tmp_dir = "D:/mcache/.tmp"
db_path = "D:/mcache/maven-haste.db"

[cache]
metadata_ttl = "5m"        # 所有可变文件（metadata、快照别名）共用此 TTL
negative_ttl = "5m"        # 上游 404 负缓存 TTL
refresh_max_concurrency = 10
refresh_timeout = "10s"
serve_stale_on_error = true

[circuit_breaker]
failure_threshold = 3
recovery_timeout = "30s"

[[repositories]]
name = "fabric"
url = "https://maven.fabricmc.net/"
rules = [
    "net.fabricmc:*",
    "net.fabricmc.fabric-api:*",
    "!*:*"
]

[[repositories]]
name = "mojang"
url = "https://libraries.minecraft.net/"
rules = [
    "com.mojang:*",
    "net.minecraft:*",
    "!*:*"
]

[[repositories]]
name = "central"
url = "https://repo1.maven.org/maven2/"
# 省略 rules 字段等同于 rules = []，即作为全局 Fallback 接收所有未匹配的请求
```

## 路由规则详解

每个仓库包含 `name`、`url` 和有序 `rules` 字符串数组。规则采用极简的字符串语法：

- **无前缀**：表示 `include`（包含）。例如 `"net.fabricmc:*"`。
- **`!` 前缀**：表示 `exclude`（排除）。例如 `"!*:*"`。

规则必须始终书写完整的 `groupId:artifactId` 格式。若需匹配某 groupId 下的所有 artifact，应显式书写 `groupId:*`。

支持 `*` 通配符。匹配逻辑：对每个请求，按配置顺序遍历仓库；对每个仓库按序遍历其 `rules`，首条匹配的 rule 决定该仓库是否参与（无前缀=参与，`!`=不参与）并停止遍历该仓库的规则；若无任何 rule 匹配，则该仓库参与。所有参与的仓库按配置顺序组成候选列表。

通配符 `*` 匹配任意字符（含 `.`、`-`），使用简单迭代 glob 匹配。在配置加载阶段，解析器自动识别 `!` 前缀并将其转换为内部的 Exclude 动作，其余转换为 Include 动作。若规则字符串中缺少 `:` 分隔符，配置校验阶段将报错退出。

## 请求路径解析

Maven URL 格式为 `/{base_path}/{groupId路径}/{artifactId}/{version}/{filename}`，groupId 中的 `.` 转换为 `/`。解析器移除 base_path 前缀后，识别文件类型（`.pom`、`.jar`、`maven-metadata.xml`、`-SNAPSHOT.jar` 等），提取 groupId、artifactId、version。特殊处理 group 级和 artifact 级 `maven-metadata.xml`，以及快照别名文件。解析采用启发式规则，路由主要依赖 groupId 前缀。所有提取出的坐标字符串严格保留原始大小写。

## 缓存管理

### 目录结构

缓存目录为标准 Maven 仓库布局，物理路径严格保留从上游或请求中解析出的原始大小写：

```
D:/mcache/repository/
├── com/example/foo/1.0/foo-1.0.jar
├── com/example/foo/1.0/foo-1.0.jar.sha1
├── net/fabricmc/loader/0.15.0/loader-0.15.0.pom
└── ...
```

所有文件路径在 SQLite 中使用相对 `root` 的路径表示，统一使用正斜杠 `/`。

### 临时文件与原子写入

下载过程先写入 `tmp_dir/<uuid>.part`，完成后校验 SHA1/SHA256（若上游提供或本地计算），再原子 `rename` 到最终缓存路径。`tmp_dir` 必须与 `root` 位于同一文件系统以保证 rename 原子性。

### SQLite 元数据表设计

SQLite 数据库是缓存状态的唯一权威，物理文件是数据库记录的具象化载体。

```sql
CREATE TABLE artifacts (
    path TEXT PRIMARY KEY,          -- 相对 root 的路径，严格大小写敏感（Binary collation）
    group_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    version TEXT NOT NULL,
    file_type TEXT NOT NULL,        -- 如 'pom', 'jar', 'module', 'sha1', 'metadata', 'snapshot_alias', 'negative' 等
    upstream TEXT NOT NULL,         -- 首次成功下载的来源仓库名
    sha1 TEXT,
    sha256 TEXT,
    etag TEXT,
    last_modified TEXT,
    file_size INTEGER,              -- 文件字节数
    created_at INTEGER NOT NULL,    -- Unix 时间戳
    last_refresh_attempt INTEGER,   -- 上次尝试刷新的时间戳
    is_not_found INTEGER DEFAULT 0  -- 1 表示负缓存（上游 404）
);

-- 坐标索引，支持高效查询
CREATE INDEX idx_group ON artifacts(group_id);
CREATE INDEX idx_group_artifact ON artifacts(group_id, artifact_id);
CREATE INDEX idx_artifact_version ON artifacts(artifact_id, version);

-- 忽略大小写索引，仅在文件系统大小写不敏感时用于写入前冲突检测的候选查询加速
CREATE INDEX idx_path_nocase ON artifacts(path COLLATE NOCASE);
```

- `path` 为主键，使用相对路径，严格区分大小写。
- `idx_path_nocase` 索引作为写入前冲突检测的候选查询加速手段。是否执行冲突检测与清理由启动时文件系统大小写敏感性探测结果决定：仅在探测结果为大小写不敏感时启用；在大小写敏感文件系统上不使用该冲突清理逻辑。
- 坐标拆分为独立列并建立索引，支持高效按 groupId/artifactId 查询、统计和清理。
- `file_type` 用于快速识别文件类别，辅助缓存策略判断。
- `is_not_found` 用于负缓存，记录上游 404 状态，避免重复穿透。

### 文件分类与缓存策略

**永久缓存**：固定版本的 `.pom`、`.jar`、`.module`、`.sha1`、`.sha256`、`.md5`（若上游提供）及快照时间戳文件。一旦缓存，永不访问上游，不设置 TTL。

**可变文件**：`maven-metadata.xml` 及快照别名文件（如 `foo-1.0-SNAPSHOT.jar`）。有缓存时立即返回旧数据，若 `last_refresh_attempt` 距今超过 `metadata_ttl` 且无正在进行的刷新任务，则触发后台刷新；无缓存时同步拉取。上游返回 404 时，记录负缓存（`is_not_found = 1`），在 `negative_ttl` 内直接返回 404，不访问上游。

**负缓存**：仅对可变文件 404 生效。负缓存记录在 SQLite 中，过期后自动失效，届时可重新尝试上游。

**校验和文件**：主文件下载完成后，尝试从上游下载对应的 `.sha1`、`.sha256` 文件；若上游未提供，则本地计算生成并原子写入。校验和文件与主文件采用相同的缓存策略（永久或可变），并一同原子替换以保证一致性。

### 核心一致性规则

系统确立 **SQLite 数据库为元数据的唯一权威**，物理文件与数据库记录保持严格的单向映射关系。

- **写入与冲突处理（后浪推前浪）**：

  服务在启动时已检测并记录文件系统大小写敏感性。当从上游下载完成，准备将临时文件写入最终缓存路径时：

  - **大小写不敏感文件系统**：执行严格冲突清理流程。
    1. **定位候选冲突**：执行查询 `SELECT path FROM artifacts WHERE path = ? COLLATE NOCASE AND path != ?`。
    2. **清理旧状态**：若查询到仅大小写不同的旧记录，从 SQLite 中删除该旧记录，并从文件系统中删除对应的旧物理文件。
    3. **原子写入**：将临时文件 `rename` 到最终目标路径。
    4. **记录元数据**：向 SQLite 中插入新路径的记录。

  - **大小写敏感文件系统**：跳过冲突检测与旧状态清理，直接执行原子写入与元数据记录。不同大小写的路径可以共存，互不干扰。

- **读取与命中判断**：
  收到请求后，提取精确大小写的路径，直接查询 SQLite。
  - **若 DB 命中**：检查物理文件是否存在。若存在则返回；若物理文件丢失，则视为**缓存未命中**，触发重新下载流程。
  - **若 DB 未命中**：即使物理文件存在（如外部工具手动放入），也视为**缓存未命中**，走正常的路由下载流程。

## 请求处理流程

1. 解析路径得到坐标与精确大小写的文件路径。
2. 查询 SQLite 数据库获取元数据。
3. 根据文件分类走对应逻辑：
   - 永久文件：DB 命中且物理文件存在则直接返回；否则路由+下载+冲突检测+原子写入+记录元数据后返回；全部上游失败返回 404。
   - 可变文件：按缓存策略与 stale-while-revalidate 处理（见文件分类），包括负缓存检查、后台刷新触发、同步拉取及上游失败时的兜底。
4. 全部上游失败时，按错误处理策略返回对应状态码（见上游客户端与熔断器）。

## 后台刷新任务

- 触发条件：可变文件在 DB 中已缓存、已过期、无相同文件的刷新任务在执行（single-flight via DashMap）、并发数未超限。
- 执行流程：
  1. 标记刷新中
  2. 从 SQLite 读取 `upstream`、etag、last_modified
  3. **优先向记录的 `upstream` 发起条件请求**（携带 `If-None-Match`/`If-Modified-Since`）；若该上游失败，则重新路由确定候选列表并依次尝试
  4. 304 则仅更新 `last_refresh_attempt`
  5. 200 则执行冲突检测、原子写入新文件并更新 SQLite（同时更新校验和文件）
  6. 404 则记录负缓存
  7. 5xx 按错误处理策略处理
  8. 清除刷新标记。刷新失败不删除旧缓存，仅记录日志。

## 上游客户端与熔断器

使用 reqwest 发送 GET 请求，支持条件请求头、超时配置、环境变量认证。404 视为未找到继续下一上游；401/403 记录警告；5xx/超时触发重试后进入熔断逻辑。
**轻量熔断器**：内存中用 `DashMap<String, CircuitState>` 维护每个上游状态（Closed/Open/HalfOpen）。连续 `failure_threshold` 次失败后切为 Open，记录时间戳；后续请求直接跳过；`recovery_timeout` 后切为 HalfOpen，放行一个请求试探，成功恢复 Closed，失败重置 Open 计时。熔断状态不持久化。

错误处理策略：

- 上游 404：继续下一候选仓库；若为可变文件且所有上游均 404，记录负缓存并返回 404。
- 上游 401/403：记录警告，无其他仓库时返回 502。
- 上游 5xx/超时：重试 1-2 次后触发熔断，继续下一仓库。
- 所有上游失败：永久文件返回 404；可变文件若 `serve_stale_on_error=true` 且有旧 DB 记录则返回旧缓存，否则若上游 404 返回 404，其他错误返回 502。
- 配置错误：启动时校验，失败则退出并提示。

## 并发与数据一致性

- Single-flight：`DashMap<String, tokio::sync::broadcast::Sender<Result<()>>>` 去重后台刷新与首次下载。
- 文件读取：`tokio::fs` 异步非阻塞。
- 写入：临时文件 + rename 原子操作。
- SQLite：连接池 + `PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;` 禁用 WAL，并利用 `synchronous=OFF` 提升 NTFS 写入性能。鉴于缓存数据可完全重建，数据库损坏风险可接受；若需更高安全性可配置为 `NORMAL`。启动时让 SQLite 自动执行崩溃恢复。
- 批量事务：后台刷新批量更新元数据时使用 `BEGIN IMMEDIATE; ... COMMIT;` 合并写入，减少 fsync 次数。
- 跨系统并发：Windows 与 Ubuntu 不同时运行，仅需处理单系统内多 Gradle 进程并发，上述机制已足够。

## Gradle 集成

用户手动创建 `~/.gradle/init.d/maven-haste.gradle`（Linux）或 `%USERPROFILE%\.gradle\init.d\maven-haste.gradle`（Windows），内容如下：

```gradle
allprojects {
    buildscript.repositories {
        maven {
            url = uri("http://127.0.0.1:8080/maven")
            allowInsecureProtocol = true
        }
    }
    repositories {
        maven {
            url = uri("http://127.0.0.1:8080/maven")
            allowInsecureProtocol = true
        }
    }
}
```

Init script 在 Gradle 生命周期早期执行，保证代理仓库位于所有项目 repositories 最前面。代理未启动时 connection refused 瞬间失败，Gradle 自动 fallback 到后续仓库。

## 跨平台与 NTFS 设计

- **路径规范**：配置中使用 `/` 作为分隔符，Rust PathBuf 自动转换。Windows 根目录如 `D:/mcache`，Linux 如 `/mnt/d/mcache`。`tmp_dir` 与 `root` 同文件系统。
- **长路径**：Windows 侧建议开启 LongPathsEnabled 注册表项，缓存根目录尽量短（如 `D:/mh`）。代码中 Windows 平台可使用 `\\?\` 前缀。
- **SQLite 跨系统**：禁用 WAL，启动时让 SQLite 自动恢复。`synchronous=OFF` 提升性能，数据库可重建。
- **时间戳**：统一使用 Unix 时间戳（INTEGER），避免双系统时钟差异导致 TTL 计算错误。

## 日志与监控

使用 tracing 输出结构化日志，包含请求路径、缓存命中状态、上游名称、状态码、耗时。健康检查端点：`GET /__health` 返回 200 OK；`GET /__cache/stats` 返回缓存文件数、总大小、命中率、各上游熔断状态等 JSON 统计信息。

## 明确的不做事项 (Non-Goals)

为了保持项目的轻量、纯粹与行为的可预测性，本设计明确拒绝引入以下机制：

1. **无配置热重载 (Hot Reload)**：配置变更必须重启服务。避免引入文件系统监听带来的复杂性和跨平台兼容问题。
2. **无环境变量配置映射**：不提供类似 `MAVEN_HASTE_SERVER_BIND` 的环境变量覆盖，仅保留 `RUST_LOG` 控制底层日志库行为，避免配置优先级混乱。
3. **无配置文件继承/Include**：不支持 `include = "base.toml"`，保持单一文件的简单性。
4. **无隐式默认上游**：不内置任何默认的 Maven Central 仓库，强制用户显式声明路由规则，确保缓存行为完全符合预期。
5. **无子命令体系**：不引入 `maven-haste clean` 等子命令，保持 CLI 的极简形态。

## 潜在风险与缓解

- **Maven 协议复杂性**（快照、元数据、校验和）：参考 Maven Resolver 实现，逐步覆盖，优先保证 MC 模组常用场景。
