use std::collections::HashSet;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::Cli;
use crate::error::ConfigError;

const CONFIG_FILE_NAME: &str = "maven-haste.toml";
pub const EXAMPLE_CONFIG: &str = include_str!("../maven-haste.example.toml");

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let current_dir = env::current_dir()
        .map_err(|error| ConfigError::new(format!("failed to read current directory: {error}")))?;
    Ok(current_dir.join(CONFIG_FILE_NAME))
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    pub repositories: Vec<RepositoryConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub base_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            base_path: "/maven".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmp_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    db_path: Option<PathBuf>,
}

impl StorageConfig {
    pub fn tmp_dir(&self) -> &Path {
        self.tmp_dir
            .as_deref()
            .expect("storage paths are resolved while loading configuration")
    }

    pub fn db_path(&self) -> &Path {
        self.db_path
            .as_deref()
            .expect("storage paths are resolved while loading configuration")
    }

    #[cfg(test)]
    pub fn resolved_for_test(root: PathBuf) -> Self {
        let internal = root.join(".maven-haste");
        Self {
            root,
            tmp_dir: Some(internal.join("tmp")),
            db_path: Some(internal.join("cache.db")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(with = "humantime_serde")]
    pub metadata_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub negative_ttl: Duration,
    pub serve_stale_on_error: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            metadata_ttl: Duration::from_secs(5 * 60),
            negative_ttl: Duration::from_secs(5 * 60),
            serve_stale_on_error: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    #[serde(with = "humantime_serde")]
    pub connect_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub read_timeout: Duration,
    pub max_concurrency: usize,
    pub default_repository_max_concurrency: usize,
    pub foreground_priority_burst: usize,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            max_concurrency: 32,
            default_repository_max_concurrency: 10,
            foreground_priority_burst: 8,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    #[serde(with = "humantime_serde")]
    pub recovery_timeout: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            recovery_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub name: String,
    pub url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default)]
    pub rules: Vec<String>,
}

pub fn load(cli: &Cli) -> Result<LoadedConfig, ConfigError> {
    let path = locate(cli.config.as_deref())?;
    let source = std::fs::read_to_string(&path).map_err(|error| {
        ConfigError::new(format!(
            "failed to read configuration {}: {error}",
            path.display()
        ))
    })?;
    let mut config: Config = toml::from_str(&source).map_err(|error| {
        ConfigError::new(format!(
            "failed to parse configuration {}: {error}",
            path.display()
        ))
    })?;

    validate_raw_storage_paths(&config.storage)?;

    let config_dir = path
        .parent()
        .expect("an absolute configuration path always has a parent");
    resolve_paths(&mut config.storage, config_dir);
    normalize(&mut config);
    if let Some(bind) = cli.bind {
        config.server.bind = bind;
    }
    validate(&config)?;

    Ok(LoadedConfig { path, config })
}

fn locate(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let current_dir = env::current_dir()
        .map_err(|error| ConfigError::new(format!("failed to read current directory: {error}")))?;

    if let Some(path) = explicit {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            current_dir.join(path)
        };
        return canonical_config_path(&path);
    }

    let mut attempted = vec![current_dir.join(CONFIG_FILE_NAME)];
    if let Some(base_dirs) = BaseDirs::new() {
        attempted.push(
            base_dirs
                .config_dir()
                .join("maven-haste")
                .join(CONFIG_FILE_NAME),
        );
    }

    for path in &attempted {
        if path.is_file() {
            return canonical_config_path(path);
        }
    }

    let paths = attempted
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(ConfigError::new(format!(
        "configuration file not found; attempted:\n{paths}\nrun `maven-haste config init` or pass --config <PATH>"
    )))
}

fn canonical_config_path(path: &Path) -> Result<PathBuf, ConfigError> {
    dunce::canonicalize(path).map_err(|error| {
        ConfigError::new(format!(
            "configuration file {} is unavailable: {error}",
            path.display()
        ))
    })
}

