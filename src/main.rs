mod cli;
mod config;
mod db;
mod error;
mod storage;

use std::process::ExitCode;

use clap::Parser;
use cli::Cli;
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
    let loaded = config::load(&cli)?;

    if cli.print_config {
        print!("{}", toml::to_string_pretty(&loaded.config)?);
        return Ok(());
    }

    let storage = storage::prepare(&loaded.config.storage).await?;

    if cli.check {
        println!("configuration is valid: {}", loaded.path.display());
        return Ok(());
    }

    init_tracing()?;
    tracing::info!(config = %loaded.path.display(), "loaded configuration");
    tracing::info!(
        root = %loaded.config.storage.root.display(),
        case_sensitive = storage.case_sensitive,
        "storage initialized"
    );

    let _database = db::Database::open(loaded.config.storage.db_path()).await?;
    tracing::info!("initialization complete");
    Ok(())
}

fn init_tracing() -> Result<(), AppError> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("maven_haste=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| AppError::Runtime(error.to_string()))
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
            "--check",
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
