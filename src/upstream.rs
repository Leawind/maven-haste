use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::oneshot;

use crate::cache::io::to_hex;
use crate::circuit::CircuitBreaker;
use crate::config::{CircuitBreakerConfig, RepositoryConfig, UpstreamConfig};
use crate::error::AppError;
use crate::routing::RouteEngine;

const MAX_ATTEMPTS: usize = 3;

/// Circuit-breaker key used inside each per-proxy circuit breaker. The
/// leading `$` is outside the repository id alphabet, so it can never collide
/// with one.
const PROXY_CIRCUIT: &str = "$proxy";

/// One configured upstream proxy with its own connection path, circuit
/// breaker, and reachability flag.
struct ProxyEgress {
    url: Url,
    client: Client,
    circuit: CircuitBreaker,
    unreachable: AtomicBool,
}

#[derive(Clone)]
pub struct UpstreamClient {
    /// Configured proxies in failover order.
    proxies: Arc<[ProxyEgress]>,
    direct_client: Client,
    proxy_fallback: bool,
    routes: RouteEngine,
    circuits: Arc<CircuitBreaker>,
    scheduler: RequestScheduler,
}

/// Connection path for one upstream request. `Proxy(index)` selects an entry
/// from the configured proxy list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Egress {
    Proxy(usize),
    Direct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyChoice {
    Proxy,
    Direct,
}

