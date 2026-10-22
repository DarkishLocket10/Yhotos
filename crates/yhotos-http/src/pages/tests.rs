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
use std::{io::Write, net::SocketAddr, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_auth::{Token, TokenKind};
use yhotos_core::{
    AlbumId, PhotoId, UserId,
    access::{Grant, ShareTarget, Visibility},
};
use yhotos_db::{DatabaseTls, Db, Role, grants, sessions, users};

struct Fixture {
    state: AppState,
    album: AlbumId,
    photos: Vec<ShortId>,
    user: UserId,
    cookie: String,
}
impl Fixture {
    async fn new(pool: PgPool, count: usize) -> Self {
        let initial = crate::tests::state(false, 100_000);
        let url = pool.connect_options().to_url_lossy().to_string().into();
        let db = Arc::new(
            Db::connect(&url, Role::Public, 4, DatabaseTls::Disable)
                .await
                .unwrap(),
        );
        let auth = crate::auth::Auth::new_for_tests(initial.config, db, None)
            .await
            .unwrap();
        let state = AppState::new(initial.config, pool, initial.store.clone(), Some(auth));
        let mut conn = state.pool.acquire().await.unwrap();
        let album = AlbumId(Uuid::from_u128(100));
        let password = yhotos_auth::generate_temp_password();
        let hash = yhotos_auth::PasswordHasher::for_tests()
            .hash_password(&password)
            .unwrap();
        let user = users::create(
            &mut conn,
            &Uuid::now_v7().to_string(),
            &hash.as_str().into(),
            false,
        )
        .await
        .unwrap()
        .id;
        let token = Token::generate();
        let now = time::OffsetDateTime::now_utc();
        sessions::create(
            &mut conn,
            &sessions::Session {
                id: Uuid::now_v7(),
                user_id: user,
                admin: false,
                created_at: now,
                last_seen_at: now,
                expires_at: now + time::Duration::days(1),
                reauth_at: Some(now),
                mfa_at: None,
                ip_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
                ua_class: "other".into(),
                token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();
        let cookie = format!("__Host-ys={}", token.encode(TokenKind::Session));
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
            cookie,
        }
    }
    fn request(path: &str) -> Request<Body> {
        let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1122".parse::<SocketAddr>().unwrap()));
        req
    }
    async fn get(&self, path: &str) -> Response {
        self.send(path, false).await
    }
    async fn get_as_user(&self, path: &str) -> Response {
        self.send(path, true).await
    }
    async fn send(&self, path: &str, authenticated: bool) -> Response {
        let mut request = Self::request(path);
        if authenticated {
            request
                .headers_mut()
                .insert(header::COOKIE, self.cookie.parse().unwrap());
        }
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
        snapshot_html(render_album(&anonymous, 1, PageSort::Taken, &context()).unwrap())
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
        snapshot_html(render_album(&granted, 1, PageSort::Taken, &context()).unwrap())
    );
    let response = f.get_as_user("/a/gallery").await;
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
    assert_eq!(
        page.split("</section>")
            .next()
            .unwrap()
            .matches("<img ")
            .count(),
        60
    );
    assert!(page.contains("Next page"));
    let second = body(f.get("/a/gallery?page=2&sort=manual").await).await;
    assert_eq!(
        second
            .split("</section>")
            .next()
            .unwrap()
            .matches("<img ")
            .count(),
        1
    );
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
    // Hover prefetch is an island (task 013); `<link rel="prefetch">` is never emitted.
    assert!(!html.contains("rel=\"prefetch\""));
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
async fn hdr_jpeg_only_representation_precedes_sdr_sources(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    let mut album = f.album_row(&SessionIdentity::Anonymous).await;
    let photo = &mut album.photos[0];
    // JPEG is the only HDR encoding; SDR modern formats must not shadow it.
    for mut derivative in photo.derivatives.clone() {
        if derivative.format == "jpeg" {
            derivative.hdr = true;
            photo.derivatives.push(derivative);
        }
    }
    let photo = photos::PhotoPage {
        photo: photo.clone(),
        chain: album.chain.clone(),
        previous: None,
        next: None,
    };
    album.album.cover = Some(photo.photo.clone());
    let detail = render_photo(&photo, PageSort::Taken, &context()).unwrap();
    let grid = render_album(&album, 1, PageSort::Taken, &context()).unwrap();
    let home = render_home(&[album.album.clone()], &context()).unwrap();
    for (html, capped) in [(&detail, false), (&grid, true), (&home, true)] {
        let picture = html.split("<picture>").nth(1).unwrap();
        let picture = picture.split("</picture>").next().unwrap();
        let hdr = picture
            .split("<source ")
            .skip(1)
            .map(|s| s.split('>').next().unwrap())
            .find(|s| s.contains("media=\"(dynamic-range: high)\" type=\"image/jpeg\""))
            .expect("full HDR JPEG source");
        assert!(hdr.contains(".jpeg.hdr 640w"));
        assert!(hdr.contains(".jpeg.hdr 1280w"));
        assert!(hdr.contains(".jpeg.hdr 2048w"));
        assert!(picture.find(hdr).unwrap() < picture.find("<source type=\"image/jxl\"").unwrap());
        if capped {
            let mobile = picture
                .split("<source ")
                .skip(1)
                .map(|s| s.split('>').next().unwrap())
                .find(|s| s.contains("media=\"(max-width: 600px) and (dynamic-range: high)\" type=\"image/jpeg\""))
                .expect("mobile HDR JPEG source");
            assert!(mobile.contains(".jpeg.hdr 640w"));
            assert!(!mobile.contains("1280w"));
            assert!(!mobile.contains("2048w"));
            assert!(
                picture.find(mobile).unwrap()
                    < picture
                        .find("media=\"(max-width: 600px)\" type=\"image/jxl\"")
                        .unwrap()
            );
        }
        let fallback = picture.split("<img ").nth(1).unwrap();
        assert!(!fallback.contains(".hdr"));
    }
    insta::assert_snapshot!("photo_hdr_jpeg", snapshot_html(detail));
    insta::assert_snapshot!("album_hdr_jpeg", snapshot_html(grid));
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
async fn legacy_exact_path_then_slug_then_numeric_id(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    for (pattern, index) in [("/category/12", 0), ("/garden", 1)] {
        redirects::insert(
            &mut conn,
            pattern,
            ShareTarget::Photo(PhotoId(Uuid::from_u128(index + 1))),
        )
        .await
        .unwrap();
    }
    for (url, index) in [
        ("/index.php?/category/12-garden", 1),
        ("/index.php?/category/12-unknown", 0),
        ("/index.php?/category/12", 0),
    ] {
        let response = f.get(url).await;
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("/p/{}", f.photos[index])
        );
    }
    // Both exact representations retain priority over the conflicting slug/ID.
    redirects::insert(
        &mut conn,
        "/category/12-garden",
        ShareTarget::Album(f.album),
    )
    .await
    .unwrap();
    let response = f.get("/index.php?/category/12-garden").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()[header::LOCATION], "/a/gallery");
    redirects::insert(
        &mut conn,
        "/index.php?/category/12-garden",
        ShareTarget::Photo(PhotoId(Uuid::from_u128(3))),
    )
    .await
    .unwrap();
    let response = f.get("/index.php?/category/12-garden").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/p/{}", f.photos[2])
    );
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
    let allowed = f.get_as_user(photo_url).await;
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
        for authenticated in [false, true] {
            let denied = f.send(url, authenticated).await;
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
        let allowed = f.get_as_user(url).await;
        assert_eq!(allowed.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(allowed.headers()[header::LOCATION], destination);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn malformed_derivative_selectors_cannot_displace_valid_sources(pool: PgPool) {
    let f = Fixture::new(pool, 1).await;
    let mut album = f.album_row(&SessionIdentity::Anonymous).await;
    let photo = &mut album.photos[0];
    for mut derivative in photo.derivatives.clone() {
        if derivative.format != "jpeg" {
            derivative.hdr = true;
            photo.derivatives.push(derivative);
        }
    }
    // Exercise dimension inference as well as photos with original dimensions.
    for infer_dimensions in [false, true] {
        if infer_dimensions {
            album.photos[0].width = None;
            album.photos[0].height = None;
        }
        let render = |mut album: albums::AlbumPage| {
            album.album.cover = Some(album.photos[0].clone());
            album.children = vec![album.album.clone()];
            let photo = photos::PhotoPage {
                photo: album.photos[0].clone(),
                chain: vec![],
                previous: None,
                next: None,
            };
            [
                render_home(&album.children, &context()).unwrap(),
                render_album(&album, 1, PageSort::Taken, &context()).unwrap(),
                render_photo(&photo, PageSort::Taken, &context()).unwrap(),
            ]
        };
        let expected = render(album.clone());
        for (variant, format) in [
            ("a-b", None),
            ("", None),
            ("Small", None),
            ("a_b", None),
            ("a/b", None),
            ("é", None),
            ("abcdefghijklmnopq", None),
            ("small", Some("JPEG")),
            ("small", Some("png")),
        ] {
            let mut malformed = album.clone();
            for mut derivative in album.photos[0].derivatives.clone() {
                derivative.variant = variant.into();
                if let Some(format) = format {
                    derivative.format = format.into();
                }
                // Same-width malformed variants sort before the valid sources.
                derivative.hash = "0".repeat(64);
                malformed.photos[0].derivatives.push(derivative);
            }
            assert_eq!(render(malformed.clone()), expected, "{variant}/{format:?}");
            malformed.photos[0].derivatives.reverse();
            assert_eq!(
                render(malformed),
                expected,
                "{variant}/{format:?}, reversed"
            );
        }
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
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Query city','CA',43.65,-79.38)",
        place
    )
    .execute(&f.state.pool)
    .await
    .unwrap();
    sqlx::query!("UPDATE photos SET place_id=$1", place)
        .execute(&f.state.pool)
        .await
        .unwrap();
    for path in [
        format!("/places/{place}"),
        "/map/photos.json?lon=-79.38&lat=43.65".into(),
    ] {
        let (response, queries) = crate::files::tests::count_queries(f.get(&path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(queries, 1, "{path}");
    }
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
        derivative.hdr = true;
        photo.derivatives.push(derivative);
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
            assert_eq!(mobile_count, 6);
            assert!(picture.contains("1280w"));
            assert!(picture.contains("2048w"));
        }
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn calendar_snapshots_dates_and_one_query_per_level(pool: PgPool) {
    use yhotos_db::calendar::{self, Selection};
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
    let mut conn = f.state.pool.acquire().await.unwrap();
    sqlx::query!(
        "UPDATE photos SET visibility='restricted' WHERE id=$1",
        Uuid::from_u128(3)
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE photos SET taken_at=NULL WHERE id=$1",
        Uuid::from_u128(2)
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    for (path, name) in [
        ("/calendar", "calendar_years"),
        ("/calendar/2026", "calendar_months"),
        ("/calendar/2026/1", "calendar_days"),
        ("/calendar/2026/5", "calendar_posted"),
        ("/calendar/2026/1/1", "calendar_day"),
    ] {
        let (response, queries) = crate::files::tests::count_queries(f.get(path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(queries, 1, "{path}");
        assert_geo_cache(&response, "calendar,visibility");
        let selection = super::calendar::selection(path).unwrap();
        let page = calendar::page(
            &mut f.state.pool.acquire().await.unwrap(),
            &yhotos_core::access::Principal::Anonymous,
            selection,
            1,
        )
        .await
        .unwrap()
        .unwrap();
        let html = super::render_calendar(&page, selection, 1, &context()).unwrap();
        assert!(!html.contains("Photo 01"));
        insta::assert_snapshot!(name, html);
    }
    for path in [
        "/calendar/0",
        "/calendar/10000",
        "/calendar/2026/13",
        "/calendar/2026/2/29",
        "/calendar/2026/1/32",
        "/calendar/x",
        "/calendar/2026/1?page=2",
        "/calendar?page=2",
    ] {
        assert_eq!(f.get(path).await.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        f.get("/calendar/2024/2/29").await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(Selection::new(Some(2024), Some(2), Some(29)).is_some());
}

fn assert_geo_cache(response: &axum::response::Response, tags: &str) {
    assert_geo_cache_policy(
        response,
        tags,
        "public, max-age=60, stale-while-revalidate=600",
    );
}

fn assert_map_json_cache(response: &axum::response::Response) {
    assert_geo_cache_policy(response, "map,visibility", "public, max-age=300");
}

fn assert_geo_cache_policy(response: &axum::response::Response, tags: &str, policy: &str) {
    assert_eq!(response.headers()[header::CACHE_CONTROL], policy);
    assert_eq!(response.headers()[header::VARY], "Cookie");
    assert_eq!(response.headers()["cache-tag"], tags);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn empty_aggregate_pages_keep_tags_and_calendar_sessions_are_private(pool: PgPool) {
    let f = Fixture::new(pool, 0).await;
    for (path, tags) in [
        ("/map", "map,visibility"),
        (
            "/map/tiles.json?bbox=-80,43,-79,44&zoom=1",
            "map,visibility",
        ),
        ("/calendar", "calendar,visibility"),
    ] {
        let response = f.get(path).await;
        assert_eq!(response.status(), StatusCode::OK);
        if path.starts_with("/map/tiles.json") {
            assert_map_json_cache(&response);
        } else {
            assert_geo_cache(&response, tags);
        }
        let response = f.get_as_user(path).await;
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        assert_eq!(response.headers()[header::VARY], "Cookie");
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn map_fallback_csp_json_and_private_cache(pool: PgPool) {
    let f = Fixture::new(pool, 3).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    let place = Uuid::from_u128(300);
    sqlx::query!("INSERT INTO places(id,name,admin1,country,lat,lon) VALUES($1,'Toronto','Ontario','CA',43.65,-79.38)",place).execute(&mut *conn).await.unwrap();
    sqlx::query!("UPDATE photos SET place_id=$1", place)
        .execute(&mut *conn)
        .await
        .unwrap();
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
    let response = f.get("/map").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_geo_cache(&response, "map,visibility");
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("connect-src 'self' https://tiles.openfreemap.org"));
    assert!(csp.contains("worker-src 'self'"));
    assert!(!csp.contains("blob:"));
    assert!(!csp.contains("unsafe-eval"));
    let html = body(response).await;
    // The worker URL is keyed on the tile origin and still receives the map CSP.
    let worker = html
        .split("data-worker=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&#38;", "&");
    assert!(worker.contains("?tiles="), "{worker}");
    let response = f.get(&worker).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("connect-src 'self' https://tiles.openfreemap.org")
    );
    let href = html
        .split("<li><a href=\"/places/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let href = format!("/places/{href}");
    let response = f.get(&href).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_geo_cache(&response, &format!("places,place:{place},visibility"));
    let place_html = body(response).await;
    assert!(place_html.contains("href=\"/map\">Photo map</a>"));
    assert!(place_html.contains("Toronto, Ontario, CA"));
    assert_eq!(place_html.matches("class=\"cell\"").count(), 2);
    assert!(place_html.contains(&format!("/p/{}?", f.photos[0])));
    assert!(!place_html.contains(&format!("/p/{}?", f.photos[2])));
    let photo_href = place_html
        .split("href=\"/p/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(
        f.get(&format!("/p/{photo_href}")).await.status(),
        StatusCode::OK
    );
    let response = f.get_as_user(&href).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(body(response).await.matches("class=\"cell\"").count(), 3);
    assert!(html.contains("2 photos"));
    assert!(!html.contains("3 photos"));
    assert!(html.contains("data-tiles="));
    let path = "/map/tiles.json?bbox=-80,43,-79,44&zoom=1";
    let response = f.get(path).await;
    assert_map_json_cache(&response);
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["features"][0]["properties"]["count"], 2);
    let response = f.get_as_user(path).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["features"][0]["properties"]["count"], 3);
    let response = f.get("/map/tiles.json?bbox=-80,43,-79,44&zoom=15").await;
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["features"].as_array().unwrap().len(), 1);
    assert_eq!(json["features"][0]["properties"]["count"], 2);
    assert_eq!(json["features"][0]["properties"]["terminal"], true);
    let point = &json["features"][0]["geometry"]["coordinates"];
    let chooser = format!("/map/photos.json?lon={}&lat={}", point[0], point[1]);
    let response = f.get(&chooser).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_map_json_cache(&response);
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["total"], 2);
    assert_eq!(json["photos"].as_array().unwrap().len(), 2);
    assert!(
        !json["photos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["short_id"] == f.photos[2].to_string())
    );
    let response = f.get_as_user(&chooser).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["total"], 3);
    assert!(!body(f.get("/").await).await.contains("/static/map."));
    assert!(
        !f.get("/").await.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("openfreemap")
    );
    for path in [
        "/map/tiles.json?bbox=NaN,0,1,1&zoom=1",
        "/map/tiles.json?bbox=0,0,1,1&zoom=21",
        "/map/tiles.json?bbox=-181,-90,181,90&zoom=0",
    ] {
        assert_eq!(f.get(path).await.status(), StatusCode::BAD_REQUEST);
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn calendar_month_grid_paginates_after_visibility_and_stays_under_budget(pool: PgPool) {
    let f = Fixture::new(pool, 62).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    sqlx::query!("UPDATE photos SET taken_at='2026-01-01 00:00:00+00'")
        .execute(&mut *conn)
        .await
        .unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(62))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    drop(conn);
    let first = body(f.get("/calendar/2026/1").await).await;
    // One day cover plus sixty grid photos, with complete visible counts.
    assert_eq!(first.matches("<img ").count(), 61);
    assert!(first.contains("61 photos"));
    assert!(first.contains("Next page"));
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(first.as_bytes()).unwrap();
    assert!(compressed.finish().unwrap().len() < 30 * 1024);
    let second = body(f.get("/calendar/2026/1?page=2").await).await;
    assert_eq!(second.matches("<img ").count(), 2);
    assert!(second.contains("Previous page"));
    assert!(!second.contains("Next page"));
    assert_eq!(
        f.get("/calendar/2026/1?page=3").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn place_and_point_pages_paginate_visible_photos_and_hide_empty_places(pool: PgPool) {
    let f = Fixture::new(pool, 62).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    let place = Uuid::now_v7();
    let hidden = Uuid::now_v7();
    sqlx::query!("INSERT INTO places(id,name,country,lat,lon) VALUES($1,'City <script>','CA',43.65,-79.38),($2,'Secret place','CA',43.65,-79.38)",place,hidden).execute(&mut *conn).await.unwrap();
    sqlx::query!("UPDATE photos SET place_id=$1", place)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE photos SET visibility='restricted',place_id=$1 WHERE short_id=$2",
        hidden,
        f.photos[61].to_string()
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let href = format!("/places/{place}");
    let html = body(f.get(&href).await).await;
    assert_eq!(html.matches("class=\"cell\"").count(), 60);
    assert!(html.contains("61 photos"));
    assert!(html.contains("City &#60;script&#62;"));
    assert!(!html.contains("City <script>"));
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(html.as_bytes()).unwrap();
    assert!(compressed.finish().unwrap().len() < 30 * 1024);
    assert!(html.contains(&format!("href=\"{href}?page=2\"")));
    let second = body(f.get(&format!("{href}?page=2")).await).await;
    assert_eq!(second.matches("class=\"cell\"").count(), 1);
    assert!(second.contains(&format!("href=\"{href}?page=1\"")));
    let point = "/map/photos.json?lon=-79.38&lat=43.65";
    let response = f.get(point).await;
    assert_eq!(response.status(), StatusCode::OK);
    let first: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    let second: serde_json::Value =
        serde_json::from_str(&body(f.get(&format!("{point}&page=2")).await).await).unwrap();
    assert_eq!(first["total"], 61);
    assert_eq!(first["photos"].as_array().unwrap().len(), 60);
    assert_eq!(second["photos"].as_array().unwrap().len(), 1);
    assert!(
        !first["photos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == &second["photos"][0])
    );
    for path in [
        format!("{href}?page=3"),
        format!("{href}?page=0"),
        format!("/places/{hidden}"),
        format!("/places/{}", Uuid::now_v7()),
        "/places/invalid".into(),
        format!("{point}&page=3"),
        format!("{point}&page=0"),
        "/map/photos.json?lon=0&lat=0".into(),
    ] {
        let response = f.get(&path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body(response).await, body(f.get("/missing").await).await);
    }
    let fallback = body(f.get("/map").await).await;
    assert!(!fallback.contains(&hidden.to_string()));
    for path in [
        "/map/photos.json?lon=NaN&lat=0",
        "/map/photos.json?lon=0&lat=inf",
        "/map/photos.json?lon=181&lat=0",
        "/map/photos.json?lon=0&lat=90",
    ] {
        assert_eq!(
            f.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn api_visibility_pagination_and_private_state(pool: PgPool) {
    let f = Fixture::new(pool, 61).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(1))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    drop(conn);
    let restricted = f.get(&format!("/api/v1/photos/{}", f.photos[0])).await;
    let missing = f
        .get(&format!(
            "/api/v1/photos/{}",
            ShortId::from_photo_id(PhotoId::new())
        ))
        .await;
    assert_eq!(restricted.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(restricted).await, body(missing).await);
    let album = format!("/api/v1/albums/{}/photos", f.album);
    let response = f.get(&album).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=60"
    );
    let data: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(data["photos"].as_array().unwrap().len(), 60);
    assert_eq!(data["next_page"], serde_json::Value::Null);
    assert_eq!(
        f.get(&format!("{album}?page=2")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        data["photos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["caps"]["like"] == false)
    );
    let response = f
        .get_as_user(&format!("/api/v1/photos/{}", f.photos[1]))
        .await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let data: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(data["state"]["liked"], false);
    assert!(data["state"].get("like_count").is_none());
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn api_like_unlike_and_concurrent_flags_are_idempotent(pool: PgPool) {
    let f = Fixture::new(pool, 2).await;
    let app = crate::public_router(f.state.clone());
    let action = |method: &str, action: &str| {
        let mut request = Fixture::request(&format!("/api/v1/photos/{}/{action}", f.photos[0]));
        *request.method_mut() = method.parse().unwrap();
        request
            .headers_mut()
            .insert("x-requested-with", "yhotos".parse().unwrap());
        request
            .headers_mut()
            .insert("sec-fetch-site", "same-origin".parse().unwrap());
        request
            .headers_mut()
            .insert(header::COOKIE, f.cookie.parse().unwrap());
        request
    };
    for (method, liked) in [
        ("POST", true),
        ("POST", true),
        ("DELETE", false),
        ("DELETE", false),
    ] {
        let response = app.clone().oneshot(action(method, "like")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(data["liked"], liked);
        assert!(data.get("like_count").is_none());
    }
    let (first, second) = tokio::join!(
        app.clone().oneshot(action("POST", "flag")),
        app.clone().oneshot(action("POST", "flag"))
    );
    for response in [first.unwrap(), second.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        let data: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(data["flagged"], true);
    }
    let flags = yhotos_db::flags::open_flags(&mut f.state.pool.acquire().await.unwrap(), 0)
        .await
        .unwrap();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].user_id, Some(f.user));
    assert_eq!(
        f.get_as_user(&format!("/p/{}", f.photos[1])).await.status(),
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn map_place_list_paginates_after_visibility_and_stays_under_budget(pool: PgPool) {
    let f = Fixture::new(pool, 410).await;
    let mut conn = f.state.pool.acquire().await.unwrap();
    // One distinct place per photo; the restricted photo's place must not be listed or counted.
    sqlx::query!("INSERT INTO places(id,name,admin1,country,lat,lon) SELECT id,'Place '||lpad(sort_order::text,4,'0'),'ON','CA',43.65,-79.38 FROM photos")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET place_id=id")
        .execute(&mut *conn)
        .await
        .unwrap();
    photos::set_visibility(
        &mut conn,
        &[PhotoId(Uuid::from_u128(410))],
        Visibility::Restricted,
    )
    .await
    .unwrap();
    drop(conn);
    let first = body(f.get("/map").await).await;
    assert_eq!(first.matches("<li><a href=\"/places/").count(), 200);
    assert!(first.contains("rel=\"next\" href=\"/map?page=2\""));
    assert!(!first.contains("rel=\"prev\""));
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(first.as_bytes()).unwrap();
    assert!(compressed.finish().unwrap().len() < 30 * 1024);
    let second = body(f.get("/map?page=2").await).await;
    assert_eq!(second.matches("<li><a href=\"/places/").count(), 200);
    let third = body(f.get("/map?page=3").await).await;
    // 410 photos, one restricted: 409 visible places.
    assert_eq!(third.matches("<li><a href=\"/places/").count(), 9);
    assert!(third.contains("rel=\"prev\" href=\"/map?page=2\""));
    assert!(!third.contains("rel=\"next\""));
    assert!(!third.contains(&Uuid::from_u128(410).to_string()));
    for path in ["/map?page=4", "/map?page=0", "/map?page=x"] {
        assert_eq!(f.get(path).await.status(), StatusCode::NOT_FOUND, "{path}");
    }
}
