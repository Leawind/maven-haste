//! Serving cached artifacts: cache lookup, freshness, request coalescing,
//! and background refresh scheduling.

use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::mapref::entry::Entry;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::cache::io::{internal, is_fresh, unix_timestamp};
use crate::cache::types::DownloadOutcome;
use crate::cache::{CacheFailure, CacheManager, CacheStatus, CachedArtifact, Flight};
use crate::db::ArtifactRecord;
use crate::request_path::{CachePolicy, MavenPath};

impl CacheManager {
    /// Resolves a request to a cached artifact, downloading it from an
    /// upstream repository on a miss.
    pub async fn get(&self, request: &MavenPath) -> Result<CachedArtifact, CacheFailure> {
        self.inner.requests.fetch_add(1, Ordering::Relaxed);
        let mut record = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;
        if record.is_some() {
            self.record_hit(request).await;
            if let Some(record) = record.as_mut() {
                record.last_accessed = unix_timestamp();
            }
        }

        if request.policy() == CachePolicy::Mutable {
            if let Some(mut cached) = self.positive_cache(request, record.as_ref()).await? {
                let stale = !is_fresh(&cached.record, self.inner.config.metadata_ttl);
                if stale {
                    cached.status = CacheStatus::Stale;
                    self.inner.stale_hits.fetch_add(1, Ordering::Relaxed);
                    self.trigger_refresh(request.clone(), cached.record.clone());
                }
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                cached.serve_guard = Some(self.inner.serves.acquire(request.relative()));
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
                self.log_not_found(request);
                return Err(CacheFailure::NotFound);
            }
        } else if let Some(mut cached) = self.positive_cache(request, record.as_ref()).await? {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            cached.serve_guard = Some(self.inner.serves.acquire(request.relative()));
            return Ok(cached);
        }

        self.inner.misses.fetch_add(1, Ordering::Relaxed);
        match self.synchronous_download(request).await? {
            DownloadOutcome::Installed => {
                if record.is_none() {
                    self.record_hit(request).await;
                }
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
                cached.serve_guard = Some(self.inner.serves.acquire(request.relative()));
                Ok(cached)
            }
            DownloadOutcome::Passthrough(prepared) => {
                let path = prepared.temporary.path().to_path_buf();
                Ok(CachedArtifact {
                    file_path: path,
                    record: prepared.record,
                    status: CacheStatus::Miss,
                    temporary: Some(prepared.temporary),
                    serve_guard: None,
                })
            }
        }
    }

    async fn record_hit(&self, request: &MavenPath) {
        if let Err(error) = self
            .inner
            .database
            .record_hit(request.relative(), unix_timestamp())
            .await
        {
            tracing::warn!(
                %error,
                path = request.relative(),
                "failed to record the request hit"
            );
        }
    }

    pub(crate) fn log_not_found(&self, request: &MavenPath) {
        if !request.is_checksum() {
            tracing::info!(
                path = request.relative(),
                "artifact was not found in any upstream repository"
            );
        }
    }

    /// Builds a cached artifact from a database record whose file is present
    /// and whose size matches the record.
    pub(crate) async fn positive_cache(
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
                let expected = record.file_size.max(0) as u64;
                if metadata.len() != expected {
                    tracing::warn!(
                        path = request.relative(),
                        database = expected,
                        file = metadata.len(),
                        "cached file size does not match its record; fetching it again"
                    );
                    return Ok(None);
                }
                Ok(Some(CachedArtifact {
                    file_path,
                    record: record.clone(),
                    status: CacheStatus::Hit,
                    temporary: None,
                    serve_guard: None,
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

    async fn synchronous_download(
        &self,
        request: &MavenPath,
    ) -> Result<DownloadOutcome, CacheFailure> {
        let key = request.relative().to_owned();
        self.run_flight(&key, || async { self.download_initial(request).await })
            .await
    }

    pub(crate) async fn synchronous_main_download(
        &self,
        request: &MavenPath,
    ) -> Result<DownloadOutcome, CacheFailure> {
        let key = request.relative().to_owned();
        self.run_flight(&key, || async { self.download_main_initial(request).await })
            .await
    }

    /// Runs one download per path: concurrent requests share the same flight,
    /// and a completed flight is removed so a later request can restart it.
    async fn run_flight<F, Fut>(
        &self,
        key: &str,
        initializer: F,
    ) -> Result<DownloadOutcome, CacheFailure>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<DownloadOutcome, CacheFailure>>,
    {
        let flight = self
            .inner
            .flights
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let result = flight.get_or_init(initializer).await.clone();
        self.remove_completed_flight(key, &flight);
        result
    }

    /// Starts a background refresh for stale mutable metadata, at most one per
    /// path at a time.
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

    /// Removes a finished flight from the registry unless a newer flight
    /// already replaced it.
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
