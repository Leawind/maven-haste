use std::io::{Read, Write};
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
        .args(["check", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        directory
            .path()
            .join("repository/.maven-haste/tmp")
            .is_dir()
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
fn bind_failure_does_not_announce_readiness() {
    let directory = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(&directory, &listener.local_addr().unwrap().to_string());
    let output = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Maven proxy is ready"), "{stderr}");
}

#[test]
fn config_init_creates_a_commented_example_without_overwriting() {
    let directory = TempDir::new().unwrap();
    let expected = example_config();
    let first = Command::new(binary())
        .args(["config", "init"])
        .current_dir(directory.path())
        .output()
        .unwrap();

    assert!(first.status.success());
    let config_path = directory.path().join("maven-haste.toml");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), expected);
    assert!(
        Command::new(binary())
            .args(["check", "--config"])
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
fn config_example_prints_the_embedded_commented_template() {
    let output = Command::new(binary())
        .args(["config", "example"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), example_config());
}

fn example_config() -> &'static str {
    include_str!("../maven-haste.example.toml")
}

#[test]
fn running_binary_serves_health_endpoint() {
    let directory = TempDir::new().unwrap();
    let address = unused_address();
    let config = write_config(&directory, &address.to_string());
    let child = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);

    let response = wait_for_health(address);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("OK"), "{response}");

    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

#[test]
fn file_logging_writes_daily_json_access_events() {
    let directory = TempDir::new().unwrap();
    let address = unused_address();
    let config = write_config(&directory, &address.to_string());
    let mut source = std::fs::read_to_string(&config).unwrap();
    source.push_str("\n[logging.file]\ndirectory = './logs'\n");
    std::fs::write(&config, source).unwrap();
    let child = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);

    wait_for_health(address);
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
fn verbose_access_logs_are_compact_without_losing_regular_log_fields() {
    let directory = TempDir::new().unwrap();
    let address = unused_address();
    let config = write_config(&directory, &address.to_string());
    let child = Command::new(binary())
        .args(["--verbose", "run", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);

    wait_for_health(address);
    let response = wait_for_response(address, "/maven/com/example/demo/1.0/demo-1.0.jar");
    assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");

    let mut stdout = child.0.stdout.take().unwrap();
    let mut stderr = child.0.stderr.take().unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut output = String::new();
    stdout.read_to_string(&mut output).unwrap();
    stderr.read_to_string(&mut output).unwrap();

    let access = output
        .lines()
        .find(|line| line.contains("[NONE] GET /maven/com/example/demo/1.0/demo-1.0.jar"))
        .unwrap_or_else(|| panic!("verbose access log was not written: {output}"));
    assert!(access.contains(" DEBUG [NONE]"), "{access}");
    for field in [
        "maven_haste::access",
        "cache=",
        "method=",
        "path=",
        "status=",
        "upstream=\"",
        "elapsed_ms=",
        "bytes_sent=",
        "completion=",
    ] {
        assert!(!access.contains(field), "{access}");
    }
    assert!(output.contains("loaded configuration") && output.contains("config="));
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
             [[repositories]]\nname = 'excluded'\nurl = 'http://127.0.0.1:9/'\nrules = ['!*']\n"
        ),
    )
    .unwrap();
    path
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn wait_for_health(address: SocketAddr) -> String {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .write_all(
                    b"GET /__health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            return response;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not listen on {address}");
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
        if let Ok(entries) = std::fs::read_dir(directory.path().join("logs")) {
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
