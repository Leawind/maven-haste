use std::collections::HashSet;

use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use crate::{cache, db, logging, server, storage, upstream};
use clap::Subcommand;
use config::ConfigCommand;

mod config;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the server
    Run,

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
                let loaded = Config::load(cli)?;
                let storage = storage::prepare(&loaded.config.storage).await?;

                let _logging_guard = logging::init(cli.verbose, &loaded.config.logging)?;
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
                server::serve(listener, loaded.config.server.base_path, cache).await
            }
            Command::Config { command } => command.execute(cli).await,
        }
    }
}
