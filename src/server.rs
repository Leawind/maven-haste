use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body::Body as _;
use pin_project_lite::pin_project;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

use crate::cache::{CacheFailure, CacheManager, CachedArtifact, ServeGuard, SharedTemp};
use crate::error::AppError;
use crate::request_path::{CachePolicy, MavenPath};

pub const HEALTH_PATH: &str = "/api/v1/health";
const CACHE_STATS_PATH: &str = "/api/v1/cache/stats";

static BYTES_SERVED: AtomicU64 = AtomicU64::new(0);
static INTERRUPTED_REQUESTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct AppState {
    cache: CacheManager,
    base_path: String,
}

pub async fn serve(
    listener: TcpListener,
    base_path: String,
    cache: CacheManager,
) -> Result<(), AppError> {
    let started = Instant::now();
    let app = router(base_path, cache.clone());
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| AppError::Runtime(format!("HTTP server failed: {error}")));
    log_shutdown_summary(&cache, started).await;
    result
}

async fn log_shutdown_summary(cache: &CacheManager, started: Instant) {
    let uptime_ms = started.elapsed().as_millis() as u64;
    let bytes = BYTES_SERVED.load(Ordering::Relaxed);
    let interrupted = INTERRUPTED_REQUESTS.load(Ordering::Relaxed);
    match cache.stats().await {
        Ok(stats) => {
            tracing::info!(
                requests = stats.requests,
                hits = stats.hits,
                stale_hits = stats.stale_hits,
                negative_hits = stats.negative_hits,
                misses = stats.misses,
                files = stats.files,
                bytes,
                interrupted,
                uptime_ms,
                "proxy stopped"
            );
        }
        Err(error) => {
            tracing::info!(bytes, interrupted, uptime_ms, %error, "proxy stopped");
        }
    }
}

pub async fn bind(bind: std::net::SocketAddr) -> Result<TcpListener, AppError> {
    TcpListener::bind(bind)
        .await
        .map_err(|error| AppError::Runtime(format!("failed to bind {bind}: {error}")))
}

pub fn router(base_path: String, cache: CacheManager) -> Router {
    let route = if base_path == "/" {
        "/{*path}".to_owned()
    } else {
        format!("{base_path}/{{*path}}")
    };
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route(CACHE_STATS_PATH, get(cache_stats))
        .route(&route, get(artifact).head(artifact))
        .with_state(AppState { cache, base_path })
}

async fn health(State(state): State<AppState>) -> Response<Body> {
    match state.cache.health().await {
        Ok(()) => Response::new(Body::from("OK")),
        Err(error) => {
            tracing::error!(%error, "health check failed");
            error_response(StatusCode::SERVICE_UNAVAILABLE, "unhealthy")
        }
    }
}

async fn cache_stats(State(state): State<AppState>) -> Response<Body> {
    match state.cache.stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to collect cache statistics");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cache statistics unavailable",
            )
        }
    }
}

async fn artifact(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let started = Instant::now();
    let request = match MavenPath::parse(uri.path(), &state.base_path) {
        Ok(request) => request,
        Err(error) => {
            let response = error_response(StatusCode::BAD_REQUEST, error.to_string());
            return track_response(
                response,
                method,
                uri.path(),
                "invalid",
                None,
                started,
                ResponseArtifacts::default(),
            );
        }
    };
    let (response, cache_status, upstream, artifacts) = match state.cache.get(&request).await {
        Ok(mut cached) => {
            let cache_status = cached.status.as_str();
            let upstream = cached.record.upstream.clone();
            let artifacts = ResponseArtifacts {
                temporary: cached.temporary.take(),
                serve_guard: cached.serve_guard.take(),
            };
            (
                cached_response(cached, request.policy(), method == Method::HEAD, &headers).await,
                cache_status,
                Some(upstream),
                artifacts,
            )
        }
        Err(CacheFailure::NotFound) => (
            error_response(StatusCode::NOT_FOUND, "artifact not found"),
            "none",
            None,
            ResponseArtifacts::default(),
        ),
        Err(CacheFailure::Gateway) => (
            error_response(StatusCode::BAD_GATEWAY, "upstream repositories failed"),
            "none",
            None,
            ResponseArtifacts::default(),
        ),
        Err(CacheFailure::Internal(error)) => {
            tracing::error!(path = request.relative(), %error, "cache request failed");
            (
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal cache error"),
                "error",
                None,
                ResponseArtifacts::default(),
            )
        }
    };
    track_response(
        response,
        method,
        uri.path(),
        cache_status,
        upstream.as_deref(),
        started,
        artifacts,
    )
}

