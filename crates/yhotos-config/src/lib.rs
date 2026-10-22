#![forbid(unsafe_code)]

//! Validates environment configuration once; secret values never enter errors.

use base64::{Engine, engine::general_purpose::STANDARD};
use figment::{
    Figment,
    providers::{Env, Serialized},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{net::SocketAddr, path::PathBuf};
use url::Url;

/// Validated process settings. Debug output redacts both secret fields.
#[derive(Debug)]
pub struct Config {
    /// Public socket to bind.
    pub public_listen: SocketAddr,
    /// Specific admin bind address, mandatory only with the admin feature.
    pub admin_listen: Option<SocketAddr>,
    /// Database connection string, never logged.
    pub database_url: SecretString,
    /// Existing storage root.
    pub data_dir: PathBuf,
    /// Canonical HTTPS origin for public links.
    pub public_base_url: Url,
    /// Base64-encoded key containing at least 32 decoded bytes.
    pub session_key: SecretString,
    /// Enables only Cloudflare's client-IP header, on tunnel deployments.
    pub trust_proxy: bool,
    /// Independent per-IP token-bucket capacities.
    pub rate_limits: RateLimits,
    /// Positive concurrent-stream cap shared by originals and video (default 64).
    pub global_stream_cap: u16,
    /// Maximum total upload and chunk lengths.
    pub upload: Upload,
    /// Structured or interactive tracing output.
    pub log_format: LogFormat,
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
    /// Maximum complete upload size.
    pub max_bytes: u64,
    /// Maximum individual chunk size.
    pub chunk_bytes: u32,
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
    trust_proxy: bool,
    rate_limits: RateLimits,
    global_stream_cap: u16,
    upload: Upload,
    log_format: LogFormat,
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            public_listen: "0.0.0.0:8080",
            admin_listen: None,
            trust_proxy: false,
            rate_limits: RateLimits {
                cheap_per_min: 600,
                expensive_per_min: 30,
                auth_per_min: 10,
            },
            global_stream_cap: 64,
            upload: Upload {
                max_bytes: 2 * 1024 * 1024 * 1024,
                chunk_bytes: 8 * 1024 * 1024,
            },
            log_format: LogFormat::Json,
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
        let database_url = read::<SecretString>(&f, "database_url", &mut errors);
        let data_dir = read::<PathBuf>(&f, "data_dir", &mut errors);
        let public_base_url = read::<Url>(&f, "public_base_url", &mut errors);
        let session_key = read::<SecretString>(&f, "session_key", &mut errors);
        let trust_proxy = read::<bool>(&f, "trust_proxy", &mut errors);
        let cheap = read::<u32>(&f, "rate_limits.cheap_per_min", &mut errors);
        let expensive = read::<u32>(&f, "rate_limits.expensive_per_min", &mut errors);
        let auth = read::<u32>(&f, "rate_limits.auth_per_min", &mut errors);
        let global_stream_cap = read::<u16>(&f, "global_stream_cap", &mut errors);
        let max_bytes = read::<u64>(&f, "upload.max_bytes", &mut errors);
        let chunk_bytes = read::<u32>(&f, "upload.chunk_bytes", &mut errors);
        let log_format = read::<LogFormat>(&f, "log_format", &mut errors);
        if let Some(Some(addr)) = admin_listen
            && addr.ip().to_canonical().is_unspecified()
        {
            errors.push("admin_listen: must be a specific address".into());
        }
        #[cfg(feature = "admin")]
        if matches!(admin_listen, Some(None)) {
            errors.push("admin_listen: required with the admin feature".into());
        }
        if let Some(url) = &database_url
            && !Url::parse(url.expose_secret()).is_ok_and(|u| {
                matches!(u.scheme(), "postgres" | "postgresql") && u.host_str().is_some()
            })
        {
            errors.push("database_url: must be a PostgreSQL URL".into());
        }
        if let Some(path) = &data_dir
            && !path.is_dir()
        {
            errors.push("data_dir: must exist and be a directory".into());
        }
        if let Some(url) = &public_base_url
            && (url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some())
        {
            errors.push("public_base_url: must be an HTTPS URL without credentials".into());
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
        if global_stream_cap == Some(0) {
            errors.push("global_stream_cap: must be positive".into());
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
            database_url,
            data_dir,
            public_base_url,
            session_key,
            trust_proxy,
            cheap,
            expensive,
            auth,
            global_stream_cap,
            max_bytes,
            chunk_bytes,
            log_format,
        ) {
            (
                Some(public_listen),
                Some(admin_listen),
                Some(database_url),
                Some(data_dir),
                Some(public_base_url),
                Some(session_key),
                Some(trust_proxy),
                Some(cheap_per_min),
                Some(expensive_per_min),
                Some(auth_per_min),
                Some(global_stream_cap),
                Some(max_bytes),
                Some(chunk_bytes),
                Some(log_format),
            ) => Ok(Self {
                public_listen,
                admin_listen,
                database_url,
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
                upload: Upload {
                    max_bytes,
                    chunk_bytes,
                },
                log_format,
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
                "database_url",
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
        valid_without_admin(dir).merge(("admin_listen", "127.0.0.1:8081"))
    }
    #[test]
    fn defaults_and_secret_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_figment(valid(dir.path())).unwrap();
        assert!(!config.trust_proxy);
        assert_eq!(config.public_listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.rate_limits.cheap_per_min, 600);
        assert_eq!(config.global_stream_cap, 64);
        assert_eq!(config.upload.max_bytes, 2 * 1024 * 1024 * 1024);
        let debug = format!("{config:?}");
        assert!(!debug.contains(config.database_url.expose_secret()));
        assert!(!debug.contains(config.session_key.expose_secret()));
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
            "database_url",
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
    fn every_field_is_independently_validated() {
        let dir = tempfile::tempdir().unwrap();
        for (field, value) in [
            ("public_listen", "nonsense"),
            ("admin_listen", "[::]:1"),
            ("database_url", "invalid"),
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
