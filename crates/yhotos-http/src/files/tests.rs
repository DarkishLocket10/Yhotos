#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use axum::{Router, extract::ConnectInfo, http::Request as HttpRequest, routing::get};
use http_body_util::BodyExt;
use sqlx::{ConnectOptions, PgPool, postgres::PgPoolOptions};
use std::{
    net::SocketAddr,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*};
use uuid::Uuid;
use yhotos_core::{
    GroupId, Slug, UserId,
    access::{Grant, Visibility},
};
use yhotos_db::{DatabaseTls, Db, Role, albums, grants, groups, sessions, users};
use yhotos_storage::Store;

struct Fixture {
    _dir: tempfile::TempDir,
    pool: PgPool,
    state: AppState,
    photo: photos::PhotoRow,
    hash: String,
    bytes: Vec<u8>,
}
impl Fixture {
    async fn new(pool: PgPool) -> Self {
        Self::with_global_cap(pool, 64).await
    }
    async fn with_global_cap(pool: PgPool, global_stream_cap: u16) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let bytes: Vec<_> = (0..=255).cycle().take(1024).collect();
        let put = store.put_original(bytes.as_slice(), 1024).await.unwrap();
        let hash = put.hash.to_string();
        let mut conn = pool.acquire().await.unwrap();
        let album = albums::create(
            &mut conn,
            None,
            &Slug::new("files").unwrap(),
            "Files",
            Visibility::Public,
        )
        .await
        .unwrap();
        let photo = photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Files".into(),
                original_hash: hash.clone(),
                original_name: "photo.jpg".into(),
                original_bytes: 1024,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await
        .unwrap();
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,640,480,1024)",Uuid::now_v7(),photo.id.0,hash).execute(&mut *conn).await.unwrap();
        sqlx::query!("INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms,hls) VALUES ($1,$2,'h264',$3,640,480,1024,1000,false)",Uuid::now_v7(),photo.id.0,hash).execute(&mut *conn).await.unwrap();
        let temp = store.tmp_dir().unwrap();
        let src = temp.path().join("image");
        tokio::fs::write(&src, &bytes).await.unwrap();
        let key = DerivativeKey {
            hash: &put.hash,
            variant: "small",
            format: "jpeg",
            hdr: false,
        };
        store.put_derivative(&key, &src).await.unwrap();
        // Storage has no video publication API yet; the fixture uses the trusted
        // typed storage path rather than deriving any path from request text.
        let path = store.path_video(&put.hash, "h264").unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(path, &bytes).await.unwrap();
        let config = crate::tests::state_with_global_cap(false, 100_000, global_stream_cap).config;
        let state = AppState::new(config, pool.clone(), store, None);
        Self {
            _dir: dir,
            pool,
            state,
            photo,
            hash,
            bytes,
        }
    }
    fn derivative(&self) -> String {
        format!("/i/{}/small.jpeg", self.hash)
    }
    fn original(&self) -> String {
        format!("/o/{}/wrong.png", self.hash)
    }
    fn video(&self) -> String {
        format!("/v/{}/h264.mp4", self.hash)
    }
    async fn restrict(&self) {
        photos::set_visibility(
            &mut self.pool.acquire().await.unwrap(),
            &[self.photo.id],
            Visibility::Restricted,
        )
        .await
        .unwrap();
        self.state.files.invalidate_all();
    }
    async fn session(&mut self, groups: &[GroupId]) -> (UserId, String) {
        use secrecy::SecretString;
        use std::sync::Arc;
        use yhotos_auth::{Token, TokenKind};

        let mut conn = self.pool.acquire().await.unwrap();
        let password = yhotos_auth::generate_temp_password();
        let hash = yhotos_auth::hash_password(&password).unwrap();
        let user = users::create(
            &mut conn,
            &Uuid::now_v7().to_string(),
            &SecretString::from(hash.as_str()),
            false,
        )
        .await
        .unwrap();
        for group in groups {
            groups::add_member(&mut conn, *group, user.id)
                .await
                .unwrap();
        }
        let token = Token::generate();
        let now = time::OffsetDateTime::now_utc();
        sessions::create(
            &mut conn,
            &sessions::Session {
                id: Uuid::now_v7(),
                user_id: user.id,
                admin: false,
                created_at: now,
                last_seen_at: now,
                expires_at: now + time::Duration::days(90),
                reauth_at: Some(now),
                mfa_at: None,
                ip_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
                ua_class: "other".into(),
                token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();
        let url = SecretString::from(self.pool.connect_options().to_url_lossy().to_string());
        let db = Arc::new(
            Db::connect(&url, Role::Public, 2, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let auth = crate::auth::Auth::new(self.state.config, db.clone(), None)
            .await
            .unwrap();
        self.state = AppState::new(
            self.state.config,
            db.pool().clone(),
            self.state.store.clone(),
            Some(auth),
        );
        (
            user.id,
            format!("__Host-ys={}", token.encode(TokenKind::Session)),
        )
    }
}
fn req(path: &str) -> Request {
    let mut request = HttpRequest::builder()
        .uri(path)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
    request
}
async fn bytes(response: Response) -> axum::body::Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}
fn bare(state: AppState) -> Router {
    Router::new()
        .route("/i/{hash}/{name}", get(derivative))
        .layer(Extension(SessionIdentity::Anonymous))
        .with_state(state)
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn public_derivative_headers_body_conditional_and_head(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    let response = app.clone().oneshot(req(&f.derivative())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let h = response.headers();
    assert_eq!(h[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(h[header::CONTENT_LENGTH], "1024");
    assert_eq!(
        h[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(h["cross-origin-resource-policy"], "cross-origin");
    assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(h[header::ACCEPT_RANGES], "bytes");
    assert!(!h.contains_key(header::LAST_MODIFIED));
    assert!(!h.contains_key(header::VARY));
    let etag = h[header::ETAG].clone();
    assert_eq!(etag, format!("\"{}\"", f.hash));
    assert_eq!(bytes(response).await.as_ref(), f.bytes);
    for tag in [
        etag.to_str().unwrap().to_owned(),
        format!("\"other\", W/{}", etag.to_str().unwrap()),
        "*".into(),
    ] {
        let mut request = req(&f.derivative());
        request
            .headers_mut()
            .insert(header::IF_NONE_MATCH, tag.parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "1024");
        assert!(bytes(response).await.is_empty());
    }
    let mut request = req(&f.derivative());
    *request.method_mut() = Method::HEAD;
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "1024");
    assert!(bytes(response).await.is_empty());
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn denied_and_missing_are_byte_identical_and_granted_requests_are_private(pool: PgPool) {
    let mut f = Fixture::new(pool).await;
    f.restrict().await;
    // Compare exact handler responses before request-specific nonce/id middleware.
    let app = bare(f.state.clone());
    let denied = app.clone().oneshot(req(&f.derivative())).await.unwrap();
    let missing = app
        .oneshot(req(&format!("/i/{}/small.jpeg", "e".repeat(64))))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(denied.status(), missing.status());
    assert_eq!(denied.headers(), missing.headers());
    assert_eq!(bytes(denied).await, bytes(missing).await);
    let (user, cookie) = f.session(&[]).await;
    grants::grant_photo(
        &mut f.pool.acquire().await.unwrap(),
        f.photo.id,
        Grant::User(user),
    )
    .await
    .unwrap();
    f.state.files.invalidate_all();
    let app = crate::public_router(f.state.clone());
    let mut request = req(&f.derivative());
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(response.headers()[header::VARY], "Cookie");
    assert_eq!(
        response.headers()["cross-origin-resource-policy"],
        "same-origin"
    );
    assert_eq!(bytes(response).await.as_ref(), f.bytes);
    // Cached authorization context must never cache a viewer's successful decision.
    let mut denied = req(&f.derivative());
    denied.headers_mut().insert(
        header::IF_NONE_MATCH,
        format!("\"{}\"", f.hash).parse().unwrap(),
    );
    denied
        .headers_mut()
        .insert(header::RANGE, HeaderValue::from_static("bytes=5-2"));
    let response = app.oneshot(denied).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::ETAG));
    assert_eq!(
        bytes(response).await,
        bytes(crate::pages::not_found()).await
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn originals_and_video_ranges_head_mime_and_disposition(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    for path in [f.original(), f.video(), f.derivative()] {
        for (range, start, len) in [
            ("bytes=0-99", 0, 100),
            ("bytes=1000-", 1000, 24),
            ("bytes=-10", 1014, 10),
            ("bytes=1023-9999", 1023, 1),
        ] {
            let mut request = req(&path);
            request
                .headers_mut()
                .insert(header::RANGE, range.parse().unwrap());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], len.to_string());
            assert_eq!(
                response.headers()[header::CONTENT_RANGE],
                format!("bytes {}-{}/1024", start, start + len - 1)
            );
            let expected = f.bytes.get(start..start + len).unwrap();
            assert_eq!(bytes(response).await.as_ref(), expected);
        }
        for range in [
            "bytes=5-2",
            "bytes=0-0,2-3",
            "bytes=1024-",
            "bytes=-0",
            "nonsense",
        ] {
            let mut request = req(&path);
            request
                .headers_mut()
                .insert(header::RANGE, range.parse().unwrap());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */1024");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
            assert!(bytes(response).await.is_empty());
        }
        let mut request = req(&path);
        *request.method_mut() = Method::HEAD;
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-99"));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "1024");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            if path.starts_with("/v/") {
                "video/mp4"
            } else {
                "image/jpeg"
            }
        );
        if path.starts_with("/o/") {
            assert_eq!(
                response.headers()[header::CONTENT_DISPOSITION],
                "inline; filename=\"photo.jpg\""
            );
        }
        assert!(bytes(response).await.is_empty());
    }
    let mut request = req(&f.original());
    request
        .headers_mut()
        .append(header::RANGE, HeaderValue::from_static("bytes=0-1"));
    request
        .headers_mut()
        .append(header::RANGE, HeaderValue::from_static("bytes=2-3"));
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    let response = app
        .oneshot(req(&format!("{}?download=1", f.original())))
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"photo.jpg\""
    );
}

#[tokio::test]
async fn invalid_hashes_and_selectors_never_reach_database_or_missing_store() {
    let original_state = crate::tests::state(false, 100_000);
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        original_state.config,
        original_state.pool.clone(),
        Store::open(dir.path()).unwrap(),
        None,
    );
    dir.close().unwrap();
    state.pool.close().await;
    let app = crate::public_router(state.clone());
    for hash in [
        "A".repeat(64),
        "a".repeat(63),
        format!("{}%2F", "a".repeat(63)),
        "..".into(),
        "%2e%2e%2fsecret".into(),
        "%FF".into(),
        "%00".into(),
    ] {
        for (prefix, name) in [("i", "small.jpeg"), ("o", "filename"), ("v", "h264.mp4")] {
            let response = app
                .clone()
                .oneshot(req(&format!("/{prefix}/{hash}/{name}")))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                bytes(response).await,
                bytes(crate::pages::not_found()).await
            );
        }
    }
    for name in [
        "../small.jpeg",
        "small.png",
        "Small.jpeg",
        "small.jpeg.hdr.hdr",
        "%2f.jpeg",
    ] {
        let response = app
            .clone()
            .oneshot(req(&format!("/i/{}/{name}", "a".repeat(64))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    for name in ["x".repeat(256), "%C3%A9".repeat(128), "x".repeat(4096)] {
        let response = app
            .clone()
            .oneshot(req(&format!("/o/{}/{name}", "a".repeat(64))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            bytes(response).await,
            bytes(crate::pages::not_found()).await
        );
    }
    state.files.rows.run_pending_tasks().await;
    assert_eq!(state.files.rows.entry_count(), 0);
}

#[test]
fn derivative_selector_alphabet_and_length_match_storage() {
    let hash = "a".repeat(64);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for (variant, valid) in [
        ("small", true),
        ("square2x", true),
        ("0123456789abcdef", true),
        ("", false),
        ("a-b", false),
        ("a_b", false),
        ("Small", false),
        ("é", false),
        ("abcdefghijklmnopq", false),
    ] {
        for format in ["jpeg", "avif", "jxl"] {
            for hdr in [false, true] {
                let suffix = if hdr { ".hdr" } else { "" };
                assert_eq!(
                    Selector::parse(
                        &hash,
                        &format!("{variant}.{format}{suffix}"),
                        Area::Derivatives
                    )
                    .is_ok(),
                    valid
                );
                assert_eq!(
                    store
                        .path_derivative(&DerivativeKey {
                            hash: &hash.parse().unwrap(),
                            variant,
                            format,
                            hdr,
                        })
                        .is_ok(),
                    valid
                );
            }
        }
    }
    for name in ["small.JPEG", "small.png"] {
        assert!(Selector::parse(&hash, name, Area::Derivatives).is_err());
    }
}

// Keep callsite interest enabled for the lifetime of the test process. A task
// local routes events to the current observer without counting parallel tests.
static CAPTURE_INIT: std::sync::Once = std::sync::Once::new();
tokio::task_local! { static CAPTURE: Events; }
struct CaptureLayer;
fn init_capture() {
    CAPTURE_INIT.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(CaptureLayer))
            .unwrap();
    });
}

struct Events {
    queries: Arc<AtomicUsize>,
    errors: Arc<Mutex<Vec<String>>>,
}
struct Fields(String);
impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
    }
}
impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let _ = CAPTURE.try_with(|capture| {
            if event.metadata().target() == "sqlx::query" {
                capture.queries.fetch_add(1, Ordering::Relaxed);
            }
            if *event.metadata().level() == tracing::Level::ERROR {
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                capture.errors.lock().unwrap().push(fields.0);
            }
        });
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn missing_file_is_concealed_and_logs_error_with_request_id(pool: PgPool) {
    init_capture();
    let f = Fixture::new(pool).await;
    let hash = f.hash.parse().unwrap();
    f.state
        .store
        .remove(
            Area::Derivatives,
            &hash,
            Some(&DerivativeKey {
                hash: &hash,
                variant: "small",
                format: "jpeg",
                hdr: false,
            }),
        )
        .await
        .unwrap();
    let errors = Arc::new(Mutex::new(vec![]));
    let capture = Events {
        queries: Arc::new(AtomicUsize::new(0)),
        errors: errors.clone(),
    };
    let response = CAPTURE
        .scope(
            capture,
            crate::public_router(f.state.clone()).oneshot(req(&f.derivative())),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        bytes(response).await,
        bytes(crate::pages::not_found()).await
    );
    let logs = errors.lock().unwrap();
    assert!(
        logs.iter()
            .any(|line| line.contains("stored file unavailable") && line.contains(&request_id)),
        "{logs:?}"
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn originals_and_video_expire_at_the_configured_absolute_deadline(pool: PgPool) {
    let f = Fixture::with_global_cap(pool, 2).await;
    let app = crate::public_router(f.state.clone());
    let mut original = app
        .clone()
        .oneshot(req(&f.original()))
        .await
        .unwrap()
        .into_body();
    let mut video = app
        .clone()
        .oneshot(req(&f.video()))
        .await
        .unwrap()
        .into_body();
    // Pause only after database/file setup. Both bodies remain unpolled,
    // so no file reads or timer-based cache expiry can affect the assertion.
    tokio::time::pause();
    tokio::time::advance(f.state.config.transfer_deadline).await;
    assert!(original.frame().await.unwrap().is_err());
    assert!(video.frame().await.unwrap().is_err());
    // Retaining failed bodies must not retain either the global or IP slots.
    let _first = f
        .state
        .files
        .streams
        .acquire("127.0.0.1".parse().unwrap())
        .unwrap();
    let _second = f
        .state
        .files
        .streams
        .acquire("127.0.0.1".parse().unwrap())
        .unwrap();
    tokio::time::resume();
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn original_cap_lives_until_stream_drop_and_is_per_ip(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    let mut held = vec![];
    for _ in 0..4 {
        let response = app.clone().oneshot(req(&f.original())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    let limited = app.clone().oneshot(req(&f.original())).await.unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut other = req(&f.original());
    other
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.2:1234".parse::<SocketAddr>().unwrap()));
    assert_eq!(
        app.clone().oneshot(other).await.unwrap().status(),
        StatusCode::OK
    );
    // Derivatives have no stream slots; video shares the originals' slots.
    assert_eq!(
        app.clone()
            .oneshot(req(&f.derivative()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(req(&f.video())).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(held.pop());
    assert_eq!(
        app.clone()
            .oneshot(req(&f.original()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let completed = held.pop().unwrap();
    assert_eq!(bytes(completed).await.len(), 1024);
    assert_eq!(
        app.oneshot(req(&f.original())).await.unwrap().status(),
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn video_cap_holds_unconsumed_responses_and_shares_slots_with_originals(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    let mut held = Vec::new();
    for _ in 0..4 {
        let response = app.clone().oneshot(req(&f.video())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    for path in [f.video(), f.original()] {
        assert_eq!(
            app.clone().oneshot(req(&path)).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let mut other = req(&f.video());
    other
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.2:1234".parse::<SocketAddr>().unwrap()));
    assert_eq!(
        app.clone().oneshot(other).await.unwrap().status(),
        StatusCode::OK
    );
    drop(held);
    for path in [f.video(), f.original()] {
        let response = app.clone().oneshot(req(&path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await, f.bytes);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn global_cap_holds_video_bodies_and_is_shared_across_ips_and_originals(pool: PgPool) {
    let f = Fixture::with_global_cap(pool, 2).await;
    let app = crate::public_router(f.state.clone());
    // First exhaust the cap with videos, then a mix of originals and videos.
    for paths in [[f.video(), f.video()], [f.original(), f.video()]] {
        let mut held = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let mut request = req(path);
            request.extensions_mut().insert(ConnectInfo(
                format!("127.0.0.{}:1234", index + 1)
                    .parse::<SocketAddr>()
                    .unwrap(),
            ));
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            held.push(response);
        }
        for path in [f.video(), f.original()] {
            let mut request = req(&path);
            request
                .extensions_mut()
                .insert(ConnectInfo("127.0.0.3:1234".parse::<SocketAddr>().unwrap()));
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::TOO_MANY_REQUESTS
            );
        }
        // Headers-only responses do not reserve slots even at the global cap.
        for (method, header, value, status) in [
            (
                Method::HEAD,
                header::RANGE,
                "bytes=0-9".into(),
                StatusCode::OK,
            ),
            (
                Method::GET,
                header::IF_NONE_MATCH,
                format!("\"{}\"", f.hash),
                StatusCode::NOT_MODIFIED,
            ),
            (
                Method::GET,
                header::RANGE,
                "bytes=9-2".into(),
                StatusCode::RANGE_NOT_SATISFIABLE,
            ),
        ] {
            let mut request = req(&f.video());
            *request.method_mut() = method;
            request
                .headers_mut()
                .insert(header, HeaderValue::from_str(&value).unwrap());
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), status);
        }
        assert_eq!(
            app.clone()
                .oneshot(req(&f.derivative()))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        drop(held);
        assert_eq!(
            app.clone().oneshot(req(&f.video())).await.unwrap().status(),
            StatusCode::OK
        );
    }
}

#[test]
fn disposition_sanitizes_quotes_newlines_unicode_and_path_separators() {
    for name in [
        "a\"b\r\ncafé/\\..jpg",
        "💣",
        "",
        ".",
        "..",
        "/tmp/a\n\";evil=true",
    ] {
        let safe = sanitize_filename(name);
        assert_ne!(safe, "");
        assert!(
            safe.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        );
        assert!(HeaderValue::from_str(&format!("inline; filename=\"{safe}\"")).is_ok());
    }
    assert_eq!(
        sanitize_filename("a\"b\r\ncafé/\\..jpg"),
        "a_b__caf___..jpg"
    );
    assert_eq!(sanitize_filename(&"x".repeat(10000)).len(), 200);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn one_cold_query_zero_warm_queries_and_validated_lookup_cache_key(pool: PgPool) {
    init_capture();
    let logging_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .log_statements(log::LevelFilter::Debug),
        )
        .await
        .unwrap();
    let f = Fixture::new(logging_pool).await;
    let counter = Arc::new(AtomicUsize::new(0));
    let capture = Events {
        queries: counter.clone(),
        errors: Arc::default(),
    };
    CAPTURE
        .scope(capture, async {
            let app = crate::public_router(f.state.clone());
            for paths in [
                vec![
                    f.derivative(),
                    format!("/i/{}/%73mall.jpeg", f.hash),
                    format!("/i/{}/small.%6apeg", f.hash),
                ],
                vec![
                    f.original(),
                    format!("/o/{}/other.jpg", f.hash),
                    format!("/o/{}/{}", f.hash, "x".repeat(255)),
                ],
                vec![f.video(); 3],
            ] {
                let before = counter.load(Ordering::Relaxed);
                for path in paths {
                    let response = app.clone().oneshot(req(&path)).await.unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    assert_eq!(bytes(response).await.len(), 1024);
                    assert_eq!(counter.load(Ordering::Relaxed), before + 1);
                }
            }
            f.state.files.invalidate_all();
            assert_eq!(
                app.oneshot(req(&f.derivative())).await.unwrap().status(),
                StatusCode::OK
            );
            assert_eq!(counter.load(Ordering::Relaxed), 4);
        })
        .await;
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn missing_original_filename_aliases_do_not_grow_metadata_cache(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    let hash = "e".repeat(64);
    for name in ["x".repeat(256), "%C3%A9".repeat(128), "x".repeat(4096)] {
        let response = app
            .clone()
            .oneshot(req(&format!("/o/{hash}/{name}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        f.state.files.rows.run_pending_tasks().await;
        assert_eq!(f.state.files.rows.entry_count(), 0);
    }
    // Negative metadata is cached once per hash, independently of display text,
    // percent encoding, or the query that controls Content-Disposition.
    for name in [
        "first.jpg".into(),
        "%66irst.jpg".into(),
        "other.jpg?download=1".into(),
        "x".repeat(255),
        format!("{}x", "%C3%A9".repeat(127)),
    ] {
        let response = app
            .clone()
            .oneshot(req(&format!("/o/{hash}/{name}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            bytes(response).await,
            bytes(crate::pages::not_found()).await
        );
        f.state.files.rows.run_pending_tasks().await;
        assert_eq!(f.state.files.rows.entry_count(), 1);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn derivative_cache_distinguishes_variant_format_and_hdr(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    for (index, name) in ["small.jpeg", "medium.jpeg", "small.avif", "small.jpeg.hdr"]
        .into_iter()
        .enumerate()
    {
        let response = app
            .clone()
            .oneshot(req(&format!("/i/{}/{name}", "e".repeat(64))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        f.state.files.rows.run_pending_tasks().await;
        assert_eq!(f.state.files.rows.entry_count(), (index + 1) as u64);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn video_metadata_uses_hash_even_when_requested_codec_is_missing(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::public_router(f.state.clone());
    let missing = app
        .clone()
        .oneshot(req(&format!("/v/{}/av1.mp4", f.hash)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let response = app.oneshot(req(&f.video())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await.as_ref(), f.bytes);
    f.state.files.rows.run_pending_tasks().await;
    assert_eq!(f.state.files.rows.entry_count(), 1);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn ancestor_group_grants_deletion_cycles_and_duplicate_owners(pool: PgPool) {
    let mut f = Fixture::new(pool.clone()).await;
    let mut conn = pool.acquire().await.unwrap();
    let root = albums::create(
        &mut conn,
        None,
        &Slug::new("root").unwrap(),
        "Root",
        Visibility::Restricted,
    )
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE albums SET parent_id=$2 WHERE id=$1",
        f.photo.album_id.0,
        root.id.0
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let group = groups::create(&mut conn, "Viewers").await.unwrap().id;
    grants::grant_album(&mut conn, root.id, Grant::Group(group))
        .await
        .unwrap();
    drop(conn);
    let (_, cookie) = f.session(&[group]).await;
    let app = crate::public_router(f.state.clone());
    let response = app.clone().oneshot(req(&f.derivative())).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let mut allowed = req(&f.derivative());
    allowed
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let response = app.clone().oneshot(allowed).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    // A duplicate live public owner must be considered, even after a private one.
    let public = albums::create(
        &mut pool.acquire().await.unwrap(),
        None,
        &Slug::new("public").unwrap(),
        "Public",
        Visibility::Public,
    )
    .await
    .unwrap();
    let duplicate = photos::create(
        &mut pool.acquire().await.unwrap(),
        &photos::NewPhoto {
            album_id: public.id,
            title: "Duplicate".into(),
            original_hash: f.hash.clone(),
            original_name: "duplicate.jpg".into(),
            original_bytes: 1024,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap();
    f.state.files.invalidate_all();
    assert_eq!(
        app.clone()
            .oneshot(req(&f.original()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    photos::soft_delete(&mut pool.acquire().await.unwrap(), duplicate.id)
        .await
        .unwrap();
    f.state.files.invalidate_all();
    assert_eq!(
        app.clone()
            .oneshot(req(&f.original()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    // Soft-deleted albums and cycles fail closed even to admins.
    for cycle in [false, true] {
        if cycle {
            sqlx::query!(
                "UPDATE albums SET deleted_at=NULL,parent_id=$2 WHERE id=$1",
                root.id.0,
                f.photo.album_id.0
            )
            .execute(&pool)
            .await
            .unwrap();
        } else {
            sqlx::query!("UPDATE albums SET deleted_at=now() WHERE id=$1", root.id.0)
                .execute(&pool)
                .await
                .unwrap();
        }
        f.state.files.invalidate_all();
        let admin = Router::new()
            .route("/i/{hash}/{name}", get(derivative))
            .layer(Extension(SessionIdentity::User {
                id: UserId::new(),
                groups: vec![],
                is_admin: true,
            }))
            .with_state(f.state.clone());
        assert_eq!(
            admin.oneshot(req(&f.derivative())).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn video_uses_originals_expensive_bucket_and_derivatives_keep_cheap_budget(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let limit = f.state.config.rate_limits.expensive_per_min;
    let app = crate::public_router(f.state.clone());
    for _ in 0..limit {
        let mut request = req(&f.original());
        *request.method_mut() = Method::HEAD;
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        app.clone().oneshot(req(&f.video())).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        app.oneshot(req(&f.derivative())).await.unwrap().status(),
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn hdr_variant_paths_are_distinct_and_database_filename_is_sanitized(pool: PgPool) {
    let f = Fixture::new(pool.clone()).await;
    let hash = f.hash.parse().unwrap();
    let temp = f.state.store.tmp_dir().unwrap();
    let src = temp.path().join("hdr");
    tokio::fs::write(&src, &f.bytes).await.unwrap();
    f.state
        .store
        .put_derivative(
            &DerivativeKey {
                hash: &hash,
                variant: "small",
                format: "jpeg",
                hdr: true,
            },
            &src,
        )
        .await
        .unwrap();
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',true,$3,640,480,1024)",Uuid::now_v7(),f.photo.id.0,f.hash).execute(&pool).await.unwrap();
    let name = "a\"b\r\ncafé/\\.jpg";
    sqlx::query!(
        "UPDATE photos SET original_name=$2 WHERE id=$1",
        f.photo.id.0,
        name
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = crate::public_router(f.state.clone());
    let response = app
        .clone()
        .oneshot(req(&format!("{}.hdr", f.derivative())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await.as_ref(), f.bytes);
    let response = app.clone().oneshot(req(&f.original())).await.unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        format!("inline; filename=\"{}\"", sanitize_filename(name))
    );
    assert_eq!(bytes(response).await.as_ref(), f.bytes);
    let mut conditional = req(&f.derivative());
    conditional
        .headers_mut()
        .insert(header::RANGE, HeaderValue::from_static("bytes=0-99"));
    conditional
        .headers_mut()
        .insert(header::IF_RANGE, HeaderValue::from_static("\"different\""));
    assert_eq!(
        app.oneshot(conditional).await.unwrap().status(),
        StatusCode::OK
    );
}

// Shares the task-local observer so page and file tests never compete for a
// process-global tracing subscriber or count another concurrent test's SQL.
pub(crate) async fn count_queries<T>(future: impl std::future::Future<Output = T>) -> (T, usize) {
    init_capture();
    let queries = Arc::new(AtomicUsize::new(0));
    let events = Events {
        queries: queries.clone(),
        errors: Arc::default(),
    };
    let value = CAPTURE.scope(events, future).await;
    (value, queries.load(Ordering::Relaxed))
}
