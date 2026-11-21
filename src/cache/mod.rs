mod download;
mod install;
pub(crate) mod io;
mod maintain;
mod serve;
mod types;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use dashmap::DashMap;
use serde::Serialize;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::cache::io::internal;
use crate::config::{CacheConfig, StorageConfig};
use crate::db::{ArtifactRecord, Database};
use crate::upstream::UpstreamClient;

use types::{DownloadOutcome, ServeRegistry};

pub use types::{ServeGuard, SharedTemp};

type Flight = OnceCell<Result<DownloadOutcome, CacheFailure>>;

/// Unreferenced objects younger than this may still be held by a response in
/// another process, so collection leaves them for a later pass.
const OBJECT_GRACE: Duration = Duration::from_secs(60 * 60);

/// Access counters are batched in memory and written to SQLite after this many
/// hits, keeping the cache-hit path off the database write lock.
const HIT_FLUSH_THRESHOLD: usize = 128;

#[derive(Default)]
struct PendingHit {
    count: u64,
    last_accessed: i64,
}

#[derive(Clone)]
pub struct CacheManager {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    storage: StorageConfig,
    runtime: RwLock<Arc<RuntimeConfig>>,
    database: Database,
    case_sensitive: bool,
    flights: DashMap<String, Arc<Flight>>,
    refreshes: DashMap<String, ()>,
    serves: ServeRegistry,
    served_objects: ServeRegistry,
    object_grace: Duration,
    pending_hits: Mutex<HashMap<String, PendingHit>>,
    pending_hit_ops: AtomicUsize,
    lifecycle: tokio::sync::RwLock<()>,
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_hits: AtomicU64,
    negative_hits: AtomicU64,
}

/// Settings that a configuration reload can replace as one unit. A fetch
/// takes a snapshot when it starts, so a reload never mixes old and new
/// upstream rules within one download.
struct RuntimeConfig {
    cache: CacheConfig,
    upstream: Arc<UpstreamClient>,
    caching_disabled: HashSet<String>,
}

#[derive(Debug)]
pub struct CachedArtifact {
    pub file_path: PathBuf,
    /// The already verified file handle; responses must not reopen its path.
    pub file: Option<fs::File>,
    pub record: ArtifactRecord,
    pub status: CacheStatus,
    /// Shared temporary file to release after the response body is fully
    /// consumed; the file is removed when the last response releases it.
    pub temporary: Option<SharedTemp>,
    /// Marks the cached file as being served so capacity eviction and prefix
    /// removal skip it until the response finishes.
    pub serve_guard: Option<ServeGuard>,
    /// Marks the backing content object as in use so object collection skips
    /// it even after its record was replaced.
    pub object_guard: Option<ServeGuard>,
}

/// Headers learned from an upstream HEAD response when the artifact is not
/// cached. The body is deliberately not downloaded.
#[derive(Clone, Debug)]
pub struct UpstreamHead {
    pub repository: String,
    pub content_length: Option<u64>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub enum HeadOutcome {
    Cached(Box<CachedArtifact>),
    Upstream(UpstreamHead),
    NotFound,
    Gateway,
    Offline,
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
    pub object_files: u64,
    pub object_bytes: u64,
    pub recorded_requests: u64,
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
    #[error("offline mode: the artifact is not cached")]
    Offline,
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
        Self::new_with_object_grace(
            storage,
            cache,
            database,
            upstream,
            case_sensitive,
            caching_disabled,
            OBJECT_GRACE,
        )
    }

