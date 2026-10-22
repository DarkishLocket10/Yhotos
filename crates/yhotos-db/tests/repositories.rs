#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
//! Real PostgreSQL repository and constraint tests, each in an isolated database.

use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use yhotos_core::{
    AlbumId, PhotoId, Slug,
    access::{Grant, ShareTarget, Visibility},
};
use yhotos_db::{
    albums, flags, grants, groups, jobs, likes, photos, redirects, settings, share_links, tags,
    users,
};

fn secret() -> SecretString {
    Uuid::now_v7().to_string().into()
}
fn hash() -> String {
    format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple())
}
async fn album(conn: &mut PgConnection) -> albums::AlbumRow {
    albums::create(
        conn,
        None,
        &Slug::from_title(&Uuid::now_v7().to_string()),
        "Album",
        Visibility::Public,
    )
    .await
    .unwrap()
}
fn original(album_id: AlbumId) -> photos::NewPhoto {
    photos::NewPhoto {
        album_id,
        title: "Photo".into(),
        original_hash: hash(),
        original_name: "display.jpg".into(),
        original_bytes: 1234,
        mime: "image/jpeg".into(),
        kind: photos::MediaKind::Image,
    }
}
async fn photo(conn: &mut PgConnection, album: AlbumId) -> photos::PhotoRow {
    photos::create(conn, &original(album)).await.unwrap()
}

async fn claim_all(pool: &PgPool, worker: &str) -> Vec<yhotos_core::JobId> {
    let mut conn = pool.acquire().await.unwrap();
    let mut ids = Vec::new();
    while let Some((job, token)) = jobs::claim(&mut conn, worker, &["ingest"]).await.unwrap() {
        assert_eq!(token.job_id, job.id);
        assert_eq!(token.worker_id, worker);
        assert_eq!(token.attempt, job.attempts);
        ids.push(job.id);
        tokio::task::yield_now().await;
    }
    ids
}

