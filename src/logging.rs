use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LoggingConfig;
use crate::error::AppError;

pub struct LoggingGuard {
    _file: Option<WorkerGuard>,
}

const SHUTDOWN_NOTICE: &str =
    "Ctrl+C received; shutting down gracefully and waiting for active requests to finish...";

pub fn notify_shutdown_requested() -> io::Result<()> {
    let stderr = io::stderr();
    let ansi = stderr.is_terminal();
    write_shutdown_notice(stderr.lock(), ansi)
}

fn write_shutdown_notice(mut writer: impl Write, ansi: bool) -> io::Result<()> {
    if ansi {
        writeln!(writer, "\x1b[33m{SHUTDOWN_NOTICE}\x1b[0m")?;
    } else {
        writeln!(writer, "{SHUTDOWN_NOTICE}")?;
    }
    writer.flush()
}

pub fn validate_directory(config: &LoggingConfig) -> Result<(), AppError> {
    fs::create_dir_all(config.directory()).map_err(|error| directory_error(config, error))?;
    let probe = config
        .directory()
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

pub fn init(verbose: bool, config: &LoggingConfig) -> Result<LoggingGuard, AppError> {
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
    let console =
        tracing_subscriber::fmt::layer()
            .compact()
            .with_filter(console_filter.clone().and(filter_fn(|metadata| {
                metadata.target() != "maven_haste::access"
            })));
    let access_console = tracing_subscriber::fmt::layer()
        .event_format(AccessConsoleFormat)
        .with_filter(console_filter.and(filter_fn(|metadata| {
            metadata.target() == "maven_haste::access"
        })));

    if config.enabled {
        validate_directory(config)?;
        cleanup(config.directory(), config.retention)?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("maven-haste")
            .filename_suffix("jsonl")
            .build(config.directory())
            .map_err(|error| AppError::Runtime(format!("failed to open file log: {error}")))?;
        let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .finish(appender);
        let file_filter = EnvFilter::try_new(&config.filter)
            .map_err(|error| AppError::Runtime(format!("invalid logging.filter: {error}")))?;
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(file_filter);
        tracing_subscriber::registry()
            .with(console)
            .with(access_console)
            .with(file_layer)
            .try_init()
            .map_err(|error| AppError::Runtime(error.to_string()))?;
        spawn_cleanup(config.clone());
        Ok(LoggingGuard { _file: Some(guard) })
    } else {
        tracing_subscriber::registry()
            .with(console)
            .with(access_console)
            .try_init()
            .map_err(|error| AppError::Runtime(error.to_string()))?;
        Ok(LoggingGuard { _file: None })
    }
}

struct AccessConsoleFormat;

impl<S, N> FormatEvent<S, N> for AccessConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        SystemTime.format_time(&mut writer)?;
        write_colored_level(&mut writer, event.metadata().level())?;
        write_colored_access_message(&mut writer, &visitor.message)
    }
}

fn write_colored_level(writer: &mut Writer<'_>, level: &Level) -> fmt::Result {
    let color = match *level {
        Level::TRACE => "35",
        Level::DEBUG => "34",
        Level::INFO => "32",
        Level::WARN => "33",
        Level::ERROR => "31",
    };
    if writer.has_ansi_escapes() {
        write!(writer, " \x1b[{color}m{level}\x1b[0m ")
    } else {
        write!(writer, " {level} ")
    }
}

fn write_colored_access_message(writer: &mut Writer<'_>, message: &str) -> fmt::Result {
    let Some(prefix_end) = message.find(']').map(|index| index + 1) else {
        return writeln!(writer, "{message}");
    };
    let (prefix, remainder) = message.split_at(prefix_end);
    let color = match prefix {
        "[HIT]" => "32",
        "[MISS]" => "33",
        "[STALE]" => "36",
        "[ERROR]" => "31",
        _ => "37",
    };
    if writer.has_ansi_escapes() {
        writeln!(writer, "\x1b[{color}m{prefix}\x1b[0m{remainder}")
    } else {
        writeln!(writer, "{message}")
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

fn spawn_cleanup(config: LoggingConfig) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = cleanup(config.directory(), config.retention) {
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

fn directory_error(config: &LoggingConfig, error: io::Error) -> AppError {
    AppError::Runtime(format!(
        "file log directory {} is not writable: {error}",
        config.directory().display()
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
        let config = LoggingConfig {
            directory: Some(directory.path().join("logs")),
            ..LoggingConfig::default()
        };
        validate_directory(&config).unwrap();
        assert!(config.directory().is_dir());
        assert!(fs::read_dir(config.directory()).unwrap().next().is_none());
    }
}
