//! Listener-bound session loading and mandatory password/admin enrollment gates.
use crate::{
    AppState,
    auth::{self, Audience},
    error::HttpError,
};
use axum::{
    Extension,
    extract::{Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use yhotos_auth::{SessionPolicy, SessionState, TokenKind, session_state};
use yhotos_core::{GroupId, UserId, access::Principal};
use yhotos_db::sessions;

/// An owned verified identity; request headers and cookies never construct this type.
#[derive(Clone, Debug, Default)]
pub enum SessionIdentity {
    /// No valid, unexpired session for this listener.
    #[default]
    Anonymous,
    /// Supplied only by trusted session loading, with verified group memberships.
    User {
        /// Verified user id.
        id: UserId,
        /// Verified group membership.
        groups: Vec<GroupId>,
        /// Verified administrator flag.
        is_admin: bool,
    },
}
impl SessionIdentity {
    /// Borrows the pure access principal without copying group memberships.
    #[must_use]
    pub fn principal(&self) -> Principal<'_> {
        match self {
            Self::Anonymous => Principal::Anonymous,
            Self::User {
                id,
                groups,
                is_admin,
            } => Principal::User {
                id: *id,
                groups,
                is_admin: *is_admin,
            },
        }
    }
}
/// Authenticated session context, available only after expiry and audience validation.
#[derive(Clone)]
pub struct Current(pub Arc<sessions::Loaded>);
fn timestamp(t: time::OffsetDateTime) -> Option<std::time::Duration> {
    u64::try_from(t.unix_timestamp())
        .ok()
        .map(std::time::Duration::from_secs)
}
async fn run(
    state: &AppState,
    a: Audience,
    mut request: Request,
    next: Next,
) -> Result<Response, HttpError> {
    let name = a.cookie_name(state);
    let carried = request.headers().get_all(header::COOKIE).iter().any(|h| {
        h.to_str().is_ok_and(|v| {
            v.split(';')
                .any(|p| p.trim().split_once('=').is_some_and(|(k, _)| k == name))
        })
    });
    let hash = auth::token_hash(request.headers(), name, TokenKind::Session);
    let loaded = if let (Some(auth), Some(hash)) = (&state.auth, &hash) {
        if a.0 {
            // Admin privilege and MFA gates must observe account changes and
            // revocations on every request, including changes in another process.
            let mut c = auth.db(a)?.pool().acquire().await?;
            sessions::load(&mut c, hash, a.0).await?.map(Arc::new)
        } else {
            auth.load(hash).await?
        }
    } else {
        None
    };
    let now = auth::now();
    let loaded = loaded.filter(|s| {
        let p = if s.is_admin {
            SessionPolicy::admin()
        } else {
            SessionPolicy::default()
        };
        hash.as_ref()
            .is_some_and(|h| bool::from(s.session.token_hash.as_slice().ct_eq(h.as_slice())))
            && !s.disabled
            && s.session.admin == a.0
            && s.session.expires_at > time::OffsetDateTime::now_utc()
            && timestamp(s.session.created_at)
                .zip(timestamp(s.session.last_seen_at))
                .is_some_and(|(created, seen)| {
                    session_state(created, seen, now, p) == SessionState::Active
                })
    });
    let mut identity = SessionIdentity::Anonymous;
    let mut gate = None;
    let path = request.uri().path();
    if let Some(s) = &loaded {
        if a.0 && !s.is_admin {
            return Err(HttpError::NotFound);
        }
        identity = SessionIdentity::User {
            id: s.session.user_id,
            groups: s.groups.clone(),
            is_admin: a.0 && s.is_admin,
        };
        if state.auth.is_some() {
            let ip = auth::ip(state, &request)?;
            if !bool::from(s.session.ip_hash.as_slice().ct_eq(ip.as_slice()))
                || s.session.ua_class != auth::ua(request.headers())
            {
                // Only coarse fingerprints and an outcome, never raw network data.
                tracing::info!(user_id=%s.session.user_id,"session fingerprint changed");
            }
        }
        if s.must_change_password
            && path != a.password()
            && path != if a.0 { "/admin/logout" } else { "/logout" }
            && !path.starts_with("/static/")
        {
            gate = Some(
                if path.starts_with("/api/") || path.starts_with("/admin/api/") {
                    HttpError::PasswordChangeRequired.into_response()
                } else {
                    auth::redirect(a.password())
                },
            );
        } else if a.0 && (s.session.mfa_at.is_none() || !s.has_mfa) && !setup_allowed(path) {
            gate = Some(auth::redirect("/admin/setup-mfa"));
        }
        request.extensions_mut().insert(Current(s.clone()));
    } else if a.0 && !path.starts_with("/admin/login") && !path.starts_with("/static/") {
        gate = Some(auth::redirect(a.login()));
    }
    let links = if a.0 {
        crate::share::Links::default()
    } else {
        crate::share::resolve(state, request.headers()).await?
    };
    let has_links = !links.0.is_empty();
    request.extensions_mut().insert(links);
    request.extensions_mut().insert(identity);
    let mut response = if let Some(gate) = gate {
        gate
    } else {
        next.run(request).await
    };
    if carried && loaded.is_none() && !replaces_cookie(&response, name) {
        auth::set_cookie(&mut response, a.cookie(name, "", true)?);
    }
    if loaded.is_some() || has_links {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    }
    Ok(response)
}
fn replaces_cookie(response: &Response, name: &str) -> bool {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|value| {
            value
                .to_str()
                .is_ok_and(|value| value.split_once('=').is_some_and(|(key, _)| key == name))
        })
}
fn setup_allowed(path: &str) -> bool {
    matches!(
        path,
        "/admin/setup-mfa"
            | "/admin/u/password"
            | "/admin/logout"
            | "/admin/u/security/totp"
            | "/admin/u/security/passkey/start"
            | "/admin/u/security/passkey/finish"
    ) || path.starts_with("/static/")
        || path.starts_with("/admin/login")
}
pub(crate) async fn load(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    request: Request,
    next: Next,
) -> Response {
    match run(&state, a, request, next).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Current {
    type Rejection = Response;
    fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            auth::redirect(
                parts
                    .extensions
                    .get::<Audience>()
                    .copied()
                    .unwrap_or(Audience(false))
                    .login(),
            )
        }))
    }
}
