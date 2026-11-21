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
- Run the container image from GitHub Packages
  ```bash
  docker run -d -p 8080:8080 \
      -v maven-haste-cache:/var/lib/maven-haste \
      -v ./config:/etc/maven-haste \
      ghcr.io/leawind/maven-haste
  ```

  The image expects a `maven-haste.toml` (or `.json`/`.yaml`/`.yml`) in the mounted `/etc/maven-haste` directory and stores cached artifacts under `/var/lib/maven-haste`; point `[storage].root` there in the config. The image is published for `linux/amd64` and `linux/arm64`, runs as an unprivileged user, and reports container health through HTTP. Set `[server].bind = "0.0.0.0:8080"` inside a container so the published port is reachable. Release archives are published with SHA-256 checksum files.

## Running

Generate a config template in the current working directory (never overwrites existing files)

```bash
maven-haste config init [--format json|yaml|toml] [PATH]
```

Start using the config in the current working directory; `maven-haste.json`, `maven-haste.toml`, `maven-haste.yaml`, or `maven-haste.yml` is discovered automatically

```bash
maven-haste run
```

Use `-c --config <PATH>` to specify a config file path.

Validate the config and storage directory before starting:

```bash
maven-haste config check
```

Maintain the local cache: re-check every cached artifact's size and checksums, remove all cached content under a path prefix, print cache totals, prefetch a manifest, or recover from a corrupt database

```bash
maven-haste cache verify
maven-haste cache remove-prefix com/example
maven-haste cache stats
maven-haste cache stats --json
maven-haste cache warm ./dependencies.txt
maven-haste cache repair
```

`cache verify` exits with an error when it finds issues, listing each one. `cache warm` reads one Maven relative path per line, ignoring blank lines and lines starting with `#`, and exits with an error if any entry cannot be fetched. `cache repair` quarantines a corrupt SQLite database and creates a fresh one; existing content objects stay on disk and are fetched again as needed.

> [!WARNING]
>
> The Maven endpoint does not authenticate clients. Keep the default loopback bind address, or place the service behind a reverse proxy or firewall that restricts access. Do not expose a cache of private artifacts to untrusted networks.

Set `[upstream].offline = true` to serve only cached content. Uncached paths then fail with `503 Service Unavailable` without contacting an upstream repository.

## Configuration

> [!TIP]
>
> Run `maven-haste config init [PATH]` to create a minimal configuration with only the required keys; the format (TOML, YAML, or JSON) follows the extension of `PATH`, defaulting to YAML (`./maven-haste.yaml`), or pass `--format json|yaml|toml` to choose it explicitly. Field descriptions live in the JSON schema listed below.

Recommended steps:

1. Configure upstream Maven repositories under `[[repositories]]`, including authentication or custom headers when needed.
2. Adjust the listen address and local endpoint prefix in `[server]` as needed.
3. Set the storage root in `[storage]`. Relative paths are resolved against the config file's directory.
4. Tune cache TTLs, upstream timeouts, concurrency, and circuit breaker parameters to match your network.
5. Run `maven-haste config check -c <config>` to validate the config, storage directory, and database before starting.

Without `-c/--config`, maven-haste looks for `maven-haste.json`, `maven-haste.toml`, `maven-haste.yaml`, or `maven-haste.yml` in the current directory and the system user config directory; if several of these files exist, it stops instead of picking one. The parser is chosen by the file extension (`json`, `yaml`, `yml`, or `toml`); an unsupported or missing extension is an error rather than a TOML fallback. Global flags can temporarily override the listen address or enable debug logging at startup; persistent settings belong in the config file.

### Schema-Assisted Editing

A JSON schema describing the whole configuration lives in [maven-haste.schema.json](maven-haste.schema.json) and is also printed by `maven-haste config schema` (use `-o <PATH>` to write it to a file). `config init` generates a minimal config in the requested format — the target path's extension, `--format`, or YAML by default — and its `$schema` key is already active and pinned to the current release, so editors or AI tools pick the schema up with no editing and no IDE settings:

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

When an upstream lacks a file, fails, or returns content that still cannot be verified against its checksums after retries, the service continues to the next upstream where applicable. Redirects are followed only to origins (scheme, host, and port) explicitly listed in the configured repository URLs.

Metadata documents are cached as the first successful response from the ordered candidates. Maven Haste does not combine `maven-metadata.xml` documents from several repositories; if dynamic versions or version ranges must see every version, route each artifact family to one upstream that already aggregates its repositories, or keep the original repositories configured in the build tool.

### Upstream Authentication and TLS

Each repository can send custom headers, HTTP Basic credentials, or a bearer token:

```toml
[[repositories]]
id = "company"
url = "https://repo.example.com/maven/"
username = "build-user"
password = "${MAVEN_HASTE_REPO_PASSWORD}"
headers = { "X-Api-Key" = "${MAVEN_HASTE_API_KEY}" }
```

`username`, `password`, `bearer_token`, and header values support `${ENV_VAR}` expansion. Basic credentials and a bearer token are mutually exclusive. Headers that control the HTTP exchange itself (`Host`, `Content-Length`, `Range`, `If-*`, `Connection`, `Transfer-Encoding`, and `Proxy-Authorization`) cannot be overridden. Print the effective configuration with `maven-haste config show`; credentials and header values are redacted, and proxy credentials are redacted in logs as well.

Set `[upstream].ca_bundle` to a PEM or DER file with additional root certificates when an upstream or proxy uses a private certificate authority.

