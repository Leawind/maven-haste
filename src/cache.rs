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
use sha2::Sha256;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OnceCell, Semaphore};
use uuid::Uuid;

use crate::config::{CacheConfig, Config, StorageConfig};
use crate::db::{ArtifactRecord, Database};
use crate::error::AppError;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::{FetchResult, UpstreamClient};

const MAX_CHECKSUM_BYTES: usize = 64 * 1024;
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
    refresh_permits: Arc<Semaphore>,
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
    pub upstreams: Vec<crate::upstream::UpstreamStatus>,
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
            config.cache.refresh_timeout,
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
                refresh_permits: Arc::new(Semaphore::new(config.cache.refresh_max_concurrency)),
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
            if let Some(record) = record.as_ref().filter(|record| record.is_not_found)
                && is_fresh(record, self.inner.config.negative_ttl)
            {
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                self.inner.negative_hits.fetch_add(1, Ordering::Relaxed);
                return Err(CacheFailure::NotFound);
            }
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
        if record.as_ref().is_some_and(|record| record.is_not_found) {
            return Err(CacheFailure::NotFound);
        }
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
            upstreams: self.inner.upstream.statuses(),
        })
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
        let Some(record) = record.filter(|record| !record.is_not_found) else {
            return Ok(None);
        };
        let file_path = request.final_path(&self.inner.storage.root);
        match fs::metadata(&file_path).await {
            Ok(metadata) if metadata.is_file() => Ok(Some(CachedArtifact {
                file_path,
                record: record.clone(),
                status: CacheStatus::Hit,
            })),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CacheFailure::Internal(format!(
                "failed to inspect cached file {}: {error}",
                file_path.display()
            ))),
        }
    }

    async fn download_initial(&self, request: &MavenPath) -> Result<(), CacheFailure> {
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
        if request.policy() == CachePolicy::Mutable
            && existing.as_ref().is_some_and(|record| {
                record.is_not_found && is_fresh(record, self.inner.config.negative_ttl)
            })
        {
            return Err(CacheFailure::NotFound);
        }

        match self.prepare_fetch(request, None).await? {
            PreparedFetch::Bundle(files) => self.install_bundle(files).await,
            PreparedFetch::NotFound => {
                if request.policy() == CachePolicy::Mutable {
                    self.store_negative(request).await?;
                }
                Err(CacheFailure::NotFound)
            }
            PreparedFetch::Gateway | PreparedFetch::NotModified => Err(CacheFailure::Gateway),
        }
    }

    fn trigger_refresh(&self, request: MavenPath, record: ArtifactRecord) {
        let key = request.relative().to_owned();
        let Entry::Vacant(entry) = self.inner.refreshes.entry(key.clone()) else {
            return;
        };
        let Ok(permit) = Arc::clone(&self.inner.refresh_permits).try_acquire_owned() else {
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
            drop(permit);
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
            PreparedFetch::NotFound => self.store_negative(request).await,
            PreparedFetch::Gateway => Err(CacheFailure::Gateway),
        }
    }

    async fn prepare_fetch(
        &self,
        request: &MavenPath,
        previous: Option<&ArtifactRecord>,
    ) -> Result<PreparedFetch, CacheFailure> {
        let mut excluded = HashSet::new();
        let mut upstream_failure = false;

        loop {
            let fetched = if let Some(previous) = previous {
                self.inner
                    .upstream
                    .refresh(
                        request.relative(),
                        &previous.upstream,
                        previous.etag.as_deref(),
                        previous.last_modified.as_deref(),
                        &excluded,
                    )
                    .await
            } else {
                self.inner
                    .upstream
                    .fetch(request.relative(), &excluded)
                    .await
            };

            match fetched {
                FetchResult::Found {
                    repository,
                    response,
                } => {
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
                    match self.prepare_bundle(request, &repository, downloaded).await {
                        Ok(files) => return Ok(PreparedFetch::Bundle(files)),
                        Err(BundleError::ChecksumMismatch(error)) => {
                            upstream_failure = true;
                            excluded.insert(repository.clone());
                            self.inner.upstream.record_body_failure(&repository);
                            tracing::warn!(upstream = %repository, %error, "upstream checksum mismatch");
                        }
                        Err(BundleError::Internal(error)) => return Err(error),
                    }
                }
                FetchResult::NotModified => return Ok(PreparedFetch::NotModified),
                FetchResult::NotFound if upstream_failure => return Ok(PreparedFetch::Gateway),
                FetchResult::NotFound => return Ok(PreparedFetch::NotFound),
                FetchResult::GatewayFailure => return Ok(PreparedFetch::Gateway),
            }
        }
    }

    async fn download_main(
        &self,
        repository: &str,
        response: reqwest::Response,
    ) -> Result<DownloadedMain, CacheFailure> {
        let etag = header_string(response.headers(), ETAG);
        let last_modified = header_string(response.headers(), LAST_MODIFIED);
        let temporary = temporary_path(self.inner.storage.tmp_dir());
        match stream_to_file(response, &temporary).await {
            Ok((size, sha1, sha256)) => {
                self.inner.upstream.record_body_success(repository);
                Ok(DownloadedMain {
                    temporary,
                    size,
                    sha1,
                    sha256,
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
    ) -> Result<Vec<PreparedFile>, BundleError> {
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
            is_not_found: false,
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
            ("sha1", downloaded.sha1.as_str()),
            ("sha256", downloaded.sha256.as_str()),
        ] {
            let relative = format!("{}.{extension}", request.relative());
            if let FetchResult::Found {
                repository: checksum_repository,
                response,
            } = self.inner.upstream.fetch_from(repository, &relative).await
            {
                let supplied = match read_checksum_response(response, MAX_CHECKSUM_BYTES).await {
                    Ok(supplied) => supplied,
                    Err(error) => {
                        cleanup_prepared(&files).await;
                        return Err(BundleError::ChecksumMismatch(error.to_string()));
                    }
                };
                let parsed = supplied
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if parsed != expected {
                    cleanup_prepared(&files).await;
                    return Err(BundleError::ChecksumMismatch(format!(
                        "{relative} expected {expected}, upstream supplied {parsed}"
                    )));
                }
                self.inner
                    .upstream
                    .record_body_success(&checksum_repository);
            }

            let content = format!("{expected}\n");
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
                    sha256: None,
                    etag: None,
                    last_modified: None,
                    file_size: i64::try_from(content.len()).unwrap_or(i64::MAX),
                    created_at: now,
                    last_refresh_attempt: refresh_attempt,
                    is_not_found: false,
                },
            });
        }
        Ok(files)
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
            .map_err(internal)
    }

    async fn store_negative(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        let now = unix_timestamp();
        self.inner
            .database
            .upsert(ArtifactRecord {
                path: request.relative().into(),
                group_id: request.group_id.clone(),
                artifact_id: request.artifact_id.clone(),
                version: request.version.clone(),
                file_type: request.file_type.clone(),
                upstream: String::new(),
                sha1: None,
                sha256: None,
                etag: None,
                last_modified: None,
                file_size: 0,
                created_at: now,
                last_refresh_attempt: Some(now),
                is_not_found: true,
            })
            .await
            .map_err(internal)
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
    ChecksumMismatch(String),
    Internal(CacheFailure),
}

async fn stream_to_file(
    response: reqwest::Response,
    destination: &Path,
) -> Result<(u64, String, String), CacheFailure> {
    let mut file = create_temporary(destination).await?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
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
        size = size.saturating_add(chunk.len() as u64);
    }
    flush_temporary(file, destination).await?;
    Ok((
        size,
        format!("{:x}", sha1.finalize()),
        format!("{:x}", sha256.finalize()),
    ))
}

async fn read_checksum_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<String, CacheFailure> {
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
