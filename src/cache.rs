use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::header::{ETAG, LAST_MODIFIED};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::config::{Config, StorageConfig};
use crate::db::{ArtifactRecord, Database};
use crate::error::AppError;
use crate::request_path::MavenPath;
use crate::upstream::{FetchResult, UpstreamClient};

type Flight = OnceCell<Result<(), CacheFailure>>;

#[derive(Clone)]
pub struct CacheManager {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    storage: StorageConfig,
    database: Database,
    upstream: UpstreamClient,
    case_sensitive: bool,
    flights: DashMap<String, Arc<Flight>>,
}

#[derive(Debug)]
pub struct CachedArtifact {
    pub file_path: PathBuf,
    pub record: ArtifactRecord,
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
            config.cache.refresh_timeout,
            &config.circuit_breaker,
        )?;
        Ok(Self {
            inner: Arc::new(CacheInner {
                storage: config.storage.clone(),
                database,
                upstream,
                case_sensitive,
                flights: DashMap::new(),
            }),
        })
    }

    pub async fn get(&self, request: &MavenPath) -> Result<CachedArtifact, CacheFailure> {
        if let Some(cached) = self.find_cached(request).await? {
            return Ok(cached);
        }

        let key = request.relative().to_owned();
        let flight = self
            .inner
            .flights
            .entry(key.clone())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let result = flight
            .get_or_init(|| async { self.download(request).await })
            .await
            .clone();
        self.remove_completed_flight(&key, &flight);
        result?;

        self.find_cached(request).await?.ok_or_else(|| {
            CacheFailure::Internal("download completed without a cache record".into())
        })
    }

    async fn find_cached(
        &self,
        request: &MavenPath,
    ) -> Result<Option<CachedArtifact>, CacheFailure> {
        let record = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;
        let Some(record) = record.filter(|record| !record.is_not_found) else {
            return Ok(None);
        };
        let file_path = request.final_path(&self.inner.storage.root);
        match fs::metadata(&file_path).await {
            Ok(metadata) if metadata.is_file() => Ok(Some(CachedArtifact { file_path, record })),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CacheFailure::Internal(format!(
                "failed to inspect cached file {}: {error}",
                file_path.display()
            ))),
        }
    }

    async fn download(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        if self.find_cached(request).await?.is_some() {
            return Ok(());
        }

        let (repository, response) = match self.inner.upstream.fetch(request.relative()).await {
            FetchResult::Found {
                repository,
                response,
            } => (repository, response),
            FetchResult::NotFound => return Err(CacheFailure::NotFound),
            FetchResult::GatewayFailure => return Err(CacheFailure::Gateway),
        };

        let etag = header_string(response.headers(), ETAG);
        let last_modified = header_string(response.headers(), LAST_MODIFIED);
        let temporary = self
            .inner
            .storage
            .tmp_dir()
            .join(format!("{}.part", Uuid::new_v4()));
        let size = match stream_to_file(response, &temporary).await {
            Ok(size) => {
                self.inner.upstream.record_body_success(&repository);
                size
            }
            Err(error) => {
                self.inner.upstream.record_body_failure(&repository);
                let _ = remove_file_if_exists(&temporary).await;
                return Err(error);
            }
        };

        if let Err(error) = self
            .install_download(
                request,
                &temporary,
                ArtifactRecord {
                    path: request.relative().into(),
                    group_id: request.group_id.clone(),
                    artifact_id: request.artifact_id.clone(),
                    version: request.version.clone(),
                    file_type: request.file_type.clone(),
                    upstream: repository,
                    etag,
                    last_modified,
                    file_size: i64::try_from(size).unwrap_or(i64::MAX),
                    created_at: unix_timestamp(),
                    last_refresh_attempt: None,
                    is_not_found: false,
                },
            )
            .await
        {
            let _ = remove_file_if_exists(&temporary).await;
            return Err(error);
        }
        Ok(())
    }

    async fn install_download(
        &self,
        request: &MavenPath,
        temporary: &Path,
        record: ArtifactRecord,
    ) -> Result<(), CacheFailure> {
        let final_path = request.final_path(&self.inner.storage.root);
        let parent = final_path
            .parent()
            .ok_or_else(|| CacheFailure::Internal("artifact path has no parent".into()))?;
        fs::create_dir_all(parent).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to create cache directory {}: {error}",
                parent.display()
            ))
        })?;

        if !self.inner.case_sensitive {
            let conflicts = self
                .inner
                .database
                .case_conflicts(request.relative())
                .await
                .map_err(internal)?;
            self.inner
                .database
                .delete_paths(conflicts.clone())
                .await
                .map_err(internal)?;
            for conflict in conflicts {
                let conflict_path = relative_file_path(&self.inner.storage.root, &conflict);
                remove_file_if_exists(&conflict_path)
                    .await
                    .map_err(|error| {
                        CacheFailure::Internal(format!(
                            "failed to remove case-conflicting file {}: {error}",
                            conflict_path.display()
                        ))
                    })?;
            }
        }

        remove_file_if_exists(&final_path).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to replace orphaned cache file {}: {error}",
                final_path.display()
            ))
        })?;
        fs::rename(temporary, &final_path).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to atomically install {}: {error}",
                final_path.display()
            ))
        })?;
        self.inner.database.upsert(record).await.map_err(internal)
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

async fn stream_to_file(
    response: reqwest::Response,
    destination: &Path,
) -> Result<u64, CacheFailure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
        .map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to create temporary file {}: {error}",
                destination.display()
            ))
        })?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
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
        size = size.saturating_add(chunk.len() as u64);
    }
    file.flush().await.map_err(|error| {
        CacheFailure::Internal(format!(
            "failed to flush temporary file {}: {error}",
            destination.display()
        ))
    })?;
    drop(file);
    Ok(size)
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
