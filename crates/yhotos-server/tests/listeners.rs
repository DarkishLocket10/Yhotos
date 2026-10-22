#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Exercises startup, actual socket separation, environment parsing and signals.

use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server {
    child: Child,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn address(ip: &str) -> SocketAddr {
    TcpListener::bind((ip, 0)).unwrap().local_addr().unwrap()
}
fn command(dir: &std::path::Path, public: SocketAddr, admin: SocketAddr) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yhotos-server"));
    command
        .env_clear()
        .env("YHOTOS_PUBLIC_LISTEN", public.to_string())
        .env("YHOTOS_ADMIN_LISTEN", admin.to_string())
        .env(
            "YHOTOS_DATABASE_URL",
            format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()),
        )
        .env("YHOTOS_DATA_DIR", dir)
        .env("YHOTOS_PUBLIC_BASE_URL", "https://example.test")
        .env(
            "YHOTOS_SESSION_KEY",
            STANDARD.encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7())),
        )
        .env("YHOTOS_RATE_LIMITS__CHEAP_PER_MIN", "1")
        .env("YHOTOS_LOG_FORMAT", "pretty")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
fn get(address: SocketAddr, path: &str) -> String {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}
fn ready(server: &mut Server, address: SocketAddr) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if TcpStream::connect_timeout(&address, Duration::from_millis(10)).is_ok() {
            return;
        }
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "listener did not become ready"
    );
}
#[test]
fn actual_listeners_are_separate_and_environment_nesting_works() {
    let dir = tempfile::tempdir().unwrap();
    let public = address("127.0.0.1");
    // macOS does not configure the rest of 127/8 as loopback aliases. IPv6
    // loopback still provides a distinct address without changing host setup.
    let admin = address(if cfg!(target_os = "macos") {
        "::1"
    } else {
        "127.0.0.2"
    });
    let mut server = Server {
        child: command(dir.path(), public, admin).spawn().unwrap(),
    };
    ready(&mut server, public);
    let response = get(public, "/admin/healthz");
    assert!(response.starts_with("HTTP/1.1 404"));
    let limited = get(public, "/healthz");
    assert!(limited.starts_with("HTTP/1.1 429"));
    assert!(limited.contains("retry-after:"));
    #[cfg(feature = "admin")]
    {
        ready(&mut server, admin);
        let response = get(admin, "/admin/healthz");
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("ok"));
        assert!(
            TcpStream::connect_timeout(
                &SocketAddr::new("127.0.0.1".parse().unwrap(), admin.port()),
                Duration::from_millis(100)
            )
            .is_err()
        );
    }
    #[cfg(not(feature = "admin"))]
    assert!(TcpStream::connect_timeout(&admin, Duration::from_millis(100)).is_err());
}
#[test]
fn invalid_configuration_prints_every_problem_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = command(dir.path(), address("127.0.0.1"), address("127.0.0.1"));
    command
        .env_remove("YHOTOS_DATABASE_URL")
        .env("YHOTOS_PUBLIC_BASE_URL", "http://example.test")
        .env(
            "YHOTOS_SESSION_KEY",
            STANDARD.encode(uuid::Uuid::now_v7().as_bytes()),
        )
        .env("YHOTOS_ADMIN_LISTEN", "0.0.0.0:1")
        .stderr(Stdio::piped());
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for field in [
        "database_url:",
        "public_base_url:",
        "session_key:",
        "admin_listen:",
    ] {
        assert_eq!(stderr.matches(field).count(), 1, "{stderr}");
    }
}
#[cfg(unix)]
#[test]
fn sigterm_and_sigint_exit_cleanly() {
    for signal in ["-TERM", "-INT"] {
        let dir = tempfile::tempdir().unwrap();
        let public = address("127.0.0.1");
        let admin = address("127.0.0.1");
        let mut server = Server {
            child: command(dir.path(), public, admin).spawn().unwrap(),
        };
        ready(&mut server, public);
        // HTTP readiness ensures the runtime has polled its signal future.
        assert!(get(public, "/healthz").starts_with("HTTP/1.1 200"));
        assert!(
            Command::new("kill")
                .arg(signal)
                .arg(server.child.id().to_string())
                .status()
                .unwrap()
                .success()
        );
        let start = Instant::now();
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                assert!(status.success(), "signal={signal} status={status}");
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(11));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(TcpStream::connect_timeout(&public, Duration::from_millis(50)).is_err());
    }
}
