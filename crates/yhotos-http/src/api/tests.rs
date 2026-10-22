#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use crate::{public_router, tests::state};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use tower::ServiceExt;
fn request(method: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri("/api/v1/photos/invalid/like")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
    req
}
#[tokio::test]
async fn csrf_requires_custom_header_and_rejects_foreign_and_duplicate_proof() {
    let app = public_router(state(false, 100_000));
    for (custom, site, origin, status) in [
        (false, None, None, StatusCode::FORBIDDEN),
        (false, Some("same-origin"), None, StatusCode::FORBIDDEN),
        (true, Some("cross-site"), None, StatusCode::FORBIDDEN),
        (true, Some("same-site"), None, StatusCode::FORBIDDEN),
        (true, Some("none"), None, StatusCode::FORBIDDEN),
        (
            true,
            Some("same-origin"),
            Some("https://evil.test"),
            StatusCode::FORBIDDEN,
        ),
        (true, None, None, StatusCode::NOT_FOUND),
        (true, Some("same-origin"), None, StatusCode::NOT_FOUND),
    ] {
        for method in ["POST", "DELETE"] {
            let mut req = request(method);
            if custom {
                req.headers_mut()
                    .insert("x-requested-with", "yhotos".parse().unwrap());
            }
            if let Some(site) = site {
                req.headers_mut()
                    .insert("sec-fetch-site", site.parse().unwrap());
            }
            if let Some(origin) = origin {
                req.headers_mut()
                    .insert(header::ORIGIN, origin.parse().unwrap());
            }
            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        }
    }
    let mut req = request("POST");
    req.headers_mut()
        .append("x-requested-with", "yhotos".parse().unwrap());
    req.headers_mut()
        .append("x-requested-with", "yhotos".parse().unwrap());
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}
#[tokio::test(start_paused = true)]
async fn sixty_mutations_per_ip_and_reads_have_separate_budgets() {
    let app = public_router(state(false, 100_000));
    for _ in 0..60 {
        let mut req = request("POST");
        req.headers_mut()
            .insert("x-requested-with", "yhotos".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = app.clone().oneshot(request("POST")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"{\"error\":\"rate_limited\"}");
    assert_eq!(
        app.oneshot(request("GET")).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

fn fixture_photo() -> yhotos_db::photos::PagePhoto {
    let id = yhotos_core::PhotoId::new();
    serde_json::from_value(serde_json::json!({"id":id,"short_id":yhotos_core::ShortId::from_photo_id(id),"album_id":yhotos_core::AlbumId::new(),"title":"</script><script>alert(1)</script>","description":"Garden","original_hash":"a".repeat(64),"original_name":"test.jpeg","kind":"image","dominant_colors":[],"width":1500,"height":1000,"derivatives":[{"variant":"large","format":"jpeg","hdr":false,"hash":"b".repeat(64),"w":2048,"h":1365}]})).unwrap()
}
#[test]
fn wire_caps_never_disclose_original_urls_without_download_permission() {
    use super::*;
    let photo = fixture_photo();
    let wire = pages::island_photo(
        &photo,
        Caps {
            download: false,
            like: false,
            flag: false,
        },
    )
    .unwrap();
    assert!(wire["original"].is_null());
    assert_eq!(wire["downloads"].as_array().unwrap().len(), 0);
    assert!(!wire.to_string().contains(&photo.original_hash));
}
#[test]
fn capability_union_requires_an_independently_covering_scope() {
    use super::*;
    use crate::share::{Links, ScopedLink};
    use yhotos_core::{ShareLinkId, access::ShareTarget};
    use yhotos_db::share_links::LinkFlags;
    let mut photo = fixture_photo();
    photo.photo_link_view = true;
    let identity = SessionIdentity::Anonymous;
    let links = Links(vec![ScopedLink {
        target: ShareTarget::Photo(photo.id),
        link_id: ShareLinkId::new(),
        passphrase_version: 0,
        caps: LinkFlags {
            view: true,
            download: false,
            like: true,
            flag: false,
        },
    }]);
    let cap = caps(&photo, &identity, &links);
    assert!(cap.like);
    assert!(!cap.download);
    assert!(!cap.flag);
    // A linked photo whose album chain is restricted is not covered by its photo link.
    photo.photo_link_view = false;
    assert!(!caps(&photo, &identity, &links).like);
    photo.photo_link_view = true;
    photo.id = yhotos_core::PhotoId::new();
    assert!(!caps(&photo, &identity, &links).like);
    photo.account_view = true;
    let user = SessionIdentity::User {
        id: yhotos_core::UserId::new(),
        groups: vec![],
        is_admin: false,
    };
    let cap = caps(&photo, &user, &links);
    assert!(cap.like && cap.download && cap.flag);
}
#[test]
fn sixty_photo_island_has_safe_json_native_links_and_fits_html_budget() {
    use super::*;
    use std::io::Write;
    let photo = fixture_photo();
    let album = yhotos_db::albums::AlbumCard {
        id: photo.album_id,
        title: "Fixture".into(),
        description: "Garden".into(),
        path: "gallery".into(),
        count: 60,
        cover: None,
    };
    let page = yhotos_db::albums::AlbumPage {
        album,
        chain: vec![],
        children: vec![],
        photos: (0..60)
            .map(|_| {
                let mut p = fixture_photo();
                p.album_id = photo.album_id;
                p.original_hash = blake3::hash(p.id.to_string().as_bytes())
                    .to_hex()
                    .to_string();
                p.derivatives = [("small", 640), ("medium", 1280), ("large", 2048)]
                    .into_iter()
                    .flat_map(|(variant, w)| {
                        ["jpeg", "avif", "jxl"].map(|format| yhotos_db::photos::PageDerivative {
                            variant: variant.into(),
                            format: format.into(),
                            hdr: false,
                            hash: blake3::hash(format!("{}-{variant}-{format}", p.id).as_bytes())
                                .to_hex()
                                .to_string(),
                            w,
                            h: w * 2 / 3,
                        })
                    })
                    .collect();
                p
            })
            .collect(),
        total: 60,
    };
    let context = pages::PageContext {
        nonce: "fixture-nonce",
        stylesheet: "/static/app.fixture.css",
        script: "/static/thumbhash.fixture.js",
        public_base_url: "https://example.test",
    };
    let html = pages::render_album(&page, 1, yhotos_db::photos::PageSort::Taken, &context).unwrap();
    assert_eq!(html.matches("data-photo-link").count(), 60);
    assert!(html.contains("<dialog id=\"lightbox\""));
    assert!(!html.contains("data-like"));
    assert!(!html.contains("data-flag"));
    let json = html
        .split("id=\"gallery-data\">")
        .nth(1)
        .unwrap()
        .split("</script>")
        .next()
        .unwrap();
    assert!(!json.contains("</script>"));
    assert!(!json.contains('<'));
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["photos"][0]["title"], page.photos[0].title);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(html.as_bytes()).unwrap();
    let bytes = encoder.finish().unwrap().len();
    assert!(bytes < 30 * 1024, "{bytes} bytes gzip");
}

#[test]
fn hidden_controls_are_available_for_capabilities_on_later_grid_pages() {
    use crate::{
        middleware::session::SessionIdentity,
        pages,
        share::{Links, ScopedLink},
    };
    use yhotos_core::{PhotoId, ShareLinkId, access::ShareTarget};
    use yhotos_db::{
        albums::{AlbumCard, AlbumPage},
        photos::PageSort,
        share_links::LinkFlags,
    };
    let mut photo = fixture_photo();
    photo.shared_albums.push(photo.album_id);
    let links = Links(vec![
        ScopedLink {
            target: ShareTarget::Album(photo.album_id),
            link_id: ShareLinkId::new(),
            passphrase_version: 0,
            caps: LinkFlags {
                view: true,
                ..LinkFlags::default()
            },
        },
        ScopedLink {
            target: ShareTarget::Photo(PhotoId::new()),
            link_id: ShareLinkId::new(),
            passphrase_version: 0,
            caps: LinkFlags {
                view: true,
                download: true,
                like: true,
                flag: true,
            },
        },
    ]);
    let page = AlbumPage {
        album: AlbumCard {
            id: photo.album_id,
            title: "Gallery".into(),
            description: String::new(),
            path: "gallery".into(),
            count: 61,
            cover: None,
        },
        chain: vec![],
        children: vec![],
        photos: vec![photo],
        total: 61,
    };
    let context = pages::PageContext {
        nonce: "fixture-nonce",
        stylesheet: "/static/app.fixture.css",
        script: "/static/thumbhash.fixture.js",
        public_base_url: "https://example.test",
    };
    let html = pages::render_album_for(
        &page,
        1,
        PageSort::Taken,
        &context,
        &SessionIdentity::Anonymous,
        &links,
    )
    .unwrap();
    assert!(html.contains("data-like aria-pressed=\"false\" hidden"));
    assert!(html.contains("data-flag aria-pressed=\"false\" hidden"));
    assert!(html.contains("data-download"));
    let json = html
        .split("id=\"gallery-data\">")
        .nth(1)
        .unwrap()
        .split("</script>")
        .next()
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(
        data["photos"][0]["caps"],
        serde_json::json!({"download":false,"like":false,"flag":false})
    );
    assert!(data["photos"][0]["original"].is_null());
}
