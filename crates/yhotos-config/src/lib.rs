#![forbid(unsafe_code)]

//! Validates environment configuration once; secret values never enter errors.

use base64::{Engine, engine::general_purpose::STANDARD};
use figment::{
    Figment,
    providers::{Env, Serialized},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use url::Url;

// The codec host accepts tightened limits only, up to this 2 GiB ceiling.
const MAX_UPLOAD_BYTES: u64 = 2 << 30;

/// Validated process settings. Debug output redacts secret fields.
#[derive(Debug)]
pub struct Config {
    /// Public socket to bind.
    pub public_listen: SocketAddr,
    /// Specific admin bind address, mandatory only with the admin feature.
    pub admin_listen: Option<SocketAddr>,
    /// Public login connection string, never logged.
    pub database_url_public: SecretString,
    /// Separate administrative login, required with the admin feature.
    pub database_url_admin: Option<SecretString>,
    /// Separate worker login, required by worker startup and unused by servers.
    pub database_url_worker: Option<SecretString>,
    /// Existing storage root.
    pub data_dir: PathBuf,
    /// Canonical HTTPS origin for public links.
    pub public_base_url: Url,
    /// Canonical HTTPS origin for admin forms and passkeys; required with admin.
    pub admin_base_url: Option<Url>,
    /// Base64-encoded key containing at least 32 decoded bytes.
    pub session_key: SecretString,
    /// Enables only Cloudflare's client-IP header, on tunnel deployments.
    pub trust_proxy: bool,
    /// Independent per-IP token-bucket capacities.
    pub rate_limits: RateLimits,
    /// Positive concurrent-stream cap shared by originals and video (default 64).
    pub global_stream_cap: u16,
    /// Maximum stalled socket write, from `YHOTOS_WRITE_TIMEOUT_SECS` (default 30).
    pub write_timeout: Duration,
    /// Absolute original/video body lifetime, from `YHOTOS_TRANSFER_DEADLINE_SECS` (default 1800).
    pub transfer_deadline: Duration,
    /// Maximum total upload and chunk lengths.
    pub upload: Upload,
    /// Structured or interactive tracing output.
    pub log_format: LogFormat,
    /// CPU worker limits and trusted codec executable.
    pub worker: Worker,
}
/// Positive per-minute capacities for independent request classes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct RateLimits {
    /// Inexpensive page and static requests.
    pub cheap_per_min: u32,
    /// Search, original downloads and archive requests.
    pub expensive_per_min: u32,
    /// Authentication and share-link unlock requests.
    pub auth_per_min: u32,
}
/// Bounded upload sizes, with chunks no larger than the whole upload.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Upload {
    /// Maximum complete upload size, at most the codec host's 2 GiB ceiling.
    pub max_bytes: u64,
    /// Maximum individual chunk size.
    pub chunk_bytes: u32,
}
/// CPU worker settings, configured through `YHOTOS_WORKER__*` variables.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Worker {
    /// Simultaneous host processes, default half the CPU count, minimum one.
    pub concurrency: u16,
    /// Trusted absolute codec executable; canonicalized by the worker at startup.
    pub codec_host_path: PathBuf,
    /// Codec wall-clock deadline in seconds, 1..=600, default 300.
    pub codec_timeout: u32,
    /// IANA timezone for offset-less EXIF dates; defaults to UTC.
    pub default_timezone: String,
    /// Threads per host, 1..=64, default two.
    pub vips_concurrency: u8,
    /// Optional structured pipeline plan; omitted uses architecture defaults.
    pub variant_plan: Option<serde_json::Value>,
    /// Explicit local-only plaintext opt-in; the database layer rejects remote hosts.
    pub local_plaintext_database: bool,
    /// Loopback-only Prometheus listener, default 127.0.0.1:9101.
    pub metrics_listen: SocketAddr,
}
impl Default for Worker {
    fn default() -> Self {
        Self {
            concurrency: std::thread::available_parallelism().map_or(1, |n| {
                u16::try_from((n.get() / 2).max(1)).unwrap_or(u16::MAX)
            }),
            codec_host_path: PathBuf::from("/usr/local/bin/yhotos-codec-host"),
            codec_timeout: 300,
            default_timezone: "UTC".into(),
            vips_concurrency: 2,
            variant_plan: None,
            local_plaintext_database: false,
            metrics_listen: SocketAddr::from(([127, 0, 0, 1], 9101)),
        }
    }
}
/// Lowercase output format accepted from the environment.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON records for production.
    Json,
    /// Human-readable records for local development.
    Pretty,
}

