use super::{bindings, caps};
use crate::{AppState, error::HttpError, middleware::session::SessionIdentity, share::Links};
use axum::{
    Extension, Json,
    extract::{Path, State},
};
use yhotos_core::{ShortId, access::ShareTarget};
use yhotos_db::{
    likes::{self, Actor},
    photos::{self, PagePhoto, PageSort},
};

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
    photo: &PagePhoto,
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
                    ShareTarget::Photo(id) => id == photo.id,
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
    let photo = photos::page_for(&mut tx, &bindings(&identity, &links), &id, PageSort::Taken)
        .await?
        .ok_or(HttpError::NotFound)?
        .photo;
    let capabilities = caps(&photo, &identity, &links);
    let flag = matches!(action, Action::Flag);
    if !(if flag {
        capabilities.flag
    } else {
        capabilities.like
    }) {
        return Err(HttpError::NotFound);
    }
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
    let value = self::state(&mut tx, &photo, &identity, &links).await?;
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
