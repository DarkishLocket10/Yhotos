#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::too_many_lines, clippy::indexing_slicing)]
//! Real PostgreSQL regressions for offline geocoding, calendar and map privacy.
use sqlx::PgPool;
use yhotos_core::{
    AlbumId, PhotoId, ShortId, UserId,
    access::{Principal, ShareTarget},
};
use yhotos_db::{
    calendar::{self, Selection},
    map::{self, Bounds},
    places,
};

async fn album(pool: &PgPool) -> AlbumId {
    let id = AlbumId::new();
    sqlx::query!(
        "INSERT INTO albums(id,slug,title) VALUES($1,'geo-fixture','Geo fixture')",
        id.0
    )
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn photo(
    pool: &PgPool,
    album: AlbumId,
    lat: Option<f64>,
    lon: Option<f64>,
    restricted: bool,
) -> PhotoId {
    let id = PhotoId::new();
    let short = ShortId::from_photo_id(id).to_string();
    let hash = "a".repeat(64);
    sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,gps_lat,gps_lon,visibility,taken_at,posted_at)
      VALUES($1,$2,$3,'Fixture',$4,'fixture.jpeg',1,'image/jpeg','image',$5,$6,
        CASE WHEN $7 THEN 'restricted'::visibility ELSE 'inherit'::visibility END,
        '2024-02-29 12:00:00+00','2024-02-01 00:00:00+00')"#,id.0,short,album.0,hash,lat,lon,restricted).execute(pool).await.unwrap();
    id
}
#[sqlx::test]
async fn offline_resolution_is_bounded_idempotent_and_handles_date_line(pool: PgPool) {
    let album = album(&pool).await;
    sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES(6167865,'Toronto','08','CA',43.70011,-79.4163,2600000),(1,'Dateline','01','FJ',0,-179.95,1000),(2,'Polar','01','NO',89.9,90,1000)").execute(&pool).await.unwrap();
    let toronto = photo(&pool, album, Some(43.7002), Some(-79.4164), false).await;
    let ocean = photo(&pool, album, Some(0.0), Some(0.0), false).await;
    let no_gps = photo(&pool, album, None, None, false).await;
    let date_line = photo(&pool, album, Some(0.0), Some(179.95), false).await;
    let pole = photo(&pool, album, Some(89.9), Some(-90.0), false).await;
    let mut conn = pool.acquire().await.unwrap();
    for id in [toronto, ocean, no_gps, date_line, pole] {
        places::reverse(&mut conn, id).await.unwrap();
    }
    let first = sqlx::query!("SELECT place_id FROM photos WHERE id=$1", toronto.0)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
        .place_id
        .unwrap();
    places::reverse(&mut conn, toronto).await.unwrap();
    let resolved=sqlx::query!("SELECT p.place_id,l.name FROM photos p LEFT JOIN places l ON l.id=p.place_id WHERE p.id=$1",toronto.0).fetch_one(&mut *conn).await.unwrap();
    assert_eq!(resolved.place_id, Some(first));
    assert_eq!(resolved.name.as_deref(), Some("Toronto"));
    for id in [ocean, no_gps] {
        assert!(
            sqlx::query!("SELECT place_id FROM photos WHERE id=$1", id.0)
                .fetch_one(&mut *conn)
                .await
                .unwrap()
                .place_id
                .is_none()
        );
    }
    for (id, name) in [(date_line, "Dateline"), (pole, "Polar")] {
        assert_eq!(
            sqlx::query!(
                "SELECT l.name FROM photos p JOIN places l ON l.id=p.place_id WHERE p.id=$1",
                id.0
            )
            .fetch_one(&mut *conn)
            .await
            .unwrap()
            .name,
            name
        );
    }
    assert_eq!(
        sqlx::query!("SELECT count(*) AS \"count!\" FROM places WHERE geonameid=6167865")
            .fetch_one(&mut *conn)
            .await
            .unwrap()
            .count,
        1
    );
    sqlx::query!(
        "UPDATE photos SET gps_lat=0,gps_lon=0 WHERE id=$1",
        toronto.0
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    places::reverse(&mut conn, toronto).await.unwrap();
    assert!(
        sqlx::query!("SELECT place_id FROM photos WHERE id=$1", toronto.0)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
            .place_id
            .is_none()
    );
}
#[sqlx::test]
async fn map_counts_and_fallback_exclude_restricted_locations(pool: PgPool) {
    let album = album(&pool).await;
    let public = photo(&pool, album, Some(43.7), Some(-79.4), false).await;
    let secret = photo(&pool, album, Some(43.7), Some(-79.4), true).await;
    for (id, hash) in [(public, "b".repeat(64)), (secret, "c".repeat(64))] {
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',$3,320,320,1)",uuid::Uuid::now_v7(),id.0,hash).execute(&pool).await.unwrap();
    }
    let place = uuid::Uuid::now_v7();
    sqlx::query!("INSERT INTO places(id,name,admin1,country,lat,lon) VALUES($1,'Toronto','Ontario','CA',43.7,-79.4)",place).execute(&pool).await.unwrap();
    sqlx::query!("UPDATE photos SET place_id=$1", place)
        .execute(&pool)
        .await
        .unwrap();
    let user = UserId::new();
    sqlx::query!(
        "INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) VALUES($1,$2,'user',$3)",
        uuid::Uuid::now_v7(),
        secret.0,
        user.0
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let bounds = Bounds::parse("-80,43,-79,44").unwrap();
    let clusters = map::clusters(&mut conn, &Principal::Anonymous, bounds, 1)
        .await
        .unwrap();
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].count, 1);
    assert_eq!(clusters[0].short_id, ShortId::from_photo_id(public));
    assert_eq!(clusters[0].thumb_hash, Some("b".repeat(64)));
    assert_eq!(
        map::places(&mut conn, &Principal::Anonymous).await.unwrap()[0].count,
        1
    );
    let user = Principal::User {
        id: user,
        groups: &[],
        is_admin: false,
    };
    assert_eq!(
        map::clusters(&mut conn, &user, bounds, 1).await.unwrap()[0].count,
        2
    );
    let share = Principal::ShareLink {
        target: ShareTarget::Photo(secret),
    };
    assert_eq!(
        map::clusters(&mut conn, &share, bounds, 1).await.unwrap()[0].short_id,
        ShortId::from_photo_id(secret)
    );
    assert_eq!(
        map::clusters(&mut conn, &share, bounds, 1).await.unwrap()[0].thumb_hash,
        Some("c".repeat(64))
    );
    sqlx::query!(
        "UPDATE albums SET visibility='restricted' WHERE id=$1",
        album.0
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    assert!(
        map::clusters(&mut conn, &Principal::Anonymous, bounds, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        map::places(&mut conn, &Principal::Anonymous)
            .await
            .unwrap()
            .is_empty()
    );
}
#[sqlx::test]
async fn map_feature_cap_preserves_low_zoom_counts_and_limits_individuals(pool: PgPool) {
    let album = album(&pool).await;
    let ids = (0..1100).map(|_| PhotoId::new()).collect::<Vec<_>>();
    let shorts = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect::<Vec<_>>();
    let ids = ids.iter().map(|id| id.0).collect::<Vec<_>>();
    sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,gps_lat,gps_lon)
      SELECT id,short,$3,'Map cap',$4,'photo.jpeg',1,'image/jpeg','image',-80+mod(n,160),-170+mod(n,340)
      FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY AS t(id,short,n)"#,&ids,&shorts,album.0,"a".repeat(64)).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let bounds = Bounds::parse("-180,-85,180,85").unwrap();
    let low = map::clusters(&mut conn, &Principal::Anonymous, bounds, 14)
        .await
        .unwrap();
    assert!(low.len() <= 484);
    assert_eq!(low.iter().map(|c| c.count).sum::<u64>(), 1100);
    let high = map::clusters(&mut conn, &Principal::Anonymous, bounds, 15)
        .await
        .unwrap();
    assert_eq!(high.len(), 500);
    assert!(high.iter().all(|c| c.count == 1));
}
#[sqlx::test]
async fn calendar_counts_covers_fallback_dates_and_pagination_are_visible(pool: PgPool) {
    let album = album(&pool).await;
    let public = photo(&pool, album, None, None, false).await;
    let posted = photo(&pool, album, None, None, false).await;
    let secret = photo(&pool, album, None, None, true).await;
    sqlx::query!("UPDATE photos SET taken_at=NULL WHERE id=$1", posted.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE photos SET taken_at='2023-01-01 00:00:00+00' WHERE id=$1",
        secret.0
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let root = calendar::page(
        &mut conn,
        &Principal::Anonymous,
        Selection::new(None, None, None).unwrap(),
        1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(root.total, 2);
    assert_eq!(root.buckets.len(), 1);
    assert_eq!(root.buckets[0].number, 2024);
    assert_eq!(root.buckets[0].posted_count, 1);
    assert_eq!(root.buckets[0].cover.id, public);
    let month = Selection::new(Some(2024), Some(2), None).unwrap();
    let page = calendar::page(&mut conn, &Principal::Anonymous, month, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.photos.len(), 2);
    assert_eq!(
        page.buckets.iter().map(|b| b.number).collect::<Vec<_>>(),
        [29, 1]
    );
    assert!(page.photos.iter().any(|p| p.photo.id == posted && p.posted));
    assert!(
        calendar::page(&mut conn, &Principal::Anonymous, month, 2)
            .await
            .unwrap()
            .is_none()
    );
    let day = Selection::new(Some(2024), Some(2), Some(29)).unwrap();
    assert_eq!(
        calendar::page(&mut conn, &Principal::Anonymous, day, 1)
            .await
            .unwrap()
            .unwrap()
            .total,
        1
    );
    let missing = Selection::new(Some(2023), None, None).unwrap();
    assert!(
        calendar::page(&mut conn, &Principal::Anonymous, missing, 1)
            .await
            .unwrap()
            .is_none()
    );
    let admin = Principal::User {
        id: UserId::new(),
        groups: &[],
        is_admin: true,
    };
    assert_eq!(
        calendar::page(&mut conn, &admin, missing, 1)
            .await
            .unwrap()
            .unwrap()
            .total,
        1
    );
}

#[sqlx::test]
async fn collocated_markers_and_chooser_preserve_every_visible_photo(pool: PgPool) {
    let album = album(&pool).await;
    let first = photo(&pool, album, Some(43.7), Some(-79.4), false).await;
    let second = photo(&pool, album, Some(43.7), Some(-79.4), false).await;
    let nearby = photo(&pool, album, Some(43.7), Some(-79.400_000_01), false).await;
    let secret = photo(&pool, album, Some(43.7), Some(-79.4), true).await;
    let mut conn = pool.acquire().await.unwrap();
    let bounds = Bounds::parse("-80,43,-79,44").unwrap();
    for zoom in [15, 20] {
        let clusters = map::clusters(&mut conn, &Principal::Anonymous, bounds, zoom)
            .await
            .unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters.iter().map(|c| c.count).sum::<u64>(), 3);
        let grouped = clusters.iter().find(|c| c.count == 2).unwrap();
        assert!(grouped.terminal);
        assert_eq!((grouped.lon, grouped.lat), (-79.4, 43.7));
        let page = map::point_photos(
            &mut conn,
            &Principal::Anonymous,
            grouped.lon,
            grouped.lat,
            1,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.photos.len(), 2);
        for id in [first, second] {
            assert!(
                page.photos
                    .iter()
                    .any(|p| p.short_id == ShortId::from_photo_id(id))
            );
        }
        for id in [nearby, secret] {
            assert!(
                !page
                    .photos
                    .iter()
                    .any(|p| p.short_id == ShortId::from_photo_id(id))
            );
        }
    }
    sqlx::query!("UPDATE photos SET deleted_at=now() WHERE id=$1", nearby.0)
        .execute(&mut *conn)
        .await
        .unwrap();
    let clusters = map::clusters(&mut conn, &Principal::Anonymous, bounds, 1)
        .await
        .unwrap();
    assert_eq!(clusters.len(), 1);
    assert!(!clusters[0].terminal); // Lower zooms still zoom towards the photo level.
    assert_eq!(clusters[0].count, 2);
}
