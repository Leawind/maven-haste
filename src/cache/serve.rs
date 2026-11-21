//! Serving cached artifacts: cache lookup, freshness, request coalescing,
//! and background refresh scheduling.

use std::collections::HashSet;
use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::mapref::entry::Entry;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use tokio::fs;
use tokio::io::AsyncSeekExt;
use tokio::sync::OnceCell;

use crate::cache::io::{
    DigestAlgorithm, hash_open_file_with, header_string, internal, is_fresh, recorded_file_path,
    unix_timestamp,
};
use crate::cache::types::DownloadOutcome;
use crate::cache::{
    CacheFailure, CacheManager, CacheStatus, CachedArtifact, Flight, HIT_FLUSH_THRESHOLD,
    HeadOutcome, UpstreamHead,
};
use crate::config::CacheConfig;
use crate::db::ArtifactRecord;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::FetchResult;

impl CacheManager {
    /// Resolves a request to a cached artifact, downloading it from an
    /// upstream repository on a miss.
    pub async fn get(&self, request: &MavenPath) -> Result<CachedArtifact, CacheFailure> {
        let runtime = self.runtime();
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
                let stale = !is_fresh(&cached.record, runtime.cache.metadata_ttl);
                if stale {
                    cached.status = CacheStatus::Stale;
                    self.inner.stale_hits.fetch_add(1, Ordering::Relaxed);
                    self.trigger_refresh(request.clone(), cached.record.clone());
                }
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                self.count_repository_request(&cached.record.upstream, true);
                return Ok(cached);
            }
        } else if let Some(cached) = self.positive_cache(request, record.as_ref()).await? {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            self.count_repository_request(&cached.record.upstream, true);
            return Ok(cached);
        }

        if self.negative_caching_enabled(request.policy(), &runtime.cache) {
            let negative = self.fresh_negative_entries(request.relative()).await?;
            if runtime
                .upstream
                .all_candidates_negative(request.relative(), &negative)
            {
                self.inner.hits.fetch_add(1, Ordering::Relaxed);
                self.inner.negative_hits.fetch_add(1, Ordering::Relaxed);
                self.log_not_found(request);
                return Err(CacheFailure::NotFound);
            }
        }

        self.inner.misses.fetch_add(1, Ordering::Relaxed);
        if runtime.upstream.offline() {
            return Err(CacheFailure::Offline);
        }
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
                self.count_repository_request(&cached.record.upstream, false);
                Ok(cached)
            }
            DownloadOutcome::Passthrough(prepared) => {
                let path = prepared.temporary.path().to_path_buf();
                self.count_repository_request(&prepared.record.upstream, false);
                Ok(CachedArtifact {
                    file_path: path,
                    file: Some(
                        fs::File::open(prepared.temporary.path())
                            .await
                            .map_err(internal)?,
                    ),
                    record: prepared.record,
                    status: CacheStatus::Miss,
                    temporary: Some(prepared.temporary),
                    serve_guard: None,
                    object_guard: None,
                })
            }
        }
    }

    fn negative_caching_enabled(&self, policy: CachePolicy, cache: &CacheConfig) -> bool {
        policy == CachePolicy::Mutable || cache.negative_cache_permanent
    }

    /// Resolves a client HEAD request. Cached artifacts go through `get`;
    /// uncached artifacts are probed with an upstream HEAD so a HEAD request
    /// never downloads a body merely to report its headers.
    pub async fn head(&self, request: &MavenPath) -> Result<HeadOutcome, CacheFailure> {
        if request.generated_checksum_source().is_some() {
            return self
                .get(request)
                .await
                .map(|cached| HeadOutcome::Cached(Box::new(cached)));
        }
        let record = self
            .inner
            .database
            .get(request.relative())
            .await
            .map_err(internal)?;
        if record.is_some() {
            return self
                .get(request)
                .await
                .map(|cached| HeadOutcome::Cached(Box::new(cached)));
        }

        self.inner.requests.fetch_add(1, Ordering::Relaxed);
        let runtime = self.runtime();
        let negative_enabled = self.negative_caching_enabled(request.policy(), &runtime.cache);
        let negative = if negative_enabled {
            self.fresh_negative_entries(request.relative()).await?
        } else {
            HashSet::new()
        };
        if negative_enabled
            && runtime
                .upstream
                .all_candidates_negative(request.relative(), &negative)
        {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            self.inner.negative_hits.fetch_add(1, Ordering::Relaxed);
            self.log_not_found(request);
            return Ok(HeadOutcome::NotFound);
        }

        self.inner.misses.fetch_add(1, Ordering::Relaxed);
        if runtime.upstream.offline() {
            return Ok(HeadOutcome::Offline);
        }
        let outcome = runtime
            .upstream
            .head(request.relative(), &HashSet::new(), &negative)
            .await;
        if negative_enabled && !outcome.not_found.is_empty() {
            self.inner
                .database
                .upsert_negative_entries(
                    request.relative(),
                    outcome.not_found.clone(),
                    unix_timestamp(),
                )
                .await
                .map_err(internal)?;
        }
        match outcome.result {
            FetchResult::Found { origin, response } => {
                if negative_enabled {
                    self.inner
                        .database
                        .delete_negative_entry(request.relative(), &origin.identity)
                        .await
                        .map_err(internal)?;
                }
                let head = UpstreamHead {
                    repository: origin.repository.clone(),
                    content_length: header_string(response.headers(), CONTENT_LENGTH)
                        .and_then(|value| value.parse().ok()),
                    content_type: header_string(response.headers(), CONTENT_TYPE),
                    etag: header_string(response.headers(), ETAG),
                    last_modified: header_string(response.headers(), LAST_MODIFIED),
                };
                drop(response);
                runtime.upstream.record_body_success(&origin.circuit);
                self.count_repository_request(&origin.repository, false);
                Ok(HeadOutcome::Upstream(head))
            }
            FetchResult::NotModified { .. } => Ok(HeadOutcome::Gateway),
            FetchResult::NotFound => Ok(HeadOutcome::NotFound),
            FetchResult::GatewayFailure => Ok(HeadOutcome::Gateway),
        }
    }

    async fn record_hit(&self, request: &MavenPath) {
        let should_flush = {
            let mut pending = self
                .inner
                .pending_hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let hit = pending.entry(request.relative().to_owned()).or_default();
            hit.count = hit.count.saturating_add(1);
            hit.last_accessed = unix_timestamp();
            self.inner
                .pending_hit_ops
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
                >= HIT_FLUSH_THRESHOLD
        };
        if should_flush {
            self.flush_hits().await;
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

    /// Opens and verifies the exact representation before allowing a response
    /// to use its validators or bytes. The handle survives later publication.
    pub(crate) async fn positive_cache(
        &self,
        request: &MavenPath,
        record: Option<&ArtifactRecord>,
    ) -> Result<Option<CachedArtifact>, CacheFailure> {
        let Some(record) = record else {
            return Ok(None);
        };
        let _lifecycle = self.inner.lifecycle.read().await;
        let file_path = recorded_file_path(&self.inner.storage.root, record)
            .await
            .map_err(internal)?;
        match fs::File::open(&file_path).await {
            Ok(mut file) => {
                let metadata = file.metadata().await.map_err(internal)?;
                let expected = record.file_size.max(0) as u64;
                if !metadata.is_file() || metadata.len() != expected {
                    tracing::warn!(
                        path = request.relative(),
                        database = expected,
                        file = metadata.len(),
                        "cached file size does not match its record; fetching it again"
                    );
                    return Ok(None);
                }
                let trusted = match record.sha256.as_deref() {
                    Some(expected_hash) => hash_open_file_with(&mut file, DigestAlgorithm::Sha256)
                        .await
                        .map_err(internal)
                        .is_ok_and(|(size, hash)| size == expected && hash == expected_hash),
                    None => match record.sha1.as_deref() {
                        Some(expected_hash) => {
                            hash_open_file_with(&mut file, DigestAlgorithm::Sha1)
                                .await
                                .map_err(internal)
                                .is_ok_and(|(size, hash)| size == expected && hash == expected_hash)
                        }
                        None => true,
                    },
                };
                if !trusted {
                    tracing::warn!(
                        path = request.relative(),
                        "cached file hash does not match its record; fetching it again"
                    );
                    return Ok(None);
                }
                file.rewind().await.map_err(internal)?;
                let object_guard = record
                    .sha256
                    .as_deref()
                    .map(|hash| self.inner.served_objects.acquire(hash));
                Ok(Some(CachedArtifact {
                    file_path,
                    file: Some(file),
                    record: record.clone(),
                    status: CacheStatus::Hit,
                    temporary: None,
                    serve_guard: Some(self.inner.serves.acquire(request.relative())),
                    object_guard,
                }))
            }
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
        if self.runtime().upstream.offline() {
            return;
        }
        let source = request.generated_checksum_source();
        let key = source.as_ref().unwrap_or(&request).relative().to_owned();
        let Entry::Vacant(entry) = self.inner.refreshes.entry(key.clone()) else {
            return;
        };
        entry.insert(());

        let cache = self.clone();
        tokio::spawn(async move {
            let result = if let Some(source) = source {
                match cache.inner.database.get(source.relative()).await {
                    Ok(Some(source_record)) => cache.refresh(&source, &source_record).await,
                    Ok(None) => cache.download_main_initial(&source).await.map(|_| ()),
                    Err(error) => Err(internal(error)),
                }
            } else {
                cache.refresh(&request, &record).await
            };
            if let Err(error) = result {
                tracing::warn!(path = request.relative(), %error, "background refresh failed");
                if let Err(touch_error) = cache
                    .inner
                    .database
                    .touch_refresh_attempt(&key, unix_timestamp())
                    .await
                {
                    tracing::error!(
                        path = request.relative(),
                        error = %touch_error,
                        "failed to record refresh attempt"
                    );
                }
            }
            // A generated checksum is refreshed through its source file. When
            // that refresh completes without a new bundle (304, 404, or an
            // error), the checksum record must still record the attempt;
            // otherwise every later request would trigger another refresh.
            if key != request.relative()
                && let Err(error) = cache
                    .inner
                    .database
                    .touch_refresh_attempt(request.relative(), unix_timestamp())
                    .await
            {
                tracing::error!(
                    path = request.relative(),
                    error = %error,
                    "failed to record the checksum refresh attempt"
                );
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
