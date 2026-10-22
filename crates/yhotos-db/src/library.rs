//! Transactional administrative library operations. All inputs are validated before writes;
//! callers must supply an authenticated administrator, never an identity from JSON.
#![allow(clippy::too_many_lines, clippy::missing_errors_doc)]

use crate::{DbError, PgConnection, albums, audit, jobs, photos, redirects};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Connection, PgPool};
use uuid::Uuid;
use yhotos_core::{
    AlbumId, PhotoId, ShortId, Slug, UserId,
    access::{ShareTarget, Visibility},
};

/// Closed administrative actions, without credentials or bearer tokens.
#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[allow(missing_docs)]
pub enum Command {
    AlbumCreate {
        title: String,
        parent_id: Option<Uuid>,
    },
    AlbumEdit {
        id: Uuid,
        title: String,
        description: String,
        visibility: String,
        manual_order: bool,
        cover_photo_id: Option<Uuid>,
        grants: Vec<Grant>,
    },
    AlbumMove {
        id: Uuid,
        direction: Direction,
        parent_id: Option<Uuid>,
        before_id: Option<Uuid>,
    },
    Bulk {
        ids: Vec<Uuid>,
        action: BulkAction,
    },
    PhotoEdit {
        id: Uuid,
        title: String,
        description: String,
        taken_at: Option<String>,
        place_id: Option<Uuid>,
        gps_lat: Option<f64>,
        gps_lon: Option<f64>,
        visibility: String,
        tags: Vec<String>,
        grants: Vec<Grant>,
    },
    Trash {
        kind: Target,
        id: Uuid,
    },
    Restore {
        kind: Target,
        id: Uuid,
    },
    Purge {
        kind: Target,
        id: Uuid,
        confirmation: String,
    },
    FlagResolve {
        id: Uuid,
        resolution: Resolution,
        visibility: Option<String>,
    },
    IngestRetry {
        id: Uuid,
    },
    GroupSave {
        id: Option<Uuid>,
        name: String,
        members: Vec<Uuid>,
    },
    UserState {
        id: Uuid,
        disabled: bool,
        delete: bool,
        confirmation: Option<String>,
    },
    UserGroups {
        id: Uuid,
        groups: Vec<Uuid>,
    },
    SessionRevoke {
        id: Uuid,
    },
    Job {
        id: Uuid,
        retry: bool,
    },
}
/// User and group identities are checked against their tables before grants are written.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// Either `user` or `group`.
    pub kind: String,
    /// Existing principal identity.
    pub id: Uuid,
}
/// Reordering supports pointer and keyboard actions through the same transaction.
#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Direction {
    Up,
    Down,
    Indent,
    Outdent,
    Move,
}
/// Bulk photo operations use one validated, distinct list of at most 1,000 ids.
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(missing_docs)]
pub enum BulkAction {
    Move {
        album_id: Uuid,
    },
    Copy {
        album_id: Uuid,
    },
    Visibility {
        visibility: String,
        grants: Vec<Grant>,
    },
    Tags {
        add: Vec<String>,
        remove: Vec<String>,
    },
    Pattern {
        title: String,
        description: String,
    },
    Reorder {
        album_id: Uuid,
    },
    Delete,
}
/// Only albums and photos can enter library trash.
#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
#[allow(missing_docs)]
pub enum Target {
    Album,
    Photo,
}
/// Every flag resolution is an explicit, auditable outcome.
#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
#[allow(missing_docs)]
pub enum Resolution {
    Replaced,
    Reedited,
    Removed,
    Visibility,
    Dismissed,
}

