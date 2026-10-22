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
fn admin_address() -> SocketAddr {
    // macOS does not configure the rest of 127/8 as loopback aliases. IPv6
    // loopback still provides a distinct address without changing host setup.
    address(if cfg!(target_os = "macos") {
        "::1"
    } else {
        "127.0.0.2"
    })
}
fn command(dir: &std::path::Path, public: SocketAddr, admin: SocketAddr) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yhotos-server"));
    command
        .env_clear()
        .env("YHOTOS_PUBLIC_LISTEN", public.to_string())
        .env("YHOTOS_ADMIN_LISTEN", admin.to_string())
        .env(
            "YHOTOS_DATABASE_URL_PUBLIC",
            std::env::var("DATABASE_URL").unwrap(),
        )
        .env(
            "YHOTOS_DATABASE_URL_ADMIN",
            std::env::var("DATABASE_URL").unwrap(),
        )
        .env("YHOTOS_ADMIN_BASE_URL", "https://admin.example.test:8443")
        .env("YHOTOS_DATABASE_TLS", "disable")
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
    let admin = admin_address();
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
        assert!(response.starts_with("HTTP/1.1 303"));
        assert!(response.contains("location: /admin/login"));
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
        .env_remove("YHOTOS_DATABASE_URL_PUBLIC")
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
        "database_url_public:",
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

#[tokio::test]
async fn create_admin_prints_one_policy_compliant_temporary_password() {
    use secrecy::{ExposeSecret, SecretString};
    let url = std::env::var("DATABASE_URL").unwrap();
    let username = format!("é-{}", uuid::Uuid::now_v7());
    let input = format!(" \u{2003}e\u{301}-{} \t", username.trim_start_matches("é-"));
    let output = Command::new(env!("CARGO_BIN_EXE_yhotos-server"))
        .env_clear()
        .env("YHOTOS_DATABASE_URL_ADMIN", &url)
        .env("YHOTOS_DATABASE_TLS", "disable")
        .args(["create-admin", "--username", &input])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stderr, [] as [u8; 0]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let password = SecretString::from(stdout.trim());
    assert_eq!(password.expose_secret().len(), 20);
    yhotos_auth::check_password_policy(&password, &username).unwrap();
    let db = yhotos_db::Db::connect(
        &url.into(),
        yhotos_db::Role::Admin,
        1,
        yhotos_db::DatabaseTls::Disable,
    )
    .await
    .unwrap();
    let mut c = db.pool().acquire().await.unwrap();
    let user = yhotos_db::users::get_by_username(&mut c, &username)
        .await
        .unwrap()
        .unwrap();
    assert!(user.is_admin);
    assert!(user.must_change_password);
    assert!(
        yhotos_auth::verify_password(
            &password,
            &yhotos_auth::PasswordHash::parse(&user.password_hash).unwrap()
        )
        .unwrap()
    );
    assert_eq!(user.username, username);
    drop(c);
    // Exercise the actual CLI-to-HTTP boundary with both composed and padded,
    // decomposed spellings. A created account must always be able to sign in.
    let dir = tempfile::tempdir().unwrap();
    let public = address("127.0.0.1");
    let mut cmd = command(dir.path(), public, admin_address());
    cmd.env("YHOTOS_RATE_LIMITS__CHEAP_PER_MIN", "100");
    let mut server = Server {
        child: cmd.spawn().unwrap(),
    };
    ready(&mut server, public);
    for spelling in [&username, &input] {
        let form = get(public, "/login");
        let csrf = form
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: __Host-yhotos-csrf="))
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let body = format!(
            "_csrf={csrf}&username={}&password={}",
            form_encode(spelling),
            form_encode(password.expose_secret())
        );
        let mut socket = TcpStream::connect(public).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(socket, "POST /login HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/x-www-form-urlencoded\r\nCookie: __Host-yhotos-csrf={csrf}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 303"));
        assert!(response.contains("location: /u/password"));
    }
}

