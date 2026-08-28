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
    #[serde(default)]
    pub logging: LoggingConfig,
    pub repositories: Vec<RepositoryConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
    pub filter: String,
}

impl LoggingConfig {
    pub fn directory(&self) -> &Path {
        self.directory
            .as_deref()
            .expect("logging paths are resolved while loading configuration")
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            retention: Duration::from_secs(30 * 24 * 60 * 60),
            filter: "maven_haste=info,maven_haste::access=debug".to_owned(),
        }
    }
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

    /// Resolves the internal storage layout for a repository root.
    pub fn resolved(root: PathBuf) -> Self {
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
    /// Maximum cached artifact bytes. No limit is applied when omitted.
    pub max_size: Option<u64>,
    #[serde(with = "humantime_serde")]
    pub metadata_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub negative_ttl: Duration,
    pub serve_stale_on_error: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Url>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            max_concurrency: 32,
            default_repository_max_concurrency: 10,
            foreground_priority_burst: 8,
            proxy: None,
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
    pub id: String,
    pub url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_proxy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default)]
    pub rules: Vec<String>,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<LoadedConfig, ConfigError> {
        fn resolve_paths(
            storage: &mut StorageConfig,
            logging: &mut LoggingConfig,
            config_dir: &Path,
        ) {
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
            logging.directory = Some(match logging.directory.take() {
                Some(path) => resolve_path(config_dir, &path),
                None => internal.join("logs"),
            });

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
            fn resolve_path(base: &Path, path: &Path) -> PathBuf {
                let joined = if path.is_absolute() {
                    path.to_owned()
                } else {
                    base.join(path)
                };
                normalize_path(&joined)
            }
        }

        fn locate(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
            fn canonical_config_path(path: &Path) -> Result<PathBuf, ConfigError> {
                dunce::canonicalize(path).map_err(|error| {
                    ConfigError::new(format!(
                        "configuration file {} is unavailable: {error}",
                        path.display()
                    ))
                })
            }

            let current_dir = env::current_dir().map_err(|error| {
                ConfigError::new(format!("failed to read current directory: {error}"))
            })?;

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
        validate_raw_logging_paths(&config.logging)?;

        let config_dir = path
            .parent()
            .expect("an absolute configuration path always has a parent");
        resolve_paths(&mut config.storage, &mut config.logging, config_dir);
        normalize(&mut config);
        if let Some(bind) = cli.bind {
            config.server.bind = bind;
        }
        validate(&config)?;

        Ok(LoadedConfig { path, config })
    }
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

fn validate_raw_logging_paths(logging: &LoggingConfig) -> Result<(), ConfigError> {
    if logging
        .directory
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(ConfigError::new(
            "logging.directory must not be empty when specified",
        ));
    }
    Ok(())
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.logging.directory().as_os_str().is_empty() {
        return Err(ConfigError::new("logging.directory must not be empty"));
    }
    if config.logging.retention < Duration::from_secs(24 * 60 * 60) {
        return Err(ConfigError::new("logging.retention must be at least 1 day"));
    }
    tracing_subscriber::EnvFilter::try_new(&config.logging.filter)
        .map_err(|error| ConfigError::new(format!("invalid logging.filter: {error}")))?;
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
    if config.server.base_path == "/"
        || config.server.base_path == "/api"
        || config.server.base_path.starts_with("/api/")
    {
        return Err(ConfigError::new(
            "server.base_path must not be '/' or overlap the reserved '/api' path",
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
    if config.cache.max_size == Some(0) {
        return Err(ConfigError::new(
            "cache.max_size must be greater than zero when specified",
        ));
    }
    if config.upstream.read_timeout.is_zero() {
        return Err(ConfigError::new(
            "upstream.read_timeout must be greater than zero",
        ));
    }
    if let Some(proxy) = &config.upstream.proxy {
        if !matches!(proxy.scheme(), "http" | "https") {
            return Err(ConfigError::new("upstream.proxy must use http or https"));
        }
        if proxy.host_str().is_none() {
            return Err(ConfigError::new("upstream.proxy must include a host"));
        }
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

    let mut ids = HashSet::new();
    for repository in &config.repositories {
        if repository.id.trim().is_empty() {
            return Err(ConfigError::new("repository id must not be empty"));
        }
        if !ids.insert(&repository.id) {
            return Err(ConfigError::new(format!(
                "repository id {:?} is duplicated",
                repository.id
            )));
        }
        if repository.use_proxy == Some(true) && config.upstream.proxy.is_none() {
            return Err(ConfigError::new(format!(
                "repository {:?} has use_proxy = true but [upstream].proxy is not configured",
                repository.id
            )));
        }
        if repository.max_concurrency == Some(0) {
            return Err(ConfigError::new(format!(
                "repository {:?} max_concurrency must be greater than zero",
                repository.id
            )));
        }
        if !matches!(repository.url.scheme(), "http" | "https") {
            return Err(ConfigError::new(format!(
                "repository {:?} URL must use http or https",
                repository.id
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
                repository.id
            )));
        }
        for rule in &repository.rules {
            validate_rule(&repository.id, rule)?;
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
        Cli::try_parse_from(["maven-haste", "run", "--config", path.to_str().unwrap()]).unwrap()
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
id = "central"
url = "https://repo.example/maven2"
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
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
id = "central"
url = "https://repo.example/"
"#,
        );
        let cli = Cli::try_parse_from([
            "maven-haste",
            "run",
            "--config",
            path.to_str().unwrap(),
            "--bind",
            "0.0.0.0:9000",
        ])
        .unwrap();

        let loaded = Config::load(&cli).unwrap();
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
        assert!(!loaded.config.logging.enabled);
        assert_eq!(
            loaded.config.logging.directory(),
            directory.path().join("repository/.maven-haste/logs")
        );
    }

    #[test]
    fn rejects_maven_base_paths_that_overlap_reserved_routes() {
        let directory = TempDir::new().unwrap();
        for base_path in ["/", "/api", "/api/v1", "/api/custom"] {
            let path = write_config(
                &directory,
                &format!(
                    "[server]\nbase_path = '{base_path}'\n\n[storage]\nroot = 'repository'\n\n[[repositories]]\nid = 'central'\nurl = 'https://repo.example/'\n"
                ),
            );

            let cli1 = &cli(&path);
            let error = Config::load(cli1).unwrap_err();
            assert!(
                error.to_string().contains("reserved '/api' path"),
                "unexpected error for {base_path}: {error}"
            );
        }
    }

    #[test]
    fn accepts_maven_base_path_with_a_similar_non_reserved_segment() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[server]
base_path = "/apiary"

[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        assert_eq!(
            Config::load(cli1).unwrap().config.server.base_path,
            "/apiary"
        );
    }

    #[test]
    fn resolves_and_validates_logging_configuration() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[logging]
enabled = true
directory = "audit"
retention = "7d"
filter = "maven_haste::access=trace"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );
        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
        let logging = loaded.config.logging;
        assert!(logging.enabled);
        assert_eq!(logging.directory(), directory.path().join("audit"));
        assert_eq!(logging.retention, Duration::from_secs(7 * 24 * 60 * 60));
    }

    #[test]
    fn rejects_invalid_logging_configuration_when_disabled() {
        let directory = TempDir::new().unwrap();
        for logging in [
            "directory = ''",
            "retention = '23h'",
            "filter = 'maven_haste=[broken'",
        ] {
            let path = write_config(
                &directory,
                &format!(
                    "[storage]\nroot = 'repository'\n\n[logging]\n{logging}\n\n[[repositories]]\nid = 'central'\nurl = 'https://repo.example/'\n"
                ),
            );
            let cli1 = &cli(&path);
            assert!(Config::load(cli1).is_err());
        }
    }

    #[test]
    fn rejects_unknown_nested_logging_configuration() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[logging.file]
enabled = true

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        assert!(Config::load(cli1).is_err());
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
id = "central"
url = "https://repo.example/"
rules = ["com.example:*"]
"#,
        );

        let cli1 = &cli(&path);
        assert!(Config::load(cli1).is_err());
    }

    #[test]
    fn parses_without_proxy_or_use_proxy() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
        assert!(loaded.config.upstream.proxy.is_none());
        assert_eq!(loaded.config.repositories[0].use_proxy, None);
    }

    #[test]
    fn parses_global_proxy_without_repository_use_proxy() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[upstream]