/// A committed mutation and its durable edge invalidation batch.
pub struct Change {
    /// Safe response data (never contains credentials).
    pub value: Value,
    /// Sorted, deduplicated tags, also persisted for retry after edge failure.
    pub tags: Vec<String>,
    /// Outbox identity, cleared only after successful purge.
    pub purge_id: Uuid,
}
fn valid_text(value: &str, max: usize) -> Result<(), DbError> {
    if value.len() > max || value.contains('\0') {
        return Err(DbError::InvalidInput);
    }
    Ok(())
}
fn visibility(value: &str, album: bool) -> Result<Visibility, DbError> {
    match value {
        "public" => Ok(Visibility::Public),
        "restricted" => Ok(Visibility::Restricted),
        "inherit" if !album => Ok(Visibility::Inherit),
        _ => Err(DbError::InvalidInput),
    }
}
fn ids_valid(ids: &[Uuid]) -> Result<(), DbError> {
    if ids.is_empty()
        || ids.len() > 1000
        || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len()
    {
        return Err(DbError::InvalidInput);
    }
    Ok(())
}
async fn grants_valid(c: &mut PgConnection, grants: &[Grant]) -> Result<(), DbError> {
    if grants.len() > 1000 {
        return Err(DbError::InvalidInput);
    }
    let users: Vec<_> = grants
        .iter()
        .filter(|g| g.kind == "user")
        .map(|g| g.id)
        .collect();
    let groups: Vec<_> = grants
        .iter()
        .filter(|g| g.kind == "group")
        .map(|g| g.id)
        .collect();
    if users.len() + groups.len() != grants.len() {
        return Err(DbError::InvalidInput);
    }
    let count = sqlx::query_scalar!(r#"SELECT (SELECT count(*) FROM users WHERE id=ANY($1) AND deleted_at IS NULL)+(SELECT count(*) FROM groups WHERE id=ANY($2)) AS "count!""#, &users, &groups).fetch_one(c).await?;
    if usize::try_from(count).ok() != Some(grants.len()) {
        return Err(DbError::InvalidInput);
    }
    Ok(())
}
async fn photo_grants(c: &mut PgConnection, ids: &[Uuid], grants: &[Grant]) -> Result<(), DbError> {
    grants_valid(c, grants).await?;
    sqlx::query!("DELETE FROM photo_grants WHERE photo_id=ANY($1)", ids)
        .execute(&mut *c)
        .await?;
    let value = serde_json::to_value(grants).map_err(|_| DbError::InvalidInput)?;
    sqlx::query!("INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) SELECT yhotos_uuid_v7(),p.id,g.kind::principal_kind,g.id FROM unnest($1::uuid[]) p(id) CROSS JOIN jsonb_to_recordset($2) g(kind text,id uuid)", ids, value).execute(c).await?;
    Ok(())
}
async fn photo_state(c: &mut PgConnection, ids: &[Uuid]) -> Result<Value, DbError> {
    // Freeze the exact fields being audited against concurrent ingest publication.
    sqlx::query!(
        "SELECT id FROM photos WHERE id=ANY($1) ORDER BY id FOR UPDATE",
        ids
    )
    .fetch_all(&mut *c)
    .await?;
    Ok(sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(to_jsonb(p)||jsonb_build_object('grants',(SELECT coalesce(jsonb_agg(jsonb_build_object('kind',g.principal_kind,'id',g.principal_id)),'[]') FROM photo_grants g WHERE g.photo_id=p.id),'tags',(SELECT coalesce(jsonb_agg(t.name),'[]') FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id WHERE pt.photo_id=p.id)) ORDER BY p.id),'[]') AS "state!" FROM photos p WHERE p.id=ANY($1)"#, ids).fetch_one(c).await?)
}
pub(crate) async fn album_state(c: &mut PgConnection) -> Result<Value, DbError> {
    Ok(sqlx::query_scalar!(r#"WITH RECURSIVE tree AS (
       SELECT a.*,a.slug::text AS path,ARRAY[a.id] AS visited,
         ARRAY[lpad((a.sort_order::bigint+2147483648)::text,10,'0')||':'||a.id::text] AS sort_path
       FROM albums a WHERE parent_id IS NULL
       UNION ALL SELECT a.*,t.path||'/'||a.slug::text,t.visited||a.id,
         t.sort_path||(lpad((a.sort_order::bigint+2147483648)::text,10,'0')||':'||a.id::text)
       FROM albums a JOIN tree t ON a.parent_id=t.id WHERE NOT a.id=ANY(t.visited)
    ) SELECT coalesce(jsonb_agg(to_jsonb(t)-'visited'-'sort_path'||jsonb_build_object('grants',(SELECT coalesce(jsonb_agg(jsonb_build_object('kind',g.principal_kind,'id',g.principal_id)),'[]') FROM album_grants g WHERE g.album_id=t.id)) ORDER BY t.sort_path),'[]') AS "state!" FROM tree t"#).fetch_one(c).await?)
}
fn state_tags(state: &Value, tags: &mut Vec<String>) {
    if let Some(rows) = state.as_array() {
        for row in rows {
            for (field, prefix) in [
                ("id", "photo"),
                ("album_id", "album"),
                ("place_id", "place"),
            ] {
                if let Some(id) = row.get(field).and_then(Value::as_str) {
                    tags.push(format!("{prefix}:{id}"));
                }
            }
        }
    }
}
/// Includes every aggregate page tag whenever public content or access can change.
#[must_use]
pub fn aggregate_tags() -> Vec<String> {
    ["visibility", "map", "calendar", "places"]
        .map(str::to_owned)
        .to_vec()
}
async fn keep_redirects(c: &mut PgConnection, before: &Value) -> Result<(), DbError> {
    if let Some(rows) = before.as_array() {
        for row in rows {
            if let (Some(id), Some(path)) = (
                row.get("id").and_then(Value::as_str),
                row.get("path").and_then(Value::as_str),
            ) {
                redirects::insert(
                    c,
                    &format!("/a/{path}"),
                    ShareTarget::Album(AlbumId(id.parse().map_err(|_| DbError::InvalidInput)?)),
                )
                .await?;
            }
        }
    }
    Ok(())
}
async fn live_album(c: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    if albums::get_by_id(c, AlbumId(id))
        .await?
        .is_none_or(|a| a.deleted_at.is_some())
    {
        return Err(DbError::InvalidInput);
    }
    Ok(())
}
/// Executes and audits an entire action atomically, including the purge outbox.
/// Rejected members roll back every write, even when late in a bulk operation.
pub async fn execute(pool: &PgPool, actor: UserId, command: &Command) -> Result<Change, DbError> {
    let mut c = pool.acquire().await?;
    execute_in(&mut c, actor, command).await
}
/// Composes an atomic action with a caller transaction using a savepoint.
/// This has the same validation, audit, and outbox guarantees as `execute`.
pub async fn execute_in(
    c: &mut PgConnection,
    actor: UserId,
    command: &Command,
) -> Result<Change, DbError> {
    let mut tx = c.begin().await?;
    // Shared tree lock also serializes subtree moves with trash/restore and bulk edits.
    sqlx::query!("SELECT pg_advisory_xact_lock(20)")
        .execute(&mut *tx)
        .await?;
    let mut tags = aggregate_tags();
    let mut before = json!(null);
    let mut after = json!(null);
    let mut result = json!({});
    let mut target = None;
    let detail = serde_json::to_value(command).map_err(|_| DbError::InvalidInput)?;
    let action = detail
        .get("op")
        .and_then(Value::as_str)
        .ok_or(DbError::InvalidInput)?;
    match command {
        Command::AlbumCreate { title, parent_id } => {
            valid_text(title, 512)?;
            if title.trim().is_empty() {
                return Err(DbError::InvalidInput);
            }
            if let Some(id) = parent_id {
                live_album(&mut tx, *id).await?;
            }
            let a = albums::create(
                &mut tx,
                parent_id.map(AlbumId),
                &Slug::from_title(title),
                title,
                Visibility::Public,
            )
            .await?;
            target = Some(a.id.0);
            result = json!({"id":a.id});
            after = album_state(&mut tx).await?;
        }
        Command::AlbumEdit {
            id,
            title,
            description,
            visibility: vis,
            manual_order,
            cover_photo_id,
            grants,
        } => {
            before = album_state(&mut tx).await?;
            live_album(&mut tx, *id).await?;
            valid_text(title, 512)?;
            valid_text(description, 32768)?;
            if title.trim().is_empty() {
                return Err(DbError::InvalidInput);
            }
            grants_valid(&mut tx, grants).await?;
            if let Some(cover) = cover_photo_id {
                let p = photos::get_by_id(&mut tx, PhotoId(*cover))
                    .await?
                    .ok_or(DbError::InvalidInput)?;
                if p.album_id.0 != *id || p.deleted_at.is_some() {
                    return Err(DbError::InvalidInput);
                }
            }
            albums::update(
                &mut tx,
                AlbumId(*id),
                &Slug::from_title(title),
                title,
                description,
                visibility(vis, true)?,
                *manual_order,
            )
            .await?;
            albums::set_cover(&mut tx, AlbumId(*id), cover_photo_id.map(PhotoId)).await?;
            sqlx::query!("DELETE FROM album_grants WHERE album_id=$1", id)
                .execute(&mut *tx)
                .await?;
            let grants = serde_json::to_value(grants).map_err(|_| DbError::InvalidInput)?;
            sqlx::query!("INSERT INTO album_grants(id,album_id,principal_kind,principal_id) SELECT yhotos_uuid_v7(),$1,g.kind::principal_kind,g.id FROM jsonb_to_recordset($2) g(kind text,id uuid)",id,grants).execute(&mut *tx).await?;
            keep_redirects(&mut tx, &before).await?;
            after = album_state(&mut tx).await?;
            target = Some(*id);
        }
        Command::AlbumMove {
            id,
            direction,
            parent_id,
            before_id,
        } => {
            before = album_state(&mut tx).await?;
            move_album(&mut tx, *id, *direction, *parent_id, *before_id).await?;
            keep_redirects(&mut tx, &before).await?;
            after = album_state(&mut tx).await?;
            target = Some(*id);
        }
        Command::Bulk { ids, action } => {
            ids_valid(ids)?;
            before = photo_state(&mut tx, ids).await?;
            if before.as_array().map(Vec::len) != Some(ids.len())
                || before.as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|r| r.get("deleted_at").is_some_and(|v| !v.is_null()))
                })
            {
                return Err(DbError::InvalidInput);
            }
            state_tags(&before, &mut tags);
            result = bulk(&mut tx, ids, action).await?;
            let after_ids = if let Some(copied) = result.get("ids").and_then(Value::as_array) {
                copied
                    .iter()
                    .map(|id| {
                        id.as_str()
                            .ok_or(DbError::InvalidInput)?
                            .parse::<Uuid>()
                            .map_err(|_| DbError::InvalidInput)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                ids.clone()
            };
            after = photo_state(&mut tx, &after_ids).await?;
            state_tags(&after, &mut tags);
            if let Some(id) = result.get("album_id").and_then(Value::as_str) {
                tags.push(format!("album:{id}"));
            }
        }
        Command::PhotoEdit {
            id,
            title,
            description,
            taken_at,
            place_id,
            gps_lat,
            gps_lon,
            visibility: vis,
            tags: names,
            grants,
        } => {
            before = photo_state(&mut tx, &[*id]).await?;
            if before.as_array().map(Vec::len) != Some(1) {
                return Err(DbError::InvalidInput);
            }
            valid_text(title, 512)?;
            valid_text(description, 32768)?;
            let at = taken_at
                .as_ref()
                .map(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .map_err(|_| DbError::InvalidInput)
                })
                .transpose()?;
            if gps_lat.is_some() != gps_lon.is_some()
                || gps_lat.is_some_and(|n| !n.is_finite() || !(-90.0..=90.0).contains(&n))
                || gps_lon.is_some_and(|n| !n.is_finite() || !(-180.0..=180.0).contains(&n))
            {
                return Err(DbError::InvalidInput);
            }
            sqlx::query!("UPDATE photos SET title=$2,description=$3,taken_at=$4,taken_at_override=$4,taken_at_local=$4 AT TIME ZONE 'UTC',place_id=$5,gps_lat=$6,gps_lon=$7,gps_overridden=true,visibility=$8 WHERE id=$1",id,title,description,at,*place_id,*gps_lat,*gps_lon,visibility(vis,false)? as _).execute(&mut *tx).await?;
            photo_grants(&mut tx, &[*id], grants).await?;
            sqlx::query!("DELETE FROM photo_tags WHERE photo_id=$1", id)
                .execute(&mut *tx)
                .await?;
            edit_tags(&mut tx, &[*id], names, &[]).await?;
            after = photo_state(&mut tx, &[*id]).await?;
            state_tags(&before, &mut tags);
            state_tags(&after, &mut tags);
            target = Some(*id);
        }
        Command::Trash { kind, id }
        | Command::Restore { kind, id }
        | Command::Purge { kind, id, .. } => {
            let restore = matches!(command, Command::Restore { .. });
            let purge = if let Command::Purge { confirmation, .. } = command {
                if confirmation != "PURGE" {
                    return Err(DbError::InvalidInput);
                }
                true
            } else {
                false
            };
            let affected = trash(&mut tx, *kind, *id, restore, purge).await?;
            before = affected;
            state_tags(&before, &mut tags);
            target = Some(*id);
            if matches!(kind, Target::Album) {
                tags.push(format!("album:{id}"));
            }
            after = json!({"restored":restore,"purged":purge});
        }
        Command::FlagResolve {
            id,
            resolution,
            visibility: vis,
        } => {
            before = photo_state(&mut tx, &[*id]).await?;
            state_tags(&before, &mut tags);
            match resolution {
                Resolution::Removed => {
                    photos::soft_delete(&mut tx, PhotoId(*id)).await?;
                }
                Resolution::Visibility => {
                    let v = visibility(vis.as_deref().ok_or(DbError::InvalidInput)?, false)?;
                    photos::set_visibility(&mut tx, &[PhotoId(*id)], v).await?;
                }
                Resolution::Replaced => return Err(DbError::InvalidInput), // only a completed replacement may resolve as replaced
                _ => {}
            }
            let r = serde_json::to_value(resolution).map_err(|_| DbError::InvalidInput)?;
            let resolution = r.as_str().ok_or(DbError::InvalidInput)?;
            before = json!({"photo":before,"flags":sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(to_jsonb(f)),'[]') AS "flags!" FROM flags f WHERE photo_id=$1 AND resolved_at IS NULL"#,id).fetch_one(&mut *tx).await?});
            let changed=sqlx::query!("UPDATE flags SET resolution=$2::text::flag_resolution,resolved_at=now() WHERE photo_id=$1 AND resolved_at IS NULL",id,resolution).execute(&mut *tx).await?.rows_affected();
            if changed == 0 {
                return Err(DbError::InvalidInput);
            }
            after = json!({"photo":photo_state(&mut tx,&[*id]).await?,"resolution":resolution,"resolved":changed});
            target = Some(*id);
        }
        Command::IngestRetry { id } => {
            before = photo_state(&mut tx, &[*id]).await?;
            retry_ingest(&mut tx, *id).await?;
            after = photo_state(&mut tx, &[*id]).await?;
            state_tags(&before, &mut tags);
            target = Some(*id);
        }
        Command::GroupSave { id, name, members } => {
            valid_text(name, 128)?;
            if name.trim().is_empty() || members.len() > 1000 {
                return Err(DbError::InvalidInput);
            }
            let group = id.unwrap_or_else(Uuid::now_v7);
            target = Some(group);
            before = group_state(&mut tx, group).await?;
            if id.is_some() && before.is_null() {
                return Err(DbError::InvalidInput);
            }
            if id.is_some() {
                crate::groups::rename(&mut tx, yhotos_core::GroupId(group), name).await?;
            } else {
                sqlx::query!("INSERT INTO groups(id,name) VALUES($1,$2)", group, name)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query!("DELETE FROM user_groups WHERE group_id=$1", group)
                .execute(&mut *tx)
                .await?;
            sqlx::query!("INSERT INTO user_groups(id,user_id,group_id) SELECT yhotos_uuid_v7(),id,$2 FROM unnest($1::uuid[]) u(id)",members,group).execute(&mut *tx).await?;
            after = group_state(&mut tx, group).await?;
            result = json!({"id":group});
        }
        Command::UserGroups { id, groups } => {
            if groups.len() > 1000 {
                return Err(DbError::InvalidInput);
            }
            crate::users::lock_auth(&mut tx, UserId(*id)).await?;
            before = user_state(&mut tx, *id).await?;
            if before.is_null() {
                return Err(DbError::InvalidInput);
            }
            sqlx::query!("DELETE FROM user_groups WHERE user_id=$1", id)
                .execute(&mut *tx)
                .await?;
            sqlx::query!("INSERT INTO user_groups(id,user_id,group_id) SELECT yhotos_uuid_v7(),$1,id FROM unnest($2::uuid[]) g(id)",id,groups).execute(&mut *tx).await?;
            after = user_state(&mut tx, *id).await?;
            target = Some(*id);
        }
        Command::UserState {
            id,
            disabled,
            delete,
            confirmation,
        } => {
            if *id == actor.0 {
                return Err(DbError::InvalidInput);
            }
            crate::users::lock_auth(&mut tx, UserId(*id)).await?;
            before = user_state(&mut tx, *id).await?;
            if before.is_null() || (*delete && confirmation.as_deref() != Some("DELETE")) {
                return Err(DbError::InvalidInput);
            }
            let changed=sqlx::query!("UPDATE users SET disabled_at=CASE WHEN $2 OR $3 THEN now() ELSE NULL END,deleted_at=CASE WHEN $3 THEN now() ELSE deleted_at END WHERE id=$1 AND NOT is_admin AND (deleted_at IS NULL OR $3)",id,disabled,delete).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(DbError::InvalidInput);
            }
            if *disabled || *delete {
                sqlx::query!("DELETE FROM sessions WHERE user_id=$1", id)
                    .execute(&mut *tx)
                    .await?;
            }
            after = user_state(&mut tx, *id).await?;
            target = Some(*id);
        }
        Command::SessionRevoke { id } => {
            before=sqlx::query_scalar!(r#"SELECT jsonb_build_object('id',id,'user_id',user_id,'admin',admin) AS "session!" FROM sessions WHERE id=$1"#,id).fetch_optional(&mut *tx).await?.ok_or(DbError::InvalidInput)?;
            sqlx::query!("DELETE FROM sessions WHERE id=$1", id)
                .execute(&mut *tx)
                .await?;
            target = Some(*id);
        }
        Command::Job { id, retry } => {
            before=sqlx::query_scalar!(r#"SELECT jsonb_build_object('id',id,'kind',kind,'attempts',attempts,'finished_at',finished_at,'error',error) AS "job!" FROM jobs WHERE id=$1 AND locked_at IS NULL FOR UPDATE"#,id).fetch_optional(&mut *tx).await?.ok_or(DbError::InvalidInput)?;
            if *retry {
                sqlx::query!("UPDATE jobs SET attempts=0,finished_at=NULL,error=NULL,run_after=now() WHERE id=$1",id).execute(&mut *tx).await?;
                sqlx::query!("SELECT pg_notify('jobs','')")
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query!("DELETE FROM jobs WHERE id=$1", id)
                    .execute(&mut *tx)
                    .await?;
            }
            after = json!({"retry":retry});
            target = Some(*id);
        }
    }
    if matches!(
        command,
        Command::AlbumCreate { .. }
            | Command::AlbumEdit { .. }
            | Command::AlbumMove { .. }
            | Command::GroupSave { .. }
            | Command::UserGroups { .. }
            | Command::UserState { .. }
            | Command::Trash {
                kind: Target::Album,
                ..
            }
            | Command::Restore {
                kind: Target::Album,
                ..
            }
            | Command::Purge {
                kind: Target::Album,
                ..
            }
    ) {
        tags.extend(access_tags(&mut tx).await?);
    }
    for s in [&before, &after] {
        if let Some(rows) = s.as_array() {
            for row in rows {
                if let Some(id) = row.get("id").and_then(Value::as_str) {
                    tags.push(format!("album:{id}"));
                }
            }
        }
    }
    audit::write_target(
        &mut tx,
        Some(actor),
        action,
        "library",
        target,
        &json!({"action":detail,"before":before,"after":after,"result":result}),
    )
    .await?;
    let change = outbox(&mut tx, result, tags).await?;
    tx.commit().await?;
    Ok(change)
}
async fn group_state(c: &mut PgConnection, id: Uuid) -> Result<Value, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT to_jsonb(g)||jsonb_build_object('members',(SELECT coalesce(jsonb_agg(user_id),'[]') FROM user_groups WHERE group_id=g.id)) AS "state!" FROM groups g WHERE id=$1"#,id).fetch_optional(c).await?.unwrap_or(Value::Null))
}
/// Safe account snapshot excludes password hashes, MFA material, and session tokens.
pub async fn user_state(c: &mut PgConnection, id: Uuid) -> Result<Value, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT jsonb_build_object('id',id,'username',username,'disabled_at',disabled_at,'deleted_at',deleted_at,'must_change_password',must_change_password,'groups',(SELECT coalesce(jsonb_agg(group_id),'[]') FROM user_groups WHERE user_id=u.id)) AS "state!" FROM users u WHERE id=$1"#,id).fetch_optional(c).await?.unwrap_or(Value::Null))
}
async fn move_album(
    c: &mut PgConnection,
    id: Uuid,
    direction: Direction,
    parent: Option<Uuid>,
    before: Option<Uuid>,
) -> Result<(), DbError> {
    let row = albums::get_by_id(c, AlbumId(id))
        .await?
        .filter(|a| a.deleted_at.is_none())
        .ok_or(DbError::InvalidInput)?;
    let mut siblings = albums::children(c, row.parent_id).await?;
    siblings.retain(|a| a.deleted_at.is_none());
    let index = siblings
        .iter()
        .position(|a| a.id.0 == id)
        .ok_or(DbError::InvalidInput)?;
    let mut parent = parent;
    match direction {
        Direction::Up | Direction::Down => {
            let other = if matches!(direction, Direction::Up) {
                index.checked_sub(1)
            } else {
                index.checked_add(1).filter(|i| *i < siblings.len())
            };
            if let Some(other) = other {
                siblings.swap(index, other);
            }
            let ids: Vec<_> = siblings.iter().map(|a| a.id).collect();
            albums::reorder(c, row.parent_id, &ids).await?;
            return Ok(());
        }
        Direction::Indent => {
            parent = Some(
                siblings
                    .get(index.checked_sub(1).ok_or(DbError::InvalidInput)?)
                    .ok_or(DbError::InvalidInput)?
                    .id
                    .0,
            );
        }
        Direction::Outdent => {
            parent = albums::get_by_id(c, row.parent_id.ok_or(DbError::InvalidInput)?)
                .await?
                .ok_or(DbError::InvalidInput)?
                .parent_id
                .map(|a| a.0);
        }
        Direction::Move => {}
    }
    if let Some(parent) = parent {
        live_album(c, parent).await?;
        if parent == id
            || albums::ancestors(c, AlbumId(parent))
                .await?
                .iter()
                .any(|a| a.id.0 == id)
        {
            return Err(DbError::InvalidInput);
        }
    }
    sqlx::query!("UPDATE albums SET parent_id=$2 WHERE id=$1", id, parent)
        .execute(&mut *c)
        .await?;
    let mut children: Vec<_> = albums::children(c, parent.map(AlbumId))
        .await?
        .into_iter()
        .filter(|a| a.deleted_at.is_none() && a.id.0 != id)
        .map(|a| a.id)
        .collect();
    let at = before
        .map(|id| {
            children
                .iter()
                .position(|a| a.0 == id)
                .ok_or(DbError::InvalidInput)
        })
        .transpose()?
        .unwrap_or(children.len());
    children.insert(at, AlbumId(id));
    albums::reorder(c, parent.map(AlbumId), &children).await?;
    Ok(())
}
async fn bulk(c: &mut PgConnection, ids: &[Uuid], action: &BulkAction) -> Result<Value, DbError> {
    match action {
        BulkAction::Move { album_id } => {
            live_album(c, *album_id).await?;
            sqlx::query!(
                "UPDATE photos SET album_id=$2 WHERE id=ANY($1)",
                ids,
                album_id
            )
            .execute(c)
            .await?;
            Ok(json!({"album_id":album_id}))
        }
        BulkAction::Copy { album_id } => {
            live_album(c, *album_id).await?;
            let copied: Vec<_> = ids.iter().map(|_| PhotoId::new()).collect();
            let new_ids: Vec<_> = copied.iter().map(|p| p.0).collect();
            let shorts: Vec<_> = copied
                .iter()
                .map(|p| ShortId::from_photo_id(*p).to_string())
                .collect();
            sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,description,taken_at,posted_at,sort_order,visibility,original_hash,original_name,original_bytes,mime,kind,width,height,orientation,is_hdr,has_gain_map,icc_name,camera_make,camera_model,lens,focal_mm,aperture,shutter,iso,gps_lat,gps_lon,place_id,dominant_colors,placeholder,status,failure_reason,derivative_plan,taken_at_local,original_orientation,generation,taken_at_override,gps_overridden) SELECT m.target,m.short,$4,p.title,p.description,p.taken_at,p.posted_at,p.sort_order,p.visibility,p.original_hash,p.original_name,p.original_bytes,p.mime,p.kind,p.width,p.height,p.orientation,p.is_hdr,p.has_gain_map,p.icc_name,p.camera_make,p.camera_model,p.lens,p.focal_mm,p.aperture,p.shutter,p.iso,p.gps_lat,p.gps_lon,p.place_id,p.dominant_colors,p.placeholder,p.status,p.failure_reason,p.derivative_plan,p.taken_at_local,p.original_orientation,p.generation,p.taken_at_override,p.gps_overridden FROM photos p JOIN unnest($1::uuid[],$2::uuid[],$3::text[]) m(source,target,short) ON p.id=m.source",ids,&new_ids,&shorts,album_id).execute(&mut *c).await?;
            sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) SELECT yhotos_uuid_v7(),m.target,d.variant,d.format,d.hdr,d.hash,d.width,d.height,d.bytes FROM derivatives d JOIN unnest($1::uuid[],$2::uuid[]) m(source,target) ON d.photo_id=m.source",ids,&new_ids).execute(&mut *c).await?;
            sqlx::query!("INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms,hls) SELECT yhotos_uuid_v7(),m.target,d.codec,d.hash,d.width,d.height,d.bytes,d.duration_ms,d.hls FROM video_renditions d JOIN unnest($1::uuid[],$2::uuid[]) m(source,target) ON d.photo_id=m.source",ids,&new_ids).execute(&mut *c).await?;
            sqlx::query!("INSERT INTO photo_tags(id,photo_id,tag_id,confidence,source) SELECT yhotos_uuid_v7(),m.target,pt.tag_id,pt.confidence,pt.source FROM photo_tags pt JOIN unnest($1::uuid[],$2::uuid[]) m(source,target) ON pt.photo_id=m.source",ids,&new_ids).execute(&mut *c).await?;
            sqlx::query!("INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) SELECT yhotos_uuid_v7(),m.target,g.principal_kind,g.principal_id FROM photo_grants g JOIN unnest($1::uuid[],$2::uuid[]) m(source,target) ON g.photo_id=m.source",ids,&new_ids).execute(&mut *c).await?;
            sqlx::query!("WITH pending AS (UPDATE photos SET status='pending',generation=generation+1 WHERE id=ANY($1) AND status IN ('pending','processing') RETURNING id,original_hash,generation) INSERT INTO jobs(id,kind,payload,max_attempts) SELECT yhotos_uuid_v7(),'ingest',jsonb_build_object('photo_id',id,'original_hash',original_hash,'generation',generation),3 FROM pending",&new_ids).execute(&mut *c).await?;
            sqlx::query!("SELECT pg_notify('jobs','')")
                .execute(c)
                .await?;
            Ok(json!({"ids":new_ids,"album_id":album_id}))
        }
        BulkAction::Visibility {
            visibility: vis,
            grants,
        } => {
            sqlx::query!(
                "UPDATE photos SET visibility=$2 WHERE id=ANY($1)",
                ids,
                visibility(vis, false)? as _
            )
            .execute(&mut *c)
            .await?;
            photo_grants(c, ids, grants).await?;
            Ok(json!({}))
        }
        BulkAction::Tags { add, remove } => {
            edit_tags(c, ids, add, remove).await?;
            Ok(json!({}))
        }
        BulkAction::Pattern { title, description } => {
            valid_text(title, 512)?;
            valid_text(description, 32768)?;
            sqlx::query!("UPDATE photos p SET title=replace(replace($2,'{album}',a.title),'{n}',o.n::text),description=replace(replace($3,'{album}',a.title),'{n}',o.n::text) FROM unnest($1::uuid[]) WITH ORDINALITY o(id,n),albums a WHERE p.id=o.id AND a.id=p.album_id",ids,title,description).execute(c).await?;
            Ok(json!({}))
        }
        BulkAction::Reorder { album_id } => {
            let album = albums::get_by_id(c, AlbumId(*album_id))
                .await?
                .ok_or(DbError::InvalidInput)?;
            if !album.manual_order {
                return Err(DbError::InvalidInput);
            }
            let count=sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM photos WHERE album_id=$1 AND deleted_at IS NULL"#,album_id).fetch_one(&mut *c).await?;
            if usize::try_from(count).ok() != Some(ids.len()) {
                return Err(DbError::InvalidInput);
            }
            let changed=sqlx::query!("UPDATE photos p SET sort_order=(o.n-1)::int FROM unnest($1::uuid[]) WITH ORDINALITY o(id,n) WHERE p.id=o.id AND p.album_id=$2",ids,album_id).execute(c).await?.rows_affected();
            if usize::try_from(changed).ok() != Some(ids.len()) {
                return Err(DbError::InvalidInput);
            }
            Ok(json!({}))
        }
        BulkAction::Delete => {
            sqlx::query!("UPDATE photos SET deleted_at=now() WHERE id=ANY($1)", ids)
                .execute(c)
                .await?;
            Ok(json!({}))
        }
    }
}
async fn edit_tags(
    c: &mut PgConnection,
    ids: &[Uuid],
    add: &[String],
    remove: &[String],
) -> Result<(), DbError> {
    if add.len() > 100 || remove.len() > 100 {
        return Err(DbError::InvalidInput);
    }
    for name in remove {
        valid_text(name, 128)?;
    }
    sqlx::query!("DELETE FROM photo_tags pt USING tags t WHERE pt.tag_id=t.id AND pt.photo_id=ANY($1) AND t.name=ANY($2)",ids,remove).execute(&mut *c).await?;
    for name in add {
        valid_text(name, 128)?;
        if name.trim().is_empty() {
            return Err(DbError::InvalidInput);
        }
        let slug = Slug::from_title(name);
        let tag=sqlx::query_scalar!("INSERT INTO tags(id,slug,name,source) VALUES($1,$2,$3,'manual') ON CONFLICT(slug) DO UPDATE SET slug=tags.slug RETURNING id",Uuid::now_v7(),slug as _,name).fetch_one(&mut *c).await?;
        sqlx::query!("INSERT INTO photo_tags(id,photo_id,tag_id,source) SELECT yhotos_uuid_v7(),id,$2,'manual' FROM unnest($1::uuid[]) p(id) ON CONFLICT(photo_id,tag_id) DO NOTHING",ids,tag).execute(&mut *c).await?;
    }
    Ok(())
}
async fn trash(
    c: &mut PgConnection,
    kind: Target,
    id: Uuid,
    restore: bool,
    purge: bool,
) -> Result<Value, DbError> {
    let (photos, albums) = match kind {
        Target::Photo => (vec![id], Vec::new()),
        Target::Album => {
            let ids=sqlx::query_scalar!(r#"WITH RECURSIVE tree AS (SELECT id FROM albums WHERE id=$1 UNION SELECT a.id FROM albums a JOIN tree t ON a.parent_id=t.id) SELECT id AS "id!" FROM tree"#,id).fetch_all(&mut *c).await?;
            if ids.is_empty() {
                return Err(DbError::InvalidInput);
            }
            let photos = sqlx::query_scalar!("SELECT id FROM photos WHERE album_id=ANY($1)", &ids)
                .fetch_all(&mut *c)
                .await?;
            (photos, ids)
        }
    };
    let before = photo_state(c, &photos).await?;
    if matches!(kind, Target::Photo) && before.as_array().map(Vec::len) != Some(1) {
        return Err(DbError::InvalidInput);
    }
    let deleted = match kind {
        Target::Album => {
            albums::get_by_id(c, AlbumId(id))
                .await?
                .ok_or(DbError::InvalidInput)?
                .deleted_at
        }
        Target::Photo => {
            photos::get_by_id(c, PhotoId(id))
                .await?
                .ok_or(DbError::InvalidInput)?
                .deleted_at
        }
    };
    if restore && deleted.is_none() {
        return Err(DbError::InvalidInput);
    }
    if purge {
        let live=sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM photos WHERE id=ANY($1) AND deleted_at IS NULL) OR EXISTS(SELECT 1 FROM albums WHERE id=ANY($2) AND deleted_at IS NULL) AS "live!""#,&photos,&albums).fetch_one(&mut *c).await?;
        if live {
            return Err(DbError::InvalidInput);
        }
        sqlx::query!("INSERT INTO original_gc SELECT DISTINCT original_hash FROM photos WHERE id=ANY($1) ON CONFLICT DO NOTHING",&photos).execute(&mut *c).await?;
        sqlx::query!("DELETE FROM photos WHERE id=ANY($1)", &photos)
            .execute(&mut *c)
            .await?;
        sqlx::query!("DELETE FROM albums WHERE id=ANY($1)", &albums)
            .execute(&mut *c)
            .await?;
        jobs::enqueue_unique(c, "gc", &json!({}), 0, None, 5).await?;
    } else {
        sqlx::query!(
            "UPDATE photos SET deleted_at=CASE WHEN $2 THEN NULL ELSE coalesce(deleted_at,now()) END WHERE id=ANY($1) AND (NOT $2 OR deleted_at=$3)",
            &photos,
            restore,deleted
        )
        .execute(&mut *c)
        .await?;
        sqlx::query!(
            "UPDATE albums SET deleted_at=CASE WHEN $2 THEN NULL ELSE coalesce(deleted_at,now()) END WHERE id=ANY($1) AND (NOT $2 OR deleted_at=$3)",
            &albums,
            restore,deleted
        )
        .execute(c)
        .await?;
    }
    Ok(before)
}
async fn retry_ingest(c: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    let row=sqlx::query!("UPDATE photos SET status='pending',failure_reason=NULL,generation=generation+1 WHERE id=$1 AND deleted_at IS NULL RETURNING original_hash,generation",id).fetch_one(&mut *c).await?;
    jobs::enqueue_unique(
        c,
        "ingest",
        &json!({"photo_id":id,"original_hash":row.original_hash,"generation":row.generation}),
        0,
        None,
        3,
    )
    .await?;
    Ok(())
}
/// Persists a cache batch in the same transaction as the mutation.
pub async fn outbox(
    c: &mut PgConnection,
    value: Value,
    mut tags: Vec<String>,
) -> Result<Change, DbError> {
    tags.sort();
    tags.dedup();
    let id = Uuid::now_v7();
    sqlx::query!("INSERT INTO cache_purges(id,tags) VALUES($1,$2)", id, &tags)
        .execute(&mut *c)
        .await?;
    sqlx::query!(
        "SELECT pg_notify('session_invalidate','library:'||$1::uuid::text)",
        id
    )
    .execute(c)
    .await?;
    Ok(Change {
        value,
        tags,
        purge_id: id,
    })
}
/// Reads outstanding purge batches for safe, idempotent retries.
pub async fn pending_purges(c: &mut PgConnection) -> Result<Vec<(Uuid, Vec<String>)>, DbError> {
    Ok(
        sqlx::query!("SELECT id,tags FROM cache_purges ORDER BY created_at LIMIT 100")
            .fetch_all(c)
            .await?
            .into_iter()
            .map(|r| (r.id, r.tags))
            .collect(),
    )
}
/// Acknowledges an edge purge only after success.
pub async fn purge_done(c: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM cache_purges WHERE id=$1", id)
        .execute(c)
        .await?;
    Ok(())
}
/// Binds a resumable upload to an existing photo under the upload owner's transaction.
pub async fn bind_replacement(
    c: &mut PgConnection,
    upload: Uuid,
    photo: Uuid,
) -> Result<(), DbError> {
    let n=sqlx::query!("UPDATE uploads SET replace_photo_id=$2 WHERE id=$1 AND EXISTS(SELECT 1 FROM photos WHERE id=$2 AND deleted_at IS NULL)",upload,photo).execute(c).await?.rows_affected();
    if n != 1 {
        return Err(DbError::InvalidInput);
    }
    Ok(())
}
/// Returns the replacement target only from durable upload state, never final PATCH headers.
pub async fn replacement_target(
    c: &mut PgConnection,
    upload: Uuid,
) -> Result<Option<Uuid>, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT replace_photo_id FROM uploads WHERE id=$1", upload)
            .fetch_one(c)
            .await?,
    )
}
/// Replaces bytes while preserving identity, interactions, and analytics. Old bytes remain a GC
/// root for thirty days. The generation fence prevents an older worker from publishing results.
pub async fn replace(
    c: &mut PgConnection,
    actor: UserId,
    id: Uuid,
    original: &photos::NewPhoto,
) -> Result<Change, DbError> {
    let mut tx = c.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock(20)")
        .execute(&mut *tx)
        .await?;
    sqlx::query!("SELECT id FROM photos WHERE id=$1 FOR UPDATE", id)
        .fetch_one(&mut *tx)
        .await?;
    let before = photo_state(&mut tx, &[id]).await?;
    let photo = photos::get_by_id(&mut tx, PhotoId(id))
        .await?
        .ok_or(DbError::InvalidInput)?;
    sqlx::query!("INSERT INTO original_trash(id,photo_id,original_hash,original_name,original_bytes) VALUES($1,$2,$3,$4,$5)",Uuid::now_v7(),id,photo.original_hash,photo.original_name,photo.original_bytes).execute(&mut *tx).await?;
    let metadata = photos::PhotoMetadata {
        title: photo.title,
        description: photo.description,
        taken_at: photo.taken_at,
        place_id: photo.place_id,
        ..Default::default()
    };
    photos::replace_original(&mut tx, photo.id, original, &metadata).await?;
    let resolved=sqlx::query_scalar!("UPDATE flags SET resolved_at=now(),resolution='replaced' WHERE photo_id=$1 AND resolved_at IS NULL RETURNING id",id).fetch_all(&mut *tx).await?;
    let after = photo_state(&mut tx, &[id]).await?;
    let mut tags = aggregate_tags();
    state_tags(&before, &mut tags);
    state_tags(&after, &mut tags);
    audit::write_target(
        &mut tx,
        Some(actor),
        "photo_replace",
        "photo",
        Some(id),
        &json!({"before":before,"after":after,"resolved_flags":resolved,"resolution":"replaced"}),
    )
    .await?;
    let change = outbox(&mut tx, json!({"id":id}), tags).await?;
    tx.commit().await?;
    Ok(change)
}
/// Returns whether any photo (including trash) or retained original still owns a hash.
pub async fn original_referenced(c: &mut PgConnection, hash: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM photos WHERE original_hash=$1) OR EXISTS(SELECT 1 FROM original_trash WHERE original_hash=$1 AND expires_at>now()) AS "keep!""#,hash).fetch_one(c).await?)
}
/// Claims expired original roots as GC candidates; still-referenced hashes remain safe.
pub async fn gc_candidates(c: &mut PgConnection) -> Result<Vec<String>, DbError> {
    sqlx::query!("WITH expired AS (DELETE FROM original_trash WHERE expires_at<=now() RETURNING original_hash) INSERT INTO original_gc SELECT DISTINCT original_hash FROM expired ON CONFLICT DO NOTHING").execute(&mut *c).await?;
    Ok(
        sqlx::query_scalar!("SELECT original_hash FROM original_gc ORDER BY original_hash")
            .fetch_all(c)
            .await?,
    )
}
/// Drops a completed GC candidate; durable roots are checked again before unlinking.
pub async fn gc_done(c: &mut PgConnection, hash: &str) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM original_gc WHERE original_hash=$1", hash)
        .execute(c)
        .await?;
    Ok(())
}
/// Sets a temporary password and revokes every session in the caller's account lock.
/// Credentials never enter audit snapshots. Admin credentials cannot be reset here.
pub async fn reset_password(
    c: &mut PgConnection,
    id: Uuid,
    hash: &secrecy::SecretString,
) -> Result<u64, DbError> {
    use secrecy::ExposeSecret;
    let n=sqlx::query!("UPDATE users SET password_hash=$2,must_change_password=true WHERE id=$1 AND NOT is_admin AND deleted_at IS NULL",id,hash.expose_secret()).execute(&mut *c).await?.rows_affected();
    if n > 0 {
        sqlx::query!("DELETE FROM sessions WHERE user_id=$1", id)
            .execute(c)
            .await?;
    }
    Ok(n)
}
/// Returns whether any derivative row still references the content hash (trash included).
pub async fn derivative_referenced(c: &mut PgConnection, hash: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM derivatives WHERE hash=$1) AS "keep!""#,
        hash
    )
    .fetch_one(c)
    .await?)
}

