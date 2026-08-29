mod download;
mod install;
mod io;
mod maintain;
mod serve;
mod types;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::Serialize;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::cache::io::internal;
use crate::config::{CacheConfig, StorageConfig};
use crate::db::{ArtifactRecord, Database};
use crate::upstream::UpstreamClient;

use types::DownloadOutcome;

type Flight = OnceCell<Result<DownloadOutcome, CacheFailure>>;

#[derive(Clone)]
pub struct CacheManager {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    storage: StorageConfig,
    config: CacheConfig,
    database: Database,
    upstream: UpstreamClient,
    case_sensitive: bool,
    caching_disabled: HashSet<String>,
    flights: DashMap<String, Arc<Flight>>,
    refreshes: DashMap<String, ()>,
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_hits: AtomicU64,
    negative_hits: AtomicU64,
}

#[derive(Debug)]
pub struct CachedArtifact {
    pub file_path: PathBuf,
    pub record: ArtifactRecord,
    pub status: CacheStatus,
    /// Temporary file to remove after the response body is fully consumed.
    pub temporary: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheStatus {
    Hit,
    Miss,
    Stale,
}

impl CacheStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Stale => "stale",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CacheStats {
    pub files: u64,
    pub total_size: u64,
    pub negative_entries: u64,
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub stale_hits: u64,
    pub negative_hits: u64,
    pub hit_rate: f64,
    pub max_size: Option<u64>,
    pub upstreams: Vec<crate::upstream::UpstreamStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemovalStats {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct IntegrityReport {
    pub checked: u64,
    pub issues: Vec<IntegrityIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IntegrityIssue {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum CacheFailure {
    #[error("artifact was not found in any upstream repository")]
    NotFound,
    #[error("all eligible upstream repositories failed")]
    Gateway,
    #[error("cache operation failed: {0}")]
    Internal(String),
}

impl CacheManager {
    /// Assembles a cache manager from independently constructed components.
    /// The runtime assembly site (the `run` command) builds each component
    /// from a loaded configuration, so a future configuration reload can
    /// rebuild and swap them without restarting the process.
    pub fn new(
        storage: StorageConfig,
        cache: CacheConfig,
        database: Database,
        upstream: UpstreamClient,
        case_sensitive: bool,
        caching_disabled: HashSet<String>,
    ) -> Self {
        Self {
            inner: Arc::new(CacheInner {
                storage,
                config: cache,
                database,
                upstream,
                case_sensitive,
                caching_disabled,
                flights: DashMap::new(),
                refreshes: DashMap::new(),
                requests: AtomicU64::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                stale_hits: AtomicU64::new(0),
                negative_hits: AtomicU64::new(0),
            }),
        }
    }

    /// Whether new cache entries may be written for artifacts fetched from `id`.
    fn caching_enabled(&self, id: &str) -> bool {
        !self.inner.caching_disabled.contains(id)
    }

    pub async fn health(&self) -> Result<(), CacheFailure> {
        self.inner.database.ping().await.map_err(internal)?;
        for path in [&self.inner.storage.root, self.inner.storage.tmp_dir()] {
            let metadata = fs::metadata(path).await.map_err(|error| {
                CacheFailure::Internal(format!(
                    "failed to inspect storage path {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_dir() {
                return Err(CacheFailure::Internal(format!(
                    "storage path {} is not a directory",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    pub async fn stats(&self) -> Result<CacheStats, CacheFailure> {
        let database = self.inner.database.stats().await.map_err(internal)?;
        let requests = self.inner.requests.load(Ordering::Relaxed);
        let hits = self.inner.hits.load(Ordering::Relaxed);
        Ok(CacheStats {
            files: database.files,
            total_size: database.total_size,
            negative_entries: database.negative_entries,
            requests,
            hits,
            misses: self.inner.misses.load(Ordering::Relaxed),
            stale_hits: self.inner.stale_hits.load(Ordering::Relaxed),
            negative_hits: self.inner.negative_hits.load(Ordering::Relaxed),
            hit_rate: if requests == 0 {
                0.0
            } else {
                hits as f64 / requests as f64
            },
            max_size: self.inner.config.max_size,
            upstreams: self.inner.upstream.statuses(),
        })
    }

    pub fn route_candidates(&self, path: &str) -> Vec<String> {
        self.inner.upstream.candidate_names(path)
    }
}

#[cfg(test)]
mod tests {
    use sha1::{Digest, Sha1};
    use sha2::Sha256;
    use tempfile::TempDir;
    use url::Url;

    use super::*;
    use crate::cache::io::relative_file_path;
    use crate::config::{
        CircuitBreakerConfig, Config, LoggingConfig, RepositoryConfig, ServerConfig, UpstreamConfig,
    };
    use crate::request_path::MavenPath;

    async fn test_cache(directory: &TempDir, max_size: Option<u64>) -> (CacheManager, Database) {
        test_cache_with_repository(directory, max_size, "test", true).await
    }

    async fn test_cache_with_repository(
        directory: &TempDir,
        max_size: Option<u64>,
        id: &str,
        cache_writes: bool,
    ) -> (CacheManager, Database) {
        let storage = StorageConfig::resolved(directory.path().join("repository"));
        fs::create_dir_all(storage.tmp_dir()).await.unwrap();
        let config = Config {
            schema: None,
            server: ServerConfig::default(),
            storage: storage.clone(),
            cache: CacheConfig {
                max_size,
                ..CacheConfig::default()
            },
            upstream: UpstreamConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            logging: LoggingConfig::default(),
            repositories: vec![RepositoryConfig {
                id: id.into(),
                url: Url::parse("https://repo.example/").unwrap(),
                use_proxy: None,
                max_concurrency: None,
                rules: Vec::new(),
                cache_writes,
            }],
        };
        let database = Database::open(storage.db_path()).await.unwrap();
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
        (
            CacheManager::new(
                storage.clone(),
                config.cache.clone(),
                database.clone(),
                upstream,
                true,
                caching_disabled,
            ),
            database,
        )
    }

    async fn add_record(
        cache: &CacheManager,
        database: &Database,
        path: &str,
        content: &[u8],
        accessed: i64,
        upstream: &str,
    ) {
        let file_path = relative_file_path(&cache.inner.storage.root, path);
        fs::create_dir_all(file_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&file_path, content).await.unwrap();
        database
            .upsert_many(vec![ArtifactRecord {
                path: path.into(),
                group_id: "com.example".into(),
                artifact_id: "demo".into(),
                version: "1.0".into(),
                file_type: "jar".into(),
                upstream: upstream.into(),
                sha1: Some(format!("{:x}", Sha1::digest(content))),
                sha256: Some(format!("{:x}", Sha256::digest(content))),
                etag: None,
                last_modified: None,
                file_size: content.len() as i64,
                created_at: accessed,
                last_refresh_attempt: None,
                last_accessed: accessed,
                request_count: 0,
            }])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn evicts_oldest_idle_files_when_capacity_is_exceeded() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, Some(8)).await;
        add_record(&cache, &database, "com/example/old.jar", b"old!", 1, "test").await;
        add_record(&cache, &database, "com/example/mid.jar", b"mid!", 2, "test").await;
        add_record(&cache, &database, "com/example/new.jar", b"new!", 3, "test").await;

        cache.enforce_capacity(&HashSet::new()).await.unwrap();

        assert!(database.get("com/example/old.jar").await.unwrap().is_none());
        assert!(database.get("com/example/mid.jar").await.unwrap().is_some());
        assert!(database.get("com/example/new.jar").await.unwrap().is_some());
        assert_eq!(database.stats().await.unwrap().total_size, 8);
    }

    #[tokio::test]
    async fn removes_prefixes_and_reports_integrity_issues() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, None).await;
        add_record(
            &cache,
            &database,
            "com/example/good.jar",
            b"good",
            1,
            "test",
        )
        .await;
        add_record(
            &cache,
            &database,
            "com/example/bad.jar",
            b"before",
            2,
            "test",
        )
        .await;
        add_record(
            &cache,
            &database,
            "org/example/keep.jar",
            b"keep",
            3,
            "test",
        )
        .await;
        fs::write(
            relative_file_path(&cache.inner.storage.root, "com/example/bad.jar"),
            b"after",
        )
        .await
        .unwrap();

        let report = cache.verify().await.unwrap();
        assert_eq!(report.checked, 3);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].path, "com/example/bad.jar");

        let removed = cache.remove_prefix("/com/example/").await.unwrap();
        assert_eq!(removed.files, 2);
        assert!(
            database
                .get("com/example/good.jar")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            database
                .get("org/example/keep.jar")
                .await
                .unwrap()
                .is_some()
        );
        assert!(cache.remove_prefix("../repository").await.is_err());
    }

    #[tokio::test]
    async fn serves_cached_entries_when_repository_writes_are_disabled() {
        let directory = TempDir::new().unwrap();
        let (cache, database) =
            test_cache_with_repository(&directory, None, "nocache", false).await;
        add_record(
            &cache,
            &database,
            "com/example/demo/1.0/demo-1.0.jar",
            b"cached",
            1,
            "nocache",
        )
        .await;

        let request =
            MavenPath::parse("/maven/com/example/demo/1.0/demo-1.0.jar", "/maven").unwrap();
        let artifact = cache.get(&request).await.unwrap();

        assert_eq!(artifact.status, CacheStatus::Hit);
        assert_eq!(artifact.record.upstream, "nocache");
        assert!(artifact.temporary.is_none());
        assert!(
            database
                .get("com/example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn counts_requests_for_cached_entries() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, None).await;
        add_record(
            &cache,
            &database,
            "com/example/demo/1.0/demo-1.0.jar",
            b"cached",
            1,
            "test",
        )
        .await;

        let request =
            MavenPath::parse("/maven/com/example/demo/1.0/demo-1.0.jar", "/maven").unwrap();
        cache.get(&request).await.unwrap();
        cache.get(&request).await.unwrap();

        let record = database
            .get("com/example/demo/1.0/demo-1.0.jar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.request_count, 2);
    }
}