    fn new_with_object_grace(
        storage: StorageConfig,
        cache: CacheConfig,
        database: Database,
        upstream: UpstreamClient,
        case_sensitive: bool,
        caching_disabled: HashSet<String>,
        object_grace: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(CacheInner {
                storage,
                runtime: RwLock::new(Arc::new(RuntimeConfig {
                    cache,
                    upstream: Arc::new(upstream),
                    caching_disabled,
                })),
                database,
                case_sensitive,
                flights: DashMap::new(),
                refreshes: DashMap::new(),
                serves: ServeRegistry::default(),
                served_objects: ServeRegistry::default(),
                object_grace,
                pending_hits: Mutex::new(HashMap::new()),
                pending_hit_ops: AtomicUsize::new(0),
                lifecycle: tokio::sync::RwLock::new(()),
                requests: AtomicU64::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                stale_hits: AtomicU64::new(0),
                negative_hits: AtomicU64::new(0),
            }),
        }
    }

    /// Returns the current reloadable settings. A snapshot keeps one set of
    /// upstream rules for the operation that holds it.
    fn runtime(&self) -> Arc<RuntimeConfig> {
        self.inner
            .runtime
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replaces all reloadable settings at once. Storage paths, the listen
    /// address, the endpoint prefix, and logging are fixed at startup.
    pub(crate) fn reload(
        &self,
        cache: CacheConfig,
        upstream: UpstreamClient,
        caching_disabled: HashSet<String>,
    ) {
        let runtime = RuntimeConfig {
            cache,
            upstream: Arc::new(upstream),
            caching_disabled,
        };
        *self
            .inner
            .runtime
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(runtime);
    }

    pub(crate) fn maintenance_interval(&self) -> Duration {
        self.runtime().cache.maintenance_interval
    }

    #[cfg(test)]
    pub(crate) fn cache_config(&self) -> CacheConfig {
        self.runtime().cache.clone()
    }

    #[cfg(test)]
    pub(crate) fn upstream_is_offline(&self) -> bool {
        self.runtime().upstream.offline()
    }

    #[cfg(test)]
    pub(crate) fn caching_enabled_for(&self, id: &str) -> bool {
        self.caching_enabled(id)
    }

    /// Whether new cache entries may be written for artifacts fetched from `id`.
    fn caching_enabled(&self, id: &str) -> bool {
        !self.runtime().caching_disabled.contains(id)
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
        self.flush_hits().await;
        let database = self.inner.database.stats().await.map_err(internal)?;
        let runtime = self.runtime();
        let requests = self.inner.requests.load(Ordering::Relaxed);
        let hits = self.inner.hits.load(Ordering::Relaxed);
        Ok(CacheStats {
            files: database.files,
            total_size: database.total_size,
            object_files: database.object_files,
            object_bytes: database.object_size,
            recorded_requests: database.recorded_requests,
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
            max_size: runtime.cache.max_size,
            upstreams: runtime.upstream.statuses(),
        })
    }

    /// Writes pending access counters to the database. Called after enough
    /// hits accumulate and before statistics are read.
    pub(crate) async fn flush_hits(&self) {
        let hits = {
            let mut pending = self
                .inner
                .pending_hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.inner.pending_hit_ops.store(0, Ordering::Relaxed);
            if pending.is_empty() {
                return;
            }
            pending
                .drain()
                .map(|(path, hit)| (path, hit.count, hit.last_accessed))
                .collect::<Vec<_>>()
        };
        if let Err(error) = self.inner.database.record_hits(hits).await {
            tracing::warn!(%error, "failed to record cached-access statistics");
        }
    }
}

#[cfg(test)]
mod tests {
    use sha1::{Digest as _, Sha1};
    use sha2::Sha256;
    use tempfile::TempDir;
    use url::Url;

    use super::*;
    use crate::cache::io::{relative_file_path, to_hex};
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
                headers: Default::default(),
                username: None,
                password: None,
                bearer_token: None,
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
            CacheManager::new_with_object_grace(
                storage.clone(),
                config.cache.clone(),
                database.clone(),
                upstream,
                true,
                caching_disabled,
                std::time::Duration::ZERO,
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
                sha1: Some(to_hex(&Sha1::digest(content))),
                sha256: Some(to_hex(&Sha256::digest(content))),
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
    async fn failed_database_publication_keeps_all_previous_records_and_bytes() {
        use crate::cache::io::{temporary_path, write_temporary};
        use crate::cache::types::PreparedFile;
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, None).await;
        let path = "com/example/demo/1.0/demo-1.0.jar";
        let checksum_path = format!("{path}.sha1");
        add_record(&cache, &database, path, b"old-body", 1, "test").await;
        add_record(
            &cache,
            &database,
            &checksum_path,
            b"old-checksum",
            1,
            "test",
        )
        .await;
        let mut files = Vec::new();
        for (relative, content) in [
            (path, &b"new-body"[..]),
            (checksum_path.as_str(), &b"new-checksum"[..]),
        ] {
            let mut record = database.get(relative).await.unwrap().unwrap();
            record.sha1 = Some(to_hex(&Sha1::digest(content)));
            record.sha256 = Some(to_hex(&Sha256::digest(content)));
            record.file_size = content.len() as i64;
            let temporary = temporary_path(cache.inner.storage.tmp_dir());
            write_temporary(&temporary, content).await.unwrap();
            files.push(PreparedFile {
                relative: relative.into(),
                temporary,
                record,
            });
        }
        let connection = rusqlite::Connection::open(cache.inner.storage.db_path()).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_publication BEFORE INSERT ON artifacts WHEN NEW.path LIKE '%.sha1' BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;").unwrap();
        assert!(cache.install_bundle(files).await.is_err());
        for (relative, content) in [
            (path, &b"old-body"[..]),
            (checksum_path.as_str(), &b"old-checksum"[..]),
        ] {
            let record = database.get(relative).await.unwrap().unwrap();
            assert_eq!(record.sha256, Some(to_hex(&Sha256::digest(content))));
            let request = MavenPath::parse(&format!("/maven/{relative}"), "/maven").unwrap();
            let cached = cache.get(&request).await.unwrap();
            assert_eq!(fs::read(&cached.file_path).await.unwrap(), content);
        }
    }

