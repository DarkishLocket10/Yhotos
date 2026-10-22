#![forbid(unsafe_code)]

//! Checked PostgreSQL repositories. Callers must authenticate identities and
//! evaluate `yhotos_core::access` before exposing any fetched content.

pub mod albums;
pub mod flags;
pub mod grants;
pub mod groups;
pub mod jobs;
pub mod likes;
pub mod photos;
pub mod redirects;
pub mod settings;
pub mod share_links;
pub mod tags;
pub mod users;

use secrecy::{ExposeSecret, SecretString};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{str::FromStr, time::Duration};

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
    ///
    /// # Errors
    /// Rejects invalid URLs, zero pool size, unavailable databases or missing roles.
    pub async fn connect(url: &SecretString, role: Role, max_conns: u32) -> Result<Self, DbError> {
        if max_conns == 0 {
            return Err(DbError::InvalidInput);
        }
        let (name, timeout) = match role {
            Role::Public => ("yhotos_public", "5000"),
            Role::Admin => ("yhotos_admin", "30000"),
            Role::Worker => ("yhotos_worker", "60000"),
        };
        let options = PgConnectOptions::from_str(url.expose_secret())?
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