/// Cache-owned resources tied to one response: released when the response
/// body finishes, aborts, or errors.
#[derive(Default)]
struct ResponseArtifacts {
    temporary: Option<SharedTemp>,
    serve_guard: Option<ServeGuard>,
}

fn track_response(
    response: Response<Body>,
    method: Method,
    path: &str,
    cache: &str,
    upstream: Option<&str>,
    started: Instant,
    artifacts: ResponseArtifacts,
) -> Response<Body> {
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();
    // HEAD responses carry no body on the wire: serve an empty one so access
    // tracking finishes eagerly with the response instead of waiting on a body
    // that is never polled.
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        body
    };
    let expected_bytes = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .or_else(|| body.size_hint().exact());
    let state = Arc::new(Mutex::new(AccessState {
        method: method.to_string(),
        path: path.to_owned(),
        status,
        cache: cache.to_owned(),
        upstream: upstream.unwrap_or("-").to_owned(),
        started,
        bytes_sent: 0,
        completion: None,
        artifacts,
    }));
    if body.is_end_stream() {
        finish_access(&state, "complete");
    }
    Response::from_parts(
        parts,
        Body::new(TrackedBody {
            body,
            state,
            expected_bytes,
        }),
    )
}

pin_project! {
    struct TrackedBody {
        #[pin]
        body: Body,
        state: Arc<Mutex<AccessState>>,
        expected_bytes: Option<u64>,
    }

    impl PinnedDrop for TrackedBody {
        fn drop(this: Pin<&mut Self>) {
            finish_access(this.project().state, "aborted");
        }
    }
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this.body.poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.state.lock().expect("access state poisoned").bytes_sent +=
                        data.len() as u64;
                }
                if this.expected_bytes.is_some_and(|expected| {
                    this.state.lock().expect("access state poisoned").bytes_sent >= expected
                }) {
                    finish_access(this.state, "complete");
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                finish_access(this.state, "body_error");
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                finish_access(this.state, "complete");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

struct AccessState {
    method: String,
    path: String,
    status: u16,
    cache: String,
    upstream: String,
    started: Instant,
    bytes_sent: u64,
    completion: Option<&'static str>,
    artifacts: ResponseArtifacts,
}

fn finish_access(state: &Arc<Mutex<AccessState>>, completion: &'static str) {
    let mut state = state.lock().expect("access state poisoned");
    if state.completion.is_some() {
        return;
    }
    state.completion = Some(completion);
    BYTES_SERVED.fetch_add(state.bytes_sent, Ordering::Relaxed);
    if completion != "complete" {
        INTERRUPTED_REQUESTS.fetch_add(1, Ordering::Relaxed);
    }
    let elapsed_ms = state.started.elapsed().as_millis() as u64;
    let prefix = state.cache.to_ascii_uppercase();
    let mut message = format!(
        "[{}] {} {} {} {}ms {}B upstream={}",
        prefix,
        state.method,
        state.path,
        state.status,
        elapsed_ms,
        state.bytes_sent,
        state.upstream
    );
    if completion != "complete" {
        message.push_str(&format!(" completion={completion}"));
    }
    tracing::debug!(
        target: "maven_haste::access",
        cache = state.cache,
        method = state.method,
        path = state.path,
        status = state.status,
        upstream = state.upstream,
        elapsed_ms,
        bytes_sent = state.bytes_sent,
        completion,
        "{message}"
    );
    // Releasing the shared temporary removes the file once this response is
    // the last holder; concurrent responses keep their own references. The
    // drop runs synchronously because it may also fire outside a runtime
    // context when a response body is dropped after shutdown.
    let artifacts = &mut state.artifacts;
    drop(artifacts.temporary.take());
    drop(artifacts.serve_guard.take());
}

async fn cached_response(
    cached: CachedArtifact,
    policy: CachePolicy,
    head: bool,
    request_headers: &HeaderMap,
) -> Response<Body> {
    let size = cached.record.file_size.max(0) as u64;
    let etag = cached.record.etag.clone().or_else(|| {
        cached
            .record
            .sha256
            .as_ref()
            .map(|hash| format!("\"sha256:{hash}\""))
    });
    let cache_control = match policy {
        CachePolicy::Permanent => "public, max-age=31536000, immutable",
        CachePolicy::Mutable => "no-cache",
    };

    if client_cache_is_fresh(
        request_headers,
        etag.as_deref(),
        cached.record.last_modified.as_deref(),
    ) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        insert_common_headers(
            response.headers_mut(),
            cache_control,
            etag.as_deref(),
            cached.record.last_modified.as_deref(),
        );
        return response;
    }

    let range = if if_range_matches(
        request_headers,
        etag.as_deref(),
        cached.record.last_modified.as_deref(),
    ) {
        requested_range(request_headers, size)
    } else {
        ByteRange::Full
    };
    if range == ByteRange::Unsatisfiable {
        let mut response =
            error_response(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable");
        response.headers_mut().insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{size}")).expect("valid content range"),
        );
        response
            .headers_mut()
            .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        return response;
    }

    let (start, length, status) = match range {
        ByteRange::Full => (0, size, StatusCode::OK),
        ByteRange::Partial { start, end } => (
            start,
            end.saturating_sub(start).saturating_add(1),
            StatusCode::PARTIAL_CONTENT,
        ),
        ByteRange::Unsatisfiable => unreachable!("handled above"),
    };
    let body = if head {
        Body::empty()
    } else {
        match File::open(&cached.file_path).await {
            Ok(mut file) => {
                if start > 0
                    && let Err(error) = file.seek(std::io::SeekFrom::Start(start)).await
                {
                    tracing::error!(path = %cached.file_path.display(), %error, "failed to seek cached file");
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "cached file is unavailable",
                    );
                }
                Body::from_stream(ReaderStream::new(file.take(length)))
            }
            Err(error) => {
                tracing::error!(path = %cached.file_path.display(), %error, "cached file disappeared");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cached file is unavailable",
                );
            }
        }
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(
            mime_guess::from_path(&cached.file_path)
                .first_or_octet_stream()
                .as_ref(),
        )
        .expect("MIME guesses are valid header values"),
    );
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        headers.insert(CONTENT_LENGTH, value);
    }
    if let ByteRange::Partial { start, end } = range {
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                .expect("valid content range"),
        );
    }
    insert_common_headers(
        headers,
        cache_control,
        etag.as_deref(),
        cached.record.last_modified.as_deref(),
    );
    response
}

