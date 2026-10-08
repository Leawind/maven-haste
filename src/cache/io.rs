use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use sha2::{Sha256, Sha512};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

use crate::cache::CacheFailure;
use crate::cache::types::{DownloadedMain, PreparedFile};
use crate::db::ArtifactRecord;
use crate::upstream::{FetchResult, RequestPriority, UpstreamClient, UpstreamResponse};

/// Upper bound accepted for an upstream checksum file response.
pub(crate) const MAX_CHECKSUM_BYTES: usize = 64 * 1024;

/// Fetches one checksum file: `None` when the repository did not return the
/// file, `Some(Ok(..))` with its raw text, `Some(Err(..))` when the response
/// could not be read as a checksum.
pub(crate) async fn fetch_checksum_file(
    upstream: &UpstreamClient,
    repository: &str,
    relative: &str,
    priority: RequestPriority,
) -> Option<Result<String, CacheFailure>> {
    let (checksum_repository, response) =
        match upstream.fetch_from(repository, relative, priority).await {
            FetchResult::Found {
                repository,
                response,
                ..
            } => (repository, response),
            FetchResult::NotFound => return None,
            _ => return Some(Err(CacheFailure::Gateway)),
        };
    match read_checksum_response(response, MAX_CHECKSUM_BYTES).await {
        Ok(supplied) => {
            upstream.record_body_success(&checksum_repository);
            Some(Ok(supplied))
        }
        Err(error) => Some(Err(error)),
    }
}

/// Why streaming an upstream body to disk failed.
pub(crate) enum StreamFailure {
    /// The upstream stream broke after `bytes` were already written; a byte
    /// range request can continue the download from that offset.
    Interrupted { error: CacheFailure, bytes: u64 },
    /// The download cannot be continued.
    Failed(CacheFailure),
}

/// Streams an upstream body into `destination`, hashing the complete file. A
/// `206 Partial Content` response whose range starts inside the bytes already
/// on disk resumes from there; any other success status restarts the file.
pub(crate) async fn stream_response_to_file(
    response: UpstreamResponse,
    destination: &Path,
    resume: Option<(u64, Option<u64>)>,
) -> Result<(u64, String, String, String), StreamFailure> {
    let (response, _permit) = response.into_parts();
    let mut file;
    let mut size;
    let expected_total;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let (start, total) = response_range(&response).map_err(StreamFailure::Failed)?;
        expected_total = Some(total);
        if !resume.is_some_and(|(offset, expected)| {
            start == offset && expected.is_none_or(|expected| expected == total)
        }) {
            return Err(StreamFailure::Failed(CacheFailure::Internal(
                "unexpected upstream partial range".into(),
            )));
        }
        (file, size) = open_for_resume(destination, start, &mut sha1, &mut sha256, &mut sha512)
            .await
            .map_err(StreamFailure::Failed)?;
    } else {
        expected_total = response.content_length();
        file = create_truncated(destination)
            .await
            .map_err(StreamFailure::Failed)?;
        size = 0;
    }
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| StreamFailure::Interrupted {
            error: CacheFailure::Internal(format!(
                "failed to read upstream response body: {error}"
            )),
            bytes: size,
        })?;
        file.write_all(&chunk).await.map_err(|error| {
            StreamFailure::Failed(CacheFailure::Internal(format!(
                "failed to write temporary file {}: {error}",
                destination.display()
            )))
        })?;
        sha1.update(&chunk);
        sha256.update(&chunk);
        sha512.update(&chunk);
        size = size.saturating_add(chunk.len() as u64);
    }
    match expected_total {
        Some(expected) if size < expected => {
            return Err(StreamFailure::Interrupted {
                error: CacheFailure::Internal(format!(
                    "upstream response length mismatch: expected {expected} bytes, received {size}"
                )),
                bytes: size,
            });
        }
        Some(expected) if size > expected => {
            return Err(StreamFailure::Failed(CacheFailure::Internal(format!(
                "upstream response length mismatch: expected {expected} bytes, received {size}"
            ))));
        }
        _ => {}
    }
    flush_temporary(file, destination)
        .await
        .map_err(StreamFailure::Failed)?;
    Ok((
        size,
        to_hex(&sha1.finalize()),
        to_hex(&sha256.finalize()),
        to_hex(&sha512.finalize()),
    ))
}

