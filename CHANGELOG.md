# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Added `maven-haste cache verify` to re-check every cached artifact's size and checksums, and `maven-haste cache remove-prefix` to delete all artifacts, checksums, and negative entries under a path prefix.
- Added a validation error when two repositories share one upstream URL.
- `.yml` configuration files are now discovered by default alongside `.json`, `.toml`, and `.yaml`.

### Changed

- Cache installation writes database records before renaming files into place, so an interrupted install is repaired by the next request instead of leaving untracked files. Temporary files are synced to disk before the atomic rename.
- Each cache hit now issues a single database write that updates the request counter and the access timestamp together.
- Prefix removal matches paths case-sensitively, symmetric with install-time case-conflict handling.

### Fixed

- Fixed a circuit breaker state where a canceled half-open probe request wedged the repository in half-open state until restart. A success observed while the circuit is open no longer closes it or resets its failures.
- Fixed concurrent passthrough responses on repositories with `cache_writes = false` sharing one temporary file that the first finishing response deleted.
- Fixed capacity eviction removing cached files while a response was still serving them. A cached file whose size no longer matches its record is fetched again instead of served under the record's Content-Length.
- `config check` now prepares and probes the storage layout, as promised by its help text.
- Fixed SIGTERM not triggering graceful shutdown.
- Fixed an invalid `Range` header with an inverted byte range answering 416; it is now ignored with a full 200 response, as RFC 9110 requires.

### Removed

- Removed the unused `cache.serve_stale_on_error` option; serving stale content while a refresh fails remains the unconditional behavior.

## [0.1.2]

### Added

- Enforced a repository id naming rule (lowercase ASCII letters, digits, underscores, and hyphens).
- Added per-repository `cache_writes` to stop writing new artifacts, checksums, and negative entries from a repository while previously cached content keeps being served.
- Added a per-artifact request counter to the cache database; every client request increments it for the requested path, and it is recorded but not used yet.
- Added JSON, TOML, and YAML configuration support; without `--config`, maven-haste discovers `maven-haste.json`, `maven-haste.toml`, or `maven-haste.yaml` and fails when several formats exist instead of choosing one. The parser is selected by the file extension (`json`, `yaml`, `yml`, or `toml`); unsupported or missing extensions are rejected instead of falling back to TOML.
- Added a JSON schema for the configuration; editors and AI tools can use `maven-haste.schema.json` for hints and validation, `maven-haste config schema` prints or writes it, and the `$schema` key is accepted in any format.
- `config init` and `config example` now generate a minimal, comment-free configuration (only the `$schema` key and the required keys) in the format of the target path's extension, defaulting to TOML; the in-repo example template file was removed. The generated `$schema` reference is pinned to the current release version, so editors and AI tools pick up the schema with zero setup; the version is injected instead of being hard-coded.

## [0.1.1]

### Added

- Added conditional and byte-range responses for cached artifacts.
- Added generated SHA-512 checksums and upstream SHA-512 validation.
- Added optional cache size limits with least-recently-used eviction.
- Added cache statistics, prefix removal, and integrity verification commands.
- Added manual build-tool configurations and Java and Minecraft compatibility projects.
- Added one shared Gradle Wrapper for building compatibility projects.

## [0.1.0]

### Added

- Initial release.
