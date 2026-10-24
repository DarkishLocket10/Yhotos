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
    #[cfg(feature = "admin")]
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
        let mut request = HttpRequest::builder().uri(path).method(method);
        if !self.jar.is_empty() {
            request = request.header(header::COOKIE, self.cookie());
        }
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
        if path.starts_with("/api/v1/") && json {
            request = request.header("x-requested-with", "yhotos");
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
            if cookie.to_str().unwrap().contains("Max-Age=0") {
                self.jar.remove(k);
            } else {
                self.jar.insert(k.into(), v.into());
            }
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
            &format!(
                "/api/v1/photos/{}/{action}",
                yhotos_core::ShortId::from_photo_id(id)
            ),
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
async fn concealed(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    if reply
        .headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v == "application/json")
    {
        assert_eq!(reply.body, b"{\"error\":\"not_found\"}");
        return;
    }
    let expected = crate::pages::not_found()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(reply.body, expected);
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
        concealed(&reply).await;
        assert_eq!(reply.body, baseline.body);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "private, no-store");
    }
    concealed(&client.get(&f.app, "/s/invalid").await).await;
    concealed(&client.get(&f.app, "/s/%FF").await).await;
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
    concealed(&client.get(&f.app, &f.image).await).await;
    // Form and JSON requests both remain CSRF protected.
    let missing = Client::default()
        .send(
            &f.app,
            Method::POST,
            &format!("{path}/unlock"),
            format!("passphrase={}", Uuid::now_v7()),
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
    let name = COOKIE_NAME;
    assert!(client.jar.contains_key(name));
    let cookie = unlocked
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .find(|v| v.to_str().unwrap().starts_with(name))
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
        "SELECT s.user_id,sl.share_link_id FROM sessions s JOIN share_session_links sl ON sl.session_id=s.id WHERE sl.share_link_id=$1",
        link.id.0
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(session.user_id.is_none());
    assert_eq!(session.share_link_id, link.id.0);
    share_links::revoke(&mut f.pool.acquire().await.unwrap(), link.id)
        .await
        .unwrap();
    // Independent DB mutation must invalidate even the warmed session/landing cache.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let reply = client.get(&f.app, &f.image).await;
            if reply.status == StatusCode::NOT_FOUND {
                concealed(&reply).await;
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    concealed(&client.get(&f.app, &path).await).await;
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
        assert!(page.text().contains("class=\"photo-grid\""));
        let photo = client
            .get(
                &f.app,
                &format!("/p/{}", yhotos_core::ShortId::from_photo_id(f.photo)),
            )
            .await;
        assert_eq!(photo.status, StatusCode::OK, "{}", photo.text());
        assert_eq!(photo.text().contains("Download original"), enabled);
        assert_eq!(photo.text().contains("Download photo"), enabled);
        assert_eq!(photo.text().contains("Like photo"), enabled);
        assert_eq!(photo.text().contains("Flag photo"), enabled);
        for path in [&f.original, &format!("{}?download=1", f.image)] {
            let reply = client.get(&f.app, path).await;
            if enabled {
                assert_eq!(reply.status, StatusCode::OK);
            } else {
                concealed(&reply).await;
            }
        }
        for action in ["like", "flag"] {
            let reply = client.action(&f.app, f.photo, action).await;
            if enabled {
                assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
            } else {
                concealed(&reply).await;
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
        concealed(&client.get(&f.app, path).await).await;
    }
    concealed(&client.action(&f.app, other_photo, "flag").await).await;
    let (_, blocked) = f.link(ShareTarget::Album(restricted), None, flags()).await;
    let mut blocked_client = Client::default();
    concealed(&blocked_client.get(&f.app, &blocked).await).await;
    concealed(&blocked_client.get(&f.app, &restricted_image).await).await; // Restricted ancestor above target.
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
    concealed(&client.get(&f.app, &restricted_image).await).await;
    // Browsing pages uses the same account/link union as file access.
    assert_eq!(client.get(&f.app, "/a/shared").await.status, StatusCode::OK);
    assert_eq!(client.get(&f.app, "/a/other").await.status, StatusCode::OK);
    let account_photo = client
        .get(
            &f.app,
            &format!("/p/{}", yhotos_core::ShortId::from_photo_id(other_photo)),
        )
        .await;
    assert_eq!(account_photo.status, StatusCode::OK);
    assert!(account_photo.text().contains("Download photo"));
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
    // An unrelated album grant must not reveal the photo-only target's album.
    assert_eq!(
        single_client.get(&f.app, &path).await.status,
        StatusCode::OK
    );
    let direct = single_client
        .get(
            &f.app,
            &format!("/p/{}", yhotos_core::ShortId::from_photo_id(photo)),
        )
        .await;
    assert_eq!(direct.status, StatusCode::OK);
    assert!(!direct.text().contains("/a/"));
    assert!(!direct.text().contains("photo-target"));
    // The unrelated link's interaction caps must not authorize this photo either.
    for action in ["like", "flag"] {
        concealed(&single_client.action(&f.app, photo, action).await).await;
    }
    concealed(
        &single_client
            .send(
                &f.app,
                Method::DELETE,
                &format!(
                    "/api/v1/photos/{}/like",
                    yhotos_core::ShortId::from_photo_id(photo)
                ),
                String::new(),
                true,
                false,
            )
            .await,
    )
    .await;
    concealed(&single_client.get(&f.app, "/a/photo-target").await).await;
    concealed(&single_client.get(&f.app, &sibling).await).await;
    concealed(
        &single_client
            .get(&f.app, &format!("{image}?download=1"))
            .await,
    )
    .await;
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
        assert!(
            shares
                .page(&f.state, hash, 1, photos::PageSort::Taken)
                .await
                .unwrap()
                .is_some()
        );
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
        shares
            .page(&f.state, hash, 1, photos::PageSort::Taken)
            .await
            .unwrap();
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
        assert_eq!(count(), before + 1, "{:?}", observer.0.lock().unwrap()); // Required open insert only; zero grant-set reads or recency writes.
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
    let password = yhotos_auth::generate_temp_password();
    let mut input = serde_json::json!({"target_kind":"album","target_id":f.album.0,"passphrase":password.expose_secret(),"allow_view":true,"allow_download":false,"allow_like":false,"allow_flag":true});
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
    assert_eq!(
        client
            .unlock(&f.app, path, password.expose_secret())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
    concealed(&client.get(&f.app, &f.original).await).await;
    let updated=admin.send(&app,Method::PATCH,&format!("{base}/{id}"),serde_json::json!({"allow_download":true,"allow_like":true,"expires_at":null,"passphrase":null}).to_string(),true,true).await;
    assert_eq!(updated.status, StatusCode::NO_CONTENT, "{}", updated.text());
    // Changing the passphrase invalidates the old unlocked cookie.
    concealed(&client.get(&f.app, &f.image).await).await;
    client.get(&f.app, path).await;
    assert_eq!(client.get(&f.app, &f.original).await.status, StatusCode::OK);
    assert_eq!(
        client.action(&f.app, f.photo, "flag").await.status,
        StatusCode::OK
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
    concealed(&client.get(&f.app, path).await).await;
    concealed(&client.get(&f.app, &f.image).await).await;
    // Public listener never exposes management, even with a valid admin cookie.
    concealed(
        &admin
            .send(&f.app, Method::POST, base, input.to_string(), true, false)
            .await,
    )
    .await;
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
    let (_, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
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
    let first = client.jar.get(COOKIE_NAME).unwrap().clone();
    let mut rebound = Client::default();
    rebound.jar.insert(
        format!(
            "yl_{}",
            yhotos_core::ShortId::from_photo_id(PhotoId(protected.id.0))
        ),
        first.clone(),
    );
    let page = rebound.get(&f.app, &protected_path).await;
    assert!(!page.text().contains(&image));
    concealed(&rebound.get(&f.app, &image).await).await;
    rebound.jar.clear();
    rebound.jar.insert("__Host-ys".into(), first);
    concealed(&rebound.get(&f.app, &f.image).await).await;
    let mut account = Client::default();
    account
        .jar
        .insert(COOKIE_NAME.into(), f.account(false).await);
    concealed(&account.get(&f.app, &f.image).await).await;
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
            COOKIE_NAME,
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
        concealed(&client.get(&f.app, &f.image).await).await;
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
    // the link lock, its trigger sees and removes the newly committed grant.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut change)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    change.await.unwrap();
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM share_session_links WHERE session_id=$1",
        session.id
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, Some(0));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn album_page_snapshot_uses_shared_responsive_template(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (second, second_image, _) =
        Fixture::add_photo(&f.pool, &f.state, f.album, "Second photo").await;
    let (third, third_image, _) =
        Fixture::add_photo(&f.pool, &f.state, f.album, "Third photo").await;
    let (_, hidden_image, _) = Fixture::add_photo(&f.pool, &f.state, f.album, "Hidden photo").await;
    let mut c = f.pool.acquire().await.unwrap();
    sqlx::query!(
        "UPDATE photos SET visibility='restricted' WHERE album_id=$1 AND title='Hidden photo'",
        f.album as _
    )
    .execute(&mut *c)
    .await
    .unwrap();
    for photo in [f.photo, second, third] {
        for format in ["avif", "jxl"] {
            sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) SELECT $1,photo_id,variant,$2,hdr,hash,width,height,bytes FROM derivatives WHERE photo_id=$3 AND format='jpeg'",Uuid::now_v7(),format,photo as _).execute(&mut *c).await.unwrap();
        }
    }
    drop(c);
    let (_, path) = f
        .link(
            ShareTarget::Album(f.album),
            None,
            LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        )
        .await;
    let page = Client::default()
        .get(&f.app, &format!("{path}?sort=manual"))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert_eq!(page.headers[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(
        page.text()
            .split("</section>")
            .next()
            .unwrap()
            .matches("<picture>")
            .count(),
        3
    );
    assert!(page.text().contains("image/jxl"));
    assert!(page.text().contains("image/avif"));
    assert!(page.text().contains("class=\"grid-row\""));
    assert!(!page.text().contains(&hidden_image));
    assert!(!page.text().contains("Download"));
    assert!(!page.text().contains("share-grid"));
    let nonce = page
        .text()
        .split("nonce=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let mut html = page
        .text()
        .replace(nonce, "fixture-nonce")
        .replace(
            crate::static_assets::stylesheet_url().unwrap(),
            "/static/app.fixture.css",
        )
        .replace(
            crate::static_assets::script_url().unwrap(),
            "/static/thumbhash.fixture.js",
        );
    for (index, (photo, image)) in [
        (f.photo, &f.image),
        (second, &second_image),
        (third, &third_image),
    ]
    .into_iter()
    .enumerate()
    {
        html = html
            .replace(
                image.split('/').nth(2).unwrap(),
                &format!("{:064x}", index + 1),
            )
            .replace(
                &yhotos_core::ShortId::from_photo_id(photo).to_string(),
                &format!("photo-{}", index + 1),
            );
    }
    insta::assert_snapshot!("share_album", crate::pages::snapshot_html(html));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn scoped_pages_sort_paginate_and_continue_into_child_albums(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let child = f.album(Some(f.album), "child", Visibility::Public).await;
    let (child_photo, child_image, _) =
        Fixture::add_photo(&f.pool, &f.state, child, "Child photo").await;
    let restricted = f
        .album(Some(f.album), "hidden-child", Visibility::Restricted)
        .await;
    let (_, hidden_image, _) =
        Fixture::add_photo(&f.pool, &f.state, restricted, "Hidden child").await;
    let mut c = f.pool.acquire().await.unwrap();
    for index in 0..62_i32 {
        let id = PhotoId::new();
        let short = yhotos_core::ShortId::from_photo_id(id).to_string();
        let hash = blake3::hash(id.0.as_bytes()).to_hex().to_string();
        let title = format!("Ordered {index:02}");
        sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,sort_order,original_hash,original_name,original_bytes,mime,kind,posted_at,taken_at) VALUES($1,$2,$3,$4,$5,$6,'photo.jpeg',0,'image/jpeg','image','2026-06-01'::timestamptz-$5::integer*interval '1 day','2026-01-01'::timestamptz+$5::integer*interval '1 day')",id as _,short,f.album as _,title,index,hash).execute(&mut *c).await.unwrap();
    }
    // A hidden photo sorts before every visible one. It must not consume a slot.
    sqlx::query!(
        "UPDATE photos SET title='Hidden',visibility='restricted',sort_order=-1 WHERE id=$1",
        f.photo as _
    )
    .execute(&mut *c)
    .await
    .unwrap();
    drop(c);
    let (_, path) = f
        .link(
            ShareTarget::Album(f.album),
            None,
            LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        )
        .await;
    let mut client = Client::default();
    for (sort, first, last) in [
        ("taken", 61, 2),
        ("posted", 0, 59),
        ("name", 0, 59),
        ("manual", 0, 59),
        ("invalid", 61, 2),
    ] {
        let page = client
            .get(&f.app, &format!("{path}?sort={sort}&per_page=1000"))
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        let grid = page
            .text()
            .split("class=\"photo-grid\"")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        assert_eq!(grid.matches("<picture>").count(), 60);
        assert!(
            grid.find(&format!("Ordered {first:02}")).unwrap()
                < grid.find(&format!("Ordered {last:02}")).unwrap()
        );
        assert!(!grid.contains("alt=\"Hidden\""));
        assert!(page.text().contains("62 photos"));
        assert!(page.text().contains("/a/shared?page=2&#38;sort="));
        assert!(!page.text().contains(&hidden_image));
    }
    let page = client
        .get(&f.app, &format!("{path}?page=2&sort=manual"))
        .await;
    assert!(page.text().contains("Ordered 60"));
    assert!(page.text().contains("Ordered 61"));
    for query in ["?page=0", "?page=3", "?page=invalid"] {
        let reply = client.get(&f.app, &format!("{path}{query}")).await;
        concealed(&reply).await;
        assert_eq!(reply.headers[header::CACHE_CONTROL], "private, no-store");
    }
    let second = client.get(&f.app, "/a/shared?page=2&sort=manual").await;
    assert!(second.text().contains("Ordered 61"));
    let child_page = client.get(&f.app, "/a/shared/child").await;
    assert_eq!(child_page.status, StatusCode::OK, "{}", child_page.text());
    assert!(child_page.text().contains(&child_image));
    let detail = client
        .get(
            &f.app,
            &format!("/p/{}", yhotos_core::ShortId::from_photo_id(child_photo)),
        )
        .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(detail.text().contains("<picture>"));
    assert!(!detail.text().contains("Download photo"));
    concealed(&client.get(&f.app, "/a/shared/hidden-child").await).await;
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn photo_landing_uses_photo_template_and_hides_disabled_downloads(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let public = f.album(None, "single", Visibility::Public).await;
    let (photo, image, _) = Fixture::add_photo(&f.pool, &f.state, public, "Single photo").await;
    for enabled in [false, true] {
        let (_, path) = f
            .link(
                ShareTarget::Photo(photo),
                None,
                LinkFlags {
                    view: true,
                    download: enabled,
                    like: enabled,
                    flag: enabled,
                },
            )
            .await;
        let mut client = Client::default();
        let page = client.get(&f.app, &path).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        assert_eq!(page.headers[header::CACHE_CONTROL], "private, no-store");
        assert!(page.text().contains("class=\"photo-page\""));
        assert!(page.text().contains("<picture>"));
        assert!(page.text().contains("sizes=\"100vw\""));
        assert!(page.text().contains(&image));
        assert!(page.text().contains("og:title"));
        assert!(!page.text().contains("/a/single"));
        assert_eq!(page.text().contains("Download photo"), enabled);
        assert_eq!(page.text().contains("Like photo"), enabled);
        assert_eq!(page.text().contains("Flag photo"), enabled);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn page_controls_keep_each_links_capabilities_with_its_own_scope(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let album = f.album(None, "independent", Visibility::Public).await;
    let (photo, _, original) =
        Fixture::add_photo(&f.pool, &f.state, album, "Restricted photo").await;
    photos::set_visibility(
        &mut f.pool.acquire().await.unwrap(),
        &[photo],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    let (_, album_link) = f.link(ShareTarget::Album(album), None, flags()).await;
    let (_, photo_link) = f
        .link(
            ShareTarget::Photo(photo),
            None,
            LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        )
        .await;
    let mut client = Client::default();
    assert_eq!(client.get(&f.app, &album_link).await.status, StatusCode::OK);
    assert_eq!(client.get(&f.app, &photo_link).await.status, StatusCode::OK);
    client
        .jar
        .insert("__Host-ys".into(), f.account(false).await);
    let path = format!("/p/{}", yhotos_core::ShortId::from_photo_id(photo));
    let page = client.get(&f.app, &path).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    for control in ["Download photo", "Like photo", "Flag photo"] {
        assert!(!page.text().contains(control));
    }
    concealed(&client.get(&f.app, &original).await).await;
    for action in ["like", "flag"] {
        concealed(&client.action(&f.app, photo, action).await).await;
    }
    grants::grant_photo(
        &mut f.pool.acquire().await.unwrap(),
        photo,
        Grant::User(f.creator),
    )
    .await
    .unwrap();
    f.invalidate();
    let page = client.get(&f.app, &path).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("Download photo"));
    assert_eq!(client.get(&f.app, &original).await.status, StatusCode::OK);
    for action in ["like", "flag"] {
        assert_eq!(
            client.action(&f.app, photo, action).await.status,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn share_short_circuits_and_fallbacks_are_always_private() {
    let state = crate::tests::state(false, 1);
    let app = crate::public_router(state);
    let mut client = Client::default();
    let missing = client.get(&app, "/s/unknown/nested").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.headers[header::CACHE_CONTROL], "private, no-store");
    let limited = client.get(&app, "/s/unknown/nested").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers[header::CACHE_CONTROL], "private, no-store");
    let app = crate::public_router(crate::tests::state(false, 100));
    let method = client
        .send(
            &app,
            Method::OPTIONS,
            "/s/unknown",
            String::new(),
            false,
            false,
        )
        .await;
    assert_eq!(method.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(method.headers[header::CACHE_CONTROL], "private, no-store");
    let csrf = client
        .send(
            &app,
            Method::POST,
            "/s/unknown/unlock",
            String::new(),
            false,
            false,
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    assert_eq!(csrf.headers[header::CACHE_CONTROL], "private, no-store");
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn one_cookie_seventeen_landings_home_and_sixty_link_lru(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let other = f.album(None, "other", Visibility::Restricted).await;
    let (_, other_image, _) = Fixture::add_photo(&f.pool, &f.state, other, "Other scope").await;
    let mut client = Client::default();
    // Old cookies, including malformed values, never authorize or block browsing.
    for index in 0..80 {
        client
            .jar
            .insert(format!("yl_{index:012}"), "malformed".into());
    }
    client
        .jar
        .insert("yl_old-name.with-dots".into(), "ignored".into());
    assert_eq!(client.get(&f.app, "/").await.status, StatusCode::OK);
    let mut visited = Vec::new();
    let mut bearer = None;
    for index in 0..60 {
        let target = if index == 0 { f.album } else { other };
        let (row, path) = f.link(ShareTarget::Album(target), None, flags()).await;
        let page = client.get(&f.app, &path).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        assert!(!client.jar.keys().any(|k| k.starts_with("yl_")));
        assert_eq!(
            client
                .jar
                .keys()
                .filter(|k| k.as_str() == COOKIE_NAME)
                .count(),
            1
        );
        let current = client.jar[COOKIE_NAME].clone();
        assert_eq!(bearer.get_or_insert_with(|| current.clone()), &current);
        visited.push((row.id, path));
        if index == 16 {
            assert_eq!(client.get(&f.app, "/").await.status, StatusCode::OK);
            assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
        }
        if index == 49 {
            // A revisit after the coalescing window determines eviction order.
            sqlx::query!("UPDATE share_session_links SET unlocked_at=clock_timestamp()-interval '2 minutes' WHERE share_link_id=$1",visited[5].0 as _)
                .execute(&f.pool).await.unwrap();
            f.invalidate();
            assert_eq!(
                client.get(&f.app, &visited[5].1).await.status,
                StatusCode::OK
            );
        }
    }
    let hash = digest(&client.jar[COOKIE_NAME], TokenKind::Session).unwrap();
    let mut c = f.pool.acquire().await.unwrap();
    let session = share_links::load_session(&mut c, &hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.links.len(), 50);
    let sessions = sqlx::query_scalar!("SELECT count(*) FROM sessions WHERE is_share")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    assert_eq!(sessions, Some(1));
    for (index, (id, _)) in visited.iter().enumerate() {
        assert_eq!(
            session.links.iter().any(|l| l.link_id == *id),
            index == 5 || index >= 11
        );
    }
    drop(c);
    assert_eq!(client.get(&f.app, "/").await.status, StatusCode::OK);
    concealed(&client.get(&f.app, &f.image).await).await;
    assert_eq!(
        client.get(&f.app, &other_image).await.status,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn invalid_share_cookie_is_cleared_without_blocking_home(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let mut client = Client::default();
    for value in [
        "invalid".to_owned(),
        Token::generate().encode(TokenKind::Session),
    ] {
        client.jar.insert(COOKIE_NAME.into(), value);
        let reply = client.get(&f.app, "/").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!client.jar.contains_key(COOKIE_NAME));
        assert!(reply.headers.get_all(header::SET_COOKIE).iter().any(|h| {
            let h = h.to_str().unwrap();
            h.starts_with(COOKIE_NAME) && h.contains("Max-Age=0")
        }));
    }
    let (_, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    client.get(&f.app, &path).await;
    // Unrelated and duplicate legacy names must not cause a 400 or extra lookups.
    for index in 0..80 {
        client.jar.insert(format!("ignored_{index}"), "x".into());
    }
    assert_eq!(client.get(&f.app, "/").await.status, StatusCode::OK);
    assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn changed_passphrase_and_expiry_remove_only_their_memberships(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let other = f.album(None, "remaining", Visibility::Restricted).await;
    let (_, other_image, _) = Fixture::add_photo(&f.pool, &f.state, other, "Remaining").await;
    let (changed, changed_path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let (expired, expired_path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let (remaining, path) = f.link(ShareTarget::Album(other), None, flags()).await;
    let mut client = Client::default();
    for path in [&changed_path, &expired_path, &path] {
        assert_eq!(client.get(&f.app, path).await.status, StatusCode::OK);
    }
    let password = yhotos_auth::generate_temp_password();
    let phc = yhotos_auth::PasswordHasher::for_tests()
        .hash_password(&passphrase(password.expose_secret()))
        .unwrap();
    let mut c = f.pool.acquire().await.unwrap();
    share_links::set_passphrase(&mut c, changed.id, Some(phc.as_str()))
        .await
        .unwrap();
    share_links::set_expiry(
        &mut c,
        expired.id,
        Some(OffsetDateTime::now_utc() - time::Duration::seconds(1)),
    )
    .await
    .unwrap();
    drop(c);
    f.invalidate();
    concealed(&client.get(&f.app, &f.image).await).await;
    assert_eq!(
        client.get(&f.app, &other_image).await.status,
        StatusCode::OK
    );
    let hash = digest(&client.jar[COOKIE_NAME], TokenKind::Session).unwrap();
    let session = share_links::load_session(&mut f.pool.acquire().await.unwrap(), &hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.links.len(), 1);
    assert_eq!(session.links[0].link_id, remaining.id);
    let removed = sqlx::query_scalar!(
        "SELECT count(*) FROM share_session_links WHERE share_link_id=ANY($1)",
        &[changed.id.0, expired.id.0]
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(removed, Some(0));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn nondefault_landing_and_expired_caches_never_load_a_page_for_an_image(pool: PgPool) {
    crate::files::tests::init_capture();
    let f = Fixture::new(pool).await;
    let (_, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let hash = f.original.split('/').nth(2).unwrap();
    photos::owner_of_hash(&mut f.state.pool.acquire().await.unwrap(), hash)
        .await
        .unwrap();
    let mut client = Client::default();
    let landing = client.get(&f.app, &format!("{path}?sort=name")).await;
    assert_eq!(landing.status, StatusCode::OK);
    let (image, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(queries, 1); // Task 008 lookup only, despite a nondefault landing sort.
    let (_, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    assert_eq!(queries, 0);
    // Exercise real TTL expiry, independently of revocation/explicit invalidation.
    tokio::time::sleep(std::time::Duration::from_secs(31)).await;
    let capture = Capture::default();
    let observer = capture.clone();
    f.state.files.invalidate_all();
    let image = client
        .get(&f.app, &f.image)
        .with_subscriber(tracing_subscriber::registry().with(capture))
        .await;
    assert_eq!(image.status, StatusCode::OK);
    let queries = observer
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("sqlx::query"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(queries.len(), 2, "{queries:?}"); // One small session-set read plus task 008.
    assert!(
        queries.iter().any(|s| s.contains("current_session")),
        "{queries:?}"
    );
    assert!(
        queries
            .iter()
            .all(|s| !s.contains("album_payload") && !s.contains("shared_link AS")),
        "{queries:?}"
    );
    let (_, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    assert_eq!(queries, 0);
    // Every landing, including a warm nondefault page, reseeds the scope caches.
    client.get(&f.app, &format!("{path}?sort=manual")).await;
    f.state.files.invalidate_all();
    let (_, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    assert_eq!(queries, 1);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn concurrent_unlocks_keep_the_database_set_bounded(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (first, _) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let hash = *Token::generate().hash(TokenKind::Session).as_bytes();
    let mut c = f.pool.acquire().await.unwrap();
    let mut tx = c.begin().await.unwrap();
    let session = share_links::create_session(&mut tx, &first, &hash, &[0; 32], "other")
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    drop(c);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..59 {
        let (row, _) = f.link(ShareTarget::Album(f.album), None, flags()).await;
        let pool = f.pool.clone();
        tasks.spawn(async move {
            let mut c = pool.acquire().await.unwrap();
            let mut tx = c.begin().await.unwrap();
            assert!(
                share_links::unlock_session_link(&mut tx, session.id, &row)
                    .await
                    .unwrap()
            );
            tx.commit().await.unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    let members = sqlx::query_scalar!(
        "SELECT count(*) FROM share_session_links WHERE session_id=$1",
        session.id
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(members, Some(50));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn passphrase_rotation_between_resolution_and_landing_cannot_restore_membership(
    pool: PgPool,
) {
    let f = Fixture::new(pool).await;
    let old = yhotos_auth::generate_temp_password();
    let new = yhotos_auth::generate_temp_password();
    let (link, path) = f
        .link(
            ShareTarget::Album(f.album),
            Some(old.expose_secret()),
            flags(),
        )
        .await;
    let mut client = Client::default();
    client.get(&f.app, &path).await;
    assert_eq!(
        client
            .unlock(&f.app, &path, old.expose_secret())
            .await
            .status,
        StatusCode::OK
    );
    let hash = digest(&client.jar[COOKIE_NAME], TokenKind::Session).unwrap();
    let phc = yhotos_auth::PasswordHasher::for_tests()
        .hash_password(&passphrase(new.expose_secret()))
        .unwrap();
    let pool = f.pool.clone();
    let state = f.state.clone();
    // The normal stack resolves the old session first. This inner layer then
    // rotates the passphrase before the landing takes its metadata snapshot.
    let router = routes().layer(axum::middleware::from_fn(
        move |Extension(links): Extension<Links>,
              request: Request,
              next: axum::middleware::Next| {
            let pool = pool.clone();
            let state = state.clone();
            let phc = phc.clone();
            async move {
                assert!(links.0.iter().any(
                    |l| l.link_id == link.id && l.passphrase_version == link.passphrase_version
                ));
                share_links::set_passphrase(
                    &mut pool.acquire().await.unwrap(),
                    link.id,
                    Some(phc.as_str()),
                )
                .await
                .unwrap();
                state.auth.as_ref().unwrap().shares.invalidate();
                next.run(request).await
            }
        },
    ));
    let app = crate::stack(router, f.state.clone(), Audience(false));
    let page = client.get(&app, &path).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.text().contains(&f.image));
    let mut c = f.pool.acquire().await.unwrap();
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM share_session_links WHERE share_link_id=$1",
        link.id as _
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(count, Some(0));
    // Even the explicit recency path is update-only and cannot restore it.
    let session = share_links::load_session(&mut c, &hash)
        .await
        .unwrap()
        .unwrap();
    share_links::record_open_with_recency(
        &mut c,
        session.id,
        link.id,
        link.passphrase_version + 1,
        &hash,
    )
    .await
    .unwrap();
    assert!(
        share_links::load_session(&mut c, &hash)
            .await
            .unwrap()
            .unwrap()
            .links
            .is_empty()
    );
    drop(c);
    concealed(&client.get(&f.app, &f.image).await).await;
    assert_eq!(
        client
            .unlock(&f.app, &path, old.expose_secret())
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .unlock(&f.app, &path, new.expose_secret())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
    let membership = sqlx::query!("SELECT sl.passphrase_version,l.passphrase_version AS current_version FROM share_session_links sl JOIN share_links l ON l.id=sl.share_link_id WHERE l.id=$1",link.id as _).fetch_one(&f.pool).await.unwrap();
    assert_eq!(membership.passphrase_version, membership.current_version);
    assert_eq!(membership.current_version, link.passphrase_version + 1);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn unlock_verifies_current_passphrase_while_holding_the_link_lock(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let old = yhotos_auth::generate_temp_password();
    let new = yhotos_auth::generate_temp_password();
    let (link, path) = f
        .link(
            ShareTarget::Album(f.album),
            Some(old.expose_secret()),
            flags(),
        )
        .await;
    let mut client = Client::default();
    client.get(&f.app, &path).await; // Cache the old hash.
    let mut c = f.pool.acquire().await.unwrap();
    let mut tx = c.begin().await.unwrap();
    share_links::get_for_update(&mut tx, link.id)
        .await
        .unwrap()
        .unwrap();
    let request = client.unlock(&f.app, &path, new.expose_secret());
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut request)
            .await
            .is_err()
    );
    let phc = yhotos_auth::PasswordHasher::for_tests()
        .hash_password(&passphrase(new.expose_secret()))
        .unwrap();
    share_links::set_passphrase(&mut tx, link.id, Some(phc.as_str()))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(request.await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn fractional_expiry_keeps_router_queries_bounded(pool: PgPool) {
    crate::files::tests::init_capture();
    let f = Fixture::new(pool).await;
    let (link, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let other = f.album(None, "surviving", Visibility::Restricted).await;
    let (_, surviving) = f.link(ShareTarget::Album(other), None, flags()).await;
    let expiry = (OffsetDateTime::now_utc() + time::Duration::seconds(5))
        .replace_nanosecond(900_000_000)
        .unwrap();
    share_links::set_expiry(&mut f.pool.acquire().await.unwrap(), link.id, Some(expiry))
        .await
        .unwrap();
    f.invalidate();
    let mut client = Client::default();
    client.get(&f.app, &path).await;
    client.get(&f.app, &surviving).await;
    assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
    let hash = digest(&client.jar[COOKIE_NAME], TokenKind::Session).unwrap();
    let session = f
        .state
        .auth
        .as_ref()
        .unwrap()
        .shares
        .session(&f.state, hash)
        .await
        .unwrap()
        .unwrap();
    let member = session.links.iter().find(|m| m.link_id == link.id).unwrap();
    assert_eq!(
        i128::from(member.expires_at.unwrap()),
        expiry.unix_timestamp_nanos() / 1000
    );
    let fractional_window = expiry - time::Duration::milliseconds(800);
    tokio::time::sleep(
        (fractional_window - OffsetDateTime::now_utc())
            .max(time::Duration::ZERO)
            .unsigned_abs(),
    )
    .await;
    for _ in 0..3 {
        let (reply, queries) = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            crate::files::tests::count_queries(client.get(&f.app, &f.image)),
        )
        .await
        .unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(queries, 0);
    }
    tokio::time::sleep(
        (expiry + time::Duration::milliseconds(10) - OffsetDateTime::now_utc())
            .max(time::Duration::ZERO)
            .unsigned_abs(),
    )
    .await;
    let (reply, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    concealed(&reply).await;
    assert_eq!(queries, 1); // Prune once, keeping the unrelated live membership.
    let (reply, queries) = crate::files::tests::count_queries(client.get(&f.app, &f.image)).await;
    concealed(&reply).await;
    assert_eq!(queries, 0);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn warm_protected_landings_only_write_opens_and_coalesce_recency(pool: PgPool) {
    crate::files::tests::init_capture();
    let f = Fixture::new(pool).await;
    let password = yhotos_auth::generate_temp_password();
    let (link, path) = f
        .link(
            ShareTarget::Album(f.album),
            Some(password.expose_secret()),
            flags(),
        )
        .await;
    let mut client = Client::default();
    client.get(&f.app, &path).await;
    client.unlock(&f.app, &path, password.expose_secret()).await;
    let capture = Capture::default();
    let observer = capture.clone();
    let before = sqlx::query_scalar!(
        "SELECT unlocked_at FROM share_session_links WHERE share_link_id=$1",
        link.id as _
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    for _ in 0..3 {
        assert_eq!(
            client
                .get(&f.app, &path)
                .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
                .await
                .status,
            StatusCode::OK
        );
    }
    let queries = observer
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("sqlx::query"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(queries.len(), 3, "{queries:?}");
    assert!(
        queries
            .iter()
            .all(|q| q.contains("INSERT INTO share_link_opens")),
        "{queries:?}"
    );
    let after = sqlx::query_scalar!(
        "SELECT unlocked_at FROM share_session_links WHERE share_link_id=$1",
        link.id as _
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    sqlx::query!("UPDATE share_session_links SET unlocked_at=clock_timestamp()-interval '2 minutes' WHERE share_link_id=$1",link.id as _).execute(&f.pool).await.unwrap();
    f.invalidate();
    // Resolve once to populate old recency; subsequent concurrent landings must
    // share a single refresh and must never reload the grant set.
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, client.cookie().parse().unwrap());
    resolve(&f.state, &headers).await.unwrap();
    snapshot(
        &f.state,
        path.strip_prefix("/s/").unwrap().into(),
        1,
        photos::PageSort::Taken,
    )
    .await
    .unwrap();
    observer.0.lock().unwrap().clear();
    let mut second = Client {
        jar: client.jar.clone(),
    };
    let mut third = Client {
        jar: client.jar.clone(),
    };
    async {
        let (a, b, c) = tokio::join!(
            client.get(&f.app, &path),
            second.get(&f.app, &path),
            third.get(&f.app, &path)
        );
        for reply in [a, b, c] {
            assert_eq!(reply.status, StatusCode::OK);
        }
    }
    .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
    .await;
    let queries = observer
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("sqlx::query"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(queries.len(), 3, "{queries:?}");
    assert_eq!(
        queries
            .iter()
            .filter(|q| q.contains("UPDATE share_session_links sl SET unlocked_at"))
            .count(),
        1
    );
    let (reply, queries) = crate::files::tests::count_queries(client.get(&f.app, &path)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(queries, 1);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn geo_views_honor_unlocked_album_share_cookies_and_account_union(pool: PgPool) {
    geo_share_visibility(pool, false).await;
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn geo_views_honor_unlocked_photo_share_cookies_and_account_union(pool: PgPool) {
    geo_share_visibility(pool, true).await;
}

async fn geo_share_visibility(pool: PgPool, single_photo: bool) {
    let f = Fixture::new(pool).await;
    let public_album = f.album(None, "public-geo", Visibility::Public).await;
    let (public, _, _) = Fixture::add_photo(&f.pool, &f.state, public_album, "Public geo").await;
    let account_album = f.album(None, "account-geo", Visibility::Restricted).await;
    let (account, _, _) = Fixture::add_photo(&f.pool, &f.state, account_album, "Account geo").await;
    let hidden_album = f.album(None, "hidden-geo", Visibility::Restricted).await;
    let (hidden, _, _) = Fixture::add_photo(&f.pool, &f.state, hidden_album, "Hidden geo").await;
    let (shared, target) = if single_photo {
        let (photo, _, _) = Fixture::add_photo(&f.pool, &f.state, public_album, "Shared geo").await;
        photos::set_visibility(
            &mut f.pool.acquire().await.unwrap(),
            &[photo],
            Visibility::Restricted,
        )
        .await
        .unwrap();
        (photo, ShareTarget::Photo(photo))
    } else {
        (f.photo, ShareTarget::Album(f.album))
    };
    let place = Uuid::now_v7();
    let hidden_place = Uuid::now_v7();
    let mut conn = f.pool.acquire().await.unwrap();
    sqlx::query!("INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Visible city','CA',43.65,-79.38),($2,'Hidden city','CA',43.65,-79.38)",place,hidden_place)
        .execute(&mut *conn).await.unwrap();
    sqlx::query!("UPDATE photos SET gps_lat=43.65,gps_lon=-79.38,place_id=$1,taken_at='2026-01-02 00:00:00+00'",place)
        .execute(&mut *conn).await.unwrap();
    sqlx::query!(
        "UPDATE photos SET place_id=$1 WHERE id=$2",
        hidden_place,
        hidden as _
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    grants::grant_album(&mut conn, account_album, Grant::User(f.creator))
        .await
        .unwrap();
    drop(conn);
    let password = yhotos_auth::generate_temp_password();
    let (_, path) = f
        .link(target, Some(password.expose_secret()), flags())
        .await;
    let mut anonymous = Client::default();
    assert_geo_views(&f, &mut anonymous, place, &[public], false).await;
    let mut client = Client::default();
    assert_eq!(client.get(&f.app, &path).await.status, StatusCode::OK);
    // A passphrase form and its CSRF cookie confer no share visibility.
    assert!(!client.jar.contains_key(COOKIE_NAME));
    assert_geo_views(&f, &mut client, place, &[public], true).await;
    assert_eq!(
        client
            .unlock(&f.app, &path, password.expose_secret())
            .await
            .status,
        StatusCode::OK
    );
    assert!(client.jar.contains_key(COOKIE_NAME));
    assert_geo_views(&f, &mut client, place, &[shared], true).await;
    concealed(&client.get(&f.app, &format!("/places/{hidden_place}")).await).await;
    client
        .jar
        .insert("__Host-ys".into(), f.account(false).await);
    assert_geo_views(&f, &mut client, place, &[public, account, shared], true).await;
    concealed(&client.get(&f.app, &format!("/places/{hidden_place}")).await).await;
    // The scoped visitor does not alter subsequent public anonymous results.
    assert_geo_views(&f, &mut anonymous, place, &[public], false).await;
}

async fn assert_geo_views(
    f: &Fixture,
    client: &mut Client,
    place: Uuid,
    expected: &[PhotoId],
    private: bool,
) {
    let count = expected.len();
    for path in [
        "/map".to_owned(),
        format!("/places/{place}"),
        "/calendar".into(),
        "/calendar/2026".into(),
        "/calendar/2026/1".into(),
        "/calendar/2026/1/2".into(),
        "/map/tiles.json?bbox=-80,43,-79,44&zoom=1".into(),
        "/map/tiles.json?bbox=-80,43,-79,44&zoom=15".into(),
        "/map/photos.json?lon=-79.38&lat=43.65".into(),
    ] {
        let reply = client.get(&f.app, &path).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}: {}", reply.text());
        let policy = if private {
            "private, no-store"
        } else if path.contains(".json") {
            "public, max-age=300"
        } else {
            "public, max-age=60, stale-while-revalidate=600"
        };
        assert_eq!(reply.headers[header::CACHE_CONTROL], policy, "{path}");
        assert_eq!(reply.headers[header::VARY], "Cookie", "{path}");
        if path.starts_with("/map/tiles.json") {
            let json: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(json["features"].as_array().unwrap().len(), 1);
            assert_eq!(json["features"][0]["properties"]["count"], count);
            let representative = json["features"][0]["properties"]["short_id"]
                .as_str()
                .unwrap();
            assert!(
                expected.iter().any(
                    |id| yhotos_core::ShortId::from_photo_id(*id).to_string() == representative
                )
            );
        } else if path.starts_with("/map/photos.json") {
            let json: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(json["total"], count);
            assert_eq!(json["photos"].as_array().unwrap().len(), count);
            for id in expected {
                assert!(
                    json["photos"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| p["short_id"]
                            == yhotos_core::ShortId::from_photo_id(*id).to_string())
                );
            }
        } else {
            assert!(
                reply.text().contains(&format!("{count} photos")),
                "{path}: {}",
                reply.text()
            );
            assert!(!reply.text().contains("Hidden city"));
            if path.starts_with("/places/") || path.starts_with("/calendar/2026/1") {
                let suffix = if path.starts_with("/places/") {
                    "?sort=taken"
                } else {
                    ""
                };
                for id in expected {
                    assert!(
                        reply.text().contains(&format!(
                            "href=\"/p/{}{suffix}\"",
                            yhotos_core::ShortId::from_photo_id(*id)
                        )),
                        "{path}"
                    );
                }
                // Every grid entry belongs to the expected union; no hidden siblings.
                let grid_count = if path.starts_with("/places/") {
                    reply.text().matches("class=\"cell\"").count()
                } else {
                    reply.text().matches("href=\"/p/").count()
                };
                assert_eq!(grid_count, count, "{path}");
            }
        }
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn retained_photo_link_loses_capabilities_when_its_album_becomes_restricted(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let album = f.album(None, "later-restricted", Visibility::Public).await;
    let (photo, _, _) = Fixture::add_photo(&f.pool, &f.state, album, "Linked photo").await;
    let (_, photo_link) = f.link(ShareTarget::Photo(photo), None, flags()).await;
    let (_, album_link) = f
        .link(
            ShareTarget::Album(album),
            None,
            LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        )
        .await;
    let mut client = Client::default();
    assert_eq!(client.get(&f.app, &photo_link).await.status, StatusCode::OK);
    assert_eq!(client.get(&f.app, &album_link).await.status, StatusCode::OK);
    // While the album is public, the photo link independently covers the photo.
    assert_eq!(
        client.action(&f.app, photo, "like").await.status,
        StatusCode::OK
    );
    sqlx::query!(
        "UPDATE albums SET visibility='restricted'::visibility WHERE id=$1",
        album as _
    )
    .execute(&f.pool)
    .await
    .unwrap();
    f.invalidate();
    // The view-only album link still shows the photo, but the retained photo link
    // no longer covers it, so its like and flag capabilities must not be borrowed.
    let page = client
        .get(
            &f.app,
            &format!("/p/{}", yhotos_core::ShortId::from_photo_id(photo)),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    for control in ["Download photo", "Like photo", "Flag photo"] {
        assert!(!page.text().contains(control), "{control}");
    }
    for action in ["like", "flag"] {
        concealed(&client.action(&f.app, photo, action).await).await;
    }
}

#[cfg(feature = "admin")]
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn renamed_album_redirects_use_validated_share_scope(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let child = f.album(Some(f.album), "child", Visibility::Public).await;
    let hidden = f
        .album(Some(f.album), "hidden", Visibility::Restricted)
        .await;
    let outside = f.album(None, "outside", Visibility::Restricted).await;
    let (_, link) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let mut client = Client::default();
    assert_eq!(client.get(&f.app, &link).await.status, StatusCode::OK);
    assert!(!client.jar.contains_key("__Host-ys"));
    for (id, title, visibility) in [
        (f.album, "Renamed", "restricted"),
        (child, "New child", "public"),
        (hidden, "New hidden", "restricted"),
        (outside, "New outside", "restricted"),
    ] {
        let command = serde_json::from_value(serde_json::json!({"op":"album_edit","id":id,"title":title,"description":"","visibility":visibility,"manual_order":false,"grants":[]})).unwrap();
        yhotos_db::library::execute(&f.pool, f.creator, &command)
            .await
            .unwrap();
    }
    for (old, new) in [
        ("/a/shared", "/a/renamed"),
        ("/a/shared/child", "/a/renamed/new-child"),
    ] {
        let response = client.get(&f.app, old).await;
        assert_eq!(response.status, StatusCode::MOVED_PERMANENTLY, "{old}");
        assert_eq!(response.headers[header::LOCATION], new);
        assert_eq!(response.headers[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(client.get(&f.app, new).await.status, StatusCode::OK);
    }
    for path in ["/a/outside", "/a/shared/hidden", "/a/unknown"] {
        concealed(&client.get(&f.app, path).await).await;
    }
    // A photo capability cannot redirect to its containing album, and a link's
    // target cannot bypass a restricted ancestor added above it later.
    let (_, photo_link) = f.link(ShareTarget::Photo(f.photo), None, flags()).await;
    let mut photo_client = Client::default();
    concealed(&photo_client.get(&f.app, &photo_link).await).await;
    concealed(&photo_client.get(&f.app, "/a/shared").await).await;
    sqlx::query!(
        "UPDATE albums SET parent_id=$2 WHERE id=$1",
        f.album.0,
        outside.0
    )
    .execute(&f.pool)
    .await
    .unwrap();
    concealed(&client.get(&f.app, "/a/shared").await).await;
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn lagging_database_clock_cannot_loop_or_keep_an_expired_grant(pool: PgPool) {
    use crate::files::tests::count_queries;
    crate::files::tests::init_capture();
    let f = Fixture::new(pool).await;
    let (expiring, path) = f.link(ShareTarget::Album(f.album), None, flags()).await;
    let other = f.album(None, "still-live", Visibility::Restricted).await;
    let (_, other_image, _) = Fixture::add_photo(&f.pool, &f.state, other, "Live photo").await;
    let (_, live) = f.link(ShareTarget::Album(other), None, flags()).await;
    // Live for another hour by the database's clock.
    let expiry = OffsetDateTime::now_utc() + time::Duration::hours(1);
    share_links::set_expiry(
        &mut f.pool.acquire().await.unwrap(),
        expiring.id,
        Some(expiry),
    )
    .await
    .unwrap();
    f.invalidate();
    let mut both = Client::default();
    both.get(&f.app, &path).await;
    both.get(&f.app, &live).await;
    let mut only = Client::default();
    only.get(&f.app, &path).await;
    for client in [&mut both, &mut only] {
        assert_eq!(client.get(&f.app, &f.image).await.status, StatusCode::OK);
    }
    // This process now runs two hours ahead of the database: the link is expired here
    // but every reload still returns it.
    let shares = &f.state.auth.as_ref().unwrap().shares;
    shares.set_clock_skew_for_tests(2 * 3600 * 1_000_000);
    for _ in 0..3 {
        let (reply, queries) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            count_queries(both.get(&f.app, &f.image)),
        )
        .await
        .expect("an expiry reload must never loop");
        concealed(&reply).await;
        assert_eq!(queries, 1, "exactly one reload per request");
    }
    // The unrelated live grant is preserved.
    assert_eq!(both.get(&f.app, &other_image).await.status, StatusCode::OK);
    // A session whose only grant is the expired one gets nothing.
    concealed(&only.get(&f.app, &f.image).await).await;
    concealed(&only.get(&f.app, &other_image).await).await;
}
