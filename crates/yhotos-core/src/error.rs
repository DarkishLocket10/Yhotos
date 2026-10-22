//! Typed domain parsing failures without infrastructure details.

/// Preserves the reason a slug or short lookup key was rejected.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A canonical URL slug could not be validated.
    #[error(transparent)]
    Slug(#[from] crate::slug::SlugError),
    /// A short photo lookup key could not be validated.
    #[error(transparent)]
    ShortId(#[from] crate::short_id::ShortIdError),
}
