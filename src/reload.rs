//! Configuration hot reload: watch the file the server started with and swap
//! reloadable settings without interrupting active requests.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;

use crate::cache::CacheManager;
use crate::config::{Config, LoadedConfig};
use crate::error::AppError;
use crate::upstream::UpstreamClient;

/// Metadata used to notice file changes. The pair is compared as a whole so
/// a replacement that preserves the timestamp but changes the length is still
/// picked up.
pub(crate) type Signature = Option<(SystemTime, u64)>;

/// Resolves the signature used to detect changes to the watched file.
pub(crate) async fn signature(path: &Path) -> Signature {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some((
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        metadata.len(),
    ))
}

/// Watches `path` until the returned task is aborted. Changes that cannot be
/// parsed or applied are reported and the previous configuration stays
/// active.
pub(crate) fn spawn(
    path: PathBuf,
    bind: Option<SocketAddr>,
    cache: CacheManager,
    mut current: Config,
    mut observed: Signature,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let signature = signature(&path).await;
            if signature == observed {
                continue;
            }
            observed = signature;
            if signature.is_none() {
                tracing::warn!(
                    path = %path.display(),
                    "configuration file was removed; keeping the previous configuration"
                );
                continue;
            }
            match reload_once(&path, bind, &cache, &current) {
                Ok((loaded, restart_required)) => {
                    for warning in &loaded.warnings {
                        tracing::warn!("{warning}");
                    }
                    if !restart_required.is_empty() {
                        tracing::warn!(
                            sections = %restart_required.join(", "),
                            "configuration changes require a restart and were not applied"
                        );
                    }
                    current = loaded.config;
                    tracing::info!(
                        path = %loaded.path.display(),
                        repositories = current.repositories.len(),
                        offline = current.upstream.offline,
                        "configuration file reloaded"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "configuration reload failed; keeping the previous configuration"
                    );
                }
            }
        }
    })
}

/// Loads and applies one configuration change. Kept separate from the watch
/// loop so tests can exercise both YAML and TOML without waiting for timers.
pub(crate) fn reload_once(
    path: &Path,
    bind: Option<SocketAddr>,
    cache: &CacheManager,
    previous: &Config,
) -> Result<(LoadedConfig, Vec<&'static str>), AppError> {
    let loaded = Config::load_path(path, bind)?;
    let restart_required = apply(cache, previous, &loaded.config)?;
    Ok((loaded, restart_required))
}

/// Applies every reloadable section, returning the sections that still
/// require a restart. The upstream client is built before anything is
/// swapped, so an invalid certificate, header, or repository leaves the
/// running configuration untouched.
fn apply(
    cache: &CacheManager,
    previous: &Config,
    next: &Config,
) -> Result<Vec<&'static str>, AppError> {
    let mut restart_required = Vec::new();
    if next.server != previous.server {
        restart_required.push("server");
    }
    if next.storage != previous.storage {
        restart_required.push("storage");
    }
    if next.logging != previous.logging {
        restart_required.push("logging");
    }

    let runtime_changed = next.cache != previous.cache
        || next.upstream != previous.upstream
        || next.circuit_breaker != previous.circuit_breaker
        || next.repositories != previous.repositories;
    if runtime_changed {
        let upstream = UpstreamClient::new(
            next.repositories.clone(),
            &next.upstream,
            &next.circuit_breaker,
        )?;
        let caching_disabled = next
            .repositories
            .iter()
            .filter(|repository| !repository.cache_writes)
            .map(|repository| repository.id.clone())
            .collect::<HashSet<_>>();
        cache.reload(next.cache.clone(), upstream, caching_disabled);
    }
    Ok(restart_required)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::cache::CacheManager;
    use crate::config::ConfigFormat;
    use crate::db::Database;

    const BASELINE_TOML: &str = r#"
[storage]
root = 'repository'

[[repositories]]
id = 'central'
url = 'https://repo.example/'
"#;

    const BASELINE_YAML: &str = r#"
storage:
    root: repository
repositories:
    - id: central
      url: https://repo.example/
