use std::fs::{self, OpenOptions};
use std::path::Path;
use std::time::Duration;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::FileLoggingConfig;
use crate::error::AppError;

pub struct LoggingGuard {
    _file: Option<WorkerGuard>,
}

pub fn validate_directory(config: &FileLoggingConfig) -> Result<(), AppError> {
    fs::create_dir_all(&config.directory).map_err(|error| directory_error(config, error))?;
    let probe = config
        .directory
        .join(format!(".maven-haste-write-test-{}", std::process::id()));
    let result = OpenOptions::new().write(true).create_new(true).open(&probe);
    match result {
        Ok(file) => {
            drop(file);
            fs::remove_file(&probe).map_err(|error| directory_error(config, error))?;
            Ok(())
        }
        Err(error) => Err(directory_error(config, error)),
    }
}

pub fn init(verbose: bool, file: Option<&FileLoggingConfig>) -> Result<LoggingGuard, AppError> {
    let console_filter = match std::env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value)
            .map_err(|error| AppError::Runtime(format!("invalid RUST_LOG: {error}")))?,
        Err(std::env::VarError::NotPresent) => EnvFilter::new(if verbose {
            "maven_haste=debug"
        } else {
            "maven_haste=info"
        }),
        Err(error) => {
            return Err(AppError::Runtime(format!(
                "failed to read RUST_LOG: {error}"
            )));
        }
    };
    let console = tracing_subscriber::fmt::layer()
        .compact()
        .with_filter(console_filter);

    if let Some(config) = file {
        validate_directory(config)?;
        cleanup(&config.directory, config.retention)?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("maven-haste")
            .filename_suffix("jsonl")
            .build(&config.directory)
            .map_err(|error| AppError::Runtime(format!("failed to open file log: {error}")))?;
        let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .finish(appender);
        let file_filter = EnvFilter::try_new(&config.filter)
            .map_err(|error| AppError::Runtime(format!("invalid logging.file.filter: {error}")))?;
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(file_filter);
        tracing_subscriber::registry()
            .with(console)
            .with(file_layer)
            .try_init()
            .map_err(|error| AppError::Runtime(error.to_string()))?;
        spawn_cleanup(config.clone());
        Ok(LoggingGuard { _file: Some(guard) })
    } else {
        tracing_subscriber::registry()
            .with(console)
            .try_init()
            .map_err(|error| AppError::Runtime(error.to_string()))?;
        Ok(LoggingGuard { _file: None })
    }
}

fn spawn_cleanup(config: FileLoggingConfig) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = cleanup(&config.directory, config.retention) {
                tracing::warn!(%error, "failed to clean old log files");
            }
        }
    });
}

fn cleanup(directory: &Path, retention: Duration) -> Result<(), AppError> {
    let today = time::OffsetDateTime::now_utc().date();
    let retention_days = i64::try_from(retention.as_secs() / (24 * 60 * 60)).unwrap_or(i64::MAX);
    for entry in fs::read_dir(directory).map_err(|error| AppError::Runtime(error.to_string()))? {
        let entry = entry.map_err(|error| AppError::Runtime(error.to_string()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_log_name(&name)
            || !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        {
            continue;
        }
        if log_date(&name).is_some_and(|date| (today - date).whole_days() > retention_days) {
            fs::remove_file(entry.path()).map_err(|error| AppError::Runtime(error.to_string()))?;
        }
    }
    Ok(())
}

fn is_log_name(name: &str) -> bool {
    log_date(name).is_some()
}

fn log_date(name: &str) -> Option<time::Date> {
    let date = name
        .strip_prefix("maven-haste.")
        .and_then(|name| name.strip_suffix(".jsonl"))?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = time::Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    time::Date::from_calendar_date(year, month, day).ok()
}

fn directory_error(config: &FileLoggingConfig, error: std::io::Error) -> AppError {
    AppError::Runtime(format!(
        "file log directory {} is not writable: {error}",
        config.directory.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn recognizes_only_valid_maven_haste_daily_log_names() {
        assert!(is_log_name("maven-haste.2026-08-23.jsonl"));
        assert!(!is_log_name("maven-haste.2026-02-30.jsonl"));
        assert!(!is_log_name("other.2026-08-23.jsonl"));
        assert!(!is_log_name("maven-haste.jsonl"));
    }

    #[test]
    fn cleanup_removes_expired_logs_but_preserves_current_and_unrelated_files() {
        let directory = TempDir::new().unwrap();
        let today = time::OffsetDateTime::now_utc().date();
        let expired = today - time::Duration::days(2);
        let expired_name = format!("maven-haste.{expired}.jsonl");
        let current_name = format!("maven-haste.{today}.jsonl");
        fs::write(directory.path().join(&expired_name), "old").unwrap();
        fs::write(directory.path().join(&current_name), "current").unwrap();
        fs::write(directory.path().join("unrelated.jsonl"), "keep").unwrap();

        cleanup(directory.path(), Duration::from_secs(24 * 60 * 60)).unwrap();

        assert!(!directory.path().join(expired_name).exists());
        assert!(directory.path().join(current_name).exists());
        assert!(directory.path().join("unrelated.jsonl").exists());
    }

    #[test]
    fn directory_validation_creates_no_formal_log_file() {
        let directory = TempDir::new().unwrap();
        let config = FileLoggingConfig {
            directory: directory.path().join("logs"),
            ..FileLoggingConfig::default()
        };
        validate_directory(&config).unwrap();
        assert!(config.directory.is_dir());
        assert!(fs::read_dir(config.directory).unwrap().next().is_none());
    }
}
