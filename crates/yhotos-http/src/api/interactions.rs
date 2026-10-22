use super::bindings;
use crate::{AppState, error::HttpError, middleware::session::SessionIdentity, share::Links};
use axum::{
    Extension, Json,
    extract::{Path, State},
};
use yhotos_core::{ShortId, access::ShareTarget};
use yhotos_db::{
    likes::{self, Actor},
    photos::PagePhoto,
};

struct InteractionPhoto {
    id: yhotos_core::PhotoId,
    account_view: bool,
    photo_link_view: bool,
    shared_albums: Vec<yhotos_core::AlbumId>,
}
impl From<&PagePhoto> for InteractionPhoto {
    fn from(photo: &PagePhoto) -> Self {
        Self {
            id: photo.id,
            account_view: photo.account_view,
            photo_link_view: photo.photo_link_view,
            shared_albums: photo.shared_albums.clone(),
        }
    }
}

fn columns(
    actor: Actor,
) -> (
    Option<yhotos_core::UserId>,
    Option<yhotos_core::ShareLinkId>,
) {
    match actor {
        Actor::User(id) => (Some(id), None),
        Actor::ShareLink(id) => (None, Some(id)),
    }
}

#[derive(serde::Serialize)]
pub(super) struct Interaction {
    liked: bool,
    flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    like_count: Option<i64>,
}
fn actor(
    photo: &InteractionPhoto,
    identity: &SessionIdentity,
    links: &Links,
    flag: bool,
) -> Option<Actor> {
    // Keep attribution consistent when a user also carries a link: the link wins.
    links
        .0
        .iter()
        .find(|link| {
            link.caps.view
                && (if flag { link.caps.flag } else { link.caps.like })
                && match link.target {
                    ShareTarget::Photo(id) => photo.photo_link_view && id == photo.id,
                    ShareTarget::Album(id) => photo.shared_albums.contains(&id),
                }
        })
        .map(|link| Actor::ShareLink(link.link_id))
        .or(match identity {
            SessionIdentity::User { id, .. } if photo.account_view => Some(Actor::User(*id)),
            _ => None,
        })
}
pub(super) async fn state(
    conn: &mut sqlx::PgConnection,
    photo: &PagePhoto,
    identity: &SessionIdentity,
    links: &Links,
) -> Result<Interaction, HttpError> {
    interaction_state(conn, &photo.into(), identity, links).await
}
async fn interaction_state(
    conn: &mut sqlx::PgConnection,
    photo: &InteractionPhoto,
    identity: &SessionIdentity,
    links: &Links,
) -> Result<Interaction, HttpError> {
    let (lu, ll) = actor(photo, identity, links, false).map_or((None, None), columns);
    let (fu, fl) = actor(photo, identity, links, true).map_or((None, None), columns);
    let admin = matches!(identity, SessionIdentity::User { is_admin: true, .. });
    let row = sqlx::query!(r#"SELECT EXISTS(SELECT FROM likes WHERE photo_id=$1 AND (user_id=$2 OR share_link_id=$3)) AS "liked!", EXISTS(SELECT FROM flags WHERE photo_id=$1 AND (user_id=$4 OR share_link_id=$5)) AS "flagged!", CASE WHEN $6 THEN (SELECT count(*) FROM likes WHERE photo_id=$1) END AS like_count"#, photo.id as _, lu as _, ll as _, fu as _, fl as _, admin)
        .fetch_one(conn).await.map_err(|e| HttpError::Internal(e.into()))?;
    Ok(Interaction {
        liked: row.liked,
        flagged: row.flagged,
        like_count: row.like_count,
    })
}
// The gallery visibility rules applied only to one photo and its ancestor chain.
async fn visible_photo(
    conn: &mut sqlx::PgConnection,
    bindings: &yhotos_db::access_sql::Bindings,
    id: &ShortId,
) -> Result<Option<InteractionPhoto>, HttpError> {
    // Filter by the indexed short id before doing authorization work. Mutations
    // need no album ordering, neighbours, derivatives, EXIF or breadcrumbs.
    let row = sqlx::query!(
        r#"WITH RECURSIVE identity AS (
          SELECT $1::uuid AS user_id, $2::uuid[] AS groups, $3::bool AS admin,
                 $4::uuid[] AS shared_albums, $5::uuid[] AS shared_photos
        ), photo AS (
          SELECT p.id,p.album_id,p.visibility,
            (p.visibility <> 'restricted' OR EXISTS (SELECT FROM photo_grants g
              WHERE g.photo_id=p.id AND ((g.principal_kind='user' AND g.principal_id=i.user_id)
                OR (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) AS levels_pass
          FROM photos p CROSS JOIN identity i WHERE p.short_id=$6 AND p.deleted_at IS NULL
        ), chain AS (
          SELECT a.id,a.parent_id,a.visibility,ARRAY[a.id] AS visited,
            (a.visibility <> 'restricted' OR EXISTS (SELECT FROM album_grants g
              WHERE g.album_id=a.id AND ((g.principal_kind='user' AND g.principal_id=i.user_id)
                OR (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) AS levels_pass
          FROM albums a JOIN photo p ON p.album_id=a.id CROSS JOIN identity i
          WHERE a.deleted_at IS NULL
          UNION ALL
          SELECT a.id,a.parent_id,a.visibility,c.visited || a.id,
            (a.visibility <> 'restricted' OR EXISTS (SELECT FROM album_grants g
              WHERE g.album_id=a.id AND ((g.principal_kind='user' AND g.principal_id=i.user_id)
                OR (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) AS levels_pass
          FROM albums a JOIN chain c ON a.id=c.parent_id CROSS JOIN identity i
          WHERE a.deleted_at IS NULL AND NOT a.id=ANY(c.visited)
        ), scope AS (
          SELECT array_agg(id) AS ids,
            coalesce(array_agg(id) FILTER (WHERE visibility='restricted'),ARRAY[]::uuid[]) AS restricted_ids,
            bool_and(levels_pass) AS levels_pass, bool_or(parent_id IS NULL) AS complete
          FROM chain
        ), authorized AS (
          SELECT p.id, (i.admin OR (i.user_id IS NOT NULL AND a.levels_pass AND p.levels_pass)) AS account_view,
            ARRAY(SELECT target FROM unnest(i.shared_albums) target
              WHERE p.visibility <> 'restricted' AND target=ANY(a.ids)
                AND a.restricted_ids <@ ARRAY[target]) AS shared_albums,
            (i.user_id IS NOT NULL OR (cardinality(i.shared_albums)=0 AND cardinality(i.shared_photos)=0))
              AND a.levels_pass AND p.levels_pass AS ordinary_view,
            p.id=ANY(i.shared_photos) AND cardinality(a.restricted_ids)=0 AS photo_link_view,
            i.admin
          FROM photo p CROSS JOIN scope a CROSS JOIN identity i WHERE a.complete
        ) SELECT id AS "id: yhotos_core::PhotoId", account_view AS "account_view!", photo_link_view AS "photo_link_view!",
          shared_albums AS "shared_albums!" FROM authorized
          WHERE admin OR ordinary_view OR photo_link_view OR cardinality(shared_albums)>0"#,
        bindings.user,
        &bindings.groups,
        bindings.admin,
        &bindings.album,
        &bindings.photo,
        id.to_string()
    )
    .fetch_optional(conn)
    .await
    .map_err(|e| HttpError::Internal(e.into()))?
    ;
    Ok(row.map(|row| InteractionPhoto {
        id: row.id,
        account_view: row.account_view,
        photo_link_view: row.photo_link_view,
        shared_albums: row
            .shared_albums
            .into_iter()
            .map(yhotos_core::AlbumId)
            .collect(),
    }))
}

#[derive(Clone, Copy)]
enum Action {
    Like,
    Unlike,
    Flag,
}
async fn mutate(
    state: AppState,
    identity: SessionIdentity,
    links: Links,
    id: String,
    action: Action,
) -> Result<Json<Interaction>, HttpError> {
    let id = id.parse::<ShortId>().map_err(|_| HttpError::NotFound)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let bindings = bindings(&identity, &links);
    let photo = visible_photo(&mut tx, &bindings, &id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let flag = matches!(action, Action::Flag);
    let actor = actor(&photo, &identity, &links, flag).ok_or(HttpError::NotFound)?;
    match action {
        Action::Like => {
            likes::like(&mut tx, photo.id, actor).await?;
        }
        Action::Unlike => {
            likes::unlike(&mut tx, photo.id, actor).await?;
        }
        Action::Flag => {
            flag_once(&mut tx, photo.id, actor).await?;
        }
    }
    let value = interaction_state(&mut tx, &photo, &identity, &links).await?;
    tx.commit()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    Ok(Json(value))
}
pub(super) async fn like(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
) -> Result<Json<Interaction>, HttpError> {
    mutate(state, identity, links, id, Action::Like).await
}
pub(super) async fn unlike(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
) -> Result<Json<Interaction>, HttpError> {
    mutate(state, identity, links, id, Action::Unlike).await
}
pub(super) async fn flag(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
) -> Result<Json<Interaction>, HttpError> {
    mutate(state, identity, links, id, Action::Flag).await
}

/// The transaction lock makes the existence check atomic without UPDATE access to photos.
pub(crate) async fn flag_once(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    photo: yhotos_core::PhotoId,
    actor: Actor,
) -> Result<(), HttpError> {
    let key = format!("photo-flag:{photo}");
    sqlx::query!(
        r#"SELECT true AS "locked!" FROM pg_advisory_xact_lock(hashtextextended($1, 0))"#,
        key
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| HttpError::Internal(e.into()))?;
    let (user, link) = columns(actor);
    sqlx::query!("INSERT INTO flags(id,photo_id,user_id,share_link_id) SELECT $1,$2,$3,$4 WHERE NOT EXISTS(SELECT FROM flags WHERE photo_id=$2 AND (user_id=$3 OR share_link_id=$4))", uuid::Uuid::now_v7(), photo as _, user as _, link as _).execute(&mut **tx).await.map_err(|e| HttpError::Internal(e.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]
    use super::*;
    use yhotos_core::{
        Slug, UserId,
        access::{Grant, Principal, Visibility},
    };
    use yhotos_db::{access_sql, albums, grants, groups, photos, users};

    #[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
    async fn targeted_authorization_matches_gallery_visibility_and_attribution(pool: sqlx::PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let password = yhotos_auth::generate_temp_password();
        let hash = yhotos_auth::PasswordHasher::for_tests()
            .hash_password(&password)
            .unwrap();
        let user = users::create(&mut conn, "direct", &hash.as_str().into(), false)
            .await
            .unwrap()
            .id;
        let member = users::create(&mut conn, "member", &hash.as_str().into(), false)
            .await
            .unwrap()
            .id;
        let group = groups::create(&mut conn, "group").await.unwrap().id;
        let memberships = [group];
        let mut chain = Vec::new();
        let mut parent = None;
        for slug in ["root", "child", "leaf"] {
            let album = albums::create(
                &mut conn,
                parent,
                &Slug::new(slug).unwrap(),
                slug,
                Visibility::Public,
            )
            .await
            .unwrap();
            parent = Some(album.id);
            chain.push(album.id);
        }
        let photo = photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: chain[2],
                title: "Photo".into(),
                original_hash: "a".repeat(64),
                original_name: "photo.jpeg".into(),
                original_bytes: 1,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await
        .unwrap();
        let principals = [
            Principal::Anonymous,
            Principal::User {
                id: user,
                groups: &[],
                is_admin: false,
            },
            Principal::User {
                id: member,
                groups: &memberships,
                is_admin: false,
            },
            Principal::User {
                id: UserId::new(),
                groups: &[],
                is_admin: true,
            },
            Principal::ShareLink {
                target: ShareTarget::Album(chain[0]),
            },
            Principal::ShareLink {
                target: ShareTarget::Album(chain[1]),
            },
            Principal::ShareLink {
                target: ShareTarget::Album(chain[2]),
            },
            Principal::ShareLink {
                target: ShareTarget::Photo(photo.id),
            },
        ];
        let album_ids: Vec<_> = chain.iter().map(|id| id.0).collect();
        // Every restricted-level combination with no, alternating, or all grants.
        // Account, group, admin, photo/album links and account/link unions all
        // compare both visibility and the independent attribution fields.
        for visibility in 0..16 {
            let restricted: Vec<_> = chain
                .iter()
                .enumerate()
                .filter(|(i, _)| visibility & (1 << i) != 0)
                .map(|(_, id)| id.0)
                .collect();
            sqlx::query!("UPDATE albums SET visibility=CASE WHEN id=ANY($1) THEN 'restricted'::visibility ELSE 'public'::visibility END WHERE id=ANY($2)", &restricted, &album_ids).execute(&mut *conn).await.unwrap();
            photos::set_visibility(
                &mut conn,
                &[photo.id],
                if visibility & 8 == 0 {
                    Visibility::Public
                } else {
                    Visibility::Restricted
                },
            )
            .await
            .unwrap();
            for granted in [0, 5, 10, 15] {
                sqlx::query!(
                    "DELETE FROM album_grants WHERE album_id=ANY($1)",
                    &album_ids
                )
                .execute(&mut *conn)
                .await
                .unwrap();
                sqlx::query!("DELETE FROM photo_grants WHERE photo_id=$1", photo.id as _)
                    .execute(&mut *conn)
                    .await
                    .unwrap();
                for grant in [Grant::User(user), Grant::Group(group)] {
                    for (i, id) in chain.iter().enumerate() {
                        if granted & (1 << i) != 0 {
                            grants::grant_album(&mut conn, *id, grant).await.unwrap();
                        }
                    }
                    if granted & 8 != 0 {
                        grants::grant_photo(&mut conn, photo.id, grant)
                            .await
                            .unwrap();
                    }
                }
                for principal in &principals {
                    for union in [false, true] {
                        let mut bindings = access_sql::visible_photos(principal);
                        if union {
                            bindings = bindings.with_links([
                                ShareTarget::Photo(photo.id),
                                ShareTarget::Album(chain[1]),
                            ]);
                        }
                        let expected = photos::page_for(
                            &mut conn,
                            &bindings,
                            &photo.short_id,
                            photos::PageSort::Taken,
                        )
                        .await
                        .unwrap();
                        let actual = visible_photo(&mut conn, &bindings, &photo.short_id)
                            .await
                            .unwrap();
                        assert_eq!(
                            actual.is_some(),
                            expected.is_some(),
                            "visibility={visibility} grants={granted} principal={principal:?} union={union}"
                        );
                        if let (Some(actual), Some(expected)) = (actual, expected) {
                            assert_eq!(actual.id, expected.photo.id);
                            assert_eq!(actual.account_view, expected.photo.account_view);
                            assert_eq!(actual.shared_albums, expected.photo.shared_albums);
                        }
                    }
                }
            }
        }
        // Even admins must not authorize a chain truncated by a deleted ancestor.
        sqlx::query!(
            "UPDATE albums SET deleted_at=now() WHERE id=$1",
            chain[0] as _
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        let admin = access_sql::visible_photos(&principals[3]);
        assert!(
            visible_photo(&mut conn, &admin, &photo.short_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
