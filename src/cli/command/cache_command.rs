use std::collections::HashSet;

use crate::cache::CacheManager;
use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use crate::{cache, db, storage, upstream};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Verify that every cached artifact exists with its recorded size and
    /// checksums
    Verify,

    /// Remove all cached artifacts and checksums under a path prefix
    RemovePrefix {
        /// Path prefix such as `com/example` or `com/example/demo`
        #[arg(value_name = "PREFIX")]
        prefix: String,
    },
}

impl CacheCommand {
    pub async fn execute(&self, cli: &Cli) -> Result<(), AppError> {
        let cache = build_cache(cli).await?;
        match self {
            CacheCommand::Verify => {
                let report = cache.verify().await.map_err(app_error)?;
                for issue in &report.issues {
                    println!("{}: {}", issue.path, issue.reason);
                }
                println!(
                    "checked {} artifacts, {} issues",
                    report.checked,
                    report.issues.len()
                );
                if !report.issues.is_empty() {
                    return Err(AppError::Runtime(format!(
                        "cache verification found {} issues",
                        report.issues.len()
                    )));
                }
                Ok(())
            }
            CacheCommand::RemovePrefix { prefix } => {
                let removed = cache.remove_prefix(prefix).await.map_err(app_error)?;
                println!("removed {} files, {} bytes", removed.files, removed.bytes);
                Ok(())
            }
        }
    }
}

/// Assembles the cache manager from a loaded configuration, mirroring the
/// runtime assembly of the `run` command without starting the server.
async fn build_cache(cli: &Cli) -> Result<CacheManager, AppError> {
    let loaded = Config::load(cli)?;
    let storage = storage::prepare(&loaded.config.storage).await?;
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
    Ok(cache::CacheManager::new(
        loaded.config.storage.clone(),
        loaded.config.cache.clone(),
        database,
        upstream,
        storage.case_sensitive,
        caching_disabled,
    ))
}

fn app_error(error: crate::cache::CacheFailure) -> AppError {
    AppError::Runtime(error.to_string())
}
