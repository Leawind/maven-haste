| [中文](README.zh.md) | English |
| -------------------- | ------- |

# Maven Haste

A lightweight, self-hostable local caching proxy for Maven repositories.

It sits between your build tool and upstream Maven repositories: on first request, dependencies are downloaded from upstream and stored locally; subsequent requests are served from cache. Ideal for personal dev machines, LAN build servers, and CI, reducing repeated downloads and improving build reliability on unstable networks.

- Multi-upstream fallback with path-based routing
- Persistent caching for release versions and timestamped snapshots
- Background refresh of expired mutable files
- Negative caching for mutable-file 404s, upstream circuit breaking, and concurrency control

## Getting Started

maven-haste ships as a single command-line tool.

- Build and install from [crates.io](https://crates.io/crates/maven-haste)
  ```bash
  cargo install maven-haste
  ```
- Build and install from [source](https://github.com/Leawind/maven-haste)
  ```bash
  git clone https://github.com/Leawind/maven-haste
  cd maven-haste
  ```
  ```bash
  cargo install --path .
  ```
- Download from [GitHub Releases](https://github.com/Leawind/maven-haste/releases)

## Running

Generate a config template in the current working directory (never overwrites existing files)

```bash
maven-haste config init [PATH]
```

Start using the config in the current working directory; `maven-haste.json`, `maven-haste.toml`, or `maven-haste.yaml` is discovered automatically

```bash
maven-haste run
```

Use `-c --config <PATH>` to specify a config file path.

## Configuration

> [!TIP]
>
> Run `maven-haste config init [PATH]` to create a minimal configuration with only the required keys; the format (TOML, YAML, or JSON) follows the extension of `PATH`, defaulting to `maven-haste.toml`. Field descriptions live in the JSON schema listed below.

Recommended steps:

1. Configure upstream Maven repositories under `[[repositories]]`.
2. Adjust the listen address and local endpoint prefix in `[server]` as needed.
3. Set the storage root in `[storage]`. Relative paths are resolved against the config file's directory.
4. Tune cache TTLs, upstream timeouts, concurrency, and circuit breaker parameters to match your network.
5. Run `maven-haste config check -c <config>` to validate the config and storage directory before starting.

Without `-c/--config`, maven-haste looks for `maven-haste.json`, `maven-haste.toml`, or `maven-haste.yaml` in the current directory and the system user config directory; if several of these files exist, it stops instead of picking one. The parser is chosen by the file extension (`json`, `yaml`, `yml`, or `toml`); an unsupported or missing extension is an error rather than a TOML fallback. Global flags can temporarily override the listen address or enable debug logging at startup; persistent settings belong in the config file.

### Schema-Assisted Editing

A JSON schema describing the whole configuration lives in [maven-haste.schema.json](maven-haste.schema.json) and is also printed by `maven-haste config schema` (use `-o <PATH>` to write it to a file). `config init` generates a minimal config in the format of the target path's extension, and its `$schema` key is already active and pinned to the current release, so editors or AI tools pick the schema up with no editing and no IDE settings:

```toml
"$schema" = "https://raw.githubusercontent.com/Leawind/maven-haste/vX.Y.Z/maven-haste.schema.json"
```

where `X.Y.Z` is the version printed by `maven-haste --version`; `config init` fills it in automatically.

The pinned URL points at the tag of the current release, not `main`, so it stays valid per released version (the reference becomes reachable once a release tag includes the schema; the tag of every later release is matchable exactly). Offline environments can instead generate a local copy next to the config once with `maven-haste config schema -o ./maven-haste.schema.json` and reference `./maven-haste.schema.json`.

Write the reference into the config file as:

- JSON: `"$schema": "<reference>"` at the top level.
- YAML: `$schema: <reference>` at the top level, or `# yaml-language-server: $schema=<reference>` as the first line.
- TOML: `"$schema" = "<reference>"` at the top level; the quotes are required by TOML and the Even Better TOML extension activates on this key.

maven-haste accepts and ignores the `$schema` key in any format, so annotated configs still pass `config check`. The schema is generated from the Rust configuration types (schemars): after changing the config structure, regenerate it with `maven-haste config schema -o maven-haste.schema.json` and commit it together with the change; a unit test fails when the committed schema is out of date.

### Upstream Routing

Repositories are tried in configuration order. `rules` is an ordered list of glob patterns matched against the Maven relative request path: the first matching rule determines whether the repository participates, `!` means exclude, `*` matches within one path segment, and `**` matches across `/`. Repositories without `rules` act as a global fallback.

Repository ids are used for logging, statistics, and circuit breaker identification, and must consist only of lowercase ASCII letters, digits, underscores, and hyphens (e.g. `central`, `kikugie-releases`, `gradle-plugin`).

When an upstream lacks a file, fails, or repeatedly returns different content that cannot be verified against its checksums, the service continues to the next upstream where applicable.

## Build Tool Integration

Maven Haste does not modify build-tool or project files. Copy and edit the relevant example for your environment:

- [Gradle init script](config-examples/gradle.init.gradle), covering plugin management, buildscript dependencies, and
  project dependencies.
- [Maven settings](config-examples/maven-settings.xml), mirroring external repositories through Maven Haste.
- [Minecraft upstream configuration](config-examples/minecraft.toml), with path routing for common loader repositories.

The Gradle example adds Maven Haste before repositories declared by the build. Existing repositories remain available as
fallbacks. A build may enforce its own repository policy, so inspect and adapt the script before installing it globally.

## Details

### Cache Semantics

- Release versions, timestamped snapshots, and their checksums are cached persistently after successful download.
- `maven-metadata.xml` and `-SNAPSHOT` aliases are mutable content. When expired, cached content is served immediately while refresh happens in the background.
- Set `cache_writes = false` on a repository to stop writing new artifacts, checksums, and negative entries from it. Content cached earlier is still served and nothing is deleted; previously confirmed 404s keep short-circuiting until their negative TTL expires.
- With stale-on-error enabled, stale content continues to be served if background refresh hits upstream failures.
- Upstream 404s for mutable files are briefly remembered to reduce repeated requests for non-existent files.
- When upstream checksums disagree with downloaded content, the service retries to distinguish an unstable download from an incorrect checksum. Stable content is accepted with a warning; repeatedly changing unverified content is discarded and other sources are tried. Cached `.sha1`, `.sha256`, and `.sha512` files are always computed from the bytes actually stored.

Upstream requests are bounded by a global concurrency limit and a per-repository limit; the scheduler prioritizes first-time downloads while periodically leaving room for cache refreshes.

### HTTP API

The local Maven endpoint is set by `[server].base_path`, defaulting to `/maven`. The root path and `/api` are reserved for the service itself and cannot be used as the Maven endpoint.

- `GET /api/v1/health`: checks SQLite, cache directory, and temp directory; returns `200 OK` when healthy, otherwise service unavailable.
- `GET /api/v1/cache/stats`: returns cached file count, total size, hit rate, negative cache count, and per-upstream circuit breaker status.

Both endpoints are suitable for liveness/readiness checks in containers or process managers. Cache stats are intended for low-frequency monitoring, not per-request polling.

The Maven endpoint supports `GET`, `HEAD`, single byte-range requests, client validators, and cache-control headers. Permanent
artifacts receive immutable client caching; mutable metadata requires revalidation against the local proxy.

### Logging

Use `RUST_LOG` to adjust log levels, e.g.:

```bash
RUST_LOG=maven_haste=debug maven-haste run -c ./maven-haste.toml
```

`--verbose` also enables debug logging; if both are set, the environment variable takes precedence.

File logging is disabled by default. Configure `[logging]` with `enabled = true` to enable. The default directory is `<root>/.maven-haste/logs`.

`logging.filter` uses the [`tracing-subscriber` EnvFilter directive syntax][env-filter-directives].

## Compatibility Projects

Small Maven, Gradle, Fabric, Forge, and NeoForge projects live in [test-projects](test-projects/README.md). They are manual
compatibility and performance checks and are not part of the offline Rust test suite. The regular Gradle fixtures form one
multi-project build, while the Stonecutter fixture remains an independent build with its own version subprojects. All
Gradle fixtures share one Wrapper under `test-projects/`. The representative Fabric fixture is inspired by the build layout of
[TerraformersMC/ModMenu](https://github.com/TerraformersMC/ModMenu).

[env-filter-directives]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
