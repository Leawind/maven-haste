use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::cache::CacheFailure;
use crate::cache::types::{DownloadedMain, PreparedFile};
use crate::db::ArtifactRecord;
use crate::error::AppError;
use crate::upstream::UpstreamResponse;

pub(crate) async fn stream_to_file(
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

pub(crate) async fn read_checksum_response(
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

pub(crate) async fn write_temporary(path: &Path, content: &[u8]) -> Result<(), CacheFailure> {
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

pub(crate) async fn create_temporary(path: &Path) -> Result<fs::File, CacheFailure> {
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

pub(crate) async fn flush_temporary(mut file: fs::File, path: &Path) -> Result<(), CacheFailure> {
    file.flush().await.map_err(|error| {
        CacheFailure::Internal(format!(
            "failed to flush temporary file {}: {error}",
            path.display()
        ))
    })?;
    drop(file);
    Ok(())
}

pub(crate) async fn cleanup_prepared(files: &[PreparedFile]) {
    for file in files {
        let _ = remove_file_if_exists(&file.temporary).await;
    }
}

pub(crate) async fn cleanup_downloads(downloads: &[DownloadedMain]) {
    for download in downloads {
        let _ = remove_file_if_exists(&download.temporary).await;
    }
}

pub(crate) async fn hash_file(
    path: &Path,
) -> Result<(u64, String, String, String), std::io::Error> {
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

pub(crate) async fn remove_empty_parents(parent: Option<&Path>, root: &Path) {
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

pub(crate) fn normalize_cache_prefix(prefix: &str) -> Result<String, CacheFailure> {
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

pub(crate) fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

pub(crate) fn temporary_path(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(format!("{}.part", Uuid::new_v4()))
}

pub(crate) fn relative_file_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

pub(crate) async fn remove_file_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn is_fresh(record: &ArtifactRecord, ttl: Duration) -> bool {
    let timestamp = record.last_refresh_attempt.unwrap_or(record.created_at);
    is_timestamp_fresh(timestamp, ttl)
}

pub(crate) fn is_timestamp_fresh(timestamp: i64, ttl: Duration) -> bool {
    let age = unix_timestamp().saturating_sub(timestamp) as u64;
    Duration::from_secs(age) < ttl
}

pub(crate) fn internal(error: AppError) -> CacheFailure {
    CacheFailure::Internal(error.to_string())
}

pub(crate) fn unix_timestamp() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}
