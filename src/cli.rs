use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{ArgGroup, Parser};

#[derive(Debug, Parser)]
#[command(author, version, about)]
#[command(group(
    ArgGroup::new("diagnostic")
        .args(["check", "print_config"])
        .multiple(false)
))]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Override the configured listen address
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<SocketAddr>,

    /// Validate configuration and storage access, then exit
    #[arg(long)]
    pub check: bool,

    /// Print the fully resolved effective configuration, then exit
    #[arg(long)]
    pub print_config: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_both_diagnostic_modes() {
        let result = Cli::try_parse_from(["maven-haste", "--check", "--print-config"]);
        assert!(result.is_err());
    }
}
