//! End-to-end library tests reuse the real account/session HTTP client.
use super::*;
use serde_json::{Value, json};
use yhotos_core::{AlbumId, PhotoId, ShortId};
use yhotos_db::{albums, library, photos};

async fn admin(pool: PgPool) -> (Client, UserId) {
    let mut client = Client::new(pool, true).await;
    let (user, name, password) = client.seed(false, true).await;
    client.login(&name, &password, true).await;
    client.seed_totp(user).await;
    sqlx::query!("UPDATE sessions SET mfa_at=now() WHERE user_id=$1", user.0)
        .execute(&client.pool)
        .await
        .unwrap();
    assert_eq!(client.get("/admin").await.status, StatusCode::OK);
    client.csrf = client.jar.get("__Host-yhotos-csrf").unwrap().clone();
    (client, user)
}
async fn album(pool: &PgPool, title: &str) -> AlbumId {
    albums::create(
        &mut pool.acquire().await.unwrap(),
        None,
        &yhotos_core::Slug::from_title(title),
        title,
        yhotos_core::access::Visibility::Public,
    )
    .await
    .unwrap()
    .id
}
async fn photo(pool: &PgPool, album: AlbumId, title: &str) -> photos::PhotoRow {
    photos::create(
        &mut pool.acquire().await.unwrap(),
        &photos::NewPhoto {
            album_id: album,
            title: title.into(),
            original_hash: blake3::hash(title.as_bytes()).to_hex().to_string(),
            original_name: "fixture.jpeg".into(),
            original_bytes: 12,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap()
}
async fn action(client: &mut Client, body: Value) {
    let r = client.json("/admin/api/actions", body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}
fn samples() -> Vec<Value> {
    let id = Uuid::now_v7();
    vec![
        json!({"op":"album_create","title":"x"}),
        json!({"op":"album_edit","id":id,"title":"x","description":"","visibility":"public","manual_order":false,"grants":[]}),
        json!({"op":"album_move","id":id,"direction":"up"}),
        json!({"op":"bulk","ids":[id],"action":{"kind":"move","album_id":id}}),
        json!({"op":"bulk","ids":[id],"action":{"kind":"visibility","visibility":"public","grants":[]}}),
        json!({"op":"photo_edit","id":id,"title":"x","description":"","visibility":"public","tags":[],"grants":[]}),
        json!({"op":"trash","kind":"photo","id":id}),
        json!({"op":"restore","kind":"photo","id":id}),
        json!({"op":"purge","kind":"photo","id":id,"confirmation":"PURGE"}),
        json!({"op":"flag_resolve","id":id,"resolution":"dismissed"}),
        json!({"op":"ingest_retry","id":id}),
        json!({"op":"group_save","name":"x","members":[]}),
        json!({"op":"user_state","id":id,"disabled":true,"delete":false}),
        json!({"op":"user_groups","id":id,"groups":[]}),
        json!({"op":"session_revoke","id":id}),
        json!({"op":"job","id":id,"retry":true}),
    ]
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn public_listener_has_no_admin_routes_and_every_mutation_is_gated(pool: PgPool) {
    let (mut client, user) = admin(pool.clone()).await;
    let a = album(&pool, "Routes").await;
    let p = photo(&pool, a, "Route photo").await;
    let mut public = client.listener(false);
    let replace = |path: &str| {
        path.replace("{id}", &a.to_string())
            .replace("{short_id}", &p.short_id.to_string())
    };
    for path in yhotos_http::admin::route_paths() {
        for method in ["GET", "POST", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
            assert_eq!(
                public
                    .send(method, &replace(&path), "{}".into(), true)
                    .await
                    .status,
                StatusCode::NOT_FOUND,
                "{method} {path}"
            );
        }
    }
    let saved = client.csrf.clone();
    client.csrf.clear();
    for body in samples() {
        assert_eq!(
            client.json("/admin/api/actions", body).await.status,
            StatusCode::FORBIDDEN
        );
    }
    for path in [
        "/admin/api/users/temporary-password",
        "/admin/api/share-links",
        "/admin/api/share-links/00000000-0000-0000-0000-000000000000/revoke",
    ] {
        assert_eq!(
            client.json(path, json!({})).await.status,
            StatusCode::FORBIDDEN
        );
    }
    client.csrf = saved;
    let origin = client.origin.clone();
    client.origin = "https://attacker.invalid".into();
    assert_eq!(
        client
            .json("/admin/api/actions", samples().remove(0))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    client.origin = origin;
    sqlx::query!("UPDATE sessions SET mfa_at=NULL WHERE user_id=$1", user.0)
        .execute(&pool)
        .await
        .unwrap();
    for body in samples() {
        let r = client.json("/admin/api/actions", body).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
    }
    for path in [
        "/admin/api/users/temporary-password",
        "/admin/api/share-links",
    ] {
        assert_eq!(
            client.json(path, json!({})).await.status,
            StatusCode::SEE_OTHER
        );
    }
    let count = sqlx::query_scalar!("SELECT count(*) FROM audit_log WHERE target_kind='library'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, Some(0));
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn thousand_photo_bulk_is_atomic_bounded_and_one_audit_per_action(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let a = album(&pool, "Source").await;
    let b = album(&pool, "Destination").await;
    let ids: Vec<_> = (0..1000).map(|_| PhotoId::new()).collect();
    let uuids: Vec<_> = ids.iter().map(|id| id.0).collect();
    let shorts: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    let hash = blake3::hash(b"fixture").to_hex().to_string();
    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind) SELECT id,short,$3,'Fixture',$4,'fixture.jpeg',1,'image/jpeg','image' FROM unnest($1::uuid[],$2::text[]) p(id,short)",&uuids,&shorts,a.0,hash).execute(&pool).await.unwrap();
    action(
        &mut client,
        json!({"op":"bulk","ids":uuids,"action":{"kind":"move","album_id":b}}),
    )
    .await;
    action(&mut client,json!({"op":"bulk","ids":uuids,"action":{"kind":"visibility","visibility":"restricted","grants":[]}})).await;
    let audit = sqlx::query!(
        "SELECT detail,xmin::text AS transaction FROM audit_log WHERE action='bulk' ORDER BY at"
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    for row in &audit {
        assert_eq!(row.detail["action"]["ids"].as_array().unwrap().len(), 1000);
    }
    let transactions = sqlx::query_scalar!(
        "SELECT DISTINCT xmin::text FROM photos WHERE id=ANY($1)",
        &uuids
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions.first(), audit.last().map(|r| &r.transaction));
    let bad=client.json("/admin/api/actions",json!({"op":"bulk","ids":uuids,"action":{"kind":"visibility","visibility":"public","grants":[{"kind":"user","id":Uuid::now_v7()}]}})).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM photos WHERE album_id=$1 AND visibility='restricted'",
        b.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, Some(1000));
    let mut over = uuids.clone();
    over.push(Uuid::now_v7());
    assert_eq!(
        client
            .json(
                "/admin/api/actions",
                json!({"op":"bulk","ids":over,"action":{"kind":"delete"}})
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let count = sqlx::query_scalar!("SELECT count(*) FROM audit_log WHERE action='bulk'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, Some(2));
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn album_rename_redirects_descendants_and_keyboard_moves_reject_cycles(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let a = album(&pool, "Before").await;
    let b = album(&pool, "Second").await;
    let child = albums::create(
        &mut pool.acquire().await.unwrap(),
        Some(a),
        &yhotos_core::Slug::from_title("Child"),
        "Child",
        yhotos_core::access::Visibility::Public,
    )
    .await
    .unwrap();
    action(&mut client,json!({"op":"album_edit","id":a,"title":"After","description":"**Sanitized** <script>bad()</script>","visibility":"public","manual_order":true,"grants":[]})).await;
    let mut public = client.listener(false);
    for (old, new) in [
        ("/a/before", "/a/after"),
        ("/a/before/child", "/a/after/child"),
    ] {
        let r = public.get(old).await;
        assert_eq!(r.status, StatusCode::MOVED_PERMANENTLY, "{}", r.body);
        assert_eq!(r.headers[header::LOCATION], new);
    }
    action(
        &mut client,
        json!({"op":"album_move","id":b,"direction":"up"}),
    )
    .await;
    let list = albums::children(&mut pool.acquire().await.unwrap(), None)
        .await
        .unwrap();
    assert_eq!(list.first().unwrap().id, b);
    let rendered = client.get("/admin/albums").await.body;
    assert!(
        rendered
            .find(&format!("href=\"/admin/albums/{b}\""))
            .unwrap()
            < rendered
                .find(&format!("href=\"/admin/albums/{a}\""))
                .unwrap()
    );
    action(
        &mut client,
        json!({"op":"album_move","id":a,"direction":"indent"}),
    )
    .await;
    assert_eq!(
        albums::get_by_id(&mut pool.acquire().await.unwrap(), a)
            .await
            .unwrap()
            .unwrap()
            .parent_id,
        Some(b)
    );
    assert_eq!(
        client
            .json(
                "/admin/api/actions",
                json!({"op":"album_move","id":b,"direction":"move","parent_id":child.id})
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    action(
        &mut client,
        json!({"op":"album_move","id":a,"direction":"outdent"}),
    )
    .await;
    action(
        &mut client,
        json!({"op":"album_move","id":b,"direction":"down"}),
    )
    .await;
    let tree = client.get("/admin/albums").await;
    assert!(!tree.body.contains("<script>bad()"));
    assert!(tree.body.contains("Move up"));
}
#[derive(Default)]
struct Purger(std::sync::Mutex<Vec<Vec<String>>>);
impl yhotos_http::admin::cache::CachePurger for Purger {
    fn purge<'a>(
        &'a self,
        tags: &'a [String],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), yhotos_http::error::HttpError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            self.0.lock().unwrap().push(tags.to_vec());
            Ok(())
        })
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn visibility_and_place_edits_purge_origin_and_all_edge_tags(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let a = album(&pool, "Cache").await;
    let p = photo(&pool, a, "Cached").await;
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Fixture','CA',1,2)",
        place
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("UPDATE photos SET place_id=$2 WHERE id=$1", p.id.0, place)
        .execute(&pool)
        .await
        .unwrap();
    let mock = Arc::new(Purger::default());
    client.state = AppState::from(yhotos_http::AppResources {
        config: client.state.config,
        auth: client.state.auth.clone(),
        pool: pool.clone(),
        store: client.state.store.clone(),
        files: yhotos_http::files::FileCache::default(),
        cache_purger: mock.clone(),
    });
    client.app = yhotos_http::admin_router(client.state.clone());
    action(&mut client,json!({"op":"bulk","ids":[p.id],"action":{"kind":"visibility","visibility":"restricted","grants":[]}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let pending = library::pending_purges(&mut pool.acquire().await.unwrap())
                .await
                .unwrap();
            if pending.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let tags = mock.0.lock().unwrap().last().unwrap().clone();
    for tag in [
        "visibility".into(),
        "map".into(),
        "calendar".into(),
        "places".into(),
        format!("place:{place}"),
        format!("album:{a}"),
        format!("photo:{}", p.id),
    ] {
        assert!(tags.contains(&tag), "missing {tag}");
    }
    let pending = library::pending_purges(&mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    assert_eq!(pending.len(), 0);
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn flag_resolution_flows_and_trash_restore_purge_confirmation(pool: PgPool) {
    let (mut client, user) = admin(pool.clone()).await;
    let a = album(&pool, "Flags").await;
    let p = photo(&pool, a, "Flagged").await;
    for resolution in ["dismissed", "reedited", "visibility", "removed"] {
        let f = yhotos_db::flags::flag(
            &mut pool.acquire().await.unwrap(),
            p.id,
            yhotos_db::likes::Actor::User(user),
        )
        .await
        .unwrap();
        action(&mut client,json!({"op":"flag_resolve","id":p.id,"resolution":resolution,"visibility":"restricted"})).await;
        let row = sqlx::query!(
            "SELECT resolution::text AS resolution,resolved_at FROM flags WHERE id=$1",
            f.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.resolution.as_deref(), Some(resolution));
        assert!(row.resolved_at.is_some());
    }
    action(
        &mut client,
        json!({"op":"restore","kind":"photo","id":p.id}),
    )
    .await;
    assert!(
        photos::get_by_id(&mut pool.acquire().await.unwrap(), p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
    action(&mut client, json!({"op":"trash","kind":"photo","id":p.id})).await;
    assert_eq!(
        client
            .json(
                "/admin/api/actions",
                json!({"op":"purge","kind":"photo","id":p.id,"confirmation":"yes"})
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    action(
        &mut client,
        json!({"op":"purge","kind":"photo","id":p.id,"confirmation":"PURGE"}),
    )
    .await;
    assert!(
        photos::get_by_id(&mut pool.acquire().await.unwrap(), p.id)
            .await
            .unwrap()
            .is_none()
    );
    let pending = yhotos_db::jobs::list(
        &mut pool.acquire().await.unwrap(),
        yhotos_db::jobs::JobStatus::Pending,
        0,
    )
    .await
    .unwrap();
    assert_ne!(pending.len(), 0);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn every_page_renders_safe_populated_controls_and_optional_walkthrough(pool: PgPool) {
    let (mut client, user) = admin(pool.clone()).await;
    let a = album(&pool, "Autumn walks").await;
    let child = albums::create(
        &mut pool.acquire().await.unwrap(),
        Some(a),
        &yhotos_core::Slug::from_title("Quiet trails"),
        "Quiet trails",
        yhotos_core::access::Visibility::Public,
    )
    .await
    .unwrap();
    let p = photo(&pool, a, "Morning light").await;
    let second = photo(&pool, a, "Forest path").await;
    let trashed = photo(&pool, child.id, "Old edit").await;
    action(&mut client,json!({"op":"album_edit","id":a,"title":"Autumn walks","description":"**October photographs** from nearby trails.","visibility":"restricted","manual_order":true,"grants":[]})).await;
    action(
        &mut client,
        json!({"op":"trash","kind":"photo","id":trashed.id}),
    )
    .await;
    yhotos_db::likes::like(
        &mut pool.acquire().await.unwrap(),
        p.id,
        yhotos_db::likes::Actor::User(user),
    )
    .await
    .unwrap();
    yhotos_db::flags::flag(
        &mut pool.acquire().await.unwrap(),
        second.id,
        yhotos_db::likes::Actor::User(user),
    )
    .await
    .unwrap();
    let (_, name, _) = client.seed(false, false).await;
    action(
        &mut client,
        json!({"op":"group_save","name":"Family","members":[user]}),
    )
    .await;
    let link=client.json("/admin/api/share-links",json!({"target_kind":"album","target_id":a,"allow_view":true,"allow_download":false,"allow_like":true,"allow_flag":true})).await;
    assert_eq!(link.status, StatusCode::CREATED);
    let job = yhotos_db::jobs::enqueue(
        &mut pool.acquire().await.unwrap(),
        "ingest",
        &json!({"photo_id":second.id}),
        0,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    sqlx::query!("UPDATE jobs SET error='Image could not be processed. Retry after replacing the original.',finished_at=now(),attempts=max_attempts WHERE id=$1",job.id.0).execute(&pool).await.unwrap();
    sqlx::query!(
        "UPDATE users SET username='Gallery administrator' WHERE id=$1",
        user.0
    )
    .execute(&pool)
    .await
    .unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/dev/fixtures/medium.jpeg");
    let bytes = std::fs::read(&source).unwrap();
    let hash: yhotos_storage::ContentHash = blake3::hash(&bytes).to_hex().as_str().parse().unwrap();
    let temp = client.state.store.tmp_dir().unwrap();
    let staged = temp.path().join("fixture.jpeg");
    std::fs::write(&staged, &bytes).unwrap();
    client
        .state
        .store
        .put_derivative(
            &yhotos_storage::DerivativeKey {
                hash: &hash,
                variant: "thumb",
                format: "jpeg",
                hdr: false,
            },
            &staged,
        )
        .await
        .unwrap();
    let hash = hash.to_string();
    for photo in [p.id, second.id] {
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',$3,640,427,$4)",Uuid::now_v7(),photo.0,hash,i64::try_from(bytes.len()).unwrap()).execute(&pool).await.unwrap();
    }
    sqlx::query!("UPDATE photos SET status='ready' WHERE id=$1", p.id.0)
        .execute(&pool)
        .await
        .unwrap();
    let pages = vec![
        ("dashboard", "/admin".to_owned()),
        ("albums", "/admin/albums".to_owned()),
        ("album", format!("/admin/albums/{a}")),
        ("photo", format!("/admin/photos/{}", p.short_id)),
        ("trash", "/admin/trash".to_owned()),
        ("flags", "/admin/flags".to_owned()),
        ("likes", "/admin/likes".to_owned()),
        ("users", "/admin/users".to_owned()),
        ("groups", "/admin/groups".to_owned()),
        ("links", "/admin/links".to_owned()),
        ("jobs", "/admin/jobs".to_owned()),
        ("audit", "/admin/audit".to_owned()),
        ("upload", "/admin/upload".to_owned()),
    ];
    for (name, path) in &pages {
        let r = client.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "{name}: {}", r.body);
        assert!(
            r.headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert!(r.body.contains("nonce="));
        if r.body.contains("class=\"admin-form\"") {
            // Without JavaScript the browser must never place passphrases in a query string.
            assert!(
                r.body
                    .contains("<form method=\"post\" action=\"/admin/api/")
            );
        }
        assert!(!r.body.contains("password_hash"));
        assert!(!r.body.contains("token_hash"));
        assert!(!r.body.contains("secret_enc"));
        if *name == "albums" {
            assert!(r.body.contains("hidden by restricted parent Autumn walks"));
            assert!(!r.body.contains("<option value=\"inherit\""));
        }
    }
    assert!(client.get("/admin/users").await.body.contains(&name));
    if std::env::var_os("YHOTOS_ADMIN_WALKTHROUGH").is_some()
        || std::env::var_os("YHOTOS_ADMIN_WALKTHROUGH_HOLD").is_some()
    {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let session = client.jar.get("ya").unwrap().clone();
        let app = axum::Router::new()
            .route(
                "/__fixture/login",
                axum::routing::get(move || async move {
                    (
                        StatusCode::SEE_OTHER,
                        [
                            (
                                header::SET_COOKIE,
                                format!("ya={session}; Path=/; HttpOnly; SameSite=Strict"),
                            ),
                            (header::LOCATION, "/admin".to_owned()),
                        ],
                    )
                }),
            )
            .merge(client.app.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        if std::env::var_os("YHOTOS_ADMIN_WALKTHROUGH_HOLD").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/admin-walkthrough.json");
            std::fs::write(
                path,
                json!({"base":format!("http://{address}"),"pages":pages}).to_string(),
            )
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_mins(30)).await;
            server.abort();
            return;
        }
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/dev/admin-walkthrough.mjs");
        let mut child = tokio::process::Command::new("node")
            .arg(script)
            .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let input=json!({"base":format!("http://{address}"),"session":client.jar.get("ya").unwrap(),"pages":pages}).to_string();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());
        server.abort();
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn users_groups_sessions_and_one_time_credentials_are_audited_without_secrets(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let reply = client
        .json(
            "/admin/api/users/temporary-password",
            json!({"username":"Jordan"}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let value: Value = serde_json::from_str(&reply.body).unwrap();
    let id: Uuid = serde_json::from_value(value["id"].clone()).unwrap();
    let password = value["temporary_password"].as_str().unwrap();
    assert!(password.len() >= 20);
    let user = users::get_by_id(&mut pool.acquire().await.unwrap(), UserId(id))
        .await
        .unwrap()
        .unwrap();
    assert!(user.must_change_password);
    assert!(
        yhotos_auth::verify_password(
            &SecretString::from(password),
            &yhotos_auth::PasswordHash::parse(&user.password_hash).unwrap()
        )
        .unwrap()
    );
    let group = Uuid::now_v7();
    action(
        &mut client,
        json!({"op":"group_save","name":"Friends","members":[id]}),
    )
    .await;
    let group_id = sqlx::query_scalar!("SELECT id FROM groups WHERE name='Friends'")
        .fetch_one(&pool)
        .await
        .unwrap();
    action(
        &mut client,
        json!({"op":"group_save","id":group_id,"name":"Close friends","members":[id]}),
    )
    .await;
    action(
        &mut client,
        json!({"op":"user_groups","id":id,"groups":[group_id]}),
    )
    .await;
    let session_id = Uuid::now_v7();
    let token = Token::generate();
    let now = time::OffsetDateTime::now_utc();
    sessions::create(
        &mut pool.acquire().await.unwrap(),
        &sessions::Session {
            id: session_id,
            user_id: UserId(id),
            admin: false,
            created_at: now,
            last_seen_at: now,
            expires_at: now + time::Duration::hours(1),
            reauth_at: None,
            mfa_at: None,
            ip_hash: vec![0; 32],
            ua_class: "other".into(),
            token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
        },
    )
    .await
    .unwrap();
    action(&mut client, json!({"op":"session_revoke","id":session_id})).await;
    assert!(
        sessions::get(
            &mut pool.acquire().await.unwrap(),
            token.hash(TokenKind::Session).as_bytes()
        )
        .await
        .unwrap()
        .is_none()
    );
    for disabled in [true, false] {
        action(
            &mut client,
            json!({"op":"user_state","id":id,"disabled":disabled,"delete":false}),
        )
        .await;
    }
    let reset = client
        .json("/admin/api/users/temporary-password", json!({"id":id}))
        .await;
    assert_eq!(reset.status, StatusCode::OK);
    let reset: Value = serde_json::from_str(&reset.body).unwrap();
    assert_ne!(reset["temporary_password"], value["temporary_password"]);
    assert_eq!(client.json("/admin/api/actions",json!({"op":"user_state","id":id,"disabled":true,"delete":true,"confirmation":"yes"})).await.status,StatusCode::BAD_REQUEST);
    action(
        &mut client,
        json!({"op":"user_state","id":id,"disabled":true,"delete":true,"confirmation":"DELETE"}),
    )
    .await;
    let audit=sqlx::query_scalar!("SELECT detail FROM audit_log WHERE action IN ('user_temporary_password','group_save','user_groups','session_revoke','user_state')").fetch_all(&pool).await.unwrap();
    assert_eq!(audit.len(), 9);
    for row in audit {
        let row = row.to_string();
        assert!(!row.contains(password));
        assert!(!row.contains(reset["temporary_password"].as_str().unwrap()));
        assert!(!row.contains("password_hash"));
        assert!(!row.contains("token_hash"));
    }
    // Nonexistent principals, including a syntactically valid UUID, are rejected atomically.
    assert_eq!(
        client
            .json(
                "/admin/api/actions",
                json!({"op":"user_groups","id":id,"groups":[group]})
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn bulk_generated_ids_preserve_uuid_v7_timestamp_and_variant(pool: PgPool) {
    let before = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    let ids =
        sqlx::query_scalar!(r#"SELECT yhotos_uuid_v7() AS "id!" FROM generate_series(1,1000)"#)
            .fetch_all(&pool)
            .await
            .unwrap();
    let after = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        1000
    );
    for id in ids {
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        let timestamp = id
            .as_bytes()
            .iter()
            .take(6)
            .fold(0_i128, |n, b| n * 256 + i128::from(*b));
        assert!((before..=after).contains(&timestamp));
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn bulk_patterns_tags_copy_and_manual_photo_order_keep_identity_rules(pool: PgPool) {
    let (mut client, user) = admin(pool.clone()).await;
    let source = album(&pool, "Batch").await;
    let destination = album(&pool, "Copies").await;
    let first = photo(&pool, source, "First").await;
    let second = photo(&pool, source, "Second").await;
    let third = photo(&pool, source, "Third").await;
    let selected = vec![second.id.0, first.id.0, third.id.0];
    let tag = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO tags(id,slug,name,source) VALUES($1,'cafe','Café','manual')",
        tag
    )
    .execute(&pool)
    .await
    .unwrap();
    action(&mut client,json!({"op":"bulk","ids":selected,"action":{"kind":"tags","add":["Cafe","Night"],"remove":[]}})).await;
    assert_eq!(
        sqlx::query_scalar!("SELECT name FROM tags WHERE id=$1", tag)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Café"
    );
    action(
        &mut client,
        json!({"op":"bulk","ids":selected,"action":{"kind":"tags","add":[],"remove":["Night"]}}),
    )
    .await;
    let links = sqlx::query_scalar!(
        "SELECT count(*) FROM photo_tags WHERE photo_id=ANY($1)",
        &selected
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(links, Some(3));
    action(&mut client,json!({"op":"bulk","ids":selected,"action":{"kind":"pattern","title":"{album} {n}","description":"Frame {n}"}})).await;
    let row = photos::get_by_id(&mut pool.acquire().await.unwrap(), second.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.title, "Batch 1");
    assert_eq!(row.description, "Frame 1");
    action(&mut client,json!({"op":"bulk","ids":[first.id],"action":{"kind":"visibility","visibility":"restricted","grants":[{"kind":"user","id":user}]}})).await;
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',$3,100,100,1)",Uuid::now_v7(),first.id.0,first.original_hash).execute(&pool).await.unwrap();
    sqlx::query!("UPDATE photos SET status='ready' WHERE id=$1", first.id.0)
        .execute(&pool)
        .await
        .unwrap();
    yhotos_db::likes::like(
        &mut pool.acquire().await.unwrap(),
        first.id,
        yhotos_db::likes::Actor::User(user),
    )
    .await
    .unwrap();
    let copy=client.json("/admin/api/actions",json!({"op":"bulk","ids":[first.id,second.id],"action":{"kind":"copy","album_id":destination}})).await;
    assert_eq!(copy.status, StatusCode::OK);
    let response: Value = serde_json::from_str(&copy.body).unwrap();
    let copies: Vec<Uuid> = serde_json::from_value(response["ids"].clone()).unwrap();
    assert_eq!(copies.len(), 2);
    let copy = photos::get_by_id(&mut pool.acquire().await.unwrap(), PhotoId(copies[0]))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(copy.id, first.id);
    assert_ne!(copy.short_id, first.short_id);
    assert_eq!(copy.original_hash, first.original_hash);
    assert_eq!(copy.album_id, destination);
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT count(*) FROM derivatives WHERE photo_id=$1",
            copy.id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT count(*) FROM photo_grants WHERE photo_id=$1 AND principal_id=$2",
            copy.id.0,
            user.0
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM likes WHERE photo_id=ANY($1)", &copies)
            .fetch_one(&pool)
            .await
            .unwrap(),
        Some(0)
    );
    action(&mut client,json!({"op":"album_edit","id":source,"title":"Batch","description":"","visibility":"public","manual_order":true,"grants":[]})).await;
    let order = vec![third.id.0, first.id.0, second.id.0];
    action(
        &mut client,
        json!({"op":"bulk","ids":order,"action":{"kind":"reorder","album_id":source}}),
    )
    .await;
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT id FROM photos WHERE album_id=$1 ORDER BY sort_order,id",
            source.0
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        order
    );
}

#[derive(Default)]
struct CachedPages(std::sync::Mutex<BTreeMap<String, (Vec<String>, String)>>);
impl yhotos_http::admin::cache::CachePurger for CachedPages {
    fn purge<'a>(
        &'a self,
        tags: &'a [String],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), yhotos_http::error::HttpError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .retain(|_, (stored, _)| !stored.iter().any(|tag| tags.contains(tag)));
            Ok(())
        })
    }
}
async fn anonymous_get(client: &mut Client, path: &str) -> Reply {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(client.network));
    let response = client.app.clone().oneshot(request).await.unwrap();
    client.receive(response).await
}
async fn cached_get(client: &mut Client, cache: &CachedPages, path: &str) -> String {
    if let Some((_, body)) = cache.0.lock().unwrap().get(path) {
        return body.clone();
    }
    let response = anonymous_get(client, path).await;
    assert_eq!(response.status, StatusCode::OK, "{path}");
    assert!(
        response.headers["cache-control"]
            .to_str()
            .unwrap()
            .starts_with("public")
    );
    let tags: Vec<String> = response.headers["cache-tag"]
        .to_str()
        .unwrap()
        .split(',')
        .map(str::to_owned)
        .collect();
    assert!(tags.iter().any(|tag| tag == "visibility"), "{path}");
    cache
        .0
        .lock()
        .unwrap()
        .insert(path.into(), (tags, response.body.clone()));
    response.body
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn restricted_child_cover_is_removed_from_cached_parent_and_aggregate_pages(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let ancestor = album(&pool, "Ancestor").await;
    let parent = album(&pool, "Parent").await;
    let child = album(&pool, "Child").await;
    sqlx::query!(
        "UPDATE albums SET parent_id=$2 WHERE id=$1",
        parent.0,
        ancestor.0
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE albums SET parent_id=$2 WHERE id=$1",
        child.0,
        parent.0
    )
    .execute(&pool)
    .await
    .unwrap();
    let p = photo(&pool, child, "Private cover title").await;
    let placeholder = STANDARD.decode("AAgCBwAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
    sqlx::query!("UPDATE photos SET status='ready',placeholder=$2,gps_lat=43.7,gps_lon=-79.4,taken_at='2024-01-02 12:00:00+00',taken_at_local='2024-01-02 12:00:00' WHERE id=$1",p.id.0,&placeholder).execute(&pool).await.unwrap();
    for a in [child, parent, ancestor] {
        sqlx::query!(
            "UPDATE albums SET cover_photo_id=$2 WHERE id=$1",
            a.0,
            p.id.0
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',$3,100,100,1)",Uuid::now_v7(),p.id.0,p.original_hash).execute(&pool).await.unwrap();
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'City','CA',43.7,-79.4)",
        place
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("UPDATE photos SET place_id=$2 WHERE id=$1", p.id.0, place)
        .execute(&pool)
        .await
        .unwrap();
    let cache = Arc::new(CachedPages::default());
    client.state = AppState::from(yhotos_http::AppResources {
        config: client.state.config,
        auth: client.state.auth.clone(),
        pool: pool.clone(),
        store: client.state.store.clone(),
        files: yhotos_http::files::FileCache::default(),
        cache_purger: cache.clone(),
    });
    client.app = yhotos_http::admin_router(client.state.clone());
    let mut public = client.listener(false);
    let paths = [
        "/".to_owned(),
        "/a/ancestor".into(),
        "/a/ancestor/parent".into(),
        "/a/ancestor/parent/child".into(),
        "/map".into(),
        "/calendar".into(),
        format!("/places/{place}"),
        format!("/p/{}", p.short_id),
        format!("/api/v1/photos/{}", p.short_id),
        format!("/api/v1/albums/{child}/photos"),
    ];
    for path in &paths {
        cached_get(&mut public, &cache, path).await;
    }
    let before = cached_get(&mut public, &cache, "/a/ancestor/parent").await;
    assert!(before.contains("Private cover title"));
    assert!(before.contains(&STANDARD.encode(&placeholder)));
    action(&mut client,json!({"op":"bulk","ids":[p.id],"action":{"kind":"visibility","visibility":"restricted","grants":[]}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !cache.0.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for path in ["/", "/a/ancestor", "/a/ancestor/parent"] {
        let after = cached_get(&mut public, &cache, path).await;
        assert!(!after.contains("Private cover title"), "{path}");
        assert!(!after.contains(&STANDARD.encode(&placeholder)), "{path}");
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn title_only_save_retains_automatic_date_and_gps_across_replacement(pool: PgPool) {
    let (mut client, actor) = admin(pool.clone()).await;
    let a = album(&pool, "Automatic metadata").await;
    let p = photo(&pool, a, "Before").await;
    let old_date = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let mut metadata = photos::PhotoMetadata {
        taken_at: Some(old_date),
        gps_lat: Some(43.7),
        gps_lon: Some(-79.4),
        ..Default::default()
    };
    photos::update_metadata(&mut pool.acquire().await.unwrap(), p.id, &metadata)
        .await
        .unwrap();
    let editor = client.get(&format!("/admin/photos/{}", p.short_id)).await;
    for name in ["taken_at_mode", "gps_mode", "place_mode"] {
        assert!(editor.body.contains(&format!("name=\"{name}\"")));
    }
    // Submit the populated controls exactly as a title-only browser save does.
    action(&mut client,json!({"op":"photo_edit","id":p.id,"title":"New title","description":"","taken_at_mode":"keep","gps_mode":"keep","place_mode":"keep","taken_at":old_date.format(&time::format_description::well_known::Rfc3339).unwrap(),"gps_lat":43.7,"gps_lon":-79.4,"place_id":null,"visibility":"inherit","tags":[],"grants":[]})).await;
    let saved = sqlx::query!(
        "SELECT taken_at_override,gps_overridden,title FROM photos WHERE id=$1",
        p.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved.title, "New title");
    assert!(saved.taken_at_override.is_none());
    assert!(!saved.gps_overridden);
    let original = photos::NewPhoto {
        album_id: a,
        title: "replacement".into(),
        original_hash: blake3::hash(b"replacement").to_hex().to_string(),
        original_name: "replacement.jpeg".into(),
        original_bytes: 12,
        mime: "image/jpeg".into(),
        kind: photos::MediaKind::Image,
    };
    library::replace(&mut pool.acquire().await.unwrap(), actor, p.id.0, &original)
        .await
        .unwrap();
    metadata.taken_at = Some(old_date + time::Duration::days(5));
    metadata.gps_lat = Some(10.0);
    metadata.gps_lon = Some(20.0);
    photos::update_metadata(&mut pool.acquire().await.unwrap(), p.id, &metadata)
        .await
        .unwrap();
    let saved = sqlx::query!(
        "SELECT taken_at,gps_lat,gps_lon FROM photos WHERE id=$1",
        p.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved.taken_at, metadata.taken_at);
    assert_eq!(saved.gps_lat, Some(10.0));
    assert_eq!(saved.gps_lon, Some(20.0));
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn explicit_place_and_empty_place_survive_geocoding_and_replacement(pool: PgPool) {
    let (mut client, actor) = admin(pool.clone()).await;
    let a = album(&pool, "Places").await;
    let p = photo(&pool, a, "Override").await;
    sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES(1,'Automatic city','Ontario','CA',43.7,-79.4,100000)").execute(&pool).await.unwrap();
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Chosen city','CA',1,2)",
        place
    )
    .execute(&pool)
    .await
    .unwrap();
    for selected in [Some(place), None] {
        sqlx::query!(
            "UPDATE photos SET gps_lat=43.7,gps_lon=-79.4 WHERE id=$1",
            p.id.0
        )
        .execute(&pool)
        .await
        .unwrap();
        action(&mut client,json!({"op":"photo_edit","id":p.id,"title":"Override","description":"","place_mode":"manual","place_id":selected,"visibility":"inherit","tags":[],"grants":[]})).await;
        yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
            .await
            .unwrap();
        let original = photos::NewPhoto {
            album_id: a,
            title: "replacement".into(),
            original_hash: blake3::hash(b"new place bytes").to_hex().to_string(),
            original_name: "replacement.jpeg".into(),
            original_bytes: 12,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        };
        library::replace(&mut pool.acquire().await.unwrap(), actor, p.id.0, &original)
            .await
            .unwrap();
        photos::update_metadata(
            &mut pool.acquire().await.unwrap(),
            p.id,
            &photos::PhotoMetadata {
                gps_lat: Some(43.7),
                gps_lon: Some(-79.4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
            .await
            .unwrap();
        let row = sqlx::query!(
            "SELECT place_id,place_overridden FROM photos WHERE id=$1",
            p.id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.place_id, selected);
        assert!(row.place_overridden);
    }
    action(&mut client,json!({"op":"photo_edit","id":p.id,"title":"Override","description":"","place_mode":"automatic","visibility":"inherit","tags":[],"grants":[]})).await;
    yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
        .await
        .unwrap();
    let row=sqlx::query!("SELECT p.place_overridden,l.name FROM photos p JOIN places l ON l.id=p.place_id WHERE p.id=$1",p.id.0).fetch_one(&pool).await.unwrap();
    assert!(!row.place_overridden);
    assert_eq!(row.name, "Automatic city");
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn subset_reorder_above_thousand_preserves_all_remaining_photos(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let a = album(&pool, "Large manual album").await;
    sqlx::query!("UPDATE albums SET manual_order=true WHERE id=$1", a.0)
        .execute(&pool)
        .await
        .unwrap();
    let ids: Vec<_> = (0..1001).map(|_| PhotoId::new()).collect();
    let uuids: Vec<_> = ids.iter().map(|id| id.0).collect();
    let shorts: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind) SELECT id,short,$3,'Fixture',$4,'fixture.jpeg',1,'image/jpeg','image' FROM unnest($1::uuid[],$2::text[]) p(id,short)",&uuids,&shorts,a.0,blake3::hash(b"large order").to_hex().to_string()).execute(&pool).await.unwrap();
    let before = sqlx::query_scalar!(
        "SELECT id FROM photos WHERE album_id=$1 ORDER BY sort_order,id",
        a.0
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    action(&mut client,json!({"op":"bulk","ids":[before[1000],before[0]],"action":{"kind":"reorder","album_id":a}})).await;
    let after = sqlx::query_scalar!(
        "SELECT id FROM photos WHERE album_id=$1 ORDER BY sort_order,id",
        a.0
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = before.clone();
    expected.swap(0, 1000);
    assert_eq!(after, expected);
    let foreign = album(&pool, "Other album").await;
    let other = photo(&pool, foreign, "Unrelated").await;
    let response=client.json("/admin/api/actions",json!({"op":"bulk","ids":[before[0],other.id],"action":{"kind":"reorder","album_id":a}})).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT id FROM photos WHERE album_id=$1 ORDER BY sort_order,id",
            a.0
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        after
    );
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn older_failed_and_running_jobs_remain_accessible_through_filtered_pages(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let failed = Uuid::now_v7();
    let running = Uuid::now_v7();
    sqlx::query!("INSERT INTO jobs(id,kind,payload,created_at,finished_at,error,attempts) VALUES($1,'ingest','{}',now()-interval '2 days',now(),'Older unresolved failure',5)",failed).execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO jobs(id,kind,payload,created_at,locked_at,locked_by) VALUES($1,'ingest','{}',now()-interval '2 days',now(),'Older active worker')",running).execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO jobs(id,kind,payload,finished_at) SELECT yhotos_uuid_v7(),'ingest','{}',now() FROM generate_series(1,205)").execute(&pool).await.unwrap();
    let failures = client.get("/admin/jobs?q=failed").await;
    assert_eq!(failures.status, StatusCode::OK);
    assert!(failures.body.contains("Older unresolved failure"));
    assert!(failures.body.contains(&failed.to_string()));
    assert!(failures.body.contains("Retry job"));
    assert!(failures.body.contains("Delete job"));
    assert!(failures.body.contains("?page=1&amp;q=failed"));
    assert!(
        client
            .get("/admin/jobs?q=running")
            .await
            .body
            .contains("Older active worker")
    );
    assert!(
        client
            .get("/admin/jobs?page=2")
            .await
            .body
            .contains(&failed.to_string())
    );
    assert_eq!(
        client.get("/admin/jobs?q=invalid").await.status,
        StatusCode::BAD_REQUEST
    );
    action(&mut client, json!({"op":"job","id":failed,"retry":true})).await;
    assert!(
        !client
            .get("/admin/jobs?q=failed")
            .await
            .body
            .contains("Older unresolved failure")
    );
    action(&mut client, json!({"op":"job","id":failed,"retry":false})).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn warmed_album_and_legacy_redirects_are_purged_on_restriction_and_deletion(pool: PgPool) {
    let (mut client, _) = admin(pool.clone()).await;
    let parent = album(&pool, "Before").await;
    let child = albums::create(
        &mut pool.acquire().await.unwrap(),
        Some(parent),
        &yhotos_core::Slug::from_title("Child"),
        "Child",
        yhotos_core::access::Visibility::Public,
    )
    .await
    .unwrap();
    let p = photo(&pool, child.id, "Redirected photo").await;
    sqlx::query!("UPDATE photos SET status='ready' WHERE id=$1", p.id.0)
        .execute(&pool)
        .await
        .unwrap();
    for (path, target) in [
        (
            "/category/222",
            yhotos_core::access::ShareTarget::Album(child.id),
        ),
        ("/old-photo", yhotos_core::access::ShareTarget::Photo(p.id)),
    ] {
        yhotos_db::redirects::insert(&mut pool.acquire().await.unwrap(), path, target)
            .await
            .unwrap();
    }
    let cache = Arc::new(CachedPages::default());
    client.state = AppState::from(yhotos_http::AppResources {
        config: client.state.config,
        auth: client.state.auth.clone(),
        pool: pool.clone(),
        store: client.state.store.clone(),
        files: yhotos_http::files::FileCache::default(),
        cache_purger: cache.clone(),
    });
    client.app = yhotos_http::admin_router(client.state.clone());
    action(&mut client, json!({"op":"album_edit","id":parent,"title":"After","description":"","visibility":"public","manual_order":false,"grants":[]})).await;
    wait_for_purges(&pool).await;
    let paths = [
        (
            "/a/before".to_owned(),
            "/a/after".to_owned(),
            format!("album:{parent}"),
        ),
        (
            "/a/before/child".into(),
            "/a/after/child".into(),
            format!("album:{}", child.id),
        ),
        (
            "/index.php?/category/222".into(),
            "/a/after/child".into(),
            format!("album:{}", child.id),
        ),
        (
            "/legacy/old-photo".into(),
            format!("/p/{}", p.short_id),
            format!("photo:{}", p.id),
        ),
    ];
    let mut public = client.listener(false);
    for mutation in [
        json!({"op":"album_edit","id":parent,"title":"After","description":"","visibility":"restricted","manual_order":false,"grants":[]}),
        json!({"op":"trash","kind":"album","id":parent}),
    ] {
        for (path, destination, tag) in &paths {
            let response = anonymous_get(&mut public, path).await;
            assert_eq!(response.status, StatusCode::MOVED_PERMANENTLY, "{path}");
            assert_eq!(response.headers[header::LOCATION], destination.as_str());
            assert_eq!(
                response.headers[header::CACHE_CONTROL],
                "public, max-age=60"
            );
            let tags: Vec<String> = response.headers["cache-tag"]
                .to_str()
                .unwrap()
                .split(',')
                .map(str::to_owned)
                .collect();
            assert!(tags.contains(tag), "{path}");
            assert!(tags.iter().any(|tag| tag == "visibility"), "{path}");
            cache
                .0
                .lock()
                .unwrap()
                .insert(path.clone(), (tags, destination.clone()));
        }
        assert_eq!(cache.0.lock().unwrap().len(), paths.len());
        // The simulated edge serves each warmed redirect before the mutation.
        for (path, destination, _) in &paths {
            assert_eq!(&cache.0.lock().unwrap()[path].1, destination);
        }
        action(&mut client, mutation).await;
        wait_for_purges(&pool).await;
        assert!(cache.0.lock().unwrap().is_empty());
        let missing = anonymous_get(&mut public, "/a/does-not-exist").await;
        for (path, _, _) in &paths {
            let response = anonymous_get(&mut public, path).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
            assert!(!response.headers.contains_key(header::LOCATION));
            assert_eq!(response.body, missing.body, "{path}");
        }
        // Make the target public again before testing deletion of warmed redirects.
        if albums::get_by_id(&mut pool.acquire().await.unwrap(), parent)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
        {
            action(&mut client, json!({"op":"album_edit","id":parent,"title":"After","description":"","visibility":"public","manual_order":false,"grants":[]})).await;
            wait_for_purges(&pool).await;
        }
    }
}

async fn wait_for_purges(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !library::pending_purges(&mut pool.acquire().await.unwrap())
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