fn resolve_paths(storage: &mut StorageConfig, config_dir: &Path) {
    storage.root = resolve_path(config_dir, &storage.root);
    let internal = storage.root.join(".maven-haste");
    storage.tmp_dir = Some(match storage.tmp_dir.take() {
        Some(path) => resolve_path(config_dir, &path),
        None => internal.join("tmp"),
    });
    storage.db_path = Some(match storage.db_path.take() {
        Some(path) => resolve_path(config_dir, &path),
        None => internal.join("cache.db"),
    });
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    normalize_path(&joined)
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn normalize(config: &mut Config) {
    if config.server.base_path.len() > 1 {
        while config.server.base_path.ends_with('/') {
            config.server.base_path.pop();
        }
    }
    for repository in &mut config.repositories {
        if !repository.url.path().ends_with('/') {
            let path = format!("{}/", repository.url.path());
            repository.url.set_path(&path);
        }
    }
}

fn validate_raw_storage_paths(storage: &StorageConfig) -> Result<(), ConfigError> {
    if storage.root.as_os_str().is_empty() {
        return Err(ConfigError::new("storage.root must not be empty"));
    }
    if storage
        .tmp_dir
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(ConfigError::new(
            "storage.tmp_dir must not be empty when specified",
        ));
    }
    if storage
        .db_path
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(ConfigError::new(
            "storage.db_path must not be empty when specified",
        ));
    }
    Ok(())
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.server.base_path.is_empty()
        || !config.server.base_path.starts_with('/')
        || config.server.base_path.contains('?')
        || config.server.base_path.contains('#')
        || config.server.base_path.contains("//")
        || !config.server.base_path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '-' | '_' | '.')
        })
        || config
            .server
            .base_path
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(ConfigError::new(
            "server.base_path must be a canonical absolute URL path",
        ));
    }
    if config.storage.root.as_os_str().is_empty() {
        return Err(ConfigError::new("storage.root must not be empty"));
    }
    let internal = config.storage.root.join(".maven-haste");
    for (name, path) in [
        ("storage.tmp_dir", config.storage.tmp_dir()),
        ("storage.db_path", config.storage.db_path()),
    ] {
        if path_is_within(path, &config.storage.root) && !path_is_within(path, &internal) {
            return Err(ConfigError::new(format!(
                "{name} must be outside storage.root or inside its reserved .maven-haste directory"
            )));
        }
    }
    if config.upstream.connect_timeout.is_zero() {
        return Err(ConfigError::new(
            "upstream.connect_timeout must be greater than zero",
        ));
    }
    if config.upstream.read_timeout.is_zero() {
        return Err(ConfigError::new(
            "upstream.read_timeout must be greater than zero",
        ));
    }
    for (name, value) in [
        ("upstream.max_concurrency", config.upstream.max_concurrency),
        (
            "upstream.default_repository_max_concurrency",
            config.upstream.default_repository_max_concurrency,
        ),
        (
            "upstream.foreground_priority_burst",
            config.upstream.foreground_priority_burst,
        ),
    ] {
        if value == 0 {
            return Err(ConfigError::new(format!(
                "{name} must be greater than zero"
            )));
        }
    }
    if config.circuit_breaker.failure_threshold == 0 {
        return Err(ConfigError::new(
            "circuit_breaker.failure_threshold must be greater than zero",
        ));
    }
    if config.circuit_breaker.recovery_timeout.is_zero() {
        return Err(ConfigError::new(
            "circuit_breaker.recovery_timeout must be greater than zero",
        ));
    }
    if config.repositories.is_empty() {
        return Err(ConfigError::new(
            "at least one [[repositories]] entry is required",
        ));
    }

    let mut names = HashSet::new();
    for repository in &config.repositories {
        if repository.name.trim().is_empty() {
            return Err(ConfigError::new("repository name must not be empty"));
        }
        if !names.insert(&repository.name) {
            return Err(ConfigError::new(format!(
                "repository name {:?} is duplicated",
                repository.name
            )));
        }
        if repository.max_concurrency == Some(0) {
            return Err(ConfigError::new(format!(
                "repository {:?} max_concurrency must be greater than zero",
                repository.name
            )));
        }
        if !matches!(repository.url.scheme(), "http" | "https") {
            return Err(ConfigError::new(format!(
                "repository {:?} URL must use http or https",
                repository.name
            )));
        }
        if repository.url.cannot_be_a_base()
            || repository.url.host_str().is_none()
            || !repository.url.username().is_empty()
            || repository.url.password().is_some()
            || repository.url.query().is_some()
            || repository.url.fragment().is_some()
        {
            return Err(ConfigError::new(format!(
                "repository {:?} URL must be a base URL without query or fragment",
                repository.name
            )));
        }
        for rule in &repository.rules {
            validate_rule(&repository.name, rule)?;
        }
    }

    Ok(())
}

fn validate_rule(repository: &str, rule: &str) -> Result<(), ConfigError> {
    let pattern = rule.strip_prefix('!').unwrap_or(rule);
    if pattern.is_empty()
        || rule.trim() != rule
        || pattern.starts_with('!')
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains(':')
        || pattern
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(ConfigError::new(format!(
            "repository {repository:?} has invalid path glob rule {rule:?}"
        )));
    }
    Ok(())
}

