use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{IF_MODIFIED_SINCE, IF_NONE_MATCH};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::circuit::CircuitBreaker;
use crate::config::{CircuitBreakerConfig, RepositoryConfig, UpstreamConfig};
use crate::error::AppError;
use crate::routing::RouteEngine;

const MAX_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct UpstreamClient {
    client: Client,
    routes: RouteEngine,
    circuits: Arc<CircuitBreaker>,
    scheduler: RequestScheduler,
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
    pub name: String,
    pub circuit: String,
    pub failures: u32,
}

#[derive(Clone)]
struct ConditionalHeaders {
    etag: Option<String>,
    last_modified: Option<String>,
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
                    repository.name.clone(),
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
        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent(concat!("maven-haste/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                AppError::Runtime(format!(
                    "failed to initialize upstream HTTP client: {error}"
                ))
            })?;
        let scheduler = RequestScheduler::new(&repositories, config);
        Ok(Self {
            client,
            routes: RouteEngine::new(repositories),
            circuits: Arc::new(CircuitBreaker::new(
                circuit.failure_threshold,
                circuit.recovery_timeout,
            )),
            scheduler,
        })
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
            .filter(|repository| !excluded.contains(&repository.name))
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
                Some(ConditionalHeaders {
                    etag: etag.map(str::to_owned),
                    last_modified: last_modified.map(str::to_owned),
                }),
            ));
        }
        repositories.extend(
            self.routes
                .candidates(relative_path)
                .into_iter()
                .filter(|repository| {
                    repository.name != preferred && !excluded.contains(&repository.name)
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
                let status = self.circuits.status(&repository.name);
                UpstreamStatus {
                    name: repository.name.clone(),
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
        repositories: Vec<(RepositoryConfig, Option<ConditionalHeaders>)>,
        priority: RequestPriority,
    ) -> FetchOutcome {
        let mut gateway_failure = false;
        let mut not_found = Vec::new();

        for (repository, conditional) in repositories {
            if !self.circuits.allow(&repository.name) {
                tracing::debug!(upstream = %repository.name, "skipping open circuit");
                gateway_failure = true;
                continue;
            }
            let url = match build_url(&repository.url, relative_path) {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!(upstream = %repository.name, %error, "failed to build upstream URL");
                    gateway_failure = true;
                    continue;
                }
            };

            match self
                .fetch_repository(&repository, url, conditional.as_ref(), priority)
                .await
            {
                RepositoryResult::Found(response) => {
                    return FetchOutcome {
                        result: FetchResult::Found {
                            repository_id: repository_id(&repository),
                            repository: repository.name,
                            response,
                        },
                        not_found,
                    };
                }
                RepositoryResult::NotModified => {
                    self.circuits.record_success(&repository.name);
                    return FetchOutcome {
                        result: FetchResult::NotModified {
                            repository_id: repository_id(&repository),
                        },
                        not_found,
                    };
                }
                RepositoryResult::NotFound => {
                    self.circuits.record_success(&repository.name);
                    not_found.push(repository_id(&repository));
                }
                RepositoryResult::GatewayFailure { breaker_failure } => {
                    gateway_failure = true;
                    if breaker_failure {
                        self.circuits.record_failure(&repository.name);
                    } else {
                        self.circuits.record_success(&repository.name);
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
        conditional: Option<&ConditionalHeaders>,
        priority: RequestPriority,
    ) -> RepositoryResult {
        for attempt in 0..MAX_ATTEMPTS {
            let permit = self.scheduler.acquire(&repository.name, priority).await;
            let mut request = self.client.get(url.clone());
            if let Some(conditional) = conditional {
                if let Some(etag) = &conditional.etag {
                    request = request.header(IF_NONE_MATCH, etag);
                }
                if let Some(last_modified) = &conditional.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, last_modified);
                }
            }

            match request.send().await {
                Ok(response) if response.status() == StatusCode::OK => {
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
                        upstream = %repository.name,
                        status = %response.status(),
                        "upstream rejected request"
                    );
                    return RepositoryResult::GatewayFailure {
                        breaker_failure: false,
                    };
                }
                Ok(response) if response.status().is_server_error() => {
                    tracing::warn!(
                        upstream = %repository.name,
                        status = %response.status(),
                        attempt = attempt + 1,
                        "upstream server error"
                    );
                }
                Ok(response) => {
                    tracing::warn!(
                        upstream = %repository.name,
                        status = %response.status(),
                        "unexpected upstream response"
                    );
                    return RepositoryResult::GatewayFailure {
                        breaker_failure: false,
                    };
                }
                Err(error) => {
                    tracing::warn!(
                        upstream = %repository.name,
                        attempt = attempt + 1,
                        %error,
                        "upstream request failed"
                    );
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

fn repository_id(repository: &RepositoryConfig) -> String {
    format!("{:x}", Sha256::digest(repository.url.as_str().as_bytes()))
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

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, timeout};

    use super::*;

    fn scheduler(global: usize, repository: usize, burst: usize) -> RequestScheduler {
        let repositories = vec![
            RepositoryConfig {
                name: "a".into(),
                url: Url::parse("https://a.example/").unwrap(),
                max_concurrency: None,
                rules: Vec::new(),
            },
            RepositoryConfig {
                name: "b".into(),
                url: Url::parse("https://b.example/").unwrap(),
                max_concurrency: None,
                rules: Vec::new(),
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