fn proxy_choice(use_proxy: Option<bool>, has_global_proxy: bool) -> ProxyChoice {
    match use_proxy {
        Some(true) => ProxyChoice::Proxy,
        Some(false) => ProxyChoice::Direct,
        None if has_global_proxy => ProxyChoice::Proxy,
        None => ProxyChoice::Direct,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestPriority {
    Foreground,
    Background,
}

pub struct UpstreamResponse {
    response: reqwest::Response,
    permit: RequestPermit,
}

impl UpstreamResponse {
    pub(crate) fn status(&self) -> StatusCode {
        self.response.status()
    }

    pub fn headers(&self) -> &reqwest::header::HeaderMap {
        self.response.headers()
    }

    pub(crate) fn into_parts(self) -> (reqwest::Response, RequestPermit) {
        (self.response, self.permit)
    }
}

pub enum FetchResult {
    Found {
        repository: String,
        repository_id: String,
        response: UpstreamResponse,
    },
    NotModified {
        repository_id: String,
    },
    NotFound,
    GatewayFailure,
}

pub struct FetchOutcome {
    pub result: FetchResult,
    pub not_found: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamStatus {
    pub id: String,
    pub circuit: String,
    pub failures: u32,
}

/// Extra request headers applied to a single upstream fetch: client
/// validators for a conditional refresh, or a byte offset to resume an
/// interrupted download from.
#[derive(Clone, Default)]
struct RequestExtras {
    etag: Option<String>,
    last_modified: Option<String>,
    range_from: Option<u64>,
    if_range: Option<String>,
}

#[derive(Clone)]
struct RequestScheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    state: Mutex<SchedulerState>,
    global_limit: usize,
    repository_limits: HashMap<String, usize>,
    foreground_priority_burst: usize,
}

#[derive(Default)]
struct SchedulerState {
    global_in_use: usize,
    repository_in_use: HashMap<String, usize>,
    foreground: VecDeque<Waiter>,
    background: VecDeque<Waiter>,
    consecutive_foreground: usize,
}

struct Waiter {
    repository: String,
    ready: oneshot::Sender<RequestPermit>,
}

pub(crate) struct RequestPermit {
    scheduler: RequestScheduler,
    repository: String,
    active: bool,
}

impl RequestScheduler {
    fn new(repositories: &[RepositoryConfig], config: &UpstreamConfig) -> Self {
        let repository_limits = repositories
            .iter()
            .map(|repository| {
                (
                    repository.id.clone(),
                    repository
                        .max_concurrency
                        .unwrap_or(config.default_repository_max_concurrency),
                )
            })
            .collect();
        Self {
            inner: Arc::new(SchedulerInner {
                state: Mutex::new(SchedulerState::default()),
                global_limit: config.max_concurrency,
                repository_limits,
                foreground_priority_burst: config.foreground_priority_burst,
            }),
        }
    }

    async fn acquire(&self, repository: &str, priority: RequestPriority) -> RequestPermit {
        let (ready, waiting) = oneshot::channel();
        {
            let mut state = self.lock_state();
            let waiter = Waiter {
                repository: repository.to_owned(),
                ready,
            };
            match priority {
                RequestPriority::Foreground => state.foreground.push_back(waiter),
                RequestPriority::Background => state.background.push_back(waiter),
            }
            self.schedule(&mut state);
        }
        waiting
            .await
            .expect("request scheduler always retains a sender for queued waiters")
    }

    fn release(&self, repository: &str) {
        let mut state = self.lock_state();
        state.global_in_use = state
            .global_in_use
            .checked_sub(1)
            .expect("global request permit count is balanced");
        let repository_in_use = state
            .repository_in_use
            .get_mut(repository)
            .expect("repository request permit count exists");
        *repository_in_use = repository_in_use
            .checked_sub(1)
            .expect("repository request permit count is balanced");
        self.schedule(&mut state);
    }

    fn schedule(&self, state: &mut SchedulerState) {
        while state.global_in_use < self.inner.global_limit {
            let foreground = self.eligible_index(&state.foreground, state);
            let background = self.eligible_index(&state.background, state);
            let selection = match (foreground, background) {
                (Some(_), Some(background_index))
                    if state.consecutive_foreground >= self.inner.foreground_priority_burst =>
                {
                    (RequestPriority::Background, background_index)
                }
                (Some(index), _) => (RequestPriority::Foreground, index),
                (None, Some(index)) => (RequestPriority::Background, index),
                (None, None) => break,
            };
            let waiter = match selection.0 {
                RequestPriority::Foreground => state.foreground.remove(selection.1),
                RequestPriority::Background => state.background.remove(selection.1),
            }
            .expect("eligible waiter index exists");

            state.global_in_use += 1;
            *state
                .repository_in_use
                .entry(waiter.repository.clone())
                .or_default() += 1;
            let permit = RequestPermit {
                scheduler: self.clone(),
                repository: waiter.repository,
                active: true,
            };
            if let Err(mut permit) = waiter.ready.send(permit) {
                permit.active = false;
                state.global_in_use -= 1;
                *state
                    .repository_in_use
                    .get_mut(&permit.repository)
                    .expect("repository request permit count exists") -= 1;
                continue;
            }
            match selection.0 {
                RequestPriority::Foreground if background.is_some() => {
                    state.consecutive_foreground += 1;
                }
                RequestPriority::Foreground | RequestPriority::Background => {
                    state.consecutive_foreground = 0;
                }
            }
        }
    }

    fn eligible_index(&self, queue: &VecDeque<Waiter>, state: &SchedulerState) -> Option<usize> {
        queue.iter().position(|waiter| {
            let limit = self
                .inner
                .repository_limits
                .get(&waiter.repository)
                .expect("configured repository has a request limit");
            state
                .repository_in_use
                .get(&waiter.repository)
                .copied()
                .unwrap_or_default()
                < *limit
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SchedulerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if self.active {
            self.scheduler.release(&self.repository);
        }
    }
}

impl UpstreamClient {
    pub fn new(
        repositories: Vec<RepositoryConfig>,
        config: &UpstreamConfig,
        circuit: &CircuitBreakerConfig,
    ) -> Result<Self, AppError> {
        let allowed_origins: Vec<_> = repositories
            .iter()
            .map(|repository| repository.url.origin())
            .collect();
        let proxies = config
            .proxy
            .iter()
            .map(|proxy| {
                Ok(ProxyEgress {
                    url: proxy.clone(),
                    client: build_http_client(config, Some(proxy), &allowed_origins)?,
                    circuit: CircuitBreaker::new(
                        circuit.failure_threshold,
                        circuit.recovery_timeout,
                    ),
                    unreachable: AtomicBool::new(false),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let direct_client = build_http_client(config, None, &allowed_origins)?;
        let scheduler = RequestScheduler::new(&repositories, config);
        Ok(Self {
            proxies: proxies.into(),
            direct_client,
            proxy_fallback: config.proxy_fallback,
            routes: RouteEngine::new(repositories),
            circuits: Arc::new(CircuitBreaker::new(
                circuit.failure_threshold,
                circuit.recovery_timeout,
            )),
            scheduler,
        })
    }

    fn client(&self, egress: Egress) -> &Client {
        match egress {
            Egress::Proxy(index) => &self.proxies[index].client,
            Egress::Direct => &self.direct_client,
        }
    }

    /// Connection paths to try for one upstream request, in order: the
    /// configured proxies, then a direct connection when the fallback is
    /// enabled and the repository does not require a proxy. Proxies with an
    /// open circuit are skipped; when every proxy is skipped, only the direct
    /// connection is offered.
    fn egresses(&self, repository: &RepositoryConfig) -> Vec<Egress> {
        match proxy_choice(repository.use_proxy, !self.proxies.is_empty()) {
            ProxyChoice::Direct => vec![Egress::Direct],
            ProxyChoice::Proxy if repository.use_proxy == Some(true) || !self.proxy_fallback => {
                (0..self.proxies.len()).map(Egress::Proxy).collect()
            }
            ProxyChoice::Proxy => {
                let mut egresses: Vec<_> = (0..self.proxies.len())
                    .filter(|index| self.proxies[*index].circuit.allow(PROXY_CIRCUIT))
                    .map(Egress::Proxy)
                    .collect();
                egresses.push(Egress::Direct);
                egresses
            }
        }
    }

    /// Sends one upstream request through the ordered connection paths. An
    /// unreachable proxy hands the request to the next path; any other error
    /// fails immediately.
    async fn send_request(
        &self,
        repository: &RepositoryConfig,
        url: &Url,
        extras: Option<&RequestExtras>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut last_error = None;
        for egress in self.egresses(repository) {
            let failure = match self.send_via(egress, url, extras).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let Egress::Proxy(index) = egress else {
                return Err(failure);
            };
            if !proxy_unreachable(&failure) {
                return Err(failure);
            }
            self.note_proxy_failure(index, &failure);
            last_error = Some(failure);
        }
        Err(last_error.expect("the connection path chain is never empty"))
    }

    async fn send_via(
        &self,
        egress: Egress,
        url: &Url,
        extras: Option<&RequestExtras>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self.client(egress).get(url.clone());
        if let Some(extras) = extras {
            if let Some(etag) = &extras.etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(last_modified) = &extras.last_modified {
                request = request.header(IF_MODIFIED_SINCE, last_modified);
            }
            if let Some(offset) = extras.range_from {
                request = request.header(RANGE, format!("bytes={offset}-"));
            }
            if let Some(validator) = &extras.if_range {
                request = request.header(IF_RANGE, validator);
            }
        }
        match request.send().await {
            Ok(response) => {
                if let Egress::Proxy(index) = egress {
                    self.note_proxy_success(index);
                }
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }

    /// Records a proxy connection failure. After
    /// `circuit_breaker.failure_threshold` consecutive failures that proxy's
    /// circuit opens and it is skipped until
    /// `circuit_breaker.recovery_timeout` has elapsed and a probe succeeds.
    fn note_proxy_failure(&self, index: usize, error: &reqwest::Error) {
        let proxy = &self.proxies[index];
        proxy.circuit.record_failure(PROXY_CIRCUIT);
        if !proxy.circuit.is_open(PROXY_CIRCUIT) {
            tracing::debug!(
                proxy = %proxy.url,
                error = %describe_error(error),
                "upstream proxy request failed; trying the next connection path"
            );
            return;
        }
        if !proxy.unreachable.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                proxy = %proxy.url,
                error = %describe_error(error),
                "upstream proxy is unreachable; skipping it until it recovers"
            );
        }
    }

    fn note_proxy_success(&self, index: usize) {
        let proxy = &self.proxies[index];
        proxy.circuit.record_success(PROXY_CIRCUIT);
        if proxy.unreachable.swap(false, Ordering::Relaxed) {
            tracing::info!(
                proxy = %proxy.url,
                "upstream proxy is reachable again; routing requests through it"
            );
        }
    }

    pub async fn fetch(
        &self,
        relative_path: &str,
        excluded: &HashSet<String>,
        negative: &HashSet<String>,
    ) -> FetchOutcome {
        let repositories = self
            .routes
            .candidates(relative_path)
            .into_iter()
            .filter(|repository| !excluded.contains(&repository.id))
            .filter(|repository| !negative.contains(&repository_id(repository)))
            .cloned()
            .map(|repository| (repository, None))
            .collect();
        self.fetch_ordered(relative_path, repositories, RequestPriority::Foreground)
            .await
    }

    pub async fn refresh(
        &self,
        relative_path: &str,
        preferred: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        excluded: &HashSet<String>,
        negative: &HashSet<String>,
    ) -> FetchOutcome {
        let mut repositories = Vec::new();
        if !excluded.contains(preferred)
            && let Some(repository) = self.routes.repository(preferred)
            && !negative.contains(&repository_id(repository))
        {
            repositories.push((
                repository.clone(),
                Some(RequestExtras {
                    etag: etag.map(str::to_owned),
                    last_modified: last_modified.map(str::to_owned),
                    range_from: None,
                    if_range: None,
                }),
            ));
        }
        repositories.extend(
            self.routes
                .candidates(relative_path)
                .into_iter()
                .filter(|repository| {
                    repository.id != preferred && !excluded.contains(&repository.id)
                })
                .filter(|repository| !negative.contains(&repository_id(repository)))
                .cloned()
                .map(|repository| (repository, None)),
        );
        self.fetch_ordered(relative_path, repositories, RequestPriority::Background)
            .await
    }

    pub async fn fetch_from(
        &self,
        repository: &str,
        relative_path: &str,
        priority: RequestPriority,
    ) -> FetchResult {
        let Some(repository) = self.routes.repository(repository).cloned() else {
            return FetchResult::GatewayFailure;
        };
        self.fetch_ordered(relative_path, vec![(repository, None)], priority)
            .await
            .result
    }

    /// Fetches `relative_path` from one repository starting at `offset`, to
    /// resume an interrupted download.
    pub async fn fetch_range(
        &self,
        repository: &str,
        relative_path: &str,
        offset: u64,
        validator: &str,
        priority: RequestPriority,
    ) -> FetchResult {
        let Some(repository) = self.routes.repository(repository).cloned() else {
            return FetchResult::GatewayFailure;
        };
        let extras = RequestExtras {
            range_from: Some(offset),
            if_range: Some(validator.to_owned()),
            ..RequestExtras::default()
        };
        self.fetch_ordered(relative_path, vec![(repository, Some(extras))], priority)
            .await
            .result
    }

    pub fn record_body_success(&self, repository: &str) {
        self.circuits.record_success(repository);
    }

    pub fn record_body_failure(&self, repository: &str) {
        self.circuits.record_failure(repository);
    }

    pub fn statuses(&self) -> Vec<UpstreamStatus> {
        self.routes
            .repositories()
            .iter()
            .map(|repository| {
                let status = self.circuits.status(&repository.id);
                UpstreamStatus {
                    id: repository.id.clone(),
                    circuit: status.state.into(),
                    failures: status.failures,
                }
            })
            .collect()
    }

    pub fn all_candidates_negative(&self, relative_path: &str, negative: &HashSet<String>) -> bool {
        let candidates = self.routes.candidates(relative_path);
        !candidates.is_empty()
            && candidates
                .into_iter()
                .all(|repository| negative.contains(&repository_id(repository)))
    }

    async fn fetch_ordered(
        &self,
        relative_path: &str,
        repositories: Vec<(RepositoryConfig, Option<RequestExtras>)>,
        priority: RequestPriority,
    ) -> FetchOutcome {
        let mut gateway_failure = false;
        let mut not_found = Vec::new();

        for (repository, extras) in repositories {
            if !self.circuits.allow(&repository.id) {
                tracing::debug!(upstream = %repository.id, path = relative_path, "skipping open circuit");
                gateway_failure = true;
                continue;
            }
            let url = match build_url(&repository.url, relative_path) {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!(upstream = %repository.id, %error, "failed to build upstream URL");
                    gateway_failure = true;
                    continue;
                }
            };

            match self
                .fetch_repository(&repository, url, extras.as_ref(), priority)
                .await
            {
                RepositoryResult::Found(response) => {
                    return FetchOutcome {
                        result: FetchResult::Found {
                            repository_id: repository_id(&repository),
                            repository: repository.id,
                            response,
                        },
                        not_found,
                    };
                }
                RepositoryResult::NotModified => {
                    self.circuits.record_success(&repository.id);
                    return FetchOutcome {
                        result: FetchResult::NotModified {
                            repository_id: repository_id(&repository),
                        },
                        not_found,
                    };
                }
                RepositoryResult::NotFound => {
                    tracing::trace!(upstream = %repository.id, path = relative_path, "upstream artifact was not found");
                    self.circuits.record_success(&repository.id);
                    if repository.cache_writes {
                        not_found.push(repository_id(&repository));
                    }
                }
                RepositoryResult::GatewayFailure { breaker_failure } => {
                    gateway_failure = true;
                    if breaker_failure {
                        self.circuits.record_failure(&repository.id);
                    } else {
                        self.circuits.record_success(&repository.id);
                    }
                }
            }
        }

        let result = if gateway_failure {
            FetchResult::GatewayFailure
        } else {
            FetchResult::NotFound
        };
        FetchOutcome { result, not_found }
    }

    async fn fetch_repository(
        &self,
        repository: &RepositoryConfig,
        url: Url,
        extras: Option<&RequestExtras>,
        priority: RequestPriority,
    ) -> RepositoryResult {
        for attempt in 0..MAX_ATTEMPTS {
            let permit = self.scheduler.acquire(&repository.id, priority).await;
            let started = Instant::now();
            let response = self.send_request(repository, &url, extras).await;
            crate::metrics::UPSTREAM_DURATION.observe(started.elapsed());
            match response {
                Ok(response)
                    if response.status() == StatusCode::OK
                        || response.status() == StatusCode::PARTIAL_CONTENT =>
                {
                    return RepositoryResult::Found(UpstreamResponse { response, permit });
                }
                Ok(response) if response.status() == StatusCode::NOT_MODIFIED => {
                    return RepositoryResult::NotModified;
                }
                Ok(response) if response.status() == StatusCode::NOT_FOUND => {
                    return RepositoryResult::NotFound;
                }
                Ok(response)
                    if matches!(
                        response.status(),
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                    ) =>
                {
                    tracing::warn!(
                        upstream = %repository.id,
                        status = %response.status(),
                        "upstream rejected request"
                    );
                    return RepositoryResult::GatewayFailure {
                        breaker_failure: false,
                    };
                }
                Ok(response) if response.status().is_server_error() => {
                    if attempt + 1 < MAX_ATTEMPTS {
                        tracing::debug!(upstream = %repository.id, status = %response.status(), attempt = attempt + 1, "retryable upstream server error");
                    } else {
                        tracing::warn!(upstream = %repository.id, status = %response.status(), attempt = attempt + 1, "upstream server error after retries");
                    }
                }
                Ok(response) => {
                    tracing::warn!(
                        upstream = %repository.id,
                        status = %response.status(),
                        "unexpected upstream response"
                    );
                    return RepositoryResult::GatewayFailure {
                        breaker_failure: false,
                    };
                }
                Err(error) => {
                    let error = describe_error(&error);
                    if attempt + 1 < MAX_ATTEMPTS {
                        tracing::debug!(upstream = %repository.id, attempt = attempt + 1, %error, "retryable upstream request failure");
                    } else {
                        tracing::warn!(upstream = %repository.id, attempt = attempt + 1, %error, "upstream request failed after retries");
                    }
                }
            }

            drop(permit);
            if attempt + 1 < MAX_ATTEMPTS {
                tokio::time::sleep(retry_delay(attempt)).await;
            }
        }
        RepositoryResult::GatewayFailure {
            breaker_failure: true,
        }
    }
}

fn build_http_client(
    config: &UpstreamConfig,
    proxy: Option<&Url>,
    allowed_origins: &[url::Origin],
) -> Result<Client, AppError> {
    let allowed_origins = allowed_origins.to_vec();
    let mut builder = Client::builder()
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many upstream redirects")
            } else if allowed_origins.contains(&attempt.url().origin()) {
                attempt.follow()
            } else {
                attempt.error("upstream redirect target is not configured")
            }
        }))
        .user_agent(concat!("maven-haste/", env!("CARGO_PKG_VERSION")))
        .no_proxy();
    if let Some(proxy) = proxy {
        let proxy = reqwest::Proxy::all(proxy.as_str()).map_err(|error| {
            AppError::Runtime(format!("failed to initialize upstream proxy: {error}"))
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| {
        AppError::Runtime(format!(
            "failed to initialize upstream HTTP client: {error}"
        ))
    })
}

fn repository_id(repository: &RepositoryConfig) -> String {
    to_hex(&Sha256::digest(repository.url.as_str().as_bytes()))
}

enum RepositoryResult {
    Found(UpstreamResponse),
    NotModified,
    NotFound,
    GatewayFailure { breaker_failure: bool },
}

fn build_url(base: &Url, relative_path: &str) -> Result<Url, &'static str> {
    let mut url = base.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| "repository URL cannot contain path segments")?;
    segments.pop_if_empty();
    segments.extend(relative_path.split('/'));
    drop(segments);
    Ok(url)
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_millis(100),
        _ => Duration::from_millis(250),
    }
}

/// Whether an error means the proxy itself could not be reached: refused,
/// unresolved, or timed out before a response arrived.
fn proxy_unreachable(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

/// Renders an error with its source chain. `reqwest::Error` displays only
/// "error sending request for url (...)", which hides the transport cause
/// (refused connection, DNS failure, TLS error) that is actually actionable.
fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, timeout};

    use super::*;

    fn scheduler(global: usize, repository: usize, burst: usize) -> RequestScheduler {
        let repositories = vec![
            RepositoryConfig {
                id: "a".into(),
                url: Url::parse("https://a.example/").unwrap(),
                use_proxy: None,
                max_concurrency: None,
                rules: Vec::new(),
                cache_writes: true,
            },
            RepositoryConfig {
                id: "b".into(),
                url: Url::parse("https://b.example/").unwrap(),
                use_proxy: None,
                max_concurrency: None,
                rules: Vec::new(),
                cache_writes: true,
            },
        ];
        RequestScheduler::new(
            &repositories,
            &UpstreamConfig {
                max_concurrency: global,
                default_repository_max_concurrency: repository,
                foreground_priority_burst: burst,
                ..UpstreamConfig::default()
            },
        )
    }

    async fn wait_for_queues(scheduler: &RequestScheduler, foreground: usize, background: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                let matches = {
                    let state = scheduler.lock_state();
                    state.foreground.len() == foreground && state.background.len() == background
                };
                if matches {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn proxied_repository(use_proxy: Option<bool>) -> RepositoryConfig {
        RepositoryConfig {
            id: "a".into(),
            url: Url::parse("https://a.example/").unwrap(),
            use_proxy,
            max_concurrency: None,
            rules: Vec::new(),
            cache_writes: true,
        }
    }

    fn proxied_client(proxies: &[&str], proxy_fallback: bool) -> UpstreamClient {
        UpstreamClient::new(
            vec![proxied_repository(None)],
            &UpstreamConfig {
                proxy: proxies
                    .iter()
                    .map(|proxy| Url::parse(proxy).unwrap())
                    .collect(),
                proxy_fallback,
                ..UpstreamConfig::default()
            },
            &CircuitBreakerConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn connects_directly_without_a_configured_proxy() {
        let client = proxied_client(&[], true);
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Direct]
        );
    }

    #[test]
    fn offers_a_direct_fallback_when_proxies_are_configured() {
        let client = proxied_client(&["http://127.0.0.1:9/"], true);
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Proxy(0), Egress::Direct]
        );
    }

    #[test]
    fn tries_proxies_in_the_configured_order() {
        let client = proxied_client(&["http://127.0.0.1:8/", "http://127.0.0.1:9/"], true);
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Proxy(0), Egress::Proxy(1), Egress::Direct]
        );
    }

    #[test]
    fn tries_the_next_proxy_while_one_circuit_is_open() {
        let client = proxied_client(&["http://127.0.0.1:8/", "http://127.0.0.1:9/"], true);
        for _ in 0..CircuitBreakerConfig::default().failure_threshold {
            client.proxies[0].circuit.record_failure(PROXY_CIRCUIT);
        }
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Proxy(1), Egress::Direct],
            "an open proxy circuit must be skipped while later proxies are still tried"
        );
    }

    #[test]
    fn goes_direct_when_every_proxy_circuit_is_open() {
        let client = proxied_client(&["http://127.0.0.1:8/", "http://127.0.0.1:9/"], true);
        for proxy in client.proxies.iter() {
            for _ in 0..CircuitBreakerConfig::default().failure_threshold {
                proxy.circuit.record_failure(PROXY_CIRCUIT);
            }
        }
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Direct],
            "open proxy circuits must not be tried again before they recover"
        );
    }

    #[test]
    fn keeps_requiring_the_proxy_when_the_fallback_is_disabled() {
        let client = proxied_client(&["http://127.0.0.1:9/"], false);
        assert_eq!(
            client.egresses(&proxied_repository(None)),
            vec![Egress::Proxy(0)]
        );
    }

    #[test]
    fn keeps_trying_every_proxy_when_the_repository_demands_it() {
        let client = proxied_client(&["http://127.0.0.1:8/", "http://127.0.0.1:9/"], true);
        for _ in 0..CircuitBreakerConfig::default().failure_threshold {
            client.proxies[0].circuit.record_failure(PROXY_CIRCUIT);
        }
        assert_eq!(
            client.egresses(&proxied_repository(Some(true))),
            vec![Egress::Proxy(0), Egress::Proxy(1)],
            "a repository that requires the proxy must try every proxy without a direct fallback"
        );
    }

    #[test]
    fn connects_directly_when_the_repository_disables_the_proxy() {
        let client = proxied_client(&["http://127.0.0.1:9/"], true);
        assert_eq!(
            client.egresses(&proxied_repository(Some(false))),
            vec![Egress::Direct]
        );
    }

    #[test]
    fn skips_the_proxy_while_its_circuit_is_open() {
        let client = proxied_client(&["http://127.0.0.1:9/"], true);
        let repository = proxied_repository(None);
        for _ in 0..CircuitBreakerConfig::default().failure_threshold {
            client.proxies[0].circuit.record_failure(PROXY_CIRCUIT);
        }
        assert_eq!(
            client.egresses(&repository),
            vec![Egress::Direct],
            "an open proxy circuit must not be tried again before it recovers"
        );
    }

    #[tokio::test]
    async fn describes_the_transport_cause_hidden_by_reqwest() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("port 1 has no listener to answer the request");
        let described = describe_error(&error);
        assert!(
            described.starts_with("error sending request"),
            "unexpected rendering: {described}"
        );
        assert!(
            described.to_lowercase().contains("connect"),
            "the transport cause is missing from the message: {described}"
        );
    }

    #[test]
    fn resolves_proxy_choice() {
        assert_eq!(proxy_choice(Some(true), true), ProxyChoice::Proxy);
        assert_eq!(proxy_choice(Some(true), false), ProxyChoice::Proxy);
        assert_eq!(proxy_choice(Some(false), true), ProxyChoice::Direct);
        assert_eq!(proxy_choice(Some(false), false), ProxyChoice::Direct);
        assert_eq!(proxy_choice(None, true), ProxyChoice::Proxy);
        assert_eq!(proxy_choice(None, false), ProxyChoice::Direct);
    }

    #[test]
    fn appends_and_encodes_artifact_path() {
        let base = Url::parse("https://repo.example/maven2/").unwrap();
        let url = build_url(&base, "com/example/a b/1.0/a b-1.0.jar").unwrap();
        assert_eq!(
            url.as_str(),
            "https://repo.example/maven2/com/example/a%20b/1.0/a%20b-1.0.jar"
        );
    }

    #[tokio::test]
    async fn prioritizes_foreground_without_starving_background() {
        let scheduler = scheduler(1, 1, 2);
        let held = scheduler.acquire("a", RequestPriority::Foreground).await;

        let background = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Background).await })
        };
        wait_for_queues(&scheduler, 0, 1).await;
        let first = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Foreground).await })
        };
        wait_for_queues(&scheduler, 1, 1).await;
        let second = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Foreground).await })
        };
        wait_for_queues(&scheduler, 2, 1).await;
        let third = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Foreground).await })
        };
        wait_for_queues(&scheduler, 3, 1).await;

        drop(held);
        let first = timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap();
        assert!(!background.is_finished());
        drop(first);
        let second = timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        assert!(!background.is_finished());
        drop(second);
        let background = timeout(Duration::from_secs(1), background)
            .await
            .unwrap()
            .unwrap();
        assert!(!third.is_finished());
        drop(background);
        drop(
            timeout(Duration::from_secs(1), third)
                .await
                .unwrap()
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn canceled_waiter_does_not_leak_capacity() {
        let scheduler = scheduler(1, 1, 2);
        let held = scheduler.acquire("a", RequestPriority::Foreground).await;
        let waiting = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Foreground).await })
        };
        wait_for_queues(&scheduler, 1, 0).await;
        waiting.abort();
        assert!(waiting.await.is_err());
        drop(held);

        let permit = timeout(
            Duration::from_secs(1),
            scheduler.acquire("a", RequestPriority::Foreground),
        )
        .await
        .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn skips_blocked_repository_without_head_of_line_blocking() {
        let scheduler = scheduler(2, 1, 2);
        let held = scheduler.acquire("a", RequestPriority::Foreground).await;
        let blocked = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("a", RequestPriority::Foreground).await })
        };
        wait_for_queues(&scheduler, 1, 0).await;

        let other = timeout(
            Duration::from_secs(1),
            scheduler.acquire("b", RequestPriority::Foreground),
        )
        .await
        .unwrap();
        assert!(!blocked.is_finished());
        drop(other);
        drop(held);
        drop(blocked.await.unwrap());
    }
}
