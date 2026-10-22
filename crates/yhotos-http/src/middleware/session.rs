//! Trusted session boundary. Task 009 will replace the anonymous loader.

use axum::{extract::Request, middleware::Next, response::Response};
use yhotos_core::{GroupId, UserId, access::Principal};

/// An owned verified identity; request headers and cookies never construct this type.
#[derive(Clone, Debug, Default)]
pub enum SessionIdentity {
    /// Until real session resolution exists, every network request is anonymous.
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
pub(crate) async fn load(mut request: Request, next: Next) -> Response {
    request
        .extensions_mut()
        .get_or_insert_with(SessionIdentity::default);
    next.run(request).await
}
