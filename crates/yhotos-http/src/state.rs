//! Startup resources shared by all requests and router clones.

use crate::files::FileCache;
use sqlx::PgPool;
use std::{ops::Deref, sync::Arc};
use yhotos_config::Config;
use yhotos_storage::Store;

/// Immutable startup handles; each request clones only one reference count.
#[derive(Clone)]
pub struct AppState(Arc<AppResources>);

/// Resources borrowed through `AppState`, without cloning store paths or caches.
pub struct AppResources {
    /// Leaked once at startup so every handler borrows the same configuration.
    pub config: &'static Config,
    /// Role-scoped PostgreSQL connections for cold file lookups.
    pub pool: PgPool,
    /// Canonical content-addressed storage root.
    pub store: Store,
    /// Bounded metadata cache and original-stream accounting shared across clones.
    pub files: FileCache,
}
impl Deref for AppState {
    type Target = AppResources;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AppState {
    /// Shares one 60-second, 50,000-entry cache and a four-stream per-IP cap.
    #[must_use]
    pub fn new(config: &'static Config, pool: PgPool, store: Store) -> Self {
        Self::from(AppResources {
            config,
            pool,
            store,
            files: FileCache::default(),
        })
    }
}

impl From<AppResources> for AppState {
    /// Shares supplied resources, including a configured original-stream cap.
    fn from(resources: AppResources) -> Self {
        Self(Arc::new(resources))
    }
}
