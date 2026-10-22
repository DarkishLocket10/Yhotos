#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Real PostgreSQL, real codec-host and full authenticated administrative HTTP stack.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use sqlx::{ConnectOptions, PgPool};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_auth::{Token, TokenKind};
use yhotos_core::{AlbumId, Slug, UserId, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, mfa, sessions, users};
use yhotos_http::{AppState, auth::Auth, middleware::csrf::CsrfToken};
struct Client {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
    pool: PgPool,
    cookie: String,
    csrf: String,
    album: AlbumId,
    user: UserId,
}
impl Client {
    #[allow(clippy::too_many_lines)] // Builds isolated authentication, storage and database fixtures.
    async fn new(pool: PgPool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = SecretString::from(pool.connect_options().to_url_lossy().to_string());
        let db = Arc::new(
            Db::connect(&url, Role::Admin, 8, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let mut key = [0; 32];
        getrandom_fill(&mut key);
        let config = Box::leak(Box::new(
            yhotos_config::Config::from_figment(
                figment::Figment::new()
                    .merge(("database_url_public", url.expose_secret()))
                    .merge(("database_url_admin", url.expose_secret()))
                    .merge(("data_dir", dir.path().to_string_lossy().as_ref()))
                    .merge(("public_base_url", "https://example.test"))
                    .merge(("admin_base_url", "https://admin.example.test"))
                    .merge(("admin_listen", "127.0.0.1:8081"))
                    .merge(("session_key", STANDARD.encode(key)))
                    .merge(("upload.max_bytes", 65536))
                    .merge(("upload.chunk_bytes", 8192))
                    .merge((
                        "worker.codec_host_path",
                        env!("CARGO_BIN_EXE_yhotos-codec-host"),
                    ))
                    .merge(("rate_limits.cheap_per_min", 10000)),
            )
            .unwrap(),
        ));
        let auth = Auth::new_for_tests(config, db.clone(), Some(db.clone()))
            .await
            .unwrap();
        let state = AppState::new(
            config,
            db.pool().clone(),
            yhotos_storage::Store::open(dir.path()).unwrap(),
            Some(auth),
        );
        let mut conn = pool.acquire().await.unwrap();
        let password = yhotos_auth::generate_temp_password();
        let hash = yhotos_auth::PasswordHasher::for_tests()
            .hash_password(&password)
            .unwrap();
        let user = users::create(
            &mut conn,
            &Uuid::now_v7().simple().to_string(),
            &SecretString::from(hash.as_str()),
            false,
        )
        .await
        .unwrap()
        .id;
        sqlx::query!("UPDATE users SET is_admin=true WHERE id=$1", user.0)
            .execute(&mut *conn)
            .await
            .unwrap();
        let secret = yhotos_auth::totp::generate_secret();
        mfa::add(
            &mut conn,
            user,
            &mfa::Method {
                id: Uuid::now_v7(),
                kind: "totp".into(),
                secret_enc: Some(secret.expose_secret().to_vec()),
                credential_id: None,
                public_key: None,
                sign_count: 0,
                label: "Test".into(),
                rp_id: None,
                last_used_step: None,
            },
        )
        .await
        .unwrap();
        let token = Token::generate();
        let now = time::OffsetDateTime::now_utc();
        sessions::create(
            &mut conn,
            &sessions::Session {
                id: Uuid::now_v7(),
                user_id: user,
                admin: true,
                created_at: now,
                last_seen_at: now,
                expires_at: now + time::Duration::hours(1),
                reauth_at: Some(now),
                mfa_at: Some(now),
                ip_hash: vec![0; 32],
                ua_class: "other".into(),
                token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();
        let album = albums::create(
            &mut conn,
            None,
            &Slug::from_title(&Uuid::now_v7().to_string()),
            "Upload",
            Visibility::Public,
        )
        .await
        .unwrap()
        .id;
        let csrf = CsrfToken::generate().unwrap().form_value().to_owned();
        let cookie = format!(
            "ya={}; __Host-yhotos-csrf={csrf}",
            token.encode(TokenKind::Session)
        );
        Self {
            app: yhotos_http::admin_router(state.clone()),
            state,
            _dir: dir,
            pool,
            cookie,
            csrf,
            album,
            user,
        }
    }
    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Response {
        self.send_on(self.app.clone(), method, path, headers, body)
            .await
    }
    async fn send_on(
        &self,
        app: Router,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &self.cookie)
            .header("origin", "https://admin.example.test")
            .header("sec-fetch-site", "same-origin")
            .header("x-csrf-token", &self.csrf)
            .header("tus-resumable", "1.0.0");
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let mut req = req.body(body).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
        app.oneshot(req).await.unwrap()
    }
    async fn create(&self, len: usize, name: &str) -> Response {
        self.send(
            "POST",
            "/admin/upload",
            &[
                ("upload-length", len.to_string()),
                (
                    "upload-metadata",
                    format!(
                        "filename {},album {},filetype {}",
                        STANDARD.encode(name),
                        STANDARD.encode(self.album.to_string()),
                        STANDARD.encode("image/png")
                    ),
                ),
            ],
            Body::empty(),
        )
        .await
    }
    async fn start(&self, len: usize) -> String {
        let r = self.create(len, "original.jpg").await;
        assert_eq!(r.status(), StatusCode::CREATED);
        r.headers()["location"].to_str().unwrap().into()
    }
    async fn patch(&self, url: &str, offset: usize, bytes: &[u8]) -> Response {
        self.send(
            "PATCH",
            url,
            &[
                ("content-type", "application/offset+octet-stream".into()),
                ("upload-offset", offset.to_string()),
            ],
            Body::from(bytes.to_vec()),
        )
        .await
    }
    async fn head(&self, url: &str) -> HeaderMap {
        let r = self.send("HEAD", url, &[], Body::empty()).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.headers().clone()
    }
    async fn upload(&self, bytes: &[u8], name: &str) -> Response {
        let r = self.create(bytes.len(), name).await;
        if r.status() != StatusCode::CREATED {
            return r;
        }
        let url = r.headers()["location"].to_str().unwrap();
        self.patch(url, 0, bytes).await
    }
    async fn no_pending(&self) {
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM uploads")
                .fetch_one(&self.pool)
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(self.state.store.list_uploads().await.unwrap(), []);
        if let Ok(entries) = std::fs::read_dir(self.state.config.data_dir.join("tmp")) {
            assert_eq!(entries.count(), 0);
        }
    }
    fn use_host(&mut self, executable: PathBuf, timeout: u32) {
        let mut config = config_for_host(self, executable);
        config.worker.codec_timeout = timeout;
        self.state = AppState::new(
            Box::leak(Box::new(config)),
            self.pool.clone(),
            self.state.store.clone(),
            self.state.auth.clone(),
        );
        self.app = yhotos_http::admin_router(self.state.clone());
    }
    async fn retained(&self, url: &str, bytes: &[u8]) {
        assert_eq!(
            self.head(url).await["upload-offset"],
            bytes.len().to_string()
        );
        let uploads = self.state.store.list_uploads().await.unwrap();
        assert_eq!(uploads.len(), 1);
        let path = self
            .state
            .store
            .upload_path_for_host(&uploads[0].0)
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM photos")
                .fetch_one(&self.pool)
                .await
                .unwrap(),
            Some(0)
        );
    }
}
fn getrandom_fill(bytes: &mut [u8]) {
    let secret = Token::generate();
    bytes.copy_from_slice(secret.hash(TokenKind::Session).as_bytes());
}
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/images")
            .join(name),
    )
    .unwrap()
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_protocol_chunks_completion_dedupe_and_status(pool: PgPool) {
    let c = Client::new(pool).await;
    let page = c.send("GET", "/admin/upload", &[], Body::empty()).await;
    assert_eq!(page.status(), StatusCode::OK);
    let body = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("data-chunk-bytes=\"8192\""));
    assert!(body.contains("nonce="));
    assert!(body.contains(&c.album.to_string()));
    let options = c.send("OPTIONS", "/admin/upload", &[], Body::empty()).await;
    assert_eq!(
        options.headers()["tus-extension"],
        "creation,termination,expiration"
    );
    let bytes = fixture("chart-srgb.png");
    let url = c.start(bytes.len()).await;
    let head = c.head(&url).await;
    assert_eq!(head["upload-offset"], "0");
    assert_eq!(
        head["upload-metadata"],
        format!(
            "filename {},album {},filetype {}",
            STANDARD.encode("original.jpg"),
            STANDARD.encode(c.album.to_string()),
            STANDARD.encode("image/png")
        )
    );

