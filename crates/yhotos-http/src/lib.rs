#![forbid(unsafe_code)]

//! axum routers, handlers, templates, middleware, static assets.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-http");
    }
}
