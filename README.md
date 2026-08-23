# Maven Haste

Maven Haste 是本地 Maven 仓库代理缓存服务。固定版本文件会永久缓存；`maven-metadata.xml` 和 `-SNAPSHOT` 别名使用 stale-while-revalidate，并支持负缓存、校验和验证、按请求路径路由和上游熔断。

## 开始使用

从 [Releases](https://github.com/Leawind/maven-haste/releases) 下载可执行文件，或自行构建后运行：

```bash
maven-haste --check -c ./maven-haste.toml
maven-haste -c ./maven-haste.toml
```

`--check` 会检查配置和存储目录后退出；不指定 `-c/--config` 时，程序会依次在当前目录和系统用户配置目录中查找 `maven-haste.toml`。配置中的相对存储路径始终相对于配置文件所在目录解析。

将 [maven-haste.example.toml](./maven-haste.example.toml) 复制为 `maven-haste.toml` 后，按需修改缓存目录、监听地址与上游仓库。仓库 `rules` 是有序的请求路径 glob：首条匹配规则决定该仓库是否参与，`!` 表示排除，`*` 可以跨 `/` 匹配；省略 `rules` 表示全局 fallback。

`[upstream]` 中的 `connect_timeout` 限制建立上游连接的时间，`read_timeout` 限制每次响应体读取允许的空闲时间；每收到一个数据块，读取计时就会重置，因此不会限制大文件的总下载时长。旧的 `cache.refresh_timeout` 已被删除，升级配置时请改用这两个字段。

## Gradle 接入

在 `~/.gradle/init.d/maven-haste.gradle`（Windows 为 `%USERPROFILE%\.gradle\init.d\maven-haste.gradle`）中加入：

```gradle
allprojects {
    buildscript.repositories {
        maven {
            url = uri('http://127.0.0.1:8080/maven')
            allowInsecureProtocol = true
        }
    }
    repositories {
        maven {
            url = uri('http://127.0.0.1:8080/maven')
            allowInsecureProtocol = true
        }
    }
}
```

代理仓库会被置于项目已有仓库之前。若 Maven Haste 未启动，Gradle 会连接失败并继续尝试后续仓库。

## 缓存行为

- 固定版本、时间戳快照及其校验和文件首次下载后永久缓存。
- `maven-metadata.xml` 与 `-SNAPSHOT` 别名会在 TTL 到期后立即返回旧缓存，并在后台刷新。
- 上游未提供 `.sha1` 或 `.sha256` 时，服务会计算并生成对应文件。
- 仅可变文件的上游 404 会被短暂负缓存，避免重复请求。

## 运维

- `GET /__health`：检查 SQLite 连接以及缓存和临时目录，健康时返回 `200 OK`。
- `GET /__cache/stats`：返回缓存文件数量和大小、命中率、负缓存数量及上游熔断状态。

使用 `RUST_LOG` 调整日志级别，例如 `RUST_LOG=maven_haste=debug`。服务收到 Ctrl-C 后会优雅停止接受请求。
