mod download;
mod install;
mod io;
mod types;

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use serde::Serialize;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::config::{CacheConfig, Config, StorageConfig};
use crate::db::{ArtifactRecord, Database};
use crate::error::AppError;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::UpstreamClient;

use io::{
    hash_file, internal, is_fresh, normalize_cache_prefix, relative_file_path, unix_timestamp,
};

type Flight = OnceCell<Result<(), CacheFailure>>;

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
    pub fn new(
        config: &Config,
        database: Database,
        case_sensitive: bool,
    ) -> Result<Self, AppError> {
        let upstream = UpstreamClient::new(
            config.repositories.clone(),
            &config.upstream,
            &config.circuit_breaker,
        )?;
        Ok(Self {
            inner: Arc::new(CacheInner {
                storage: config.storage.clone(),
                config: config.cache.clone(),
                database,
                upstream,
                case_sensitive,
                flights: DashMap::new(),
                refreshes: DashMap::new(),
                requests: AtomicU64::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                stale_hits: AtomicU64::new(0),
                negative_hits: AtomicU64::new(0),
            }),
        })
    }

    pub async fn get(&self, request: &MavenPath) -> Result<CachedArtifact, CacheFailure> {
        self.inner.requests.fetch_add(1, Ordering::Relaxed);
        let record = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;

        if request.policy() == CachePolicy::Mutable {
            if let Some(mut cached) = self.positive_cache(request, record.as_ref()).await? {
                let stale = !is_fresh(&cached.record, self.inner.config.metadata_ttl);
                if stale {
                    cached.status = CacheStatus::Stale;
                    self.inner.stale_hits.fetch_add(1, Ordering::Relaxed);
                    self.trigger_refresh(request.clone(), cached.record.clone());
                }
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(cached);
            }
            let negative = self.fresh_negative_entries(request.relative()).await?;
            if self
                .inner
                .upstream
                .all_candidates_negative(request.relative(), &negative)
            {
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                self.inner.negative_hits.fetch_add(1, Ordering::Relaxed);
                return Err(CacheFailure::NotFound);
            }
        } else if let Some(cached) = self.positive_cache(request, record.as_ref()).await? {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(cached);
        }

        self.inner.misses.fetch_add(1, Ordering::Relaxed);
        self.synchronous_download(request).await?;
        let record = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;
        let mut cached = self
            .positive_cache(request, record.as_ref())
            .await?
            .ok_or_else(|| {
                CacheFailure::Internal("download completed without a cache record".into())
            })?;
        cached.status = CacheStatus::Miss;
        Ok(cached)
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

    pub async fn remove_prefix(&self, prefix: &str) -> Result<RemovalStats, CacheFailure> {
        let prefix = normalize_cache_prefix(prefix)?;
        let records = self
            .inner
            .database
            .records_with_prefix(&prefix)
            .await
            .map_err(internal)?;
        self.remove_records(records).await
    }

    pub async fn verify(&self) -> Result<IntegrityReport, CacheFailure> {
        let records = self
            .inner
            .database
            .records_by_access()
            .await
            .map_err(internal)?;
        let mut report = IntegrityReport {
            checked: 0,
            issues: Vec::new(),
        };
        for record in records {
            report.checked += 1;
            let file_path = relative_file_path(&self.inner.storage.root, &record.path);
            match hash_file(&file_path).await {
                Ok((size, sha1, sha256, _)) => {
                    if size != record.file_size.max(0) as u64 {
                        report.issues.push(IntegrityIssue {
                            path: record.path.clone(),
                            reason: format!(
                                "size mismatch: database={}, file={size}",
                                record.file_size.max(0)
                            ),
                        });
                    } else if record
                        .sha256
                        .as_deref()
                        .is_some_and(|expected| expected != sha256)
                    {
                        report.issues.push(IntegrityIssue {
                            path: record.path.clone(),
                            reason: format!(
                                "SHA-256 mismatch: expected {}, found {sha256}",
                                record.sha256.as_deref().expect("checked above")
                            ),
                        });
                    } else if record
                        .sha1
                        .as_deref()
                        .is_some_and(|expected| expected != sha1)
                    {
                        report.issues.push(IntegrityIssue {
                            path: record.path,
                            reason: format!(
                                "SHA-1 mismatch: expected {}, found {sha1}",
                                record.sha1.as_deref().expect("checked above")
                            ),
                        });
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    report.issues.push(IntegrityIssue {
                        path: record.path,
                        reason: "file is missing".into(),
                    });
                }
                Err(error) => {
                    report.issues.push(IntegrityIssue {
                        path: record.path,
                        reason: format!("failed to read file: {error}"),
                    });
                }
            }
        }
        Ok(report)
    }

    async fn positive_cache(
        &self,
        request: &MavenPath,
        record: Option<&ArtifactRecord>,
    ) -> Result<Option<CachedArtifact>, CacheFailure> {
        let Some(record) = record else {
            return Ok(None);
        };
        let file_path = request.final_path(&self.inner.storage.root);
        match fs::metadata(&file_path).await {
            Ok(metadata) if metadata.is_file() => {
                let now = unix_timestamp();
                let mut record = record.clone();
                if record.last_accessed != now {
                    self.inner
                        .database
                        .touch_access(request.relative(), now)
                        .await
                        .map_err(internal)?;
                    record.last_accessed = now;
                }
                Ok(Some(CachedArtifact {
                    file_path,
                    record,
                    status: CacheStatus::Hit,
                }))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CacheFailure::Internal(format!(
                "failed to inspect cached file {}: {error}",
                file_path.display()
            ))),
        }
    }

    async fn synchronous_download(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        let key = request.relative().to_owned();
        let flight = self
            .inner
            .flights
            .entry(key.clone())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let result = flight
            .get_or_init(|| async { self.download_initial(request).await })
            .await
            .clone();
        self.remove_completed_flight(&key, &flight);
        result
    }

    async fn synchronous_main_download(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        let key = request.relative().to_owned();
        let flight = self
            .inner
            .flights
            .entry(key.clone())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let result = flight
            .get_or_init(|| async { self.download_main_initial(request).await })
            .await
            .clone();
        self.remove_completed_flight(&key, &flight);
        result
    }

    fn trigger_refresh(&self, request: MavenPath, record: ArtifactRecord) {
        let key = request.relative().to_owned();
        let Entry::Vacant(entry) = self.inner.refreshes.entry(key.clone()) else {
            return;
        };
        entry.insert(());

        let cache = self.clone();
        tokio::spawn(async move {
            let result = cache.refresh(&request, &record).await;
            if let Err(error) = result {
                tracing::warn!(path = request.relative(), %error, "background refresh failed");
                if let Err(touch_error) = cache
                    .inner
                    .database
                    .touch_refresh_attempt(request.relative(), unix_timestamp())
                    .await
                {
                    tracing::error!(
                        path = request.relative(),
                        error = %touch_error,
                        "failed to record refresh attempt"
                    );
                }
            }
            cache.inner.refreshes.remove(&key);
        });
    }

    fn remove_completed_flight(&self, key: &str, completed: &Arc<Flight>) {
        let same_flight = self
            .inner
            .flights
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current.value(), completed));
        if same_flight {
            self.inner.flights.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sha1::{Digest, Sha1};
    use sha2::Sha256;
    use tempfile::TempDir;
    use url::Url;

    use super::*;
    use crate::cache::io::relative_file_path;
    use crate::config::{
        CircuitBreakerConfig, LoggingConfig, RepositoryConfig, ServerConfig, UpstreamConfig,
    };

    async fn test_cache(directory: &TempDir, max_size: Option<u64>) -> (CacheManager, Database) {
        let storage = StorageConfig::resolved(directory.path().join("repository"));
        fs::create_dir_all(storage.tmp_dir()).await.unwrap();
        let config = Config {
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
                name: "test".into(),
                url: Url::parse("https://repo.example/").unwrap(),
                use_proxy: None,
                max_concurrency: None,
                rules: Vec::new(),
            }],
        };
        let database = Database::open(storage.db_path()).await.unwrap();
        (
            CacheManager::new(&config, database.clone(), true).unwrap(),
            database,
        )
    }

    async fn add_record(
        cache: &CacheManager,
        database: &Database,
        path: &str,
        content: &[u8],
        accessed: i64,
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
                upstream: "test".into(),
                sha1: Some(format!("{:x}", Sha1::digest(content))),
                sha256: Some(format!("{:x}", Sha256::digest(content))),
                etag: None,
                last_modified: None,
                file_size: content.len() as i64,
                created_at: accessed,
                last_refresh_attempt: None,
                last_accessed: accessed,
            }])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn evicts_oldest_idle_files_when_capacity_is_exceeded() {
        let directory = TempDir::new().unwrap();
        let (cache, database) = test_cache(&directory, Some(8)).await;
        add_record(&cache, &database, "com/example/old.jar", b"old!", 1).await;
        add_record(&cache, &database, "com/example/mid.jar", b"mid!", 2).await;
        add_record(&cache, &database, "com/example/new.jar", b"new!", 3).await;

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
        add_record(&cache, &database, "com/example/good.jar", b"good", 1).await;
        add_record(&cache, &database, "com/example/bad.jar", b"before", 2).await;
        add_record(&cache, &database, "org/example/keep.jar", b"keep", 3).await;
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
}
