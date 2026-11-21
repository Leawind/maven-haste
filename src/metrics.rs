//! Prometheus text exposition of cache and upstream statistics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::cache::CacheStats;

/// Buckets shared by the response and upstream request histograms.
const DURATION_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Latency histogram of responses served to clients.
pub static RESPONSE_DURATION: Histogram = Histogram::new();

/// Latency histogram of upstream requests, from connect to response headers.
pub static UPSTREAM_DURATION: Histogram = Histogram::new();

pub struct Histogram {
    buckets: [AtomicU64; DURATION_BUCKETS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct HistogramSnapshot {
    /// Per-bucket counts, in the same order as [`DURATION_BUCKETS`].
    pub buckets: Vec<u64>,
    pub count: u64,
    /// Total observed time in seconds.
    pub sum: f64,
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; DURATION_BUCKETS.len()],
            count: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        if let Some(index) = DURATION_BUCKETS.iter().position(|upper| seconds <= *upper) {
            self.buckets[index].fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: self
                .buckets
                .iter()
                .map(|bucket| bucket.load(Ordering::Relaxed))
                .collect(),
            count: self.count.load(Ordering::Relaxed),
            sum: self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        }
    }
}

/// Renders the statistics in Prometheus text format 0.0.4. Every metric family
/// writes exactly one HELP and TYPE line before its samples, as the format
/// requires.
pub fn render(
    stats: &CacheStats,
    bytes_served: u64,
    interrupted_requests: u64,
    response_duration: &HistogramSnapshot,
    upstream_duration: &HistogramSnapshot,
) -> String {
    let mut output = String::new();
    push_family(
        &mut output,
        "maven_haste_build_info",
        "Build information; the value is always 1.",
        MetricType::Gauge,
        &[(vec![("version", env!("CARGO_PKG_VERSION"))], 1.0)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_files",
        "Number of cached artifact files.",
        MetricType::Gauge,
        &[(Vec::new(), stats.files as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_bytes",
        "Total logical size of cached artifact files.",
        MetricType::Gauge,
        &[(Vec::new(), stats.total_size as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_negative_entries",
        "Number of remembered upstream 404s.",
        MetricType::Gauge,
        &[(Vec::new(), stats.negative_entries as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_max_bytes",
        "Configured cache size limit; 0 when unlimited.",
        MetricType::Gauge,
        &[(Vec::new(), stats.max_size.unwrap_or(0) as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_requests_total",
        "Artifact requests served.",
        MetricType::Counter,
        &[(Vec::new(), stats.requests as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_hits_total",
        "Requests served from the cache.",
        MetricType::Counter,
        &[(Vec::new(), stats.hits as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_misses_total",
        "Requests that required an upstream download.",
        MetricType::Counter,
        &[(Vec::new(), stats.misses as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_stale_hits_total",
        "Requests served stale while a refresh ran.",
        MetricType::Counter,
        &[(Vec::new(), stats.stale_hits as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_negative_hits_total",
        "Requests answered from remembered 404s.",
        MetricType::Counter,
        &[(Vec::new(), stats.negative_hits as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_bytes_served_total",
        "Response body bytes sent to clients.",
        MetricType::Counter,
        &[(Vec::new(), bytes_served as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_interrupted_requests_total",
        "Client responses that ended aborted or with a body error.",
        MetricType::Counter,
        &[(Vec::new(), interrupted_requests as f64)],
    );
    push_family(
        &mut output,
        "maven_haste_cache_hit_rate",
        "Share of requests served from the cache in this process.",
        MetricType::Gauge,
        &[(Vec::new(), stats.hit_rate)],
    );

    push_histogram(
        &mut output,
        "maven_haste_response_duration_seconds",
        "Time spent serving one client request.",
        response_duration,
    );
    push_histogram(
        &mut output,
        "maven_haste_upstream_request_duration_seconds",
        "Time from starting an upstream request until its headers arrive.",
        upstream_duration,
    );

    let circuit_samples = stats
        .upstreams
        .iter()
        .flat_map(|upstream| {
            ["closed", "open", "half_open"].map(move |state| {
                (
                    vec![("repository", upstream.id.as_str()), ("state", state)],
                    f64::from(u8::from(upstream.circuit == state)),
                )
            })
        })
        .collect::<Vec<_>>();
    push_family(
        &mut output,
        "maven_haste_upstream_circuit_state",
        "1 when the repository circuit breaker is in this state.",
        MetricType::Gauge,
        &circuit_samples,
    );

    let failure_samples = stats
        .upstreams
        .iter()
        .map(|upstream| {
            (
                vec![("repository", upstream.id.as_str())],
                upstream.failures as f64,
            )
        })
        .collect::<Vec<_>>();
    push_family(
        &mut output,
        "maven_haste_upstream_failures",
        "Consecutive upstream failures recorded by the breaker.",
        MetricType::Gauge,
        &failure_samples,
    );
    output
}

#[derive(Clone, Copy)]
enum MetricType {
    Counter,
    Gauge,
}

impl MetricType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

fn push_family(
    output: &mut String,
    name: &str,
    help: &str,
    metric_type: MetricType,
    samples: &[(Vec<(&str, &str)>, f64)],
) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push(' ');
    output.push_str(metric_type.as_str());
    output.push('\n');
    for (labels, value) in samples {
        push_sample(output, name, labels, *value);
    }
}

fn push_sample(output: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    output.push_str(name);
    if !labels.is_empty() {
        output.push('{');
        for (index, (key, value)) in labels.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            output.push_str(key);
            output.push_str("=\"");
            output.push_str(&escape_label_value(value));
            output.push('"');
        }
        output.push('}');
    }
    output.push(' ');
    output.push_str(&format_metric_value(value));
    output.push('\n');
}

fn push_histogram(output: &mut String, name: &str, help: &str, snapshot: &HistogramSnapshot) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" histogram\n");
    let mut cumulative = 0_u64;
    for (index, upper) in DURATION_BUCKETS.iter().enumerate() {
        cumulative = cumulative.saturating_add(snapshot.buckets.get(index).copied().unwrap_or(0));
        let le = format_metric_value(*upper);
        push_sample(
            output,
            &format!("{name}_bucket"),
            &[("le", le.as_str())],
            cumulative as f64,
        );
    }
    push_sample(
        output,
        &format!("{name}_bucket"),
        &[("le", "+Inf")],
        snapshot.count as f64,
    );
    push_sample(output, &format!("{name}_sum"), &[], snapshot.sum);
    push_sample(output, &format!("{name}_count"), &[], snapshot.count as f64);
}

/// Escapes the two characters the text format requires: backslash and the
/// double quote, plus newlines for readability.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn format_metric_value(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.2e18 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::UpstreamStatus;

    fn stats() -> CacheStats {
        CacheStats {
            files: 3,
            total_size: 1024,
            negative_entries: 1,
            requests: 10,
            hits: 7,
            misses: 3,
            stale_hits: 2,
            negative_hits: 1,
            hit_rate: 0.7,
            max_size: Some(4096),
            upstreams: vec![
                UpstreamStatus {
                    id: "central".into(),
                    circuit: "open".into(),
                    failures: 3,
                },
                UpstreamStatus {
                    id: "fabric".into(),
                    circuit: "closed".into(),
                    failures: 0,
                },
            ],
        }
    }

    #[test]
    fn renders_metrics_with_labels_and_escaped_values() {
        let response_duration = Histogram::new();
        response_duration.observe(Duration::from_millis(20));
        response_duration.observe(Duration::from_secs(2));
        let upstream_duration = Histogram::new();
        upstream_duration.observe(Duration::from_millis(5));

        let text = render(
            &stats(),
            512,
            1,
            &response_duration.snapshot(),
            &upstream_duration.snapshot(),
        );
        assert!(text.contains("maven_haste_cache_files 3\n"));
        assert!(text.contains("maven_haste_cache_bytes 1024\n"));
        assert!(text.contains("maven_haste_cache_max_bytes 4096\n"));
        assert!(text.contains("maven_haste_bytes_served_total 512\n"));
        assert!(text.contains("maven_haste_interrupted_requests_total 1\n"));
        assert!(text.contains(
            "maven_haste_upstream_circuit_state{repository=\"central\",state=\"open\"} 1\n"
        ));
        assert!(text.contains(
            "maven_haste_upstream_circuit_state{repository=\"central\",state=\"closed\"} 0\n"
        ));
        assert!(text.contains("maven_haste_upstream_failures{repository=\"central\"} 3\n"));
        assert!(text.contains("# TYPE maven_haste_requests_total counter\n"));
    }

    #[test]
    fn emits_one_metadata_header_per_metric_family() {
        let text = render(
            &stats(),
            0,
            0,
            &HistogramSnapshot::default(),
            &HistogramSnapshot::default(),
        );
        for family in [
            "maven_haste_upstream_circuit_state",
            "maven_haste_upstream_failures",
            "maven_haste_response_duration_seconds",
        ] {
            assert_eq!(
                text.matches(&format!("# TYPE {family} ")).count(),
                1,
                "{family} must have exactly one TYPE line"
            );
            assert_eq!(
                text.matches(&format!("# HELP {family} ")).count(),
                1,
                "{family} must have exactly one HELP line"
            );
        }
    }

    #[test]
    fn renders_histogram_buckets_cumulatively() {
        let histogram = Histogram::new();
        histogram.observe(Duration::from_millis(20));
        histogram.observe(Duration::from_secs(2));
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 2);
        let text = render(
            &CacheStats {
                upstreams: Vec::new(),
                ..stats()
            },
            0,
            0,
            &snapshot,
            &HistogramSnapshot::default(),
        );
        assert!(text.contains("maven_haste_response_duration_seconds_bucket{le=\"0.025\"} 1\n"));
        assert!(text.contains("maven_haste_response_duration_seconds_bucket{le=\"2.5\"} 2\n"));
        assert!(text.contains("maven_haste_response_duration_seconds_bucket{le=\"+Inf\"} 2\n"));
        assert!(text.contains("maven_haste_response_duration_seconds_count 2\n"));
    }

    #[test]
    fn escapes_special_characters_in_label_values() {
        assert_eq!(escape_label_value("a\\b\"c"), "a\\\\b\\\"c");
    }
}
