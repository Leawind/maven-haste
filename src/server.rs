use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body::Body as _;
use pin_project_lite::pin_project;
use tokio::fs::File;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

use crate::cache::{CacheFailure, CacheManager, CachedArtifact};
use crate::error::AppError;
use crate::request_path::MavenPath;

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
    let app = router(base_path, cache);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| AppError::Runtime(format!("HTTP server failed: {error}")))
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
        .route("/__health", get(health))
        .route("/__cache/stats", get(cache_stats))
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
) -> Response<Body> {
    let started = Instant::now();
    let request = match MavenPath::parse(uri.path(), &state.base_path) {
        Ok(request) => request,
        Err(error) => {
            let response = error_response(StatusCode::BAD_REQUEST, error.to_string());
            return track_response(response, method, uri.path(), "invalid", None, started);
        }
    };

    let (response, cache_status, upstream) = match state.cache.get(&request).await {
        Ok(cached) => {
            let cache_status = cached.status.as_str();
            let upstream = cached.record.upstream.clone();
            (
                cached_response(cached, method == Method::HEAD).await,
                cache_status,
                Some(upstream),
            )
        }
        Err(CacheFailure::NotFound) => (
            error_response(StatusCode::NOT_FOUND, "artifact not found"),
            "none",
            None,
        ),
        Err(CacheFailure::Gateway) => (
            error_response(StatusCode::BAD_GATEWAY, "upstream repositories failed"),
            "none",
            None,
        ),
        Err(CacheFailure::Internal(error)) => {
            tracing::error!(path = request.relative(), %error, "cache request failed");
            (
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal cache error"),
                "error",
                None,
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
    )
}

fn track_response(
    response: Response<Body>,
    method: Method,
    path: &str,
    cache: &str,
    upstream: Option<&str>,
    started: Instant,
) -> Response<Body> {
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();
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
}

fn finish_access(state: &Arc<Mutex<AccessState>>, completion: &'static str) {
    let mut state = state.lock().expect("access state poisoned");
    if state.completion.is_some() {
        return;
    }
    state.completion = Some(completion);
    let elapsed_ms = state.started.elapsed().as_millis() as u64;
    let prefix = state.cache.to_ascii_uppercase();
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
        "[{}] {} {} {} {}ms {}B upstream={}",
        prefix, state.method, state.path, state.status, elapsed_ms, state.bytes_sent, state.upstream
    );
}

async fn cached_response(cached: CachedArtifact, head: bool) -> Response<Body> {
    let body = if head {
        Body::empty()
    } else {
        match File::open(&cached.file_path).await {
            Ok(file) => Body::from_stream(ReaderStream::new(file)),
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
    *response.status_mut() = StatusCode::OK;
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
    if let Ok(value) = HeaderValue::from_str(&cached.record.file_size.max(0).to_string()) {
        headers.insert(CONTENT_LENGTH, value);
    }
    insert_optional_header(headers, ETAG, cached.record.etag.as_deref());
    insert_optional_header(
        headers,
        LAST_MODIFIED,
        cached.record.last_modified.as_deref(),
    );
    response
}

fn insert_optional_header(
    headers: &mut axum::http::HeaderMap,
    name: axum::http::HeaderName,
    value: Option<&str>,
) {
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
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::http::{Request, Uri};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use url::Url;

    use super::*;
    use crate::config::{
        CacheConfig, CircuitBreakerConfig, Config, RepositoryConfig, ServerConfig, StorageConfig,
        UpstreamConfig,
    };
    use crate::db::Database;
    use crate::storage;

    const ARTIFACT_PATH: &str = "/maven/com/example/demo/1.0/demo-1.0.jar";

    #[tokio::test]
    async fn downloads_once_then_serves_get_and_head_from_permanent_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    let mut response = Response::new(Body::from("artifact-body"));
                    response
                        .headers_mut()
                        .insert(ETAG, HeaderValue::from_static("\"upstream-tag\""));
                    response
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, database) = test_app(&directory, vec![repository("central", &url, &[])]).await;
        let cached_path = directory
            .path()
            .join("repository/com/example/demo/1.0/demo-1.0.jar");
        tokio::fs::create_dir_all(cached_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cached_path, "untracked-orphan")
            .await
            .unwrap();

        let first = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[ETAG], "\"upstream-tag\"");
        assert_eq!(body(first).await, "artifact-body");

        let second = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(body(second).await, "artifact-body");
        let head = request(&app, Method::HEAD, ARTIFACT_PATH).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[CONTENT_LENGTH], "13");
        assert!(body(head).await.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::fs::remove_file(&cached_path).await.unwrap();
        let repaired = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(body(repaired).await, "artifact-body");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let record = database
            .get("com/example/demo/1.0/demo-1.0.jar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.group_id, "com.example");
        assert_eq!(record.artifact_id, "demo");
        assert_eq!(record.upstream, "central");
        task.abort();
    }

    #[tokio::test]
    async fn reports_health_and_cache_statistics() {
        let upstream = Router::new().route(
            "/{*path}",
            get(|OriginalUri(uri): OriginalUri| async move {
                if is_checksum_uri(&uri) {
                    error_response(StatusCode::NOT_FOUND, "missing checksum")
                } else {
                    Response::new(Body::from("abc"))
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, _) = test_app(&directory, vec![repository("central", &url, &[])]).await;

        let health = request(&app, Method::GET, "/__health").await;
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(body(health).await, "OK");

        assert_eq!(
            request(&app, Method::GET, ARTIFACT_PATH).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, Method::GET, ARTIFACT_PATH).await.status(),
            StatusCode::OK
        );
        let stats = request(&app, Method::GET, "/__cache/stats").await;
        assert_eq!(stats.status(), StatusCode::OK);
        assert_eq!(stats.headers()[CONTENT_TYPE], "application/json");
        let stats: Value = serde_json::from_str(&body(stats).await).unwrap();
        assert_eq!(stats["files"].as_u64(), Some(3));
        assert!(stats["total_size"].as_u64().is_some_and(|size| size >= 3));
        assert_eq!(stats["requests"].as_u64(), Some(2));
        assert_eq!(stats["hits"].as_u64(), Some(1));
        assert_eq!(stats["misses"].as_u64(), Some(1));
        assert_eq!(stats["hit_rate"].as_f64(), Some(0.5));
        assert!(stats["upstreams"].as_array().is_some_and(|upstreams| {
            upstreams.iter().any(|upstream| {
                upstream["name"].as_str() == Some("central")
                    && upstream["circuit"].as_str() == Some("closed")
            })
        }));
        task.abort();
    }

    #[tokio::test]
    async fn newer_case_variant_replaces_old_database_identity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    Response::new(Body::from("case-body"))
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let storage = StorageConfig::resolved_for_test(directory.path().join("repository"));
        let config = Config {
            server: ServerConfig::default(),
            storage,
            cache: CacheConfig::default(),
            upstream: UpstreamConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            logging: crate::config::LoggingConfig::default(),
            repositories: vec![repository("central", &url, &[])],
        };
        storage::prepare(&config.storage).await.unwrap();
        let database = Database::open(config.storage.db_path()).await.unwrap();
        let cache = CacheManager::new(&config, database.clone(), false).unwrap();
        let app = router("/maven".into(), cache);

        let upper = "/maven/Com/Example/demo/1.0/demo-1.0.jar";
        let lower = "/maven/com/example/demo/1.0/demo-1.0.jar";
        assert_eq!(
            request(&app, Method::GET, upper).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, Method::GET, lower).await.status(),
            StatusCode::OK
        );

        assert!(
            database
                .get("Com/Example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            database
                .get("com/example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_upstream_download() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Response::new(Body::from("shared"))
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, _) = test_app(&directory, vec![repository("central", &url, &[])]).await;

        let requests = (0..8).map(|_| {
            let app = app.clone();
            tokio::spawn(async move { request(&app, Method::GET, ARTIFACT_PATH).await })
        });
        for response in futures_util::future::join_all(requests).await {
            assert_eq!(response.unwrap().status(), StatusCode::OK);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn upstream_limit_is_held_until_response_body_finishes() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let upstream = Router::new().route(
            "/{*path}",
            get({
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                move |OriginalUri(uri): OriginalUri| {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    async move {
                        if is_checksum_uri(&uri) {
                            return error_response(StatusCode::NOT_FOUND, "missing checksum");
                        }
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        let stream = futures_util::stream::once(async move {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok::<_, Infallible>(Bytes::from_static(b"limited"))
                        });
                        Response::new(Body::from_stream(stream))
                    }
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("limited", &url, &[])],
            CacheConfig::default(),
            UpstreamConfig {
                max_concurrency: 1,
                default_repository_max_concurrency: 1,
                ..UpstreamConfig::default()
            },
        )
        .await;

        let first = {
            let app = app.clone();
            tokio::spawn(async move { request(&app, Method::GET, ARTIFACT_PATH).await })
        };
        let second = {
            let app = app.clone();
            tokio::spawn(async move {
                request(
                    &app,
                    Method::GET,
                    "/maven/com/example/other/1.0/other-1.0.jar",
                )
                .await
            })
        };
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(second.await.unwrap().status(), StatusCode::OK);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn retries_server_errors_and_respects_excluding_routes() {
        let excluded_calls = Arc::new(AtomicUsize::new(0));
        let excluded_handler_calls = Arc::clone(&excluded_calls);
        let excluded = Router::new().route(
            "/{*path}",
            get(move || {
                let calls = Arc::clone(&excluded_handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "wrong"
                }
            }),
        );
        let (excluded_url, excluded_task) = spawn_upstream(excluded).await;

        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback_handler_calls = Arc::clone(&fallback_calls);
        let fallback = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&fallback_handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        error_response(StatusCode::INTERNAL_SERVER_ERROR, "retry")
                    } else {
                        Response::new(Body::from("eventual-success"))
                    }
                }
            }),
        );
        let (fallback_url, fallback_task) = spawn_upstream(fallback).await;
        let directory = TempDir::new().unwrap();
        let repositories = vec![
            repository("excluded", &excluded_url, &["org/other/*", "!*"]),
            repository("fallback", &fallback_url, &[]),
        ];
        let (app, _) = test_app(&directory, repositories).await;

        let response = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "eventual-success");
        assert_eq!(excluded_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 3);
        excluded_task.abort();
        fallback_task.abort();
    }

    #[tokio::test]
    async fn returns_not_found_when_every_repository_is_excluded() {
        let directory = TempDir::new().unwrap();
        let unreachable = Url::parse("http://127.0.0.1:9/").unwrap();
        let repositories = vec![repository("excluded", &unreachable, &["!*"])];
        let (app, _) = test_app(&directory, repositories).await;

        let response = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn distinguishes_permanent_and_mutable_upstream_failures() {
        let directory = TempDir::new().unwrap();
        let unreachable = Url::parse("http://127.0.0.1:9/").unwrap();
        let upstream_config = UpstreamConfig {
            connect_timeout: Duration::from_millis(100),
            read_timeout: Duration::from_millis(100),
            ..UpstreamConfig::default()
        };
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("unreachable", &unreachable, &[])],
            CacheConfig::default(),
            upstream_config,
        )
        .await;

        assert_eq!(
            request(&app, Method::GET, ARTIFACT_PATH).await.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            request(
                &app,
                Method::GET,
                "/maven/com/example/demo/maven-metadata.xml",
            )
            .await
            .status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn downloads_slow_streams_that_continue_before_read_timeout() {
        let upstream = Router::new().route(
            "/{*path}",
            get(|OriginalUri(uri): OriginalUri| async move {
                if is_checksum_uri(&uri) {
                    return error_response(StatusCode::NOT_FOUND, "missing checksum");
                }
                Response::new(delayed_body(vec![
                    (Duration::from_millis(30), b"slow-"),
                    (Duration::from_millis(30), b"stream-"),
                    (Duration::from_millis(30), b"still-"),
                    (Duration::from_millis(30), b"works"),
                ]))
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("slow", &url, &[])],
            CacheConfig::default(),
            UpstreamConfig {
                connect_timeout: Duration::from_millis(100),
                read_timeout: Duration::from_millis(50),
                ..UpstreamConfig::default()
            },
        )
        .await;

        let response = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "slow-stream-still-works");
        task.abort();
    }

    #[tokio::test]
    async fn returns_gateway_and_cleans_temporary_file_after_read_stalls() {
        let upstream = Router::new().route(
            "/{*path}",
            get(|OriginalUri(uri): OriginalUri| async move {
                if is_checksum_uri(&uri) {
                    return error_response(StatusCode::NOT_FOUND, "missing checksum");
                }
                Response::new(delayed_body(vec![
                    (Duration::ZERO, b"first"),
                    (Duration::from_millis(150), b"never-received"),
                ]))
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, database) = test_app_with_cache(
            &directory,
            vec![repository("stalled", &url, &[])],
            CacheConfig::default(),
            UpstreamConfig {
                connect_timeout: Duration::from_millis(100),
                read_timeout: Duration::from_millis(50),
                ..UpstreamConfig::default()
            },
        )
        .await;

        assert_eq!(
            request(&app, Method::GET, ARTIFACT_PATH).await.status(),
            StatusCode::BAD_GATEWAY
        );
        assert!(
            database
                .get("com/example/demo/1.0/demo-1.0.jar")
                .await
                .unwrap()
                .is_none()
        );
        let tmp = directory.path().join("repository/.maven-haste/tmp");
        assert!(
            tokio::fs::read_dir(tmp)
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
        task.abort();
    }

    #[tokio::test]
    async fn returns_stale_metadata_while_refreshing_in_background() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Response::new(Body::from("metadata-v1"))
                    } else {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Response::new(Body::from("metadata-v2"))
                    }
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let cache_config = CacheConfig {
            metadata_ttl: Duration::ZERO,
            ..CacheConfig::default()
        };
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
            UpstreamConfig::default(),
        )
        .await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "metadata-v1"
        );
        let stale_requests = (0..8).map(|_| {
            let app = app.clone();
            tokio::spawn(async move { body(request(&app, Method::GET, metadata).await).await })
        });
        for stale in futures_util::future::join_all(stale_requests).await {
            assert_eq!(stale.unwrap(), "metadata-v1");
        }
        wait_for(|| calls.load(Ordering::SeqCst) >= 2).await;
        wait_for_file(
            &directory
                .path()
                .join("repository/com/example/demo/maven-metadata.xml"),
            b"metadata-v2",
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "metadata-v2"
        );
        task.abort();
    }

    #[tokio::test]
    async fn keeps_stale_metadata_when_refresh_is_not_found() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    if is_checksum_uri(&uri) {
                        return error_response(StatusCode::NOT_FOUND, "missing checksum");
                    }
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        Response::new(Body::from("metadata-v1"))
                    } else {
                        error_response(StatusCode::NOT_FOUND, "missing")
                    }
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let cache_config = CacheConfig {
            metadata_ttl: Duration::ZERO,
            ..CacheConfig::default()
        };
        let (app, database) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
            UpstreamConfig::default(),
        )
        .await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "metadata-v1"
        );
        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "metadata-v1"
        );
        wait_for(|| calls.load(Ordering::SeqCst) == 2).await;
        wait_for(|| {
            std::fs::read_to_string(
                directory
                    .path()
                    .join("repository/com/example/demo/maven-metadata.xml"),
            )
            .is_ok_and(|content| content == "metadata-v1")
        })
        .await;
        for _ in 0..100 {
            if database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .is_ok_and(|entries| entries.len() == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "metadata-v1"
        );
        task.abort();
    }

    #[tokio::test]
    async fn conditional_refresh_accepts_not_modified() {
        let calls = Arc::new(AtomicUsize::new(0));
        let saw_condition = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_calls = Arc::clone(&calls);
        let handler_condition = Arc::clone(&saw_condition);
        let upstream = Router::new().route(
            "/{*path}",
            get(
                move |OriginalUri(uri): OriginalUri, headers: axum::http::HeaderMap| {
                    let calls = Arc::clone(&handler_calls);
                    let saw_condition = Arc::clone(&handler_condition);
                    async move {
                        if is_checksum_uri(&uri) {
                            return error_response(StatusCode::NOT_FOUND, "missing checksum");
                        }
                        calls.fetch_add(1, Ordering::SeqCst);
                        if headers.get(reqwest::header::IF_NONE_MATCH).is_some() {
                            saw_condition.store(true, Ordering::SeqCst);
                            return error_response(StatusCode::NOT_MODIFIED, "");
                        }
                        let mut response = Response::new(Body::from("unchanged"));
                        response
                            .headers_mut()
                            .insert(ETAG, HeaderValue::from_static("\"metadata-tag\""));
                        response
                    }
                },
            ),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let cache_config = CacheConfig {
            metadata_ttl: Duration::ZERO,
            ..CacheConfig::default()
        };
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
            UpstreamConfig::default(),
        )
        .await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "unchanged"
        );
        assert_eq!(
            body(request(&app, Method::GET, metadata).await).await,
            "unchanged"
        );
        wait_for(|| saw_condition.load(Ordering::SeqCst)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn caches_not_found_only_for_mutable_files() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    error_response(StatusCode::NOT_FOUND, "missing")
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, database) = test_app(&directory, vec![repository("central", &url, &[])]).await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .len(),
            1
        );
        task.abort();
    }

    #[tokio::test]
    async fn newly_configured_upstream_is_tried_despite_existing_negative_entry() {
        let missing_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&missing_calls);
        let missing = Router::new().route(
            "/{*path}",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    error_response(StatusCode::NOT_FOUND, "missing")
                }
            }),
        );
        let (missing_url, missing_task) = spawn_upstream(missing).await;
        let directory = TempDir::new().unwrap();
        let (first_app, _) =
            test_app(&directory, vec![repository("missing", &missing_url, &[])]).await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";
        assert_eq!(
            request(&first_app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );

        let found_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&found_calls);
        let found = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if is_checksum_uri(&uri) {
                        error_response(StatusCode::NOT_FOUND, "missing checksum")
                    } else {
                        Response::new(Body::from("metadata"))
                    }
                }
            }),
        );
        let (found_url, found_task) = spawn_upstream(found).await;
        let (second_app, database) = test_app(
            &directory,
            vec![
                repository("missing", &missing_url, &[]),
                repository("found", &found_url, &[]),
            ],
        )
        .await;

        assert_eq!(
            body(request(&second_app, Method::GET, metadata).await).await,
            "metadata"
        );
        assert_eq!(missing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(found_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .len(),
            1
        );
        missing_task.abort();
        found_task.abort();
    }

    #[tokio::test]
    async fn changing_upstream_url_invalidates_its_negative_entry() {
        let missing = Router::new().route(
            "/{*path}",
            get(|| async { error_response(StatusCode::NOT_FOUND, "missing") }),
        );
        let (missing_url, missing_task) = spawn_upstream(missing).await;
        let directory = TempDir::new().unwrap();
        let (first_app, _) =
            test_app(&directory, vec![repository("central", &missing_url, &[])]).await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";
        assert_eq!(
            request(&first_app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );

        let found = Router::new().route(
            "/{*path}",
            get(|OriginalUri(uri): OriginalUri| async move {
                if is_checksum_uri(&uri) {
                    error_response(StatusCode::NOT_FOUND, "missing checksum")
                } else {
                    Response::new(Body::from("metadata"))
                }
            }),
        );
        let (found_url, found_task) = spawn_upstream(found).await;
        let (second_app, _) =
            test_app(&directory, vec![repository("central", &found_url, &[])]).await;
        assert_eq!(
            body(request(&second_app, Method::GET, metadata).await).await,
            "metadata"
        );
        missing_task.abort();
        found_task.abort();
    }

    #[tokio::test]
    async fn records_explicit_not_found_when_another_upstream_fails() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let missing = Router::new().route(
            "/{*path}",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    error_response(StatusCode::NOT_FOUND, "missing")
                }
            }),
        );
        let (missing_url, missing_task) = spawn_upstream(missing).await;
        let unreachable = Url::parse("http://127.0.0.1:9/").unwrap();
        let directory = TempDir::new().unwrap();
        let (app, database) = test_app(
            &directory,
            vec![
                repository("missing", &missing_url, &[]),
                repository("failed", &unreachable, &[]),
            ],
        )
        .await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            database
                .negative_entries("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .len(),
            1
        );
        missing_task.abort();
    }

    #[tokio::test]
    async fn retries_mutable_not_found_after_negative_ttl_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    error_response(StatusCode::NOT_FOUND, "missing")
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let cache_config = CacheConfig {
            negative_ttl: Duration::ZERO,
            ..CacheConfig::default()
        };
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
            UpstreamConfig::default(),
        )
        .await;
        let metadata = "/maven/com/example/demo/maven-metadata.xml";

        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(&app, Method::GET, metadata).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn generates_and_serves_missing_checksums() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let upstream = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if is_checksum_uri(&uri) {
                        error_response(StatusCode::NOT_FOUND, "missing checksum")
                    } else {
                        Response::new(Body::from("abc"))
                    }
                }
            }),
        );
        let (url, task) = spawn_upstream(upstream).await;
        let directory = TempDir::new().unwrap();
        let (app, database) = test_app(&directory, vec![repository("central", &url, &[])]).await;

        assert_eq!(
            body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
            "abc"
        );
        let sha1_path = format!("{ARTIFACT_PATH}.sha1");
        let sha256_path = format!("{ARTIFACT_PATH}.sha256");
        assert_eq!(
            body(request(&app, Method::GET, &sha1_path).await).await,
            "a9993e364706816aba3e25717850c26c9cd0d89d\n"
        );
        assert_eq!(
            body(request(&app, Method::GET, &sha256_path).await).await,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let record = database
            .get("com/example/demo/1.0/demo-1.0.jar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.sha1.as_deref(),
            Some("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        task.abort();
    }

    #[tokio::test]
    async fn checksum_mismatch_falls_back_to_next_repository() {
        let bad_calls = Arc::new(AtomicUsize::new(0));
        let bad_handler_calls = Arc::clone(&bad_calls);
        let bad = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&bad_handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if uri.path().ends_with(".sha1") {
                        Response::new(Body::from("0000000000000000000000000000000000000000\n"))
                    } else {
                        Response::new(Body::from("abc"))
                    }
                }
            }),
        );
        let (bad_url, bad_task) = spawn_upstream(bad).await;

        let good_calls = Arc::new(AtomicUsize::new(0));
        let good_handler_calls = Arc::clone(&good_calls);
        let good = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let calls = Arc::clone(&good_handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if is_checksum_uri(&uri) {
                        error_response(StatusCode::NOT_FOUND, "missing checksum")
                    } else {
                        Response::new(Body::from("xyz"))
                    }
                }
            }),
        );
        let (good_url, good_task) = spawn_upstream(good).await;
        let directory = TempDir::new().unwrap();
        let repositories = vec![
            repository("bad", &bad_url, &[]),
            repository("good", &good_url, &[]),
        ];
        let (app, _) = test_app(&directory, repositories).await;

        assert_eq!(
            body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
            "xyz"
        );
        assert_eq!(bad_calls.load(Ordering::SeqCst), 2);
        assert_eq!(good_calls.load(Ordering::SeqCst), 3);
        bad_task.abort();
        good_task.abort();
    }

    async fn test_app(
        directory: &TempDir,
        repositories: Vec<RepositoryConfig>,
    ) -> (Router, Database) {
        test_app_with_cache(
            directory,
            repositories,
            CacheConfig::default(),
            UpstreamConfig::default(),
        )
        .await
    }

    async fn test_app_with_cache(
        directory: &TempDir,
        repositories: Vec<RepositoryConfig>,
        cache_config: CacheConfig,
        upstream_config: UpstreamConfig,
    ) -> (Router, Database) {
        let storage = StorageConfig::resolved_for_test(directory.path().join("repository"));
        let config = Config {
            server: ServerConfig::default(),
            storage,
            cache: cache_config,
            upstream: upstream_config,
            circuit_breaker: CircuitBreakerConfig::default(),
            logging: crate::config::LoggingConfig::default(),
            repositories,
        };
        let environment = storage::prepare(&config.storage).await.unwrap();
        let database = Database::open(config.storage.db_path()).await.unwrap();
        let cache =
            CacheManager::new(&config, database.clone(), environment.case_sensitive).unwrap();
        (router("/maven".into(), cache), database)
    }

    fn repository(name: &str, url: &Url, rules: &[&str]) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            url: url.clone(),
            max_concurrency: None,
            rules: rules.iter().map(|rule| (*rule).into()).collect(),
        }
    }

    async fn spawn_upstream(app: Router) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    async fn request(app: &Router, method: Method, uri: &str) -> Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(Uri::try_from(uri).unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body(response: Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn is_checksum_uri(uri: &Uri) -> bool {
        uri.path().ends_with(".sha1") || uri.path().ends_with(".sha256")
    }

    fn delayed_body(chunks: Vec<(Duration, &'static [u8])>) -> Body {
        Body::from_stream(futures_util::stream::unfold(
            chunks.into_iter(),
            |mut chunks| async move {
                let (delay, bytes) = chunks.next()?;
                tokio::time::sleep(delay).await;
                Some((Ok::<_, Infallible>(Bytes::from_static(bytes)), chunks))
            },
        ))
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        for _ in 0..100 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition was not met before timeout");
    }

    async fn wait_for_file(path: &std::path::Path, expected: &[u8]) {
        for _ in 0..100 {
            if tokio::fs::read(path)
                .await
                .is_ok_and(|content| content == expected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("file {} was not updated before timeout", path.display());
    }

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
        }))
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
