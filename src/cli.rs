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
    pub command: Command,
}

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
        assert_eq!(cli.config, Some(PathBuf::from("config.toml")));
        assert!(!cli.verbose);
    }

    #[test]
    fn parses_configuration_subcommands() {
        let cli = Cli::try_parse_from(["maven-haste", "config", "init", "custom.toml"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                command: ConfigCommand::Init { path: Some(path) }
            } if path == Path::new("custom.toml")
        ));
    }

    #[test]
    fn parses_verbose_flag() {
        let cli = Cli::try_parse_from(["maven-haste", "--verbose", "run"]).unwrap();
        assert!(cli.verbose);
        assert!(matches!(cli.command, Command::Run));
    }
}