/// Parses the first byte position of a `Content-Range` header such as
/// `bytes 300-1023/1024`.
fn response_range(response: &reqwest::Response) -> Result<(u64, u64), CacheFailure> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            CacheFailure::Internal("partial response is missing Content-Range".into())
        })?;
    let invalid = || CacheFailure::Internal(format!("invalid Content-Range {value:?}"));
    let (range, total) = value
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(invalid)?;
    let (start, end) = range.split_once('-').ok_or_else(invalid)?;
    let start = start.parse::<u64>().map_err(|_| invalid())?;
    let end = end.parse::<u64>().map_err(|_| invalid())?;
    let total = total.parse::<u64>().map_err(|_| invalid())?;
    if start > end
        || end.checked_add(1) != Some(total)
        || response
            .content_length()
            .is_some_and(|length| length != total - start)
    {
        return Err(invalid());
    }
    Ok((start, total))
}

/// Opens an interrupted download for continuation: hashes the first `start`
/// bytes kept on disk into the running digests, truncates any misaligned
/// tail, and positions the file for appending.
async fn open_for_resume(
    destination: &Path,
    start: u64,
    sha1: &mut Sha1,
    sha256: &mut Sha256,
    sha512: &mut Sha512,
) -> Result<(fs::File, u64), CacheFailure> {
    let open_error = |error: std::io::Error| {
        CacheFailure::Internal(format!(
            "failed to open partial download {}: {error}",
            destination.display()
        ))
    };
    let present = fs::metadata(destination).await.map_err(open_error)?.len();
    if present < start {
        return Err(CacheFailure::Internal(format!(
            "cannot resume {} at byte {start}: only {present} bytes are present",
            destination.display()
        )));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)
        .await
        .map_err(open_error)?;
    let mut remaining = start;
    let mut buffer = vec![0_u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..want]).await.map_err(open_error)?;
        if read == 0 {
            return Err(CacheFailure::Internal(format!(
                "partial download {} ended before byte {start}",
                destination.display()
            )));
        }
        let chunk = &buffer[..read];
        sha1.update(chunk);
        sha256.update(chunk);
        sha512.update(chunk);
        remaining -= read as u64;
    }
    file.set_len(start).await.map_err(open_error)?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(open_error)?;
    Ok((file, start))
}

/// Creates or truncates the destination so a fresh or restarted download
/// begins from an empty file.
async fn create_truncated(path: &Path) -> Result<fs::File, CacheFailure> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|error| {
            CacheFailure::Internal(format!(
                "failed to create temporary file {}: {error}",
                path.display()
            ))
        })
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

/// Flushes user-space buffers and persists the file contents before the file
/// is renamed into the cache, so a rename can never publish a file whose data
/// was not yet written to disk.
pub(crate) async fn flush_temporary(mut file: fs::File, path: &Path) -> Result<(), CacheFailure> {
    file.flush().await.map_err(|error| {
        CacheFailure::Internal(format!(
            "failed to flush temporary file {}: {error}",
            path.display()
        ))
    })?;
    file.sync_all().await.map_err(|error| {
        CacheFailure::Internal(format!(
            "failed to persist temporary file {}: {error}",
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
    hash_open_file(&mut file).await
}

pub(crate) async fn hash_open_file(
    file: &mut fs::File,
) -> Result<(u64, String, String, String), std::io::Error> {
    file.seek(std::io::SeekFrom::Start(0)).await?;
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
        to_hex(&sha1.finalize()),
        to_hex(&sha256.finalize()),
        to_hex(&sha512.finalize()),
    ))
}

/// Content-addressed objects let one database transaction publish a whole
/// artifact bundle without replacing files used by older responses.
pub(crate) fn object_path(root: &Path, hash: &str) -> Option<PathBuf> {
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| root.join(".maven-haste/objects").join(hash))
}

pub(crate) async fn recorded_file_path(
    root: &Path,
    record: &ArtifactRecord,
) -> Result<PathBuf, std::io::Error> {
    if let Some(path) = record
        .sha256
        .as_deref()
        .and_then(|hash| object_path(root, hash))
    {
        match fs::metadata(&path).await {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(relative_file_path(root, &record.path))
}

/// Serialize publication and collection across server and CLI processes.
/// The OS releases the advisory lock if the owning process terminates.
pub(crate) async fn lock_objects(root: &Path) -> Result<std::fs::File, CacheFailure> {
    let path = root.join(".maven-haste/objects.lock");
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(internal)?
    .map_err(internal)
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

pub(crate) fn internal(error: impl std::fmt::Display) -> CacheFailure {
    CacheFailure::Internal(error.to_string())
}

pub(crate) fn unix_timestamp() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}