fn insert_common_headers(
    headers: &mut HeaderMap,
    cache_control: &'static str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    insert_optional_header(headers, ETAG, etag);
    insert_optional_header(headers, LAST_MODIFIED, last_modified);
}

fn client_cache_is_fresh(
    headers: &HeaderMap,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    if let Some(supplied) = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        return supplied == "*"
            || etag.is_some_and(|etag| {
                supplied
                    .split(',')
                    .map(str::trim)
                    .any(|candidate| weak_etag(candidate) == weak_etag(etag))
            });
    }
    headers
        .get(IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .zip(last_modified)
        .is_some_and(|(supplied, cached)| supplied == cached)
}

fn if_range_matches(headers: &HeaderMap, etag: Option<&str>, last_modified: Option<&str>) -> bool {
    let Some(supplied) = headers.get(IF_RANGE).and_then(|value| value.to_str().ok()) else {
        return true;
    };
    if supplied.starts_with('"') || supplied.starts_with("W/\"") {
        etag.is_some_and(|etag| supplied == etag)
    } else {
        last_modified.is_some_and(|last_modified| supplied == last_modified)
    }
}

fn weak_etag(etag: &str) -> &str {
    etag.strip_prefix("W/").unwrap_or(etag)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn requested_range(headers: &HeaderMap, size: u64) -> ByteRange {
    let Some(value) = headers.get(RANGE).and_then(|value| value.to_str().ok()) else {
        return ByteRange::Full;
    };
    let Some(value) = value.strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    if value.contains(',') {
        return ByteRange::Full;
    }
    let Some((start, end)) = value.split_once('-') else {
        return ByteRange::Full;
    };
    if size == 0 {
        return ByteRange::Unsatisfiable;
    }
    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return ByteRange::Full;
        };
        if suffix == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Partial {
            start: size.saturating_sub(suffix),
            end: size - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return ByteRange::Full;
    };
    if start >= size {
        return ByteRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return ByteRange::Full;
        };
        end.min(size - 1)
    };
    if end < start {
        ByteRange::Unsatisfiable
    } else {
        ByteRange::Partial { start, end }
    }
}

