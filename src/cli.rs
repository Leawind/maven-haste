use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A local Maven repository proxy cache
#[derive(Debug, Parser)]
#[command(author, version)]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Override the configured listen address
    #[arg(long, global = true, value_name = "ADDR")]
    pub bind: Option<SocketAddr>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the server
    Run,

    /// Validate configuration and storage access, then exit
    Check,

    /// Inspect or maintain cached artifacts
    #[deprecated]
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },

    /// Create, print, or inspect configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[deprecated]
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Print persistent cache usage
    Stats,

    /// Remove one cached path and all of its descendants
    Remove {
        /// Maven repository path prefix, such as com/example/library
        #[arg(value_name = "PREFIX")]
        prefix: String,
    },

    /// Verify tracked file sizes and checksums
    Verify,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a commented example configuration without overwriting an existing file
    Init {
        /// Destination path; defaults to ./maven-haste.toml
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },

    /// Print the fully resolved effective configuration
    Show,

    /// Print the commented example configuration
    Example,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn defaults_to_run_without_a_subcommand() {
        let cli = Cli::try_parse_from(["maven-haste", "--config", "config.toml"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("config.toml")));
        assert!(!cli.verbose);
    }

    #[test]
    fn parses_configuration_subcommands() {
        let cli = Cli::try_parse_from(["maven-haste", "config", "init", "custom.toml"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                command: ConfigCommand::Init { path: Some(path) }
            }) if path == Path::new("custom.toml")
        ));
    }

    #[test]
    fn parses_verbose_flag() {
        let cli = Cli::try_parse_from(["maven-haste", "--verbose", "run"]).unwrap();
        assert!(cli.verbose);
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn parses_cache_maintenance_commands() {
        let cli = Cli::try_parse_from(["maven-haste", "cache", "remove", "com/example"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Cache {
                command: CacheCommand::Remove { prefix }
            }) if prefix == "com/example"
        ));
    }
}
