use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use crate::{cache, db, logging, reload, server, storage, upstream};
use clap::Subcommand;
use config::ConfigCommand;

mod cache_command;
mod config;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the server; the configuration file is hot-reloaded when it changes
    Run,

    /// Verify and maintain the local artifact cache
    Cache {
        #[command(subcommand)]
        command: cache_command::CacheCommand,
    },

    /// Create, print, validate, or inspect configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

impl Command {
    pub async fn execute(&self, cli: &Cli) -> Result<(), AppError> {
        match self {
            Command::Run => {
                let path = Config::resolve_path(cli)?;
                let observed = reload::signature(&path).await;
                let loaded = Config::load_path(&path, cli.bind)?;
                let storage = storage::prepare(&loaded.config.storage).await?;

                let _logging_guard = logging::init(cli.verbose, &loaded.config.logging)?;
                for warning in &loaded.warnings {
                    tracing::warn!("{warning}");
                }
                tracing::info!(
                    config = %loaded.path.display(),
                    version = env!("CARGO_PKG_VERSION"),
                    root = %loaded.config.storage.root.display(),
                    repositories = loaded.config.repositories.len(),
                    offline = loaded.config.upstream.offline,
                    "loaded configuration"
                );
                tracing::debug!(case_sensitive = storage.case_sensitive, "storage inspected");
                let database = db::Database::open(loaded.config.storage.db_path()).await?;
                let upstream = upstream::UpstreamClient::new(
                    loaded.config.repositories.clone(),
                    &loaded.config.upstream,
                    &loaded.config.circuit_breaker,
                )?;
                let caching_disabled = loaded
                    .config
                    .repositories
                    .iter()
                    .filter(|repository| !repository.cache_writes)
                    .map(|repository| repository.id.clone())
                    .collect::<HashSet<_>>();
                let cache = cache::CacheManager::new(
                    loaded.config.storage.clone(),
                    loaded.config.cache.clone(),
                    database,
                    upstream,
                    storage.case_sensitive,
                    caching_disabled,
                );
                let listener = server::bind(loaded.config.server.bind).await?;
                let bind = listener.local_addr().map_err(|error| {
                    AppError::Runtime(format!("failed to inspect HTTP listener: {error}"))
                })?;
                tracing::info!(
                    endpoint = %endpoint_url(bind, &loaded.config.server.base_path),
                    "Maven proxy is ready"
                );
                let reload_tracker = reload::ReloadTracker::default();
                let reload = reload::spawn(
                    loaded.path.clone(),
                    cli.bind,
                    cache.clone(),
                    loaded.config.clone(),
                    observed,
                    reload_tracker.clone(),
                );
                let maintenance = tokio::spawn(run_maintenance(cache.clone()));
                let result = server::serve(
                    listener,
                    loaded.config.server.base_path,
                    cache.clone(),
                    reload_tracker,
                )
                .await;
                maintenance.abort();
                reload.abort();
                result
            }
            Command::Cache { command } => command.execute(cli).await,
            Command::Config { command } => command.execute(cli).await,
        }
    }
}

/// Client-facing URL of the Maven endpoint. An unspecified bind address is
/// replaced with the matching loopback address, so the printed URL can be
/// pasted into build-tool settings as is.
fn endpoint_url(bind: SocketAddr, base_path: &str) -> String {
    let address = if bind.ip().is_unspecified() {
        match bind {
            SocketAddr::V4(address) => {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())
            }
            SocketAddr::V6(address) => {
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port())
            }
        }
    } else {
        bind
    };
    format!("http://{address}{base_path}")
}

/// Runs maintenance on the interval from the current configuration. A short
/// poll lets a reload change the interval without waiting for the previous
/// (possibly much longer) one to elapse.
async fn run_maintenance(cache: cache::CacheManager) {
    let mut last_run: Option<Instant> = None;
    loop {
        let interval = cache.maintenance_interval();
        let wait = match last_run {
            Some(last) => interval.saturating_sub(last.elapsed()),
            None => Duration::ZERO,
        };
        if wait.is_zero() {
            if let Err(error) = cache.run_maintenance().await {
                tracing::warn!(%error, "cache maintenance pass failed");
            }
            last_run = Some(Instant::now());
        } else {
            tokio::time::sleep(wait.min(Duration::from_secs(1))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_a_usable_endpoint_for_unspecified_bind_addresses() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
        assert_eq!(endpoint_url(bind, "/maven"), "http://127.0.0.1:8080/maven");

        let bind = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 8080);
        assert_eq!(endpoint_url(bind, "/maven"), "http://[::1]:8080/maven");
    }

    #[test]
    fn keeps_an_explicit_bind_address_in_the_endpoint() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 8080);
        assert_eq!(
            endpoint_url(bind, "/maven"),
            "http://192.168.1.10:8080/maven"
        );
    }
}
