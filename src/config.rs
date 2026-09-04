use std::collections::HashSet;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::BaseDirs;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::Cli;
use crate::error::ConfigError;

/// Configuration file names probed per directory, one for each supported format.
const CONFIG_FILE_NAMES: &[&str] = &[
    "maven-haste.json",
    "maven-haste.toml",
    "maven-haste.yaml",
    "maven-haste.yml",
];

/// File name used by `config init` when no path is given; the minimal example
/// is written as TOML.
pub const CONFIG_EXAMPLE_FILE_NAME: &str = "maven-haste.toml";

/// Configuration formats supported by file extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
    Json,
}

/// Supported file extensions, listed in errors when a path cannot be matched.
const SUPPORTED_EXTENSIONS: &str = "json, yaml, yml, toml";

/// Selects the configuration format from the path's file extension; an
/// unsupported or missing extension is an error instead of a fallback.
pub fn format_for_path(path: &Path) -> Result<ConfigFormat, ConfigError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            ConfigError::new(format!(
                "configuration {} has no file extension; supported extensions are {SUPPORTED_EXTENSIONS}",
                path.display()
            ))
        })?;
    match extension {
        "json" => Ok(ConfigFormat::Json),
        "yaml" | "yml" => Ok(ConfigFormat::Yaml),
        "toml" => Ok(ConfigFormat::Toml),
        _ => Err(ConfigError::new(format!(
            "unsupported configuration file extension `{extension}` for {}; supported extensions are {SUPPORTED_EXTENSIONS}",
            path.display()
        ))),
    }
}

/// Schema reference pinned to the tag of the current release instead of
/// `main`; the version comes from the package at compile time.
fn schema_reference() -> String {
    format!(
        "https://raw.githubusercontent.com/Leawind/maven-haste/v{}/maven-haste.schema.json",
        env!("CARGO_PKG_VERSION")
    )
}

/// Minimal example model: only the pinned schema reference and the required
/// keys, so the serialized output stays minimal and comment-free and
/// round-trips through the config parser.
#[derive(Serialize)]
struct Example {
    #[serde(rename = "$schema")]
    schema: String,
    storage: ExampleStorage,
    repositories: Vec<ExampleRepository>,
}

#[derive(Serialize)]
struct ExampleStorage {
    root: String,
}

#[derive(Serialize)]
struct ExampleRepository {
    id: String,
    url: String,
}

/// Minimal example configuration in the given format, generated from the same
/// model for every format; the schema reference is pinned to the current
/// release instead of `main`.
pub fn example_config(format: ConfigFormat) -> String {
    let example = Example {
        schema: schema_reference(),
        storage: ExampleStorage {
            root: "./repository".to_owned(),
        },
        repositories: vec![ExampleRepository {
            id: "central".to_owned(),
            url: "https://repo.example/".to_owned(),
        }],
    };
    let serialized = match format {
        ConfigFormat::Toml => {
            toml::to_string_pretty(&example).expect("a static example serializes to TOML")
        }
        ConfigFormat::Yaml => {
            serde_yaml_ng::to_string(&example).expect("a static example serializes to YAML")
        }
        ConfigFormat::Json => {
            serde_json::to_string_pretty(&example).expect("a static example serializes to JSON")
        }
    };
    let trimmed = serialized.trim_end_matches('\n');
    format!("{trimmed}\n")
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let current_dir = env::current_dir()
        .map_err(|error| ConfigError::new(format!("failed to read current directory: {error}")))?;
    Ok(current_dir.join(CONFIG_EXAMPLE_FILE_NAME))
}

/// Canonicalizes a configuration path so the reported path is stable.
fn canonical_config_path(path: &Path) -> Result<PathBuf, ConfigError> {
    dunce::canonicalize(path).map_err(|error| {
        ConfigError::new(format!(
            "configuration file {} is unavailable: {error}",
            path.display()
        ))
    })
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
    let config_dir = BaseDirs::new().map(|base_dirs| base_dirs.config_dir().join("maven-haste"));
    find_default_config(&current_dir, config_dir.as_deref())
}

