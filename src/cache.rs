use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use futures_util::StreamExt;
use reqwest::header::{ETAG, LAST_MODIFIED};
use serde::Serialize;
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::config::{CacheConfig, Config, StorageConfig};
use crate::db::{ArtifactRecord, Database, NegativeCacheEntry};
use crate::error::AppError;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::{FetchResult, RequestPriority, UpstreamClient, UpstreamResponse};

const MAX_CHECKSUM_BYTES: usize = 64 * 1024;
const MAX_CHECKSUM_VALIDATION_ATTEMPTS: usize = 3;
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

enum PreparedFetch {
    Bundle(Vec<PreparedFile>),
    NotModified,
    NotFound,
    Gateway,
}

struct PreparedFile {
    relative: String,
    temporary: PathBuf,
    record: ArtifactRecord,
}

struct DownloadedMain {
    temporary: PathBuf,
    size: u64,
    sha1: String,
    sha256: String,
    sha512: String,
    etag: Option<String>,
    last_modified: Option<String>,
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

    async fn download_initial(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        if let Some(source) = request.generated_checksum_source() {
            self.synchronous_main_download(&source).await?;
            let record = self
                .inner
                .database
                .get(request.relative())
                .await
                .map_err(internal)?;
            if self
                .positive_cache(request, record.as_ref())
                .await?
                .is_some()
            {
                return Ok(());
            }
            return self.regenerate_checksum(request, &source).await;
        }
        self.download_main_initial(request).await
    }

