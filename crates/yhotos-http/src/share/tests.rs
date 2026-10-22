#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::*;
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request as HttpRequest},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::ExposeSecret;
use sqlx::{ConnectOptions, PgPool};
use std::{collections::BTreeMap, net::SocketAddr, sync::Mutex};
use tower::ServiceExt;
use tracing::{Event, Subscriber, field::Visit, instrument::WithSubscriber};
use tracing_subscriber::{Layer, layer::Context, prelude::*};
use uuid::Uuid;
use yhotos_core::Slug;
use yhotos_db::{DatabaseTls, Db, Role, albums, grants, mfa, photos, users};
use yhotos_storage::{DerivativeKey, Store};

struct Fixture {
    _dir: tempfile::TempDir,
    pool: PgPool,
    state: AppState,
    app: Router,
    creator: UserId,
    album: AlbumId,
    photo: PhotoId,
    image: String,
    original: String,
}
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}
impl Reply {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}
#[derive(Default)]
struct Client {
    jar: BTreeMap<String, String>,
}
impl Client {
    fn cookie(&self) -> String {
        self.jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
    async fn send(
        &mut self,
        app: &Router,
        method: Method,
        path: &str,
        body: String,
        json: bool,
        admin: bool,
    ) -> Reply {
        let mut request = HttpRequest::builder()
            .uri(path)
            .method(method)
            .header(header::COOKIE, self.cookie());
        if json {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::ORIGIN,
                    if admin {
                        "https://admin.example.test:8443"
                    } else {
                        "https://example.test"
                    },
                )
                .header("sec-fetch-site", "same-origin")
                .header(
                    "x-csrf-token",
                    self.jar
                        .get("__Host-yhotos-csrf")
                        .map_or("", String::as_str),
                );
        } else {
            request = request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let mut request = request.body(Body::from(body)).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
        let response = app.clone().oneshot(request).await.unwrap();
        for cookie in response.headers().get_all(header::SET_COOKIE) {
            let (k, v) = cookie
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap();
            self.jar.insert(k.into(), v.into());
        }
        Reply {
            status: response.status(),
            headers: response.headers().clone(),
            body: response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        }
    }
    async fn get(&mut self, app: &Router, path: &str) -> Reply {
        self.send(app, Method::GET, path, String::new(), false, false)
            .await
    }
    async fn unlock(&mut self, app: &Router, path: &str, password: &str) -> Reply {
        let csrf = self.jar.get("__Host-yhotos-csrf").unwrap();
        let mut fields = url::form_urlencoded::Serializer::new(String::new());
        fields
            .append_pair("_csrf", csrf)
            .append_pair("passphrase", password);
        self.send(
            app,
            Method::POST,
            &format!("{path}/unlock"),
            fields.finish(),
            false,
            false,
        )
        .await
    }
    async fn action(&mut self, app: &Router, id: PhotoId, action: &str) -> Reply {
        self.send(
            app,
            Method::POST,
            &format!("/api/v1/photos/{id}/{action}"),
            "{}".into(),
            true,
            false,
        )
        .await
    }
}
impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut c = pool.acquire().await.unwrap();
        let password = yhotos_auth::generate_temp_password();
        let phc = yhotos_auth::PasswordHasher::for_tests()
            .hash_password(&password)
            .unwrap();
        let creator = users::create(
            &mut c,
            &Uuid::now_v7().to_string(),
            &SecretString::from(phc.as_str()),
            true,
        )
        .await
        .unwrap()
        .id;
        sqlx::query!(
            "UPDATE users SET must_change_password=false,is_admin=true WHERE id=$1",
            creator as _
        )
        .execute(&mut *c)
        .await
        .unwrap();
        let album = albums::create(
            &mut c,
            None,
            &Slug::new("shared").unwrap(),
            "Shared <album>",
            Visibility::Restricted,
        )
        .await
        .unwrap()
        .id;
        drop(c);
        let url = SecretString::from(pool.connect_options().to_url_lossy().to_string());
        let public = Arc::new(
            Db::connect(&url, Role::Public, 1, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let admin = Arc::new(
            Db::connect(&url, Role::Admin, 2, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let mut key = [0; 32];
        getrandom::fill(&mut key).unwrap();
        let config = Box::leak(Box::new(
            yhotos_config::Config::from_figment(
                figment::Figment::new()
                    .merge(("database_url_public", url.expose_secret()))
                    .merge(("session_key", STANDARD.encode(key)))
                    .merge(("data_dir", dir.path().to_string_lossy().as_ref()))
                    .merge(("public_base_url", "https://example.test"))
                    .merge(("admin_listen", "127.0.0.1:8081"))
                    .merge(("admin_base_url", "https://admin.example.test:8443"))
                    .merge(("database_url_admin", url.expose_secret()))
                    .merge(("rate_limits.cheap_per_min", 10000u32))
                    .merge(("rate_limits.expensive_per_min", 10000u32))
                    .merge(("rate_limits.auth_per_min", 10000u32)),
            )
            .unwrap(),
        ));
        let auth = auth::Auth::new_for_tests(config, public.clone(), Some(admin))
            .await
            .unwrap();
        let state = AppState::new(config, public.pool().clone(), store, Some(auth));
        let (photo, image, original) = Self::add_photo(&pool, &state, album, "Shared photo").await;
        Self {
            _dir: dir,
            pool,
            state: state.clone(),
            app: crate::public_router(state),
            creator,
            album,
            photo,
            image,
            original,
        }
    }
    async fn add_photo(
        pool: &PgPool,
        state: &AppState,
        album: AlbumId,
        title: &str,
    ) -> (PhotoId, String, String) {
        let data = Uuid::now_v7().as_bytes().to_vec();
        let put = state.store.put_original(data.as_slice(), 16).await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let photo = photos::create(
            &mut c,
            &photos::NewPhoto {
                album_id: album,
                title: title.into(),
                original_hash: put.hash.to_string(),
                original_name: "photo.jpeg".into(),
                original_bytes: 16,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await
        .unwrap();
        let hash = put.hash.to_string();
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES($1,$2,'small','jpeg',false,$3,640,480,16)",Uuid::now_v7(),photo.id as _,hash).execute(&mut *c).await.unwrap();
        let tmp = state.store.tmp_dir().unwrap();
        let source = tmp.path().join("derivative");
        tokio::fs::write(&source, data).await.unwrap();
        state
            .store
            .put_derivative(
                &DerivativeKey {
                    hash: &put.hash,
                    variant: "small",
                    format: "jpeg",
                    hdr: false,
                },
                &source,
            )
            .await
            .unwrap();
        (
            photo.id,
            format!("/i/{hash}/small.jpeg"),
            format!("/o/{hash}/photo.jpeg"),
        )
    }
    async fn album(&self, parent: Option<AlbumId>, slug: &str, visibility: Visibility) -> AlbumId {
        albums::create(
            &mut self.pool.acquire().await.unwrap(),
            parent,
            &Slug::new(slug).unwrap(),
            slug,
            visibility,
        )
        .await
        .unwrap()
        .id
    }
    async fn link(
        &self,
        target: ShareTarget,
        password: Option<&str>,
        flags: LinkFlags,
    ) -> (ShareLinkRow, String) {
        let token = Token::generate();
        let passphrase_hash = password.map(|v| {
            SecretString::from(
                yhotos_auth::PasswordHasher::for_tests()
                    .hash_password(&passphrase(v))
                    .unwrap()
                    .as_str(),
            )
        });
        let row = share_links::create(
            &mut self.pool.acquire().await.unwrap(),
            &share_links::NewShareLink {
                token_hash: *token.hash(TokenKind::ShareLink).as_bytes(),
                target,
                passphrase_hash,
                expires_at: None,
                flags,
                created_by: self.creator,
            },
        )
        .await
        .unwrap();
        (row, format!("/s/{}", token.encode(TokenKind::ShareLink)))
    }
    async fn account(&self, admin: bool) -> String {
        let mut c = self.pool.acquire().await.unwrap();
        if admin {
            let id = Uuid::now_v7();
            let secret = yhotos_auth::totp::generate_secret();
            mfa::add(
                &mut c,
                self.creator,
                &mfa::Method {
                    id,
                    kind: "totp".into(),
                    secret_enc: Some(
                        self.state
                            .auth
                            .as_ref()
                            .unwrap()
                            .keys
                            .seal(id.as_bytes(), secret.expose_secret())
                            .unwrap(),
                    ),
                    credential_id: None,
                    public_key: None,
                    sign_count: 0,
                    last_used_step: None,
                    label: "Authenticator app".into(),
                    rp_id: None,
                },
            )
            .await
            .unwrap();
        }
        let token = Token::generate();
        let now = OffsetDateTime::now_utc();
        sessions::create(
            &mut c,
            &sessions::Session {
                id: Uuid::now_v7(),
                user_id: self.creator,
                admin,
                created_at: now,
                last_seen_at: now,
                expires_at: now + time::Duration::days(1),
                reauth_at: Some(now),
                mfa_at: admin.then_some(now),
                ip_hash: vec![0; 32],
                ua_class: "other".into(),
                token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();
        token.encode(TokenKind::Session)
    }
    fn invalidate(&self) {
        self.state.auth.as_ref().unwrap().shares.invalidate();
        self.state.files.invalidate_all();
    }
}
fn flags() -> LinkFlags {
    LinkFlags {
        view: true,
        download: true,
        like: true,
        flag: true,
    }
}
fn concealed(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    assert_eq!(reply.body, b"Not found");
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn unknown_expired_revoked_and_malformed_are_identical(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let mut client = Client::default();
    let baseline = client.get(&f.app, "/unknown").await;
    for mode in ["unknown", "expired", "revoked", "no-view"] {
        let (row, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
        let mut c = f.pool.acquire().await.unwrap();
        match mode {
            "expired" => {
                share_links::set_expiry(
                    &mut c,
                    row.id,
                    Some(OffsetDateTime::now_utc() - time::Duration::hours(1)),
                )
                .await
                .unwrap();
            }
            "revoked" => {
                share_links::revoke(&mut c, row.id).await.unwrap();
            }
            "no-view" => {
                share_links::update_flags(&mut c, row.id, LinkFlags::default())
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query!("DELETE FROM share_links WHERE id=$1", row.id as _)
                    .execute(&mut *c)
                    .await
                    .unwrap();
            }
        }
        drop(c);
        f.invalidate();
        let reply = client.get(&f.app, &path).await;
        concealed(&reply);
        assert_eq!(reply.body, baseline.body);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "private, no-store");
    }
    concealed(&client.get(&f.app, "/s/invalid").await);
    concealed(&client.get(&f.app, "/s/%FF").await);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn passphrase_lockout_cookie_scope_and_revocation(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let mut client = Client::default();
    let password = yhotos_auth::generate_temp_password();
    let (link, path) = f
        .link(
            ShareTarget::Album(f.album),
            Some(password.expose_secret()),
            flags(),
        )
        .await;
    let page = client.get(&f.app, &path).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.text().contains(&f.image));
    concealed(&client.get(&f.app, &f.image).await);
    // Form and JSON requests both remain CSRF protected.
    let missing = Client::default()
        .send(
            &f.app,
            Method::POST,
            &format!("{path}/unlock"),
            "passphrase=x".into(),
            false,
            false,
        )
        .await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    let wrong = yhotos_auth::generate_temp_password();
    for _ in 0..10 {
        assert_eq!(
            client
                .unlock(&f.app, &path, wrong.expose_secret())
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        client
            .unlock(&f.app, &path, password.expose_secret())
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let mut c = f.pool.acquire().await.unwrap();
    let hash = link.token_hash.clone();
    let state = sessions::lockout_get(&mut c, &hash).await.unwrap();
    let mut lock: UnlockState = serde_json::from_value(state).unwrap();
    assert_eq!(lock.failures, 10);
    let until = lock.locked_until.unwrap();
    assert!((until - OffsetDateTime::now_utc().unix_timestamp() - 3600).abs() < 5);
    assert!(lock.locked(until - 1));
    assert!(!lock.locked(until));
    let audits = sqlx::query_scalar!(
        "SELECT count(*) FROM audit_log WHERE action='share_unlock_locked' AND target_id=$1",
        link.id.0
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(audits, Some(1));
    lock.locked_until = Some(OffsetDateTime::now_utc().unix_timestamp() - 1);
    sessions::lockout_set(&mut c, &hash, &serde_json::to_value(lock).unwrap())
        .await
        .unwrap();
    drop(c);
    let unlocked = client.unlock(&f.app, &path, password.expose_secret()).await;
    assert_eq!(unlocked.status, StatusCode::OK, "{}", unlocked.text());
    let name = cookie_name(link.id);
    assert!(client.jar.contains_key(&name));
    let cookie = unlocked
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .find(|v| v.to_str().unwrap().starts_with(&name))
        .unwrap()
        .to_str()
        .unwrap();
    for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(attribute));
    }
    let image = client.get(&f.app, &f.image).await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(image.headers[header::CACHE_CONTROL], "private, no-store");
    let session = sqlx::query!(
        "SELECT user_id,share_link_id FROM sessions WHERE share_link_id=$1",
        link.id.0
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(session.user_id.is_none());
    assert_eq!(session.share_link_id, Some(link.id.0));
    share_links::revoke(&mut f.pool.acquire().await.unwrap(), link.id)
        .await
        .unwrap();
    // Independent DB mutation must invalidate even the warmed session/landing cache.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let reply = client.get(&f.app, &f.image).await;
            if reply.status == StatusCode::NOT_FOUND {
                concealed(&reply);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    concealed(&client.get(&f.app, &path).await);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn capabilities_and_link_attribution(pool: PgPool) {
    let f = Fixture::new(pool).await;
    for enabled in [false, true] {
        let caps = LinkFlags {
            view: true,
            download: enabled,
            like: enabled,
            flag: enabled,
        };
        let (row, path) = f.link(ShareTarget::Album(f.album), None, caps).await;
        let mut client = Client::default();
        let page = client.get(&f.app, &path).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        assert_eq!(page.text().contains("Download original"), enabled);
        assert_eq!(page.text().contains("Like photo"), enabled);
        assert_eq!(page.text().contains("Flag photo"), enabled);
        for path in [&f.original, &format!("{}?download=1", f.image)] {
            let reply = client.get(&f.app, path).await;
            if enabled {
                assert_eq!(reply.status, StatusCode::OK);
            } else {
                concealed(&reply);
            }
        }
        for action in ["like", "flag"] {
            let reply = client.action(&f.app, f.photo, action).await;
            if enabled {
                assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
            } else {
                concealed(&reply);
            }
        }
        let likes = sqlx::query!("SELECT user_id FROM likes WHERE share_link_id=$1", row.id.0)
            .fetch_all(&f.pool)
            .await
            .unwrap();
        let flags = sqlx::query!("SELECT user_id FROM flags WHERE share_link_id=$1", row.id.0)
            .fetch_all(&f.pool)
            .await
            .unwrap();
        assert_eq!(likes.len(), usize::from(enabled));
        assert_eq!(flags.len(), usize::from(enabled));
        assert!(likes.iter().all(|r| r.user_id.is_none()));
        assert!(flags.iter().all(|r| r.user_id.is_none()));
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn scope_descendants_restricted_ancestors_photo_only_and_user_union(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let other = f.album(None, "other", Visibility::Restricted).await;
    let (other_photo, other_image, _) =
        Fixture::add_photo(&f.pool, &f.state, other, "Other photo").await;
    let child = f.album(Some(f.album), "child", Visibility::Public).await;
    let (_, child_image, _) = Fixture::add_photo(&f.pool, &f.state, child, "Child photo").await;
    let restricted = f
        .album(Some(f.album), "restricted", Visibility::Restricted)
        .await;
    let (_, restricted_image, _) =
        Fixture::add_photo(&f.pool, &f.state, restricted, "Restricted child").await;
    let (_, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let mut client = Client::default();
    let page = client.get(&f.app, &path).await;
    assert!(page.text().contains(&f.image));
    assert!(page.text().contains(&child_image));
    assert!(!page.text().contains(&restricted_image));
    assert!(!page.text().contains(&other_image));
    assert!(page.text().contains("Shared "));
    assert!(!page.text().contains("<album>"));
    for path in [&f.image, &child_image] {
        assert_eq!(client.get(&f.app, path).await.status, StatusCode::OK);
    }
    for path in [&other_image, &restricted_image] {
        concealed(&client.get(&f.app, path).await);
    }
    concealed(&client.action(&f.app, other_photo, "flag").await);
    let (_, blocked) = f.link(ShareTarget::Album(restricted), None, flags()).await;
    let mut blocked_client = Client::default();
    concealed(&blocked_client.get(&f.app, &blocked).await);
    concealed(&blocked_client.get(&f.app, &restricted_image).await); // Restricted ancestor above target.
    // A public-listener account does not inherit its user's administrator bit.
    grants::grant_album(
        &mut f.pool.acquire().await.unwrap(),
        other,
        Grant::User(f.creator),
    )
    .await
    .unwrap();
    f.invalidate(); // Direct fixture mutation evicts task 008's file metadata cache.
    client
        .jar
        .insert("__Host-ys".into(), f.account(false).await);
    for path in [&f.image, &other_image] {
        assert_eq!(
            client.get(&f.app, path).await.status,
            StatusCode::OK,
            "{path}"
        );
    }
    concealed(&client.get(&f.app, &restricted_image).await);
    let public = f.album(None, "photo-target", Visibility::Public).await;
    let (photo, image, _) = Fixture::add_photo(&f.pool, &f.state, public, "Single photo").await;
    let (_, sibling, _) = Fixture::add_photo(&f.pool, &f.state, public, "Hidden sibling").await;
    photos::set_visibility(
        &mut f.pool.acquire().await.unwrap(),
        &[photo],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    f.invalidate();
    let (_, single) = f
        .link(
            ShareTarget::Photo(photo),
            None,
            LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        )
        .await;
    let mut single_client = Client::default();
    let page = single_client.get(&f.app, &single).await;
    assert!(page.text().contains(&image));
    assert!(!page.text().contains(&sibling));
    assert!(!page.text().contains("/a/"));
    assert_eq!(
        single_client.get(&f.app, &image).await.status,
        StatusCode::OK
    );
    concealed(&single_client.get(&f.app, "/a/photo-target").await);
    concealed(&single_client.get(&f.app, &sibling).await);
    concealed(
        &single_client
            .get(&f.app, &format!("{image}?download=1"))
            .await,
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn opens_once_per_landing_rotating_private_hash_and_stats(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (row, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let mut client = Client::default();
    client.get(&f.app, &path).await;
    client.get(&f.app, &f.image).await;
    client.action(&f.app, f.photo, "like").await;
    let read = || async {
        sqlx::query!("SELECT visitor_hash,country,city FROM share_link_opens WHERE share_link_id=$1 ORDER BY opened_at,id",row.id.0).fetch_all(&f.pool).await.unwrap()
    };
    let first = read().await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].visitor_hash.len(), 32);
    assert!(first[0].country.is_none() && first[0].city.is_none());
    client.get(&f.app, &path).await;
    let second = read().await;
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].visitor_hash, second[1].visitor_hash);
    // Move the in-memory day to yesterday; the next landing rotates before hashing.
    let shares = &f.state.auth.as_ref().unwrap().shares;
    shares
        .visitor(
            "127.0.0.1".parse().unwrap(),
            "other",
            OffsetDateTime::now_utc() - time::Duration::days(1),
        )
        .unwrap();
    client.get(&f.app, &path).await;
    let third = read().await;
    assert_ne!(third[0].visitor_hash, third[2].visitor_hash);
    let stats = share_links::stats(&mut f.pool.acquire().await.unwrap(), row.id)
        .await
        .unwrap();
    assert_eq!(stats.opens, 3);
    assert_eq!(stats.unique_visitors, 2);
    assert_eq!(stats.countries, serde_json::json!({}));
    assert!(stats.first_open <= stats.last_open);
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);
struct Fields(String);
impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
    }
}
impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields(event.metadata().target().into());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = Fields(String::new());
        attrs.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn digest_only_cache_query_budgets_and_no_token_in_tracing(pool: PgPool) {
    // Keep query callsite interest enabled while parallel tasks have no local
    // observer, matching the existing task-008 query-count instrumentation.
    crate::files::tests::init_capture();
    let f = Fixture::new(pool).await;
    let (row, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    // Prime SQLx's per-connection PostgreSQL enum catalog cache. The
    // application file metadata cache remains cold, as in task 008's test.
    let hash = f.original.split('/').nth(2).unwrap();
    photos::owner_of_hash(&mut f.state.pool.acquire().await.unwrap(), hash)
        .await
        .unwrap();
    let capture = Capture::default();
    let observer = capture.clone();
    async {
        // sqlx query telemetry is counted on this task, excluding background LISTEN.
        let shares = &f.state.auth.as_ref().unwrap().shares;
        let hash = row.token_hash.as_slice().try_into().unwrap();
        assert!(shares.link(&f.state, hash).await.unwrap().is_some());
        let count = || {
            observer
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.starts_with("sqlx::query"))
                .count()
        };
        let cold = count();
        assert_eq!(cold, 1, "{:?}", observer.0.lock().unwrap());
        shares.link(&f.state, hash).await.unwrap();
        assert_eq!(count(), cold);
        let mut client = Client::default();
        assert_eq!(client.get(&f.app, &path).await.status, StatusCode::OK);
        let before = count();
        assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
        assert_eq!(count(), before + 1); // Only file metadata, session seeded at issuance.
        let before = count();
        assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
        assert_eq!(count(), before);
        let before = count();
        assert_eq!(client.get(&f.app, &path).await.status, StatusCode::OK);
        assert_eq!(count(), before + 1); // Only required open write.
    }
    .with_subscriber(tracing_subscriber::registry().with(capture))
    .await;
    let logs = observer.0.lock().unwrap().join("\n");
    assert!(logs.contains("/s/{token}"));
    assert!(!logs.contains(path.strip_prefix("/s/").unwrap()));
}

#[cfg(feature = "admin")]
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn admin_create_patch_stats_revoke_and_listener_isolation(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let app = crate::admin_router(f.state.clone());
    let mut admin = Client::default();
    admin.jar.insert("ya".into(), f.account(true).await);
    let page = admin.get(&app, "/admin/u/password").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(admin.jar.contains_key("__Host-yhotos-csrf"));
    let base = "/admin/api/share-links";
    let mut input = serde_json::json!({"target_kind":"album","target_id":f.album.0,"passphrase":"","allow_view":true,"allow_download":false,"allow_like":false,"allow_flag":true});
    let created = admin
        .send(&app, Method::POST, base, input.to_string(), true, true)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let value = created.json();
    let id = value["id"].as_str().unwrap();
    let path = value["url"]
        .as_str()
        .unwrap()
        .strip_prefix("https://example.test")
        .unwrap();
    let mut client = Client::default();
    client.get(&f.app, path).await;
    assert_eq!(client.unlock(&f.app, path, "").await.status, StatusCode::OK);
    assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
    concealed(&client.get(&f.app, &f.original).await);
    let updated=admin.send(&app,Method::PATCH,&format!("{base}/{id}"),serde_json::json!({"allow_download":true,"allow_like":true,"expires_at":null,"passphrase":null}).to_string(),true,true).await;
    assert_eq!(updated.status, StatusCode::NO_CONTENT, "{}", updated.text());
    // Changing the passphrase invalidates the old unlocked cookie.
    concealed(&client.get(&f.app, &f.image).await);
    client.get(&f.app, path).await;
    assert_eq!(client.get(&f.app, &f.original).await.status, StatusCode::OK);
    assert_eq!(
        client.action(&f.app, f.photo, "flag").await.status,
        StatusCode::NO_CONTENT
    );
    let stats = admin.get(&app, &format!("{base}/{id}/stats")).await;
    assert_eq!(stats.status, StatusCode::OK);
    assert_eq!(stats.json()["opens"], 2);
    assert!(stats.json().get("url").is_none());
    let revoked = admin
        .send(
            &app,
            Method::POST,
            &format!("{base}/{id}/revoke"),
            "{}".into(),
            true,
            true,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    concealed(&client.get(&f.app, path).await);
    concealed(&client.get(&f.app, &f.image).await);
    // Public listener never exposes management, even with a valid admin cookie.
    concealed(
        &admin
            .send(&f.app, Method::POST, base, input.to_string(), true, false)
            .await,
    );
    input["target_kind"] = serde_json::json!("image");
    assert_eq!(
        admin
            .send(&app, Method::POST, base, input.to_string(), true, true)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let audits=sqlx::query_scalar!("SELECT count(*) FROM audit_log WHERE target_id=$1 AND action IN ('share_created','share_updated','share_revoked')",Uuid::parse_str(id).unwrap()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(audits, Some(3));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn session_binding_multiple_links_and_idle_absolute_hard_expiry(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (row, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let other = f.album(None, "second", Visibility::Restricted).await;
    let (_, image, _) = Fixture::add_photo(&f.pool, &f.state, other, "Second scope").await;
    let password = yhotos_auth::generate_temp_password();
    let (protected, protected_path) = f
        .link(
            ShareTarget::Album(other),
            Some(password.expose_secret()),
            flags(),
        )
        .await;
    let mut client = Client::default();
    client.get(&f.app, &path).await;
    let first = client.jar.get(&cookie_name(row.id)).unwrap().clone();
    let mut rebound = Client::default();
    rebound.jar.insert(cookie_name(protected.id), first.clone());
    let page = rebound.get(&f.app, &protected_path).await;
    assert!(!page.text().contains(&image));
    concealed(&rebound.get(&f.app, &image).await);
    rebound.jar.clear();
    rebound.jar.insert("__Host-ys".into(), first);
    concealed(&rebound.get(&f.app, &f.image).await);
    let mut account = Client::default();
    account
        .jar
        .insert(cookie_name(row.id), f.account(false).await);
    concealed(&account.get(&f.app, &f.image).await);
    client.get(&f.app, &protected_path).await;
    assert_eq!(
        client
            .unlock(&f.app, &protected_path, password.expose_secret())
            .await
            .status,
        StatusCode::OK
    );
    for path in [&f.image, &image] {
        assert_eq!(client.get(&f.app, path).await.status, StatusCode::OK);
    }
    for mode in ["idle", "absolute", "hard"] {
        let mut client = Client::default();
        client.get(&f.app, &path).await;
        let mut c = f.pool.acquire().await.unwrap();
        let digest = auth::token_hash(
            &{
                let mut h = HeaderMap::new();
                h.insert(header::COOKIE, client.cookie().parse().unwrap());
                h
            },
            &cookie_name(row.id),
            TokenKind::Session,
        )
        .unwrap();
        let now = OffsetDateTime::now_utc();
        let created = now - time::Duration::days(if mode == "absolute" { 91 } else { 30 });
        let seen = now - time::Duration::days(if mode == "idle" { 15 } else { 0 });
        let expires = now + time::Duration::days(if mode == "hard" { -1 } else { 1 });
        sqlx::query!(
            "UPDATE sessions SET created_at=$2,last_seen_at=$3,expires_at=$4 WHERE token_hash=$1",
            digest,
            created,
            seen,
            expires
        )
        .execute(&mut *c)
        .await
        .unwrap();
        drop(c);
        f.invalidate();
        concealed(&client.get(&f.app, &f.image).await);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn passphrase_change_serializes_with_session_issuance(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (row, _) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let mut c = f.state.pool.acquire().await.unwrap();
    let mut tx = c.begin().await.unwrap();
    let hash = *Token::generate().hash(TokenKind::Session).as_bytes();
    let session = share_links::create_session(&mut tx, &row, &hash, &[0; 32], "other")
        .await
        .unwrap()
        .unwrap();
    let mut admin = f.pool.acquire().await.unwrap();
    let password = yhotos_auth::generate_temp_password();
    let phc = yhotos_auth::PasswordHasher::for_tests()
        .hash_password(&passphrase(password.expose_secret()))
        .unwrap();
    let change = share_links::set_passphrase(&mut admin, row.id, Some(phc.as_str()));
    tokio::pin!(change);
    // The admin change waits for the issuing transaction. Once it acquires
    // the link lock, its trigger sees and deletes the newly committed session.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut change)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    change.await.unwrap();
    let count = sqlx::query_scalar!("SELECT count(*) FROM sessions WHERE id=$1", session.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, Some(0));
}