### Upstream Proxy

Set `[upstream].proxy` to route every request through one or more HTTP proxies; a single URL or an ordered list both work, and the proxies are tried in the listed order. A repository's `use_proxy` overrides that choice: `true` always uses the proxies, `false` always connects directly. When a proxy cannot be reached (connection refused, DNS failure, or connect timeout), maven-haste hands the request to the next proxy instead of failing, and finally to a direct connection. After `circuit_breaker.failure_threshold` consecutive failures a proxy is skipped for `circuit_breaker.recovery_timeout` and then probed again, so a dead proxy does not stall every download. Set `proxy_fallback = false` to keep the proxies mandatory.

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
- If a background refresh hits upstream failures, the stale cached content continues to be served.
- Upstream 404s for mutable files are briefly remembered to reduce repeated requests for non-existent files. Set `cache.negative_cache_permanent = true` to apply the negative TTL to immutable paths as well.
- Every cached response, including `HEAD` and conditional requests, verifies the file's size and strongest stored digest before serving it. Corrupt files are fetched again; if no upstream can repair them, the request fails. Access counters are buffered and written to SQLite in batches.
- A `HEAD` request for an uncached artifact probes the upstreams with `HEAD` instead of downloading the body. Generated checksums still resolve through their source artifact.
- When an upstream checksum disagrees with downloaded content or cannot be retrieved due to an upstream error, the service retries and then tries other sources. Repeatedly identical content and failed retries never override a checksum mismatch. Only explicit checksum 404s permit generating missing checksums. Cached `.md5`, `.sha1`, `.sha256`, and `.sha512` files are computed from the bytes actually stored.
- A single artifact bundle larger than `cache.max_size` is served directly and is not cached, so the size limit cannot be exceeded by one oversized download.

New cache files are stored as SHA-256 content objects under `<root>/.maven-haste/objects`. All objects in an artifact bundle are persisted before one SQLite transaction publishes the main file and its checksums. Failed publication keeps the previous bundle available, and open responses retain their verified file handles during refresh. Existing Maven-layout caches remain readable without a database migration. Maintenance retires unreferenced objects after a grace period and removes replaced legacy files; objects used by active responses are tracked by hash. Incomplete downloads in the temporary directory are removed only after 24 hours, so a CLI command or a second server process cannot delete an in-flight download. Cache verification reads the whole file before streaming, adding local disk I/O to each hit.

Upstream requests are bounded by a global concurrency limit and a per-repository limit; the scheduler prioritizes first-time downloads while periodically leaving room for cache refreshes. Checksum files are fetched concurrently with the main artifact download. Interrupted downloads resume with `Range` and `If-Range` only when a strong ETag pins the representation. Missing or weak validators, changed ETags, or ignored ranges restart the download from byte zero; inconsistent partial ranges are rejected. There are up to three body-transfer attempts before the repository is skipped. Refreshing a generated checksum refreshes its main artifact and the entire checksum bundle.

The cache size limit is enforced after every write and by a periodic background maintenance pass (`cache.maintenance_interval`, default 1m) that also purges expired negative entries and stale partial downloads. `cache stats` reports both logical artifact bytes and the deduplicated content-object counts.

### HTTP API

The local Maven endpoint is set by `[server].base_path`, defaulting to `/maven`. The root path and `/api` are reserved for the service itself and cannot be used as the Maven endpoint.

- `GET /api/v1/health`: checks SQLite, cache directory, and temp directory; returns `200 OK` when healthy, otherwise service unavailable.
- `GET /api/v1/cache/stats`: returns cached file count, logical and object sizes, recorded requests, hit rate, negative cache count, and per-upstream circuit breaker status.
- `GET /api/v1/metrics`: Prometheus text-format metrics covering cache totals, object counts, request and hit counters, bytes served, interrupted responses, response and upstream latency histograms, and per-upstream circuit breaker states. Each metric family has one `HELP` and one `TYPE` line; counters use the counter type.

These endpoints are suitable for liveness/readiness checks and scraping in containers or process managers. Cache stats and metrics are intended for low-frequency monitoring, not per-request polling.

The Maven endpoint supports `GET`, `HEAD`, single byte-range requests, client validators, and cache-control headers. Permanent
artifacts receive immutable client caching; mutable metadata requires revalidation against the local proxy.

### Logging

Use `RUST_LOG` to adjust log levels, e.g.:

```bash
RUST_LOG=maven_haste=debug maven-haste run -c ./maven-haste.toml
```

`--verbose` also enables debug logging; if both are set, the environment variable takes precedence.

File logging is disabled by default. Configure `[logging]` with `enabled = true` to enable. The default directory is `<root>/.maven-haste/logs`.

`logging.filter` uses the [`tracing-subscriber` EnvFilter directive syntax][env-filter-directives]. Proxy URLs in logs have their credentials redacted.

## Compatibility Projects

Small Maven, Gradle, Fabric, Forge, and NeoForge projects live in [test-projects](test-projects/README.md). They are manual
compatibility and performance checks and are not part of the offline Rust test suite. The regular Gradle fixtures form one
multi-project build, while the Stonecutter fixture remains an independent build with its own version subprojects. All
Gradle fixtures share one Wrapper under `test-projects/`. The representative Fabric fixture is inspired by the build layout of
[TerraformersMC/ModMenu](https://github.com/TerraformersMC/ModMenu).

[env-filter-directives]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