proxy = "http://127.0.0.1:7890"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
        assert_eq!(
            loaded.config.upstream.proxy.as_ref().map(Url::as_str),
            Some("http://127.0.0.1:7890/")
        );
        assert_eq!(loaded.config.repositories[0].use_proxy, None);
    }

    #[test]
    fn parses_global_proxy_with_repository_use_proxy_true() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[upstream]
proxy = "http://user:password@127.0.0.1:7890"

[[repositories]]
id = "central"
url = "https://repo.example/"
use_proxy = true
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
        assert_eq!(
            loaded.config.upstream.proxy.as_ref().map(Url::as_str),
            Some("http://user:password@127.0.0.1:7890/")
        );
        assert_eq!(loaded.config.repositories[0].use_proxy, Some(true));
    }

    #[test]
    fn parses_global_proxy_with_repository_use_proxy_false() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[upstream]
proxy = "http://127.0.0.1:7890"

[[repositories]]
id = "central"
url = "https://repo.example/"
use_proxy = false
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
        assert_eq!(
            loaded.config.upstream.proxy.as_ref().map(Url::as_str),
            Some("http://127.0.0.1:7890/")
        );
        assert_eq!(loaded.config.repositories[0].use_proxy, Some(false));
    }

    #[test]
    fn rejects_use_proxy_true_without_global_proxy() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/"
use_proxy = true
"#,
        );

        let cli1 = &cli(&path);
        let error = Config::load(cli1).unwrap_err();
        assert!(
            error.to_string().contains("use_proxy = true"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_unsupported_proxy_scheme() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[upstream]
proxy = "socks5://127.0.0.1:1080"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        let error = Config::load(cli1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("upstream.proxy must use http or https"),
            "unexpected error: {error}"
        );
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
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        let loaded = Config::load(cli1).unwrap();
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
id = "central"
url = "https://repo.example/"
"#,
        );

        let cli1 = &cli(&path);
        assert!(Config::load(cli1).is_err());
    }

    #[test]
    fn rejects_zero_upstream_timeouts_and_removed_refresh_timeout() {
        let directory = TempDir::new().unwrap();
        for body in [
            r#"
[storage]
root = "repository"

[cache]
max_size = 0

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
connect_timeout = "0s"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
read_timeout = "0s"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[cache]
refresh_timeout = "10s"

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[cache]
refresh_max_concurrency = 10

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
max_concurrency = 0

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
default_repository_max_concurrency = 0

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[upstream]
foreground_priority_burst = 0

[[repositories]]
id = "central"
url = "https://repo.example/"
"#,
            r#"
[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/"
max_concurrency = 0
"#,
        ] {
            let path = write_config(&directory, body);
            let cli1 = &cli(&path);
            assert!(Config::load(cli1).is_err());
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
id = "central"
url = "https://repo.example/"
max_concurrency = 17
"#,
        );

        let cli1 = &cli(&path);
        assert_eq!(
            Config::load(cli1).unwrap().config.repositories[0].max_concurrency,
            Some(17)
        );
    }
}
