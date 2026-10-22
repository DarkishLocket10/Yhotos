//! Digest-only caches. Epoch keys prevent in-flight reads from undoing invalidation.
use crate::{AppState, auth, error::HttpError};
use moka::future::Cache;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use yhotos_db::share_links::{self, Landing, LinkSession};
use zeroize::Zeroizing;

type Key = (u64, [u8; 32]);
type PageKey = (u64, [u8; 32], u32, yhotos_db::photos::PageSort);
/// No URL bearer or plaintext passphrase is retained in this state.
pub(crate) struct Shares {
    epoch: AtomicU64,
    links: Cache<PageKey, Option<Arc<Snapshot>>>,
    sessions: Cache<Key, Option<Arc<LinkSession>>>,
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
            links: Cache::builder()
                .max_capacity(32 * 1024 * 1024)
                .weigher(|_: &PageKey, value: &Option<Arc<Snapshot>>| {
                    value.as_ref().map_or(64, |s| {
                        u32::try_from(s.landing.bytes.saturating_add(1024)).unwrap_or(u32::MAX)
                    })
                })
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
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
    pub(crate) async fn remember_session(&self, epoch: u64, hash: [u8; 32], session: LinkSession) {
        // An invalidation during issuance leaves this unreachable old epoch.
        self.sessions
            .insert((epoch, hash), Some(Arc::new(session)))
            .await;
    }
    pub(crate) fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.links.invalidate_all();
        self.sessions.invalidate_all();
    }
    pub(crate) async fn link(
        &self,
        state: &AppState,
        hash: [u8; 32],
    ) -> Result<Option<Arc<Snapshot>>, HttpError> {
        self.page(state, hash, 1, yhotos_db::photos::PageSort::Taken)
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
                .links
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
            let session = self
                .sessions
                .try_get_with((version, hash), async {
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
            if version == self.epoch.load(Ordering::SeqCst) {
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