    async fn regenerate_checksum(
        &self,
        request: &MavenPath,
        source: &MavenPath,
    ) -> Result<(), CacheFailure> {
        let source_record = self
            .inner
            .database
            .get(source.relative())
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                CacheFailure::Internal(format!(
                    "cached checksum source {} has no database record",
                    source.relative()
                ))
            })?;
        let source_path = source.final_path(&self.inner.storage.root);
        let (_, sha1, sha256, sha512) = hash_file(&source_path).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to hash cached checksum source {}: {error}",
                source_path.display()
            ))
        })?;
        let expected = match request.file_type.as_str() {
            "sha1" => sha1,
            "sha256" => sha256,
            "sha512" => sha512,
            extension => {
                return Err(CacheFailure::Internal(format!(
                    "cannot generate unsupported checksum type {extension}"
                )));
            }
        };
        let content = format!("{expected}\n");
        let temporary = temporary_path(self.inner.storage.tmp_dir());
        write_temporary(&temporary, content.as_bytes()).await?;
        let now = unix_timestamp();
        self.install_bundle(vec![PreparedFile {
            relative: request.relative().into(),
            temporary,
            record: ArtifactRecord {
                path: request.relative().into(),
                group_id: request.group_id.clone(),
                artifact_id: request.artifact_id.clone(),
                version: request.version.clone(),
                file_type: request.file_type.clone(),
                upstream: source_record.upstream,
                sha1: None,
                sha256: Some(format!("{:x}", Sha256::digest(content.as_bytes()))),
                etag: None,
                last_modified: None,
                file_size: content.len() as i64,
                created_at: now,
                last_refresh_attempt: (request.policy() == CachePolicy::Mutable).then_some(now),
                last_accessed: now,
            },
        }])
        .await
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

    async fn download_main_initial(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        let existing = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;
        if self
            .positive_cache(request, existing.as_ref())
            .await?
            .is_some()
        {
            return Ok(());
        }
        match self.prepare_fetch(request, None).await? {
            PreparedFetch::Bundle(files) => self.install_bundle(files).await,
            PreparedFetch::NotFound => Err(CacheFailure::NotFound),
            PreparedFetch::Gateway | PreparedFetch::NotModified => Err(CacheFailure::Gateway),
        }
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

    async fn refresh(
        &self,
        request: &MavenPath,
        previous: &ArtifactRecord,
    ) -> Result<(), CacheFailure> {
        match self.prepare_fetch(request, Some(previous)).await? {
            PreparedFetch::Bundle(files) => self.install_bundle(files).await,
            PreparedFetch::NotModified => self
                .inner
                .database
                .touch_refresh_attempt(request.relative(), unix_timestamp())
                .await
                .map_err(internal),
            PreparedFetch::NotFound => self
                .inner
                .database
                .touch_refresh_attempt(request.relative(), unix_timestamp())
                .await
                .map_err(internal),
            PreparedFetch::Gateway => Err(CacheFailure::Gateway),
        }
    }

    async fn prepare_fetch(
        &self,
        request: &MavenPath,
        previous: Option<&ArtifactRecord>,
    ) -> Result<PreparedFetch, CacheFailure> {
        let priority = if previous.is_some() {
            RequestPriority::Background
        } else {
            RequestPriority::Foreground
        };
        let mut excluded = HashSet::new();
        let mut upstream_failure = false;
        let mut negative = if request.policy() == CachePolicy::Mutable {
            self.fresh_negative_entries(request.relative()).await?
        } else {
            HashSet::new()
        };

        loop {
            let outcome = if let Some(previous) = previous {
                self.inner
                    .upstream
                    .refresh(
                        request.relative(),
                        &previous.upstream,
                        previous.etag.as_deref(),
                        previous.last_modified.as_deref(),
                        &excluded,
                        &negative,
                    )
                    .await
            } else {
                self.inner
                    .upstream
                    .fetch(request.relative(), &excluded, &negative)
                    .await
            };

            if request.policy() == CachePolicy::Mutable && !outcome.not_found.is_empty() {
                self.inner
                    .database
                    .upsert_negative_entries(
                        request.relative(),
                        outcome.not_found.clone(),
                        unix_timestamp(),
                    )
                    .await
                    .map_err(internal)?;
                negative.extend(outcome.not_found);
            }

            match outcome.result {
                FetchResult::Found {
                    repository,
                    repository_id,
                    response,
                } => {
                    if request.policy() == CachePolicy::Mutable {
                        self.inner
                            .database
                            .delete_negative_entry(request.relative(), &repository_id)
                            .await
                            .map_err(internal)?;
                    }
                    let downloaded = match self.download_main(&repository, response).await {
                        Ok(downloaded) => downloaded,
                        Err(error) => {
                            upstream_failure = true;
                            self.inner.upstream.record_body_failure(&repository);
                            excluded.insert(repository);
                            tracing::warn!(%error, "discarding failed upstream download");
                            continue;
                        }
                    };
                    match self
                        .prepare_bundle(request, &repository, downloaded, priority)
                        .await
                    {
                        Ok(files) => return Ok(PreparedFetch::Bundle(files)),
                        Err(BundleError::Unstable(error)) => {
                            upstream_failure = true;
                            excluded.insert(repository.clone());
                            self.inner.upstream.record_body_failure(&repository);
                            tracing::warn!(upstream = %repository, %error, "upstream content remained unstable");
                        }
                        Err(BundleError::Internal(error)) => return Err(error),
                    }
                }
                FetchResult::NotModified { repository_id } => {
                    if request.policy() == CachePolicy::Mutable {
                        self.inner
                            .database
                            .delete_negative_entry(request.relative(), &repository_id)
                            .await
                            .map_err(internal)?;
                    }
                    return Ok(PreparedFetch::NotModified);
                }
                FetchResult::NotFound if upstream_failure => return Ok(PreparedFetch::Gateway),
                FetchResult::NotFound => return Ok(PreparedFetch::NotFound),
                FetchResult::GatewayFailure => return Ok(PreparedFetch::Gateway),
            }
        }
    }

    async fn download_main(
        &self,
        repository: &str,
        response: UpstreamResponse,
    ) -> Result<DownloadedMain, CacheFailure> {
        let etag = header_string(response.headers(), ETAG);
        let last_modified = header_string(response.headers(), LAST_MODIFIED);
        let temporary = temporary_path(self.inner.storage.tmp_dir());
        match stream_to_file(response, &temporary).await {
            Ok((size, sha1, sha256, sha512)) => {
                self.inner.upstream.record_body_success(repository);
                Ok(DownloadedMain {
                    temporary,
                    size,
                    sha1,
                    sha256,
                    sha512,
                    etag,
                    last_modified,
                })
            }
            Err(error) => {
                let _ = remove_file_if_exists(&temporary).await;
                Err(error)
            }
        }
    }

    async fn prepare_bundle(
        &self,
        request: &MavenPath,
        repository: &str,
        downloaded: DownloadedMain,
        priority: RequestPriority,
    ) -> Result<Vec<PreparedFile>, BundleError> {
        let downloaded = self
            .select_stable_download(request, repository, downloaded, priority)
            .await?;
        let now = unix_timestamp();
        let refresh_attempt = (request.policy() == CachePolicy::Mutable).then_some(now);
        let main_record = ArtifactRecord {
            path: request.relative().into(),
            group_id: request.group_id.clone(),
            artifact_id: request.artifact_id.clone(),
            version: request.version.clone(),
            file_type: request.file_type.clone(),
            upstream: repository.into(),
            sha1: Some(downloaded.sha1.clone()),
            sha256: Some(downloaded.sha256.clone()),
            etag: downloaded.etag,
            last_modified: downloaded.last_modified,
            file_size: i64::try_from(downloaded.size).unwrap_or(i64::MAX),
            created_at: now,
            last_refresh_attempt: refresh_attempt,
            last_accessed: now,
        };
        let mut files = vec![PreparedFile {
            relative: request.relative().into(),
            temporary: downloaded.temporary,
            record: main_record,
        }];

        if request.is_checksum() {
            return Ok(files);
        }

        for (extension, expected) in [
            ("sha1", &downloaded.sha1),
            ("sha256", &downloaded.sha256),
            ("sha512", &downloaded.sha512),
        ] {
            let relative = format!("{}.{extension}", request.relative());
            let content = format!("{expected}\n");
            let content_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
            let temporary = temporary_path(self.inner.storage.tmp_dir());
            if let Err(error) = write_temporary(&temporary, content.as_bytes()).await {
                cleanup_prepared(&files).await;
                return Err(BundleError::Internal(error));
            }
            files.push(PreparedFile {
                relative: relative.clone(),
                temporary,
                record: ArtifactRecord {
                    path: relative,
                    group_id: request.group_id.clone(),
                    artifact_id: request.artifact_id.clone(),
                    version: request.version.clone(),
                    file_type: extension.into(),
                    upstream: repository.into(),
                    sha1: None,
                    sha256: Some(content_sha256),
                    etag: None,
                    last_modified: None,
                    file_size: i64::try_from(content.len()).unwrap_or(i64::MAX),
                    created_at: now,
                    last_refresh_attempt: refresh_attempt,
                    last_accessed: now,
                },
            });
        }
        Ok(files)
    }

    async fn select_stable_download(
        &self,
        request: &MavenPath,
        repository: &str,
        initial: DownloadedMain,
        priority: RequestPriority,
    ) -> Result<DownloadedMain, BundleError> {
        if request.is_checksum() {
            return Ok(initial);
        }

        let mut rejected = Vec::new();
        let mut candidate = initial;
        for attempt in 1..=MAX_CHECKSUM_VALIDATION_ATTEMPTS {
            let issues = self
                .checksum_issues(request, repository, &candidate, priority)
                .await;
            if issues.is_empty() {
                cleanup_downloads(&rejected).await;
                if attempt > 1 {
                    tracing::warn!(
                        upstream = %repository,
                        path = request.relative(),
                        attempt,
                        "upstream checksum mismatch recovered by retry"
                    );
                }
                return Ok(candidate);
            }

            if rejected.iter().any(|previous: &DownloadedMain| {
                previous.sha1 == candidate.sha1 && previous.sha256 == candidate.sha256
            }) {
                cleanup_downloads(&rejected).await;
                tracing::warn!(
                    upstream = %repository,
                    path = request.relative(),
                    issues = %issues.join("; "),
                    attempt,
                    "accepting stable upstream content with inconsistent checksums"
                );
                return Ok(candidate);
            }

            rejected.push(candidate);
            if attempt == MAX_CHECKSUM_VALIDATION_ATTEMPTS {
                let hashes = rejected
                    .iter()
                    .map(|download| download.sha256.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                cleanup_downloads(&rejected).await;
                return Err(BundleError::Unstable(format!(
                    "{} produced {attempt} distinct unverified downloads with SHA-256 values {hashes}",
                    request.relative()
                )));
            }

            let retry = match self
                .inner
                .upstream
                .fetch_from(repository, request.relative(), priority)
                .await
            {
                FetchResult::Found { response, .. } => {
                    self.download_main(repository, response).await.ok()
                }
                _ => None,
            };
            let Some(retry) = retry else {
                let accepted = rejected.remove(0);
                cleanup_downloads(&rejected).await;
                tracing::warn!(
                    upstream = %repository,
                    path = request.relative(),
                    issues = %issues.join("; "),
                    attempt,
                    "accepting complete upstream download after checksum validation retry failed"
                );
                return Ok(accepted);
            };
            candidate = retry;
        }
        unreachable!("checksum validation loop always returns")
    }

    async fn checksum_issues(
        &self,
        request: &MavenPath,
        repository: &str,
        downloaded: &DownloadedMain,
        priority: RequestPriority,
    ) -> Vec<String> {
        let mut issues = Vec::new();
        for (extension, expected) in [
            ("sha1", &downloaded.sha1),
            ("sha256", &downloaded.sha256),
            ("sha512", &downloaded.sha512),
        ] {
            let relative = format!("{}.{extension}", request.relative());
            let FetchResult::Found {
                repository: checksum_repository,
                response,
                ..
            } = self
                .inner
                .upstream
                .fetch_from(repository, &relative, priority)
                .await
            else {
                continue;
            };
            match read_checksum_response(response, MAX_CHECKSUM_BYTES).await {
                Ok(supplied) => {
                    self.inner
                        .upstream
                        .record_body_success(&checksum_repository);
                    let parsed = supplied
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if parsed != *expected {
                        issues.push(format!(
                            "{relative} expected {expected}, upstream supplied {parsed}"
                        ));
                    }
                }
                Err(error) => issues.push(format!("{relative}: {error}")),
            }
        }
        issues
    }

    async fn install_bundle(&self, files: Vec<PreparedFile>) -> Result<(), CacheFailure> {
        let result = self.install_bundle_inner(&files).await;
        if result.is_err() {
            cleanup_prepared(&files).await;
        }
        result
    }

    async fn install_bundle_inner(&self, files: &[PreparedFile]) -> Result<(), CacheFailure> {
        let mut conflicts = Vec::new();
        if !self.inner.case_sensitive {
            for file in files {
                conflicts.extend(
                    self.inner
                        .database
                        .case_conflicts(&file.relative)
                        .await
                        .map_err(internal)?,
                );
            }
            conflicts.sort();
            conflicts.dedup();
            self.inner
                .database
                .delete_paths(conflicts.clone())
                .await
                .map_err(internal)?;
            for conflict in conflicts {
                remove_file_if_exists(&relative_file_path(&self.inner.storage.root, &conflict))
                    .await
                    .map_err(|error| {
                        CacheFailure::Internal(format!(
                            "failed to remove case-conflicting file {conflict}: {error}"
                        ))
                    })?;
            }
        }

        for file in files {
            let final_path = relative_file_path(&self.inner.storage.root, &file.relative);
            let parent = final_path
                .parent()
                .ok_or_else(|| CacheFailure::Internal("artifact path has no parent".into()))?;
            fs::create_dir_all(parent).await.map_err(|error| {
                CacheFailure::Internal(format!(
                    "failed to create cache directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        let install_order = (1..files.len()).chain(std::iter::once(0));
        for index in install_order {
            let file = &files[index];
            let final_path = relative_file_path(&self.inner.storage.root, &file.relative);
            fs::rename(&file.temporary, &final_path)
                .await
                .map_err(|error| {
                    CacheFailure::Internal(format!(
                        "failed to atomically install {}: {error}",
                        final_path.display()
                    ))
                })?;
        }
        self.inner
            .database
            .upsert_many(files.iter().map(|file| file.record.clone()).collect())
            .await
            .map_err(internal)?;
        let protected = files
            .iter()
            .map(|file| file.relative.as_str())
            .collect::<HashSet<_>>();
        if let Err(error) = self.enforce_capacity(&protected).await {
            tracing::warn!(%error, "cache capacity enforcement did not complete");
        }
        Ok(())
    }

    async fn enforce_capacity(&self, protected: &HashSet<&str>) -> Result<(), CacheFailure> {
        let Some(max_size) = self.inner.config.max_size else {
            return Ok(());
        };
        let stats = self.inner.database.stats().await.map_err(internal)?;
        if stats.total_size <= max_size {
            return Ok(());
        }
        let mut remaining = stats.total_size;
        let candidates = self
            .inner
            .database
            .records_by_access()
            .await
            .map_err(internal)?;
        for record in candidates {
            if remaining <= max_size {
                break;
            }
            if protected.contains(record.path.as_str()) || self.path_is_busy(&record.path) {
                continue;
            }
            let removed = self.remove_records(vec![record]).await?;
            remaining = remaining.saturating_sub(removed.bytes);
        }
        Ok(())
    }

    async fn remove_records(
        &self,
        records: Vec<ArtifactRecord>,
    ) -> Result<RemovalStats, CacheFailure> {
        let mut removed_paths = Vec::new();
        let mut stats = RemovalStats { files: 0, bytes: 0 };
        for record in records {
            if self.path_is_busy(&record.path) {
                continue;
            }
            let file_path = relative_file_path(&self.inner.storage.root, &record.path);
            match fs::remove_file(&file_path).await {
                Ok(()) => {
                    stats.files += 1;
                    stats.bytes = stats.bytes.saturating_add(record.file_size.max(0) as u64);
                    removed_paths.push(record.path);
                    remove_empty_parents(file_path.parent(), &self.inner.storage.root).await;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    stats.bytes = stats.bytes.saturating_add(record.file_size.max(0) as u64);
                    removed_paths.push(record.path);
                }
                Err(error) => {
                    tracing::warn!(path = %file_path.display(), %error, "failed to remove cached file");
                }
            }
        }
        self.inner
            .database
            .delete_paths(removed_paths)
            .await
            .map_err(internal)?;
        Ok(stats)
    }

    fn path_is_busy(&self, path: &str) -> bool {
        self.inner.flights.contains_key(path) || self.inner.refreshes.contains_key(path)
    }

    async fn fresh_negative_entries(&self, path: &str) -> Result<HashSet<String>, CacheFailure> {
        let entries = self
            .inner
            .database
            .negative_entries(path)
            .await
            .map_err(internal)?;
        let (fresh, expired): (Vec<NegativeCacheEntry>, Vec<NegativeCacheEntry>) =
            entries.into_iter().partition(|entry| {
                is_timestamp_fresh(entry.observed_at, self.inner.config.negative_ttl)
            });
        self.inner
            .database
            .delete_negative_entries(
                path,
                expired
                    .into_iter()
                    .map(|entry| entry.repository_id)
                    .collect(),
            )
            .await
            .map_err(internal)?;
        Ok(fresh.into_iter().map(|entry| entry.repository_id).collect())
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

enum BundleError {
    Unstable(String),
    Internal(CacheFailure),
}

async fn stream_to_file(
    response: UpstreamResponse,
    destination: &Path,
) -> Result<(u64, String, String, String), CacheFailure> {
    let (response, _permit) = response.into_parts();
    let expected_size = response.content_length();
    let mut file = create_temporary(destination).await?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            CacheFailure::Internal(format!("failed to read upstream response body: {error}"))
        })?;
        file.write_all(&chunk).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to write temporary file {}: {error}",
                destination.display()
            ))
        })?;
        sha1.update(&chunk);
        sha256.update(&chunk);
        sha512.update(&chunk);
        size = size.saturating_add(chunk.len() as u64);
    }
    if expected_size.is_some_and(|expected| expected != size) {
        return Err(CacheFailure::Internal(format!(
            "upstream response length mismatch: expected {} bytes, received {size}",
            expected_size.expect("expected size is present")
        )));
    }
    flush_temporary(file, destination).await?;
    Ok((
        size,
        format!("{:x}", sha1.finalize()),
        format!("{:x}", sha256.finalize()),
        format!("{:x}", sha512.finalize()),
    ))
}

async fn read_checksum_response(
    response: UpstreamResponse,
    limit: usize,
) -> Result<String, CacheFailure> {
    let (response, _permit) = response.into_parts();
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(CacheFailure::Internal(
            "upstream checksum response is too large".into(),
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            CacheFailure::Internal(format!("failed to read upstream checksum: {error}"))
        })?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(CacheFailure::Internal(
                "upstream checksum response is too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| CacheFailure::Internal("upstream checksum is not UTF-8".into()))
}

async fn write_temporary(path: &Path, content: &[u8]) -> Result<(), CacheFailure> {
    let result = async {
        let mut file = create_temporary(path).await?;
        file.write_all(content).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to write temporary file {}: {error}",
                path.display()
            ))
        })?;
        flush_temporary(file, path).await
    }
    .await;
    if result.is_err() {
        let _ = remove_file_if_exists(path).await;
    }
    result
}

