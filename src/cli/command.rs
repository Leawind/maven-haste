use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use crate::{cache, db, logging, server, storage};
use clap::Subcommand;
use config::ConfigCommand;

mod config;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the server
    Run,

    /// Validate configuration and storage access, then exit
    Check,

    /// Create, print, or inspect configuration
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
                let cache =
                    cache::CacheManager::new(&loaded.config, database, storage.case_sensitive)?;
                let listener = server::bind(loaded.config.server.bind).await?;
                let bind = listener.local_addr().map_err(|error| {
                    AppError::Runtime(format!("failed to inspect HTTP listener: {error}"))
                })?;
                tracing::info!(%bind, "Maven proxy is ready");
                server::serve(listener, loaded.config.server.base_path, cache).await
            }
            Command::Check => {
                let loaded = Config::load(cli)?;
                if loaded.config.logging.enabled {
                    logging::validate_directory(&loaded.config.logging)?;
                }
                println!("configuration is valid: {}", loaded.path.display());
                println!(
                    "start the proxy: maven-haste run --config {}",
                    loaded.path.display()
                );
                Ok(())
            }
            Command::Config { command } => command.execute(cli).await,
        }
    }
}
