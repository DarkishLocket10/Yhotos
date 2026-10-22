#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::indexing_slicing
)]
//! Differential property test: the production page CTE against the pure access function.
use proptest::{
    prelude::*,
    test_runner::{Config, TestRunner},
};
use sqlx::PgPool;
use yhotos_core::{
    AlbumId, GroupId, PhotoId, UserId,
    access::{self, AlbumAccess, Grant, PhotoAccess, Principal, ShareTarget, Visibility},
};
use yhotos_db::access_sql;

#[sqlx::test]
async fn sql_visibility_matches_core_for_500_random_trees(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    // Temp shadows preserve production column types but allow album 'inherit',
    // which core supports although the current persisted schema disallows it.
    sqlx::query!("CREATE TEMP TABLE albums (LIKE public.albums INCLUDING DEFAULTS)")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!("CREATE TEMP TABLE photos (LIKE public.photos INCLUDING DEFAULTS)")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!("CREATE TEMP TABLE album_grants (LIKE public.album_grants INCLUDING DEFAULTS)")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!("CREATE TEMP TABLE photo_grants (LIKE public.photo_grants INCLUDING DEFAULTS)")
        .execute(&mut *conn)
        .await
        .unwrap();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let strategy=(prop::collection::vec((0_usize..20,0_u8..3,0_u8..4,0_u8..3,0_u8..4),1..10),0_u8..8,0_usize..20);
        let conn=tokio::sync::Mutex::new(conn);
        let mut runner=TestRunner::new(Config {cases:500,..Config::default()});
        runner.run(&strategy,|(nodes,kind,target)| {
            handle.block_on(async {
                let mut conn=conn.lock().await;
                sqlx::query!("TRUNCATE pg_temp.photos,pg_temp.albums,pg_temp.album_grants,pg_temp.photo_grants").execute(&mut **conn).await.unwrap();
                let user=UserId::new();let group=GroupId::new();let other=UserId::new();
                let ids:Vec<_>=nodes.iter().map(|_|AlbumId::new()).collect();
                let pids:Vec<_>=nodes.iter().map(|_|PhotoId::new()).collect();
                let grants=|kind|match kind {1=>vec![Grant::User(user)],2=>vec![Grant::Group(group)],3=>vec![Grant::User(other)],_=>vec![]};
                let visibility=|v|match v {0=>Visibility::Public,1=>Visibility::Restricted,_=>Visibility::Inherit};
                let album_grants:Vec<_>=nodes.iter().map(|n|grants(n.2)).collect();
                let photo_grants:Vec<_>=nodes.iter().map(|n|grants(n.4)).collect();
                for (index,node) in nodes.iter().enumerate() {
                    let parent=if index==0||node.0%3==0 {None}else{Some(ids[node.0%index].0)};
                    let vis=visibility(node.1);
                    sqlx::query!("INSERT INTO albums(id,parent_id,slug,title,visibility) VALUES ($1,$2,$3,'Property album',$4)",ids[index].0,parent,format!("album-{index}"),vis as _).execute(&mut **conn).await.unwrap();
                    let pv=visibility(node.3);
                    let short=yhotos_core::ShortId::from_photo_id(pids[index]).to_string();
                    let hash="0".repeat(64);
                    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,visibility) VALUES ($1,$2,$3,'Property photo',$4,'photo.jpeg',0,'image/jpeg','image',$5)",pids[index].0,short,ids[index].0,hash,pv as _).execute(&mut **conn).await.unwrap();
                    for grant in &album_grants[index] {
                        let (kind,id)=match grant{Grant::User(id)=>("user",id.0),Grant::Group(id)=>("group",id.0)};
                        sqlx::query!("INSERT INTO album_grants(id,album_id,principal_kind,principal_id) VALUES($1,$2,$3::text::principal_kind,$4)",uuid::Uuid::now_v7(),ids[index].0,kind,id).execute(&mut **conn).await.unwrap();
                    }
                    for grant in &photo_grants[index] {
                        let (kind,id)=match grant{Grant::User(id)=>("user",id.0),Grant::Group(id)=>("group",id.0)};
                        sqlx::query!("INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) VALUES($1,$2,$3::text::principal_kind,$4)",uuid::Uuid::now_v7(),pids[index].0,kind,id).execute(&mut **conn).await.unwrap();
                    }
                }
                let groups=[group];let at=target%nodes.len();
                let principal=match kind {0=>Principal::Anonymous,1=>Principal::User{id:user,groups:&[],is_admin:false},2=>Principal::User{id:user,groups:&groups,is_admin:false},3=>Principal::User{id:user,groups:&[],is_admin:true},4=>Principal::ShareLink{target:ShareTarget::Album(ids[at])},5=>Principal::ShareLink{target:ShareTarget::Photo(pids[at])},6=>Principal::ShareLink{target:ShareTarget::Album(AlbumId::new())},_=>Principal::ShareLink{target:ShareTarget::Photo(PhotoId::new())}};
                let bindings=access_sql::visible_photos(&principal);
                let row=yhotos_db::visible_page_query!(r#"SELECT ARRAY(SELECT id FROM visible_albums) AS "albums!", ARRAY(SELECT id FROM visible_photos) AS "photos!""#,bindings).fetch_one(&mut **conn).await.unwrap();
                for (index,node) in nodes.iter().enumerate() {
                    let mut chain=Vec::new();let mut current=index;
                    while current!=0&&nodes[current].0%3!=0 {
                        current=nodes[current].0%current;
                        chain.push(AlbumAccess{id:ids[current],visibility:visibility(nodes[current].1),grants:&album_grants[current],ancestors:&[]});
                    }
                    let album=AlbumAccess{id:ids[index],visibility:visibility(node.1),grants:&album_grants[index],ancestors:&chain};
                    let photo=PhotoAccess{id:pids[index],album:&album,visibility:visibility(node.3),grants:&photo_grants[index]};
                    prop_assert_eq!(row.albums.contains(&ids[index].0),access::can_view_album(&principal,&album));
                    prop_assert_eq!(row.photos.contains(&pids[index].0),access::can_view_photo(&principal,&photo));
                }
                Ok(())
            })
        }).unwrap();
    }).await.unwrap();
}
