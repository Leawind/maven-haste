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

use clap::Parser;
use cli::Cli;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
