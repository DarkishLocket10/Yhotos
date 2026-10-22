#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::indexing_slicing
)]
use super::*;
use axum::{body::Body, extract::ConnectInfo, http::Request};
use flate2::{Compression, write::GzEncoder};
use http_body_util::BodyExt;
use sqlx::{ConnectOptions, PgPool};
use std::{io::Write, net::SocketAddr};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_core::{
    AlbumId, PhotoId, UserId,
    access::{Grant, ShareTarget, Visibility},
};
use yhotos_db::grants;

struct Fixture {
    state: AppState,
    album: AlbumId,
    photos: Vec<ShortId>,
    user: UserId,
}
impl Fixture {
    async fn new(pool: PgPool, count: usize) -> Self {
        let initial = crate::tests::state(false, 100_000);
        let state = AppState::new(initial.config, pool, initial.store.clone());
        let mut conn = state.pool.acquire().await.unwrap();
        let album = AlbumId(Uuid::from_u128(100));
        let user = UserId(Uuid::from_u128(200));
        sqlx::query!("INSERT INTO albums(id,slug,title,description) VALUES($1,'gallery','Fixture gallery','Photos from the garden')",album.0).execute(&mut *conn).await.unwrap();
        let mut photos = Vec::new();
        for i in 0..count {
            let id = PhotoId(Uuid::from_u128((i + 1) as u128));
            let short = ShortId::from_photo_id(id);
            let key = short.to_string();
            let title = format!("Photo {:02}", count - i);
            let hash = blake3::hash(title.as_bytes()).to_hex().to_string();
            let order = i32::try_from(i).unwrap();
            let kind = match i % 3 {
                1 => "raw",
                2 => "video",
                _ => "image",
            };
            let (width, height) = match i % 3 {
                1 => (900, 1200),
                2 => (1600, 900),
                _ => (1500, 1000),
            };
            let placeholder = vec![
                0, 8, 2, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,description,sort_order,original_hash,original_name,original_bytes,mime,kind,width,height,dominant_colors,placeholder,camera_make,camera_model,iso,gps_lat,gps_lon,taken_at,posted_at) VALUES($1,$2,$3,$4,'Garden detail',$5,$6,'garden.jpeg',1,'image/jpeg',$7::text::media_kind,$8,$9,ARRAY[2443131],$10,'Fixture','Camera',100,43.65,-79.38,'2026-01-01'::timestamptz + $5::integer*interval '1 day','2026-06-01'::timestamptz - $5::integer*interval '1 day')",id.0,key,album.0,title,order,hash,kind,width,height,placeholder).execute(&mut *conn).await.unwrap();
            for (variant, w) in [("small", 640), ("medium", 1280), ("large", 2048)] {
                for format in ["jpeg", "avif", "jxl"] {
                    let digest = blake3::hash(format!("{key}-{variant}-{format}").as_bytes())
                        .to_hex()
                        .to_string();
                    let h = w * height / width;
                    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,$3,$4,$5,$6,$7,1)",Uuid::now_v7(),id.0,variant,format,digest,w,h).execute(&mut *conn).await.unwrap();
                }
            }
            photos.push(short);
        }
        Self {
            state,
            album,
            photos,
            user,
        }
    }
    fn request(path: &str) -> Request<Body> {
        let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1122".parse::<SocketAddr>().unwrap()));
        req
    }
    async fn get(&self, path: &str) -> Response {
        self.get_as(path, SessionIdentity::Anonymous).await
    }
    async fn get_as(&self, path: &str, identity: SessionIdentity) -> Response {
        let mut request = Self::request(path);
        request.extensions_mut().insert(identity);
        crate::public_router(self.state.clone())
            .oneshot(request)
            .await
            .unwrap()
    }
    async fn album_row(&self, identity: &SessionIdentity) -> albums::AlbumPage {
        albums::page(
            &mut self.state.pool.acquire().await.unwrap(),
            &identity.principal(),
            &[Slug::new("gallery").unwrap()],
            1,
            PageSort::Taken,
        )
        .await
        .unwrap()
        .unwrap()
    }
}
async fn body(response: Response) -> String {
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
fn context() -> PageContext<'static> {
    PageContext {
        nonce: "fixture-nonce",
        stylesheet: "/static/app.fixture.css",
        script: "/static/thumbhash.fixture.js",
        public_base_url: "https://gallery.example",
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn album_snapshots_anonymous_and_user_with_grant(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(3))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    grants::grant_photo(&mut conn, PhotoId(Uuid::from_u128(3)), Grant::User(f.user))
        .await
        .unwrap();
    drop(conn);
    let anonymous = f.album_row(&SessionIdentity::Anonymous).await;
    assert_eq!(anonymous.total, 2);
    insta::assert_snapshot!(
        "album_anonymous",
        render_album(&anonymous, 1, PageSort::Taken, &context()).unwrap()
    );
    let identity = SessionIdentity::User {
        id: f.user,
        groups: vec![],
        is_admin: false,
    };
    let granted = f.album_row(&identity).await;
    assert_eq!(granted.total, 3);
    insta::assert_snapshot!(
        "album_granted",
        render_album(&granted, 1, PageSort::Taken, &context()).unwrap()
    );
    let mut req = Fixture::request("/a/gallery");
    req.extensions_mut().insert(identity);
    let response = crate::public_router(f.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert!(body(response).await.contains("3 photos"));
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn pagination_sorting_counts_and_private_cache(pool: PgPool) {
    let f = Fixture::new(pool, 62).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(1))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    for (sort, expected) in [
        ("taken", 62),
        ("posted", 2),
        ("name", 62),
        ("manual", 2),
        ("invalid", 62),
    ] {
        let row = albums::page(
            &mut conn,
            &yhotos_core::access::Principal::Anonymous,
            &[Slug::new("gallery").unwrap()],
            1,
            PageSort::parse(sort),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(row.photos.len(), 60);
        assert_eq!(row.total, 61);
        assert_eq!(row.photos[0].id, PhotoId(Uuid::from_u128(expected)));
    }
    drop(conn);
    let page = body(f.get("/a/gallery?per_page=1000").await).await;
    assert_eq!(page.matches("<img ").count(), 60);
    assert!(page.contains("Next page"));
    let second = body(f.get("/a/gallery?page=2&sort=manual").await).await;
    assert_eq!(second.matches("<img ").count(), 1);
    for suffix in [
        "?page=3",
        "?page=0",
        "?page=-1",
        "?page=4294967296",
        "?page=garbage",
    ] {
        assert_eq!(
            f.get(&format!("/a/gallery{suffix}")).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = f.get("/a/gallery").await;
    assert_eq!(response.headers()[header::VARY], "Cookie");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=60, stale-while-revalidate=600"
    );
    assert_eq!(
        response.headers()["cache-tag"],
        format!("album:{}", f.album)
    );
    for cookie in ["ys=invalid", "__Host-ys=invalid", "share=invalid"] {
        let mut req = Fixture::request("/a/gallery");
        req.headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        let response = crate::public_router(f.state.clone())
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn restricted_and_missing_are_byte_identical_and_children_are_filtered(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    albums::create(
        &mut conn,
        Some(f.album),
        &Slug::new("child").unwrap(),
        "Visible child",
        Visibility::Public,
    )
    .await
    .unwrap();
    let restricted = albums::create(
        &mut conn,
        None,
        &Slug::new("private").unwrap(),
        "Restricted secret",
        Visibility::Restricted,
    )
    .await
    .unwrap();
    albums::create(
        &mut conn,
        Some(restricted.id),
        &Slug::new("child").unwrap(),
        "Hidden child",
        Visibility::Public,
    )
    .await
    .unwrap();
    let home = body(f.get("/").await).await;
    assert!(!home.contains("Restricted secret"));
    assert!(home.contains("3 photos"));
    assert!(
        body(f.get("/a/gallery").await)
            .await
            .contains("Visible child")
    );
    let missing = f.get("/a/missing").await;
    let denied = f.get("/a/private").await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(denied).await, body(missing).await);
    assert_eq!(
        body(f.get("/a/private/child").await).await,
        body(f.get("/a/missing/child").await).await
    );
    albums::update(
        &mut conn,
        f.album,
        &Slug::new("gallery").unwrap(),
        "Hidden",
        "",
        Visibility::Restricted,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        body(f.get(&format!("/p/{}", f.photos[0])).await).await,
        body(f.get("/p/aaaaaaaaaaaz").await).await
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn photos_have_sorted_neighbours_metadata_hdr_downloads_and_og(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    let hash = "a".repeat(64);
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES($1,$2,'large','avif',true,$3,1536,2048,1)",Uuid::now_v7(),Uuid::from_u128(2),hash).execute(&mut *conn).await.unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(3))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    let row = photos::page(
        &mut conn,
        &yhotos_core::access::Principal::Anonymous,
        &f.photos[1],
        PageSort::Manual,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(row.previous, Some(f.photos[0]));
    assert_eq!(row.next, None);
    let html = render_photo(&row, PageSort::Manual, &context()).unwrap();
    for value in [
        "sizes=\"100vw\"",
        "media=\"(dynamic-range: high)\"",
        "og:image:width",
        "https://gallery.example/i/",
        "summary_large_image",
        "RAW",
        "Download original",
        "/o/",
        "Camera model",
        "OpenStreetMap",
        "rel=\"prev\"",
    ] {
        assert!(html.contains(value), "{value}");
    }
    assert!(!html.contains("prefetch"));
    let response = f.get(&format!("/p/{}?sort=manual", f.photos[1])).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["cache-tag"]
            .to_str()
            .unwrap()
            .contains("photo:")
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn dimensions_size_budget_and_xss_in_all_contexts(pool: PgPool) {
    let f = Fixture::new(pool, 60).await;
    let mut row = f.album_row(&SessionIdentity::Anonymous).await;
    row.photos[0].title = "<script>alert(1)</script>".into();
    row.album.title = row.photos[0].title.clone();
    row.album.description = "\" onload=\"<script>alert(1)</script>".into();
    let html = render_album(&row, 1, PageSort::Taken, &context()).unwrap();
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&#60;script&#62;alert(1)&#60;/script&#62;"));
    assert!(html.contains(r"\u003cscript\u003ealert(1)\u003c\/script\u003e")); // inert JSON only
    for img in html.split("<img ").skip(1) {
        let tag = img.split('>').next().unwrap();
        assert!(tag.contains("width=\""));
        assert!(tag.contains("height=\""));
    }
    assert!(html.contains("data:image/png;base64,"));
    assert!(html.contains("loading=\"lazy\""));
    assert!(html.contains("loading=\"eager\""));
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(html.as_bytes()).unwrap();
    let compressed = gz.finish().unwrap();
    eprintln!(
        "60-photo HTML: {} bytes raw, {} bytes gzip",
        html.len(),
        compressed.len()
    );
    assert!(compressed.len() < 30 * 1024, "{} bytes", compressed.len());
    assert!(include_str!("../../assets/css/app.css").len() < 30 * 1024);
    let page = photos::PhotoPage {
        photo: row.photos[0].clone(),
        chain: row.chain,
        previous: None,
        next: None,
    };
    let html = render_photo(&page, PageSort::Taken, &context()).unwrap();
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("property=\"og:title\" content=\"&#60;script&#62;"));
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn legacy_table_drives_only_relative_redirects(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    redirects::insert(&mut conn, "/category/12", ShareTarget::Album(f.album))
        .await
        .unwrap();
    redirects::insert(
        &mut conn,
        "/picture.php?/1/category/12",
        ShareTarget::Photo(PhotoId(Uuid::from_u128(1))),
    )
    .await
    .unwrap();
    redirects::insert(&mut conn, "/old-gallery", ShareTarget::Album(f.album))
        .await
        .unwrap();
    for url in [
        "/index.php?/category/12",
        "/index.php?/category/12-garden",
        "/index.php?/category/12-Garden_Views",
        "/legacy/old-gallery",
    ] {
        let response = f.get(url).await;
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(response.headers()[header::LOCATION], "/a/gallery");
    }
    let response = f.get("/picture.php?/1/category/12").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/p/{}", f.photos[0])
    );
    for url in [
        "/legacy/unknown",
        "/index.php?/category/0",
        "/index.php?/category/999",
        "/index.php?/category/12-../evil",
        "/picture.php?/1/category/12/evil",
    ] {
        assert_eq!(f.get(url).await.status(), StatusCode::NOT_FOUND);
    }
    assert!(
        redirects::insert(
            &mut conn,
            "https://evil.example/",
            ShareTarget::Album(f.album)
        )
        .await
        .is_err()
    );
    for target in [
        "https://evil.example/",
        "//evil.example/",
        "/a/../evil",
        "/a/%2f%2fevil",
        "/a/a\\evil",
        "/p/aaaaaaaaaaaa?next=https://evil.example",
    ] {
        assert!(!redirects::valid_destination(target));
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn legacy_visibility_matches_pages_for_targets_and_ancestors(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    let photo = PhotoId(Uuid::from_u128(1));
    let child = albums::create(
        &mut conn,
        Some(f.album),
        &Slug::new("child").unwrap(),
        "Child",
        Visibility::Public,
    )
    .await
    .unwrap();
    for (pattern, target) in [
        ("/category/12", ShareTarget::Album(f.album)),
        ("/category/13", ShareTarget::Album(child.id)),
        ("/picture.php?/1/category/12", ShareTarget::Photo(photo)),
        ("/legacy/missing-target", ShareTarget::Album(AlbumId::new())),
    ] {
        redirects::insert(&mut conn, pattern, target).await.unwrap();
    }
    let missing = body(f.get("/legacy/missing-target").await).await;
    photos::set_visibility(&mut conn, &[photo], Visibility::Restricted)
        .await
        .unwrap();
    let photo_url = "/picture.php?/1/category/12";
    let denied = f.get(photo_url).await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert!(!denied.headers().contains_key(header::LOCATION));
    assert_eq!(body(denied).await, missing);
    grants::grant_photo(&mut conn, photo, Grant::User(f.user))
        .await
        .unwrap();
    let user = SessionIdentity::User {
        id: f.user,
        groups: vec![],
        is_admin: false,
    };
    let allowed = f.get_as(photo_url, user.clone()).await;
    assert_eq!(allowed.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        allowed.headers()[header::LOCATION],
        format!("/p/{}", f.photos[0])
    );
    assert_eq!(
        allowed.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(allowed.headers()[header::VARY], "Cookie");
    albums::update(
        &mut conn,
        f.album,
        &Slug::new("gallery").unwrap(),
        "Hidden",
        "",
        Visibility::Restricted,
        false,
    )
    .await
    .unwrap();
    // A photo grant or a grant below the restricted ancestor cannot bypass it.
    grants::grant_album(&mut conn, child.id, Grant::User(f.user))
        .await
        .unwrap();
    for url in [
        "/index.php?/category/12",
        "/index.php?/category/13",
        photo_url,
    ] {
        for identity in [SessionIdentity::Anonymous, user.clone()] {
            let denied = f.get_as(url, identity).await;
            assert_eq!(denied.status(), StatusCode::NOT_FOUND);
            assert!(!denied.headers().contains_key(header::LOCATION));
            assert_eq!(body(denied).await, missing);
        }
    }
    grants::grant_album(&mut conn, f.album, Grant::User(f.user))
        .await
        .unwrap();
    for (url, destination) in [
        ("/index.php?/category/12", "/a/gallery".to_owned()),
        ("/index.php?/category/13", "/a/gallery/child".to_owned()),
        (photo_url, format!("/p/{}", f.photos[0])),
    ] {
        let allowed = f.get_as(url, user.clone()).await;
        assert_eq!(allowed.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(allowed.headers()[header::LOCATION], destination);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn responsive_sources_exclude_crops_and_resolve_width_ties(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let row = f.album_row(&SessionIdentity::Anonymous).await;
    for mut photo in row.photos {
        for format in ["jpeg", "avif", "jxl"] {
            for (variant, w) in [("thumb", 320), ("square", 640), ("square2x", 1280)] {
                photo.derivatives.push(photos::PageDerivative {
                    variant: variant.into(),
                    format: format.into(),
                    hdr: false,
                    hash: "0".repeat(64),
                    w,
                    h: w,
                });
            }
            let mut duplicate = photo
                .derivatives
                .iter()
                .find(|d| d.format == format && d.variant == "small")
                .unwrap()
                .clone();
            let mut thumb = duplicate.clone();
            thumb.variant = "thumbfull".into();
            thumb.hash = "e".repeat(64);
            thumb.w = 320;
            thumb.h /= 2;
            photo.derivatives.push(thumb);
            duplicate.hash = "f".repeat(64);
            duplicate.variant = "small2x".into();
            photo.derivatives.push(duplicate);
        }
        let mut page = photos::PhotoPage {
            photo,
            chain: vec![],
            previous: None,
            next: None,
        };
        let html = render_photo(&page, PageSort::Taken, &context()).unwrap();
        let picture = html
            .split("<picture>")
            .nth(1)
            .unwrap()
            .split("</picture>")
            .next()
            .unwrap();
        assert!(!picture.contains(&"0".repeat(64)));
        assert!(!picture.contains(&"f".repeat(64)));
        assert!(picture.contains("320w"));
        assert!(picture.contains("640w"));
        assert!(picture.contains("1280w"));
        assert!(picture.contains("2048w"));
        assert!(picture.contains(&format!(
            "width=\"{}\" height=\"{}\"",
            page.photo.width.unwrap(),
            page.photo.height.unwrap()
        )));
        page.photo.derivatives.reverse();
        assert_eq!(
            html,
            render_photo(&page, PageSort::Taken, &context()).unwrap()
        );
        page.photo.width = None;
        page.photo.height = None;
        let html = render_photo(&page, PageSort::Taken, &context()).unwrap();
        let picture = html
            .split("<picture>")
            .nth(1)
            .unwrap()
            .split("</picture>")
            .next()
            .unwrap();
        assert!(!picture.contains(&"0".repeat(64)));
        assert!(!picture.contains(&"f".repeat(64)));
        page.photo.derivatives.reverse();
        assert_eq!(
            html,
            render_photo(&page, PageSort::Taken, &context()).unwrap()
        );
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn downloads_include_sizes_without_jpeg(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    let row = f.album_row(&SessionIdentity::Anonymous).await;
    let mut photo = row.photos[0].clone();
    photo
        .derivatives
        .retain(|d| d.variant != "large" || d.format != "jpeg");
    let mut xlarge = photo
        .derivatives
        .iter()
        .find(|d| d.variant == "large" && d.format == "jxl")
        .unwrap()
        .clone();
    xlarge.variant = "xlarge".into();
    xlarge.w = 3200;
    xlarge.h = 2133;
    photo.derivatives.push(xlarge);
    let mut page = photos::PhotoPage {
        photo,
        chain: vec![],
        previous: None,
        next: None,
    };
    let html = render_photo(&page, PageSort::Taken, &context()).unwrap();
    let menu = html
        .split("<details")
        .nth(1)
        .unwrap()
        .split("</details>")
        .next()
        .unwrap();
    assert_eq!(menu.matches(" download").count(), 5);
    for size in ["small", "medium", "large", "xlarge"] {
        assert_eq!(menu.matches(&format!("Download {size} (")).count(), 1);
    }
    for encoding in ["small.jpeg", "medium.jpeg", "large.avif", "xlarge.jxl"] {
        assert!(menu.contains(encoding), "{encoding}");
    }
    page.photo.derivatives.reverse();
    assert_eq!(
        html,
        render_photo(&page, PageSort::Taken, &context()).unwrap()
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn covers_and_counts_never_expose_hidden_photos(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    let hidden = PhotoId(Uuid::from_u128(3));
    photos::set_visibility(&mut conn, &[hidden], Visibility::Restricted)
        .await
        .unwrap();
    albums::set_cover(&mut conn, f.album, Some(hidden))
        .await
        .unwrap();
    let rows = albums::home(&mut conn, &yhotos_core::access::Principal::Anonymous)
        .await
        .unwrap();
    assert_eq!(rows[0].count, 2);
    assert_ne!(rows[0].cover.as_ref().unwrap().id, hidden);
    let mut request = Fixture::request("/a/unknown");
    request
        .headers_mut()
        .insert(header::COOKIE, "ys=invalid".parse().unwrap());
    let response = crate::public_router(f.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(body(response).await, body(f.get("/a/unknown").await).await);
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn query_counts_stay_within_page_budgets(pool: PgPool) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .log_statements(log::LevelFilter::Debug),
        )
        .await
        .unwrap();
    let f = Fixture::new(pool, 3).await;
    let (response, queries) = crate::files::tests::count_queries(f.get("/a/gallery")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(queries, 1);
    let (response, queries) =
        crate::files::tests::count_queries(f.get(&format!("/p/{}", f.photos[0]))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(queries, 1);
}
#[test]
fn grid_is_linear_contiguous_and_fills_each_row() {
    let ratios = vec![1.5; 60];
    let rows = grid::justified_rows(&ratios);
    let mut end = 0;
    for row in rows {
        assert_eq!(row.start, end);
        assert!(row.end > row.start);
        assert!((ratios[row.start..row.end].iter().sum::<f64>() / row.ratio - 1.0).abs() < 1e-10);
        end = row.end;
    }
    assert_eq!(end, 60);
    assert!(grid::justified_rows(&[]).is_empty());
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn rotated_photos_keep_normalized_sources_and_layout(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    photos::update_metadata(
        &mut f.state.pool.acquire().await.unwrap(),
        PhotoId(Uuid::from_u128(1)),
        &photos::PhotoMetadata {
            title: "Rotated photo".into(),
            width: Some(6000),
            height: Some(4000),
            orientation: Some(6),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut row = f.album_row(&SessionIdentity::Anonymous).await;
    assert_eq!(row.photos[0].orientation, Some(6));
    assert_eq!(row.album.cover.as_ref().unwrap().orientation, Some(6));
    let detail = photos::page(
        &mut f.state.pool.acquire().await.unwrap(),
        &yhotos_core::access::Principal::Anonymous,
        &f.photos[0],
        PageSort::Taken,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(detail.photo.orientation, Some(6));
    // A landscape original rotated at decode produces portrait derivatives.
    let photo = &mut row.photos[0];
    photo.width = Some(6000);
    photo.height = Some(4000);
    for derivative in &mut photo.derivatives {
        derivative.h = derivative.w * 3 / 2;
    }
    photo.derivatives.push(photos::PageDerivative {
        variant: "thumbsquare".into(),
        format: "jpeg".into(),
        hdr: false,
        hash: "0".repeat(64),
        w: 320,
        h: 320,
    });
    let photo = photo.clone();
    for orientation in 1..=8 {
        let mut photo = photo.clone();
        photo.orientation = Some(orientation);
        if orientation < 5 {
            std::mem::swap(&mut photo.width, &mut photo.height);
        }
        let mut album = row.clone();
        album.photos = vec![photo.clone()];
        album.album.cover = Some(photo.clone());
        let page = photos::PhotoPage {
            photo,
            chain: vec![],
            previous: None,
            next: None,
        };
        for html in [
            render_photo(&page, PageSort::Taken, &context()).unwrap(),
            render_album(&album, 1, PageSort::Taken, &context()).unwrap(),
            render_home(&[album.album.clone()], &context()).unwrap(),
        ] {
            let picture = html
                .split("<picture>")
                .nth(1)
                .unwrap()
                .split("</picture>")
                .next()
                .unwrap();
            assert!(picture.contains("width=\"4000\" height=\"6000\""));
            assert!(picture.contains("small.jpeg"));
            assert!(picture.contains("640w"));
            assert!(picture.contains("1280w"));
            assert!(picture.contains("2048w"));
            assert!(!picture.contains("thumbsquare"));
            assert!(!picture.contains("data:image/svg"));
        }
        let html = render_album(&album, 1, PageSort::Taken, &context()).unwrap();
        assert!(html.contains("aspect-ratio:4000/6000"));
        // Missing source dimensions use already-normalized derivative dimensions
        // directly, without applying the source orientation a second time.
        let mut page = page;
        page.photo.width = None;
        page.photo.height = None;
        let html = render_photo(&page, PageSort::Taken, &context()).unwrap();
        assert!(html.contains("width=\"2048\" height=\"3072\""));
        assert!(html.contains("640w"));
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn open_graph_prefers_large_jpeg_and_falls_back_to_largest_sdr(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    let mut album = f.album_row(&SessionIdentity::Anonymous).await;
    let mut photo = album.photos[0].clone();
    let mut extra = photo
        .derivatives
        .iter()
        .find(|d| d.variant == "large" && d.format == "jpeg")
        .unwrap()
        .clone();
    extra.variant = "xlarge".into();
    extra.w = 3200;
    extra.h = 2133;
    photo.derivatives.push(extra);
    for expected in ["large", "xlarge", "medium"] {
        let selected = photo
            .derivatives
            .iter()
            .find(|d| d.variant == expected && d.format == "jpeg")
            .unwrap()
            .clone();
        album.album.cover = Some(photo.clone());
        let page = photos::PhotoPage {
            photo: photo.clone(),
            chain: vec![],
            previous: None,
            next: None,
        };
        for html in [
            render_album(&album, 1, PageSort::Taken, &context()).unwrap(),
            render_photo(&page, PageSort::Taken, &context()).unwrap(),
        ] {
            assert!(html.contains(&format!(
                "property=\"og:image\" content=\"https://gallery.example/i/{}/{}.jpeg\"",
                selected.hash, selected.variant
            )));
            assert!(html.contains(&format!(
                "property=\"og:image:width\" content=\"{}\"",
                selected.w
            )));
            assert!(html.contains(&format!(
                "property=\"og:image:height\" content=\"{}\"",
                selected.h
            )));
        }
        // Retain modern large representations, matching the earlier default plan.
        photo
            .derivatives
            .retain(|d| d.variant != expected || d.format != "jpeg");
    }
    let mut hdr = photo.derivatives[0].clone();
    hdr.variant = "large".into();
    hdr.format = "jpeg".into();
    hdr.hdr = true;
    photo.derivatives.push(hdr);
    let mut invalid = photo
        .derivatives
        .iter()
        .find(|d| d.format == "jpeg" && !d.hdr)
        .unwrap()
        .clone();
    invalid.variant = "large".into();
    invalid.hash = "invalid".into();
    photo.derivatives.push(invalid);
    album.album.cover = Some(photo);
    let html = render_album(&album, 1, PageSort::Taken, &context()).unwrap();
    assert!(html.contains("/small.jpeg\""));
    assert!(!html.contains("property=\"og:image\" content=\"https://gallery.example/i/invalid"));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn mobile_grid_and_cover_sources_are_capped_across_formats(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    let mut album = f.album_row(&SessionIdentity::Anonymous).await;
    let photo = &mut album.photos[0];
    for mut derivative in photo.derivatives.clone() {
        if derivative.format != "jpeg" {
            derivative.hdr = true;
            photo.derivatives.push(derivative);
        }
    }
    album.album.cover = Some(photo.clone());
    album.children = vec![album.album.clone()];
    for html in [
        render_album(&album, 1, PageSort::Taken, &context()).unwrap(),
        render_home(&album.children, &context()).unwrap(),
    ] {
        for picture in html
            .split("<picture>")
            .skip(1)
            .map(|s| s.split("</picture>").next().unwrap())
        {
            let mut mobile_count = 0;
            for source in picture
                .split("<source ")
                .skip(1)
                .map(|s| s.split('>').next().unwrap())
            {
                if source.contains("media=\"(max-width: 600px)") {
                    mobile_count += 1;
                    assert!(source.contains("640w"));
                    assert!(!source.contains("1280w"));
                    assert!(!source.contains("2048w"));
                    assert!(source.contains("sizes=\""));
                }
            }
            assert_eq!(mobile_count, 5);
            assert!(picture.contains("1280w"));
            assert!(picture.contains("2048w"));
        }
    }
}
