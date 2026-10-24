//! Startup resources shared by all requests and router clones.

use crate::{auth::Auth, files::FileCache};
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
    /// Shared authentication services; absent only in isolated handler tests.
    pub auth: Option<Arc<Auth>>,
    /// Role-scoped PostgreSQL connections for cold file lookups.
    pub pool: PgPool,
    /// Canonical content-addressed storage root.
    pub store: Store,
    /// Bounded metadata cache and original/video stream accounting across clones.
    pub files: FileCache,
    /// Admin-only edge invalidation provider, replaceable with a test mock.
    #[cfg(feature = "admin")]
    pub cache_purger: Arc<dyn crate::admin::cache::CachePurger>,
    /// Bounded query embedding cache and internal network client.
    pub search: crate::search::Client,
}
impl Deref for AppState {
    type Target = AppResources;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AppState {
    #[cfg(feature = "admin")]
    pub(crate) fn start_cache_worker(&self) {
        let weak = Arc::downgrade(&self.0);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                loop {
                    let Some(resources) = weak.upgrade() else {
                        break;
                    };
                    if crate::admin::cache::drain(&Self(resources)).await.is_err() {
                        tracing::warn!("cache purge retry deferred");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        }
    }
    /// Shares metadata and stream caps: four per IP and the configured global cap.
    #[must_use]
    pub fn new(
        config: &'static Config,
        pool: PgPool,
        store: Store,
        auth: Option<Arc<Auth>>,
    ) -> Self {
        #[cfg(feature = "admin")]
        let cache_purger: Arc<dyn crate::admin::cache::CachePurger> =
            match (&config.cloudflare_zone, &config.cloudflare_api_token) {
                (Some(zone), Some(token)) => {
                    match crate::admin::cache::CloudflarePurger::new(zone.clone(), token.clone()) {
                        Ok(purger) => Arc::new(purger),
                        Err(_) => Arc::new(crate::admin::cache::NoopPurger),
                    }
                }
                _ => Arc::new(crate::admin::cache::NoopPurger),
            };
        Self::from(AppResources {
            config,
            auth,
            pool,
            store,
            files: FileCache::new(4, config.global_stream_cap),
            #[cfg(feature = "admin")]
            cache_purger,
            search: crate::search::Client::new(config.ml_address.clone()),
        })
    }
}

impl From<AppResources> for AppState {
    /// Shares supplied resources, including original/video stream caps.
    fn from(resources: AppResources) -> Self {
        Self(Arc::new(resources))
    }
}
