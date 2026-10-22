use axum::serve::Listener;
use std::{io, net::SocketAddr, pin::Pin, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tokio_io_timeout::TimeoutStream;

/// Every accepted connection aborts a write that makes no progress in time.
pub(crate) struct WriteTimeoutListener {
    inner: TcpListener,
    timeout: Duration,
}
impl WriteTimeoutListener {
    pub(crate) fn new(inner: TcpListener, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}
impl Listener for WriteTimeoutListener {
    type Io = Pin<Box<TimeoutStream<TcpStream>>>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Delegate accept-error logging and retry to Axum's TCP listener.
        let (socket, peer) = Listener::accept(&mut self.inner).await;
        let mut socket = TimeoutStream::new(socket);
        socket.set_write_timeout(Some(self.timeout));
        (Box::pin(socket), peer)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use axum::{body::Body, extract::ConnectInfo, http::Request, serve::ListenerExt};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use figment::Figment;
    use sqlx::PgPool;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::{Instant, sleep, timeout},
    };
    use tower::ServiceExt;
    use uuid::Uuid;
    use yhotos_config::Config;
    use yhotos_core::{Slug, access::Visibility};
    use yhotos_db::{albums, photos};
    use yhotos_http::{AppState, public_router};
    use yhotos_storage::Store;

    async fn get_range(address: SocketAddr, path: &str) -> String {
        timeout(Duration::from_secs(1), async {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket
                .write_all(
                    format!(
                        "GET {path} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
        .unwrap()
    }

    async fn fixture(pool: PgPool) -> (tempfile::TempDir, AppState, String) {
        const BYTES: u64 = 64 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // Larger than TCP send/receive buffers, with bounded memory throughout.
        let put = store
            .put_original(tokio::io::repeat(b'x').take(BYTES), BYTES)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let album = albums::create(
            &mut conn,
            None,
            &Slug::new("stalled-receiver").unwrap(),
            "Stalled receiver",
            Visibility::Public,
        )
        .await
        .unwrap();
        photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Original".into(),
                original_hash: put.hash.to_string(),
                original_name: "original.jpg".into(),
                original_bytes: i64::try_from(BYTES).unwrap(),
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await
        .unwrap();
        drop(conn);
        let config = Box::leak(Box::new(
            Config::from_figment(
                Figment::new()
                    .merge(("database_url", "postgres://127.0.0.1/test"))
                    .merge(("data_dir", dir.path().to_string_lossy().as_ref()))
                    .merge(("public_base_url", "https://example.test"))
                    .merge(("admin_listen", "127.0.0.1:8081"))
                    .merge((
                        "session_key",
                        STANDARD.encode(format!("{}{}", Uuid::now_v7(), Uuid::now_v7())),
                    ))
                    .merge(("global_stream_cap", 1))
                    .merge(("write_timeout_secs", 1))
                    // A much longer body deadline ensures recovery is transport-driven.
                    .merge(("transfer_deadline_secs", 60))
                    .merge(("rate_limits.expensive_per_min", 100_000)),
            )
            .unwrap(),
        ));
        let path = format!("/o/{}/original.jpg", put.hash);
        (dir, AppState::new(config, pool, store), path)
    }

    #[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
    async fn stalled_receiver_times_out_and_releases_stream_capacity(pool: PgPool) {
        let (_dir, state, path) = fixture(pool).await;
        let write_timeout = state.config.write_timeout;
        let app = public_router(state);
        // Warm metadata before timing; do not leave a response body holding a slot.
        let mut request = Request::builder()
            .method("HEAD")
            .uri(&path)
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 200);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(
                WriteTimeoutListener::new(listener, write_timeout).tap_io(|_| {}),
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
        });
        let mut stalled = TcpStream::connect(address).await.unwrap();
        let started = Instant::now();
        stalled
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        // Never read or close `stalled` while observing capacity recovery.
        loop {
            if get_range(address, &path).await.starts_with("HTTP/1.1 429") {
                break;
            }
            assert!(
                started.elapsed() < write_timeout / 2,
                "stream never acquired capacity"
            );
            sleep(Duration::from_millis(5)).await;
        }
        loop {
            let response = get_range(address, &path).await;
            if response.starts_with("HTTP/1.1 206") {
                assert!(response.ends_with("\r\n\r\nx"), "{response}");
                break;
            }
            assert!(response.starts_with("HTTP/1.1 429"), "{response}");
            assert!(
                started.elapsed() < write_timeout + Duration::from_millis(500),
                "stalled receiver retained capacity past the write timeout"
            );
            sleep(Duration::from_millis(10)).await;
        }
        assert!(started.elapsed() < write_timeout + Duration::from_millis(500));
        // Capacity is available while the original client still holds its socket.
        assert!(get_range(address, &path).await.starts_with("HTTP/1.1 206"));
        drop(stalled);
        let _ = shutdown_tx.send(());
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