fn insert_optional_header(headers: &mut HeaderMap, name: HeaderName, value: Option<&str>) {
    if let Some(value) = value.and_then(|value| HeaderValue::from_str(value).ok()) {
        headers.insert(name, value);
    }
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    let message = message.into();
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            if let Err(error) = crate::logging::notify_shutdown_requested() {
                tracing::error!(%error, "failed to print shutdown notice");
            }
        }
        Err(error) => tracing::error!(%error, "failed to install Ctrl-C handler"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use http_body_util::BodyExt;

    use super::*;

    fn access_state() -> Arc<Mutex<AccessState>> {
        Arc::new(Mutex::new(AccessState {
            method: "GET".into(),
            path: "/maven/test".into(),
            status: 200,
            cache: "hit".into(),
            upstream: "central".into(),
            started: Instant::now(),
            bytes_sent: 0,
            completion: None,
            artifacts: ResponseArtifacts::default(),
        }))
    }

    #[tokio::test]
    async fn head_error_responses_complete_with_the_response() {
        let response = track_response(
            error_response(StatusCode::NOT_FOUND, "artifact not found"),
            Method::HEAD,
            "/maven/test",
            "none",
            None,
            Instant::now(),
            ResponseArtifacts::default(),
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.body().is_end_stream());
    }

    #[tokio::test]
    async fn get_error_responses_keep_their_message_body() {
        let response = track_response(
            error_response(StatusCode::NOT_FOUND, "artifact not found"),
            Method::GET,
            "/maven/test",
            "none",
            None,
            Instant::now(),
            ResponseArtifacts::default(),
        );
        assert!(!response.body().is_end_stream());
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, &b"artifact not found"[..]);
    }

    #[tokio::test]
    async fn tracked_body_records_complete_bytes_once() {
        let state = access_state();
        let body = TrackedBody {
            body: Body::from("abcdef"),
            state: state.clone(),
            expected_bytes: Some(6),
        };
        Body::new(body).collect().await.unwrap();
        finish_access(&state, "aborted");
        let state = state.lock().unwrap();
        assert_eq!(state.bytes_sent, 6);
        assert_eq!(state.completion, Some("complete"));
    }

    #[tokio::test]
    async fn tracked_body_distinguishes_errors_and_aborts() {
        let error_state = access_state();
        let stream = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("broken"))]);
        let body = TrackedBody {
            body: Body::from_stream(stream),
            state: error_state.clone(),
            expected_bytes: None,
        };
        assert!(Body::new(body).collect().await.is_err());
        assert_eq!(error_state.lock().unwrap().completion, Some("body_error"));

        let aborted_state = access_state();
        drop(TrackedBody {
            body: Body::from_stream(
                futures_util::stream::pending::<Result<Bytes, std::io::Error>>(),
            ),
            state: aborted_state.clone(),
            expected_bytes: None,
        });
        assert_eq!(aborted_state.lock().unwrap().completion, Some("aborted"));
    }
}
