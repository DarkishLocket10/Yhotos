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
use yhotos_db::{DatabaseTls, Db, Role, sessions, users};
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
        ip_hash: Vec::new(),
        ua_class: "other".into(),
        token_hash: hash.clone(),
    };
    runtime.block_on(sessions::create(&mut conn, &s))?;
    let peer: SocketAddr = "127.0.0.1:1".parse()?;
    let dir = tempfile::tempdir()?;
    // A generated 32-byte token is also a valid master-key input after standard
    // base64 encoding. Construct with a generated 32-byte digest for this bench.
    let key = Token::generate().hash(TokenKind::Session);
    let master = encode_standard(key.as_bytes());
    let config = Box::leak(Box::new(Config {
        public_listen: peer,
        admin_listen: Some(peer),
        database_url: url,
        data_dir: dir.path().to_owned(),
        public_base_url: "https://example.test".parse()?,
        session_key: master.into(),
        trust_proxy: false,
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
    }));
    let auth = Auth::new(config, db)?;
    let app = public_router(AppState {
        config,
        auth: Some(auth.clone()),
    });
    let bearer = format!("__Host-ys={}", token.encode(TokenKind::Session));
    let request = |authenticated: bool| -> Result<Request<Body>, axum::http::Error> {
        let mut r = Request::builder().uri("/");
        if authenticated {
            r = r.header(header::COOKIE, &bearer);
        }
        let mut r = r.body(Body::empty())?;
        r.extensions_mut().insert(ConnectInfo(peer));
        Ok(r)
    };
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
                    auth.invalidate(&hash);
                }
                let result =
                    request(authenticated).map(|r| runtime.block_on(app.clone().oneshot(r)));
                std::hint::black_box(result)
            });
        });
    }
    runtime.block_on(sessions::revoke_all(&mut conn, user.id))?;
    criterion.final_summary();
    Ok(())
}
fn encode_standard(bytes: &[u8; 32]) -> String {
    // Reuse base64 through the bench's existing direct dependency.
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(bytes)
}
