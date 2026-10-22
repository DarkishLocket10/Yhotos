#![forbid(unsafe_code)]
//! Complete session middleware stack, measured with a real development database.
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, header},
};
use criterion::Criterion;
use secrecy::SecretString;
use std::{error::Error, net::SocketAddr, sync::Arc};
use time::OffsetDateTime;
use tower::ServiceExt;
use yhotos_auth::{Token, TokenKind};
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_db::{DatabaseTls, Db, Role, mfa, sessions, users};
use yhotos_http::{AppState, auth::Auth, public_router};

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let url = SecretString::from(std::env::var("DATABASE_URL")?);
    let admin = runtime.block_on(Db::connect(&url, Role::Admin, 2, DatabaseTls::Disable))?;
    let db =
        Arc::new(runtime.block_on(Db::connect(&url, Role::Public, 2, DatabaseTls::Disable))?);
    let mut conn = runtime.block_on(admin.pool().acquire())?;
    let username = uuid::Uuid::now_v7().to_string();
    let password = yhotos_auth::generate_temp_password();
    let phc = yhotos_auth::hash_password(&password)?;
    let user = runtime.block_on(users::create(
        &mut conn,
        &username,
        &SecretString::from(phc.as_str()),
        false,
    ))?;
    let token = Token::generate();
    let hash = token.hash(TokenKind::Session).as_bytes().to_vec();
    let now = OffsetDateTime::now_utc();
    let s = sessions::Session {
        id: uuid::Uuid::now_v7(),
        user_id: user.id,
        admin: false,
        created_at: now,
        last_seen_at: now,
        expires_at: now + time::Duration::days(90),
        reauth_at: Some(now),
        mfa_at: None,
        ip_hash: Vec::new(),
        ua_class: "other".into(),
        token_hash: hash.clone(),
    };
    runtime.block_on(sessions::create(&mut conn, &s))?;
    runtime.block_on(analyze_auth_tables(&mut conn))?;
    let peer: SocketAddr = "127.0.0.1:1".parse()?;
    let dir = tempfile::tempdir()?;
    let config = config(dir.path(), url, peer)?;
    let auth = runtime.block_on(Auth::new(config, db.clone(), None))?;
    let app = public_router(AppState::new(
        config,
        db.pool().clone(),
        yhotos_storage::Store::open(dir.path())?,
        Some(auth.clone()),
    ));
    let bearer = format!("__Host-ys={}", token.encode(TokenKind::Session));
    let request =
        |authenticated: bool| session_request(authenticated.then_some(bearer.as_str()), peer);
    let status = runtime
        .block_on(app.clone().oneshot(request(true)?))?
        .status();
    if status != axum::http::StatusCode::OK {
        return Err("session benchmark setup failed".into());
    }
    let mut criterion = Criterion::default().configure_from_args();
    for (label, authenticated, cold) in [
        ("session/anonymous_baseline", false, false),
        ("session/warm_full_stack", true, false),
        ("session/cold_one_query_full_stack", true, true),
    ] {
        criterion.bench_function(label, |b| {
            b.iter(|| {
                if cold {
                    auth.invalidate(s.id);
                }
                let result =
                    request(authenticated).map(|r| runtime.block_on(app.clone().oneshot(r)));
                std::hint::black_box(result)
            });
        });
    }
    // Exercise an absent factor lookup with enabled factors for many unrelated
    // accounts. An empty MFA table conceals a missing per-user index.
    let mut population = runtime.block_on(seed_mfa_population(&mut conn, phc.as_str()))?;
    population.push(user.id.0);
    auth.invalidate(s.id);
    let loaded = runtime
        .block_on(auth.load(&hash))?
        .ok_or("session missing")?;
    if loaded.has_mfa {
        return Err("unrelated factors leaked into session context".into());
    }
    for (label, cold) in [
        ("session/warm_10000_mfa_users_full_stack", false),
        ("session/cold_one_query_10000_mfa_users_full_stack", true),
    ] {
        criterion.bench_function(label, |b| {
            b.iter(|| {
                if cold {
                    auth.invalidate(s.id);
                }
                let result = request(true).map(|r| runtime.block_on(app.clone().oneshot(r)));
                std::hint::black_box(result)
            });
        });
    }
    bench_login_pages(&mut criterion, &runtime, &app, peer)?;
    runtime.block_on(
        sqlx::query!("DELETE FROM users WHERE id=ANY($1)", &population).execute(&mut *conn),
    )?;
    criterion.final_summary();
    Ok(())
}