"#;

    async fn build_cache(config: &Config) -> CacheManager {
        crate::storage::prepare(&config.storage).await.unwrap();
        let database = Database::open(config.storage.db_path()).await.unwrap();
        let upstream = UpstreamClient::new(
            config.repositories.clone(),
            &config.upstream,
            &config.circuit_breaker,
        )
        .unwrap();
        let caching_disabled = config
            .repositories
            .iter()
            .filter(|repository| !repository.cache_writes)
            .map(|repository| repository.id.clone())
            .collect();
        CacheManager::new(
            config.storage.clone(),
            config.cache.clone(),
            database,
            upstream,
            true,
            caching_disabled,
        )
    }

    async fn assert_reload(format: ConfigFormat, baseline: &str, changed: &str) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(format.default_file_name());
        fs::write(&path, baseline).unwrap();
        let initial = Config::load_path(&path, None).unwrap();
        assert_eq!(
            initial.config.cache.metadata_ttl,
            std::time::Duration::from_secs(5 * 60)
        );
        let cache = build_cache(&initial.config).await;

        fs::write(&path, changed).unwrap();
        let (loaded, restart_required) = reload_once(&path, None, &cache, &initial.config).unwrap();

        assert!(restart_required.is_empty(), "{restart_required:?}");
        assert_eq!(
            loaded.config.cache.metadata_ttl,
            std::time::Duration::from_secs(2)
        );
        assert_eq!(
            cache.cache_config().metadata_ttl,
            std::time::Duration::from_secs(2)
        );
        assert!(cache.upstream_is_offline());
        assert!(!cache.caching_enabled_for("central"));
    }

    #[tokio::test]
    async fn reloads_toml_settings() {
        assert_reload(
            ConfigFormat::Toml,
            BASELINE_TOML,
            r#"
[storage]
root = 'repository'

[cache]
metadata_ttl = '2s'

[upstream]
offline = true

[[repositories]]
id = 'central'
url = 'https://repo.example/'
cache_writes = false
"#,
        )
        .await;
    }

    #[tokio::test]
    async fn reloads_yaml_settings() {
        assert_reload(
            ConfigFormat::Yaml,
            BASELINE_YAML,
            r#"
storage:
    root: repository
cache:
    metadata_ttl: "2s"
upstream:
    offline: true
repositories:
    - id: central
      url: https://repo.example/
      cache_writes: false
"#,
        )
        .await;
    }

    #[tokio::test]
    async fn keeps_previous_settings_when_reloading_fails() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.toml");
        fs::write(&path, BASELINE_TOML).unwrap();
        let initial = Config::load_path(&path, None).unwrap();
        let cache = build_cache(&initial.config).await;

        fs::write(&path, "[cache]\nmetadata_ttl = 'not-a-duration'\n").unwrap();
        let error = reload_once(&path, None, &cache, &initial.config).unwrap_err();
        assert!(
            error.to_string().contains("failed to parse configuration"),
            "{error}"
        );
        assert_eq!(
            cache.cache_config().metadata_ttl,
            std::time::Duration::from_secs(5 * 60)
        );
        assert!(!cache.upstream_is_offline());
    }

    #[tokio::test]
    async fn reports_restart_only_sections_while_applying_other_settings() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.toml");
        fs::write(&path, BASELINE_TOML).unwrap();
        let initial = Config::load_path(&path, None).unwrap();
        let cache = build_cache(&initial.config).await;

        fs::write(
            &path,
            r#"
[server]
base_path = '/other'

[storage]
root = 'repository'

[cache]
metadata_ttl = '2s'

[[repositories]]
id = 'central'
url = 'https://repo.example/'
"#,
        )
        .unwrap();
        let (_, restart_required) = reload_once(&path, None, &cache, &initial.config).unwrap();

        assert_eq!(restart_required, vec!["server"]);
        assert_eq!(
            cache.cache_config().metadata_ttl,
            std::time::Duration::from_secs(2)
        );
    }

    #[tokio::test]
    async fn signature_changes_when_the_file_changes() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("maven-haste.toml");
        fs::write(&path, "first").unwrap();
        let first = signature(&path).await;
        fs::write(&path, "second-content").unwrap();
        let second = signature(&path).await;
        assert_ne!(first, second);
    }
}
