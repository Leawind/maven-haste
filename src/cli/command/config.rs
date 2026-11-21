use crate::cli::Cli;
use crate::config::{Config, ConfigFormat};
use crate::error::{AppError, ConfigError};
use clap::Subcommand;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a minimal example configuration without overwriting an existing file
    Init {
        /// Destination path; defaults to ./maven-haste.yaml
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,

        /// Format of the generated file; defaults to YAML, or the extension of PATH when given
        #[arg(short, long, value_enum)]
        format: Option<ConfigFormat>,
    },

    /// Validate the configuration and storage access, then exit
    Check,

    /// Print the fully resolved effective configuration
    Show,

    /// Print the minimal example configuration in the requested format
    Example {
        /// Format of the printed example
        #[arg(short, long, value_enum, default_value = "yaml")]
        format: ConfigFormat,
    },

    /// Write the JSON schema describing the configuration
    Schema {
        /// Destination path; defaults to standard output
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

impl ConfigCommand {
    pub async fn execute(&self, cli: &Cli) -> Result<(), AppError> {
        match self {
            ConfigCommand::Init { path, format } => {
                let destination = path.as_deref().or(cli.config.as_deref());
                let path = initialize_config(destination, *format)?;
                println!("created minimal example configuration: {}", path.display());
                println!("next: maven-haste config check --config {}", path.display());
                println!("then: maven-haste run --config {}", path.display());
                println!(
                    "then (optional): maven-haste config schema -o ./maven-haste.schema.json  # IDE and AI tool hints"
                );
                Ok(())
            }
            ConfigCommand::Check => {
                let loaded = Config::load(cli)?;
                for warning in &loaded.warnings {
                    println!("warning: {warning}");
                }
                let storage = crate::storage::prepare(&loaded.config.storage).await?;
                if loaded.config.logging.enabled {
                    crate::logging::validate_directory(&loaded.config.logging)?;
                }
                println!("configuration is valid: {}", loaded.path.display());
                println!(
                    "storage is ready: {} ({})",
                    loaded.config.storage.root.display(),
                    if storage.case_sensitive {
                        "case-sensitive"
                    } else {
                        "case-insensitive"
                    }
                );
                println!(
                    "start the proxy: maven-haste run --config {}",
                    loaded.path.display()
                );
                Ok(())
            }
            ConfigCommand::Show => {
                let loaded = Config::load(cli)?;
                for warning in &loaded.warnings {
                    eprintln!("warning: {warning}");
                }

                print!("{}", toml::to_string_pretty(&loaded.config.redacted())?);
                Ok(())
            }
            ConfigCommand::Example { format } => {
                print!("{}", crate::config::example_config(*format));
                Ok(())
            }
            ConfigCommand::Schema { output } => {
                let schema = serde_json::to_string_pretty(&crate::config::generated_schema())?;
                match output {
                    Some(path) => {
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(path)
                            .map_err(|error| {
                                AppError::Runtime(format!(
                                    "failed to write configuration schema: {error}"
                                ))
                            })?;
                        file.write_all(schema.as_bytes())
                            .and_then(|()| file.sync_all())
                            .map_err(|error| {
                                AppError::Runtime(format!(
                                    "failed to write configuration schema: {error}"
                                ))
                            })?;
                    }
                    None => print!("{schema}"),
                }
                Ok(())
            }
        }
    }
}

/// Resolves the destination and format for `config init`. A path decides its
/// format by extension; `--format` alone picks the default file name for that
/// format; both together must agree.
fn initialize_config(
    destination: Option<&Path>,
    requested: Option<ConfigFormat>,
) -> Result<PathBuf, AppError> {
    let default_path = crate::config::default_config_path()?;
    let (path, format) = match (destination, requested) {
        (Some(path), Some(requested)) => {
            let path = resolve_destination(&default_path, path);
            let actual = crate::config::format_for_path(&path)?;
            if actual != requested {
                return Err(AppError::Config(ConfigError::new(format!(
                    "--format {} does not match the extension of {}, which is {}",
                    requested.name(),
                    path.display(),
                    actual.name()
                ))));
            }
            (path, actual)
        }
        (Some(path), None) => {
            let path = resolve_destination(&default_path, path);
            let format = crate::config::format_for_path(&path)?;
            (path, format)
        }
        (None, Some(requested)) => {
            let path = default_path
                .parent()
                .expect("current directory configuration path has a parent")
                .join(requested.default_file_name());
            (path, requested)
        }
        (None, None) => (default_path, ConfigFormat::Yaml),
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
    file.write_all(crate::config::example_config(format).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            AppError::Runtime(format!(
                "failed to write configuration {}: {error}",
                path.display()
            ))
        })?;
    Ok(path)
}

fn resolve_destination(default_path: &Path, destination: &Path) -> PathBuf {
    if destination.is_absolute() {
        destination.to_owned()
    } else {
        default_path
            .parent()
            .expect("current directory configuration path has a parent")
            .join(destination)
    }
}
