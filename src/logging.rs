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
use tracing_subscriber::fmt::time::FormatTime;
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
    let console = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false)
        .with_timer(ConsoleTime)
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

struct ConsoleTime;

impl FormatTime for ConsoleTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        write!(
            writer,
            "{:02}:{:02}:{:02}.{:03}",
            now.hour(),
            now.minute(),
            now.second(),
            now.millisecond()
        )
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
        let mut fields = AccessFields::default();
        event.record(&mut fields);
        ConsoleTime.format_time(&mut writer)?;
        write_colored_level(&mut writer, event.metadata().level())?;
        let message = render_access(&fields);
        write_colored_access_message(&mut writer, &message, access_color(&fields))
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

fn write_colored_access_message(
    writer: &mut Writer<'_>,
    message: &str,
    color: &'static str,
) -> fmt::Result {
    if writer.has_ansi_escapes() {
        writeln!(writer, "\x1b[{color}m{message}\x1b[0m")
    } else {
        writeln!(writer, "{message}")
    }
}

/// Renders one request line. Fixed leading columns keep the status, method,
/// duration, and size readable while the path, whose length varies, comes
/// last.
fn render_access(fields: &AccessFields) -> String {
    let cache = if fields.cache.is_empty() {
        "-"
    } else {
        fields.cache.as_str()
    };
    let status = fields
        .status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let method = if fields.method.is_empty() {
        "-"
    } else {
        fields.method.as_str()
    };
    let mut line = format!(
        "[{}] {} {} {} {} {}",
        cache.to_ascii_uppercase(),
        status,
        method,
        human_duration(fields.elapsed_ms),
        human_bytes(fields.bytes_sent),
        fields.path
    );
    if !fields.upstream.is_empty() && fields.upstream != "-" {
        line.push_str(" upstream=");
        line.push_str(&fields.upstream);
    }
    if !fields.completion.is_empty() && fields.completion != "complete" {
        line.push_str(" completion=");
        line.push_str(&fields.completion);
    }
    line
}

fn human_duration(millis: u64) -> String {
    match millis {
        0 => "<1ms".to_owned(),
        1..1000 => format!("{millis}ms"),
        _ => format!("{:.2}s", millis as f64 / 1000.0),
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    match bytes {
        0 => "-".to_owned(),
        1..1024 => format!("{bytes}B"),
        _ if bytes < 1024 * 1024 => format!("{:.1}KB", bytes as f64 / KIB),
        _ if bytes < 1024 * 1024 * 1024 => {
            format!("{:.1}MB", bytes as f64 / (KIB * KIB))
        }
        _ => format!("{:.2}GB", bytes as f64 / (KIB * KIB * KIB)),
    }
}

fn access_color(fields: &AccessFields) -> &'static str {
    match fields.status {
        Some(400..=499) => "33",
        Some(500..=599) => "31",
        _ => match fields.cache.as_str() {
            "hit" => "32",
            "miss" | "none" => "33",
            "stale" => "36",
            "error" => "31",
            _ => "37",
        },
    }
}

/// The structured fields of one access event, collected for the console
/// formatter.
#[derive(Default)]
struct AccessFields {
    cache: String,
    method: String,
    path: String,
    status: Option<u16>,
    upstream: String,
    elapsed_ms: u64,
    bytes_sent: u64,
    completion: String,
}

impl Visit for AccessFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "cache" => self.cache = value.to_owned(),
            "method" => self.method = value.to_owned(),
            "path" => self.path = value.to_owned(),
            "upstream" => self.upstream = value.to_owned(),
            "completion" => self.completion = value.to_owned(),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "status" => self.status = u16::try_from(value).ok(),
            "elapsed_ms" => self.elapsed_ms = value,
            "bytes_sent" => self.bytes_sent = value,
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // Values without a typed recorder (for example a `Display` wrapper)
        // still render, so a formatter change cannot silently drop a field.
        let rendered = format!("{value:?}");
        match field.name() {
            "cache" => self.cache = rendered,
            "method" => self.method = rendered,
            "path" => self.path = rendered,
            "upstream" => self.upstream = rendered,
            "completion" => self.completion = rendered,
            "status" => self.status = rendered.parse().ok(),
            "elapsed_ms" => self.elapsed_ms = rendered.parse().unwrap_or_default(),
            "bytes_sent" => self.bytes_sent = rendered.parse().unwrap_or_default(),
            _ => {}
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

    fn access_fields(
        cache: &str,
        status: Option<u16>,
        bytes_sent: u64,
        elapsed_ms: u64,
    ) -> AccessFields {
        AccessFields {
            cache: cache.to_owned(),
            method: "GET".to_owned(),
            path: "com/example/demo/1.0/demo-1.0.jar".to_owned(),
            status,
            upstream: "central".to_owned(),
            elapsed_ms,
            bytes_sent,
            completion: "complete".to_owned(),
        }
    }

    #[test]
    fn access_colors_highlight_error_statuses() {
        assert_eq!(access_color(&access_fields("hit", Some(200), 1, 1)), "32");
        assert_eq!(access_color(&access_fields("hit", Some(304), 0, 1)), "32");
        assert_eq!(access_color(&access_fields("none", Some(404), 0, 1)), "33");
        assert_eq!(access_color(&access_fields("none", Some(502), 0, 1)), "31");
        assert_eq!(access_color(&access_fields("error", Some(500), 0, 1)), "31");
        assert_eq!(access_color(&access_fields("miss", Some(200), 1, 1)), "33");
        assert_eq!(access_color(&access_fields("stale", Some(200), 1, 1)), "36");
        assert_eq!(access_color(&access_fields("other", Some(200), 1, 1)), "37");
    }

    #[test]
    fn access_line_puts_fixed_columns_before_the_path() {
        let mut fields = access_fields("hit", Some(200), 1234, 3);
        assert_eq!(
            render_access(&fields),
            "[HIT] 200 GET 3ms 1.2KB com/example/demo/1.0/demo-1.0.jar upstream=central"
        );

        fields.elapsed_ms = 1234;
        assert_eq!(
            render_access(&fields),
            "[HIT] 200 GET 1.23s 1.2KB com/example/demo/1.0/demo-1.0.jar upstream=central"
        );

        fields.upstream = "-".to_owned();
        fields.bytes_sent = 0;
        fields.completion = "aborted".to_owned();
        fields.status = None;
        assert_eq!(
            render_access(&fields),
            "[HIT] - GET 1.23s - com/example/demo/1.0/demo-1.0.jar completion=aborted"
        );
    }

    #[test]
    fn humanizes_sizes_and_durations() {
        assert_eq!(human_duration(0), "<1ms");
        assert_eq!(human_duration(999), "999ms");
        assert_eq!(human_duration(1500), "1.50s");
        assert_eq!(human_bytes(0), "-");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0MB");
    }

    #[test]
    fn renders_access_events_recorded_as_structured_fields() {
        #[derive(Clone)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = Buffer(output.clone());
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .event_format(AccessConsoleFormat)
                .with_ansi(false)
                .with_writer(move || writer.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "maven_haste::access",
                cache = "hit",
                method = "GET",
                path = "com/example/demo-1.0.jar",
                status = 200_u16,
                upstream = "central",
                elapsed_ms = 3_u64,
                bytes_sent = 1234_u64,
                completion = "complete",
            );
        });

        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("[HIT] 200 GET 3ms 1.2KB com/example/demo-1.0.jar upstream=central"),
            "{text}"
        );
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
