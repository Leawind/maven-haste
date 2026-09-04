use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::OriginalUri;
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
    RANGE,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::get;
use http_body_util::BodyExt;
use maven_haste::cache::CacheManager;
use maven_haste::config::{
    CacheConfig, CircuitBreakerConfig, Config, LoggingConfig, RepositoryConfig, ServerConfig,
    StorageConfig, UpstreamConfig,
};
use maven_haste::db::Database;
use maven_haste::server::router;
use maven_haste::storage;
use maven_haste::upstream::UpstreamClient;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;
use url::Url;

const ARTIFACT_PATH: &str = "/maven/com/example/demo/1.0/demo-1.0.jar";

fn error_response(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    let mut response = Response::new(Body::from(message.into()));
    *response.status_mut() = status;
    response
}

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
    assert_eq!(record.request_count, 4);
    task.abort();
}

#[tokio::test]
async fn refetches_a_cached_file_whose_size_no_longer_matches_its_record() {
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
                Response::new(Body::from("artifact-body"))
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("central", &url, &[])]).await;
    let cached_path = directory
        .path()
        .join("repository/com/example/demo/1.0/demo-1.0.jar");
    tokio::fs::create_dir_all(cached_path.parent().unwrap())
        .await
        .unwrap();

    let first = request(&app, Method::GET, ARTIFACT_PATH).await;
    assert_eq!(body(first).await, "artifact-body");
    tokio::fs::write(&cached_path, "corrupt").await.unwrap();

    let second = request(&app, Method::GET, ARTIFACT_PATH).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(body(second).await, "artifact-body");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn serves_client_validators_and_single_byte_ranges_from_cache() {
    let upstream = Router::new().route(
        "/{*path}",
        get(|OriginalUri(uri): OriginalUri| async move {
            if is_checksum_uri(&uri) {
                return error_response(StatusCode::NOT_FOUND, "missing checksum");
            }
            let mut response = Response::new(Body::from("artifact-body"));
            response
                .headers_mut()
                .insert(ETAG, HeaderValue::from_static("\"artifact-tag\""));
            response
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("central", &url, &[])]).await;
    let initial = request(&app, Method::GET, ARTIFACT_PATH).await;
    assert_eq!(
        initial.headers()[CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(initial.headers()[ACCEPT_RANGES], "bytes");
    assert_eq!(body(initial).await, "artifact-body");

    let mut validators = HeaderMap::new();
    validators.insert(
        IF_NONE_MATCH,
        HeaderValue::from_static("W/\"artifact-tag\""),
    );
    let not_modified = request_with_headers(&app, Method::GET, ARTIFACT_PATH, validators).await;
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    assert!(body(not_modified).await.is_empty());

    let mut partial_headers = HeaderMap::new();
    partial_headers.insert(RANGE, HeaderValue::from_static("bytes=2-5"));
    let partial = request_with_headers(&app, Method::GET, ARTIFACT_PATH, partial_headers).await;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.headers()[CONTENT_RANGE], "bytes 2-5/13");
    assert_eq!(partial.headers()[CONTENT_LENGTH], "4");
    assert_eq!(body(partial).await, "tifa");

    let mut suffix_headers = HeaderMap::new();
    suffix_headers.insert(RANGE, HeaderValue::from_static("bytes=-4"));
    let suffix = request_with_headers(&app, Method::HEAD, ARTIFACT_PATH, suffix_headers).await;
    assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(suffix.headers()[CONTENT_RANGE], "bytes 9-12/13");
    assert_eq!(suffix.headers()[CONTENT_LENGTH], "4");
    assert!(body(suffix).await.is_empty());

    let mut inverted_headers = HeaderMap::new();
    inverted_headers.insert(RANGE, HeaderValue::from_static("bytes=5-2"));
    let inverted = request_with_headers(&app, Method::GET, ARTIFACT_PATH, inverted_headers).await;
    assert_eq!(inverted.status(), StatusCode::OK);
    assert_eq!(inverted.headers()[CONTENT_LENGTH], "13");
    assert_eq!(body(inverted).await, "artifact-body");

    let mut unsatisfiable_headers = HeaderMap::new();
    unsatisfiable_headers.insert(RANGE, HeaderValue::from_static("bytes=20-30"));
    let unsatisfiable =
        request_with_headers(&app, Method::GET, ARTIFACT_PATH, unsatisfiable_headers).await;
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(unsatisfiable.headers()[CONTENT_RANGE], "bytes */13");
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

    let health = request(&app, Method::GET, "/api/v1/health").await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body(health).await, "OK");
    for removed_path in ["/", "/__health", "/__cache/stats"] {
        assert_eq!(
            request(&app, Method::GET, removed_path).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    assert_eq!(
        request(&app, Method::GET, ARTIFACT_PATH).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, Method::GET, ARTIFACT_PATH).await.status(),
        StatusCode::OK
    );
    let stats = request(&app, Method::GET, "/api/v1/cache/stats").await;
    assert_eq!(stats.status(), StatusCode::OK);
    assert_eq!(stats.headers()[CONTENT_TYPE], "application/json");
    let stats: Value = serde_json::from_str(&body(stats).await).unwrap();
    assert_eq!(stats["files"].as_u64(), Some(4));
    assert!(stats["total_size"].as_u64().is_some_and(|size| size >= 3));
    assert_eq!(stats["requests"].as_u64(), Some(2));
    assert_eq!(stats["hits"].as_u64(), Some(1));
    assert_eq!(stats["misses"].as_u64(), Some(1));
    assert_eq!(stats["hit_rate"].as_f64(), Some(0.5));
    assert!(stats["upstreams"].as_array().is_some_and(|upstreams| {
        upstreams.iter().any(|upstream| {
            upstream["id"].as_str() == Some("central")
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
    let storage = StorageConfig::resolved(directory.path().join("repository"));
    let config = Config {
        schema: None,
        server: ServerConfig::default(),
        storage,
        cache: CacheConfig::default(),
        upstream: UpstreamConfig::default(),
        circuit_breaker: CircuitBreakerConfig::default(),
        logging: LoggingConfig::default(),
        repositories: vec![repository("central", &url, &[])],
    };
    storage::prepare(&config.storage).await.unwrap();
    let database = Database::open(config.storage.db_path()).await.unwrap();
    let upstream = UpstreamClient::new(
        config.repositories.clone(),
        &config.upstream,
        &config.circuit_breaker,
    )
    .unwrap();
    let caching_disabled = config
        .repositories
        .iter()
        .filter(|repository| !repository.cache_writes)
        .map(|repository| repository.id.clone())
        .collect::<HashSet<_>>();
    let cache = CacheManager::new(
        config.storage.clone(),
        config.cache.clone(),
        database.clone(),
        upstream,
        false,
        caching_disabled,
    );
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
        repository("excluded", &excluded_url, &["org/other/**", "!**"]),
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
    let repositories = vec![repository("excluded", &unreachable, &["!**"])];
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
async fn rejects_upstream_body_shorter_than_content_length() {
    let upstream = Router::new().route(
        "/{*path}",
        get(|| async {
            Response::builder()
                .header(CONTENT_LENGTH, "4")
                .body(Body::from("abc"))
                .unwrap()
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, database) = test_app(&directory, vec![repository("short", &url, &[])]).await;

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
        get(move |OriginalUri(uri): OriginalUri, headers: HeaderMap| {
            let calls = Arc::clone(&handler_calls);
            let saw_condition = Arc::clone(&handler_condition);
            async move {
                if is_checksum_uri(&uri) {
                    return error_response(StatusCode::NOT_FOUND, "missing checksum");
                }
                calls.fetch_add(1, Ordering::SeqCst);
                if headers.get(IF_NONE_MATCH).is_some() {
                    saw_condition.store(true, Ordering::SeqCst);
                    return error_response(StatusCode::NOT_MODIFIED, "");
                }
                let mut response = Response::new(Body::from("unchanged"));
                response
                    .headers_mut()
                    .insert(ETAG, HeaderValue::from_static("\"metadata-tag\""));
                response
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
    let (first_app, _) = test_app(&directory, vec![repository("missing", &missing_url, &[])]).await;
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
    assert_eq!(found_calls.load(Ordering::SeqCst), 4);
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
    let (first_app, _) = test_app(&directory, vec![repository("central", &missing_url, &[])]).await;
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
    let (second_app, _) = test_app(&directory, vec![repository("central", &found_url, &[])]).await;
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
    let sha512_path = format!("{ARTIFACT_PATH}.sha512");
    assert_eq!(
        body(request(&app, Method::GET, &sha1_path).await).await,
        "a9993e364706816aba3e25717850c26c9cd0d89d\n"
    );
    assert_eq!(
        body(request(&app, Method::GET, &sha256_path).await).await,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"
    );
    assert_eq!(
        body(request(&app, Method::GET, &sha512_path).await).await,
        concat!(
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2",
            "192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f\n"
        )
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    let record = database
        .get("com/example/demo/1.0/demo-1.0.jar")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.sha1.as_deref(),
        Some("a9993e364706816aba3e25717850c26c9cd0d89d")
    );

    let sha512_relative = sha512_path.strip_prefix("/maven/").unwrap();
    database
        .delete_paths(vec![sha512_relative.into()])
        .await
        .unwrap();
    tokio::fs::remove_file(directory.path().join("repository").join(sha512_relative))
        .await
        .unwrap();
    assert_eq!(
        body(request(&app, Method::GET, &sha512_path).await).await,
        concat!(
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2",
            "192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f\n"
        )
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    task.abort();
}

#[tokio::test]
async fn checksum_first_request_is_generated_from_cached_content() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let upstream = Router::new().route(
        "/{*path}",
        get(move |OriginalUri(uri): OriginalUri| {
            let calls = Arc::clone(&handler_calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if is_checksum_uri(&uri) {
                    Response::new(Body::from("upstream-is-wrong\n"))
                } else {
                    Response::new(Body::from("abc"))
                }
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("wrong", &url, &[])]).await;
    let sha1_path = format!("{ARTIFACT_PATH}.sha1");

    assert_eq!(
        body(request(&app, Method::GET, &sha1_path).await).await,
        "a9993e364706816aba3e25717850c26c9cd0d89d\n"
    );
    assert_eq!(
        body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
        "abc"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    task.abort();
}

#[tokio::test]
async fn stable_checksum_mismatch_keeps_first_repository() {
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
        "abc"
    );
    assert_eq!(
        body(request(&app, Method::GET, &format!("{ARTIFACT_PATH}.sha1")).await).await,
        "a9993e364706816aba3e25717850c26c9cd0d89d\n"
    );
    assert_eq!(bad_calls.load(Ordering::SeqCst), 8);
    assert_eq!(good_calls.load(Ordering::SeqCst), 0);
    let stats: Value =
        serde_json::from_str(&body(request(&app, Method::GET, "/api/v1/cache/stats").await).await)
            .unwrap();
    let bad_status = stats["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|upstream| upstream["id"] == "bad")
        .unwrap();
    assert_eq!(bad_status["failures"], 0);
    bad_task.abort();
    good_task.abort();
}

#[tokio::test]
async fn stable_metadata_with_inconsistent_checksums_returns_success() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let upstream = Router::new().route(
        "/{*path}",
        get(move |OriginalUri(uri): OriginalUri| {
            let calls = Arc::clone(&handler_calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if is_checksum_uri(&uri) {
                    Response::new(Body::from("invalid-checksum\n"))
                } else {
                    Response::new(Body::from("<metadata/>"))
                }
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("metadata", &url, &[])]).await;
    let metadata = "/maven/com/example/demo/1.0-SNAPSHOT/maven-metadata.xml";

    let response = request(&app, Method::GET, metadata).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, "<metadata/>");
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    task.abort();
}

#[tokio::test]
async fn checksum_retry_uses_subsequent_verified_download() {
    let main_calls = Arc::new(AtomicUsize::new(0));
    let handler_main_calls = Arc::clone(&main_calls);
    let upstream = Router::new().route(
        "/{*path}",
        get(move |OriginalUri(uri): OriginalUri| {
            let calls = Arc::clone(&handler_main_calls);
            async move {
                if uri.path().ends_with(".sha1") {
                    Response::new(Body::from("66b27417d37e024c46526c2f6d358a754fc552f3\n"))
                } else if uri.path().ends_with(".sha256") {
                    Response::new(Body::from(
                        "3608bca1e44ea6c4d268eb6db02260269892c0b42b86bbf1e77a6fa16c3c9282\n",
                    ))
                } else if uri.path().ends_with(".sha512") {
                    error_response(StatusCode::NOT_FOUND, "missing checksum")
                } else if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Response::new(Body::from("abc"))
                } else {
                    Response::new(Body::from("xyz"))
                }
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("changing", &url, &[])]).await;

    assert_eq!(
        body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
        "xyz"
    );
    assert_eq!(main_calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn checksum_retry_failure_keeps_first_complete_download() {
    let main_calls = Arc::new(AtomicUsize::new(0));
    let handler_main_calls = Arc::clone(&main_calls);
    let upstream = Router::new().route(
        "/{*path}",
        get(move |OriginalUri(uri): OriginalUri| {
            let calls = Arc::clone(&handler_main_calls);
            async move {
                if is_checksum_uri(&uri) {
                    Response::new(Body::from("invalid-checksum\n"))
                } else if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Response::new(Body::from("abc"))
                } else {
                    error_response(StatusCode::NOT_FOUND, "retry unavailable")
                }
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app(&directory, vec![repository("retry", &url, &[])]).await;

    assert_eq!(
        body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
        "abc"
    );
    assert_eq!(main_calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn unstable_checksum_mismatch_falls_back_to_next_repository() {
    let bad_main_calls = Arc::new(AtomicUsize::new(0));
    let bad_handler_main_calls = Arc::clone(&bad_main_calls);
    let bad = Router::new().route(
        "/{*path}",
        get(move |OriginalUri(uri): OriginalUri| {
            let calls = Arc::clone(&bad_handler_main_calls);
            async move {
                if is_checksum_uri(&uri) {
                    return Response::new(Body::from("0000000000000000000000000000000000000000\n"));
                }
                let body = match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => "abc",
                    1 => "def",
                    _ => "ghi",
                };
                Response::new(Body::from(body))
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
        repository("unstable", &bad_url, &[]),
        repository("good", &good_url, &[]),
    ];
    let (app, _) = test_app(&directory, repositories).await;

    assert_eq!(
        body(request(&app, Method::GET, ARTIFACT_PATH).await).await,
        "xyz"
    );
    assert_eq!(bad_main_calls.load(Ordering::SeqCst), 3);
    assert_eq!(good_calls.load(Ordering::SeqCst), 4);
    bad_task.abort();
    good_task.abort();
}

#[tokio::test]
async fn does_not_cache_artifacts_when_repository_writes_are_disabled() {
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
    let (app, database) = test_app_with_cache(
        &directory,
        vec![repository_with_cache("nocache", &url, &[], false)],
        CacheConfig::default(),
        UpstreamConfig::default(),
    )
    .await;

    for _ in 0..2 {
        let response = request(&app, Method::GET, ARTIFACT_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "artifact-body");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let checksum_path = "/maven/com/example/demo/1.0/demo-1.0.jar.sha256";
    for _ in 0..2 {
        let response = request(&app, Method::GET, checksum_path).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    assert!(
        !directory
            .path()
            .join("repository/com/example/demo/1.0/demo-1.0.jar")
            .exists()
    );
    assert!(
        database
            .get("com/example/demo/1.0/demo-1.0.jar")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        database
            .get("com/example/demo/1.0/demo-1.0.jar.sha256")
            .await
            .unwrap()
            .is_none()
    );
    wait_for(|| {
        std::fs::read_dir(directory.path().join("repository/.maven-haste/tmp"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(false)
    })
    .await;

    task.abort();
}

#[tokio::test]
async fn concurrent_passthrough_requests_share_one_temporary_file_safely() {
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
                Response::new(Body::from("artifact-body"))
            }
        }),
    );
    let (url, task) = spawn_upstream(upstream).await;
    let directory = TempDir::new().unwrap();
    let (app, _) = test_app_with_cache(
        &directory,
        vec![repository_with_cache("nocache", &url, &[], false)],
        CacheConfig::default(),
        UpstreamConfig::default(),
    )
    .await;

    let requests = (0..8).map(|_| {
        let app = app.clone();
        tokio::spawn(async move { request(&app, Method::GET, ARTIFACT_PATH).await })
    });
    for response in futures_util::future::join_all(requests).await {
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "artifact-body");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    wait_for(|| {
        std::fs::read_dir(directory.path().join("repository/.maven-haste/tmp"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(false)
    })
    .await;

    task.abort();
}

async fn test_app(directory: &TempDir, repositories: Vec<RepositoryConfig>) -> (Router, Database) {
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
    let storage = StorageConfig::resolved(directory.path().join("repository"));
    let config = Config {
        schema: None,
        server: ServerConfig::default(),
        storage,
        cache: cache_config,
        upstream: upstream_config,
        circuit_breaker: CircuitBreakerConfig::default(),
        logging: LoggingConfig::default(),
        repositories,
    };
    let environment = storage::prepare(&config.storage).await.unwrap();
    let database = Database::open(config.storage.db_path()).await.unwrap();
    let upstream = UpstreamClient::new(
        config.repositories.clone(),
        &config.upstream,
        &config.circuit_breaker,
    )
    .unwrap();
    let caching_disabled = config
        .repositories
        .iter()
        .filter(|repository| !repository.cache_writes)
        .map(|repository| repository.id.clone())
        .collect::<HashSet<_>>();
    let cache = CacheManager::new(
        config.storage.clone(),
        config.cache.clone(),
        database.clone(),
        upstream,
        environment.case_sensitive,
        caching_disabled,
    );
    (router("/maven".into(), cache), database)
}

fn repository(id: &str, url: &Url, rules: &[&str]) -> RepositoryConfig {
    repository_with_cache(id, url, rules, true)
}

fn repository_with_cache(
    id: &str,
    url: &Url,
    rules: &[&str],
    cache_writes: bool,
) -> RepositoryConfig {
    RepositoryConfig {
        id: id.into(),
        url: url.clone(),
        use_proxy: None,
        max_concurrency: None,
        rules: rules.iter().map(|rule| (*rule).into()).collect(),
        cache_writes,
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
    request_with_headers(app, method, uri, HeaderMap::new()).await
}

async fn request_with_headers(
    app: &Router,
    method: Method,
    uri: &str,
    headers: HeaderMap,
) -> Response<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(Uri::try_from(uri).unwrap())
        .body(Body::empty())
        .unwrap();
    *request.headers_mut() = headers;
    app.clone().oneshot(request).await.unwrap()
}

async fn body(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn is_checksum_uri(uri: &Uri) -> bool {
    uri.path().ends_with(".sha1")
        || uri.path().ends_with(".sha256")
        || uri.path().ends_with(".sha512")
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
