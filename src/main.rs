mod cache;
mod circuit;
mod cli;
mod config;
mod db;
mod error;
mod logging;
mod request_path;
mod routing;
mod server;
mod storage;
mod upstream;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use cli::{CacheCommand, Cli, Command, ConfigCommand};
use error::AppError;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

async fn run(cli: Cli) -> Result<(), AppError> {
    if let Some(Command::Config {
        command: ConfigCommand::Init { path },
    }) = &cli.command
    {
        let path = path.as_deref().or(cli.config.as_deref());
        let path = initialize_config(path)?;
        println!(
            "created commented example configuration: {}",
            path.display()
        );
        println!("next: maven-haste check --config {}", path.display());
        println!("then: maven-haste run --config {}", path.display());
        return Ok(());
    }
    if matches!(
        cli.command.as_ref(),
        Some(Command::Config {
            command: ConfigCommand::Example
        })
    ) {
        print!("{}", config::EXAMPLE_CONFIG);
        return Ok(());
    }

    let loaded = config::load(&cli)?;

    if matches!(
        cli.command.as_ref(),
        Some(Command::Config {
            command: ConfigCommand::Show
        })
    ) {
        print!("{}", toml::to_string_pretty(&loaded.config)?);
        return Ok(());
    }

    let storage = storage::prepare(&loaded.config.storage).await?;

    if matches!(cli.command.as_ref(), Some(Command::Check)) {
        if loaded.config.logging.enabled {
            logging::validate_directory(&loaded.config.logging)?;
        }
        println!("configuration is valid: {}", loaded.path.display());
        println!(
            "start the proxy: maven-haste run --config {}",
            loaded.path.display()
        );
        return Ok(());
    }

    if let Some(Command::Cache { command }) = &cli.command {
        let database = db::Database::open(loaded.config.storage.db_path()).await?;
        let cache = cache::CacheManager::new(&loaded.config, database, storage.case_sensitive)?;
        match command {
            CacheCommand::Stats => {
                let stats = cache.stats().await.map_err(cache_error)?;
                println!("files: {}", stats.files);
                println!("size: {} bytes", stats.total_size);
                println!("negative entries: {}", stats.negative_entries);
                if let Some(max_size) = stats.max_size {
                    println!("limit: {max_size} bytes");
                } else {
                    println!("limit: none");
                }
            }
            CacheCommand::Remove { prefix } => {
                let removed = cache.remove_prefix(prefix).await.map_err(cache_error)?;
                println!(
                    "removed {} cached files ({} bytes) under {}",
                    removed.files, removed.bytes, prefix
                );
            }
            CacheCommand::Verify => {
                let report = cache.verify().await.map_err(cache_error)?;
                println!("checked {} cached files", report.checked);
                for issue in &report.issues {
                    println!("{}: {}", issue.path, issue.reason);
                }
                if !report.issues.is_empty() {
                    return Err(AppError::Runtime(format!(
                        "cache verification found {} issues",
                        report.issues.len()
                    )));
                }
            }
        }
        return Ok(());
    }

    let _logging_guard = logging::init(cli.verbose, &loaded.config.logging)?;
    tracing::info!(config = %loaded.path.display(), "loaded configuration");
    tracing::info!(
        root = %loaded.config.storage.root.display(),
        case_sensitive = storage.case_sensitive,
        "storage initialized"
    );
    let database = db::Database::open(loaded.config.storage.db_path()).await?;
    let cache = cache::CacheManager::new(&loaded.config, database, storage.case_sensitive)?;
    let listener = server::bind(loaded.config.server.bind).await?;
    let bind = listener
        .local_addr()
        .map_err(|error| AppError::Runtime(format!("failed to inspect HTTP listener: {error}")))?;
    tracing::info!(%bind, "Maven proxy is ready");
    server::serve(listener, loaded.config.server.base_path, cache).await
}

fn cache_error(error: cache::CacheFailure) -> AppError {
    AppError::Runtime(error.to_string())
}

fn initialize_config(destination: Option<&Path>) -> Result<PathBuf, AppError> {
    let default_path = config::default_config_path()?;
    let path = match destination {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => default_path
            .parent()
            .expect("current directory configuration path has a parent")
            .join(path),
        None => default_path,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AppError::Runtime(format!(
                "failed to create configuration directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            AppError::Runtime(format!(
                "refusing to create configuration {}: {error}",
                path.display()
            ))
        })?;
    file.write_all(config::EXAMPLE_CONFIG.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            AppError::Runtime(format!(
                "failed to write configuration {}: {error}",
                path.display()
            ))
        })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn check_mode_validates_real_storage_access_without_creating_database() {
        let directory = TempDir::new().unwrap();
        let config_path = directory.path().join("maven-haste.toml");
        fs::write(
            &config_path,
            r#"
[storage]
root = "repository"

[[repositories]]
name = "central"
url = "https://repo.example/"
"#,
        )
        .unwrap();
        let cli = Cli::try_parse_from([
            "maven-haste",
            "check",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .unwrap();

        run(cli).await.unwrap();

        let internal = directory.path().join("repository/.maven-haste");
        assert!(internal.join("tmp").is_dir());
        assert!(!internal.join("cache.db").exists());
    }
}
