//! Digest-only caches. Epoch keys prevent in-flight reads from undoing invalidation.
use crate::{AppState, auth, error::HttpError};
use moka::future::Cache;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use yhotos_db::share_links::{self, Landing, LinkSession, SessionLink};
use zeroize::Zeroizing;

type PageKey = (u64, [u8; 32], u32, yhotos_db::photos::PageSort);
/// No URL bearer or plaintext passphrase is retained in this state.
pub(crate) struct Shares {
    epoch: AtomicU64,
    pages: Cache<PageKey, Option<Arc<Snapshot>>>,
    scopes: Cache<(u64, yhotos_core::ShareLinkId), Arc<SessionLink>>,
    session_epoch: AtomicU64,
    session_versions: moka::sync::Cache<[u8; 32], Arc<AtomicU64>>,
    sessions: Cache<(u64, u64, [u8; 32]), Option<Arc<LinkSession>>>,
    salt: Mutex<Salt>,
}
struct Salt {
    day: i64,
    bytes: Zeroizing<[u8; 32]>,
}
pub(crate) struct Snapshot {
    pub landing: Landing,
}
impl Shares {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            pages: Cache::builder()
                .max_capacity(32 * 1024 * 1024)
                .weigher(|_: &PageKey, value: &Option<Arc<Snapshot>>| {
                    value.as_ref().map_or(64, |s| {
                        u32::try_from(s.landing.bytes.saturating_add(1024)).unwrap_or(u32::MAX)
                    })
                })
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
            scopes: Cache::builder()
                .max_capacity(16384)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
            session_epoch: AtomicU64::new(0),
            session_versions: moka::sync::Cache::builder().max_capacity(16384).build(),
            sessions: Cache::builder()
                .max_capacity(16384)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
            salt: Mutex::new(Salt {
                day: i64::MIN,
                bytes: Zeroizing::new([0; 32]),
            }),
        }
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub(crate) fn session_revision(&self, hash: [u8; 32]) -> u64 {
        self.session_version(hash);
        self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn session_version(&self, hash: [u8; 32]) -> Arc<AtomicU64> {
        self.session_versions.get_with(hash, || {
            Arc::new(AtomicU64::new(
                self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1,
            ))
        })
    }
    pub(crate) async fn remember_session(
        &self,
        epoch: u64,
        revision: u64,
        hash: [u8; 32],
        session: LinkSession,
    ) {
        let version = self.session_version(hash);
        if version.fetch_max(revision, Ordering::SeqCst) > revision {
            return;
        }
        // Revisions are reserved while the transaction holds the session lock.
        // A delayed older commit cannot replace a more recent LRU snapshot.
        self.sessions
            .insert((epoch, revision, hash), Some(Arc::new(session)))
            .await;
    }
    pub(crate) fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.pages.invalidate_all();
        self.scopes.invalidate_all();
        self.sessions.invalidate_all();
    }
    pub(crate) async fn scope(&self, link: &SessionLink) -> Arc<SessionLink> {
        self.scopes
            .get_with((self.epoch(), link.link_id), async {
                Arc::new(link.clone())
            })
            .await
    }
    pub(crate) async fn page(
        &self,
        state: &AppState,
        hash: [u8; 32],
        page: u32,
        sort: yhotos_db::photos::PageSort,
    ) -> Result<Option<Arc<Snapshot>>, HttpError> {
        if !auth::required(state)?.caching() {
            self.invalidate();
        }
        loop {
            let version = self.epoch.load(Ordering::SeqCst);
            let snapshot = self
                .pages
                .try_get_with((version, hash, page, sort), async {
                    let mut c = auth::required(state)?
                        .db(auth::Audience(false))?
                        .pool()
                        .acquire()
                        .await?;
                    let row = share_links::landing(&mut c, &hash, page, sort).await?;
                    row.filter(|landing| {
                        bool::from(landing.link.token_hash.as_slice().ct_eq(&hash))
                    })
                    .map(|landing| {
                        if landing
                            .link
                            .passphrase_hash
                            .as_ref()
                            .is_some_and(|h| yhotos_auth::PasswordHash::parse(h).is_err())
                        {
                            return Err(HttpError::Internal(anyhow::anyhow!(
                                "invalid stored share metadata"
                            )));
                        }
                        Ok(Arc::new(Snapshot { landing }))
                    })
                    .transpose()
                })
                .await
                .map_err(|_: Arc<HttpError>| {
                    HttpError::Internal(anyhow::anyhow!("share lookup failed"))
                })?;
            if version == self.epoch.load(Ordering::SeqCst) {
                if let Some(snapshot) = &snapshot {
                    let scope = SessionLink::from(&snapshot.landing.link);
                    self.scopes
                        .insert((version, scope.link_id), Arc::new(scope))
                        .await;
                }
                return Ok(snapshot);
            }
        }
    }
    pub(crate) async fn session(
        &self,
        state: &AppState,
        hash: [u8; 32],
    ) -> Result<Option<Arc<LinkSession>>, HttpError> {
        if !auth::required(state)?.caching() {
            self.invalidate();
        }
        loop {
            let version = self.epoch.load(Ordering::SeqCst);
            let session_version = self.session_version(hash);
            let session_epoch = session_version.load(Ordering::SeqCst);
            let session = self
                .sessions
                .try_get_with((version, session_epoch, hash), async {
                    let mut c = auth::required(state)?
                        .db(auth::Audience(false))?
                        .pool()
                        .acquire()
                        .await?;
                    Ok::<_, HttpError>(
                        share_links::load_session(&mut c, &hash)
                            .await?
                            .map(Arc::new),
                    )
                })
                .await
                .map_err(|_| HttpError::Internal(anyhow::anyhow!("share session lookup failed")))?;
            if version == self.epoch.load(Ordering::SeqCst)
                && session_epoch == session_version.load(Ordering::SeqCst)
            {
                if session.as_ref().is_some_and(|s| {
                    s.links.iter().any(|l| {
                        l.expires_at
                            .is_some_and(|t| t <= OffsetDateTime::now_utc().unix_timestamp())
                    })
                }) {
                    // Expiry is checked even on a warm session. Reload only the
                    // small grant set so expired memberships are also pruned.
                    self.sessions
                        .invalidate(&(version, session_epoch, hash))
                        .await;
                    continue;
                }
                return Ok(session);
            }
        }
    }
    pub(crate) fn visitor(
        &self,
        ip: std::net::IpAddr,
        ua: &str,
        now: OffsetDateTime,
    ) -> Result<[u8; 32], HttpError> {
        let mut salt = self
            .salt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let day = now.unix_timestamp().div_euclid(86400);
        if day != salt.day {
            getrandom::fill(salt.bytes.as_mut())
                .map_err(|_| HttpError::Internal(anyhow::anyhow!("visitor entropy unavailable")))?;
            salt.day = day;
        }
        let mut h = blake3::Hasher::new();
        h.update(ip.to_string().as_bytes());
        h.update(&[0]);
        h.update(ua.as_bytes());
        h.update(salt.bytes.as_ref());
        Ok(*h.finalize().as_bytes())
    }
}