/// Finds the default configuration by probing each supported file name, first
/// in the working directory and then in the user configuration directory.
/// Several present formats in one directory are an error instead of a choice.
fn find_default_config(
    current_dir: &Path,
    config_dir: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut directories = vec![current_dir.to_owned()];
    if let Some(config_dir) = config_dir {
        directories.push(config_dir.to_owned());
    }

    let mut attempted = Vec::new();
    for directory in &directories {
        let mut found = Vec::new();
        for name in CONFIG_FILE_NAMES {
            let path = directory.join(name);
            attempted.push(path.clone());
            if path.is_file() {
                found.push(path);
            }
        }
        match found.len() {
            0 => continue,
            1 => return canonical_config_path(&found[0]),
            _ => {
                let paths = found
                    .iter()
                    .map(|path| format!("  - {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(ConfigError::new(format!(
                    "multiple configuration files found in {}:\n{paths}\n\
                     keep only one format or pass --config <PATH>",
                    directory.display()
                )));
            }
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

/// Parses configuration source in the format selected by the path's file
/// extension; an unsupported or missing extension is an error.
fn parse_config(source: &str, path: &Path) -> Result<Config, ConfigError> {
    let parse_error = |message: &str| {
        ConfigError::new(format!(
            "failed to parse configuration {}: {message}",
            path.display()
        ))
    };
    match format_for_path(path)? {
        ConfigFormat::Json => {
            serde_json::from_str(source).map_err(|error| parse_error(&error.to_string()))
        }
        ConfigFormat::Yaml => {
            serde_yaml_ng::from_str(source).map_err(|error| parse_error(&error.to_string()))
        }
        ConfigFormat::Toml => {
            toml::from_str(source).map_err(|error| parse_error(&error.to_string()))
        }
    }
}

/// Builds the JSON schema for humantime-formatted duration values (`5m`,
/// `1h30m`, `30d`). Humantime also accepts looser natural-language forms, so
/// no pattern is enforced; the example guides editors instead.
pub fn duration_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "description": "A human-readable duration such as `5m`, `1h30m`, or `30d`.",
        "examples": ["5m"]
    })
}

/// Builds the JSON schema for listen addresses written as `host:port`.
pub fn socket_addr_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": "^(\\[[0-9a-fA-F:]+\\]|[0-9.]+):\\d+$",
        "examples": ["127.0.0.1:8080"]
    })
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
}