#[sqlx::test]
async fn albums_all_operations_and_parent_slug_uniqueness(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let root = album(&mut conn).await;
    let child_slug = Slug::new("child").unwrap();
    let child = albums::create(
        &mut conn,
        Some(root.id),
        &child_slug,
        "Child",
        Visibility::Restricted,
    )
    .await
    .unwrap();
    assert!(
        albums::create(
            &mut conn,
            Some(root.id),
            &child_slug,
            "Duplicate",
            Visibility::Public
        )
        .await
        .is_err()
    );
    assert!(
        albums::create(
            &mut conn,
            None,
            &root.slug,
            "Duplicate root",
            Visibility::Public
        )
        .await
        .is_err()
    );
    let other = albums::create(
        &mut conn,
        None,
        &child_slug,
        "Same slug different parent",
        Visibility::Public,
    )
    .await
    .unwrap();
    assert_eq!(
        albums::get_by_id(&mut conn, root.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        root.id
    );
    assert!(
        albums::get_by_id(&mut conn, AlbumId::new())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        albums::get_by_path(&mut conn, &[root.slug.clone(), child_slug.clone()])
            .await
            .unwrap()
            .unwrap()
            .id,
        child.id
    );
    assert!(albums::get_by_path(&mut conn, &[]).await.unwrap().is_none());
    assert!(
        albums::get_by_path(&mut conn, &[child_slug, root.slug.clone()])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        albums::children(&mut conn, Some(root.id))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(albums::children(&mut conn, None).await.unwrap().len(), 2);
    assert_eq!(
        albums::ancestors(&mut conn, child.id)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id,
        root.id
    );
    assert!(
        albums::ancestors(&mut conn, root.id)
            .await
            .unwrap()
            .is_empty()
    );
    let updated = albums::update(
        &mut conn,
        child.id,
        &Slug::new("renamed").unwrap(),
        "Renamed",
        "Description",
        Visibility::Public,
        true,
    )
    .await
    .unwrap();
    assert_eq!(updated.description, "Description");
    assert!(updated.manual_order);
    assert!(updated.updated_at > child.updated_at);
    assert_eq!(
        albums::reorder(&mut conn, None, &[other.id, root.id])
            .await
            .unwrap(),
        2
    );
    assert!(
        albums::reorder(&mut conn, None, &[root.id, root.id])
            .await
            .is_err()
    );
    assert_eq!(
        albums::children(&mut conn, None)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id,
        other.id
    );
    let cover = photo(&mut conn, child.id).await;
    albums::set_cover(&mut conn, child.id, Some(cover.id))
        .await
        .unwrap();
    assert_eq!(
        albums::get_by_id(&mut conn, child.id)
            .await
            .unwrap()
            .unwrap()
            .cover_photo_id,
        Some(cover.id)
    );
    albums::set_cover(&mut conn, child.id, None).await.unwrap();
    albums::soft_delete(&mut conn, child.id).await.unwrap();
    assert!(
        albums::get_by_id(&mut conn, child.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some()
    );
    albums::restore(&mut conn, child.id).await.unwrap();
    assert!(
        albums::get_by_id(&mut conn, child.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
    assert_eq!(albums::list_tree(&mut conn).await.unwrap().len(), 3);
}

#[sqlx::test]
async fn photos_all_operations_and_replacement_preserves_identity(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let a = album(&mut conn).await;
    let b = album(&mut conn).await;
    let p = photo(&mut conn, a.id).await;
    let second = photo(&mut conn, a.id).await;
    let duplicate = photos::create(
        &mut conn,
        &photos::NewPhoto {
            original_hash: p.original_hash.clone(),
            ..original(a.id)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        photos::get_by_hash(&mut conn, &p.original_hash)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        photos::get_by_hash(&mut conn, &hash())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        photos::get_by_short_id(&mut conn, &p.short_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        p.id
    );
    let metadata = photos::PhotoMetadata {
        title: "Updated".into(),
        description: "Description".into(),
        width: Some(6000),
        height: Some(4000),
        orientation: Some(1),
        taken_at: Some(OffsetDateTime::now_utc()),
        is_hdr: true,
        has_gain_map: true,
        camera_make: Some("Camera".into()),
        camera_model: Some("Model".into()),
        lens: Some("Lens".into()),
        focal_mm: Some(50.0),
        aperture: Some(2.8),
        shutter: Some("1/100".into()),
        iso: Some(100),
        gps_lat: Some(45.0),
        gps_lon: Some(-75.0),
        dominant_colors: vec![0x0012_3456],
        placeholder: Some(vec![1, 2, 3]),
        ..Default::default()
    };
    photos::update_metadata(&mut conn, p.id, &metadata)
        .await
        .unwrap();
    let updated = photos::get_by_short_id(&mut conn, &p.short_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.width, Some(6000));
    assert_eq!(updated.dominant_colors, vec![0x0012_3456]);
    assert!(updated.is_hdr);
    assert_eq!(
        photos::move_to_album(&mut conn, &[p.id, second.id], b.id)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        photos::set_visibility(&mut conn, &[p.id, second.id], Visibility::Restricted)
            .await
            .unwrap(),
        2
    );
    for order in [
        photos::PhotoOrder::Manual,
        photos::PhotoOrder::Taken,
        photos::PhotoOrder::Posted,
        photos::PhotoOrder::Name,
    ] {
        let page = photos::list_in_album(&mut conn, b.id, order, 0, 1)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page.first().unwrap().visibility, Visibility::Restricted);
        assert_eq!(
            photos::list_in_album(&mut conn, b.id, order, 1, 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert!(
        photos::list_in_album(&mut conn, b.id, photos::PhotoOrder::Manual, 0, 0)
            .await
            .is_err()
    );
    assert!(
        photos::list_in_album(&mut conn, b.id, photos::PhotoOrder::Manual, 0, 201)
            .await
            .is_err()
    );
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,640,480,123)",Uuid::now_v7(),p.id.0,hash()).execute(&mut *conn).await.unwrap();
    let u = users::create(&mut conn, "viewer", &secret(), true)
        .await
        .unwrap();
    likes::like(&mut conn, p.id, likes::Actor::User(u.id))
        .await
        .unwrap();
    let f = flags::flag(&mut conn, p.id, likes::Actor::User(u.id))
        .await
        .unwrap();
    let new_original = original(a.id);
    photos::replace_original(&mut conn, p.id, &new_original, &metadata)
        .await
        .unwrap();
    let replaced = photos::get_by_short_id(&mut conn, &p.short_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.id, p.id);
    assert_eq!(replaced.original_hash, new_original.original_hash);
    assert_eq!(replaced.album_id, b.id);
    assert_eq!(
        likes::likes_of_photo(&mut conn, p.id).await.unwrap().len(),
        1
    );
    assert_eq!(
        flags::open_flags(&mut conn, 0)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id,
        f.id
    );
    let page = photos::list_in_album(&mut conn, b.id, photos::PhotoOrder::Manual, 0, 50)
        .await
        .unwrap();
    assert_eq!(
        page.iter().find(|row| row.id == p.id).unwrap().derivatives,
        json!([])
    );
    photos::soft_delete(&mut conn, second.id).await.unwrap();
    assert_eq!(
        photos::list_in_album(&mut conn, b.id, photos::PhotoOrder::Manual, 0, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    photos::restore(&mut conn, second.id).await.unwrap();
    assert_eq!(
        photos::list_in_album(&mut conn, b.id, photos::PhotoOrder::Manual, 0, 50)
            .await
            .unwrap()
            .len(),
        2
    );
    photos::soft_delete(&mut conn, duplicate.id).await.unwrap();
    assert_eq!(
        photos::purge_deleted_before(&mut conn, OffsetDateTime::now_utc() + Duration::seconds(1))
            .await
            .unwrap(),
        1
    );
    assert!(
        photos::get_by_short_id(&mut conn, &duplicate.short_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        photos::create(
            &mut conn,
            &photos::NewPhoto {
                original_hash: "../invalid".into(),
                ..original(a.id)
            }
        )
        .await
        .is_err()
    );
}

#[sqlx::test]
async fn users_groups_and_grants_all_operations(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let password = secret();
    let u = users::create(&mut conn, "Viewer", &password, true)
        .await
        .unwrap();
    assert!(u.must_change_password);
    assert!(!u.is_admin);
    assert_eq!(
        users::get_by_username(&mut conn, "viewer")
            .await
            .unwrap()
            .unwrap()
            .id,
        u.id
    );
    assert!(
        users::create(&mut conn, "VIEWER", &secret(), false)
            .await
            .is_err()
    );
    assert!(!format!("{u:?}").contains(secrecy::ExposeSecret::expose_secret(&password)));
    users::set_password(&mut conn, u.id, &secret())
        .await
        .unwrap();
    assert!(
        !users::get_by_id(&mut conn, u.id)
            .await
            .unwrap()
            .unwrap()
            .must_change_password
    );
    users::disable(&mut conn, u.id).await.unwrap();
    assert!(
        users::get_by_id(&mut conn, u.id)
            .await
            .unwrap()
            .unwrap()
            .disabled_at
            .is_some()
    );
    users::enable(&mut conn, u.id).await.unwrap();
    assert!(
        users::get_by_id(&mut conn, u.id)
            .await
            .unwrap()
            .unwrap()
            .disabled_at
            .is_none()
    );
    users::touch_login(&mut conn, u.id).await.unwrap();
    assert!(
        users::get_by_id(&mut conn, u.id)
            .await
            .unwrap()
            .unwrap()
            .last_login_at
            .is_some()
    );
    let g = groups::create(&mut conn, "Friends").await.unwrap();
    groups::rename(&mut conn, g.id, "Family").await.unwrap();
    groups::add_member(&mut conn, g.id, u.id).await.unwrap();
    assert_eq!(groups::add_member(&mut conn, g.id, u.id).await.unwrap(), 0);
    assert_eq!(groups::members(&mut conn, g.id).await.unwrap(), vec![u.id]);
    assert_eq!(
        groups::groups_of(&mut conn, u.id)
            .await
            .unwrap()
            .first()
            .unwrap()
            .name,
        "Family"
    );
    let a = album(&mut conn).await;
    let p = photo(&mut conn, a.id).await;
    grants::grant_album(&mut conn, a.id, Grant::User(u.id))
        .await
        .unwrap();
    grants::grant_album(&mut conn, a.id, Grant::Group(g.id))
        .await
        .unwrap();
    let album_grants = grants::grants_for_album(&mut conn, a.id).await.unwrap();
    assert_eq!(album_grants.len(), 2);
    assert!(
        matches!(Grant::from(album_grants.first().unwrap().clone()),Grant::User(id) if id==u.id)
    );
    grants::grant_photo(&mut conn, p.id, Grant::Group(g.id))
        .await
        .unwrap();
    assert!(
        matches!(Grant::from(grants::grants_for_photo(&mut conn,p.id).await.unwrap().remove(0)),Grant::Group(id) if id==g.id)
    );
    grants::revoke_album(&mut conn, a.id, Grant::User(u.id))
        .await
        .unwrap();
    assert_eq!(
        grants::grants_for_album(&mut conn, a.id)
            .await
            .unwrap()
            .len(),
        1
    );
    grants::revoke_photo(&mut conn, p.id, Grant::Group(g.id))
        .await
        .unwrap();
    assert!(
        grants::grants_for_photo(&mut conn, p.id)
            .await
            .unwrap()
            .is_empty()
    );
    groups::remove_member(&mut conn, g.id, u.id).await.unwrap();
    assert_eq!(groups::members(&mut conn, g.id).await.unwrap(), Vec::new());
}

#[sqlx::test]
async fn links_tags_likes_flags_settings_redirects_all_operations(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let u = users::create(&mut conn, "viewer", &secret(), false)
        .await
        .unwrap();
    let a = album(&mut conn).await;
    let p = photo(&mut conn, a.id).await;
    let token_hash = *Uuid::now_v7().as_bytes();
    let token_hash: [u8; 32] = [token_hash, *Uuid::now_v7().as_bytes()]
        .concat()
        .try_into()
        .unwrap();
    let link = share_links::create(
        &mut conn,
        &share_links::NewShareLink {
            token_hash,
            target: ShareTarget::Photo(p.id),
            passphrase_hash: Some(secret()),
            expires_at: None,
            flags: share_links::LinkFlags {
                view: true,
                download: true,
                like: true,
                flag: true,
            },
            created_by: u.id,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        share_links::get_by_token_hash(&mut conn, &token_hash)
            .await
            .unwrap()
            .unwrap()
            .id,
        link.id
    );
    assert!(matches!(ShareTarget::from(link.clone()),ShareTarget::Photo(id) if id==p.id));
    share_links::update_flags(&mut conn, link.id, share_links::LinkFlags::default())
        .await
        .unwrap();
    let expiry = OffsetDateTime::now_utc() - Duration::seconds(1);
    share_links::set_expiry(&mut conn, link.id, Some(expiry))
        .await
        .unwrap();
    let fetched = share_links::get_by_token_hash(&mut conn, &token_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetched.expires_at.unwrap().unix_timestamp(),
        expiry.unix_timestamp()
    );
    assert!(!fetched.allow_download);
    // Expired links remain available for caller-side expiry evaluation.
    let empty = share_links::stats(&mut conn, link.id).await.unwrap();
    assert_eq!(empty.opens, 0);
    assert!(empty.first_open.is_none());
    let visitor = Uuid::now_v7();
    for _ in 0..2 {
        share_links::record_open(
            &mut conn,
            link.id,
            visitor.as_bytes(),
            Some("CA"),
            Some("Ottawa"),
        )
        .await
        .unwrap();
    }
    let stats = share_links::stats(&mut conn, link.id).await.unwrap();
    assert_eq!(stats.opens, 2);
    assert_eq!(stats.unique_visitors, 1);
    assert_eq!(stats.countries, json!({"CA":2}));
    assert!(stats.first_open.is_some() && stats.last_open.is_some());
    let tag = tags::upsert_by_slug(
        &mut conn,
        &Slug::new("travel").unwrap(),
        "Travel",
        tags::TagSource::Manual,
    )
    .await
    .unwrap();
    assert_eq!(
        tags::upsert_by_slug(&mut conn, &tag.slug, "Journeys", tags::TagSource::Auto)
            .await
            .unwrap()
            .id,
        tag.id
    );
    tags::attach(&mut conn, p.id, tag.id, tags::TagSource::Auto, Some(0.8))
        .await
        .unwrap();
    assert_eq!(
        tags::tags_of(&mut conn, p.id)
            .await
            .unwrap()
            .first()
            .unwrap()
            .confidence,
        Some(0.8)
    );
    assert_eq!(
        tags::photos_with_tag(&mut conn, tag.id, 0)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id,
        p.id
    );
    assert!(
        tags::photos_with_tag(&mut conn, tag.id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    tags::detach(&mut conn, p.id, tag.id).await.unwrap();
    assert!(tags::tags_of(&mut conn, p.id).await.unwrap().is_empty());
    let user = likes::Actor::User(u.id);
    let shared = likes::Actor::ShareLink(link.id);
    likes::like(&mut conn, p.id, user).await.unwrap();
    assert_eq!(likes::like(&mut conn, p.id, user).await.unwrap(), 0);
    likes::like(&mut conn, p.id, shared).await.unwrap();
    assert_eq!(
        likes::likes_of_photo(&mut conn, p.id).await.unwrap().len(),
        2
    );
    assert_eq!(
        likes::liked_by_user(&mut conn, u.id, 0)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id,
        p.id
    );
    assert!(
        likes::liked_by_user(&mut conn, u.id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    likes::unlike(&mut conn, p.id, user).await.unwrap();
    likes::unlike(&mut conn, p.id, shared).await.unwrap();
    assert!(
        likes::likes_of_photo(&mut conn, p.id)
            .await
            .unwrap()
            .is_empty()
    );
    let f = flags::flag(&mut conn, p.id, user).await.unwrap();
    flags::flag(&mut conn, p.id, shared).await.unwrap();
    assert_eq!(flags::open_flags(&mut conn, 0).await.unwrap().len(), 2);
    flags::resolve_flag(&mut conn, f.id, flags::FlagResolution::Dismissed)
        .await
        .unwrap();
    assert_eq!(flags::open_flags(&mut conn, 0).await.unwrap().len(), 1);
    assert!(settings::get(&mut conn, "theme").await.unwrap().is_none());
    settings::set(&mut conn, "theme", &json!({"color":"blue"}))
        .await
        .unwrap();
    settings::set(&mut conn, "theme", &json!({"color":"green"}))
        .await
        .unwrap();
    assert_eq!(
        settings::get(&mut conn, "theme").await.unwrap(),
        Some(json!({"color":"green"}))
    );
    assert!(
        redirects::lookup(&mut conn, "/missing")
            .await
            .unwrap()
            .is_none()
    );
    redirects::insert(
        &mut conn,
        "/index.php?/category/1",
        ShareTarget::Album(a.id),
    )
    .await
    .unwrap();
    redirects::insert(
        &mut conn,
        "/index.php?/category/1",
        ShareTarget::Photo(p.id),
    )
    .await
    .unwrap();
    assert!(
        matches!(ShareTarget::from(redirects::lookup(&mut conn,"/index.php?/category/1").await.unwrap().unwrap()),ShareTarget::Photo(id) if id==p.id)
    );
    share_links::revoke(&mut conn, link.id).await.unwrap();
    assert!(
        share_links::get_by_token_hash(&mut conn, &token_hash)
            .await
            .unwrap()
            .is_none()
    );
    // Both invalid XOR states are rejected by the database, independently of Actor.
    for (user, link) in [(None, None), (Some(u.id.0), Some(link.id.0))] {
        let err = sqlx::query!(
            "INSERT INTO likes(id,photo_id,user_id,share_link_id) VALUES ($1,$2,$3,$4)",
            Uuid::now_v7(),
            p.id.0,
            user,
            link
        )
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
        let err = sqlx::query!(
            "INSERT INTO flags(id,photo_id,user_id,share_link_id) VALUES ($1,$2,$3,$4)",
            Uuid::now_v7(),
            p.id.0,
            user,
            link
        )
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
}

#[sqlx::test]
async fn jobs_all_operations_retries_stale_and_notifications(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen("jobs").await.unwrap();
    let j = jobs::enqueue(
        &mut conn,
        "ingest",
        &json!({"photo_id":PhotoId::new().to_string()}),
        1,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    assert_eq!(listener.recv().await.unwrap().channel(), "jobs");
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Pending, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        jobs::claim(&mut conn, "worker", &["other"])
            .await
            .unwrap()
            .is_none()
    );
    let (claimed, token) = jobs::claim(&mut conn, "worker", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, j.id);
    assert_eq!(claimed.attempts, 1);
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Running, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    jobs::fail(
        &mut conn,
        &token,
        "temporary decode failure",
        std::time::Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(
        jobs::claim(&mut conn, "worker", &["ingest"])
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query!(
        "UPDATE jobs SET run_after=now(),max_attempts=2 WHERE id=$1",
        j.id.0
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let (_, token) = jobs::claim(&mut conn, "worker", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    jobs::fail(
        &mut conn,
        &token,
        "terminal decode failure",
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Failed, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        jobs::claim(&mut conn, "worker", &["ingest"])
            .await
            .unwrap()
            .is_none()
    );
    let next = jobs::enqueue(
        &mut conn,
        "ingest",
        &json!({}),
        2,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    jobs::claim(&mut conn, "worker", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    jobs::release_stale(&mut conn, OffsetDateTime::now_utc() + Duration::seconds(1))
        .await
        .unwrap();
    let (reclaimed, token) = jobs::claim(&mut conn, "other-worker", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.id, next.id);
    assert!(jobs::complete(&mut conn, &token).await.unwrap());
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Complete, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Complete, 1)
            .await
            .unwrap(),
        []
    );
    let stale = jobs::enqueue(
        &mut conn,
        "ingest",
        &json!({}),
        0,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    sqlx::query!("UPDATE jobs SET max_attempts=1 WHERE id=$1", stale.id.0)
        .execute(&mut *conn)
        .await
        .unwrap();
    jobs::claim(&mut conn, "worker", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    jobs::release_stale(&mut conn, OffsetDateTime::now_utc() + Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(
        jobs::list(&mut conn, jobs::JobStatus::Failed, 0)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[sqlx::test]
async fn stale_workers_cannot_complete_or_fail_reclaimed_jobs(pool: PgPool) {
    let mut worker_a = pool.acquire().await.unwrap();
    let mut worker_b = pool.acquire().await.unwrap();
    // Reusing a worker name must still invalidate the old attempt's token.
    for replacement_worker in ["worker-b", "worker-a"] {
        let queued = jobs::enqueue(
            &mut worker_a,
            "ingest",
            &json!({}),
            0,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let (first, stale) = jobs::claim(&mut worker_a, "worker-a", &["ingest"])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, queued.id);
        assert_eq!(stale.attempt, 1);
        assert_eq!(
            jobs::release_stale(
                &mut worker_b,
                OffsetDateTime::now_utc() + Duration::seconds(1)
            )
            .await
            .unwrap(),
            1
        );
        // A released claim also has no right to complete or reschedule work.
        assert!(!jobs::complete(&mut worker_a, &stale).await.unwrap());
        assert!(
            !jobs::fail(
                &mut worker_a,
                &stale,
                "late error",
                std::time::Duration::ZERO
            )
            .await
            .unwrap()
        );
        let (second, current) = jobs::claim(&mut worker_b, replacement_worker, &["ingest"])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.id, queued.id);
        assert_eq!(current.worker_id, replacement_worker);
        assert_eq!(current.attempt, 2);
        // Check each ownership predicate independently as well as the real stale token.
        let wrong_worker = jobs::ClaimToken {
            worker_id: "different-worker".into(),
            ..current.clone()
        };
        let wrong_attempt = jobs::ClaimToken {
            attempt: stale.attempt,
            ..current.clone()
        };
        for invalid in [&stale, &wrong_worker, &wrong_attempt] {
            assert!(!jobs::complete(&mut worker_a, invalid).await.unwrap());
            assert_eq!(
                jobs::list(&mut worker_b, jobs::JobStatus::Running, 0)
                    .await
                    .unwrap(),
                vec![second.clone()]
            );
            assert!(
                !jobs::fail(
                    &mut worker_a,
                    invalid,
                    "late error",
                    std::time::Duration::from_secs(60)
                )
                .await
                .unwrap()
            );
            // Includes timestamps, retry schedule, error, attempt count and lock identity.
            assert_eq!(
                jobs::list(&mut worker_b, jobs::JobStatus::Running, 0)
                    .await
                    .unwrap(),
                vec![second.clone()]
            );
        }
        assert!(
            jobs::claim(&mut worker_a, "worker-c", &["ingest"])
                .await
                .unwrap()
                .is_none()
        );
        assert!(jobs::complete(&mut worker_b, &current).await.unwrap());
        assert!(!jobs::complete(&mut worker_a, &stale).await.unwrap());
        assert!(
            !jobs::fail(
                &mut worker_a,
                &stale,
                "late error",
                std::time::Duration::ZERO
            )
            .await
            .unwrap()
        );
    }
}

#[sqlx::test]
async fn two_claimers_ten_jobs_no_double_claim(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    for priority in 0..10 {
        jobs::enqueue(
            &mut conn,
            "ingest",
            &json!({}),
            priority,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    }

    let (a, b) = tokio::join!(claim_all(&pool, "worker-a"), claim_all(&pool, "worker-b"));
    let all: Vec<_> = a.into_iter().chain(b).collect();
    assert_eq!(all.len(), 10);
    assert_eq!(
        all.iter().collect::<std::collections::HashSet<_>>().len(),
        10
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn user_creation_and_lookup_share_username_normalization(pool: PgPool) {
    let mut c = pool.acquire().await.unwrap();
    let hash = secret();
    let user = users::create(&mut c, " \u{2003}e\u{301} \t", &hash, false)
        .await
        .unwrap();
    assert_eq!(user.username, "é");
    assert_eq!(
        users::get_by_username(&mut c, " e\u{301} ")
            .await
            .unwrap()
            .unwrap()
            .id,
        user.id
    );
    assert!(users::create(&mut c, "é", &hash, false).await.is_err());
    for invalid in [
        String::new(),
        " \t\u{2003}".into(),
        "x\0y".into(),
        "x".repeat(65),
    ] {
        assert!(matches!(
            users::create(&mut c, &invalid, &hash, false).await,
            Err(yhotos_db::DbError::InvalidInput)
        ));
    }
    let longest = "é".repeat(64);
    assert_eq!(
        users::create(&mut c, &longest, &hash, false)
            .await
            .unwrap()
            .username,
        longest
    );
}