#[derive(Serialize)]
struct Defaults {
    public_listen: &'static str,
    admin_listen: Option<SocketAddr>,
    database_url_admin: Option<String>,
    database_url_worker: Option<String>,
    admin_base_url: Option<String>,
    trust_proxy: bool,
    rate_limits: RateLimits,
    global_stream_cap: u16,
    write_timeout_secs: u32,
    transfer_deadline_secs: u32,
    upload: Upload,
    log_format: LogFormat,
    worker: Worker,
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            public_listen: "0.0.0.0:8080",
            admin_listen: None,
            database_url_admin: None,
            database_url_worker: None,
            admin_base_url: None,
            trust_proxy: false,
            rate_limits: RateLimits {
                cheap_per_min: 600,
                expensive_per_min: 30,
                auth_per_min: 10,
            },
            global_stream_cap: 64,
            write_timeout_secs: 30,
            transfer_deadline_secs: 30 * 60,
            upload: Upload {
                max_bytes: MAX_UPLOAD_BYTES,
                chunk_bytes: 8 * 1024 * 1024,
            },
            log_format: LogFormat::Json,
            worker: Worker::default(),
        }
    }
}

/// Contains every configuration problem, with no supplied values or secrets.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration:\n{}", .0.join("\n"))]
pub struct ConfigError(Vec<String>);
impl ConfigError {
    /// Independent validation messages, each suitable for startup diagnostics.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.0
    }
}
fn read<T: DeserializeOwned>(f: &Figment, key: &str, errors: &mut Vec<String>) -> Option<T> {
    if let Ok(value) = f.extract_inner(key) {
        Some(value)
    } else {
        // Figment's own errors can quote input. Never propagate them.
        errors.push(format!("{key}: missing or invalid value"));
        None
    }
}
impl Config {
    /// Loads YHOTOS_* variables, with `__` separating nested keys.
    ///
    /// # Errors
    /// Reports all extraction and validation failures together without values.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_figment(Figment::new().merge(Env::prefixed("YHOTOS_").split("__")))
    }

    /// Validates a provider using the same defaults and checks as the environment.
    ///
    /// # Errors
    /// Returns all independent problems without echoing provider values.
    #[allow(
        clippy::too_many_lines,
        reason = "keep independent field checks and aggregate construction together"
    )]
    pub fn from_figment(provider: Figment) -> Result<Self, ConfigError> {
        let f = Figment::from(Serialized::defaults(Defaults::default())).merge(provider);
        let mut errors = Vec::new();
        let public_listen = read::<SocketAddr>(&f, "public_listen", &mut errors);
        let admin_listen = read::<Option<SocketAddr>>(&f, "admin_listen", &mut errors);
        let database_url_public = read::<SecretString>(&f, "database_url_public", &mut errors);
        let database_url_admin =
            read::<Option<SecretString>>(&f, "database_url_admin", &mut errors);
        let database_url_worker =
            read::<Option<SecretString>>(&f, "database_url_worker", &mut errors);
        let admin_base_url = read::<Option<Url>>(&f, "admin_base_url", &mut errors);
        let data_dir = read::<PathBuf>(&f, "data_dir", &mut errors);
        let public_base_url = read::<Url>(&f, "public_base_url", &mut errors);
        let session_key = read::<SecretString>(&f, "session_key", &mut errors);
        let trust_proxy = read::<bool>(&f, "trust_proxy", &mut errors);
        let cheap = read::<u32>(&f, "rate_limits.cheap_per_min", &mut errors);
        let expensive = read::<u32>(&f, "rate_limits.expensive_per_min", &mut errors);
        let auth = read::<u32>(&f, "rate_limits.auth_per_min", &mut errors);
        let global_stream_cap = read::<u16>(&f, "global_stream_cap", &mut errors);
        let write_timeout_secs = read::<u32>(&f, "write_timeout_secs", &mut errors);
        let transfer_deadline_secs = read::<u32>(&f, "transfer_deadline_secs", &mut errors);
        let max_bytes = read::<u64>(&f, "upload.max_bytes", &mut errors);
        let chunk_bytes = read::<u32>(&f, "upload.chunk_bytes", &mut errors);
        let log_format = read::<LogFormat>(&f, "log_format", &mut errors);
        let worker = read::<Worker>(&f, "worker", &mut errors);
        if let Some(w) = &worker
            && (w.concurrency == 0
                || w.concurrency > 1024
                || w.codec_timeout == 0
                || w.codec_timeout > 600
                || !(1..=64).contains(&w.vips_concurrency)
                || !w.codec_host_path.is_absolute()
                || !w.metrics_listen.ip().is_loopback()
                || jiff::tz::TimeZone::get(&w.default_timezone).is_err())
        {
            errors.push(
                "worker: invalid concurrency, timeout, codec path, metrics address or timezone"
                    .into(),
            );
        }
        if let Some(Some(addr)) = admin_listen
            && addr.ip().to_canonical().is_unspecified()
        {
            errors.push("admin_listen: must be a specific address".into());
        }
        #[cfg(feature = "admin")]
        if matches!(admin_listen, Some(None)) {
            errors.push("admin_listen: required with the admin feature".into());
        }
        #[cfg(feature = "admin")]
        for (field, missing) in [
            (
                "database_url_admin",
                matches!(database_url_admin, Some(None)),
            ),
            ("admin_base_url", matches!(admin_base_url, Some(None))),
        ] {
            if missing {
                errors.push(format!("{field}: required with the admin feature"));
            }
        }
        for (field, url) in [
            ("database_url_public", database_url_public.as_ref()),
            (
                "database_url_admin",
                database_url_admin.as_ref().and_then(Option::as_ref),
            ),
            (
                "database_url_worker",
                database_url_worker.as_ref().and_then(Option::as_ref),
            ),
        ] {
            if let Some(url) = url
                && !Url::parse(url.expose_secret()).is_ok_and(|u| {
                    matches!(u.scheme(), "postgres" | "postgresql") && u.host_str().is_some()
                })
            {
                errors.push(format!("{field}: must be a PostgreSQL URL"));
            }
        }
        if let Some(path) = &data_dir
            && !path.is_dir()
        {
            errors.push("data_dir: must exist and be a directory".into());
        }
        for (field, url) in [
            ("public_base_url", public_base_url.as_ref()),
            (
                "admin_base_url",
                admin_base_url.as_ref().and_then(Option::as_ref),
            ),
        ] {
            if let Some(url) = url
                && (url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some())
            {
                errors.push(format!(
                    "{field}: must be an HTTPS URL without credentials, query or fragment"
                ));
            }
        }
        if let Some(key) = &session_key
            && !STANDARD
                .decode(key.expose_secret())
                .is_ok_and(|bytes| bytes.len() >= 32)
        {
            errors.push("session_key: must be base64 encoding at least 32 bytes".into());
        }
        for (name, value) in [
            ("cheap_per_min", cheap),
            ("expensive_per_min", expensive),
            ("auth_per_min", auth),
        ] {
            if value == Some(0) {
                errors.push(format!("rate_limits.{name}: must be positive"));
            }
        }
        if max_bytes == Some(0) {
            errors.push("upload.max_bytes: must be positive".into());
        }
        if max_bytes.is_some_and(|max| max > MAX_UPLOAD_BYTES) {
            errors.push("upload.max_bytes: must not exceed the codec host ceiling of 2 GiB".into());
        }
        if global_stream_cap == Some(0) {
            errors.push("global_stream_cap: must be positive".into());
        }
        for (name, value) in [
            ("write_timeout_secs", write_timeout_secs),
            ("transfer_deadline_secs", transfer_deadline_secs),
        ] {
            if value == Some(0) {
                errors.push(format!("{name}: must be positive"));
            }
        }
        if chunk_bytes == Some(0) {
            errors.push("upload.chunk_bytes: must be positive".into());
        }
        if let (Some(max), Some(chunk)) = (max_bytes, chunk_bytes)
            && u64::from(chunk) > max
        {
            errors.push("upload.chunk_bytes: must not exceed max_bytes".into());
        }
        if !errors.is_empty() {
            return Err(ConfigError(errors));
        }
        // All missing fields were reported above; this match keeps construction
        // total without unwraps, even if the extraction list changes later.
        match (
            public_listen,
            admin_listen,
            database_url_public,
            database_url_admin,
            database_url_worker,
            admin_base_url,
            data_dir,
            public_base_url,
            session_key,
            trust_proxy,
            cheap,
            expensive,
            auth,
            global_stream_cap,
            write_timeout_secs,
            transfer_deadline_secs,
            max_bytes,
            chunk_bytes,
            log_format,
            worker,
        ) {
            (
                Some(public_listen),
                Some(admin_listen),
                Some(database_url_public),
                Some(database_url_admin),
                Some(database_url_worker),
                Some(admin_base_url),
                Some(data_dir),
                Some(public_base_url),
                Some(session_key),
                Some(trust_proxy),
                Some(cheap_per_min),
                Some(expensive_per_min),
                Some(auth_per_min),
                Some(global_stream_cap),
                Some(write_timeout_secs),
                Some(transfer_deadline_secs),
                Some(max_bytes),
                Some(chunk_bytes),
                Some(log_format),
                Some(worker),
            ) => Ok(Self {
                public_listen,
                admin_listen,
                database_url_public,
                database_url_admin,
                database_url_worker,
                admin_base_url,
                data_dir,
                public_base_url,
                session_key,
                trust_proxy,
                rate_limits: RateLimits {
                    cheap_per_min,
                    expensive_per_min,
                    auth_per_min,
                },
                global_stream_cap,
                write_timeout: Duration::from_secs(u64::from(write_timeout_secs)),
                transfer_deadline: Duration::from_secs(u64::from(transfer_deadline_secs)),
                upload: Upload {
                    max_bytes,
                    chunk_bytes,
                },
                log_format,
                worker,
            }),
            _ => Err(ConfigError(vec!["configuration is incomplete".into()])),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn valid_without_admin(dir: &std::path::Path) -> Figment {
        Figment::new()
            .merge((
                "database_url_public",
                format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()),
            ))
            .merge(("data_dir", dir.to_string_lossy().as_ref()))
            .merge(("public_base_url", "https://example.test"))
            .merge((
                "session_key",
                STANDARD.encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7())),
            ))
    }
    fn valid(dir: &std::path::Path) -> Figment {
        valid_without_admin(dir)
            .merge(("admin_listen", "127.0.0.1:8081"))
            .merge((
                "database_url_admin",
                format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()),
            ))
            .merge(("admin_base_url", "https://admin.example.test:8443"))
    }
    #[test]
    fn defaults_and_secret_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_figment(valid(dir.path()).merge((
            "database_url_worker",
            format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()),
        )))
        .unwrap();
        assert!(!config.trust_proxy);
        assert_eq!(config.public_listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.rate_limits.cheap_per_min, 600);
        assert_eq!(config.global_stream_cap, 64);
        assert_eq!(config.write_timeout, Duration::from_secs(30));
        assert_eq!(config.transfer_deadline, Duration::from_mins(30));
        assert_eq!(config.upload.max_bytes, 2 * 1024 * 1024 * 1024);
        let debug = format!("{config:?}");
        assert!(!debug.contains(config.database_url_public.expose_secret()));
        assert!(!debug.contains(config.database_url_admin.as_ref().unwrap().expose_secret()));
        assert!(!debug.contains(config.database_url_worker.as_ref().unwrap().expose_secret()));
        assert!(!debug.contains(config.session_key.expose_secret()));
    }
    #[test]
    fn server_config_does_not_require_a_worker_login() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_figment(valid(dir.path())).unwrap();
        assert!(config.database_url_worker.is_none());
    }
    #[test]
    fn upload_limit_cannot_exceed_the_host_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        for max in [MAX_UPLOAD_BYTES, MAX_UPLOAD_BYTES - 1] {
            let config =
                Config::from_figment(valid(dir.path()).merge(("upload.max_bytes", max))).unwrap();
            assert_eq!(config.upload.max_bytes, max);
        }
        for max in [MAX_UPLOAD_BYTES + 1, 4 << 30, u64::MAX] {
            let error = Config::from_figment(valid(dir.path()).merge(("upload.max_bytes", max)))
                .unwrap_err();
            assert_eq!(
                error.problems(),
                &["upload.max_bytes: must not exceed the codec host ceiling of 2 GiB"]
            );
        }
    }
    #[test]
    fn worker_settings_are_bounded_and_overridable() {
        let dir = tempfile::tempdir().unwrap();
        let worker = Worker {
            concurrency: 4,
            codec_timeout: 120,
            vips_concurrency: 3,
            default_timezone: "America/Toronto".into(),
            ..Worker::default()
        };
        let config =
            Config::from_figment(valid(dir.path()).merge(("worker", worker.clone()))).unwrap();
        assert_eq!(config.worker.concurrency, 4);
        assert_eq!(config.worker.codec_timeout, 120);
        assert_eq!(config.worker.vips_concurrency, 3);
        assert_eq!(config.worker.default_timezone, "America/Toronto");
        for bad in [
            Worker {
                default_timezone: "invalid/timezone".into(),
                ..worker.clone()
            },
            Worker {
                concurrency: 0,
                ..worker.clone()
            },
            Worker {
                codec_timeout: 601,
                ..worker.clone()
            },
            Worker {
                vips_concurrency: 0,
                ..worker.clone()
            },
            Worker {
                codec_host_path: PathBuf::from("relative"),
                ..worker.clone()
            },
            Worker {
                metrics_listen: SocketAddr::from(([0, 0, 0, 0], 9101)),
                ..worker.clone()
            },
        ] {
            assert!(Config::from_figment(valid(dir.path()).merge(("worker", bad))).is_err());
        }
    }
    #[test]
    fn reports_all_problems_and_never_echoes_bad_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secret = uuid::Uuid::now_v7().to_string();
        let f = Figment::new()
            .merge(("data_dir", dir.path().to_string_lossy().as_ref()))
            .merge(("public_base_url", "http://example.test"))
            .merge(("session_key", secret.as_str()))
            .merge(("admin_listen", "0.0.0.0:1"));
        let err = Config::from_figment(f).unwrap_err();
        for field in [
            "database_url_public",
            "public_base_url",
            "session_key",
            "admin_listen",
        ] {
            assert_eq!(
                err.problems()
                    .iter()
                    .filter(|p| p.starts_with(field))
                    .count(),
                1
            );
        }
        assert!(!err.to_string().contains(&secret));
    }
    #[test]
    fn transfer_timeouts_accept_positive_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_figment(
            valid(dir.path())
                .merge(("write_timeout_secs", 7))
                .merge(("transfer_deadline_secs", 90)),
        )
        .unwrap();
        assert_eq!(config.write_timeout, Duration::from_secs(7));
        assert_eq!(config.transfer_deadline, Duration::from_secs(90));
    }
    #[test]
    fn every_field_is_independently_validated() {
        let dir = tempfile::tempdir().unwrap();
        for (field, value) in [
            ("public_listen", "nonsense"),
            ("admin_listen", "[::]:1"),
            ("database_url_public", "invalid"),
            ("database_url_admin", "invalid"),
            ("database_url_worker", "invalid"),
            ("admin_base_url", "http://admin.example.test"),
            ("data_dir", "/a/nonexistent/yhotos/directory"),
            ("public_base_url", "invalid"),
            ("session_key", "invalid"),
            ("trust_proxy", "invalid"),
            ("rate_limits.cheap_per_min", "0"),
            ("rate_limits.expensive_per_min", "0"),
            ("rate_limits.auth_per_min", "0"),
            ("global_stream_cap", "0"),
            ("global_stream_cap", "invalid"),
            ("global_stream_cap", "65536"),
            ("write_timeout_secs", "0"),
            ("write_timeout_secs", "invalid"),
            ("write_timeout_secs", "-1"),
            ("write_timeout_secs", "4294967296"),
            ("transfer_deadline_secs", "0"),
            ("transfer_deadline_secs", "invalid"),
            ("transfer_deadline_secs", "-1"),
            ("transfer_deadline_secs", "4294967296"),
            ("upload.max_bytes", "0"),
            ("upload.chunk_bytes", "0"),
            ("log_format", "invalid"),
        ] {
            let err = Config::from_figment(valid(dir.path()).merge((field, value))).unwrap_err();
            assert!(
                err.problems().iter().any(|p| p.starts_with(field)),
                "{field}: {err}"
            );
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(Config::from_figment(valid(file.path())).is_err());
        assert!(
            Config::from_figment(valid(dir.path()).merge(("upload.chunk_bytes", u32::MAX)))
                .is_err()
        );
    }
    #[test]
    fn missing_admin_is_feature_gated() {
        let dir = tempfile::tempdir().unwrap();
        let f = valid_without_admin(dir.path());
        assert_eq!(Config::from_figment(f).is_err(), cfg!(feature = "admin"));
        for field in ["admin_listen", "database_url_admin", "admin_base_url"] {
            let f = valid(dir.path()).merge((field, Option::<String>::None));
            assert_eq!(Config::from_figment(f).is_err(), cfg!(feature = "admin"));
        }
    }
    #[test]
    fn listener_origins_reject_credentials_queries_and_fragments() {
        let dir = tempfile::tempdir().unwrap();
        for field in ["public_base_url", "admin_base_url"] {
            for value in [
                "https://user@example.test",
                "https://example.test/?query=value",
                "https://example.test/#fragment",
            ] {
                let error =
                    Config::from_figment(valid(dir.path()).merge((field, value))).unwrap_err();
                assert!(error.problems().iter().any(|p| p.starts_with(field)));
            }
        }
    }
    #[test]
    fn admin_rejects_every_wildcard_spelling() {
        let dir = tempfile::tempdir().unwrap();
        for address in [
            "0.0.0.0:8081",
            "[::]:8081",
            "[::ffff:0.0.0.0]:8081",
            "[::ffff:0:0]:8081",
            "[0:0:0:0:0:ffff:0000:0000]:8081",
        ] {
            let error = Config::from_figment(valid(dir.path()).merge(("admin_listen", address)))
                .unwrap_err();
            assert_eq!(
                error.problems(),
                &["admin_listen: must be a specific address"],
                "{address}"
            );
        }
        for address in [
            "127.0.0.1:8081",
            "[::1]:8081",
            "[::ffff:127.0.0.1]:8081",
            "[fd00::1]:8081",
        ] {
            assert!(
                Config::from_figment(valid(dir.path()).merge(("admin_listen", address))).is_ok(),
                "{address}"
            );
        }
    }
}