    assert_eq!(c.head(&url).await["upload-length"], bytes.len().to_string());
    let a = bytes.len() / 3;
    let b = a * 2;
    assert_eq!(
        c.patch(&url, 1, &bytes[..a]).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        c.patch(&url, 0, &bytes[..a]).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(c.head(&url).await["upload-offset"], a.to_string());
    assert_eq!(
        c.patch(&url, a, &bytes[a..b]).await.status(),
        StatusCode::NO_CONTENT
    );
    let r = c.patch(&url, b, &bytes[b..]).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers()["upload-result"], "created");
    let location = r.headers()["location"].to_str().unwrap().to_owned();
    let status = c
        .send("GET", &format!("{location}/status"), &[], Body::empty())
        .await;
    assert_eq!(status.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&status.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["status"], "pending");
    let duplicate = c.upload(&bytes, "again.png").await;
    assert_eq!(duplicate.headers()["upload-result"], "duplicate");
    assert_eq!(duplicate.headers()["location"], location);
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    let job = sqlx::query!("SELECT payload FROM jobs WHERE kind='ingest'")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(job.payload["generation"], 0);
    let id: yhotos_core::PhotoId = serde_json::from_value(job.payload["photo_id"].clone()).unwrap();
    yhotos_db::photos::soft_delete(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap();
    let duplicate = c.upload(&bytes, "trashed-original.png").await;
    assert_eq!(duplicate.headers()["upload-result"], "duplicate");
    assert_eq!(duplicate.headers()["location"], location);
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_limits_headers_and_chunked_overflow(pool: PgPool) {
    let c = Client::new(pool).await;
    assert_eq!(
        c.create(65537, "large.jpg").await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let r = c
        .send(
            "POST",
            "/admin/upload",
            &[("tus-resumable", "0.2.0".into())],
            Body::empty(),
        )
        .await;
    assert_eq!(r.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(r.headers()["tus-version"], "1.0.0");
    let url = c.start(20000).await;
    assert_eq!(
        c.send(
            "PATCH",
            &url,
            &[("upload-offset", "0".into())],
            Body::empty()
        )
        .await
        .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        c.patch(&url, 0, &vec![0; 8193]).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(c.head(&url).await["upload-offset"], "0");
    assert_eq!(
        c.send(
            "POST",
            &url,
            &[("x-http-method-override", "DELETE".into())],
            Body::empty()
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    c.no_pending().await;
}

struct Interrupted {
    step: u8,
}
impl futures_core::Stream for Interrupted {
    type Item = Result<axum::body::Bytes, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.step += 1;
        std::task::Poll::Ready(match self.step {
            1 => Some(Ok(axum::body::Bytes::from(vec![42; 1024]))),
            2 => Some(Err(std::io::Error::other("client disconnected"))),
            _ => None,
        })
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_interrupted_chunk_resumes_from_fsynced_database_offset_and_concurrent_patch_serializes(
    pool: PgPool,
) {
    let c = Client::new(pool).await;
    let bytes = fixture("chart-srgb.png");
    let url = c.start(bytes.len()).await;
    assert_eq!(
        c.patch(&url, 0, &bytes[..100]).await.status(),
        StatusCode::NO_CONTENT
    );
    let r = c
        .send(
            "PATCH",
            &url,
            &[
                ("content-type", "application/offset+octet-stream".into()),
                ("upload-offset", "100".into()),
            ],
            Body::from_stream(Interrupted { step: 0 }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(c.head(&url).await["upload-offset"], "100");
    let (a, b) = tokio::join!(
        c.patch(&url, 100, &bytes[100..200]),
        c.patch(&url, 100, &bytes[100..200])
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [204, 409]);
    let r = c.patch(&url, 200, &bytes[200..]).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    c.no_pending().await;
}

// Signals only once PATCH has acquired its upload lock and started reading the body.
struct GatedChunk {
    bytes: Option<axum::body::Bytes>,
    started: Option<tokio::sync::oneshot::Sender<()>>,
    release: tokio::sync::oneshot::Receiver<()>,
}
impl futures_core::Stream for GatedChunk {
    type Item = Result<axum::body::Bytes, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.bytes.is_none() {
            return std::task::Poll::Ready(None);
        }
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        std::task::ready!(std::future::Future::poll(
            std::pin::Pin::new(&mut self.release),
            cx
        ))
        .unwrap();
        std::task::Poll::Ready(self.bytes.take().map(Ok))
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_competing_final_chunks_return_one_result_and_one_conflict(pool: PgPool) {
    let c = Client::new(pool).await;
    let bytes = fixture("chart-srgb.png");
    let url = c.start(bytes.len()).await;
    assert_eq!(
        c.patch(&url, 0, &bytes[..100]).await.status(),
        StatusCode::NO_CONTENT
    );
    let (started, reading) = tokio::sync::oneshot::channel();
    let (release, waiting) = tokio::sync::oneshot::channel();
    let deadline = std::time::Duration::from_secs(5);
    let headers = [
        ("content-type", "application/offset+octet-stream".into()),
        ("upload-offset", "100".into()),
    ];
    let (winner, loser) = tokio::join!(
        c.send(
            "PATCH",
            &url,
            &headers,
            Body::from_stream(GatedChunk {
                bytes: Some(bytes[100..].to_vec().into()),
                started: Some(started),
                release: waiting,
            }),
        ),
        async {
            tokio::time::timeout(deadline, reading)
                .await
                .unwrap()
                .unwrap();
            let response = tokio::time::timeout(deadline, c.patch(&url, 100, &bytes[100..])).await;
            // Release before asserting so a blocking lookup cannot hang the test.
            release.send(()).unwrap();
            response.expect("competing final PATCH waited for the upload lock")
        }
    );
    assert_eq!(winner.status(), StatusCode::NO_CONTENT);
    assert_eq!(winner.headers()["upload-result"], "created");
    assert!(
        winner.headers()["location"]
            .as_bytes()
            .starts_with(b"/admin/api/photos/")
    );
    assert_eq!(loser.status(), StatusCode::CONFLICT);
    assert!(!loser.headers().contains_key("upload-result"));
    assert_eq!(
        c.patch(&url, 100, &bytes[100..]).await.status(),
        StatusCode::CONFLICT,
        "a competitor arriving after commit must also get 409"
    );
    assert_eq!(
        c.send("DELETE", &url, &[], Body::empty()).await.status(),
        StatusCode::CONFLICT,
        "terminating a completed upload must not look like a successful cancel"
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_completed_patch_conflicts_are_owner_scoped_and_expire(pool: PgPool) {
    let c = Client::new(pool).await;
    let other = Client::new(c.pool.clone()).await;
    let bytes = fixture("chart-srgb.png");
    let mut urls = Vec::new();
    for result in ["created", "duplicate"] {
        let url = c.start(bytes.len()).await;
        let response = c.patch(&url, 0, &bytes).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["upload-result"], result);
        assert_eq!(
            c.patch(&url, 0, &bytes).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            other.patch(&url, 0, &bytes).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            c.send("HEAD", &url, &[], Body::empty()).await.status(),
            StatusCode::NOT_FOUND
        );
        urls.push(url);
    }
    assert_eq!(
        c.patch(&format!("/admin/upload/{}", Uuid::now_v7()), 0, &bytes)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let id: Uuid = urls[0].rsplit('/').next().unwrap().parse().unwrap();
    sqlx::query!(
        "UPDATE completed_uploads SET expires_at=now()-interval '1 second' WHERE id=$1",
        id
    )
    .execute(&c.pool)
    .await
    .unwrap();
    assert_eq!(
        c.patch(&urls[0], 0, &bytes).await.status(),
        StatusCode::NOT_FOUND
    );
    yhotos_pipeline::Ingest::new(
        c.state.store.clone(),
        c.pool.clone(),
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    )
    .sweep_uploads()
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM completed_uploads")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        c.patch(&urls[1], 0, &bytes).await.status(),
        StatusCode::CONFLICT
    );
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_malicious_files_rejected_wrong_extension_accepted_and_names_sanitized(pool: PgPool) {
    let c = Client::new(pool).await;
    for name in [
        "bomb-png.png",
        "huge-dims-tiff.tif",
        "polyglot-jpg-html.jpg",
        "truncated.jpg",
        "zero.jpg",
    ] {
        let r = c.upload(&fixture(&format!("malicious/{name}")), name).await;
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY, "{name}");
        c.no_pending().await;
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM photos")
                .fetch_one(&c.pool)
                .await
                .unwrap(),
            Some(0)
        );
    }
    let r = c
        .upload(&fixture("malicious/wrong-ext.png"), "../../etc/passwd")
        .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let photo = sqlx::query!("SELECT mime,original_name FROM photos")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(photo.mime, "image/jpeg");
    assert_eq!(photo.original_name, "etcpasswd");
    let url = c.create(100, &"雪é".repeat(190)).await;
    assert_eq!(url.status(), StatusCode::CREATED);
    let row = sqlx::query!("SELECT filename FROM uploads")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert!(row.filename.len() <= 255);
    assert!(row.filename.contains('雪'));
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_auth_ownership_csrf_and_expiry_sweep(pool: PgPool) {
    let c = Client::new(pool).await;
    let public = yhotos_http::public_router(c.state.clone());
    let r = c
        .send_on(public, "GET", "/admin/upload", &[], Body::empty())
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let url = c.start(100).await;
    let other = Client::new(c.pool.clone()).await;
    assert_eq!(
        other.send("HEAD", &url, &[], Body::empty()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other.patch(&url, 0, b"x").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .send("POST", &format!("{url}/finish"), &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut request = Request::builder()
        .method("DELETE")
        .uri(&url)
        .header(header::COOKIE, &c.cookie)
        .header("tus-resumable", "1.0.0")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:4321".parse::<SocketAddr>().unwrap()));
    assert_eq!(
        c.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let id: Uuid = url.rsplit('/').next().unwrap().parse().unwrap();
    sqlx::query!(
        "UPDATE uploads SET expires_at=now()-interval '1 second' WHERE id=$1",
        id
    )
    .execute(&c.pool)
    .await
    .unwrap();
    let r = c.send("HEAD", &url, &[], Body::empty()).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    yhotos_pipeline::Ingest::new(
        c.state.store.clone(),
        c.pool.clone(),
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    )
    .sweep_uploads()
    .await
    .unwrap();
    c.no_pending().await;
    sqlx::query!("UPDATE sessions SET mfa_at=NULL WHERE user_id=$1", c.user.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let r = c.send("GET", "/admin/upload", &[], Body::empty()).await;
    assert!(r.status().is_redirection());
    assert_eq!(r.headers()["location"], "/admin/setup-mfa");
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_video_identity_queues_ingest_without_decode(pool: PgPool) {
    let c = Client::new(pool).await;
    for name in ["header-only.mp4", "header-only.mov"] {
        let r = c.upload(&fixture(name), name).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT, "{name}");
    }
    let rows = sqlx::query!("SELECT kind::text AS \"kind!\",mime FROM photos ORDER BY mime")
        .fetch_all(&c.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.kind == "video"));
    assert_eq!(rows[0].mime, "video/mp4");
    assert_eq!(rows[1].mime, "video/quicktime");
    c.no_pending().await;
    let rows = sqlx::query!("SELECT id FROM photos")
        .fetch_all(&c.pool)
        .await
        .unwrap();
    let ingest = yhotos_pipeline::Ingest::new(
        c.state.store.clone(),
        c.pool.clone(),
        PathBuf::from("/unavailable-codec-host"),
    );
    for row in rows {
        ingest.ingest(yhotos_core::PhotoId(row.id)).await.unwrap();
    }
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE kind='video.transcode'")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(2)
    );
}

// Validation is stubbed; HTTP, fsync, completion hash/copy and enqueue remain real.
#[sqlx::test(migrations = "../yhotos-db/migrations")]
#[ignore = "loopback throughput measurement, run explicitly with --ignored --nocapture"]
async fn tus_loopback_30mb_in_8mib_chunks(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut c = Client::new(pool).await;
    let stub = tempfile::tempdir().unwrap();
    let executable = stub.path().join("identity-host");
    std::fs::write(&executable, r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"identity":{"kind":"Jpeg","width":1,"height":1,"pages":1,"orientation":1,"bits":8,"has_gain_map":false,"icc_desc":null}}'
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = config_for_host(&c, executable);
    config.upload.max_bytes = 32 * 1024 * 1024;
    config.upload.chunk_bytes = 8 * 1024 * 1024;
    let state = AppState::new(
        Box::leak(Box::new(config)),
        c.pool.clone(),
        c.state.store.clone(),
        c.state.auth.clone(),
    );
    let app = yhotos_http::admin_router(state);
    c.app = app.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    // Model an already-running admin stack, with its connection pool warmed.
    let warmup = c.start(1).await;
    assert_eq!(
        c.send("DELETE", &warmup, &[], Body::empty()).await.status(),
        StatusCode::NO_CONTENT
    );
    let length = 30_000_000;
    let mut bytes = vec![73u8; 8 * 1024 * 1024];
    bytes[..3].copy_from_slice(b"\xff\xd8\xff");
    let start = std::time::Instant::now();
    let url = c.start(length).await;
    let mut offset = 0;
    while offset < length {
        let n = (length - offset).min(bytes.len());
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let headers = format!(
            "PATCH {url} HTTP/1.1\r\nHost: {address}\r\nOrigin: https://admin.example.test\r\nSec-Fetch-Site: same-origin\r\nX-CSRF-Token: {}\r\nCookie: {}\r\nTus-Resumable: 1.0.0\r\nContent-Type: application/offset+octet-stream\r\nUpload-Offset: {offset}\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n",
            c.csrf, c.cookie
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(&bytes[..n]).await.unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 204"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        if offset + n == length {
            assert!(String::from_utf8_lossy(&response).contains("upload-result: created"));
        }
        offset += n;
    }
    eprintln!(
        "30 MB tus loopback, 8 MiB chunks, validation stubbed, including hash/copy/enqueue: {:?}",
        start.elapsed()
    );
    c.no_pending().await;
    server.abort();
}
fn config_for_host(c: &Client, executable: PathBuf) -> yhotos_config::Config {
    let mut config = yhotos_config::Config::from_figment(
        figment::Figment::new()
            .merge((
                "database_url_public",
                c.state.config.database_url_public.expose_secret(),
            ))
            .merge((
                "database_url_admin",
                c.state
                    .config
                    .database_url_admin
                    .as_ref()
                    .unwrap()
                    .expose_secret(),
            ))
            .merge((
                "data_dir",
                c.state.config.data_dir.to_string_lossy().as_ref(),
            ))
            .merge(("public_base_url", "https://example.test"))
            .merge(("admin_base_url", "https://admin.example.test"))
            .merge(("admin_listen", "127.0.0.1:8081"))
            .merge(("session_key", c.state.config.session_key.expose_secret())),
    )
    .unwrap();
    assert_eq!(
        config.upload.max_bytes,
        yhotos_codec::Limits::default().max_bytes
    );
    config.worker.codec_host_path = executable;
    config.upload = c.state.config.upload;
    config.worker.codec_timeout = c.state.config.worker.codec_timeout;
    config
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_concurrent_identical_uploads_publish_one_photo_and_one_ingest(pool: PgPool) {
    let c = Client::new(pool).await;
    let bytes = fixture("chart-srgb.png");
    let a = c.start(bytes.len()).await;
    let b = c.start(bytes.len()).await;
    let (a, b) = tokio::join!(c.patch(&a, 0, &bytes), c.patch(&b, 0, &bytes));
    assert_eq!(a.status(), StatusCode::NO_CONTENT);
    assert_eq!(b.status(), StatusCode::NO_CONTENT);
    let mut result = [
        a.headers()["upload-result"].to_str().unwrap(),
        b.headers()["upload-result"].to_str().unwrap(),
    ];
    result.sort_unstable();
    assert_eq!(result, ["created", "duplicate"]);
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE kind='ingest'")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    c.no_pending().await;
    let id = sqlx::query_scalar!("SELECT id FROM photos")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    let ingest = yhotos_pipeline::Ingest::new(
        c.state.store.clone(),
        c.pool.clone(),
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    );
    ingest.ingest(yhotos_core::PhotoId(id)).await.unwrap();
    let location = a.headers()["location"].to_str().unwrap();
    let r = c
        .send("GET", &format!("{location}/status"), &[], Body::empty())
        .await;
    let value: serde_json::Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["status"], "ready");
}

async fn retry_completion_after_recovery(pool: PgPool, script: Option<&str>, via_finish: bool) {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let mut c = Client::new(pool).await;
    let host = c.state.config.data_dir.join("recovering-host");
    if let Some(script) = script {
        std::fs::write(&host, format!("#!/bin/sh\ncat >/dev/null\n{script}\n")).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    c.use_host(host.clone(), 1);
    let bytes = fixture("chart-srgb.png");
    let url = c.start(bytes.len()).await;
    let failure = c.patch(&url, 0, &bytes).await;
    assert_eq!(failure.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failure.headers()[header::RETRY_AFTER], "30");
    assert_eq!(failure.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        failure.into_body().collect().await.unwrap().to_bytes(),
        "Upload temporarily unavailable. Retry later."
    );
    c.retained(&url, &bytes).await;
    // A second authenticated admin cannot resume or complete the creator's upload,
    // including when all bytes are present and the host has recovered.
    let other = Client::new(c.pool.clone()).await;
    if script.is_some() {
        std::fs::remove_file(&host).unwrap();
    }
    symlink(env!("CARGO_BIN_EXE_yhotos-codec-host"), &host).unwrap();
    assert_eq!(
        other.patch(&url, bytes.len(), &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .send("POST", &format!("{url}/finish"), &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    c.retained(&url, &bytes).await;
    let response = if via_finish {
        c.send("POST", &format!("{url}/finish"), &[], Body::empty())
            .await
    } else {
        c.patch(&url, bytes.len(), &[]).await
    };
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["upload-result"], "created");
    assert_eq!(response.headers()["upload-offset"], bytes.len().to_string());
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE kind='ingest'")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(1)
    );
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_unavailable_host_retains_bytes_and_finish_recovers(pool: PgPool) {
    retry_completion_after_recovery(pool, None, true).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_failed_host_retains_bytes_and_final_patch_recovers(pool: PgPool) {
    retry_completion_after_recovery(pool, Some("exit 3"), false).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_host_timeout_exit_retains_bytes_and_finish_recovers(pool: PgPool) {
    retry_completion_after_recovery(pool, Some("exit 4"), true).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_host_wall_timeout_retains_bytes_and_final_patch_recovers(pool: PgPool) {
    retry_completion_after_recovery(pool, Some("exec /bin/sleep 10"), false).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_invalid_host_report_retains_bytes_and_finish_recovers(pool: PgPool) {
    retry_completion_after_recovery(pool, Some("echo '{}'"), true).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_storage_failure_preserves_final_offset_and_bytes(pool: PgPool) {
    let c = Client::new(pool).await;
    let bytes = fixture("chart-srgb.png");
    let url = c.start(bytes.len()).await;
    // Occupy the originals directory with a file, so publication fails after
    // successful identity validation without affecting the durable upload.
    let originals = c.state.config.data_dir.join("originals");
    std::fs::write(&originals, []).unwrap();
    let failure = c.patch(&url, 0, &bytes).await;
    assert_eq!(failure.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failure.headers()[header::RETRY_AFTER], "30");
    c.retained(&url, &bytes).await;
    std::fs::remove_file(originals).unwrap();
    let response = c
        .send("POST", &format!("{url}/finish"), &[], Body::empty())
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["upload-result"], "created");
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_deleted_album_blocks_patch_and_completion(pool: PgPool) {
    let mut c = Client::new(pool).await;
    c.use_host(c.state.config.data_dir.join("unavailable-host"), 1);
    let bytes = fixture("chart-srgb.png");
    let partial = c.start(bytes.len()).await;
    let full = c.start(bytes.len()).await;
    assert_eq!(
        c.patch(&partial, 0, &bytes[..10]).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        c.patch(&full, 0, &bytes).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    yhotos_db::albums::soft_delete(&mut c.pool.acquire().await.unwrap(), c.album)
        .await
        .unwrap();
    c.use_host(env!("CARGO_BIN_EXE_yhotos-codec-host").into(), 10);
    assert_eq!(
        c.patch(&partial, 10, &bytes[10..]).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.patch(&full, bytes.len(), &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.send("POST", &format!("{full}/finish"), &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(c.head(&partial).await["upload-offset"], "10");
    assert_eq!(
        c.head(&full).await["upload-offset"],
        bytes.len().to_string()
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM photos")
            .fetch_one(&c.pool)
            .await
            .unwrap(),
        Some(0)
    );
    // Cleanup remains available to the owner even after the album disappears.
    for url in [partial, full] {
        assert_eq!(
            c.send("DELETE", &url, &[], Body::empty()).await.status(),
            StatusCode::NO_CONTENT
        );
    }
    c.no_pending().await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_finish_requires_all_bytes_and_valid_csrf(pool: PgPool) {
    let c = Client::new(pool).await;
    let url = c.start(100).await;
    assert_eq!(
        c.send("POST", &format!("{url}/finish"), &[], Body::empty())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("{url}/finish"))
        .header(header::COOKIE, &c.cookie)
        .header("tus-resumable", "1.0.0")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:4321".parse::<SocketAddr>().unwrap()));
    assert_eq!(
        c.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(c.head(&url).await["upload-offset"], "0");
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn tus_configured_ceiling_matches_the_host_and_is_advertised(pool: PgPool) {
    let mut c = Client::new(pool).await;
    let mut config = config_for_host(&c, env!("CARGO_BIN_EXE_yhotos-codec-host").into());
    let ceiling = yhotos_codec::Limits::default().max_bytes;
    assert_eq!(ceiling, 2 << 30);
    config.upload.max_bytes = ceiling;
    let state = AppState::new(
        Box::leak(Box::new(config)),
        c.pool.clone(),
        c.state.store.clone(),
        c.state.auth.clone(),
    );
    c.state = state;
    c.app = yhotos_http::admin_router(c.state.clone());
    let response = c.send("OPTIONS", "/admin/upload", &[], Body::empty()).await;
    assert_eq!(response.headers()["tus-max-size"], ceiling.to_string());
    assert_eq!(
        c.create(usize::try_from(ceiling).unwrap() + 1, "large.jpg")
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        c.upload(&fixture("chart-srgb.png"), "small.png")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    c.no_pending().await;
}
