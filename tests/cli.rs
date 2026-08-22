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
        .args(["--check", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("configuration is valid")
    );
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
        .arg("--config")
        .arg(directory.path().join("missing.toml"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("configuration file")
    );
}

#[test]
fn running_binary_serves_health_endpoint() {
    let directory = TempDir::new().unwrap();
    let address = unused_address();
    let config = write_config(&directory, &address.to_string());
    let child = Command::new(binary())
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

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
