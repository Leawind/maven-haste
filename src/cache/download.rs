use std::collections::HashSet;
use std::path::Path;

use reqwest::header::{ETAG, LAST_MODIFIED};
use sha2::{Digest, Sha256};

use crate::cache::CacheFailure;
use crate::cache::io::{
    cleanup_downloads, cleanup_prepared, hash_file, header_string, internal,
    read_checksum_response, remove_file_if_exists, stream_to_file, temporary_path, unix_timestamp,
    write_temporary,
};
use crate::cache::types::{DownloadOutcome, DownloadedMain, PreparedFetch, PreparedFile};
use crate::db::ArtifactRecord;
use crate::request_path::{CachePolicy, MavenPath};
use crate::upstream::{FetchResult, RequestPriority, UpstreamResponse};

const MAX_CHECKSUM_BYTES: usize = 64 * 1024;
const MAX_CHECKSUM_VALIDATION_ATTEMPTS: usize = 3;

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
                DownloadOutcome::Passthrough(prepared) => {
                    let source_temporary = prepared.temporary.clone();
                    let checksum = self
                        .build_checksum(request, &prepared.record.upstream, &prepared.temporary)
                        .await?;
                    let _ = remove_file_if_exists(&source_temporary).await;
                    Ok(DownloadOutcome::Passthrough(Box::new(checksum)))
                }
            };
        }
        self.download_main_initial(request).await
    }

    pub(crate) async fn download_main_initial(
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
                if self.caching_enabled(&files[0].record.upstream) {
                    self.install_bundle(files).await?;
                    return Ok(DownloadOutcome::Installed);
                }
                let (main, generated) = files
                    .split_first()
                    .expect("a prepared bundle always contains the main file");
                cleanup_prepared(generated).await;
                Ok(DownloadOutcome::Passthrough(Box::new(main.clone())))
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
                if self.caching_enabled(&files[0].record.upstream) {
                    return self.install_bundle(files).await;
                }
                tracing::debug!(
                    upstream = %files[0].record.upstream,
                    path = request.relative(),
                    "refreshed content is not cached because cache writes are disabled for this repository"
                );
                cleanup_prepared(&files).await;
                return self
                    .inner
                    .database
                    .touch_refresh_attempt(request.relative(), unix_timestamp())
                    .await
                    .map_err(internal);
            }
            PreparedFetch::NotModified => self
                .inner
                .database
                .touch_refresh_attempt(request.relative(), unix_timestamp())
                .await
                .map_err(internal),
            PreparedFetch::NotFound => {
                self.log_not_found(request);
                self.inner
                    .database
                    .touch_refresh_attempt(request.relative(), unix_timestamp())
                    .await
                    .map_err(internal)
            }
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

    async fn build_checksum(
        &self,
        request: &MavenPath,
        upstream: &str,
        source_path: &Path,
    ) -> Result<PreparedFile, CacheFailure> {
        let (_, sha1, sha256, sha512) = hash_file(source_path).await.map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to hash checksum source {}: {error}",
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
                sha256: Some(format!("{:x}", Sha256::digest(content.as_bytes()))),
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
        let checksum = self
            .build_checksum(request, &source_record.upstream, &source_path)
            .await?;
        self.install_bundle(vec![checksum]).await
    }
}

enum BundleError {
    Unstable(String),
    Internal(CacheFailure),
}
