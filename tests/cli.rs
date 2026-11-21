use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

#[test]
fn check_mode_runs_through_the_built_binary() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");

    let output = Command::new(binary())
        .args(["config", "check", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        !directory
            .path()
            .join("repository/.maven-haste/logs")
            .exists()
    );
}

#[test]
fn missing_explicit_config_exits_with_configuration_error() {
    let directory = TempDir::new().unwrap();
    let output = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(directory.path().join("missing.toml"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn invalid_rust_log_is_reported_instead_of_ignored() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");
    let output = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "maven_haste=[broken")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid RUST_LOG"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_init_creates_a_minimal_yaml_example_without_overwriting() {
    let directory = TempDir::new().unwrap();
    let expected = example_config("yaml");
    let first = Command::new(binary())
        .args(["config", "init"])
        .current_dir(directory.path())
        .output()
        .unwrap();

    assert!(first.status.success());
    let config_path = directory.path().join("maven-haste.yaml");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), expected);
    assert!(
        Command::new(binary())
            .args(["config", "check", "--config"])
            .arg(&config_path)
            .status()
            .unwrap()
            .success()
    );

    let second = Command::new(binary())
        .args(["config", "init"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), expected);
}

#[test]
fn config_init_creates_the_requested_format_without_a_path() {
    let directory = TempDir::new().unwrap();
    for (format, name) in [
        ("json", "maven-haste.json"),
        ("yaml", "maven-haste.yaml"),
        ("toml", "maven-haste.toml"),
    ] {
        let output = Command::new(binary())
            .args(["config", "init", "--format", format])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "init --format {format}");

        let config_path = directory.path().join(name);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            example_config(format),
            "{format}"
        );
        assert!(
            Command::new(binary())
                .args(["config", "check", "--config"])
                .arg(&config_path)
                .status()
                .unwrap()
                .success(),
            "check {format}"
        );
        std::fs::remove_file(&config_path).unwrap();
    }
}

