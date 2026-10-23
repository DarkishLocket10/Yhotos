//! Startup resources shared by all requests and router clones.

use crate::{auth::Auth, files::FileCache};
use sqlx::PgPool;
use std::{ops::Deref, sync::Arc};
use yhotos_config::Config;
use yhotos_storage::Store;

/// Immutable startup handles; each request clones only one reference count.
#[derive(Clone)]
pub struct AppState(Arc<Shared>);
struct Shared {
    resources: AppResources,
    zips: Arc<crate::files::streams::Streams>,
}

/// Resources borrowed through `AppState`, without cloning store paths or caches.
pub struct AppResources {
    /// Leaked once at startup so every handler borrows the same configuration.
    pub config: &'static Config,
    /// Shared authentication services; absent only in isolated handler tests.
    pub auth: Option<Arc<Auth>>,
    /// Role-scoped PostgreSQL connections for cold file lookups.
    pub pool: PgPool,
    /// Canonical content-addressed storage root.
    pub store: Store,
    /// Bounded metadata cache and original/video stream accounting across clones.
    pub files: FileCache,
}
impl Deref for AppState {
    type Target = AppResources;
    fn deref(&self) -> &Self::Target {
        &self.0.resources
    }
}
impl AppState {
    /// Shares metadata and stream caps: four per IP and the configured global cap.
    #[must_use]
    pub fn new(
        config: &'static Config,
        pool: PgPool,
        store: Store,
        auth: Option<Arc<Auth>>,
    ) -> Self {
        Self::from(AppResources {
            config,
            auth,
            pool,
            store,
            files: FileCache::new(4, config.global_stream_cap),
        })
    }
}

impl From<AppResources> for AppState {
    /// Shares supplied resources, including original/video stream caps.
    fn from(resources: AppResources) -> Self {
        let zips = crate::files::streams::Streams::new(1, resources.config.zip.concurrency);
        Self(Arc::new(Shared { resources, zips }))
    }
}

impl AppState {
    pub(crate) fn zip_streams(&self) -> &Arc<crate::files::streams::Streams> {
        &self.0.zips
    }
}
