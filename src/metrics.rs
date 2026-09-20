//! Prometheus text exposition of cache and upstream statistics.

use crate::cache::CacheStats;

/// Renders the statistics in Prometheus text format 0.0.4.
pub fn render(stats: &CacheStats, bytes_served: u64, interrupted_requests: u64) -> String {
    let mut output = String::new();
    push_metric(
        &mut output,
        "maven_haste_build_info",
        "Build information; the value is always 1.",
        1.0,
        &[("version", env!("CARGO_PKG_VERSION"))],
    );
    push_metric(
        &mut output,
        "maven_haste_cache_files",
        "Number of cached artifact files.",
        stats.files as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_cache_bytes",
        "Total size of cached artifact files.",
        stats.total_size as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_negative_entries",
        "Number of remembered upstream 404s.",
        stats.negative_entries as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_cache_max_bytes",
        "Configured cache size limit; 0 when unlimited.",
        stats.max_size.unwrap_or(0) as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_requests_total",
        "Artifact requests served.",
        stats.requests as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_cache_hits_total",
        "Requests served from the cache.",
        stats.hits as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_cache_misses_total",
        "Requests that required an upstream download.",
        stats.misses as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_stale_hits_total",
        "Requests served stale while a refresh ran.",
        stats.stale_hits as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_negative_hits_total",
        "Requests answered from remembered 404s.",
        stats.negative_hits as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_bytes_served_total",
        "Response body bytes sent to clients.",
        bytes_served as f64,
        &[],
    );
    push_metric(
        &mut output,
        "maven_haste_interrupted_requests_total",
        "Client responses that ended aborted or with a body error.",
        interrupted_requests as f64,
        &[],
    );
    for upstream in &stats.upstreams {
        for state in ["closed", "open", "half_open"] {
            push_metric(
                &mut output,
                "maven_haste_upstream_circuit_state",
                "1 when the repository circuit breaker is in this state.",
                f64::from(u8::from(upstream.circuit == state)),
                &[("repository", &upstream.id), ("state", state)],
            );
        }
        push_metric(
            &mut output,
            "maven_haste_upstream_failures_total",
            "Consecutive upstream failures recorded by the breaker.",
            upstream.failures as f64,
            &[("repository", &upstream.id)],
        );
    }
    output
}

fn push_metric(output: &mut String, name: &str, help: &str, value: f64, labels: &[(&str, &str)]) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" gauge\n");
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

    #[test]
    fn renders_metrics_with_labels_and_escaped_values() {
        let stats = CacheStats {
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
            upstreams: vec![UpstreamStatus {
                id: "central".into(),
                circuit: "open".into(),
                failures: 3,
            }],
        };

        let text = render(&stats, 512, 1);
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
        assert!(text.contains("maven_haste_upstream_failures_total{repository=\"central\"} 3\n"));
        assert!(text.contains("# TYPE maven_haste_requests_total gauge\n"));
    }

    #[test]
    fn escapes_special_characters_in_label_values() {
        assert_eq!(escape_label_value("a\\b\"c"), "a\\\\b\\\"c");
    }
}