#[test]
fn config_init_rejects_a_format_that_conflicts_with_the_path_extension() {
    let directory = TempDir::new().unwrap();
    let output = Command::new(binary())
        .args(["config", "init", "maven-haste.toml", "--format", "json"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--format json does not match the extension"),
        "{stderr}"
    );
    assert!(
        !directory.path().join("maven-haste.toml").exists(),
        "no file must be created on a conflicting request"
    );
}

#[test]
fn config_example_defaults_to_yaml_and_honors_the_format_flag() {
    let output = Command::new(binary())
        .args(["config", "example"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        example_config("yaml")
    );

    for format in ["json", "yaml", "toml"] {
        let output = Command::new(binary())
            .args(["config", "example", "--format", format])
            .output()
            .unwrap();
        assert!(output.status.success(), "example --format {format}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            example_config(format),
            "{format}"
        );
    }
}

#[test]
fn config_init_pins_the_schema_reference_to_the_current_version() {
    let directory = TempDir::new().unwrap();
    let output = Command::new(binary())
        .args(["config", "init"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let version = Command::new(binary()).arg("--version").output().unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();

    let config = std::fs::read_to_string(directory.path().join("maven-haste.yaml")).unwrap();
    let expected = format!(
        "https://raw.githubusercontent.com/Leawind/maven-haste/v{version}/maven-haste.schema.json"
    );
    assert!(
        config.contains(&expected),
        "schema reference must be pinned to {version}"
    );
    assert!(!config.contains("main/maven-haste.schema.json"));
    assert!(!config.contains("${VERSION}"));
}

#[test]
fn config_init_generates_the_format_of_the_target_extension() {
    let directory = TempDir::new().unwrap();
    for name in [
        "maven-haste.toml",
        "maven-haste.json",
        "maven-haste.yaml",
        "maven-haste.yml",
    ] {
        let output = Command::new(binary())
            .args(["config", "init"])
            .arg(name)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "init {name}");

        let config_path = directory.path().join(name);
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            config.contains("https://raw.githubusercontent.com/Leawind/maven-haste/v"),
            "{name}"
        );
        assert!(!config.contains("${VERSION}"), "{name}");
        assert!(!config.contains("#"), "{name} must not contain comments");

        let check = Command::new(binary())
            .args(["config", "check", "--config"])
            .arg(&config_path)
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "check {name}: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        std::fs::remove_file(config_path).unwrap();
    }
}

#[test]
fn config_init_rejects_unsupported_and_missing_extensions() {
    let directory = TempDir::new().unwrap();
    for name in ["maven-haste.foo", "maven-haste"] {
        let output = Command::new(binary())
            .args(["config", "init"])
            .arg(name)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{name} must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("supported extensions are json, yaml, yml, toml"),
            "{stderr}"
        );
        assert!(
            !directory.path().join(name).exists(),
            "{name} must not be created"
        );
    }
}

#[test]
fn discovers_json_and_yaml_configuration_in_the_working_directory() {
    let directory = TempDir::new().unwrap();
    let bind = unused_address().to_string();
    let json = directory.path().join("maven-haste.json");
    std::fs::write(
        &json,
        format!(
            "{{\"server\": {{\"bind\": \"{bind}\"}}, \"storage\": {{\"root\": \"./repository\"}}, \
             \"repositories\": [{{\"id\": \"excluded\", \"url\": \"http://127.0.0.1:9/\", \
             \"rules\": [\"!**\"]}}]}}"
        ),
    )
    .unwrap();

    let output = Command::new(binary())
        .args(["config", "check"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("configuration is valid"), "{stdout}");
    assert!(stdout.contains("maven-haste.json"), "{stdout}");

    std::fs::remove_file(&json).unwrap();
    let yaml = directory.path().join("maven-haste.yaml");
    std::fs::write(
        &yaml,
        format!(
            "server:\n  bind: '{bind}'\nstorage:\n  root: ./repository\nrepositories:\n  - id: excluded\n    url: http://127.0.0.1:9/\n    rules:\n      - '!**'\n"
        ),
    )
    .unwrap();
    let output = Command::new(binary())
        .args(["config", "check"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("configuration is valid"), "{stdout}");
    assert!(stdout.contains("maven-haste.yaml"), "{stdout}");
}

#[test]
fn multiple_default_configurations_are_rejected() {
    let directory = TempDir::new().unwrap();
    std::fs::write(
        directory.path().join("maven-haste.json"),
        "{\"server\": {\"bind\": \"127.0.0.1:9\"}, \"storage\": {\"root\": \"./repository\"}, \
         \"repositories\": [{\"id\": \"excluded\", \"url\": \"http://127.0.0.1:9/\", \
         \"rules\": [\"!**\"]}]}",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("maven-haste.toml"),
        "[server]\nbind = '127.0.0.1:9'\n\n[storage]\nroot = './repository'\n\n\
         [[repositories]]\nid = 'excluded'\nurl = 'http://127.0.0.1:9/'\nrules = ['!**']\n",
    )
    .unwrap();

    let output = Command::new(binary())
        .args(["config", "check"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("multiple configuration files found"),
        "{stderr}"
    );
}

#[test]
fn config_schema_prints_a_valid_json_schema() {
    let output = Command::new(binary())
        .args(["config", "schema"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(schema["$defs"].is_object());
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
}

fn example_config(format: &str) -> String {
    let format = match format {
        "json" => maven_haste::config::ConfigFormat::Json,
        "yaml" => maven_haste::config::ConfigFormat::Yaml,
        _ => maven_haste::config::ConfigFormat::Toml,
    };
    maven_haste::config::example_config(format)
}

#[test]
fn running_binary_serves_health_endpoint() {
    let directory = TempDir::new().unwrap();
    let (_, response, mut child) = spawn_server(&directory, |bind| {
        write_config(&directory, bind);
    });
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("OK"), "{response}");

    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

#[test]
fn reloads_configuration_while_running() {
    let upstream = spawn_raw_upstream("artifact-body");
    let directory = TempDir::new().unwrap();
    let (address, _, _child) = spawn_server(&directory, |bind| {
        write_reload_config(&directory, bind, &upstream, false);
    });
    let response = wait_for_response(address, "/maven/com/example/demo/1.0/demo-1.0.jar");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

    write_reload_config(&directory, &address.to_string(), &upstream, true);

    for attempt in 0..100 {
        let version = format!("9.{attempt}");
        let response = wait_for_response(
            address,
            &format!("/maven/com/example/demo/{version}/demo-{version}.jar"),
        );
        if response.starts_with("HTTP/1.1 503") {
            let status = wait_for_response(address, "/api/v1/config/reload");
            assert!(
                status.contains("\"successful_reloads\":1"),
                "reload status did not report the applied reload: {status}"
            );
            let metrics = wait_for_response(address, "/api/v1/metrics");
            assert!(
                metrics.contains("maven_haste_config_reloads_total{status=\"ok\"} 1\n"),
                "metrics did not report the applied reload: {metrics}"
            );
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("configuration reload did not take effect");
}

fn write_reload_config(directory: &TempDir, bind: &str, upstream: &str, offline: bool) {
    std::fs::write(
        directory.path().join("maven-haste.toml"),
        format!(
            "[server]\nbind = '{bind}'\n\n[storage]\nroot = './repository'\n\n\
             [upstream]\noffline = {offline}\n\n\
             [[repositories]]\nid = 'local'\nurl = '{upstream}'\n"
        ),
    )
    .unwrap();
}

#[test]
fn file_logging_writes_daily_json_access_events() {
    let directory = TempDir::new().unwrap();
    let (address, _, mut child) = spawn_server(&directory, |bind| {
        let config = write_config(&directory, bind);
        let mut source = std::fs::read_to_string(&config).unwrap();
        source.push_str("\n[logging]\nenabled = true\n");
        std::fs::write(&config, source).unwrap();
    });

    let response = wait_for_response(address, "/maven/com/example/demo/1.0/demo-1.0.jar");
    assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");

    let log = wait_for_access_log(&directory);
    let events = std::fs::read_to_string(log).unwrap();
    assert!(
        events
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(
        events.lines().any(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            event["target"] == "maven_haste::access"
                && event["fields"]["completion"] == "complete"
                && event["fields"]["bytes_sent"].as_u64().is_some()
        }),
        "{events}"
    );

    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

#[test]
fn cache_verify_reports_a_fresh_cache_as_healthy() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");

    let output = Command::new(binary())
        .args(["cache", "verify", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("checked 0 artifacts, 0 issues"), "{stdout}");
}

#[test]
fn cache_stats_reports_an_empty_cache() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");

    let output = Command::new(binary())
        .args(["cache", "stats", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("files: 0"), "{stdout}");
    assert!(stdout.contains("total size: 0 bytes"), "{stdout}");
    assert!(stdout.contains("object files: 0"), "{stdout}");
    assert!(stdout.contains("recorded requests: 0"), "{stdout}");
    assert!(stdout.contains("negative entries: 0"), "{stdout}");
    assert!(stdout.contains("max size: unlimited"), "{stdout}");

    let output = Command::new(binary())
        .args(["cache", "stats", "--json", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["files"], 0);
    assert_eq!(stats["object_files"], 0);
}

#[test]
fn cache_repair_quarantines_a_corrupt_database() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");
    let internal = directory.path().join("repository/.maven-haste");
    std::fs::create_dir_all(&internal).unwrap();
    std::fs::write(internal.join("cache.db"), b"not a sqlite database").unwrap();

    let output = Command::new(binary())
        .args(["cache", "repair", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("quarantined"), "{stdout}");
    assert!(stdout.contains("created a fresh database"), "{stdout}");
    assert!(
        std::fs::read_dir(&internal)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
        "the corrupt database must be preserved"
    );

    let stats = Command::new(binary())
        .args(["cache", "stats", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(stats.status.success());
}

#[test]
fn cache_warm_prefetches_manifest_entries() {
    let upstream = spawn_raw_upstream("artifact-body");
    let directory = TempDir::new().unwrap();
    let config = directory.path().join("maven-haste.toml");
    std::fs::write(
        &config,
        format!(
            "[storage]\nroot = './repository'\n\n[[repositories]]\nid = 'central'\nurl = '{upstream}'\n"
        ),
    )
    .unwrap();
    let manifest = directory.path().join("warm.txt");
    std::fs::write(
        &manifest,
        "# warmed from a build lockfile\ncom/example/demo/1.0/demo-1.0.jar\n",
    )
    .unwrap();

    let output = Command::new(binary())
        .args(["cache", "warm"])
        .arg(&manifest)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("warmed 1 artifacts, 0 failures"),
        "{stdout}"
    );

    let stats = Command::new(binary())
        .args(["cache", "stats", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(stats.status.success());
    let stdout = String::from_utf8(stats.stdout).unwrap();
    assert!(stdout.contains("files: 4"), "{stdout}");
}

#[test]
fn cache_remove_prefix_reports_removed_files_and_rejects_unsafe_prefixes() {
    let directory = TempDir::new().unwrap();
    let config = write_config(&directory, "127.0.0.1:0");

    let output = Command::new(binary())
        .args(["cache", "remove-prefix", "com/example", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("removed 0 files, 0 bytes"), "{stdout}");

    let output = Command::new(binary())
        .args(["cache", "remove-prefix", "../outside", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("non-empty safe relative path"), "{stderr}");
}

/// Serves one artifact body for every non-checksum path and 404 for checksum
/// files, which is enough for `cache warm` to generate its checksums.
fn spawn_raw_upstream(content: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(reader_stream) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(reader_stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let checksum = [".sha1", ".sha256", ".sha512", ".md5"]
                .iter()
                .any(|suffix| path.ends_with(suffix));
            let (status, body) = if checksum {
                ("404 Not Found", "missing")
            } else {
                ("200 OK", content)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"raw\"\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{address}/")
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_maven-haste")
}

fn write_config(directory: &TempDir, bind: &str) -> std::path::PathBuf {
    let path = directory.path().join("maven-haste.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nbind = '{bind}'\n\n[storage]\nroot = './repository'\n\n\
             [[repositories]]\nid = 'excluded'\nurl = 'http://127.0.0.1:9/'\nrules = ['!**']\n"
        ),
    )
    .unwrap();
    path
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Spawns the binary on a freshly probed unused port and waits for its health
/// endpoint, returning the address, the health response, and the child
/// process. The port probe cannot hold the port, so losing the bind to
/// another process between the probe and the server start is retried with a
/// new port instead of failing the test.
fn spawn_server(directory: &TempDir, configure: impl Fn(&str)) -> (SocketAddr, String, KillOnDrop) {
    for _ in 0..5 {
        let address = unused_address();
        configure(&address.to_string());
        let mut child = KillOnDrop(
            Command::new(binary())
                .arg("run")
                .arg("--config")
                .arg(directory.path().join("maven-haste.toml"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        if let Some(response) = wait_for_health(&address, &mut child) {
            return (address, response, child);
        }
    }
    panic!("server did not start on any unused port");
}

fn wait_for_health(address: &SocketAddr, child: &mut KillOnDrop) -> Option<String> {
    for _ in 0..100 {
        if matches!(child.0.try_wait(), Ok(Some(_))) {
            return None;
        }
        if let Ok(mut stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .write_all(
                    b"GET /api/v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            return Some(response);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

fn wait_for_response(address: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn wait_for_access_log(directory: &TempDir) -> std::path::PathBuf {
    for _ in 0..100 {
        if let Ok(entries) =
            std::fs::read_dir(directory.path().join("repository/.maven-haste/logs"))
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("maven-haste.")
                    && name.ends_with(".jsonl")
                    && std::fs::read_to_string(entry.path())
                        .is_ok_and(|source| source.contains("maven_haste::access"))
                {
                    return entry.path();
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("access event was not written to the daily JSON log");
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
