#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Real-Postgres tests exercise handlers through the complete Tower stack.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use sqlx::{ConnectOptions, Connection, PgPool};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};
use yhotos_auth::{
    Token, TokenKind, totp,
    webauthn::{CreationChallengeResponse, RequestChallengeResponse, Url},
};
use yhotos_core::UserId;
use yhotos_db::{DatabaseTls, Db, Role, mfa, sessions, users};
use yhotos_http::{AppState, auth::Auth, public_router};

struct Client {
    app: Router,
    state: AppState,
    pool: PgPool,
    db: Arc<Db>,
    jar: BTreeMap<String, String>,
    network: SocketAddr,
    origin: String,
}
struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}
impl Client {
    async fn new(pool: PgPool, admin: bool) -> Self {
        let url = SecretString::from(pool.connect_options().to_url_lossy().to_string());
        let db = Arc::new(
            Db::connect(&url, Role::Public, 1, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let mut key = [0; 32];
        getrandom::fill(&mut key).unwrap();
        let config = Box::leak(Box::new(
            yhotos_config::Config::from_figment(
                figment::Figment::new()
                    .merge(("database_url_public", url.expose_secret()))
                    .merge((
                        "data_dir",
                        std::env::current_dir().unwrap().to_string_lossy().as_ref(),
                    ))
                    .merge(("public_base_url", "https://example.test"))
                    .merge(("admin_listen", "127.0.0.1:8081"))
                    .merge(("admin_base_url", "https://admin.example.test:8443"))
                    .merge(("database_url_admin", "postgres://127.0.0.1/test"))
                    .merge(("session_key", STANDARD.encode(key)))
                    .merge(("rate_limits.auth_per_min", 1000)),
            )
            .unwrap(),
        ));
        #[cfg(feature = "admin")]
        let admin_db = Some(Arc::new(
            Db::connect(&url, Role::Admin, 1, DatabaseTls::Disable)
                .await
                .unwrap(),
        ));
        #[cfg(not(feature = "admin"))]
        let admin_db = None;
        let state = AppState {
            config,
            auth: Some(Auth::new(config, db.clone(), admin_db).await.unwrap()),
        };
        #[cfg(feature = "admin")]
        let app = if admin {
            yhotos_http::admin_router(state.clone())
        } else {
            public_router(state.clone())
        };
        #[cfg(not(feature = "admin"))]
        let app = {
            assert!(!admin);
            public_router(state.clone())
        };
        Self {
            app,
            state,
            pool,
            db,
            jar: BTreeMap::new(),
            network: "127.0.0.1:1234".parse().unwrap(),
            origin: if admin {
                "https://admin.example.test:8443"
            } else {
                "https://example.test"
            }
            .into(),
        }
    }
    #[cfg(feature = "admin")]
    fn listener(&self, admin: bool) -> Self {
        Self {
            app: if admin {
                yhotos_http::admin_router(self.state.clone())
            } else {
                public_router(self.state.clone())
            },
            state: self.state.clone(),
            pool: self.pool.clone(),
            db: self.db.clone(),
            jar: BTreeMap::new(),
            network: self.network,
            origin: if admin {
                "https://admin.example.test:8443"
            } else {
                "https://example.test"
            }
            .into(),
        }
    }
    async fn enroll_passkey(&mut self, authenticator: &mut WebauthnAuthenticator<SoftToken>) {
        let prefix = if self.origin == "https://example.test" {
            ""
        } else {
            "/admin"
        };
        let start = self
            .json(
                &format!("{prefix}/u/security/passkey/start"),
                serde_json::json!({}),
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.body);
        let challenge: CreationChallengeResponse = serde_json::from_str(&start.body).unwrap();
        let registration = authenticator
            .do_registration(Url::parse(&self.origin).unwrap(), challenge)
            .unwrap();
        let finish = self
            .json(
                &format!("{prefix}/u/security/passkey/finish"),
                serde_json::to_value(registration).unwrap(),
            )
            .await;
        assert_eq!(finish.status, StatusCode::OK, "{}", finish.body);
    }
    async fn enroll_totp(&mut self) -> totp::Secret {
        let admin = self.origin != "https://example.test";
        let base = if admin {
            "/admin/u/security"
        } else {
            "/u/security"
        };
        let reply = self.get(&format!("{base}/totp")).await;
        assert_eq!(reply.status, StatusCode::OK);
        let secret = self.pending_totp_secret(admin).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let code = totp::generate(&secret, now).unwrap();
        location(
            &self
                .post(&format!("{base}/totp"), &[("code", code.expose_secret())])
                .await,
            base,
        );
        secret
    }
    async fn pending_totp_secret(&self, admin: bool) -> totp::Secret {
        let binding = self.session(admin).await.id;
        let pending = sqlx::query_scalar!(
            "SELECT state FROM pending_webauthn WHERE binding_id=$1 AND kind='totp'",
            binding
        )
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let raw = STANDARD
            .decode(self.state.config.session_key.expose_secret())
            .unwrap();
        let key = blake3::derive_key("yhotos MFA encryption v1", &raw);
        let ring =
            yhotos_auth::Keyring::from_master(&SecretString::from(STANDARD.encode(key))).unwrap();
        let id: Uuid = serde_json::from_value(pending["id"].clone()).unwrap();
        let sealed: Vec<u8> = serde_json::from_value(pending["sealed"].clone()).unwrap();
        totp::Secret::from_bytes(
            ring.open(id.as_bytes(), &sealed)
                .unwrap()
                .try_into()
                .unwrap(),
        )
    }
    async fn send(&mut self, method: &str, path: &str, body: String, json: bool) -> Reply {
        let mut builder = Request::builder().method(method).uri(path).header(
            header::COOKIE,
            self.jar
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        );
        if method == "POST" {
            builder = builder.header(
                header::CONTENT_TYPE,
                if json {
                    "application/json"
                } else {
                    "application/x-www-form-urlencoded"
                },
            );
            if json {
                builder = builder
                    .header(header::ORIGIN, &self.origin)
                    .header("sec-fetch-site", "same-origin")
                    .header(
                        "x-csrf-token",
                        self.jar
                            .get("__Host-yhotos-csrf")
                            .cloned()
                            .unwrap_or_default(),
                    );
            }
        }
        let mut request = builder.body(Body::from(body)).unwrap();
        request.extensions_mut().insert(ConnectInfo(self.network));
        let response = self.app.clone().oneshot(request).await.unwrap();
        self.receive(response).await
    }
    async fn receive(&mut self, response: Response) -> Reply {
        let status = response.status();
        let headers = response.headers().clone();
        for cookie in headers.get_all(header::SET_COOKIE) {
            let cookie = cookie.to_str().unwrap();
            let (name, value) = cookie.split(';').next().unwrap().split_once('=').unwrap();
            if cookie.contains("Max-Age=0") {
                self.jar.remove(name);
            } else {
                self.jar.insert(name.into(), value.into());
            }
        }
        let body = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        Reply {
            status,
            headers,
            body,
        }
    }
    async fn get(&mut self, path: &str) -> Reply {
        self.send("GET", path, String::new(), false).await
    }
    async fn post(&mut self, path: &str, fields: &[(&str, &str)]) -> Reply {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair(
            "_csrf",
            self.jar
                .get("__Host-yhotos-csrf")
                .map_or("", String::as_str),
        );
        for (k, v) in fields {
            form.append_pair(k, v);
        }
        let body = form.finish();
        self.send("POST", path, body, false).await
    }
    async fn json(&mut self, path: &str, value: serde_json::Value) -> Reply {
        self.send("POST", path, value.to_string(), true).await
    }
    async fn seed(&self, forced: bool, admin: bool) -> (UserId, String, SecretString) {
        let username = Uuid::now_v7().simple().to_string();
        let password = yhotos_auth::generate_temp_password();
        let p = password.clone();
        let hash = tokio::task::spawn_blocking(move || yhotos_auth::hash_password(&p).unwrap())
            .await
            .unwrap();
        let mut c = self.pool.acquire().await.unwrap();
        let user = users::create(
            &mut c,
            &username,
            &SecretString::from(hash.as_str()),
            forced,
        )
        .await
        .unwrap();
        if admin {
            sqlx::query!("UPDATE users SET is_admin=true WHERE id=$1", user.id.0)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        (user.id, username, password)
    }
    async fn seed_totp(&self, user: UserId) -> totp::Secret {
        let raw = STANDARD
            .decode(self.state.config.session_key.expose_secret())
            .unwrap();
        let key = blake3::derive_key("yhotos MFA encryption v1", &raw);
        let ring =
            yhotos_auth::Keyring::from_master(&SecretString::from(STANDARD.encode(key))).unwrap();
        let secret = totp::generate_secret();
        let id = Uuid::now_v7();
        mfa::add(
            &mut self.pool.acquire().await.unwrap(),
            user,
            &mfa::Method {
                id,
                kind: "totp".into(),
                secret_enc: Some(ring.seal(id.as_bytes(), secret.expose_secret()).unwrap()),
                credential_id: None,
                public_key: None,
                sign_count: 0,
                label: "Authenticator app".into(),
                last_used_step: None,
                rp_id: None,
            },
        )
        .await
        .unwrap();
        secret
    }
    async fn login(&mut self, username: &str, password: &SecretString, admin: bool) -> Reply {
        let path = if admin { "/admin/login" } else { "/login" };
        self.get(path).await;
        self.post(
            path,
            &[
                ("username", username),
                ("password", password.expose_secret()),
            ],
        )
        .await
    }
    fn hash(&self, admin: bool) -> Vec<u8> {
        let value = self
            .jar
            .get(if admin { "ya" } else { "__Host-ys" })
            .unwrap();
        let (kind, token) = Token::decode(value).unwrap();
        assert_eq!(kind, TokenKind::Session);
        token.hash(kind).as_bytes().to_vec()
    }
    async fn session(&self, admin: bool) -> sessions::Session {
        sessions::get(&mut self.pool.acquire().await.unwrap(), &self.hash(admin))
            .await
            .unwrap()
            .unwrap()
    }
}
fn location(reply: &Reply, path: &str) {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.headers[header::LOCATION], path);
}
fn auth_headers(reply: &Reply) {
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(reply.headers[header::REFERRER_POLICY], "no-referrer");
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn login_rotation_logout_cookie_and_audit(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, password) = c.seed(false, false).await;
    let form = c.get("/login").await;
    assert_eq!(form.status, StatusCode::OK);
    auth_headers(&form);
    assert!(form.body.contains("name=\"_csrf\""));
    assert!(!form.body.contains("<script"));
    let response = c.login(&name, &password, false).await;
    location(&response, "/");
    let cookie = response
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .find(|v| v.to_str().unwrap().starts_with("__Host-ys="))
        .unwrap()
        .to_str()
        .unwrap();
    let bearer = c.jar.get("__Host-ys").unwrap().clone();
    assert_eq!(
        cookie,
        format!("__Host-ys={bearer}; Path=/; Secure; HttpOnly; SameSite=Lax")
    );
    assert!(!c.jar.contains_key("ya"));
    let old = c.hash(false);
    let page = c.get("/u/security").await;
    auth_headers(&page);
    assert!(page.body.contains("current session"));
    assert!(page.body.contains("nonce=\""));
    location(&c.login(&name, &password, false).await, "/");
    assert_ne!(old, c.hash(false));
    assert!(
        sessions::get(&mut c.pool.acquire().await.unwrap(), &old)
            .await
            .unwrap()
            .is_none()
    );
    c.get("/u/security").await;
    let live = c.jar.get("__Host-ys").unwrap().clone();
    location(&c.post("/logout", &[]).await, "/login");
    c.jar.insert("__Host-ys".into(), live);
    location(&c.get("/u/security").await, "/login");
    assert!(!c.jar.contains_key("__Host-ys"));
    let actions = sqlx::query_scalar!("SELECT action FROM audit_log WHERE user_id=$1", id.0)
        .fetch_all(&c.pool)
        .await
        .unwrap();
    assert_eq!(
        actions
            .iter()
            .filter(|a| a.as_str() == "login.success")
            .count(),
        2
    );
    assert!(actions.iter().any(|a| a == "session.revoke"));
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn failures_are_identical_and_fifth_failure_locks(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, _) = c.seed(false, false).await;
    let wrong = yhotos_auth::generate_temp_password();
    let unknown = Uuid::now_v7().to_string();
    let first = c.login(&unknown, &wrong, false).await;
    let second = c.login(&name, &wrong, false).await;
    assert_eq!(first.status, second.status);
    assert_eq!(first.body, second.body);
    for _ in 0..2 {
        assert_eq!(
            c.login(&name, &wrong, false).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let locked = c.login(&name, &wrong, false).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(locked.headers[header::RETRY_AFTER], "30");
    assert_eq!(locked.body, "Sign in failed");
    let blocked = c.login(&Uuid::now_v7().to_string(), &wrong, false).await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        blocked.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            <= 30
    );
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM audit_log WHERE action='login.failure'"#
    )
    .fetch_one(&c.pool)
    .await
    .unwrap();
    assert_eq!(count, 6);
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn forced_password_gate_rotation_and_other_sessions_revoked(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, pw) = c.seed(true, false).await;
    location(&c.login(&name, &pw, false).await, "/u/password");
    for path in ["/", "/missing", "/u/security", "/login", "/u/security/totp"] {
        location(&c.get(path).await, "/u/password");
    }
    let api = c.get("/api/v1/anything").await;
    assert_eq!(api.status, StatusCode::FORBIDDEN);
    assert_eq!(api.body, r#"{"error":"password_change_required"}"#);
    let old = c.hash(false);
    let mut other = c.session(false).await;
    other.id = Uuid::now_v7();
    other.token_hash = Token::generate()
        .hash(TokenKind::Session)
        .as_bytes()
        .to_vec();
    sessions::create(&mut c.pool.acquire().await.unwrap(), &other)
        .await
        .unwrap();
    c.get("/u/password").await;
    let new = yhotos_auth::generate_temp_password();
    location(
        &c.post("/u/password", &[("new_password", new.expose_secret())])
            .await,
        "/",
    );
    assert_ne!(old, c.hash(false));
    assert_eq!(c.get("/u/security").await.status, StatusCode::OK);
    let all = sessions::list(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_ne!(all.first().unwrap().id, other.id);
    let user = users::get_by_id(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap()
        .unwrap();
    assert!(!user.must_change_password);
    let failed = c
        .post(
            "/u/password",
            &[
                ("current_password", pw.expose_secret()),
                (
                    "new_password",
                    yhotos_auth::generate_temp_password().expose_secret(),
                ),
            ],
        )
        .await;
    assert_eq!(failed.status, StatusCode::UNAUTHORIZED);
    let count=sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM audit_log WHERE user_id=$1 AND action='password.change'"#,id.0).fetch_one(&c.pool).await.unwrap();
    assert_eq!(count, 1);
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn expiry_invalid_cookies_csrf_and_redirect_validation(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, pw) = c.seed(false, false).await;
    assert_eq!(
        c.send("POST", "/login", String::new(), false).await.status,
        StatusCode::FORBIDDEN
    );
    for next in [
        "//evil",
        "https://evil",
        "/\\evil",
        "javascript:",
        "/%2f%2fevil",
        "/..//evil.invalid",
        "/a/../..//evil.invalid",
        "/..///evil.invalid?next=/u/security#fragment",
    ] {
        c.get("/login").await;
        location(
            &c.post(
                "/login",
                &[
                    ("username", &name),
                    ("password", pw.expose_secret()),
                    ("next", next),
                ],
            )
            .await,
            "/",
        );
    }
    c.get("/login").await;
    location(
        &c.post(
            "/login",
            &[
                ("username", &name),
                ("password", pw.expose_secret()),
                ("next", "/u/security"),
            ],
        )
        .await,
        "/u/security",
    );
    for absolute in [false, true] {
        let s = c.session(false).await;
        if absolute {
            sqlx::query!("UPDATE sessions SET created_at=now()-interval '91 days',last_seen_at=now(),expires_at=now()+interval '1 day' WHERE id=$1",s.id).execute(&c.pool).await.unwrap();
        } else {
            sqlx::query!("UPDATE sessions SET created_at=now()-interval '16 days',last_seen_at=now()-interval '15 days' WHERE id=$1",s.id).execute(&c.pool).await.unwrap();
        }
        c.state.auth.as_ref().unwrap().invalidate(s.id);
        location(&c.get("/u/security").await, "/login");
        assert!(!c.jar.contains_key("__Host-ys"));
        c.login(&name, &pw, false).await;
    }
    c.jar.insert(
        "__Host-ys".into(),
        Token::generate().encode(TokenKind::PendingLogin),
    );
    location(&c.get("/u/security").await, "/login");
    assert!(!c.jar.contains_key("__Host-ys"));
    c.jar.insert("__Host-ys".into(), "malformed".into());
    let cleared = c.get("/").await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(!c.jar.contains_key("__Host-ys"));
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn totp_enrollment_login_replay_attempt_limit_and_removal_reauth(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, pw) = c.seed(false, false).await;
    c.login(&name, &pw, false).await;
    let enroll = c.get("/u/security/totp").await;
    auth_headers(&enroll);
    assert!(enroll.body.contains("<svg"));
    assert!(enroll.body.contains("otpauth://"));
    let binding = c.session(false).await.id;
    let pending = sqlx::query_scalar!(
        "SELECT state FROM pending_webauthn WHERE binding_id=$1 AND kind='totp'",
        binding
    )
    .fetch_one(&c.pool)
    .await
    .unwrap();
    // The test decrypts only via the same derived configured key, never a fixture secret.
    let raw = STANDARD
        .decode(c.state.config.session_key.expose_secret())
        .unwrap();
    let key = blake3::derive_key("yhotos MFA encryption v1", &raw);
    let ring =
        yhotos_auth::Keyring::from_master(&SecretString::from(STANDARD.encode(key))).unwrap();
    let mid: Uuid = serde_json::from_value(pending["id"].clone()).unwrap();
    let sealed: Vec<u8> = serde_json::from_value(pending["sealed"].clone()).unwrap();
    let secret = totp::Secret::from_bytes(
        ring.open(mid.as_bytes(), &sealed)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let code = totp::generate(&secret, now).unwrap();
    location(
        &c.post("/u/security/totp", &[("code", code.expose_secret())])
            .await,
        "/u/security",
    );
    c.get("/u/security").await;
    c.post("/logout", &[]).await;
    location(&c.login(&name, &pw, false).await, "/login/mfa");
    assert!(!c.jar.contains_key("__Host-ys"));
    let (_, pending_token) = Token::decode(c.jar.get("yc").unwrap()).unwrap();
    assert_ne!(
        pending_token.hash(TokenKind::PendingLogin).as_bytes(),
        pending_token.hash(TokenKind::Session).as_bytes()
    );
    c.get("/login/mfa").await;
    assert_eq!(
        c.post("/login/mfa", &[("code", code.expose_secret())])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // A drift-window code for the next step can be used immediately, without sleep.
    let next = totp::generate(&secret, now + std::time::Duration::from_secs(30)).unwrap();
    c.jar.insert("__Host-ys".into(), "stale".into());
    location(
        &c.post("/login/mfa", &[("code", next.expose_secret())])
            .await,
        "/",
    );
    c.get("/u/security").await;
    let s = c.session(false).await;
    assert!(s.mfa_at.is_some());
    sqlx::query!(
        "UPDATE sessions SET reauth_at=now()-interval '6 minutes' WHERE id=$1",
        s.id
    )
    .execute(&c.pool)
    .await
    .unwrap();
    assert_eq!(
        c.post(&format!("/u/security/mfa/{mid}/remove"), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    location(
        &c.post("/u/security/reauth", &[("password", pw.expose_secret())])
            .await,
        "/u/security",
    );
    location(
        &c.post(&format!("/u/security/mfa/{mid}/remove"), &[]).await,
        "/u/security",
    );
    assert!(
        mfa::list(&mut c.pool.acquire().await.unwrap(), id)
            .await
            .unwrap()
            .is_empty()
    );
    let actions = sqlx::query_scalar!("SELECT action FROM audit_log WHERE user_id=$1", id.0)
        .fetch_all(&c.pool)
        .await
        .unwrap();
    for action in ["mfa.enroll", "mfa.remove", "login.failure", "login.success"] {
        assert!(actions.iter().any(|a| a == action));
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn passkey_roundtrip_single_use_and_json_csrf(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, pw) = c.seed(false, false).await;
    c.login(&name, &pw, false).await;
    c.get("/u/security").await;
    let mut forged = Request::builder()
        .method("POST")
        .uri("/u/security/passkey/start")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "cross-site")
        .body(Body::from("{}"))
        .unwrap();
    forged.extensions_mut().insert(ConnectInfo(c.network));
    let denied = c.app.clone().oneshot(forged).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let start = c
        .json("/u/security/passkey/start", serde_json::json!({}))
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let challenge: CreationChallengeResponse = serde_json::from_str(&start.body).unwrap();
    let origin = Url::parse("https://example.test").unwrap();
    let (soft, _) = SoftToken::new(true).unwrap();
    let mut client = WebauthnAuthenticator::new(soft);
    let response = client.do_registration(origin.clone(), challenge).unwrap();
    let finish = c
        .json(
            "/u/security/passkey/finish",
            serde_json::to_value(&response).unwrap(),
        )
        .await;
    assert_eq!(finish.status, StatusCode::OK, "{}", finish.body);
    assert_ne!(
        c.json(
            "/u/security/passkey/finish",
            serde_json::to_value(response).unwrap()
        )
        .await
        .status,
        StatusCode::OK
    );
    c.get("/u/security").await;
    c.post("/logout", &[]).await;
    location(&c.login(&name, &pw, false).await, "/login/mfa");
    c.get("/login/mfa").await;
    let start = c
        .json("/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let challenge: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
    let response = client.do_authentication(origin, challenge).unwrap();
    location(
        &c.json("/login/mfa", serde_json::to_value(&response).unwrap())
            .await,
        "/",
    );
    let methods = mfa::list(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap();
    assert!(methods.first().unwrap().sign_count > 0);
    assert_eq!(
        c.json(
            "/login/mfa/passkey/finish",
            serde_json::to_value(response).unwrap()
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[cfg(feature = "admin")]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn admin_enrollment_guard_and_listener_cookie_isolation(pool: PgPool) {
    let mut admin = Client::new(pool.clone(), true).await;
    let (id, name, pw) = admin.seed(false, true).await;
    location(&admin.login(&name, &pw, true).await, "/admin/setup-mfa");
    let signed = admin.login(&name, &pw, true).await;
    let cookie = signed
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .find(|h| h.to_str().unwrap().starts_with("ya="))
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        cookie,
        format!(
            "ya={}; Path=/; Secure; HttpOnly; SameSite=Strict",
            admin.jar.get("ya").unwrap()
        )
    );
    let strict = admin.jar.get("ya").unwrap().clone();

    let response = admin.get("/admin/healthz").await;
    location(&response, "/admin/setup-mfa");
    auth_headers(&response);
    let setup = admin.get("/admin/setup-mfa").await;
    assert_eq!(setup.status, StatusCode::OK);
    assert!(setup.body.contains("Enroll an authenticator"));
    let mut public = Client::new(pool.clone(), false).await;
    public.jar = admin.jar.clone();
    location(&public.get("/u/security").await, "/login");
    assert!(public.jar.contains_key("ya"));
    public.jar.insert("__Host-ys".into(), strict);
    location(&public.get("/u/security").await, "/login");
    assert!(!public.jar.contains_key("__Host-ys"));
    let login = public.login(&name, &pw, false).await;
    location(&login, "/");
    assert!(
        login
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .all(|h| !h.to_str().unwrap().starts_with("ya="))
    );
    let token = public.jar.get("__Host-ys").unwrap().clone();
    let mut intruder = Client::new(pool.clone(), true).await;
    intruder.jar = public.jar.clone();
    intruder.jar.remove("ya");
    location(&intruder.get("/admin/healthz").await, "/admin/login");
    intruder.jar.insert("ya".into(), token);
    location(&intruder.get("/admin/healthz").await, "/admin/login");
    let (nonadmin, user, password) = public.seed(false, false).await;
    let s = public.login(&user, &password, false).await;
    location(&s, "/");
    let mut session = public.session(false).await;
    // Even a deliberately inconsistent admin-audience row cannot grant privilege.
    sqlx::query!("UPDATE sessions SET admin=true WHERE id=$1", session.id)
        .execute(&pool)
        .await
        .unwrap();
    session.admin = true;
    intruder
        .jar
        .insert("ya".into(), public.jar.get("__Host-ys").unwrap().clone());
    assert_eq!(
        intruder.get("/admin/healthz").await.status,
        StatusCode::NOT_FOUND
    );
    assert_ne!(id, nonadmin);
    let enrolled = admin.get("/admin/u/security/totp").await;
    assert_eq!(enrolled.status, StatusCode::OK);
    let mut other = admin.listener(true);
    location(&other.login(&name, &pw, true).await, "/admin/setup-mfa");
    let other_hash = other.hash(true);
    assert!(other.session(true).await.mfa_at.is_none());
    // Both independent browsers are password-only until one proves enrollment.
    location(&other.get("/admin/healthz").await, "/admin/setup-mfa");
    let old = admin.hash(true);
    admin.enroll_totp().await;
    assert_ne!(old, admin.hash(true));
    assert!(admin.session(true).await.mfa_at.is_some());
    assert!(
        sessions::get(&mut pool.acquire().await.unwrap(), &other_hash)
            .await
            .unwrap()
            .is_none()
    );
    location(&other.get("/admin/healthz").await, "/admin/login");
    assert!(!other.jar.contains_key("ya"));
    assert_eq!(admin.get("/admin/healthz").await.status, StatusCode::OK);
    sqlx::query!("UPDATE users SET is_admin=false WHERE id=$1", id.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        admin.get("/admin/healthz").await.status,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn pending_login_attempts_and_expiry_are_server_side(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, pw) = c.seed(false, false).await;
    let mut conn = c.pool.acquire().await.unwrap();
    mfa::add(
        &mut conn,
        id,
        &mfa::Method {
            id: Uuid::now_v7(),
            kind: "passkey".into(),
            secret_enc: Some(Vec::new()),
            credential_id: None,
            public_key: None,
            sign_count: 0,
            label: "Authenticator app".into(),
            last_used_step: None,
            rp_id: Some("example.test".into()),
        },
    )
    .await
    .unwrap();
    drop(conn);
    c.login(&name, &pw, false).await;
    c.get("/login/mfa").await;
    for attempt in 1..=5 {
        assert_eq!(
            c.post("/login/mfa", &[("code", "invalid")]).await.status,
            if attempt == 5 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
    }
    assert!(!c.jar.contains_key("yc"));
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM pending_login WHERE user_id=$1"#,
        id.0
    )
    .fetch_one(&c.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    sqlx::query!(
        "UPDATE users SET mfa_locked_until=now()-interval '1 second' WHERE id=$1",
        id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    c.login(&name, &pw, false).await;
    sqlx::query!(
        "UPDATE pending_login SET expires_at=now()-interval '1 second' WHERE user_id=$1",
        id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    location(&c.get("/login/mfa").await, "/login");
    assert_eq!(
        c.post("/login/mfa", &[("code", "000000")]).await.status,
        StatusCode::UNAUTHORIZED
    );
}
#[ignore = "wall-clock timing assertion; run explicitly on an idle host"]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn unknown_and_wrong_password_timing_within_twenty_percent(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, _) = c.seed(false, false).await;
    let wrong = yhotos_auth::generate_temp_password();
    let mut times = [std::time::Duration::ZERO; 2];
    for sample in 0..5 {
        for (index, username) in [Uuid::now_v7().to_string(), name.clone()]
            .into_iter()
            .enumerate()
        {
            c.network = format!("127.0.{}.{}:1234", sample + 1, index + 1)
                .parse()
                .unwrap();
            let start = std::time::Instant::now();
            let reply = c.login(&username, &wrong, false).await;
            times[index] += start.elapsed();
            assert_eq!(reply.body, "Sign in failed");
        }
    }
    let ratio = times[0].as_secs_f64() / times[1].as_secs_f64();
    assert!((0.8..=1.2).contains(&ratio), "unknown/wrong ratio {ratio}");
}

struct QueryCount(Arc<std::sync::atomic::AtomicUsize>);
impl<S> tracing_subscriber::Layer<S> for QueryCount
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let measured = context.lookup_current().is_some_and(|span| {
            span.scope()
                .any(|parent| parent.name() == "session_query_count")
        });
        if event.metadata().target() == "sqlx::query" && measured {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn cache_query_count_and_activity_writes_are_coalesced(pool: PgPool) {
    use tracing_subscriber::prelude::*;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // One global dispatch avoids racing tracing interest-cache registration with
    // parallel tests. The layer counts only events inside our explicit spans.
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(QueryCount(count.clone())),
    )
    .unwrap();
    check_session_queries(pool, count).await;
}
async fn check_session_queries(pool: PgPool, count: Arc<std::sync::atomic::AtomicUsize>) {
    use tracing::Instrument;
    let mut c = Client::new(pool, false).await;
    let (_, name, pw) = c.seed(false, false).await;
    c.login(&name, &pw, false).await;
    let s = c.session(false).await;
    let auth = c.state.auth.as_ref().unwrap();
    // Warm connections before counting; pool setup is independent of lookup cost.
    auth.load(&s.token_hash).await.unwrap();
    auth.invalidate(s.id);
    // Measure the exact repository used on misses on a dedicated connection,
    // excluding pool housekeeping and its independently spawned release task.
    let mut conn = sqlx::PgConnection::connect_with(
        &c.pool
            .connect_options()
            .as_ref()
            .clone()
            .log_statements(log::LevelFilter::Debug),
    )
    .await
    .unwrap();
    count.store(0, std::sync::atomic::Ordering::Relaxed);
    let context = sessions::load(&mut conn, &s.token_hash)
        .instrument(tracing::debug_span!("session_query_count"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(context.session.id, s.id);
    auth.load(&s.token_hash).await.unwrap();
    // Occupying the only pool connection makes an accidental warm query block.
    let held = c.db.pool().acquire().await.unwrap();
    count.store(0, std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        async {
            for _ in 0..10 {
                auth.load(&s.token_hash).await.unwrap();
            }
        }
        .instrument(tracing::debug_span!("session_query_count")),
    )
    .await
    .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
    drop(held);
    sqlx::query!("UPDATE sessions SET created_at=now()-interval '7 minutes',last_seen_at=now()-interval '6 minutes' WHERE id=$1",s.id).execute(&c.pool).await.unwrap();
    auth.invalidate(s.id);
    let refreshed = auth.load(&s.token_hash).await.unwrap().unwrap();
    assert!(
        refreshed.session.last_seen_at
            > time::OffsetDateTime::now_utc() - time::Duration::minutes(1)
    );
    let before = sqlx::query_scalar!("SELECT updated_at FROM sessions WHERE id=$1", s.id)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    for _ in 0..5 {
        auth.invalidate(s.id);
        auth.load(&s.token_hash).await.unwrap();
    }
    let after = sqlx::query_scalar!("SELECT updated_at FROM sessions WHERE id=$1", s.id)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn password_change_invalidates_pending_mfa_and_session_revocation_checks_ownership(
    pool: PgPool,
) {
    let mut owner = Client::new(pool.clone(), false).await;
    let (id, name, pw) = owner.seed(false, false).await;
    owner.login(&name, &pw, false).await;
    let mut attacker = Client::new(pool.clone(), false).await;
    let (_, other, other_pw) = attacker.seed(false, false).await;
    attacker.login(&other, &other_pw, false).await;
    let owner_session = owner.session(false).await;
    attacker.get("/u/security").await;
    assert_eq!(
        attacker
            .post(
                &format!("/u/security/sessions/{}/revoke", owner_session.id),
                &[]
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    owner.get("/u/security").await;
    let start = owner
        .json("/u/security/passkey/start", serde_json::json!({}))
        .await;
    let (soft, _) = SoftToken::new(true).unwrap();
    let mut authenticator = WebauthnAuthenticator::new(soft);
    let origin = Url::parse("https://example.test").unwrap();
    let registration = authenticator
        .do_registration(origin.clone(), serde_json::from_str(&start.body).unwrap())
        .unwrap();
    assert_eq!(
        owner
            .json(
                "/u/security/passkey/finish",
                serde_json::to_value(registration).unwrap()
            )
            .await
            .status,
        StatusCode::OK
    );
    owner.get("/u/security").await;
    let mut pending = Client::new(pool.clone(), false).await;
    location(&pending.login(&name, &pw, false).await, "/login/mfa");
    pending.get("/login/mfa").await;
    let challenge = pending
        .json("/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    let assertion = authenticator
        .do_authentication(origin, serde_json::from_str(&challenge.body).unwrap())
        .unwrap();
    let (kind, token) = Token::decode(pending.jar.get("yc").unwrap()).unwrap();
    assert_eq!(kind, TokenKind::PendingLogin);
    let hash = token.hash(kind);
    let version_before = sqlx::query_scalar!(
        "SELECT password_version FROM pending_login WHERE token_hash=$1",
        hash.as_bytes().as_slice()
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .unwrap();
    let replacement = yhotos_auth::generate_temp_password();
    location(
        &owner
            .post(
                "/u/password",
                &[
                    ("current_password", pw.expose_secret()),
                    ("new_password", replacement.expose_secret()),
                ],
            )
            .await,
        "/",
    );
    let user = users::get_by_id(&mut pool.acquire().await.unwrap(), id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        version_before,
        blake3::hash(user.password_hash.as_bytes()).as_bytes()
    );
    // A valid, signed assertion cannot complete a login whose password was replaced.
    assert_eq!(
        pending
            .json(
                "/login/mfa/passkey/finish",
                serde_json::to_value(assertion).unwrap()
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert!(!pending.jar.contains_key("__Host-ys"));
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn unicode_username_and_account_lockout_across_spelling_and_ip_changes(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, _, pw) = c.seed(false, false).await;
    let user = users::get_by_id(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap()
        .unwrap();
    let name = format!("İ-{}", Uuid::now_v7());
    users::create(
        &mut c.pool.acquire().await.unwrap(),
        &name,
        &SecretString::from(user.password_hash),
        false,
    )
    .await
    .unwrap();
    location(&c.login(&name, &pw, false).await, "/");
    c.post("/logout", &[]).await;
    let wrong = yhotos_auth::generate_temp_password();
    for attempt in 1..=5 {
        c.network = format!("127.0.1.{attempt}:1234").parse().unwrap();
        let spelling = if attempt % 2 == 0 {
            name.to_uppercase()
        } else {
            name.clone()
        };
        let reply = c.login(&spelling, &wrong, false).await;
        assert_eq!(
            reply.status,
            if attempt == 5 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
    }
    c.network = "127.0.1.6:1234".parse().unwrap();
    assert_eq!(
        c.login(&name, &pw, false).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[cfg(feature = "admin")]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn admin_revoke_evicts_warmed_public_listener_immediately(pool: PgPool) {
    cross_listener_invalidation(pool, false).await;
}
#[cfg(feature = "admin")]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn admin_password_change_evicts_warmed_public_listener_immediately(pool: PgPool) {
    cross_listener_invalidation(pool, true).await;
}
#[cfg(feature = "admin")]
async fn cross_listener_invalidation(pool: PgPool, password_change: bool) {
    let mut admin = Client::new(pool.clone(), true).await;
    let mut public = admin.listener(false);
    assert!(Arc::ptr_eq(
        admin.state.auth.as_ref().unwrap(),
        public.state.auth.as_ref().unwrap()
    ));
    let (_, name, password) = admin.seed(false, true).await;
    admin.login(&name, &password, true).await;
    let secret = admin.enroll_totp().await;
    location(&public.login(&name, &password, false).await, "/login/mfa");
    public.get("/login/mfa").await;
    let next_step = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        + std::time::Duration::from_secs(30);
    let code = totp::generate(&secret, next_step).unwrap();
    location(
        &public
            .post("/login/mfa", &[("code", code.expose_secret())])
            .await,
        "/",
    );
    let session = public.session(false).await;
    assert!(session.mfa_at.is_some());
    assert_eq!(public.get("/u/security").await.status, StatusCode::OK);

    // An independent Auth models another process and warms a separate cache.
    let db = Arc::new(
        Db::connect(
            &SecretString::from(pool.connect_options().to_url_lossy().to_string()),
            Role::Public,
            1,
            DatabaseTls::Disable,
        )
        .await
        .unwrap(),
    );
    let remote_auth = Auth::new(admin.state.config, db, None).await.unwrap();
    assert!(
        remote_auth
            .load(&session.token_hash)
            .await
            .unwrap()
            .is_some()
    );
    let mut remote = public.listener(false);
    remote.state.auth = Some(remote_auth.clone());
    remote.app = public_router(remote.state.clone());
    remote.jar = public.jar.clone();
    assert_eq!(remote.get("/u/security").await.status, StatusCode::OK);
    admin.get("/admin/u/security").await;
    if password_change {
        let replacement = yhotos_auth::generate_temp_password();
        location(
            &admin
                .post(
                    "/admin/u/password",
                    &[
                        ("current_password", password.expose_secret()),
                        ("new_password", replacement.expose_secret()),
                    ],
                )
                .await,
            "/admin/",
        );
        assert!(admin.session(true).await.mfa_at.is_some());
    } else {
        location(
            &admin
                .post(
                    &format!("/admin/u/security/sessions/{}/revoke", session.id),
                    &[],
                )
                .await,
            "/admin/u/security",
        );
    }
    // No sleeps, polling or manual invalidation between listeners.
    location(&public.get("/u/security").await, "/login");
    assert!(!public.jar.contains_key("__Host-ys"));
    // PostgreSQL delivery is asynchronous across processes. Bound convergence
    // below the cache TTL, yielding to the receiver without a timing sleep.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while remote_auth
            .load(&session.token_hash)
            .await
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    location(&remote.get("/u/security").await, "/login");
}

#[cfg(feature = "admin")]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn admin_passkeys_use_distinct_origin_for_registration_and_login(pool: PgPool) {
    let mut admin = Client::new(pool, true).await;
    let (_, name, password) = admin.seed(false, true).await;
    admin.login(&name, &password, true).await;
    admin.get("/admin/setup-mfa").await;
    // A public origin must fail the admin JSON CSRF check, even with valid tokens.
    admin.origin = "https://example.test".into();
    assert_eq!(
        admin
            .json("/admin/u/security/passkey/start", serde_json::json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    admin.origin = "https://admin.example.test:8443".into();
    let start = admin
        .json("/admin/u/security/passkey/start", serde_json::json!({}))
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let challenge: CreationChallengeResponse = serde_json::from_str(&start.body).unwrap();
    assert_eq!(challenge.public_key.rp.id, "admin.example.test");
    let (soft, _) = SoftToken::new(true).unwrap();
    let mut authenticator = WebauthnAuthenticator::new(soft);
    let origin = Url::parse(&admin.origin).unwrap();
    let registration = authenticator
        .do_registration(origin.clone(), challenge)
        .unwrap();
    assert_eq!(
        admin
            .json(
                "/admin/u/security/passkey/finish",
                serde_json::to_value(registration).unwrap()
            )
            .await
            .status,
        StatusCode::OK
    );
    assert!(admin.session(true).await.mfa_at.is_some());
    assert_eq!(admin.get("/admin/healthz").await.status, StatusCode::OK);
    admin.get("/admin/u/security").await;
    admin.post("/admin/logout", &[]).await;
    location(
        &admin.login(&name, &password, true).await,
        "/admin/login/mfa",
    );
    admin.get("/admin/login/mfa").await;
    let start = admin
        .json("/admin/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let challenge: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
    let wrong_port = Url::parse("https://admin.example.test:9443").unwrap();
    let wrong_assertion = authenticator
        .do_authentication(wrong_port, challenge)
        .unwrap();
    // Correct HTTP Origin cannot rescue an assertion signed for another origin.
    assert_eq!(
        admin
            .json(
                "/admin/login/mfa",
                serde_json::to_value(wrong_assertion).unwrap()
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let start = admin
        .json("/admin/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    assert_eq!(start.status, StatusCode::OK);
    let challenge: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
    let assertion = authenticator.do_authentication(origin, challenge).unwrap();
    location(
        &admin
            .json("/admin/login/mfa", serde_json::to_value(assertion).unwrap())
            .await,
        "/admin/",
    );
    assert!(admin.session(true).await.mfa_at.is_some());
    assert_eq!(admin.get("/admin/healthz").await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn stale_session_cookie_does_not_clear_password_login(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, password) = c.seed(false, false).await;
    c.login(&name, &password, false).await;
    let old = c.session(false).await;
    let expired = c.jar["__Host-ys"].clone();
    sqlx::query!(
        "UPDATE sessions SET expires_at=now()-interval '1 second' WHERE id=$1",
        old.id
    )
    .execute(&c.pool)
    .await
    .unwrap();
    c.state.auth.as_ref().unwrap().invalidate(old.id);
    for stale in [
        "malformed".to_owned(),
        Token::generate().encode(TokenKind::Session),
        expired,
    ] {
        c.get("/login").await;
        c.jar.insert("__Host-ys".into(), stale);
        let reply = c
            .post(
                "/login",
                &[("username", &name), ("password", password.expose_secret())],
            )
            .await;
        location(&reply, "/");
        assert_eq!(
            reply
                .headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter(|v| v.to_str().unwrap().starts_with("__Host-ys="))
                .count(),
            1
        );
        assert_eq!(c.get("/u/security").await.status, StatusCode::OK);
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn mfa_backoff_survives_replacement_challenges_and_network_changes(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, password) = c.seed(false, false).await;
    let secret = c.seed_totp(id).await;
    let mut previous = String::new();
    for attempt in 1..=5 {
        c.network = format!("192.0.2.{attempt}:1234").parse().unwrap();
        if attempt % 2 == 1 {
            location(&c.login(&name, &password, false).await, "/login/mfa");
            assert_ne!(c.jar["yc"], previous);
            previous.clone_from(&c.jar["yc"]);
            let user = users::get_by_id(&mut c.pool.acquire().await.unwrap(), id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(user.mfa_failures, attempt - 1);
        }
        let reply = c.post("/login/mfa", &[("code", "invalid")]).await;
        assert_eq!(
            reply.status,
            if attempt == 5 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        if attempt == 5 {
            let retry: u64 = reply.headers[header::RETRY_AFTER]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!((30..=31).contains(&retry));
        }
    }
    c.network = "198.51.100.1:1234".parse().unwrap();
    assert_eq!(
        c.login(&name, &password, false).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        c.json("/login/mfa/passkey/start", serde_json::json!({}))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let code = totp::generate(&secret, now).unwrap();
    assert_eq!(
        c.post("/login/mfa", &[("code", code.expose_secret())])
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query!(
        "UPDATE users SET mfa_locked_until=now()-interval '1 second' WHERE id=$1",
        id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    location(&c.login(&name, &password, false).await, "/login/mfa");
    let sixth = c.post("/login/mfa", &[("code", "invalid")]).await;
    assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = sixth.headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((60..=61).contains(&retry));
    sqlx::query!(
        "UPDATE users SET mfa_locked_until=now()-interval '1 second' WHERE id=$1",
        id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    location(
        &c.post("/login/mfa", &[("code", code.expose_secret())])
            .await,
        "/",
    );
    let user = users::get_by_id(&mut c.pool.acquire().await.unwrap(), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.mfa_failures, 0);
    assert!(user.mfa_locked_until.is_none());
}

#[cfg(feature = "admin")]
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn listener_unusable_mfa_fails_closed_and_enrollment_requires_mfa(pool: PgPool) {
    let client = Client::new(pool, false).await;
    for first_admin in [false, true] {
        let (id, name, password) = client.seed(false, true).await;
        let mut first = client.listener(first_admin);
        let mut second = client.listener(!first_admin);
        first.login(&name, &password, first_admin).await;
        second.login(&name, &password, !first_admin).await;
        let base = if first_admin {
            "/u/security"
        } else {
            "/admin/u/security"
        };
        // Begin both ceremonies while the account still has no factors.
        assert_eq!(
            second.get(&format!("{base}/totp")).await.status,
            StatusCode::OK
        );
        let start = second
            .json(&format!("{base}/passkey/start"), serde_json::json!({}))
            .await;
        assert_eq!(start.status, StatusCode::OK);
        let challenge: CreationChallengeResponse = serde_json::from_str(&start.body).unwrap();
        let (soft, _) = SoftToken::new(true).unwrap();
        let mut authenticator = WebauthnAuthenticator::new(soft);
        let registration = authenticator
            .do_registration(Url::parse(&second.origin).unwrap(), challenge)
            .unwrap();
        let stale = second.session(!first_admin).await;
        let pending_secret = second.pending_totp_secret(!first_admin).await;
        first.enroll_passkey(&mut authenticator).await;
        assert!(first.session(first_admin).await.mfa_at.is_some());
        // Model a legacy password-only session surviving factor enrollment.
        // The handlers must check current factors at both start and completion.
        sessions::create(&mut client.pool.acquire().await.unwrap(), &stale)
            .await
            .unwrap();
        if !first_admin {
            let setup = second.get("/admin/setup-mfa").await;
            assert!(setup.body.contains("TOTP to sign in on both"));
            location(&second.get("/admin/healthz").await, "/admin/setup-mfa");
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let pending_code = totp::generate(&pending_secret, time).unwrap();
        for reply in [
            second.get(&format!("{base}/totp")).await,
            second
                .post(
                    &format!("{base}/totp"),
                    &[("code", pending_code.expose_secret())],
                )
                .await,
            second
                .json(&format!("{base}/passkey/start"), serde_json::json!({}))
                .await,
            second
                .json(
                    &format!("{base}/passkey/finish"),
                    serde_json::to_value(registration).unwrap(),
                )
                .await,
        ] {
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
            assert_eq!(reply.body, "Sign in failed");
        }
        assert_eq!(
            mfa::list(&mut client.pool.acquire().await.unwrap(), id)
                .await
                .unwrap()
                .len(),
            1
        );
        let prefix = if first_admin { "" } else { "/admin" };
        second.post(&format!("{prefix}/logout"), &[]).await;
        let denied = second.login(&name, &password, !first_admin).await;
        assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.body, "Sign in failed");
        assert!(
            !second
                .jar
                .contains_key(if first_admin { "__Host-ys" } else { "ya" })
        );
        assert!(
            !second
                .jar
                .contains_key(if first_admin { "yc" } else { "yac" })
        );
        let reasons = sqlx::query_scalar!("SELECT detail FROM audit_log WHERE user_id=$1", id.0)
            .fetch_all(&client.pool)
            .await
            .unwrap();
        assert!(
            reasons
                .iter()
                .any(|detail| detail["reason"] == "mfa_unavailable_on_listener")
        );
        // TOTP is not RP-bound. Prove it permits MFA login on the other listener,
        // after which enrolling that listener's passkey is allowed.
        let secret = first.enroll_totp().await;
        location(
            &second.login(&name, &password, !first_admin).await,
            &format!("{prefix}/login/mfa"),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let code = totp::generate(&secret, now + std::time::Duration::from_secs(30)).unwrap();
        location(
            &second
                .post(
                    &format!("{prefix}/login/mfa"),
                    &[("code", code.expose_secret())],
                )
                .await,
            if first_admin { "/" } else { "/admin/" },
        );
        second.enroll_passkey(&mut authenticator).await;
        let methods = mfa::list(&mut client.pool.acquire().await.unwrap(), id)
            .await
            .unwrap();
        assert_eq!(methods.len(), 3);
        // Each RP offers only its own passkey, and both remain usable.
        for (browser, admin) in [(&mut first, first_admin), (&mut second, !first_admin)] {
            let prefix = if admin { "/admin" } else { "" };
            browser.post(&format!("{prefix}/logout"), &[]).await;
            location(
                &browser.login(&name, &password, admin).await,
                &format!("{prefix}/login/mfa"),
            );
            let start = browser
                .json(
                    &format!("{prefix}/login/mfa/passkey/start"),
                    serde_json::json!({}),
                )
                .await;
            assert_eq!(start.status, StatusCode::OK);
            let challenge: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
            assert_eq!(challenge.public_key.allow_credentials.len(), 1);
            let rp = if admin {
                "admin.example.test"
            } else {
                "example.test"
            };
            let method = methods
                .iter()
                .find(|m| m.rp_id.as_deref() == Some(rp))
                .unwrap();
            assert_eq!(
                challenge.public_key.allow_credentials[0].id.as_slice(),
                method.credential_id.as_deref().unwrap()
            );
            let assertion = authenticator
                .do_authentication(Url::parse(&browser.origin).unwrap(), challenge)
                .unwrap();
            location(
                &browser
                    .json(
                        &format!("{prefix}/login/mfa/passkey/finish"),
                        serde_json::to_value(assertion).unwrap(),
                    )
                    .await,
                if admin { "/admin/" } else { "/" },
            );
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn session_and_account_eviction_preserve_other_users_warm_caches(pool: PgPool) {
    let mut a = Client::new(pool.clone(), false).await;
    let mut b = Client::new(pool, false).await;
    let (_, a_name, a_password) = a.seed(false, false).await;
    let (_, b_name, b_password) = b.seed(false, false).await;
    b.login(&b_name, &b_password, false).await;
    let b_session = b.session(false).await;
    let local = a.state.auth.as_ref().unwrap().clone();
    let remote = b.state.auth.as_ref().unwrap().clone();
    for password_change in [false, true] {
        a.login(&a_name, &a_password, false).await;
        let a_session = a.session(false).await;
        for auth in [&local, &remote] {
            auth.load(&a_session.token_hash).await.unwrap().unwrap();
            auth.load(&b_session.token_hash).await.unwrap().unwrap();
        }
        if password_change {
            let new = yhotos_auth::generate_temp_password();
            location(
                &a.post(
                    "/u/password",
                    &[
                        ("current_password", a_password.expose_secret()),
                        ("new_password", new.expose_secret()),
                    ],
                )
                .await,
                "/",
            );
        } else {
            location(&a.post("/logout", &[]).await, "/login");
        }
        // The second Auth must process PostgreSQL's notification before we measure.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while remote.load(&a_session.token_hash).await.unwrap().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(local.load(&a_session.token_hash).await.unwrap().is_none());
        let held_local = a.db.pool().acquire().await.unwrap();
        let held_remote = b.db.pool().acquire().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            for auth in [&local, &remote] {
                for _ in 0..10 {
                    assert_eq!(
                        auth.load(&b_session.token_hash)
                            .await
                            .unwrap()
                            .unwrap()
                            .session
                            .id,
                        b_session.id
                    );
                }
            }
        })
        .await
        .expect("unrelated warmed lookups must issue no database query");
        drop((held_local, held_remote));
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn session_eviction_rejects_an_inflight_stale_database_snapshot(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, password) = c.seed(false, false).await;
    c.login(&name, &password, false).await;
    let session = c.session(false).await;
    sqlx::query!("UPDATE sessions SET created_at=now()-interval '7 minutes',last_seen_at=now()-interval '6 minutes' WHERE id=$1",session.id)
        .execute(&c.pool).await.unwrap();
    let auth = c.state.auth.as_ref().unwrap().clone();
    auth.invalidate(session.id);
    let mut request_conn = c.db.pool().acquire().await.unwrap();
    sqlx::query!("SET statement_timeout='30s'")
        .execute(&mut *request_conn)
        .await
        .unwrap();
    drop(request_conn);
    let mut tx = c.pool.begin().await.unwrap();
    sqlx::query!("SELECT id FROM sessions WHERE id=$1 FOR UPDATE", session.id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let task_auth = auth.clone();
    let hash = session.token_hash.clone();
    let lookup = tokio::spawn(async move { task_auth.load(&hash).await.unwrap() });
    // Wait until load's SQL snapshot exists and its activity write is blocked.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked = sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT FROM pg_stat_activity
                WHERE datname=current_database() AND wait_event_type='Lock'
                AND query LIKE 'WITH touched AS (%') AS "blocked!""#
            )
            .fetch_one(&c.pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sessions::revoke(&mut tx, session.user_id, session.id)
        .await
        .unwrap();
    // Invalidate while the older lookup is still blocked in its original snapshot.
    auth.invalidate(session.id);
    tx.commit().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), lookup)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn reauth_lockout_spans_ips_and_password_endpoints(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, password) = c.seed(false, false).await;
    c.login(&name, &password, false).await;
    c.enroll_totp().await;
    let session = c.session(false).await;
    sqlx::query!(
        "UPDATE sessions SET reauth_at=now()-interval '6 minutes' WHERE id=$1",
        session.id
    )
    .execute(&c.pool)
    .await
    .unwrap();
    c.state.auth.as_ref().unwrap().invalidate(session.id);
    let wrong = yhotos_auth::generate_temp_password();
    for attempt in 1..=5 {
        c.network = format!("127.0.2.{attempt}:1234").parse().unwrap();
        let reply = c
            .post("/u/security/reauth", &[("password", wrong.expose_secret())])
            .await;
        assert_eq!(reply.body, "Sign in failed");
        assert_eq!(
            reply.status,
            if attempt == 5 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        if attempt == 5 {
            assert_eq!(reply.headers[header::RETRY_AFTER], "30");
        }
    }
    c.network = "127.0.2.6:1234".parse().unwrap();
    assert_eq!(
        c.post(
            "/u/security/reauth",
            &[("password", password.expose_secret())]
        )
        .await
        .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        c.post(
            "/u/password",
            &[
                ("current_password", password.expose_secret()),
                ("new_password", wrong.expose_secret())
            ]
        )
        .await
        .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let method = mfa::list(&mut c.pool.acquire().await.unwrap(), session.user_id)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        c.post(&format!("/u/security/mfa/{}/remove", method.id), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // Login cannot bypass failures accumulated on reauthentication either.
    c.post("/logout", &[]).await;
    assert_eq!(
        c.login(&name, &password, false).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM audit_log WHERE action='login.failure'"#
    )
    .fetch_one(&c.pool)
    .await
    .unwrap();
    assert_eq!(count, 8);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn password_change_failures_share_ip_lockout_and_reauth_resets_success(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (_, name, password) = c.seed(false, false).await;
    c.login(&name, &password, false).await;
    let wrong = yhotos_auth::generate_temp_password();
    for _ in 0..4 {
        assert_eq!(
            c.post(
                "/u/password",
                &[
                    ("current_password", wrong.expose_secret()),
                    ("new_password", wrong.expose_secret())
                ]
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }
    // A successful reauth clears both counters before they reach the threshold.
    location(
        &c.post(
            "/u/security/reauth",
            &[("password", password.expose_secret())],
        )
        .await,
        "/u/security",
    );
    for _ in 0..4 {
        assert_eq!(
            c.post(
                "/u/password",
                &[
                    ("current_password", wrong.expose_secret()),
                    ("new_password", wrong.expose_secret())
                ]
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }
    c.post("/logout", &[]).await;
    // A different account on the same IP shares the network's failure count.
    assert_eq!(
        c.login(&Uuid::now_v7().to_string(), &wrong, false)
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    c.network = "127.0.3.1:1234".parse().unwrap();
    // The real account has four failures; a fresh IP does not reset that state.
    assert_eq!(
        c.login(&name, &wrong, false).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn overlapping_passkey_ceremonies_cannot_roll_back_the_counter(pool: PgPool) {
    let mut c = Client::new(pool, false).await;
    let (id, name, password) = c.seed(false, false).await;
    c.login(&name, &password, false).await;
    let (soft, _) = SoftToken::new(true).unwrap();
    let mut authenticator = WebauthnAuthenticator::new(soft);
    c.enroll_passkey(&mut authenticator).await;
    c.post("/logout", &[]).await;
    c.login(&name, &password, false).await;
    let start = c
        .json("/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    let first: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
    // Keep the first pending token while creating a second independent ceremony.
    let pending = c.jar["yc"].clone();
    let first_assertion = authenticator
        .do_authentication(Url::parse(&c.origin).unwrap(), first)
        .unwrap();
    c.login(&name, &password, false).await;
    let start = c
        .json("/login/mfa/passkey/start", serde_json::json!({}))
        .await;
    let second: RequestChallengeResponse = serde_json::from_str(&start.body).unwrap();
    let second_assertion = authenticator
        .do_authentication(Url::parse(&c.origin).unwrap(), second)
        .unwrap();
    location(
        &c.json(
            "/login/mfa/passkey/finish",
            serde_json::to_value(second_assertion).unwrap(),
        )
        .await,
        "/",
    );
    c.post("/logout", &[]).await;
    c.jar.insert("yc".into(), pending);
    assert_eq!(
        c.json(
            "/login/mfa/passkey/finish",
            serde_json::to_value(first_assertion).unwrap()
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let mut connection = c.pool.acquire().await.unwrap();
    let method = mfa::list(&mut connection, id).await.unwrap().remove(0);
    assert_eq!(method.sign_count, 2);
    let key = method.public_key.clone().unwrap();
    // This is the storage check used after signature validation against a stale
    // zero-counter ceremony: a now-counted credential must never reset to zero.
    assert!(
        !mfa::use_passkey(&mut connection, id, &method, 0, &key)
            .await
            .unwrap()
    );
    assert!(
        !mfa::use_passkey(&mut connection, id, &method, 2, &key)
            .await
            .unwrap()
    );
    assert!(
        mfa::use_passkey(&mut connection, id, &method, 3, &key)
            .await
            .unwrap()
    );
    assert!(
        !mfa::use_passkey(&mut connection, id, &method, 4, &key)
            .await
            .unwrap()
    );
    assert_eq!(
        mfa::list(&mut connection, id).await.unwrap()[0].sign_count,
        3
    );
    // Genuine counterless credentials still accept zero, including overlapping uses.
    let mut counterless = method;
    counterless.id = Uuid::now_v7();
    counterless.credential_id = Some(Uuid::now_v7().as_bytes().to_vec());
    counterless.sign_count = 0;
    mfa::add(&mut connection, id, &counterless).await.unwrap();
    for _ in 0..2 {
        assert!(
            mfa::use_passkey(&mut connection, id, &counterless, 0, &key)
                .await
                .unwrap()
        );
    }
}
