| [中文](README.zh.md) | English |
| -------------------- | ------- |

# Maven Haste

Maven Haste is a local Maven repository proxy cache. Fixed-version files are cached permanently; `maven-metadata.xml` and `-SNAPSHOT` aliases use stale-while-revalidate, with support for negative caching, checksum verification, per-path routing, and upstream circuit breaking.

## Getting Started

Download a binary from [Releases](https://github.com/Leawind/maven-haste/releases), or build it yourself and run:

```bash
maven-haste config init
maven-haste check -c ./maven-haste.toml
maven-haste run -c ./maven-haste.toml
```

`config init` creates a sample config with English comments in the current directory and never overwrites existing files; `config example` prints the same sample to the terminal. `check` validates the config and storage directory, then exits. Without `-c/--config`, the program looks for `maven-haste.toml` in the current directory, then in the system user config directory. Relative storage paths in the config are always resolved relative to the config file's directory.

Edit the generated config to adjust cache directories, listen addresses, and upstream repositories. Repository `rules` are ordered request-path globs: the first matching rule decides whether that repository participates, `!` means exclude, and `*` matches across `/`. Omitting `rules` makes the repository a global fallback.

In `[upstream]`, `connect_timeout` limits how long establishing an upstream connection may take, and `read_timeout` limits idle time allowed per response body read; the read timer resets on every chunk received, so it does not cap total download time for large files. The old `cache.refresh_timeout` has been removed — use these two fields instead when upgrading configs.

Upstream requests are bounded by a global `max_concurrency` and a per-repository default `default_repository_max_concurrency`; individual repositories can override the latter with their own `max_concurrency`. First-time downloads take priority over background cache refreshes, but both are queued and eventually executed; `foreground_priority_burst` controls how many first-time downloads are admitted before a cache refresh gets a turn under sustained load.

## Gradle Integration

Add the following to `~/.gradle/init.d/maven-haste.gradle` (on Windows: `%USERPROFILE%\.gradle\init.d\maven-haste.gradle`):

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

The proxy repository is placed before the project's existing repositories. If Maven Haste is not running, Gradle will fail to connect and fall through to the remaining repositories.

## Caching Behavior

- Fixed versions, timestamped snapshots, and their checksum files are cached permanently after first download.
- `maven-metadata.xml` and `-SNAPSHOT` aliases serve stale cached content immediately after TTL expiry and refresh in the background.
- When the upstream does not provide `.sha1` or `.sha256`, the service computes and generates the corresponding files.
- Only upstream 404s for mutable files are briefly negatively cached to avoid repeated requests.

## Operations

- `GET /api/v1/health`: checks the SQLite connection and cache/temp directories; returns `200 OK` when healthy.
- `GET /api/v1/cache/stats`: returns cached file count and size, hit rate, negative cache count, and upstream circuit breaker status.

Use `RUST_LOG` to adjust log levels, e.g. `RUST_LOG=maven_haste=debug`. The service gracefully stops accepting requests on Ctrl-C.

You can also enable debug logging at startup with `--verbose`, e.g. `maven-haste --verbose run -c ./maven-haste.toml`; if `RUST_LOG` is also set, the environment variable takes precedence.
