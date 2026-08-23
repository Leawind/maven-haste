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

使用当前工作目录下的配置文件 `maven-haste.toml` 启动

```bash
maven-haste run
```

可以通过命令行参数 `-c --config <PATH>` 指定配置文件路径。

## 配置

> [!TIP]
>
> 配置文件模板和所有配置项说明见 [maven-haste.example.toml](https://github.com/Leawind/maven-haste/blob/main/maven-haste.example.toml)。

推荐步骤：

1. 在 `[[repositories]]` 中配置上游 Maven 仓库。
2. 根据需要调整 `[server]` 的监听地址和本地 endpoint 前缀。
3. 设置 `[storage]` 的存储根目录。相对路径视为相对于配置文件所在目录。
4. 按网络环境调整缓存 TTL、上游超时、并发和熔断参数。
5. 用 `maven-haste check -c <配置文件>` 验证配置和存储目录，再启动服务。

程序未指定 `-c/--config` 时，会依次查找当前目录和系统用户配置目录中的 `maven-haste.toml`。启动时可以用全局参数临时覆盖监听地址或启用调试日志；长期配置应写入 TOML 文件。

### 上游路由

仓库按配置顺序参与请求。`rules` 是按 Maven 相对请求路径匹配的有序 glob：第一条匹配规则决定该仓库是否参与，`!` 表示排除，`*` 仅匹配单个路径段，`**` 可以跨 `/` 匹配多级目录。省略 `rules` 的仓库作为全局 fallback。

当某个上游没有文件、请求失败，或反复返回无法通过校验且内容不同的文件时，服务会在适用的情况下继续尝试后续上游。仓库名称用于日志、统计和熔断状态识别。

## 接入 Gradle

在 `~/.gradle/init.d/maven-haste.gradle`（Windows 为 `%USERPROFILE%\.gradle\init.d\maven-haste.gradle`）中加入：

```gradle
allprojects {
    buildscript.repositories {
        maven {
            url = uri("${maven_haste_url}")
            allowInsecureProtocol = true
        }
    }
    repositories {
        maven {
            url = uri("${maven_haste_url}")
            allowInsecureProtocol = true
        }
    }
}
```

将其中的 `${maven_haste_url}` 替换为你部署的 maven-haste 的 URL，例如 `http://127.0.0.1:8080/maven`。

代理仓库会被置于项目已有仓库之前。若 Maven Haste 未启动，Gradle 会连接失败并继续尝试后续仓库。

## 细节

### 缓存语义

- 固定版本、时间戳快照及其校验和成功下载后会持久保存。
- `maven-metadata.xml` 和 `-SNAPSHOT` 别名属于可变内容。缓存过期后，已有内容可以立即返回，刷新在后台进行。
- 启用 stale-on-error 时，后台刷新遇到上游故障仍可继续提供旧内容。
- 仅可变文件的上游 404 会被短暂记忆，减少对不存在文件的重复请求。
- 上游校验和与下载内容不一致时，服务会通过重试区分不稳定下载和错误校验和。稳定内容会在记录警告后被接受；反复变化且无法验证的内容会被丢弃并尝试其他来源。缓存的 `.sha1` 和 `.sha256` 始终根据实际保存的字节计算。

上游请求受全局并发上限和单仓库并发上限共同约束；调度器会让首次下载的请求优先，同时定期为缓存刷新留出机会。

### HTTP 接口

本地 Maven endpoint 由 `[server].base_path` 决定，默认是 `/maven`。根路径和 `/api` 保留给服务自身，不可配置为 Maven endpoint。

- `GET /api/v1/health`：检查 SQLite、缓存目录和临时目录；健康时返回 `200 OK`，否则返回服务不可用。
- `GET /api/v1/cache/stats`：返回缓存文件数量、总大小、命中率、负缓存数量和各上游熔断状态。

这两个接口适合接入容器或进程管理器的存活/就绪检查。缓存统计适合用于低频监控，不建议在每个请求中轮询。

### 日志

使用 `RUST_LOG` 调整日志级别，例如：

```bash
RUST_LOG=maven_haste=debug maven-haste run -c ./maven-haste.toml
```

也可以使用 `--verbose` 快速启用调试日志；若同时设置 `RUST_LOG`，环境变量优先。

默认配置下日志不输出到文件，可在 `[logging.file]` 中配置日志保存位置、保留期限等。
