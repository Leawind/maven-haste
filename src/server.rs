use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::routing::get;
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
    bind: std::net::SocketAddr,
    base_path: String,
    cache: CacheManager,
) -> Result<(), AppError> {
    let app = router(base_path, cache);
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|error| AppError::Runtime(format!("failed to bind {bind}: {error}")))?;
    tracing::info!(%bind, "HTTP server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| AppError::Runtime(format!("HTTP server failed: {error}")))
}

pub fn router(base_path: String, cache: CacheManager) -> Router {
    let route = if base_path == "/" {
        "/{*path}".to_owned()
    } else {
        format!("{base_path}/{{*path}}")
    };
    Router::new()
        .route(&route, get(artifact).head(artifact))
        .with_state(AppState { cache, base_path })
}

async fn artifact(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    method: Method,
) -> Response<Body> {
    let request = match MavenPath::parse(uri.path(), &state.base_path) {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };

    match state.cache.get(&request).await {
        Ok(cached) => cached_response(cached, method == Method::HEAD).await,
        Err(CacheFailure::NotFound) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(CacheFailure::Gateway) => {
            error_response(StatusCode::BAD_GATEWAY, "upstream repositories failed")
        }
        Err(CacheFailure::Internal(error)) => {
            tracing::error!(path = request.relative(), %error, "cache request failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal cache error")
        }
    }
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
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install Ctrl-C handler");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::http::{Request, Uri};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use url::Url;

    use super::*;
    use crate::config::{
        CacheConfig, CircuitBreakerConfig, Config, RepositoryConfig, ServerConfig, StorageConfig,
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
            circuit_breaker: CircuitBreakerConfig::default(),
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
        let mut cache_config = CacheConfig::default();
        cache_config.metadata_ttl = Duration::ZERO;
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
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
        let mut cache_config = CacheConfig::default();
        cache_config.metadata_ttl = Duration::ZERO;
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
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
        assert!(
            database
                .get("com/example/demo/maven-metadata.xml")
                .await
                .unwrap()
                .unwrap()
                .is_not_found
        );
        task.abort();
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
        let mut cache_config = CacheConfig::default();
        cache_config.negative_ttl = Duration::ZERO;
        let (app, _) = test_app_with_cache(
            &directory,
            vec![repository("central", &url, &[])],
            cache_config,
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
        test_app_with_cache(directory, repositories, CacheConfig::default()).await
    }

    async fn test_app_with_cache(
        directory: &TempDir,
        repositories: Vec<RepositoryConfig>,
        cache_config: CacheConfig,
    ) -> (Router, Database) {
        let storage = StorageConfig::resolved_for_test(directory.path().join("repository"));
        let config = Config {
            server: ServerConfig::default(),
            storage,
            cache: cache_config,
            circuit_breaker: CircuitBreakerConfig::default(),
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
}
