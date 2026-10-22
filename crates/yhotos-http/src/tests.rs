#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use figment::Figment;
use http_body_util::BodyExt;
use secrecy::ExposeSecret;
use std::net::SocketAddr;
use tower::ServiceExt;
use yhotos_config::Config;

pub(crate) fn state(trust_proxy: bool, cheap: u32) -> AppState {
    state_with_global_cap(trust_proxy, cheap, 64)
}

pub(crate) fn state_with_global_cap(trust_proxy: bool, cheap: u32, global: u16) -> AppState {
    let dir = tempfile::tempdir().unwrap();
    let f = Figment::new()
        .merge((
            "database_url_public",
            format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()),
        ))
        .merge((
            "session_key",
            STANDARD.encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7())),
        ))
        .merge(("data_dir", dir.path().to_string_lossy().as_ref()))
        .merge(("public_base_url", "https://example.test"))
        .merge(("admin_listen", "127.0.0.1:8081"))
        .merge(("admin_base_url", "https://admin.example.test:8443"))
        .merge(("database_url_admin", "postgres://127.0.0.1/test"))
        .merge(("trust_proxy", trust_proxy))
        .merge(("global_stream_cap", global))
        .merge(("rate_limits.cheap_per_min", cheap));
    let config = Box::leak(Box::new(Config::from_figment(f).unwrap()));
    AppState::new(
        config,
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1/test")
            .unwrap(),
        yhotos_storage::Store::open(dir.path()).unwrap(),
        None,
    )
}
fn request(path: &str, ip: &str) -> Request<Body> {
    let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(ip.parse::<SocketAddr>().unwrap()));
    req
}
async fn body(response: axum::response::Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
fn security(headers: &HeaderMap, cross_origin: bool) {
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    for directive in [
        "default-src 'none'",
        "img-src 'self'",
        "script-src 'nonce-",
        "style-src 'self' 'nonce-",
        "connect-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "form-action 'self'",
    ] {
        assert!(csp.contains(directive), "{csp}");
    }
    assert!(!csp.contains("unsafe-inline"));
    assert!(!csp.contains("unsafe-eval"));
    assert_eq!(
        headers[header::STRICT_TRANSPORT_SECURITY],
        "max-age=63072000; includeSubDomains; preload"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(matches!(
        headers[header::REFERRER_POLICY].to_str().unwrap(),
        "strict-origin-when-cross-origin" | "no-referrer"
    ));
    assert!(
        headers["permissions-policy"]
            .to_str()
            .unwrap()
            .contains("camera=()")
    );
    assert_eq!(headers["cross-origin-opener-policy"], "same-origin");
    assert_eq!(
        headers["cross-origin-resource-policy"],
        if cross_origin {
            "cross-origin"
        } else {
            "same-origin"
        }
    );
    assert!(!headers.contains_key(header::SERVER));
    assert!(headers.contains_key("x-request-id"));
}
#[tokio::test]
async fn health_is_minimal_and_server_header_is_removed() {
    let app = public_router(state(false, 600));
    let health = app
        .clone()
        .oneshot(request("/healthz", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    security(health.headers(), false);
    assert_eq!(health.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(health.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body(health).await, "ok");
    let server = app
        .clone()
        .oneshot(request("/__test/server", "127.0.0.1:1"))
        .await
        .unwrap();
    security(server.headers(), false);
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn templates_assets_and_errors_get_nonce_and_security_headers(pool: sqlx::PgPool) {
    let initial = state(false, 600);
    let app = public_router(AppState::new(
        initial.config,
        pool,
        initial.store.clone(),
        None,
    ));
    let home = app
        .clone()
        .oneshot(request("/", "127.0.0.1:1"))
        .await
        .unwrap();
    security(home.headers(), false);
    let csp = home.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_owned();
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    assert_eq!(STANDARD.decode(nonce).unwrap().len(), 16);
    assert_eq!(csp.matches(&format!("'nonce-{nonce}'")).count(), 2);
    let html = body(home).await;
    assert!(html.contains("Yhotos photo gallery"));
    assert!(html.contains(&format!("nonce=\"{nonce}\"")));
    for tag in html
        .split('<')
        .filter(|s| s.starts_with("script") || s.starts_with("style"))
    {
        assert!(tag.contains(&format!("nonce=\"{nonce}\"")));
        if tag.starts_with("script") {
            assert!(tag.contains("src=\"/static/"));
        }
    }
    for script in html.split("<script").skip(1) {
        let (_, content) = script.split_once('>').unwrap();
        assert!(
            content.starts_with("</script>"),
            "scripts must have no inline body"
        );
    }
    let second = app
        .clone()
        .oneshot(request("/", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_ne!(second.headers()[header::CONTENT_SECURITY_POLICY], csp);
    for (url, content_type) in [
        (
            static_assets::stylesheet_url().unwrap(),
            "text/css; charset=utf-8",
        ),
        (
            static_assets::script_url().unwrap(),
            "text/javascript; charset=utf-8",
        ),
    ] {
        assert!(html.contains(url));
        let response = app
            .clone()
            .oneshot(request(url, "127.0.0.1:1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        security(response.headers(), true);
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
    }
    for path in [
        "/missing",
        "/static/app.css",
        "/static/app.bad.css",
        "/static/../Cargo.toml",
        "/static/%2e%2e/Cargo.toml",
        "/i/missing",
        "/o/missing",
        "/v/missing",
    ] {
        let response = app
            .clone()
            .oneshot(request(path, "127.0.0.1:1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        security(
            response.headers(),
            middleware::security_headers::cross_origin_resource(path),
        );
    }
}
#[tokio::test]
async fn forbidden_equals_not_found_and_internal_details_are_private() {
    let app = public_router(state(false, 600));
    let denied = app
        .clone()
        .oneshot(request("/__test/forbidden", "127.0.0.1:1"))
        .await
        .unwrap();
    let missing = app
        .clone()
        .oneshot(request("/__test/not-found", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_eq!(denied.status(), missing.status());
    assert_eq!(
        denied.headers()[header::CONTENT_TYPE],
        missing.headers()[header::CONTENT_TYPE]
    );
    assert_eq!(body(denied).await, body(missing).await);
    let response = app
        .oneshot(request("/__test/internal", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    security(response.headers(), false);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        body(response).await,
        format!("Internal server error. Request id: {id}")
    );
}
#[tokio::test]
async fn request_ids_are_generated_and_spoofing_is_ignored() {
    let app = public_router(state(false, 600));
    let client_id = uuid::Uuid::now_v7().to_string();
    let mut req = request("/healthz", "127.0.0.1:1");
    req.headers_mut()
        .insert("x-request-id", client_id.parse().unwrap());
    let first = app.clone().oneshot(req).await.unwrap();
    let id = first.headers()["x-request-id"].to_str().unwrap();
    assert_ne!(id, client_id);
    assert_eq!(id.parse::<uuid::Uuid>().unwrap().get_version_num(), 7);
    let second = app
        .oneshot(request("/healthz", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_ne!(
        first.headers()["x-request-id"],
        second.headers()["x-request-id"]
    );
}
#[tokio::test]
async fn cheap_budget_is_per_ip() {
    let app = public_router(state(false, 3));
    for _ in 0..3 {
        assert_eq!(
            app.clone()
                .oneshot(request("/healthz", "127.0.0.1:1"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    let response = app
        .clone()
        .oneshot(request("/healthz", "127.0.0.1:2"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    security(response.headers(), false);
    assert_eq!(
        app.oneshot(request("/healthz", "127.0.0.2:1"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
#[tokio::test]
async fn proxy_trust_uses_only_cloudflare_and_rejects_invalid_values() {
    for trust in [false, true] {
        let app = public_router(state(trust, 1));
        for (ip, expected) in [
            ("192.0.2.1", StatusCode::OK),
            (
                "192.0.2.2",
                if trust {
                    StatusCode::OK
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                },
            ),
        ] {
            let mut req = request("/healthz", "127.0.0.1:1");
            req.headers_mut()
                .insert("cf-connecting-ip", ip.parse().unwrap());
            req.headers_mut()
                .insert("x-forwarded-for", ip.parse().unwrap());
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
        }
    }
    let app = public_router(state(true, 600));
    for value in [None, Some("not-an-ip"), Some("192.0.2.1, 192.0.2.2")] {
        let mut req = request("/healthz", "127.0.0.1:1");
        req.headers_mut()
            .insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
        if let Some(value) = value {
            req.headers_mut()
                .insert("cf-connecting-ip", value.parse().unwrap());
        }
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut req = request("/healthz", "127.0.0.1:1");
    req.headers_mut()
        .append("cf-connecting-ip", "192.0.2.1".parse().unwrap());
    req.headers_mut()
        .append("cf-connecting-ip", "192.0.2.2".parse().unwrap());
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn admin_never_leaks_onto_public_listener() {
    let state = state(false, 600);
    assert_eq!(
        public_router(state.clone())
            .oneshot(request("/admin/healthz", "127.0.0.1:1"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    #[cfg(feature = "admin")]
    {
        let response = admin_router(state)
            .oneshot(request("/admin/healthz", "127.0.0.1:1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/admin/login");
    }
}
#[tokio::test(start_paused = true)]
async fn early_rejections_have_security_headers() {
    let app = public_router(state(false, 600));
    let mut oversized = request("/__test/body", "127.0.0.1:1");
    *oversized.method_mut() = axum::http::Method::POST;
    oversized.headers_mut().insert(
        header::CONTENT_LENGTH,
        (1024 * 1024 + 1).to_string().parse().unwrap(),
    );
    *oversized.body_mut() = Body::from(vec![0; 1024 * 1024 + 1]);
    let response = app.clone().oneshot(oversized).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    security(response.headers(), false);
    let response = app
        .oneshot(request("/__test/slow", "127.0.0.1:1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    security(response.headers(), false);
}
#[tokio::test]
async fn csrf_tokens_are_generated_validated_and_body_is_replayed() {
    let app = public_router(state(false, 600));
    let response = app
        .clone()
        .oneshot(request("/__test/form", "127.0.0.1:1"))
        .await
        .unwrap();
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.contains("Secure; HttpOnly; SameSite=Lax"));
    let cookie_pair = cookie.split(';').next().unwrap();
    let token = cookie_pair.split('=').nth(1).unwrap();
    assert!(body(response).await.contains(token));
    let other = middleware::csrf::CsrfToken::generate().unwrap();
    assert!(
        other
            .cookie(true)
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("SameSite=Strict")
    );
    for (cookie_header, value, expected) in [
        (Some(cookie_pair.to_owned()), token, StatusCode::OK),
        (None, token, StatusCode::FORBIDDEN),
        (
            Some(cookie_pair.to_owned()),
            other.form_value(),
            StatusCode::FORBIDDEN,
        ),
        (
            Some(format!("{cookie_pair}; {cookie_pair}")),
            token,
            StatusCode::FORBIDDEN,
        ),
        (Some(cookie_pair.to_owned()), "bad", StatusCode::FORBIDDEN),
    ] {
        let mut req = request("/__test/form", "127.0.0.1:1");
        *req.method_mut() = axum::http::Method::POST;
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        if let Some(cookie) = cookie_header {
            req.headers_mut()
                .insert(header::COOKIE, cookie.parse().unwrap());
        }
        *req.body_mut() = Body::from(format!("_csrf={value}"));
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), expected);
        security(response.headers(), false);
    }
}

#[tokio::test]
async fn auth_posts_share_a_bucket_and_get_forms_use_the_cheap_bucket() {
    let state = state(false, 600);
    let config = Box::leak(Box::new(Config {
        rate_limits: yhotos_config::RateLimits {
            auth_per_min: 1,
            ..state.config.rate_limits
        },
        ..Config::from_figment(
            figment::Figment::new()
                .merge((
                    "database_url_public",
                    state.config.database_url_public.expose_secret(),
                ))
                .merge((
                    "data_dir",
                    std::env::current_dir().unwrap().to_string_lossy().as_ref(),
                ))
                .merge(("public_base_url", "https://example.test"))
                .merge(("session_key", state.config.session_key.expose_secret()))
                .merge(("admin_listen", "127.0.0.1:8081"))
                .merge(("admin_base_url", "https://admin.example.test:8443"))
                .merge(("database_url_admin", "postgres://127.0.0.1/test")),
        )
        .unwrap()
    }));
    let app = public_router(AppState::new(
        config,
        state.pool.clone(),
        state.store.clone(),
        None,
    ));
    for path in ["/login", "/login/mfa", "/u/security"] {
        let response = app
            .clone()
            .oneshot(request(path, "127.0.0.1:1"))
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    let mut first = request("/login/mfa", "127.0.0.1:1");
    *first.method_mut() = axum::http::Method::POST;
    assert_eq!(
        app.clone().oneshot(first).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut second = request("/u/security/passkey/start", "127.0.0.1:1");
    *second.method_mut() = axum::http::Method::POST;
    assert_eq!(
        app.oneshot(second).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn map_tile_csp_does_not_weaken_share_page_headers(pool: sqlx::PgPool) {
    let initial = state(false, 600);
    let worker = crate::static_assets::map_worker_url().unwrap();
    let app = public_router(AppState::new(
        initial.config,
        pool,
        initial.store.clone(),
        None,
    ));
    for path in [
        "/map",
        worker,
        "/s/invalid",
        "/s/invalid/unlock",
        "/map/tiles.json?bbox=-80,43,-79,44&zoom=1",
        "/map/photos.json?lon=0&lat=0",
    ] {
        let response = app
            .clone()
            .oneshot(request(path, "127.0.0.1:1"))
            .await
            .unwrap();
        let headers = response.headers();
        security(headers, path == worker);
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        if path == "/map" || path == worker {
            assert!(csp.contains("img-src 'self' data: https://tiles.openfreemap.org"));
            assert!(csp.contains("connect-src 'self' https://tiles.openfreemap.org"));
            assert!(csp.contains("worker-src 'self'"));
        } else {
            assert!(!csp.contains("openfreemap"), "{path}");
            assert!(!csp.contains("worker-src"), "{path}");
        }
        if path.starts_with("/s/") {
            assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
            assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        }
    }
}
