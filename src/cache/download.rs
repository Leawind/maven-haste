use std::collections::HashSet;

use reqwest::header::{ETAG, LAST_MODIFIED};
use sha2::{Digest as _, Sha256};

use crate::cache::CacheFailure;
use crate::cache::io::{
    DigestAlgorithm, StreamFailure, cleanup_downloads, cleanup_prepared, hash_open_file_with,
    header_string, internal, remove_file_if_exists, stream_response_to_file, temporary_path,
    to_hex, unix_timestamp, write_temporary,
};
use crate::cache::types::{
    ChecksumPrefetch, DownloadOutcome, DownloadedMain, FetchedChecksum, PassthroughFile,
    PreparedFetch, PreparedFile, SharedTemp, fetch_all_checksums,
};
use crate::db::ArtifactRecord;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::{FetchResult, RequestPriority, UpstreamResponse};

const MAX_CHECKSUM_VALIDATION_ATTEMPTS: usize = 3;
const MAX_DOWNLOAD_ATTEMPTS: usize = 3;

impl crate::cache::CacheManager {
    pub(crate) async fn download_initial(
        &self,
        request: &MavenPath,
    ) -> Result<DownloadOutcome, CacheFailure> {
        if let Some(source) = request.generated_checksum_source() {
            let source_outcome = self.synchronous_main_download(&source).await?;
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
                return Ok(DownloadOutcome::Installed);
            }
            return match source_outcome {
                DownloadOutcome::Installed => self
                    .regenerate_checksum(request, &source)
                    .await
                    .map(|()| DownloadOutcome::Installed),
                DownloadOutcome::Passthrough(source) => {
                    let mut file = tokio::fs::File::open(source.temporary.path())
                        .await
                        .map_err(internal)?;
                    let checksum = self
                        .build_checksum(request, &source.record.upstream, &mut file)
                        .await?;
                    drop(source);
                    Ok(DownloadOutcome::Passthrough(Box::new(PassthroughFile {
                        temporary: SharedTemp::new(checksum.temporary),
                        record: checksum.record,
                    })))
                }
            };
        }
        self.download_main_initial(request).await
    }

    pub(crate) async fn download_main_initial(
        &self,
        request: &MavenPath,
    ) -> Result<DownloadOutcome, CacheFailure> {
        self.fetch_main_or_cached(request).await
    }

    /// Fetches the requested main file, installing it into the cache when its
    /// repository allows writes, or returning it for direct passthrough when
    /// caching is disabled. Returns `Installed` when a concurrent request
    /// already cached it.
    async fn fetch_main_or_cached(
        &self,
        request: &MavenPath,
    ) -> Result<DownloadOutcome, CacheFailure> {
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
            return Ok(DownloadOutcome::Installed);
        }
        match self.prepare_fetch(request, None).await? {
            PreparedFetch::Bundle(files) => {
                let fits = self.bundle_fits_capacity(&files);
                if self.caching_enabled(&files[0].record.upstream) && fits {
                    self.install_bundle(files).await?;
                    return Ok(DownloadOutcome::Installed);
                }
                if !fits {
                    tracing::info!(
                        path = request.relative(),
                        size = bundle_size(&files),
                        "artifact bundle exceeds cache.max_size; serving it without caching"
                    );
                }
                let (main, generated) = files
                    .split_first()
                    .expect("a prepared bundle always contains the main file");
                cleanup_prepared(generated).await;
                Ok(DownloadOutcome::Passthrough(Box::new(PassthroughFile {
                    temporary: SharedTemp::new(main.temporary.clone()),
                    record: main.record.clone(),
                })))
            }
            PreparedFetch::NotFound => {
                self.log_not_found(request);
                Err(CacheFailure::NotFound)
            }
            PreparedFetch::Gateway | PreparedFetch::NotModified => Err(CacheFailure::Gateway),
        }
    }

    pub(crate) async fn refresh(
        &self,
        request: &MavenPath,
        previous: &ArtifactRecord,
    ) -> Result<(), CacheFailure> {
        match self.prepare_fetch(request, Some(previous)).await? {
            PreparedFetch::Bundle(files) => {
                if !self.bundle_fits_capacity(&files) {
                    tracing::warn!(
                        path = request.relative(),
                        size = bundle_size(&files),
                        "refreshed bundle exceeds cache.max_size; keeping the previous bundle"
                    );
                    cleanup_prepared(&files).await;
                    return self.record_refresh_attempt(request).await;
                }
                if self.caching_enabled(&files[0].record.upstream) {
                    return self.install_bundle(files).await;
                }
                tracing::debug!(
                    upstream = %files[0].record.upstream,
                    path = request.relative(),
                    "refreshed content is not cached because cache writes are disabled for this repository"
                );
                cleanup_prepared(&files).await;
                self.record_refresh_attempt(request).await
            }
            PreparedFetch::NotModified => self.record_refresh_attempt(request).await,
            PreparedFetch::NotFound => {
                self.log_not_found(request);
                self.record_refresh_attempt(request).await
            }
            PreparedFetch::Gateway => Err(CacheFailure::Gateway),
        }
    }

    async fn record_refresh_attempt(&self, request: &MavenPath) -> Result<(), CacheFailure> {
        self.inner
            .database
            .touch_refresh_attempt(request.relative(), unix_timestamp())
            .await
            .map_err(internal)
    }

    /// Whether a bundle can be stored without immediately exceeding the cache
    /// size limit. An oversized single artifact is served directly instead of
    /// evicting the rest of the cache and still staying above the limit.
    fn bundle_fits_capacity(&self, files: &[PreparedFile]) -> bool {
        self.inner
            .config
            .max_size
            .is_none_or(|max_size| bundle_size(files) <= max_size)
    }

    async fn prepare_fetch(
        &self,
        request: &MavenPath,
        previous: Option<&ArtifactRecord>,
    ) -> Result<PreparedFetch, CacheFailure> {
        if self.inner.upstream.offline() {
            return Err(CacheFailure::Offline);
        }
        let priority = if previous.is_some() {
            RequestPriority::Background
        } else {
            RequestPriority::Foreground
        };
        let mut excluded = HashSet::new();
        let mut upstream_failure = false;
        let negative_enabled =
            request.policy() == CachePolicy::Mutable || self.inner.config.negative_cache_permanent;
        let mut negative = if negative_enabled {
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
                negative.extend(outcome.not_found);
            }

            match outcome.result {
                FetchResult::Found {
                    repository,
                    repository_id,
                    response,
                } => {
                    if negative_enabled {
                        self.inner
                            .database
                            .delete_negative_entry(request.relative(), &repository_id)
                            .await
                            .map_err(internal)?;
                    }
                    // The checksum files are fetched while the main body
                    // streams, so a slow artifact download no longer waits on
                    // extra checksum round-trips afterwards.
                    let prefetch = self.start_checksum_prefetch(request, &repository, priority);
                    let downloaded = match self
                        .download_main(request, &repository, response, priority)
                        .await
                    {
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
                        .prepare_bundle(request, &repository, downloaded, priority, prefetch)
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
                    if negative_enabled {
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

    /// Streams the main file to a temporary file, resuming from the bytes
    /// already written when the upstream connection breaks mid-body.
    async fn download_main(
        &self,
        request: &MavenPath,
        repository: &str,
        response: UpstreamResponse,
        priority: RequestPriority,
    ) -> Result<DownloadedMain, CacheFailure> {
        let mut etag = header_string(response.headers(), ETAG);
        let mut last_modified = header_string(response.headers(), LAST_MODIFIED);
        let temporary = temporary_path(self.inner.storage.tmp_dir());
        let mut response = response;
        let mut total = header_string(response.headers(), reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.parse::<u64>().ok());
        let mut resume = None;
        for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
            match stream_response_to_file(response, &temporary, resume).await {
                Ok((size, sha1, sha256, sha512)) => {
                    self.inner.upstream.record_body_success(repository);
                    return Ok(DownloadedMain {
                        temporary,
                        size,
                        sha1,
                        sha256,
                        sha512,
                        etag,
                        last_modified,
                    });
                }
                Err(StreamFailure::Interrupted { error, bytes }) => {
                    tracing::debug!(
                        upstream = repository,
                        path = request.relative(),
                        bytes,
                        %error,
                        "upstream download was interrupted"
                    );
                    if attempt == MAX_DOWNLOAD_ATTEMPTS {
                        let _ = remove_file_if_exists(&temporary).await;
                        return Err(CacheFailure::Internal(format!(
                            "upstream download failed after {attempt} attempts: {error}"
                        )));
                    }
                    // A byte range is safe only when a strong validator pins
                    // the representation. Otherwise restart from byte zero.
                    let validator = etag
                        .as_deref()
                        .filter(|value| value.starts_with('"') && value.ends_with('"'));
                    let outcome = match validator {
                        Some(validator) => {
                            self.inner
                                .upstream
                                .fetch_range(
                                    repository,
                                    request.relative(),
                                    bytes,
                                    validator,
                                    priority,
                                )
                                .await
                        }
                        None => {
                            self.inner
                                .upstream
                                .fetch_from(repository, request.relative(), priority)
                                .await
                        }
                    };
                    match outcome {
                        FetchResult::Found {
                            response: mut resumed,
                            ..
                        } => {
                            if resumed.status() == reqwest::StatusCode::PARTIAL_CONTENT
                                && (validator.is_none()
                                    || header_string(resumed.headers(), ETAG).as_deref()
                                        != validator)
                            {
                                drop(resumed);
                                let FetchResult::Found {
                                    response: restarted,
                                    ..
                                } = self
                                    .inner
                                    .upstream
                                    .fetch_from(repository, request.relative(), priority)
                                    .await
                                else {
                                    let _ = remove_file_if_exists(&temporary).await;
                                    return Err(CacheFailure::Gateway);
                                };
                                resumed = restarted;
                                if resumed.status() != reqwest::StatusCode::OK {
                                    let _ = remove_file_if_exists(&temporary).await;
                                    return Err(CacheFailure::Gateway);
                                }
                            }
                            resume = (validator.is_some()
                                && resumed.status() == reqwest::StatusCode::PARTIAL_CONTENT)
                                .then_some((bytes, total));
                            if resumed.status() == reqwest::StatusCode::OK {
                                etag = header_string(resumed.headers(), ETAG);
                                last_modified = header_string(resumed.headers(), LAST_MODIFIED);
                                total = header_string(
                                    resumed.headers(),
                                    reqwest::header::CONTENT_LENGTH,
                                )
                                .and_then(|value| value.parse().ok());
                            }
                            response = resumed;
                        }
                        outcome => {
                            let _ = remove_file_if_exists(&temporary).await;
                            return Err(CacheFailure::Internal(format!(
                                "resuming the download of {} failed: {}",
                                request.relative(),
                                resume_failure_reason(&outcome)
                            )));
                        }
                    }
                }
                Err(StreamFailure::Failed(error)) => {
                    let _ = remove_file_if_exists(&temporary).await;
                    return Err(error);
                }
            }
        }
        unreachable!("the resume loop always returns within its attempts")
    }

    /// Spawns a background fetch of the checksum files belonging to a main
    /// artifact, overlapping their round-trips with the main body download.
    fn start_checksum_prefetch(
        &self,
        request: &MavenPath,
        repository: &str,
        priority: RequestPriority,
    ) -> ChecksumPrefetch {
        if request.is_checksum() {
            return ChecksumPrefetch::none();
        }
        let upstream = self.inner.upstream.clone();
        let repository = repository.to_owned();
        let relative = request.relative().to_owned();
        ChecksumPrefetch::spawn(async move {
            fetch_all_checksums(&upstream, &repository, &relative, priority).await
        })
    }

    async fn prepare_bundle(
        &self,
        request: &MavenPath,
        repository: &str,
        downloaded: DownloadedMain,
        priority: RequestPriority,
        prefetch: ChecksumPrefetch,
    ) -> Result<Vec<PreparedFile>, BundleError> {
        let downloaded = self
            .select_stable_download(request, repository, downloaded, priority, prefetch)
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
            request_count: 0,
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
            let content_sha256 = to_hex(&Sha256::digest(content.as_bytes()));
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
                    request_count: 0,
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
        prefetch: ChecksumPrefetch,
    ) -> Result<DownloadedMain, BundleError> {
        if request.is_checksum() {
            return Ok(initial);
        }

        let mut prefetched = prefetch.resolve().await;
        let mut rejected = Vec::new();
        let mut candidate = initial;
        for attempt in 1..=MAX_CHECKSUM_VALIDATION_ATTEMPTS {
            let issues = self
                .checksum_issues(request, repository, &candidate, priority, prefetched.take())
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

            rejected.push(candidate);
            if attempt == MAX_CHECKSUM_VALIDATION_ATTEMPTS {
                let hashes = rejected
                    .iter()
                    .map(|download| download.sha256.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                cleanup_downloads(&rejected).await;
                return Err(BundleError::Unstable(format!(
                    "{} produced {attempt} unverified downloads with SHA-256 values {hashes}: {}",
                    request.relative(),
                    issues.join("; ")
                )));
            }

            let retry = match self
                .inner
                .upstream
                .fetch_from(repository, request.relative(), priority)
                .await
            {
                FetchResult::Found { response, .. } => self
                    .download_main(request, repository, response, priority)
                    .await
                    .ok(),
                _ => None,
            };
            let Some(retry) = retry else {
                cleanup_downloads(&rejected).await;
                return Err(BundleError::Unstable(format!(
                    "checksum mismatch and retry failed for {}: {}",
                    request.relative(),
                    issues.join("; ")
                )));
            };
            candidate = retry;
        }
        unreachable!("checksum validation loop always returns")
    }

    /// Compares the checksum files of a downloaded artifact against its
    /// computed hashes. Prefetched results are used as-is; otherwise all
    /// three files are fetched concurrently.
    async fn checksum_issues(
        &self,
        request: &MavenPath,
        repository: &str,
        downloaded: &DownloadedMain,
        priority: RequestPriority,
        prefetched: Option<Vec<FetchedChecksum>>,
    ) -> Vec<String> {
        let files = match prefetched {
            Some(files) => files,
            None => {
                fetch_all_checksums(
                    &self.inner.upstream,
                    repository,
                    request.relative(),
                    priority,
                )
                .await
            }
        };
        let mut issues = Vec::new();
        for (extension, expected) in [
            ("sha1", &downloaded.sha1),
            ("sha256", &downloaded.sha256),
            ("sha512", &downloaded.sha512),
        ] {
            let Some(file) = files.iter().find(|file| file.extension == extension) else {
                continue;
            };
            let Some(supplied) = &file.supplied else {
                continue;
            };
            let relative = format!("{}.{extension}", request.relative());
            match supplied {
                Ok(supplied) => {
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

    async fn build_checksum(
        &self,
        request: &MavenPath,
        upstream: &str,
        source: &mut tokio::fs::File,
    ) -> Result<PreparedFile, CacheFailure> {
        let algorithm = match request.file_type.as_str() {
            "md5" => DigestAlgorithm::Md5,
            "sha1" => DigestAlgorithm::Sha1,
            "sha256" => DigestAlgorithm::Sha256,
            "sha512" => DigestAlgorithm::Sha512,
            extension => {
                return Err(CacheFailure::Internal(format!(
                    "cannot generate unsupported checksum type {extension}"
                )));
            }
        };
        let (_, expected) = hash_open_file_with(source, algorithm)
            .await
            .map_err(internal)?;
        let content = format!("{expected}\n");
        let temporary = temporary_path(self.inner.storage.tmp_dir());
        write_temporary(&temporary, content.as_bytes()).await?;
        let now = unix_timestamp();
        Ok(PreparedFile {
            relative: request.relative().into(),
            temporary,
            record: ArtifactRecord {
                path: request.relative().into(),
                group_id: request.group_id.clone(),
                artifact_id: request.artifact_id.clone(),
                version: request.version.clone(),
                file_type: request.file_type.clone(),
                upstream: upstream.into(),
                sha1: None,
                sha256: Some(to_hex(&Sha256::digest(content.as_bytes()))),
                etag: None,
                last_modified: None,
                file_size: content.len() as i64,
                created_at: now,
                last_refresh_attempt: (request.policy() == CachePolicy::Mutable).then_some(now),
                last_accessed: now,
                request_count: 0,
            },
        })
    }

    async fn regenerate_checksum(
        &self,
        request: &MavenPath,
        source: &MavenPath,
    ) -> Result<(), CacheFailure> {
        for _ in 0..MAX_CHECKSUM_VALIDATION_ATTEMPTS {
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
            let mut cached = self
                .positive_cache(source, Some(&source_record))
                .await?
                .ok_or_else(|| {
                    CacheFailure::Internal("checksum source is unavailable or corrupt".into())
                })?;
            let checksum = self
                .build_checksum(
                    request,
                    &source_record.upstream,
                    cached.file.as_mut().expect("verified source has a file"),
                )
                .await?;
            if self
                .install_bundle_checked(vec![checksum], Some(&source_record))
                .await?
            {
                return Ok(());
            }
        }
        Err(CacheFailure::Gateway)
    }
}

fn bundle_size(files: &[PreparedFile]) -> u64 {
    files.iter().fold(0_u64, |total, file| {
        total.saturating_add(file.record.file_size.max(0) as u64)
    })
}

enum BundleError {
    Unstable(String),
    Internal(CacheFailure),
}

/// Describes why a byte-range resume request could not continue a download.
fn resume_failure_reason(outcome: &FetchResult) -> &'static str {
    match outcome {
        FetchResult::Found { .. } => "a response was returned for the range request",
        FetchResult::NotModified { .. } => {
            "the repository returned Not Modified for a range request"
        }
        FetchResult::NotFound => "the artifact disappeared while resuming",
        FetchResult::GatewayFailure => "the repository failed the range request",
    }
}