fn path_is_within(path: &Path, base: &Path) -> bool {
    let mut path_components = path.components();
    base.components().all(|base_component| {
        path_components.next().is_some_and(|path_component| {
            component_eq(path_component.as_os_str(), base_component.as_os_str())
        })
    })
}

#[cfg(windows)]
fn component_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn component_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;
    use tempfile::TempDir;

    use super::*;

    fn write_config(directory: &TempDir, body: &str) -> PathBuf {
        let path = directory.path().join(CONFIG_FILE_NAME);
        fs::write(&path, body).unwrap();
        path
    }

    fn cli(path: &Path) -> Cli {
        Cli::try_parse_from(["maven-haste", "--config", path.to_str().unwrap()]).unwrap()
    }

    #[test]
    fn resolves_storage_paths_from_configuration_directory() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/maven2"
"#,
        );

        let loaded = load(&cli(&path)).unwrap();
        assert_eq!(
            loaded.config.storage.root,
            directory.path().join("repository")
        );
        assert_eq!(
            loaded.config.storage.tmp_dir(),
            directory.path().join("repository/.maven-haste/tmp")
        );
        assert_eq!(
            loaded.config.storage.db_path(),
            directory.path().join("repository/.maven-haste/cache.db")
        );
        assert_eq!(
            loaded.config.repositories[0].url.as_str(),
            "https://repo.example/maven2/"
        );
    }

    #[test]
    fn applies_bind_override_and_defaults() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
        );
        let cli = Cli::try_parse_from([
            "maven-haste",
            "--config",
            path.to_str().unwrap(),
            "--bind",
            "0.0.0.0:9000",
        ])
        .unwrap();

        let loaded = load(&cli).unwrap();
        assert_eq!(loaded.config.server.bind, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(loaded.config.server.base_path, "/maven");
        assert_eq!(loaded.config.cache.metadata_ttl, Duration::from_secs(300));
        assert_eq!(
            loaded.config.upstream.connect_timeout,
            Duration::from_secs(10)
        );
        assert_eq!(loaded.config.upstream.read_timeout, Duration::from_secs(60));
        assert_eq!(loaded.config.upstream.max_concurrency, 32);
        assert_eq!(
            loaded.config.upstream.default_repository_max_concurrency,
            10
        );
        assert_eq!(loaded.config.upstream.foreground_priority_burst, 8);
        assert_eq!(loaded.config.repositories[0].max_concurrency, None);
    }

    #[test]
    fn rejects_coordinate_style_rules() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
rules = ["com.example:*"]
"#,
        );

        assert!(load(&cli(&path)).is_err());
    }

    #[test]
    fn serializes_fully_resolved_configuration() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
        );

        let loaded = load(&cli(&path)).unwrap();
        let serialized = toml::to_string_pretty(&loaded.config).unwrap();

        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.storage.root, loaded.config.storage.root);
        assert_eq!(reparsed.storage.tmp_dir(), loaded.config.storage.tmp_dir());
        assert_eq!(reparsed.storage.db_path(), loaded.config.storage.db_path());
        assert_eq!(
            reparsed.cache.metadata_ttl,
            loaded.config.cache.metadata_ttl
        );
        assert_eq!(
            reparsed.upstream.connect_timeout,
            loaded.config.upstream.connect_timeout
        );
        assert_eq!(
            reparsed.upstream.read_timeout,
            loaded.config.upstream.read_timeout
        );
    }

    #[test]
    fn rejects_internal_files_in_addressable_repository_paths() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"
db_path = "repository/com/example/cache.db"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
        );

        assert!(load(&cli(&path)).is_err());
    }

    #[test]
    fn rejects_zero_upstream_timeouts_and_removed_refresh_timeout() {
        let directory = TempDir::new().unwrap();
        for body in [
            r#"
[storage]
root = "repository"

[upstream]
connect_timeout = "0s"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
read_timeout = "0s"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[cache]
refresh_timeout = "10s"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[cache]
refresh_max_concurrency = 10

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
max_concurrency = 0

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
default_repository_max_concurrency = 0

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
foreground_priority_burst = 0

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
max_concurrency = 0
"#,
        ] {
            let path = write_config(&directory, body);
            assert!(load(&cli(&path)).is_err());
        }
    }

    #[test]
    fn loads_repository_concurrency_override() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
max_concurrency = 17
"#,
        );

        assert_eq!(
            load(&cli(&path)).unwrap().config.repositories[0].max_concurrency,
            Some(17)
        );
    }
}
