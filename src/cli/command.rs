use std::collections::HashSet;
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
                tracing::info!(config = %loaded.path.display(), "loaded configuration");
                tracing::info!(
                    root = %loaded.config.storage.root.display(),
                    case_sensitive = storage.case_sensitive,
                    "storage initialized"
                );
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
                tracing::info!(%bind, "Maven proxy is ready");
                let reload = reload::spawn(
                    loaded.path.clone(),
                    cli.bind,
                    cache.clone(),
                    loaded.config.clone(),
                    observed,
                );
                let maintenance = tokio::spawn(run_maintenance(cache.clone()));
                let result =
                    server::serve(listener, loaded.config.server.base_path, cache.clone()).await;
                maintenance.abort();
                reload.abort();
                result
            }
            Command::Cache { command } => command.execute(cli).await,
            Command::Config { command } => command.execute(cli).await,
        }
    }
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