/// Everything maven-haste needs to run: server, storage, cache, upstream,
/// circuit breaker, logging, and repository settings.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Maven Haste Configuration")]
pub struct Config {
    /// Editor and linter hint that locates the JSON schema describing this
    /// configuration; maven-haste accepts and ignores this key.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
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

/// Optional JSON Lines file logging settings.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Write JSON Lines logs to daily files.
    pub enabled: bool,
    /// Directory for daily JSON Lines log files; defaults to
    /// `<root>/.maven-haste/logs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    /// Retention period for completed daily log files.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
    pub retention: Duration,
    /// Rust EnvFilter expression for file output; see
    /// <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>.
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

/// Optional server settings; omit this table to use `127.0.0.1:8080` and
/// `/maven`.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address on which Maven Haste accepts HTTP requests.
    #[schemars(schema_with = "crate::config::socket_addr_schema")]
    pub bind: SocketAddr,
    /// URL prefix for the local Maven repository endpoint. The root path and
    /// `/api` namespace are reserved for Maven Haste itself.
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

/// Storage layout: the cache root plus the internal directories, all resolved
/// relative to the configuration file's directory.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory used for cached artifacts, temporary downloads, and the
    /// SQLite database. Relative paths are resolved against the configuration
    /// file's directory.
    pub root: PathBuf,
    /// Optional directory for temporary downloads; defaults to
    /// `<root>/.maven-haste/tmp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmp_dir: Option<PathBuf>,
    /// Optional SQLite database path; defaults to
    /// `<root>/.maven-haste/cache.db`.
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

/// Optional cache settings; omit this table to use the defaults below.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Optional maximum number of cached artifact bytes; omit to retain
    /// cached files indefinitely.
    #[schemars(range(min = 1))]
    pub max_size: Option<u64>,
    /// How long mutable metadata may be served before a background refresh
    /// starts.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
    pub metadata_ttl: Duration,
    /// How long a confirmed 404 is remembered for one path in one upstream
    /// repository.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
    pub negative_ttl: Duration,
    /// Serve the last cached mutable file when refreshing it encounters an
    /// upstream failure.
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

/// Optional upstream settings; omit this table to use the defaults below.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Maximum time allowed to establish a connection to an upstream
    /// repository.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
    pub connect_timeout: Duration,
    /// Maximum idle time while reading each upstream response body; this is
    /// not a limit on the total download time of a large file.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
    pub read_timeout: Duration,
    /// Maximum number of simultaneous requests across all upstream
    /// repositories.
    #[schemars(range(min = 1))]
    pub max_concurrency: usize,
    /// Per-repository concurrency limit when a repository does not override
    /// it.
    #[schemars(range(min = 1))]
    pub default_repository_max_concurrency: usize,
    /// Admit one queued cache refresh after this many foreground downloads
    /// while both are waiting.
    #[schemars(range(min = 1))]
    pub foreground_priority_burst: usize,
    /// Optional global upstream proxy.
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

/// Optional circuit-breaker settings; omit this table to use the defaults
/// below.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerConfig {
    /// Consecutive upstream failures required before temporarily skipping
    /// that repository.
    #[schemars(range(min = 1))]
    pub failure_threshold: u32,
    /// Time to wait before probing an upstream repository after its circuit
    /// opens.
    #[serde(with = "humantime_serde")]
    #[schemars(schema_with = "crate::config::duration_schema")]
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

/// One upstream Maven repository; at least one entry is required.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    /// Required unique repository id used for routing, circuit breaking,
    /// logging, and statistics; use only lowercase letters, digits,
    /// underscores, and hyphens.
    pub id: String,
    /// Upstream repository base URL.
    pub url: Url,
    /// Optional per-repository proxy switch: `true` uses `[upstream].proxy`,
    /// `false` connects directly, omitted follows `[upstream].proxy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_proxy: Option<bool>,
    /// Optional per-repository override of
    /// `default_repository_max_concurrency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_concurrency: Option<usize>,
    /// Optional ordered request-path glob rules; the first matching rule
    /// decides participation. Prefix a rule with `!` to exclude it; `*`
    /// matches within one path segment and `**` may match across path
    /// separators.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Per-repository cache switch; defaults to `true`. `false` stops writing
    /// new artifacts, checksums, and negative entries from this repository:
    /// previously cached content is still served and nothing is deleted.
    #[serde(default = "default_true")]
    pub cache_writes: bool,
}

fn default_true() -> bool {
    true
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

        let path = locate(cli.config.as_deref())?;
        let source = std::fs::read_to_string(&path).map_err(|error| {
            ConfigError::new(format!(
                "failed to read configuration {}: {error}",
                path.display()
            ))
        })?;
        let mut config: Config = parse_config(&source, &path)?;

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
    validate_logging(config)?;
    validate_server(config)?;
    validate_storage(config)?;
    validate_upstream(config)?;
    validate_cache(config)?;
    validate_circuit_breaker(config)?;
    validate_repositories(config)
}

fn validate_logging(config: &Config) -> Result<(), ConfigError> {
    if config.logging.directory().as_os_str().is_empty() {
        return Err(ConfigError::new("logging.directory must not be empty"));
    }
    if config.logging.retention < Duration::from_secs(24 * 60 * 60) {
        return Err(ConfigError::new("logging.retention must be at least 1 day"));
    }
    tracing_subscriber::EnvFilter::try_new(&config.logging.filter)
        .map_err(|error| ConfigError::new(format!("invalid logging.filter: {error}")))?;
    Ok(())
}

fn validate_server(config: &Config) -> Result<(), ConfigError> {
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
    Ok(())
}

fn validate_storage(config: &Config) -> Result<(), ConfigError> {
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
    Ok(())
}

fn validate_upstream(config: &Config) -> Result<(), ConfigError> {
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
    Ok(())
}

fn validate_cache(config: &Config) -> Result<(), ConfigError> {
    if config.cache.max_size == Some(0) {
        return Err(ConfigError::new(
            "cache.max_size must be greater than zero when specified",
        ));
    }
    Ok(())
}

fn validate_circuit_breaker(config: &Config) -> Result<(), ConfigError> {
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
    Ok(())
}

