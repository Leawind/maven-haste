use crate::cli::Cli;
use crate::config::Config;
use crate::error::AppError;
use clap::Subcommand;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a commented example configuration without overwriting an existing file
    Init {
        /// Destination path; defaults to ./maven-haste.toml
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },

    /// Validate the configuration and storage access, then exit
    Check,

    /// Print the fully resolved effective configuration
    Show,

    /// Print the commented example configuration
    Example,
}

impl ConfigCommand {
    pub async fn execute(&self, cli: &Cli) -> Result<(), AppError> {
        match self {
            ConfigCommand::Init { path } => {
                let path = path.as_deref().or(cli.config.as_deref());
                let path = initialize_config(path)?;
                println!(
                    "created commented example configuration: {}",
                    path.display()
                );
                println!("next: maven-haste config check --config {}", path.display());
                println!("then: maven-haste run --config {}", path.display());
                Ok(())
            }
            ConfigCommand::Check => {
                let loaded = Config::load(cli)?;
                if loaded.config.logging.enabled {
                    crate::logging::validate_directory(&loaded.config.logging)?;
                }
                println!("configuration is valid: {}", loaded.path.display());
                println!(
                    "start the proxy: maven-haste run --config {}",
                    loaded.path.display()
                );
                Ok(())
            }
            ConfigCommand::Show => {
                let loaded = Config::load(cli)?;

                print!("{}", toml::to_string_pretty(&loaded.config)?);
                Ok(())
            }
            ConfigCommand::Example => {
                print!("{}", crate::config::EXAMPLE_CONFIG);
                Ok(())
            }
        }
    }
}

fn initialize_config(destination: Option<&Path>) -> Result<PathBuf, AppError> {
    let default_path = crate::config::default_config_path()?;
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
    file.write_all(crate::config::EXAMPLE_CONFIG.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            AppError::Runtime(format!(
                "failed to write configuration {}: {error}",
                path.display()
            ))
        })?;
    Ok(path)
}
