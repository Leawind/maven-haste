# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Enforced a repository id naming rule (lowercase ASCII letters, digits, underscores, and hyphens).
- Added per-repository `cache_writes` to stop writing new artifacts, checksums, and negative entries from a repository while previously cached content keeps being served.
- Added a per-artifact request counter to the cache database; every client request increments it for the requested path, and it is recorded but not used yet.
- Added JSON, TOML, and YAML configuration support; without `--config`, maven-haste discovers `maven-haste.json`, `maven-haste.toml`, or `maven-haste.yaml` and fails when several formats exist instead of choosing one.

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
