#![forbid(unsafe_code)]

//! Domain values and access decisions, without IO or an async runtime.

pub mod access;
pub mod error;
pub mod ids;
pub mod short_id;
pub mod slug;
pub mod thumbhash;

pub use ids::*;
pub use short_id::ShortId;
pub use slug::Slug;
