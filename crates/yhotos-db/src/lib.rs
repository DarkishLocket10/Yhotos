#![forbid(unsafe_code)]

//! Checked PostgreSQL repositories. Callers must authenticate identities and
//! evaluate `yhotos_core::access` before exposing any fetched content.

pub mod albums;
pub mod audit;
pub mod flags;
pub mod grants;
pub mod groups;
pub mod jobs;
pub mod likes;
pub mod mfa;
pub mod photos;
pub mod redirects;
pub mod sessions;
pub mod settings;
pub mod share_links;
pub mod tags;
pub mod users;

use secrecy::{ExposeSecret, SecretString};
/// Connection type for composing repository calls in a transaction.
pub use sqlx::{Error as PoolError, PgConnection};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
use std::{net::IpAddr, str::FromStr, time::Duration};

/// Forward-only migrations, including required pgvector and least-privilege roles.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Process privilege boundary; each deployment supplies its own login URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Public reads and narrowly scoped interaction writes.
    Public,
    /// Administrative writes and migration startup.
    Admin,
    /// Pipeline writes without account, settings or audit access.
    Worker,
}

/// Transport policy applied after parsing the URL, so URL options cannot weaken it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DatabaseTls {
    /// Requires TLS with certificate and hostname verification (`verify-full`).
    /// Uses `sslrootcert` from the connection URL when configured, otherwise trusted roots.
    #[default]
    Require,
    /// Tries TLS but permits plaintext fallback; only for explicitly opted-in development.
    Prefer,
    /// Permits plaintext only through a Unix socket or a literal loopback IP address.
    Disable,
}

impl DatabaseTls {
    fn configure(self, options: PgConnectOptions) -> Result<PgConnectOptions, DbError> {
        let mode = match self {
            Self::Require => PgSslMode::VerifyFull,
            Self::Prefer => PgSslMode::Prefer,
            Self::Disable => {
                let host = options.get_host();
                // SQLx also interprets an absolute host path as a Unix socket directory.
                let socket = options.get_socket().is_some() || host.starts_with('/');
                let host = host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host);
                if !socket && !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
                    return Err(DbError::InsecureTransport);
                }
                PgSslMode::Disable
            }
        };
        Ok(options.ssl_mode(mode))
    }
}

/// Errors have generic display messages; database details remain available to
/// trusted callers via the error source, never in public responses.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A database statement or connection failed.
    #[error("database operation failed")]
    Sqlx(#[from] sqlx::Error),
    /// A forward migration failed.
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// Only an administrative handle may migrate.
    #[error("administrative database role required")]
    AdminRequired,
    /// Invalid caller input was rejected before issuing SQL.
    #[error("invalid database input")]
    InvalidInput,
    /// Plaintext TCP was requested for a host that is not a literal loopback address.
    #[error("database TLS may only be disabled for local connections")]
    InsecureTransport,
}

