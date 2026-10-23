#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]
use super::*;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sqlx::{ConnectOptions, PgPool};
use tower::ServiceExt;
use yhotos_core::{AlbumId, access::Principal};
use yhotos_db::{DatabaseTls, Db, Role};

pub(super) fn candidate(id: u128) -> Candidate {
    let photo = PhotoId(uuid::Uuid::from_u128(id));
    serde_json::from_value(serde_json::json!({"text":"<script>evil</script>","photo":{
        "id":photo,"album_id":AlbumId::new(),"short_id":ShortId::from_photo_id(photo),
        "title":"sunset","description":"","original_hash":"a".repeat(64),"original_name":"sunset.jpg","kind":"image",
        "width":64,"height":48,"orientation":1,"dominant_colors":[],"thumbhash":null,"derivatives":[],
        "camera_make":null,"camera_model":null,"lens":null,"focal_mm":null,"aperture":null,"shutter":null,"iso":null,"gps_lat":null,"gps_lon":null
    }})).unwrap()
}
#[test]
fn reciprocal_rank_order_and_duplicates() {
    let rows = fuse([
        vec![candidate(1), candidate(2), candidate(2)],
        vec![candidate(2), candidate(3)],
    ]);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].candidate.photo.id,
        PhotoId(uuid::Uuid::from_u128(2))
    );
    assert!((rows[0].score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
}
async fn fixture(pool: &PgPool) -> AppState {
    let initial = crate::tests::state(false, 10_000);
    let url = pool.connect_options().to_url_lossy().to_string().into();
    let db = Db::connect(&url, Role::Public, 8, DatabaseTls::Disable)
        .await
        .unwrap();
    let album = AlbumId::new();
    sqlx::query!(
        "INSERT INTO albums(id,slug,title) VALUES($1,'search-fixture','Search')",
        album.0
    )
    .execute(pool)
    .await
    .unwrap();
    for (index, visibility) in [(1, "public"), (2, "restricted")] {
        let photo = PhotoId(uuid::Uuid::from_u128(index));
        let short = ShortId::from_photo_id(photo).to_string();
        sqlx::query!("INSERT INTO photos(id,album_id,short_id,title,original_hash,original_name,original_bytes,mime,kind,status,visibility,camera_model) VALUES($1,$2,$3,'sunset','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','photo.jpg',1,'image/jpeg','image','ready',$4::text::visibility,'Test camera')",photo.0,album.0,short,visibility).execute(pool).await.unwrap();
        sqlx::query!("INSERT INTO ocr_text(id,photo_id,text,model) VALUES($1,$2,'sunset <script>alert(1)</script>','fixture')",uuid::Uuid::now_v7(),photo.0).execute(pool).await.unwrap();
        let vector = format!("[1,{}]", vec!["0"; 767].join(","));
        sqlx::query!(
            "INSERT INTO embeddings(photo_id,model,vector) VALUES($1,$3,$2::text::vector)",
            photo.0,
            vector,
            search::EMBED_MODEL
        )
        .execute(pool)
        .await
        .unwrap();
    }
    AppState::new(
        initial.config,
        db.pool().clone(),
        initial.store.clone(),
        None,
    )
}
fn request(path: &str) -> Request<Body> {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
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
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn all_lists_apply_visibility_and_filters(pool: PgPool) {
    let state = fixture(&pool).await;
    let p = access_sql::visible_photos(&Principal::Anonymous);
    let vector = format!("[1,{}]", vec!["0"; 767].join(","));
    for list in [List::Semantic, List::Text, List::Structured] {
        let text = if matches!(list, List::Structured) {
            ""
        } else {
            "sunset"
        };
        let rows = search::candidates(&state.pool, &p, &[], text, Some(&vector), None, list)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].photo.short_id,
            ShortId::from_photo_id(PhotoId(uuid::Uuid::from_u128(1)))
        );
        let filters = [search::Filter {
            key: "camera".into(),
            value: "missing camera".into(),
        }];
        assert!(
            search::candidates(&state.pool, &p, &filters, text, Some(&vector), None, list)
                .await
                .unwrap()
                .is_empty()
        );
    }
    // A restricted seed embedding cannot be used to probe neighbours.
    let private = ShortId::from_photo_id(PhotoId(uuid::Uuid::from_u128(2))).to_string();
    assert!(
        search::candidates(
            &state.pool,
            &p,
            &[],
            "",
            None,
            Some(&private),
            List::Semantic
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        sqlx::query!("SELECT has_table_privilege('yhotos_public','faces','SELECT') AS allowed")
            .fetch_one(&pool)
            .await
            .unwrap()
            .allowed,
        Some(false)
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn unavailable_ml_and_ocr_escaping_end_to_end(pool: PgPool) {
    let state = fixture(&pool).await;
    let app = crate::public_router(state);
    for path in [
        "/search?q=sunset",
        "/api/v1/search?q=sunset",
        "/search?q=camera:%22Test%20camera%22",
    ] {
        let response = app.clone().oneshot(request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let html = body(response).await;
        assert!(!html.contains("<script>alert(1)</script>"));
        if path.starts_with("/api") {
            assert!(html.contains("\\u003cscript\\u003e"));
            let json: serde_json::Value = serde_json::from_str(&html).unwrap();
            assert_eq!(json["photos"].as_array().unwrap().len(), 1);
        } else {
            assert!((html.contains("&lt;script&gt;") || html.contains("&#60;script&#62;")));
        }
    }
    let response = app
        .oneshot(request("/api/v1/search?q=person:Yash"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body(response).await).unwrap()["total"],
        0
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn suggest_counts_are_visible_and_rate_limited(pool: PgPool) {
    let state = fixture(&pool).await;
    let p = access_sql::visible_photos(&Principal::Anonymous);
    let suggestions = search::suggest(&state.pool, &p, "Test").await.unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].count, 1);
    let initial = crate::tests::state(false, 1);
    let state = AppState::new(
        initial.config,
        state.pool.clone(),
        state.store.clone(),
        None,
    );
    let app = crate::public_router(state);
    assert_eq!(
        app.clone()
            .oneshot(request("/api/v1/suggest?q=Test"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.oneshot(request("/api/v1/suggest?q=Test"))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn search_and_gallery_apis_share_routes_and_preserve_cache_policies(pool: PgPool) {
    let state = fixture(&pool).await;
    let app = crate::public_router(state);
    let photo = ShortId::from_photo_id(PhotoId(uuid::Uuid::from_u128(1)));
    for (path, policy) in [
        (format!("/api/v1/photos/{photo}"), "public, max-age=60"),
        ("/api/v1/search?q=sunset".into(), "private, no-store"),
        ("/api/v1/suggest?q=Test".into(), "private, no-store"),
    ] {
        let response = app.clone().oneshot(request(&path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], policy, "{path}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }
}
#[tokio::test]
async fn embedding_client_validates_and_caches() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(listener.local_addr().unwrap().to_string());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        assert!(
            BufReader::new(&mut stream)
                .read_until(b'}', &mut buffer)
                .await
                .unwrap()
                > 0
        );
        let body =
            serde_json::json!({"vector":([vec![1_f32],vec![0_f32;767]].concat())}).to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    assert!(client.embed("sunset").await.is_some());
    server.await.unwrap();
    assert!(client.embed("sunset").await.is_some());
    assert!(client.embed("other").await.is_none());
}

#[test]
fn analytics_redacts_identifying_text() {
    assert_eq!(
        analytics_query("sunset alice@example.test person:yash"),
        "sunset [redacted] [redacted]"
    );
}

#[cfg(feature = "admin")]
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn hidden_people_never_appear_in_admin_search(pool: PgPool) {
    let _state = fixture(&pool).await;
    let id = uuid::Uuid::now_v7();
    let vector = format!("[1,{}]", vec!["0"; 127].join(","));
    sqlx::query!(
        "INSERT INTO persons(id,name) VALUES($1,'Fixture person')",
        id
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("INSERT INTO faces(id,photo_id,bbox,embedding,person_id,quality,model) VALUES($1,$2,ARRAY[0.1,0.1,0.5,0.5],$3::text::vector,$4,0.9,'fixture')",uuid::Uuid::now_v7(),uuid::Uuid::from_u128(1),vector,id).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        yhotos_db::faces::named_photos(&mut conn, "Fixture person", 1)
            .await
            .unwrap()
            .photos
            .len(),
        1
    );
    assert!(
        yhotos_db::faces::update(&mut conn, id, "Fixture person", true)
            .await
            .unwrap()
    );
    assert!(
        yhotos_db::faces::named_photos(&mut conn, "Fixture person", 1)
            .await
            .unwrap()
            .photos
            .is_empty()
    );
    assert!(
        yhotos_db::faces::people(&mut conn, 1, None)
            .await
            .unwrap()
            .people
            .is_empty()
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn semantic_scan_continues_past_excluded_nearest_neighbors(pool: PgPool) {
    let state = fixture(&pool).await;
    let album = AlbumId::new();
    sqlx::query!(
        "INSERT INTO albums(id,slug,title) VALUES($1,'excluded','Excluded')",
        album.0
    )
    .execute(&pool)
    .await
    .unwrap();
    // More excluded nearest neighbors than ef_search. Half are restricted and
    // half visible but fail the camera filter. Eligible neighbors are farther.
    let ids: Vec<_> = (0..600).map(|_| PhotoId::new()).collect();
    let short: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,status,visibility,camera_model) SELECT id,short_id,$3,'excluded',repeat('b',64),'fixture.jpg',1,'image/jpeg','image','ready',CASE WHEN n<=300 THEN 'restricted'::visibility ELSE 'public'::visibility END,CASE WHEN n<=300 THEN 'Test camera' ELSE 'other' END FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY AS f(id,short_id,n)",&ids,&short,album.0).execute(&pool).await.unwrap();
    let near = format!("[1,{}]", vec!["0"; 767].join(","));
    sqlx::query!("INSERT INTO embeddings(photo_id,model,vector) SELECT id,$2,$3::text::vector FROM unnest($1::uuid[]) id",&ids,search::EMBED_MODEL,near).execute(&pool).await.unwrap();
    let eligible = format!("[0.8,0.6,{}]", vec!["0"; 766].join(","));
    sqlx::query!(
        "UPDATE embeddings SET vector=$1::text::vector WHERE photo_id=$2",
        eligible,
        uuid::Uuid::from_u128(1)
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("ANALYZE photos,embeddings")
        .execute(&pool)
        .await
        .unwrap();
    // Force the HNSW path even for this deliberately small regression fixture.
    let options = state
        .pool
        .connect_options()
        .as_ref()
        .clone()
        .options([("enable_seqscan", "off"), ("enable_bitmapscan", "off")]);
    let hnsw = sqlx::postgres::PgPoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    let p = access_sql::visible_photos(&Principal::Anonymous);
    let filters = [search::Filter {
        key: "camera".into(),
        value: "test camera".into(),
    }];
    for (vector, like) in [
        (Some(near.as_str()), None),
        (None, Some(short[599].as_str())),
    ] {
        let rows = search::candidates(&hnsw, &p, &filters, "", vector, like, List::Semantic)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].photo.id.0, uuid::Uuid::from_u128(1));
    }
}

#[tokio::test]
async fn private_face_routes_are_unreachable_on_public_listener() {
    let app = crate::public_router(crate::tests::state(false, 600));
    for path in [
        "/admin/people".into(),
        "/admin/people/search?q=person:Yash".into(),
        "/api/v1/faces".into(),
        "/api/v1/persons".into(),
        "/admin/people/faces/00000000-0000-0000-0000-000000000001.jpeg".into(),
        format!("/i/{}/face.jpeg", "a".repeat(64)),
        format!("/i/{}/face.avif", "a".repeat(64)),
        format!("/i/{}/face.jpeg.hdr", "a".repeat(64)),
    ] {
        assert_eq!(
            app.clone().oneshot(request(&path)).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
}

mod regressions;