async fn create_temporary(path: &Path) -> Result<fs::File, CacheFailure> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to create temporary file {}: {error}",
                path.display()
            ))
        })
}

async fn flush_temporary(mut file: fs::File, path: &Path) -> Result<(), CacheFailure> {
    file.flush().await.map_err(|error| {
        CacheFailure::Internal(format!(
            "failed to flush temporary file {}: {error}",
            path.display()
        ))
    })?;
    drop(file);
    Ok(())
}

async fn cleanup_prepared(files: &[PreparedFile]) {
    for file in files {
        let _ = remove_file_if_exists(&file.temporary).await;
    }
}

async fn cleanup_downloads(downloads: &[DownloadedMain]) {
    for download in downloads {
        let _ = remove_file_if_exists(&download.temporary).await;
    }
}

async fn hash_file(path: &Path) -> Result<(u64, String, String, String), std::io::Error> {
    let mut file = fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut size = 0_u64;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        size = size.saturating_add(read as u64);
        sha1.update(chunk);
        sha256.update(chunk);
        sha512.update(chunk);
    }
    Ok((
        size,
        format!("{:x}", sha1.finalize()),
        format!("{:x}", sha256.finalize()),
        format!("{:x}", sha512.finalize()),
    ))
}

async fn remove_empty_parents(parent: Option<&Path>, root: &Path) {
    let Some(mut current) = parent.map(Path::to_path_buf) else {
        return;
    };
    while current != root && current.starts_with(root) {
        match fs::remove_dir(&current).await {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::DirectoryNotEmpty | ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => {
                tracing::debug!(path = %current.display(), %error, "failed to remove empty cache directory");
                break;
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
}

fn normalize_cache_prefix(prefix: &str) -> Result<String, CacheFailure> {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty()
        || prefix.contains('\\')
        || prefix.split('/').any(|segment| {
            segment.is_empty()
                || matches!(segment, "." | "..")
                || segment.chars().any(char::is_control)
        })
        || prefix
            .split('/')
            .next()
            .is_some_and(|segment| segment.eq_ignore_ascii_case(".maven-haste"))
    {
        return Err(CacheFailure::Internal(
            "cache prefix must be a non-empty safe relative path".into(),
        ));
    }
    Ok(prefix.to_owned())
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn temporary_path(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(format!("{}.part", Uuid::new_v4()))
}

fn relative_file_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

async fn remove_file_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_fresh(record: &ArtifactRecord, ttl: Duration) -> bool {
    let timestamp = record.last_refresh_attempt.unwrap_or(record.created_at);
    is_timestamp_fresh(timestamp, ttl)
}

fn is_timestamp_fresh(timestamp: i64, ttl: Duration) -> bool {
    let age = unix_timestamp().saturating_sub(timestamp) as u64;
    Duration::from_secs(age) < ttl
}

fn internal(error: AppError) -> CacheFailure {
    CacheFailure::Internal(error.to_string())
}

fn unix_timestamp() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use url::Url;

    use super::*;
    use crate::config::{
        CircuitBreakerConfig, LoggingConfig, RepositoryConfig, ServerConfig, UpstreamConfig,
    };

    async fn test_cache(directory: &TempDir, max_size: Option<u64>) -> (CacheManager, Database) {
        let storage = StorageConfig::resolved_for_test(directory.path().join("repository"));
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
