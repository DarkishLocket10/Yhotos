//! Archive manifests use the same visibility CTE as gallery pages, with download-capable link scopes only.
use crate::{DbError, access_sql::Bindings};
use sqlx::PgConnection;
use uuid::Uuid;
/// Selected file metadata; every row has passed visibility before selection and LIMIT.
#[derive(Debug)]
pub struct Entry {
    /// Escaped later into an archive filename.
    pub title: String,
    /// Canonical public identifier used when title is empty.
    pub short_id: String,
    /// Validated by storage before opening.
    pub hash: String,
    /// Trusted MIME vocabulary, validated before choosing an extension.
    pub mime: String,
    /// Size of the selected stored representation.
    pub bytes: i64,
}
/// Returns bounded selected files, preserving a deterministic order and excluding unavailable sizes.
/// # Errors
/// Returns database failures; callers reject the sentinel 100001st entry.
pub async fn manifest(
    conn: &mut PgConnection,
    bindings: &Bindings,
    album: Option<Uuid>,
    ids: &[String],
    size: &str,
) -> Result<Vec<Entry>, DbError> {
    let rows = crate::visible_page_query!(r#"
      SELECT left(p.title,256) AS "title!",p.short_id::text AS "short_id!",
        CASE WHEN $8='original' THEN p.original_hash ELSE d.hash END AS "hash!",
        CASE WHEN $8='original' THEN left(p.mime,128) ELSE 'image/jpeg' END AS "mime!",
        CASE WHEN $8='original' THEN p.original_bytes ELSE d.bytes END AS "bytes!"
      FROM visible_photos p LEFT JOIN derivatives d ON d.photo_id=p.id AND d.variant=$8 AND d.format='jpeg' AND NOT d.hdr
      WHERE (($6::uuid IS NOT NULL AND p.album_id=$6) OR p.short_id::text=ANY($7::text[]) OR p.id::text=ANY($7::text[]))
        AND ($8='original' OR d.id IS NOT NULL)
      ORDER BY p.sort_order,p.id LIMIT 100001"#, bindings,album,ids,size).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|r| Entry {
            title: r.title,
            short_id: r.short_id,
            hash: r.hash,
            mime: r.mime,
            bytes: r.bytes,
        })
        .collect())
}
/// Records a privacy-preserving archive download without tokens, IPs or filenames.
/// # Errors
/// Returns database failures.
pub async fn record(
    conn: &mut PgConnection,
    album: Option<Uuid>,
    visitor: &[u8],
    extra: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!("INSERT INTO events(id,kind,album_id,visitor_hash,extra) VALUES($1,'zip_download',$2,$3,$4)",Uuid::now_v7(),album,visitor,extra).execute(conn).await?;
    Ok(())
}
