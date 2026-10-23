//! Bounded administrative read models. Credential columns are never selected.
use crate::{DbError, PgConnection};
use serde_json::{Value, json};
use uuid::Uuid;

/// Returns one server-renderable page, with fixed SQL and bound filters.
/// Unknown pages return invalid input, rather than selecting arbitrary tables.
/// # Errors
/// Returns database failures or invalid page names.
#[allow(clippy::too_many_lines)]
pub async fn page(
    c: &mut PgConnection,
    name: &str,
    id: Option<Uuid>,
    filter: &str,
    offset: i64,
) -> Result<Value, DbError> {
    if offset < 0
        || (name == "jobs" && !["", "pending", "running", "failed", "complete"].contains(&filter))
    {
        return Err(DbError::InvalidInput);
    }
    let value=match name{
        "dashboard"=>sqlx::query_scalar!(r#"SELECT jsonb_build_object(
          'photos',(SELECT count(*) FROM photos WHERE deleted_at IS NULL),'albums',(SELECT count(*) FROM albums WHERE deleted_at IS NULL),
          'pending ingests',(SELECT count(*) FROM photos WHERE status='pending'),'failed ingests',(SELECT count(*) FROM photos WHERE status='failed'),
          'open flags',(SELECT count(*) FROM flags WHERE resolved_at IS NULL),'links expiring soon',(SELECT count(*) FROM share_links WHERE revoked_at IS NULL AND expires_at BETWEEN now() AND now()+interval '7 days'),
          'activity',(SELECT coalesce(jsonb_agg(to_jsonb(a)),'[]') FROM (SELECT at,action,target_kind,target_id FROM audit_log ORDER BY at DESC LIMIT 20) a)) AS "page!""#).fetch_one(c).await?,
        "albums"=>super::library::album_state(c).await?,
        "album"=>sqlx::query_scalar!(r#"SELECT jsonb_build_object('album',to_jsonb(a),'photos',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',p.id,'short_id',p.short_id,'title',p.title,'visibility',p.visibility,'kind',p.kind,'is_hdr',p.is_hdr,'status',p.status,'deleted_at',p.deleted_at,'preview',(SELECT '/i/'||d.hash||'/'||d.variant||'.'||d.format FROM derivatives d WHERE d.photo_id=p.id AND NOT hdr ORDER BY (variant='thumb') DESC LIMIT 1)) ORDER BY p.sort_order,p.id),'[]') FROM photos p WHERE p.album_id=a.id)) AS "page!" FROM albums a WHERE id=$1"#,id).fetch_optional(c).await?.ok_or(DbError::InvalidInput)?,
        "photo"=>sqlx::query_scalar!(r#"SELECT to_jsonb(p)||jsonb_build_object('derivatives',(SELECT coalesce(jsonb_agg(to_jsonb(d)),'[]') FROM derivatives d WHERE photo_id=p.id),'tags',(SELECT coalesce(jsonb_agg(t.name),'[]') FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id WHERE pt.photo_id=p.id),'grants',(SELECT coalesce(jsonb_agg(jsonb_build_object('kind',g.principal_kind,'id',g.principal_id)),'[]') FROM photo_grants g WHERE photo_id=p.id)) AS "page!" FROM photos p WHERE id=$1"#,id).fetch_optional(c).await?.ok_or(DbError::InvalidInput)?,
        "trash"=>sqlx::query_scalar!(r#"SELECT jsonb_build_object('albums',(SELECT coalesce(jsonb_agg(to_jsonb(a)),'[]') FROM albums a WHERE deleted_at IS NOT NULL),'photos',(SELECT coalesce(jsonb_agg(to_jsonb(p)),'[]') FROM photos p WHERE deleted_at IS NOT NULL),'originals',(SELECT coalesce(jsonb_agg(to_jsonb(t)),'[]') FROM original_trash t)) AS "page!""#).fetch_one(c).await?,
        "flags"=>sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(to_jsonb(p)||jsonb_build_object('flags',(SELECT jsonb_agg(jsonb_build_object('id',f.id,'flagger',coalesce(u.username::text,'Share link '||f.share_link_id::text),'at',f.created_at) ORDER BY f.created_at) FROM flags f LEFT JOIN users u ON u.id=f.user_id WHERE f.photo_id=p.id AND f.resolved_at IS NULL))),'[]') AS "page!" FROM photos p WHERE EXISTS(SELECT 1 FROM flags f WHERE f.photo_id=p.id AND f.resolved_at IS NULL)"#).fetch_one(c).await?,
        "likes"=>sqlx::query_scalar!(r#"SELECT jsonb_build_object('photos',(SELECT coalesce(jsonb_agg(to_jsonb(x)),'[]') FROM (SELECT p.id,p.short_id,p.title,count(*) AS likes,jsonb_agg(jsonb_build_object('user',u.username,'link',l.share_link_id,'at',l.created_at)) AS likers FROM likes l JOIN photos p ON p.id=l.photo_id LEFT JOIN users u ON u.id=l.user_id GROUP BY p.id ORDER BY count(*) DESC LIMIT 200) x),'users',(SELECT coalesce(jsonb_agg(to_jsonb(x)),'[]') FROM (SELECT u.username,count(*) AS likes FROM likes l JOIN users u ON u.id=l.user_id GROUP BY u.id ORDER BY count(*) DESC) x)) AS "page!""#).fetch_one(c).await?,
        "users"=>sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(jsonb_build_object('id',u.id,'username',u.username,'disabled_at',u.disabled_at,'deleted_at',u.deleted_at,'is_admin',u.is_admin,'must_change_password',u.must_change_password,'mfa',(SELECT coalesce(jsonb_agg(jsonb_build_object('kind',kind,'label',label,'enabled_at',enabled_at)),'[]') FROM user_mfa WHERE user_id=u.id),'groups',(SELECT coalesce(jsonb_agg(group_id),'[]') FROM user_groups WHERE user_id=u.id),'sessions',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'last_seen_at',last_seen_at,'expires_at',expires_at,'admin',admin,'ua_class',ua_class)),'[]') FROM sessions WHERE user_id=u.id)) ORDER BY u.username),'[]') AS "page!" FROM users u"#).fetch_one(c).await?,
        "groups"=>sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(to_jsonb(g)||jsonb_build_object('members',(SELECT coalesce(jsonb_agg(user_id),'[]') FROM user_groups WHERE group_id=g.id)) ORDER BY g.name),'[]') AS "page!" FROM groups g"#).fetch_one(c).await?,
        "links"=>sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(jsonb_build_object('id',l.id,'target_kind',l.target_kind,'target_id',l.target_id,'allow_view',l.allow_view,'allow_download',l.allow_download,'allow_like',l.allow_like,'allow_flag',l.allow_flag,'expires_at',l.expires_at,'revoked_at',l.revoked_at,'passphrase',l.passphrase_hash IS NOT NULL,'opens',(SELECT count(*) FROM share_link_opens WHERE share_link_id=l.id),'unique_visitors',(SELECT count(DISTINCT visitor_hash) FROM share_link_opens WHERE share_link_id=l.id),'last_open',(SELECT max(opened_at) FROM share_link_opens WHERE share_link_id=l.id)) ORDER BY l.created_at DESC),'[]') AS "page!" FROM share_links l"#).fetch_one(c).await?,
        "jobs"=>sqlx::query_scalar!(r#"SELECT jsonb_build_object('depth',(SELECT coalesce(jsonb_agg(to_jsonb(x)),'[]') FROM (SELECT kind,count(*) FROM jobs WHERE finished_at IS NULL GROUP BY kind) x),'jobs',(SELECT coalesce(jsonb_agg(to_jsonb(x)),'[]') FROM (SELECT id,kind,attempts,max_attempts,locked_at,locked_by,finished_at,error,run_after FROM jobs WHERE CASE $1::text
            WHEN 'running' THEN finished_at IS NULL AND locked_at IS NOT NULL
            WHEN 'failed' THEN finished_at IS NOT NULL AND error IS NOT NULL
            WHEN 'complete' THEN finished_at IS NOT NULL AND error IS NULL
            WHEN 'pending' THEN finished_at IS NULL AND locked_at IS NULL
            ELSE true END ORDER BY created_at DESC,id DESC LIMIT 100 OFFSET $2) x)) AS "page!""#,filter,offset).fetch_one(c).await?,
        "audit"=>sqlx::query_scalar!(r#"SELECT coalesce(jsonb_agg(to_jsonb(a)),'[]') AS "page!" FROM (SELECT at,user_id,action,target_kind,target_id,detail FROM audit_log WHERE action ILIKE '%'||$1||'%' OR target_id::text=$1 OR user_id::text=$1 ORDER BY at DESC,id DESC LIMIT 100 OFFSET $2) a"#,filter,offset).fetch_one(c).await?,
        _=>return Err(DbError::InvalidInput)
    };
    Ok(value)
}
/// Supplies safe choices for grant, membership, move and place controls in two queries.
/// # Errors
/// Returns database failures.
pub async fn choices(c: &mut PgConnection) -> Result<Value, DbError> {
    let albums = super::library::album_state(c).await?;
    let other=sqlx::query_scalar!(r#"SELECT jsonb_build_object('users',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'username',username)),'[]') FROM users WHERE deleted_at IS NULL),'groups',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'name',name)),'[]') FROM groups),'photos',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'album_id',album_id,'title',title)),'[]') FROM photos WHERE deleted_at IS NULL),'places',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'name',name)),'[]') FROM places)) AS "choices!""#).fetch_one(c).await?;
    Ok(
        json!({"albums":albums,"users":other.get("users"),"groups":other.get("groups"),"places":other.get("places"),"photos":other.get("photos")}),
    )
}
