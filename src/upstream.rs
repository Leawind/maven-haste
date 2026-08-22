use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{IF_MODIFIED_SINCE, IF_NONE_MATCH};
use reqwest::{Client, StatusCode, Url};

use crate::circuit::CircuitBreaker;
use crate::config::{CircuitBreakerConfig, RepositoryConfig};
use crate::error::AppError;
use crate::routing::RouteEngine;

const MAX_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct UpstreamClient {
    client: Client,
    routes: RouteEngine,
    circuits: Arc<CircuitBreaker>,
}

pub enum FetchResult {
    Found {
        repository: String,
        response: reqwest::Response,
    },
    NotModified,
    NotFound,
    GatewayFailure,
}

#[derive(Clone)]
struct ConditionalHeaders {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl UpstreamClient {
    pub fn new(
        repositories: Vec<RepositoryConfig>,
        timeout: Duration,
        circuit: &CircuitBreakerConfig,
    ) -> Result<Self, AppError> {
        let client = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent(concat!("maven-haste/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                AppError::Runtime(format!(
                    "failed to initialize upstream HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            client,
            routes: RouteEngine::new(repositories),
            circuits: Arc::new(CircuitBreaker::new(
                circuit.failure_threshold,
                circuit.recovery_timeout,
            )),
        })
    }

    pub async fn fetch(&self, relative_path: &str, excluded: &HashSet<String>) -> FetchResult {
        let repositories = self
            .routes
            .candidates(relative_path)
            .into_iter()
            .filter(|repository| !excluded.contains(&repository.name))
            .cloned()
            .map(|repository| (repository, None))
            .collect();
        self.fetch_ordered(relative_path, repositories).await
    }

    pub async fn refresh(
        &self,
        relative_path: &str,
        preferred: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        excluded: &HashSet<String>,
    ) -> FetchResult {
        let mut repositories = Vec::new();
        if !excluded.contains(preferred)
            && let Some(repository) = self.routes.repository(preferred)
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
                .cloned()
                .map(|repository| (repository, None)),
        );
        self.fetch_ordered(relative_path, repositories).await
    }

    pub async fn fetch_from(&self, repository: &str, relative_path: &str) -> FetchResult {
        let Some(repository) = self.routes.repository(repository).cloned() else {
            return FetchResult::GatewayFailure;
        };
        self.fetch_ordered(relative_path, vec![(repository, None)])
            .await
    }

    pub fn record_body_success(&self, repository: &str) {
        self.circuits.record_success(repository);
    }

    pub fn record_body_failure(&self, repository: &str) {
        self.circuits.record_failure(repository);
    }

    async fn fetch_ordered(
        &self,
        relative_path: &str,
        repositories: Vec<(RepositoryConfig, Option<ConditionalHeaders>)>,
    ) -> FetchResult {
        let mut gateway_failure = false;

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
                .fetch_repository(&repository, url, conditional.as_ref())
                .await
            {
                RepositoryResult::Found(response) => {
                    return FetchResult::Found {
                        repository: repository.name,
                        response,
                    };
                }
                RepositoryResult::NotModified => {
                    self.circuits.record_success(&repository.name);
                    return FetchResult::NotModified;
                }
                RepositoryResult::NotFound => self.circuits.record_success(&repository.name),
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

        if gateway_failure {
            FetchResult::GatewayFailure
        } else {
            FetchResult::NotFound
        }
    }

    async fn fetch_repository(
        &self,
        repository: &RepositoryConfig,
        url: Url,
        conditional: Option<&ConditionalHeaders>,
    ) -> RepositoryResult {
        for attempt in 0..MAX_ATTEMPTS {
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
                    return RepositoryResult::Found(response);
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

            if attempt + 1 < MAX_ATTEMPTS {
                tokio::time::sleep(retry_delay(attempt)).await;
            }
        }
        RepositoryResult::GatewayFailure {
            breaker_failure: true,
        }
    }
}

enum RepositoryResult {
    Found(reqwest::Response),
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
    use super::*;

    #[test]
    fn appends_and_encodes_artifact_path() {
        let base = Url::parse("https://repo.example/maven2/").unwrap();
        let url = build_url(&base, "com/example/a b/1.0/a b-1.0.jar").unwrap();
        assert_eq!(
            url.as_str(),
            "https://repo.example/maven2/com/example/a%20b/1.0/a%20b-1.0.jar"
        );
    }
}