fn session_request(
    cookie: Option<&str>,
    peer: SocketAddr,
) -> Result<Request<Body>, axum::http::Error> {
    let mut r = Request::builder().uri("/");
    if let Some(cookie) = cookie {
        r = r.header(header::COOKIE, cookie);
    }
    let mut r = r.body(Body::empty())?;
    r.extensions_mut().insert(ConnectInfo(peer));
    Ok(r)
}

async fn seed_mfa_population(
    conn: &mut yhotos_db::PgConnection,
    password_hash: &str,
) -> Result<Vec<uuid::Uuid>, Box<dyn Error>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sqlx::Connection;

    let key = Token::generate().hash(TokenKind::Session);
    let keys = yhotos_auth::Keyring::from_master(&STANDARD.encode(key.as_bytes()).into())?;
    let mut tx = conn.begin().await?;
    let mut users = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let user = users::create(
            &mut tx,
            &uuid::Uuid::now_v7().to_string(),
            &SecretString::from(password_hash),
            false,
        )
        .await?;
        let id = uuid::Uuid::now_v7();
        let secret = yhotos_auth::totp::generate_secret();
        mfa::add(
            &mut tx,
            user.id,
            &mfa::Method {
                id,
                kind: "totp".into(),
                secret_enc: Some(keys.seal(id.as_bytes(), secret.expose_secret())?),
                credential_id: None,
                public_key: None,
                sign_count: 0,
                label: "Authenticator app".into(),
                rp_id: None,
                last_used_step: None,
            },
        )
        .await?;
        users.push(user.id.0);
    }
    tx.commit().await?;
    analyze_auth_tables(conn).await?;
    Ok(users)
}

// Keep cached PostgreSQL plans independent of autovacuum timing after bulk setup.
async fn analyze_auth_tables(conn: &mut yhotos_db::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query!("ANALYZE users").execute(&mut *conn).await?;
    sqlx::query!("ANALYZE user_mfa").execute(&mut *conn).await?;
    sqlx::query!("ANALYZE sessions").execute(conn).await?;
    Ok(())
}
fn config(
    dir: &std::path::Path,
    url: SecretString,
    peer: SocketAddr,
) -> Result<&'static Config, Box<dyn Error>> {
    // A generated 32-byte token is also a valid master-key input after standard
    // base64 encoding. Construct with a generated 32-byte digest for this bench.
    let key = Token::generate().hash(TokenKind::Session);
    let master = encode_standard(key.as_bytes());
    Ok(Box::leak(Box::new(Config {
        cloudflare_zone: None,
        cloudflare_api_token: None,
        worker: yhotos_config::Worker::default(),
        public_listen: peer,
        admin_listen: Some(peer),
        database_url_public: url,
        database_url_admin: None,
        database_url_worker: None,
        admin_base_url: None,
        data_dir: dir.to_owned(),
        public_base_url: "https://example.test".parse()?,
        session_key: master.into(),
        trust_proxy: false,
        global_stream_cap: 64,
        write_timeout: std::time::Duration::from_secs(30),
        transfer_deadline: std::time::Duration::from_mins(30),
        rate_limits: RateLimits {
            cheap_per_min: u32::MAX,
            expensive_per_min: u32::MAX,
            auth_per_min: u32::MAX,
        },
        upload: Upload {
            max_bytes: 1,
            chunk_bytes: 1,
        },
        log_format: LogFormat::Json,
    })))
}
fn bench_login_pages(
    criterion: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    app: &axum::Router,
    peer: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let csrf = yhotos_http::middleware::csrf::CsrfToken::generate()?;
    let csrf_cookie = format!("__Host-yhotos-csrf={}", csrf.form_value());
    for (label, cookie) in [
        ("auth/login_page_new_csrf", None),
        ("auth/login_page_existing_csrf", Some(&csrf_cookie)),
    ] {
        criterion.bench_function(label, |b| {
            b.iter(|| {
                let mut r = Request::builder().uri("/login");
                if let Some(cookie) = cookie {
                    r = r.header(header::COOKIE, cookie);
                }
                let result = r.body(Body::empty()).map(|mut r| {
                    r.extensions_mut().insert(ConnectInfo(peer));
                    runtime.block_on(app.clone().oneshot(r))
                });
                std::hint::black_box(result)
            });
        });
    }
    Ok(())
}
fn encode_standard(bytes: &[u8; 32]) -> String {
    // Reuse base64 through the bench's existing direct dependency.
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(bytes)
}