/// All content identities affected by changes to the album tree or principal memberships.
pub async fn access_tags(c: &mut PgConnection) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT 'album:'||id AS "tag!" FROM albums UNION SELECT 'photo:'||id FROM photos UNION SELECT 'place:'||place_id FROM photos WHERE place_id IS NOT NULL"#).fetch_all(c).await?)
}

/// Includes the photo, containing album, affected place, and every aggregate tag.
pub async fn photo_cache_tags(c: &mut PgConnection, id: Uuid) -> Result<Vec<String>, DbError> {
    let state = photo_state(c, &[id]).await?;
    let mut tags = aggregate_tags();
    state_tags(&state, &mut tags);
    Ok(tags)
}
/// Safe link snapshot; bearer and passphrase digests are deliberately absent.
pub async fn link_state(c: &mut PgConnection, id: Uuid) -> Result<Value, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT jsonb_build_object('id',id,'target_kind',target_kind,'target_id',target_id,'allow_view',allow_view,'allow_download',allow_download,'allow_like',allow_like,'allow_flag',allow_flag,'expires_at',expires_at,'has_passphrase',passphrase_hash IS NOT NULL,'revoked_at',revoked_at) AS "state!" FROM share_links WHERE id=$1"#,id).fetch_optional(c).await?.unwrap_or(Value::Null))
}