#[tokio::test]
async fn startup_uses_separate_restricted_login_roles() {
    use secrecy::SecretString;
    use sqlx::ConnectOptions;
    let url = std::env::var("DATABASE_URL").unwrap();
    let db = yhotos_db::Db::connect(
        &SecretString::from(url),
        yhotos_db::Role::Admin,
        1,
        yhotos_db::DatabaseTls::Disable,
    )
    .await
    .unwrap();
    let mut connection = db.pool().acquire().await.unwrap();
    // Serialize this test's cluster-wide login names; never change deployed roles.
    sqlx::query!("SELECT pg_advisory_lock(9009)")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query!("DROP ROLE IF EXISTS yhotos_009_start_public, yhotos_009_start_admin")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query!("CREATE ROLE yhotos_009_start_public LOGIN IN ROLE yhotos_public")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query!("CREATE ROLE yhotos_009_start_admin LOGIN IN ROLE yhotos_admin")
        .execute(&mut *connection)
        .await
        .unwrap();
    // Reuse only the disposable migration login's generated password verifier.
    // No password appears in SQL, fixtures, diagnostics or process output.
    sqlx::query!("UPDATE pg_authid SET rolpassword=(SELECT rolpassword FROM pg_authid WHERE rolname=current_user) WHERE rolname IN ('yhotos_009_start_public','yhotos_009_start_admin')")
        .execute(&mut *connection).await.unwrap();
    let login_url = |name| {
        db.pool()
            .connect_options()
            .as_ref()
            .clone()
            .username(name)
            .to_url_lossy()
            .to_string()
    };
    let public_url = login_url("yhotos_009_start_public");
    let admin_url = login_url("yhotos_009_start_admin");
    // The exact original failure: admin-only credentials cannot SET ROLE public.
    assert!(
        yhotos_db::Db::connect(
            &SecretString::from(admin_url.clone()),
            yhotos_db::Role::Public,
            1,
            yhotos_db::DatabaseTls::Disable
        )
        .await
        .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let public = address("127.0.0.1");
    let admin = admin_address();
    let mut cmd = command(dir.path(), public, admin);
    cmd.env("YHOTOS_DATABASE_URL_PUBLIC", public_url);
    #[cfg(feature = "admin")]
    cmd.env("YHOTOS_DATABASE_URL_ADMIN", admin_url);
    #[cfg(not(feature = "admin"))]
    cmd.env_remove("YHOTOS_DATABASE_URL_ADMIN");
    let mut server = Server {
        child: cmd.spawn().unwrap(),
    };
    ready(&mut server, public);
    assert!(get(public, "/login").starts_with("HTTP/1.1 200"));
    #[cfg(feature = "admin")]
    {
        ready(&mut server, admin);
        assert!(get(admin, "/admin/login").starts_with("HTTP/1.1 200"));
    }
    drop(server);
    sqlx::query!("DROP ROLE yhotos_009_start_public, yhotos_009_start_admin")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query!("SELECT pg_advisory_unlock(9009)")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
}

fn form_encode(value: &str) -> String {
    use std::fmt::Write;
    value.bytes().fold(String::new(), |mut encoded, byte| {
        write!(&mut encoded, "%{byte:02X}").unwrap();
        encoded
    })
}

#[test]
fn create_admin_rejects_empty_whitespace_and_oversized_usernames() {
    for username in [String::new(), " \t\n\u{2003}".into(), "x".repeat(65)] {
        let output = Command::new(env!("CARGO_BIN_EXE_yhotos-server"))
            .env_clear()
            .args(["create-admin", "--username", &username])
            .output()
            .unwrap();
        assert!(!output.status.success());
        #[allow(
            clippy::assert_is_empty,
            reason = "never print an accidentally generated password on failure"
        )]
        {
            assert!(output.stdout.is_empty());
        }
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().trim(),
            "invalid username"
        );
    }
}
