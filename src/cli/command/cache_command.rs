use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::cache::{CacheFailure, CacheManager};
use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use crate::request_path::MavenPath;
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

    /// Print cache statistics from the database
    Stats {
        /// Print the statistics as JSON
        #[arg(long)]
        json: bool,
    },

    /// Quarantine a corrupt database and create a fresh one
    Repair,

    /// Prefetch artifacts listed in a manifest of Maven repository paths
    Warm {
        /// Text file with one Maven relative path per line; blank lines and
        /// lines starting with `#` are ignored
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
    },
}

impl CacheCommand {
    pub async fn execute(&self, cli: &Cli) -> Result<(), AppError> {
        if matches!(self, CacheCommand::Repair) {
            return repair(cli).await;
        }
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
            CacheCommand::Stats { json } => {
                let stats = cache.stats().await.map_err(app_error)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&stats)?);
                    return Ok(());
                }
                println!("files: {}", stats.files);
                println!("total size: {} bytes", stats.total_size);
                println!("object files: {}", stats.object_files);
                println!("object bytes: {}", stats.object_bytes);
                println!("recorded requests: {}", stats.recorded_requests);
                println!("negative entries: {}", stats.negative_entries);
                match stats.max_size {
                    Some(max_size) => println!("max size: {max_size} bytes"),
                    None => println!("max size: unlimited"),
                }
                Ok(())
            }
            CacheCommand::Warm { manifest } => warm(&cache, manifest).await,
            CacheCommand::Repair => unreachable!("repair is handled before building the cache"),
        }
    }
}

async fn repair(cli: &Cli) -> Result<(), AppError> {
    let loaded = Config::load(cli)?;
    storage::prepare(&loaded.config.storage).await?;
    let path = loaded.config.storage.db_path();
    match crate::db::Database::open(path).await {
        Ok(_) => {
            println!("database is healthy: {}", path.display());
            Ok(())
        }
        Err(error) if crate::db::is_corruption(&error) => {
            for quarantined in quarantine_database(path)? {
                println!("quarantined {}", quarantined.display());
            }
            crate::db::Database::open(path).await?;
            println!("created a fresh database: {}", path.display());
            println!("existing cache files will be re-fetched as needed");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn quarantine_database(path: &Path) -> Result<Vec<PathBuf>, AppError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut moved = Vec::new();
    for (suffix, label) in [
        ("", "corrupt"),
        ("-wal", "corrupt-wal"),
        ("-shm", "corrupt-shm"),
    ] {
        let source = append_suffix(path, suffix);
        if !source.exists() {
            continue;
        }
        let target = append_suffix(path, &format!(".{label}-{stamp}"));
        std::fs::rename(&source, &target).map_err(|error| {
            AppError::Runtime(format!(
                "failed to quarantine {}: {error}",
                source.display()
            ))
        })?;
        moved.push(target);
    }
    Ok(moved)
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

async fn warm(cache: &CacheManager, manifest: &Path) -> Result<(), AppError> {
    let source = std::fs::read_to_string(manifest).map_err(|error| {
        AppError::Runtime(format!(
            "failed to read warm manifest {}: {error}",
            manifest.display()
        ))
    })?;
    let mut warmed = 0_usize;
    let mut failures = Vec::new();
    for entry in manifest_entries(&source) {
        let request = match MavenPath::parse(&format!("/{entry}"), "/") {
            Ok(request) => request,
            Err(error) => {
                failures.push(format!("{entry}: {error}"));
                continue;
            }
        };
        match cache.get(&request).await {
            Ok(_) => warmed += 1,
            Err(CacheFailure::NotFound) => failures.push(format!("{entry}: not found")),
            Err(error) => failures.push(format!("{entry}: {error}")),
        }
    }
    for failure in &failures {
        eprintln!("{failure}");
    }
    println!("warmed {warmed} artifacts, {} failures", failures.len());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Runtime(format!(
            "failed to warm {} artifacts",
            failures.len()
        )))
    }
}

fn manifest_entries(source: &str) -> impl Iterator<Item = &str> {
    source.lines().filter_map(|line| {
        let line = line.trim();
        (!line.is_empty() && !line.starts_with('#')).then(|| line.trim_start_matches('/'))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_entries_skip_comments_and_blank_lines() {
        let entries = manifest_entries(
            "# comment\n\ncom/example/demo/1.0/demo-1.0.jar\n  /com/example/demo/1.0/demo-1.0.pom  \n",
        )
        .collect::<Vec<_>>();

        assert_eq!(
            entries,
            vec![
                "com/example/demo/1.0/demo-1.0.jar",
                "com/example/demo/1.0/demo-1.0.pom"
            ]
        );
    }
}