    #[tokio::test]
    async fn object_collection_keeps_readers_and_shared_references() {
        use crate::cache::io::object_path;
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, None).await;
        for path in ["com/example/first.jar", "org/example/second.jar"] {
            add_record(&cache, &database, path, b"shared", 1, "test").await;
        }
        let shared = object_path(
            &cache.inner.storage.root,
            &to_hex(&Sha256::digest(b"shared")),
        )
        .unwrap();
        let orphan = object_path(
            &cache.inner.storage.root,
            &to_hex(&Sha256::digest(b"orphan")),
        )
        .unwrap();
        fs::create_dir_all(shared.parent().unwrap()).await.unwrap();
        fs::write(&shared, b"shared").await.unwrap();
        fs::write(&orphan, b"orphan").await.unwrap();
        let orphan_hash = to_hex(&Sha256::digest(b"orphan"));
        let serving = cache.inner.served_objects.acquire(&orphan_hash);
        cache.run_maintenance().await.unwrap();
        assert!(orphan.exists());
        drop(serving);
        cache.run_maintenance().await.unwrap();
        assert!(!orphan.exists());
        cache.remove_prefix("com/example").await.unwrap();
        assert!(shared.exists());
        cache.remove_prefix("org/example").await.unwrap();
        assert!(!shared.exists());
    }

    #[tokio::test]
    async fn publication_finishes_when_the_requesting_task_is_canceled() {
        use crate::cache::io::{lock_objects, temporary_path, write_temporary};
        use crate::cache::types::PreparedFile;
        use std::time::Duration;
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, None).await;
        let path = "com/example/demo/1.0/demo-1.0.jar";
        add_record(&cache, &database, path, b"old-body", 1, "test").await;
        let mut record = database.get(path).await.unwrap().unwrap();
        record.sha1 = Some(to_hex(&Sha1::digest(b"new-body")));
        record.sha256 = Some(to_hex(&Sha256::digest(b"new-body")));
        let temporary = temporary_path(cache.inner.storage.tmp_dir());
        write_temporary(&temporary, b"new-body").await.unwrap();
        // Hold the same OS lock that another server or CLI process uses.
        let blocker = lock_objects(&cache.inner.storage.root).await.unwrap();
        let installing = cache.clone();
        let task = tokio::spawn(async move {
            installing
                .install_bundle(vec![PreparedFile {
                    relative: path.into(),
                    temporary,
                    record,
                }])
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while cache.inner.lifecycle.try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(blocker);
        let expected = to_hex(&Sha256::digest(b"new-body"));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if database.get(path).await.unwrap().unwrap().sha256.as_deref() == Some(&expected) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let request = MavenPath::parse(&format!("/maven/{path}"), "/maven").unwrap();
        let cached = cache.get(&request).await.unwrap();
        assert_eq!(fs::read(&cached.file_path).await.unwrap(), b"new-body");
    }

    #[tokio::test]
    async fn maintenance_pass_enforces_capacity_and_purges_expired_negatives() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, Some(8)).await;
        add_record(&cache, &database, "com/example/old.jar", b"old!", 1, "test").await;
        add_record(
            &cache,
            &database,
            "com/example/new.jar",
            b"newnew!!",
            2,
            "test",
        )
        .await;
        database
            .upsert_negative_entries(
                "com/example/demo/maven-metadata.xml",
                vec!["test".into()],
                1,
            )
            .await
            .unwrap();

        cache.run_maintenance().await.unwrap();

        assert!(database.get("com/example/old.jar").await.unwrap().is_none());
        assert!(database.get("com/example/new.jar").await.unwrap().is_some());
        assert_eq!(
            database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .len(),
            0,
            "expired negative entries are purged"
        );
        assert_eq!(database.stats().await.unwrap().total_size, 8);
    }

    #[tokio::test]
    async fn eviction_skips_files_that_are_currently_being_served() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, Some(8)).await;
        add_record(&cache, &database, "com/example/old.jar", b"old!", 1, "test").await;
        add_record(&cache, &database, "com/example/mid.jar", b"mid!", 2, "test").await;
        add_record(&cache, &database, "com/example/new.jar", b"new!", 3, "test").await;

        let serving = cache.inner.serves.acquire("com/example/old.jar");
        cache.enforce_capacity(&HashSet::new()).await.unwrap();

        assert!(database.get("com/example/old.jar").await.unwrap().is_some());
        assert!(database.get("com/example/mid.jar").await.unwrap().is_none());
        assert!(database.get("com/example/new.jar").await.unwrap().is_some());
        drop(serving);
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
    async fn batches_access_updates_until_flush() {
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

        assert_eq!(
            database
                .get("com/example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .unwrap()
                .request_count,
            0,
            "access counters are buffered"
        );
        cache.flush_hits().await;
        assert_eq!(
            database
                .get("com/example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .unwrap()
                .request_count,
            2
        );
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
        cache.flush_hits().await;

        let record = database
            .get("com/example/demo/1.0/demo-1.0.jar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.request_count, 2);
    }
}