fn validate_repositories(config: &Config) -> Result<(), ConfigError> {
    if config.repositories.is_empty() {
        return Err(ConfigError::new(
            "at least one [[repositories]] entry is required",
        ));
    }

    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
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
        validate_repository_id(&repository.id)?;
        if !urls.insert(repository.url.as_str().to_owned()) {
            return Err(ConfigError::new(format!(
                "repository {:?} URL {} is already used by another repository",
                repository.id, repository.url
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

fn validate_repository_id(id: &str) -> Result<(), ConfigError> {
    const MAX_ID_LENGTH: usize = 64;
    if id.len() > MAX_ID_LENGTH {
        return Err(ConfigError::new(format!(
            "repository id {id:?} must be at most {MAX_ID_LENGTH} characters"
        )));
    }
    if !id.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '_' | '-')
    }) {
        return Err(ConfigError::new(format!(
            "repository id {id:?} must contain only lowercase letters, digits, underscore, and hyphen"
        )));
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
        let path = directory.path().join("maven-haste.toml");
        fs::write(&path, body).unwrap();
        path
    }

    fn cli(path: &Path) -> Cli {
        Cli::try_parse_from(["maven-haste", "run", "--config", path.to_str().unwrap()]).unwrap()
    }

    #[test]
    fn loads_a_json_configuration() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.json");
        fs::write(
            &path,
            r#"{
  "storage": {"root": "repository"},
  "repositories": [{"id": "central", "url": "https://repo.example/maven2"}]
}"#,
        )
        .unwrap();

        let loaded = Config::load(&cli(&path)).unwrap();
        assert_eq!(
            loaded.config.storage.root,
            directory.path().join("repository")
        );
        assert_eq!(
            loaded.config.repositories[0].url.as_str(),
            "https://repo.example/maven2/"
        );
    }

    #[test]
    fn loads_a_yaml_configuration() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.yaml");
        fs::write(
            &path,
            "storage:\n  root: repository\nrepositories:\n  - id: central\n    url: https://repo.example/maven2\n",
        )
        .unwrap();

        let loaded = Config::load(&cli(&path)).unwrap();
        assert_eq!(
            loaded.config.storage.root,
            directory.path().join("repository")
        );
        assert_eq!(
            loaded.config.repositories[0].url.as_str(),
            "https://repo.example/maven2/"
        );
    }

    #[test]
    fn rejects_an_explicit_configuration_with_unknown_extension() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.conf");
        fs::write(
            &path,
            "[storage]\nroot = 'repository'\n\n[[repositories]]\nid = 'central'\nurl = 'https://repo.example/maven2'\n",
        )
        .unwrap();

        let error = Config::load(&cli(&path)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported configuration file extension `conf`"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("json, yaml, yml, toml"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parses_an_explicit_yml_configuration_as_yaml() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.yml");
        fs::write(
            &path,
            "storage:\n  root: repository\nrepositories:\n  - id: central\n    url: https://repo.example/maven2\n",
        )
        .unwrap();

        let loaded = Config::load(&cli(&path)).unwrap();
        assert_eq!(loaded.config.repositories[0].id, "central");
        assert_eq!(
            loaded.config.repositories[0].url.as_str(),
            "https://repo.example/maven2/"
        );
    }

    #[test]
    fn format_for_path_dispatches_by_extension() {
        assert_eq!(
            format_for_path(Path::new("maven-haste.json")).unwrap(),
            ConfigFormat::Json
        );
        assert_eq!(
            format_for_path(Path::new("maven-haste.yaml")).unwrap(),
            ConfigFormat::Yaml
        );
        assert_eq!(
            format_for_path(Path::new("maven-haste.yml")).unwrap(),
            ConfigFormat::Yaml
        );
        assert_eq!(
            format_for_path(Path::new("maven-haste.toml")).unwrap(),
            ConfigFormat::Toml
        );
        let missing = format_for_path(Path::new("maven-haste")).unwrap_err();
        assert!(
            missing.to_string().contains("has no file extension"),
            "unexpected error: {missing}"
        );
        let unsupported = format_for_path(Path::new("maven-haste.foo")).unwrap_err();
        assert!(
            unsupported
                .to_string()
                .contains("unsupported configuration file extension `foo`"),
            "unexpected error: {unsupported}"
        );
    }

    #[test]
    fn example_config_generates_minimal_configs_in_each_format() {
        let version = env!("CARGO_PKG_VERSION");
        let pinned = format!(
            "https://raw.githubusercontent.com/Leawind/maven-haste/v{version}/maven-haste.schema.json"
        );
        for format in [ConfigFormat::Toml, ConfigFormat::Yaml, ConfigFormat::Json] {
            let example = example_config(format);
            assert!(example.contains(&pinned), "{format:?}");
            assert!(
                !example.contains("main/maven-haste.schema.json"),
                "{format:?}"
            );
            assert!(!example.contains("${VERSION}"), "{format:?}");
            assert!(!example.contains("# "), "comments in {format:?}");
            assert!(
                example.ends_with('\n'),
                "missing trailing newline in {format:?}"
            );
            assert!(
                !example.ends_with("\n\n"),
                "more than one trailing newline in {format:?}"
            );
        }

        let toml_value: toml::Value = toml::from_str(&example_config(ConfigFormat::Toml)).unwrap();
        assert_eq!(toml_value["storage"]["root"].as_str(), Some("./repository"));
        assert_eq!(
            toml_value["repositories"][0]["id"].as_str(),
            Some("central")
        );
        assert_eq!(
            toml_value["repositories"][0]["url"].as_str(),
            Some("https://repo.example/")
        );

        let yaml_value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&example_config(ConfigFormat::Yaml)).unwrap();
        assert_eq!(yaml_value["storage"]["root"].as_str(), Some("./repository"));
        assert_eq!(
            yaml_value["repositories"][0]["id"].as_str(),
            Some("central")
        );

        let json_value: serde_json::Value =
            serde_json::from_str(&example_config(ConfigFormat::Json)).unwrap();
        assert_eq!(json_value["storage"]["root"].as_str(), Some("./repository"));
        assert_eq!(
            json_value["repositories"][0]["url"].as_str(),
            Some("https://repo.example/")
        );
    }

    #[test]
    fn finds_a_single_default_configuration_in_any_format() {
        let directory = TempDir::new().unwrap();
        let candidates = [
            (
                "maven-haste.json",
                "{\"storage\":{\"root\":\"repository\"},\"repositories\":[]}",
            ),
            (
                "maven-haste.toml",
                "[storage]\nroot = 'repository'\n\n[[repositories]]\n",
            ),
            (
                "maven-haste.yaml",
                "storage:\n  root: repository\nrepositories: []\n",
            ),
            (
                "maven-haste.yml",
                "storage:\n  root: repository\nrepositories: []\n",
            ),
        ];
        for (name, body) in candidates {
            let path = directory.path().join(name);
            fs::write(&path, body).unwrap();
            assert_eq!(
                find_default_config(directory.path(), None).unwrap(),
                dunce::canonicalize(&path).unwrap()
            );
            fs::remove_file(&path).unwrap();
        }

        let error = find_default_config(directory.path(), None).unwrap_err();
        assert!(
            error.to_string().contains("configuration file not found"),
            "{error}"
        );
    }

    #[test]
    fn rejects_multiple_default_configurations_in_one_directory() {
        let directory = TempDir::new().unwrap();
        fs::write(
            directory.path().join("maven-haste.json"),
            "{\"storage\":{\"root\":\"repository\"},\"repositories\":[]}",
        )
        .unwrap();
        fs::write(
            directory.path().join("maven-haste.toml"),
            "[storage]\nroot = 'repository'\n\n[[repositories]]\n",
        )
        .unwrap();

        let error = find_default_config(directory.path(), None).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("multiple configuration files found"),
            "{message}"
        );
        assert!(message.contains("maven-haste.json"), "{message}");
        assert!(message.contains("maven-haste.toml"), "{message}");
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
    fn rejects_repositories_that_share_one_url() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/maven2"

