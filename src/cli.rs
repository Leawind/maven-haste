mod command;

use crate::cli::command::Command;
use crate::error::AppError;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

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

impl Cli {
    pub async fn execute(&self) -> Result<(), AppError> {
        self.command.execute(self).await
    }
}
