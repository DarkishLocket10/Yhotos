#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! Count actual rendition payload reads through the production page query.
use sqlx::PgPool;
use yhotos_core::{
    PhotoId, ShortId, Slug,
    access::{Principal, Visibility},
};
use yhotos_db::{
    albums,
    photos::{self, PageSort},
};

#[sqlx::test]
async fn only_selected_video_photos_and_covers_read_renditions(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("video-pagination").unwrap(),
        "Video pagination",
        Visibility::Public,
    )
    .await
    .unwrap();
    let ids = (0..180).map(|_| PhotoId::new()).collect::<Vec<_>>();
    let shorts = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect::<Vec<_>>();
    let raw = ids.iter().map(|id| id.0).collect::<Vec<_>>();
    sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,sort_order)
      SELECT id,short,$3,'Video',$4,'fixture.mov',1,'video/quicktime','video',n::int
      FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY t(id,short,n)"#,&raw,&shorts,album.id.0,"a".repeat(64)).execute(&mut *conn).await.unwrap();
    sqlx::query!(r#"INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms,av1_level)
      SELECT gen_random_uuid(),id,codec,$2,128,96,1,1200,CASE WHEN codec='av1' THEN 0 END
      FROM photos CROSS JOIN (VALUES ('av1'),('h264')) v(codec) WHERE album_id=$1"#,album.id.0,"b".repeat(64)).execute(&mut *conn).await.unwrap();
    // An explicit cover outside page one must still receive its video payload.
    let cover = raw.last().unwrap();
    sqlx::query!(
        "UPDATE albums SET cover_photo_id=$2 WHERE id=$1",
        album.id.0,
        cover
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query!("CREATE TEMP SEQUENCE video_payload_reads")
        .execute(&mut *conn)
        .await
        .unwrap();
    // The volatile duration expression records each rendition actually aggregated,
    // including accidental work in materialized CTEs consumed by a total count.
    sqlx::query!(
        r#"CREATE TEMP VIEW video_renditions AS
      SELECT id,created_at,photo_id,codec,hash,width,height,bytes,hls,av1_level,
        duration_ms + 0*nextval('pg_temp.video_payload_reads') AS duration_ms
      FROM public.video_renditions"#
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let path = [Slug::new("video-pagination").unwrap()];
    for page in [1, 3] {
        sqlx::query!(
            "SELECT setval($1::text::regclass,1,false)",
            "pg_temp.video_payload_reads"
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        let result = albums::page(
            &mut conn,
            &Principal::Anonymous,
            &path,
            page,
            PageSort::Manual,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.total, 180);
        assert_eq!(result.photos.len(), 60);
        assert!(result.photos.iter().all(|p| p.video.len() == 2));
        assert_eq!(result.album.cover.as_ref().unwrap().video.len(), 2);
        let reads = sqlx::query_scalar!(
            r#"SELECT currval($1::text::regclass) AS "reads!""#,
            "pg_temp.video_payload_reads"
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(reads, 122, "page {page} aggregated off-page renditions");
    }
    sqlx::query!(
        "SELECT setval($1::text::regclass,1,false)",
        "pg_temp.video_payload_reads"
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let photo = photos::page(
        &mut conn,
        &Principal::Anonymous,
        &ShortId::from_photo_id(*ids.first().unwrap()),
        PageSort::Manual,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(photo.photo.video.len(), 2);
    let reads = sqlx::query_scalar!(
        r#"SELECT currval($1::text::regclass) AS "reads!""#,
        "pg_temp.video_payload_reads"
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(reads, 2, "photo navigation aggregated other videos");
    // Stale rendition rows on still images must not cause even page-local reads.
    sqlx::query!(
        "UPDATE photos SET kind='image' WHERE album_id=$1 AND id<>$2",
        album.id.0,
        cover
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query!(
        "SELECT setval($1::text::regclass,1,false)",
        "pg_temp.video_payload_reads"
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let result = albums::page(&mut conn, &Principal::Anonymous, &path, 1, PageSort::Manual)
        .await
        .unwrap()
        .unwrap();
    assert!(result.photos.iter().all(|p| p.video.is_empty()));
    let reads = sqlx::query_scalar!(
        r#"SELECT currval($1::text::regclass) AS "reads!""#,
        "pg_temp.video_payload_reads"
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(reads, 2, "still images read stale rendition rows");
}