/// A bounded connection pool configured for one process privilege boundary.
pub struct Db {
    pool: PgPool,
    role: Role,
}
impl Db {
    /// Connects without exposing the URL; configures every new connection.
    /// Public and worker pools explicitly assume their NOLOGIN role to prevent
    /// accidental excess privileges inherited by a deployment login.
    /// `DatabaseTls::default()` requires verified TLS; local development must opt
    /// into `Disable` explicitly. Configured `sslrootcert` is preserved.
    ///
    /// # Errors
    /// Rejects invalid URLs, zero pool size, insecure remote transport, unavailable
    /// databases, certificate verification failures or missing roles.
    pub async fn connect(
        url: &SecretString,
        role: Role,
        max_conns: u32,
        tls: DatabaseTls,
    ) -> Result<Self, DbError> {
        if max_conns == 0 {
            return Err(DbError::InvalidInput);
        }
        let (name, timeout) = match role {
            Role::Public => ("yhotos_public", "5000"),
            Role::Admin => ("yhotos_admin", "30000"),
            Role::Worker => ("yhotos_worker", "60000"),
        };
        let options = tls
            .configure(PgConnectOptions::from_str(url.expose_secret())?)?
            .application_name(name)
            .options([
                ("statement_timeout", timeout),
                ("idle_in_transaction_session_timeout", "30000"),
            ]);
        let pool = PgPoolOptions::new()
            .max_connections(max_conns)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    match role {
                        Role::Public => {
                            sqlx::query!("SET ROLE yhotos_public").execute(conn).await?;
                        }
                        Role::Worker => {
                            sqlx::query!("SET ROLE yhotos_worker").execute(conn).await?;
                        }
                        // The migration login needs CREATE EXTENSION/CREATEROLE.
                        // Production admin logins otherwise inherit yhotos_admin.
                        Role::Admin => {}
                    }
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        Ok(Self { pool, role })
    }
    /// Applies migrations idempotently using the administrative deployment login.
    ///
    /// # Errors
    /// Rejects non-admin handles and failed migrations.
    pub async fn migrate(&self) -> Result<(), DbError> {
        if self.role != Role::Admin {
            return Err(DbError::AdminRequired);
        }
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }
    /// Borrows the pool for transactions and repository calls.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// Pages are zero-based and bounded before they reach a LIMIT/OFFSET.
fn page(page: u32, per_page: u32) -> Result<(i64, i64), DbError> {
    if !(1..=200).contains(&per_page) {
        return Err(DbError::InvalidInput);
    }
    Ok((i64::from(per_page), i64::from(page) * i64::from(per_page)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sqlx::ConnectOptions;

    #[test]
    fn tls_policy_overrides_url_mode_and_preserves_root_certificate() {
        let options = PgConnectOptions::from_str(
            "postgres://db.example.invalid/gallery?sslmode=disable&sslrootcert=%2Fconfigured%2Fca.pem",
        )
        .unwrap();
        let root_certificate = |options: &PgConnectOptions| {
            options
                .to_url_lossy()
                .query_pairs()
                .find(|(key, _)| key == "sslrootcert")
                .map(|(_, value)| value.into_owned())
        };
        let configured_root = root_certificate(&options);
        assert!(configured_root.is_some());
        let required = DatabaseTls::default().configure(options.clone()).unwrap();
        assert!(matches!(required.get_ssl_mode(), PgSslMode::VerifyFull));
        assert_eq!(root_certificate(&required), configured_root);
        assert!(matches!(
            DatabaseTls::Prefer
                .configure(options)
                .unwrap()
                .get_ssl_mode(),
            PgSslMode::Prefer
        ));
    }

    #[test]
    fn plaintext_policy_checks_effective_host_without_dns() {
        for url in [
            "postgres://127.0.0.1/gallery",
            "postgres://127.1.2.3/gallery",
            "postgres://[::1]/gallery",
            "postgres://%2Fvar%2Frun%2Fpostgresql/gallery",
            "postgres:///gallery?host=%2Fvar%2Frun%2Fpostgresql",
            "postgres://db.example.invalid/gallery?hostaddr=127.0.0.1",
        ] {
            let options = PgConnectOptions::from_str(url).unwrap();
            assert!(
                matches!(
                    DatabaseTls::Disable
                        .configure(options)
                        .unwrap()
                        .get_ssl_mode(),
                    PgSslMode::Disable
                ),
                "{url}"
            );
        }
        for url in [
            "postgres://192.0.2.1/gallery",
            "postgres://[2001:db8::1]/gallery",
            "postgres://db.example.invalid/gallery",
            "postgres://localhost/gallery",
            "postgres://127.0.0.1/gallery?host=192.0.2.1",
            "postgres://127.0.0.1/gallery?hostaddr=192.0.2.1",
        ] {
            let options = PgConnectOptions::from_str(url).unwrap();
            assert!(
                matches!(
                    DatabaseTls::Disable.configure(options),
                    Err(DbError::InsecureTransport)
                ),
                "{url}"
            );
        }
    }
}