[[repositories]]
id = "mirror"
url = "https://repo.example/maven2"
"#,
        );

        let cli1 = &cli(&path);
        let error = Config::load(cli1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is already used by another repository"),
            "unexpected error: {error}"
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

    #[test]
    fn accepts_repository_ids_with_simple_naming() {
        let directory = TempDir::new().unwrap();
        for id in [
            "central",
            "kikugie_releases",
            "kikugie-snapshots",
            "a1-b2_c",
        ] {
            let path = write_config(
                &directory,
                &format!(
                    "[storage]\nroot = 'repository'\n\n[[repositories]]\nid = '{id}'\nurl = 'https://repo.example/'\n"
                ),
            );
            let cli1 = &cli(&path);
            assert!(
                Config::load(cli1).is_ok(),
                "expected repository id {id:?} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_repository_ids_outside_simple_naming() {
        let directory = TempDir::new().unwrap();
        for id in [
            "Fabric",
            "Maven Central",
            "Gradle-Plugin!",
            "with.dot",
            "with slash/inside",
            "带中文",
            "Gradle:Plugin",
            "super_long_repository_id_that_exceeds_the_sixty_four_character_limit",
        ] {
            let path = write_config(
                &directory,
                &format!(
                    "[storage]\nroot = 'repository'\n\n[[repositories]]\nid = '{id}'\nurl = 'https://repo.example/'\n"
                ),
            );
            let cli1 = &cli(&path);
            let error = Config::load(cli1).unwrap_err();
            assert!(
                error.to_string().contains("repository id"),
                "unexpected error for repository id {id:?}: {error}"
            );
        }
    }

    #[test]
    fn parses_cache_writes_default_true() {
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
        assert!(Config::load(cli1).unwrap().config.repositories[0].cache_writes);
    }

    #[test]
    fn parses_cache_writes_false() {
        let directory = TempDir::new().unwrap();
        let path = write_config(
            &directory,
            r#"
[storage]
root = "repository"

[[repositories]]
id = "central"
url = "https://repo.example/"
cache_writes = false
"#,
        );

        let cli1 = &cli(&path);
        assert!(!Config::load(cli1).unwrap().config.repositories[0].cache_writes);
    }

    #[test]
    fn committed_schema_matches_the_generated_schema() {
        // `maven-haste.schema.json` is generated by `config schema`; this test
        // guards against forgetting to regenerate it after a config change.
        let generated = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../maven-haste.schema.json")).unwrap();
        assert_eq!(generated, committed);
    }

    #[test]
    fn schema_describes_key_validation_rules() {
        let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
        let root = schema.as_object().unwrap();
        assert_eq!(root["additionalProperties"], serde_json::Value::Bool(false));
        assert_eq!(
            root["required"],
            serde_json::json!(["storage", "repositories"])
        );
        let defs = root["$defs"].as_object().unwrap();
        let server = defs["ServerConfig"].as_object().unwrap();
        assert!(server["properties"]["bind"]["pattern"].is_string());
        assert_eq!(
            server["properties"]["bind"]["examples"],
            serde_json::json!(["127.0.0.1:8080"])
        );
        assert_eq!(
            defs["CacheConfig"]["properties"]["metadata_ttl"]["type"],
            serde_json::json!("string")
        );
        assert_eq!(
            defs["RepositoryConfig"]["required"],
            serde_json::json!(["id", "url"])
        );
    }

    #[test]
    fn accepts_a_schema_key_in_json_and_toml() {
        let directory = TempDir::new().unwrap();
        let json = directory.path().join("maven-haste.json");
        fs::write(
            &json,
            r#"{"$schema": "./maven-haste.schema.json", "storage": {"root": "repository"}, "repositories": [{"id": "central", "url": "https://repo.example/maven2"}]}"#,
        )
        .unwrap();

        let loaded = Config::load(&cli(&json)).unwrap();
        assert_eq!(
            loaded.config.schema.as_deref(),
            Some("./maven-haste.schema.json")
        );
        assert_eq!(loaded.config.repositories[0].id, "central");

        let toml = write_config(
            &directory,
            "\"$schema\" = './maven-haste.schema.json'\n[storage]\nroot = 'repository'\n\n[[repositories]]\nid = 'central'\nurl = 'https://repo.example/maven2'\n",
        );
        let loaded = Config::load(&cli(&toml)).unwrap();
        assert_eq!(
            loaded.config.schema.as_deref(),
            Some("./maven-haste.schema.json")
        );
    }

    #[test]
    fn example_config_injects_the_current_version() {
        let example = example_config(ConfigFormat::Toml);
        let version = env!("CARGO_PKG_VERSION");
        let expected = format!(
            "https://raw.githubusercontent.com/Leawind/maven-haste/v{version}/maven-haste.schema.json"
        );
        assert!(
            example.contains(&expected),
            "missing pinned schema reference"
        );
        assert!(
            !example.contains("main/maven-haste.schema.json"),
            "schema reference must not point at main"
        );
        assert!(
            !example.contains("${VERSION}"),
            "placeholder must be injected"
        );
    }
}
