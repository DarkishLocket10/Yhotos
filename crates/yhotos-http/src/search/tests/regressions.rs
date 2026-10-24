//! UTC cutoffs and every browsable/capped search window have boundary coverage.
use super::*;
#[cfg(feature = "admin")]
use sqlx::Acquire;
use uuid::Uuid;

async fn photos(pool: &PgPool, count: usize) -> Vec<Uuid> {
    let mut ids: Vec<_> = (0..count).map(|_| PhotoId::new()).collect();
    ids.sort_unstable();
    let shorts: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    let ids: Vec<_> = ids.into_iter().map(|id| id.0).collect();
    sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,status,visibility,taken_at,posted_at)
        SELECT id,short_id,(SELECT id FROM albums WHERE slug='search-fixture'),'sunset',repeat('b',64),'fixture.jpg',1,'image/jpeg','image','ready','public',TIMESTAMPTZ '2026-01-01 00:00:00+00',TIMESTAMPTZ '2026-01-01 00:00:00+00'
        FROM unnest($1::uuid[],$2::text[]) f(id,short_id)"#,&ids,&shorts).execute(pool).await.unwrap();
    let vector = format!("[1,{}]", vec!["0"; 767].join(","));
    sqlx::query!("INSERT INTO embeddings(photo_id,model,vector) SELECT id,$2,$3::text::vector FROM unnest($1::uuid[]) id",&ids,search::EMBED_MODEL,vector).execute(pool).await.unwrap();
    sqlx::query!("INSERT INTO ocr_text(id,photo_id,text,model) SELECT id,id,'sunset','fixture' FROM unnest($1::uuid[]) id",&ids).execute(pool).await.unwrap();
    ids
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn date_filters_use_utc_in_every_list_and_session(pool: PgPool) {
    let _ = fixture(&pool).await;
    let instants = [
        "2025-12-31T23:59:59Z",
        "2026-01-01T00:00:00Z",
        "2026-01-01T04:59:59Z",
        "2026-01-01T05:00:00Z",
        "2026-01-31T23:59:59Z",
        "2026-02-01T00:00:00Z",
        "2026-03-07T23:59:59Z",
        "2026-03-08T00:00:00Z",
        "2026-10-31T23:59:59Z",
        "2026-11-01T00:00:00Z",
    ];
    let ids = photos(&pool, instants.len()).await;
    let times: Vec<_> = instants
        .iter()
        .map(|s| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .to_offset(time::UtcOffset::UTC)
        })
        .collect();
    // Exercise taken_at and the posted_at fallback at every midnight boundary.
    sqlx::query!("UPDATE photos p SET posted_at=f.at,taken_at=CASE WHEN f.n%2=0 THEN NULL ELSE f.at END FROM unnest($1::uuid[],$2::timestamptz[]) WITH ORDINALITY f(id,at,n) WHERE p.id=f.id",&ids,&times).execute(&pool).await.unwrap();
    sqlx::query!(
        "UPDATE photos SET status='pending' WHERE id=$1",
        Uuid::from_u128(1)
    )
    .execute(&pool)
    .await
    .unwrap();
    let cases = [
        ("before", "2026-01-01", vec![0]),
        ("after", "2026-01-01", (1..10).collect()),
        ("year", "2025", vec![0]),
        ("year", "2026", (1..10).collect()),
        ("month", "1", (1..5).collect()),
        ("month", "2", vec![5]),
        ("before", "2026-02-01", (0..5).collect()),
        ("after", "2026-02-01", (5..10).collect()),
        ("before", "2026-03-08", (0..7).collect()),
        ("after", "2026-03-08", (7..10).collect()),
        ("before", "2026-11-01", (0..9).collect()),
        ("after", "2026-11-01", vec![9]),
    ];
    let principal = access_sql::visible_photos(&Principal::Anonymous);
    let vector = format!("[1,{}]", vec!["0"; 767].join(","));
    for zone in ["UTC", "America/Toronto"] {
        // Set the actual connection session, not a global/pool default that a
        // later checkout might miss. No application SET TimeZone masks the SQL.
        let options = pool
            .connect_options()
            .as_ref()
            .clone()
            .options([("jit", "off")]);
        let zoned = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            // SQLx initializes new connections to UTC after startup options.
            // Override that session setting after initialization and assert it.
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    sqlx::query_scalar!("SELECT set_config('TimeZone',$1,false)", zone)
                        .fetch_one(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar!("SHOW TimeZone")
                .fetch_one(&zoned)
                .await
                .unwrap()
                .as_deref(),
            Some(zone)
        );
        for (key, value, expected) in &cases {
            let filters = [search::Filter {
                key: (*key).into(),
                value: (*value).into(),
            }];
            for list in [List::Semantic, List::Text, List::Structured] {
                let text = if matches!(list, List::Structured) {
                    ""
                } else {
                    "sunset"
                };
                let rows = search::candidates(
                    &zoned,
                    &principal,
                    &filters,
                    text,
                    Some(&vector),
                    None,
                    list,
                )
                .await
                .unwrap();
                for row in &rows {
                    let index = ids.iter().position(|id| *id == row.photo.id.0).unwrap();
                    let expected =
                        (index % 2 == 0).then(|| instants[index].replace('Z', ".000000Z"));
                    assert_eq!(row.photo.taken_at, expected, "{zone} serialized date");
                }
                let mut actual: Vec<_> = rows.into_iter().map(|r| r.photo.id.0).collect();
                actual.sort_unstable();
                let expected: Vec<_> = expected.iter().map(|i| ids[*i]).collect();
                assert_eq!(actual, expected, "{zone} {key}:{value}");
            }
        }
        zoned.close().await;
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn search_candidates_are_deliberately_capped_and_every_result_page_is_reachable(
    pool: PgPool,
) {
    let state = fixture(&pool).await;
    let ids = photos(&pool, 201).await;
    let principal = access_sql::visible_photos(&Principal::Anonymous);
    let vector = format!("[1,{}]", vec!["0"; 767].join(","));
    for list in [List::Semantic, List::Text, List::Structured] {
        let text = if matches!(list, List::Structured) {
            ""
        } else {
            "sunset"
        };
        assert_eq!(
            search::candidates(
                &state.pool,
                &principal,
                &[],
                text,
                Some(&vector),
                None,
                list
            )
            .await
            .unwrap()
            .len(),
            200
        );
    }
    // Also cover the exact-distance branch's <=200 boundary.
    sqlx::query!(
        "UPDATE photos SET status='pending' WHERE id=ANY($1)",
        &ids[..2]
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        search::candidates(
            &state.pool,
            &principal,
            &[],
            "sunset",
            Some(&vector),
            None,
            List::Semantic
        )
        .await
        .unwrap()
        .len(),
        200
    );
    let app = crate::public_router(state);
    let mut seen = HashSet::new();
    for (page, count) in [(1, 60), (2, 60), (3, 60), (4, 20)] {
        let response = app
            .clone()
            .oneshot(request(&format!("/api/v1/search?page={page}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(json["total"], 200);
        let rows = json["photos"].as_array().unwrap();
        assert_eq!(rows.len(), count);
        for row in rows {
            assert!(seen.insert(row["id"].as_str().unwrap().to_owned()));
        }
        let response = app
            .clone()
            .oneshot(request(&format!("/search?page={page}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(response).await;
        assert_eq!(html.contains("rel=\"prev\""), page > 1);
        assert_eq!(html.contains("rel=\"next\""), page < 4);
    }
    assert_eq!(seen.len(), 200);
    for path in [
        "/api/v1/search?page=5",
        "/search?page=5",
        "/search?page=0",
        "/search?page=11",
    ] {
        assert_eq!(
            app.clone().oneshot(request(path)).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn suggestions_have_a_small_cap_and_refining_reaches_every_kind(pool: PgPool) {
    let state = fixture(&pool).await;
    let principal = access_sql::visible_photos(&Principal::Anonymous);
    let names: Vec<_> = (0..21).map(|i| format!("Prefix{i:02}")).collect();
    let ids: Vec<_> = names.iter().map(|_| Uuid::now_v7()).collect();
    let photo_ids = photos(&pool, names.len()).await;
    sqlx::query!("INSERT INTO tags(id,slug,name,source) SELECT id,lower(name),name,'manual' FROM unnest($1::uuid[],$2::text[]) f(id,name)",&ids,&names).execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO photo_tags(id,photo_id,tag_id,source) SELECT tag,photo,tag,'manual' FROM unnest($1::uuid[],$2::uuid[]) f(tag,photo)",&ids,&photo_ids).execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO places(id,name,country,lat,lon) SELECT id,name,'CA',0,0 FROM unnest($1::uuid[],$2::text[]) f(id,name)",&ids,&names).execute(&pool).await.unwrap();
    sqlx::query!("UPDATE photos p SET camera_model=f.name,lens=f.name,place_id=f.place FROM unnest($1::uuid[],$2::text[],$3::uuid[]) f(id,name,place) WHERE p.id=f.id",&photo_ids,&names,&ids).execute(&pool).await.unwrap();
    assert_eq!(
        search::suggest(&state.pool, &principal, "Prefix")
            .await
            .unwrap()
            .len(),
        20
    );
    let refined = search::suggest(&state.pool, &principal, "Prefix20")
        .await
        .unwrap();
    assert_eq!(refined.len(), 4);
    assert_eq!(
        refined
            .iter()
            .map(|s| s.kind.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from(["tag", "place", "camera", "lens"])
    );
    assert!(
        refined
            .iter()
            .all(|s| s.value == "Prefix20" && s.count == 1)
    );
}

#[cfg(feature = "admin")]
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn every_cluster_merge_target_and_person_photo_is_reachable(pool: PgPool) {
    use yhotos_db::faces;
    let _ = fixture(&pool).await;
    let ids: Vec<_> = (0..205).map(|_| Uuid::now_v7()).collect();
    let names: Vec<_> = (0..205)
        .map(|i| (i < 200).then(|| format!("Person{i:03}")))
        .collect();
    sqlx::query!(
        "INSERT INTO persons(id,name) SELECT id,name FROM unnest($1::uuid[],$2::text[]) f(id,name)",
        &ids,
        &names as _
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("INSERT INTO faces(id,photo_id,person_id,bbox,quality,model) SELECT id,$2,id,ARRAY[0.1,0.1,0.5,0.5],0.9,'fixture' FROM unnest($1::uuid[]) id",&ids,Uuid::from_u128(1)).execute(&pool).await.unwrap();
    let extra_photos = photos(&pool, 120).await;
    sqlx::query!("INSERT INTO faces(id,photo_id,person_id,bbox,quality,model) SELECT id,id,$2,ARRAY[0.1,0.1,0.5,0.5],0.8,'fixture' FROM unnest($1::uuid[]) id",&extra_photos,ids[0]).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    for exclude in [None, Some(ids[0])] {
        let mut seen = HashSet::new();
        for (page, count) in [
            (1, 60),
            (2, 60),
            (3, 60),
            (4, if exclude.is_some() { 24 } else { 25 }),
        ] {
            let result = faces::people(&mut conn, page, exclude).await.unwrap();
            assert_eq!(result.total, if exclude.is_some() { 204 } else { 205 });
            assert_eq!(result.people.len(), count);
            for person in result.people {
                assert!(seen.insert(person.id));
                assert!(person.samples.len() <= 3);
            }
        }
        assert_eq!(seen.len(), if exclude.is_some() { 204 } else { 205 });
        assert_eq!(seen.contains(&ids[0]), exclude.is_none());
        assert!(seen.contains(&ids[204]));
        let empty = faces::people(&mut conn, 5, exclude).await.unwrap();
        assert!(empty.people.is_empty());
        assert!(empty.total >= 204);
        assert!(faces::people(&mut conn, 0, exclude).await.is_err());
    }
    for zone in ["UTC", "America/Toronto"] {
        sqlx::query_scalar!("SELECT set_config('TimeZone',$1,false)", zone)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        for named in [false, true] {
            let mut seen = HashSet::new();
            for (page, count) in [(1, 60), (2, 60), (3, 1), (4, 0)] {
                let result = if named {
                    faces::named_photos(&mut conn, "Person000", page).await
                } else {
                    faces::photos(&mut conn, ids[0], page).await
                }
                .unwrap();
                assert_eq!(result.total, 121);
                assert_eq!(result.photos.len(), count);
                for photo in result.photos {
                    let expected =
                        (photo.id.0 != Uuid::from_u128(1)).then_some("2026-01-01T00:00:00.000000Z");
                    assert_eq!(
                        photo.taken_at.as_deref(),
                        expected,
                        "{zone} serialized date"
                    );
                    assert!(seen.insert(photo.id));
                }
            }
            assert_eq!(seen.len(), 121);
        }
    }
    // A source on the first page can merge into an unnamed target past 200.
    let mut tx = conn.begin().await.unwrap();
    faces::merge(&mut tx, ids[0], ids[204]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        faces::photos(&mut conn, ids[204], 3).await.unwrap().total,
        121
    );
    assert!(
        faces::update(&mut conn, ids[204], "Last person", true)
            .await
            .unwrap()
    );
    assert_eq!(faces::people(&mut conn, 1, None).await.unwrap().total, 203);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn tag_repository_pages_cross_each_fifty_photo_boundary(pool: PgPool) {
    use yhotos_db::tags::{self, TagSource};
    let _ = fixture(&pool).await;
    let ids = photos(&pool, 101).await;
    let mut conn = pool.acquire().await.unwrap();
    let tag = tags::upsert_by_slug(
        &mut conn,
        &yhotos_core::Slug::new("pagination").unwrap(),
        "Pagination",
        TagSource::Manual,
    )
    .await
    .unwrap();
    sqlx::query!("INSERT INTO photo_tags(id,photo_id,tag_id,source) SELECT id,id,$2,'manual' FROM unnest($1::uuid[]) id",&ids,tag.id.0).execute(&mut *conn).await.unwrap();
    let mut seen = HashSet::new();
    for (page, count) in [(0, 50), (1, 50), (2, 1), (3, 0)] {
        let rows = tags::photos_with_tag(&mut conn, tag.id, page)
            .await
            .unwrap();
        assert_eq!(rows.len(), count);
        for row in rows {
            assert!(seen.insert(row.id));
        }
    }
    assert_eq!(seen.len(), 101);
}

#[test]
fn fused_window_caps_three_lists_at_two_hundred_each() {
    let lists = (0..4).map(|list| (1..=201).map(|i| candidate(list * 1000 + i)).collect());
    let rows = fuse(lists);
    assert_eq!(rows.len(), 600);
    assert!(
        !rows
            .iter()
            .any(|row| row.candidate.photo.id.0.as_u128() % 1000 == 201)
    );
}
