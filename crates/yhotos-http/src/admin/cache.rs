//! Immediate origin invalidation and durable, retryable edge purges.
use crate::{AppState, error::HttpError};
use secrecy::{ExposeSecret, SecretString};
use std::{future::Future, pin::Pin, process::Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yhotos_db::library;

/// Edge implementations must fail closed on transport or API errors and never log credentials.
pub trait CachePurger: Send + Sync {
    /// Purges only the supplied validated tags. Callers retain failed batches durably.
    fn purge<'a>(
        &'a self,
        tags: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<(), HttpError>> + Send + 'a>>;
}
/// Used when no edge provider is configured.
pub struct NoopPurger;
impl CachePurger for NoopPurger {
    fn purge<'a>(
        &'a self,
        _tags: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<(), HttpError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}
/// Fixed Cloudflare endpoint with no redirects, proxy inheritance, or command-line credentials.
/// Uses the deployment's curl TLS transport; missing curl leaves the durable batch pending.
pub struct CloudflarePurger {
    zone: String,
    token: SecretString,
}
impl CloudflarePurger {
    /// Validates the provider identity and credential without returning either in errors.
    /// # Errors
    /// Rejects malformed zone ids and credentials.
    pub fn new(zone: String, token: SecretString) -> Result<Self, HttpError> {
        if zone.len() != 32
            || !zone.bytes().all(|b| b.is_ascii_hexdigit())
            || token.expose_secret().is_empty()
            || token.expose_secret().len() > 512
            || !token
                .expose_secret()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(HttpError::BadRequest);
        }
        Ok(Self { zone, token })
    }
}
fn unavailable() -> HttpError {
    HttpError::Internal(anyhow::anyhow!("edge cache purge unavailable"))
}
impl CachePurger for CloudflarePurger {
    fn purge<'a>(
        &'a self,
        tags: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<(), HttpError>> + Send + 'a>> {
        Box::pin(async move {
            for batch in tags.chunks(100) {
                if batch.iter().any(|t| {
                    t.len() > 128
                        || !t
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'-')
                }) {
                    return Err(HttpError::BadRequest);
                }
                let body = serde_json::json!({"tags":batch}).to_string();
                // A single escaped config stream contains the token; argv and stderr never do.
                let input = zeroize::Zeroizing::new(format!(
                    "url = \"https://api.cloudflare.com/client/v4/zones/{}/purge_cache\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\ndata = {}\n",
                    self.zone,
                    self.token.expose_secret(),
                    serde_json::to_string(&body).map_err(|_| unavailable())?
                ));
                let mut child = tokio::process::Command::new("/usr/bin/curl")
                    .args([
                        "-q",
                        "--config",
                        "-",
                        "--proto",
                        "=https",
                        "--tlsv1.2",
                        "--noproxy",
                        "*",
                        "--max-time",
                        "5",
                        "--fail",
                        "--silent",
                        "--max-filesize",
                        "65536",
                    ])
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|_| unavailable())?;
                let mut stdin = child.stdin.take().ok_or_else(unavailable)?;
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|_| unavailable())?;
                drop(stdin);
                let mut output = Vec::new();
                child
                    .stdout
                    .take()
                    .ok_or_else(unavailable)?
                    .take(65537)
                    .read_to_end(&mut output)
                    .await
                    .map_err(|_| unavailable())?;
                if output.len() > 65536
                    || !child.wait().await.map_err(|_| unavailable())?.success()
                    || serde_json::from_slice::<serde_json::Value>(&output)
                        .ok()
                        .and_then(|v| v.get("success").and_then(serde_json::Value::as_bool))
                        != Some(true)
                {
                    return Err(unavailable());
                }
            }
            Ok(())
        })
    }
}
/// Origin snapshots are invalidated before responding. The durable outbox is drained in
/// the admin listener's background task so edge latency never blocks a mutation.
pub(crate) fn changed(state: &AppState) {
    state.files.invalidate_all();
    if let Some(auth) = &state.auth {
        auth.invalidate_all();
    }
}
pub(crate) async fn drain(state: &AppState) -> Result<(), HttpError> {
    let pool = super::pool(state)?;
    let pending = library::pending_purges(&mut *pool.acquire().await?).await?;
    for (id, tags) in pending {
        if state.cache_purger.purge(&tags).await.is_err() {
            tracing::warn!(purge_id=%id,"edge purge deferred");
            break;
        }
        library::purge_done(&mut *pool.acquire().await?, id).await?;
    }
    Ok(())
}
