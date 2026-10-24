//! Visibility-aware bounded candidate lists; every value remains a SQL binding.
use crate::{DbError, access_sql::Bindings, photos::PagePhoto};
use sqlx::PgPool;
use uuid::Uuid;

/// Pinned embedding coordinate system, shared with the offline ML artifact lock.
pub const EMBED_MODEL: &str = "siglip-base-patch16-224@4649052661e53c7000355844105f8a1792088239";

/// Validated structured restriction emitted by the HTTP grammar.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Filter {
    /// Closed parser vocabulary.
    pub key: String,
    /// Canonical value, bound as data.
    pub value: String,
}
/// A visible photo and bounded OCR excerpt; no biometric data or vectors leave SQL.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Candidate {
    /// Page-safe metadata used by the existing grid.
    pub photo: PagePhoto,
    /// Untrusted model text, escaped at every rendering boundary.
    pub text: String,
}

// Ranked lists apply access before LIMIT. Their metadata joins use the same
// statement snapshot, so they need not recompute the access tree. OFFSET 0 keeps
// semantic membership correlated and preserves the HNSW distance-order scan.
// Selective semantic sets (<=200 eligible photos) use exact distance ordering;
// broader sets retain bounded iterative HNSW scans on the public/admin pools.
// Sampling at most 201 eligible ids decides which branch without a full count.
macro_rules! candidates {
    ($tail:literal, $p:expr, $f:expr, $text:expr, $vector:expr, $like:expr) => {
        crate::visible_page_query!(@with r#"WITH RECURSIVE identity AS (
          SELECT $1::uuid AS user_id,$2::uuid[] AS groups,$3::bool AS admin,
            $4::uuid[] AS shared_albums,$5::uuid[] AS shared_photos)"#;
          [r#", filters AS (SELECT * FROM jsonb_to_recordset($6::jsonb) AS f(key text,value text)),
          filtered AS NOT MATERIALIZED (SELECT p.* FROM visible_photos p WHERE p.status='ready'
            AND NOT EXISTS (SELECT FROM filters f WHERE NOT coalesce(CASE f.key
              WHEN 'camera' THEN lower(concat_ws(' ',nullif(p.camera_make,''),nullif(p.camera_model,'')))=f.value OR lower(p.camera_model)=f.value
              WHEN 'lens' THEN lower(p.lens)=f.value
              WHEN 'tag' THEN EXISTS (SELECT FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id WHERE pt.photo_id=p.id AND NOT t.hidden AND (lower(t.name)=f.value OR t.slug::text=f.value))
              WHEN 'place' THEN EXISTS (SELECT FROM places l WHERE l.id=p.place_id AND lower(l.name)=f.value)
              WHEN 'year' THEN extract(year FROM coalesce(p.taken_at,p.posted_at) AT TIME ZONE 'UTC')=f.value::int
              WHEN 'month' THEN extract(month FROM coalesce(p.taken_at,p.posted_at) AT TIME ZONE 'UTC')=f.value::int
              WHEN 'before' THEN coalesce(p.taken_at,p.posted_at)<(f.value::date::timestamp AT TIME ZONE 'UTC')
              WHEN 'after' THEN coalesce(p.taken_at,p.posted_at)>=(f.value::date::timestamp AT TIME ZONE 'UTC')
              WHEN 'is' THEN CASE f.value WHEN 'hdr' THEN p.is_hdr ELSE p.kind::text=f.value END
              WHEN 'color' THEN EXISTS (SELECT FROM unnest(p.dominant_colors) c WHERE
                abs(((c>>16)&255)-((f.value::int>>16)&255))<=64 AND
                abs(((c>>8)&255)-((f.value::int>>8)&255))<=64 AND abs((c&255)-(f.value::int&255))<=64)
              ELSE false END,false))),
          seed AS (SELECT e.vector,e.model FROM embeddings e JOIN visible_photos p ON p.id=e.photo_id WHERE p.short_id=$9::text),
          query_vector AS (SELECT coalesce($8::text::vector,(SELECT vector FROM seed)) AS v,
            CASE WHEN $8::text IS NOT NULL THEN 'siglip-base-patch16-224@4649052661e53c7000355844105f8a1792088239' ELSE (SELECT model FROM seed) END AS model),
          text_query AS MATERIALIZED (SELECT plainto_tsquery('simple',$7::text) AS q),
          ranked AS ("#, $tail, r#") SELECT jsonb_build_object('photo',to_jsonb(p) || jsonb_build_object(
            'taken_at',to_char(p.taken_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
            'thumbhash',encode(p.placeholder,'base64'),'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
              'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
              ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)),
            'text',left(coalesce((SELECT text FROM ocr_text o WHERE o.photo_id=p.id),''),4096)) AS "payload!"
          FROM ranked r JOIN photos p ON p.id=r.id ORDER BY r.rank,p.id"#];
          [$p.user,&$p.groups,$p.admin,&$p.album,&$p.photo,$f,$text,$vector,$like])
    };
}

/// Candidate source, deliberately capped at two hundred before fusion.
/// This relevance window bounds database/response work; users can narrow filters
/// to reach other matches. The fused window is paginated sixty photos at a time.
#[derive(Clone, Copy)]
pub enum List {
    /// HNSW cosine neighbours of a query or visible seed photo.
    Semantic,
    /// OCR full text and title trigram relevance.
    Text,
    /// Matching tags, or recency for a filter-only query.
    Structured,
}
/// Fetches exactly one candidate list with access and all filters applied before its limit.
///
/// # Errors
/// Returns database failures or malformed stored photo metadata.
pub async fn candidates(
    pool: &PgPool,
    p: &Bindings,
    filters: &[Filter],
    text: &str,
    vector: Option<&str>,
    like: Option<&str>,
    list: List,
) -> Result<Vec<Candidate>, DbError> {
    let f = serde_json::to_value(filters).map_err(|_| DbError::InvalidInput)?;
    let rows = match list {
        List::Semantic => candidates!(r#"WITH eligible AS MATERIALIZED (
          SELECT p.id FROM filtered p WHERE $9::text IS NULL OR p.short_id<>$9 ORDER BY p.id LIMIT 201
        ) SELECT id,rank FROM (
          (SELECT e.photo_id AS id,e.vector <=> (SELECT v FROM query_vector) AS rank
           FROM eligible p JOIN embeddings e ON e.photo_id=p.id
           WHERE (SELECT count(*) FROM eligible)<=200 AND (SELECT v FROM query_vector) IS NOT NULL
           AND e.model=(SELECT model FROM query_vector) AND $7::text IS NOT NULL
           ORDER BY (e.vector <=> (SELECT v FROM query_vector))+0,e.photo_id LIMIT 200)
          UNION ALL
          (SELECT e.photo_id AS id,e.vector <=> (SELECT v FROM query_vector) AS rank
           FROM embeddings e WHERE (SELECT count(*) FROM eligible)>200 AND (SELECT v FROM query_vector) IS NOT NULL
           AND e.model=(SELECT model FROM query_vector) AND $7::text IS NOT NULL
           AND EXISTS (SELECT FROM filtered p WHERE p.id=e.photo_id AND ($9::text IS NULL OR p.short_id<>$9) OFFSET 0)
           ORDER BY e.vector <=> (SELECT v FROM query_vector) LIMIT 200)
        ) neighbors ORDER BY rank,id LIMIT 200"#, p, f, text, vector, like).fetch_all(pool).await?.into_iter().map(|r| r.payload).collect::<Vec<_>>(),
        List::Text => candidates!(r#"SELECT id,-max(score) AS rank FROM (
          (SELECT title_photo.id,similarity(title_photo.title,$7) AS score FROM photos title_photo
           WHERE $7<>'' AND title_photo.title % $7 AND EXISTS (
             SELECT FROM filtered p WHERE p.id=title_photo.id OFFSET 0)
           ORDER BY title_photo.title <-> $7 LIMIT 200)
          UNION ALL
          (SELECT o.photo_id AS id,ts_rank(o.tsv,(SELECT q FROM text_query)) AS score FROM ocr_text o
           WHERE $7<>'' AND o.tsv @@ (SELECT q FROM text_query) AND EXISTS (
             SELECT FROM filtered p WHERE p.id=o.photo_id OFFSET 0)
           ORDER BY score DESC,o.photo_id LIMIT 200)
          ) matches GROUP BY id ORDER BY rank,id LIMIT 200"#, p, f, text, vector, like).fetch_all(pool).await?.into_iter().map(|r| r.payload).collect(),
        List::Structured => candidates!(r#"SELECT p.id,-extract(epoch FROM p.posted_at) AS rank FROM filtered p
          WHERE ($7='' OR EXISTS (SELECT FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id
            WHERE pt.photo_id=p.id AND NOT t.hidden AND lower(t.name)=lower($7)))
          AND ($8::text IS NULL OR true) AND ($9::text IS NULL OR true) ORDER BY rank,p.id LIMIT 200"#, p, f, text, vector, like).fetch_all(pool).await?.into_iter().map(|r| r.payload).collect(),
    };
    rows.into_iter()
        .map(|row| serde_json::from_value(row).map_err(|_| DbError::InvalidInput))
        .collect()
}

/// Prefix completion with counts restricted to the current principal's visible photos.
#[derive(serde::Serialize)]
pub struct Suggestion {
    /// Filter prefix, never a person identity.
    pub kind: String,
    /// Plain model or metadata text, serialized as data.
    pub value: String,
    /// Number of visible photos containing this value.
    pub count: i64,
}
/// Returns at most twenty suggestions, escaping LIKE metacharacters as literals.
/// This deliberate typeahead cap keeps the dropdown small; a more specific
/// prefix reaches values outside the current window, without an exhaustive list.
///
/// # Errors
/// Returns invalid prefix lengths or database failures.
pub async fn suggest(
    pool: &PgPool,
    p: &Bindings,
    prefix: &str,
) -> Result<Vec<Suggestion>, DbError> {
    if prefix.chars().count() > 80 || prefix.chars().any(char::is_control) {
        return Err(DbError::InvalidInput);
    }
    let prefix = format!(
        "{}%",
        prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows = crate::visible_page_query!(r#", values AS (
       SELECT p.id,'tag' AS kind,t.name AS value FROM visible_photos p JOIN photo_tags pt ON pt.photo_id=p.id JOIN tags t ON t.id=pt.tag_id WHERE NOT t.hidden
       UNION ALL SELECT p.id,'place',l.name FROM visible_photos p JOIN places l ON l.id=p.place_id
       UNION ALL SELECT p.id,'camera',concat_ws(' ',nullif(p.camera_make,''),nullif(p.camera_model,'')) FROM visible_photos p
       UNION ALL SELECT p.id,'lens',p.lens FROM visible_photos p)
       SELECT kind AS "kind!",left(value,200) AS "value!",count(DISTINCT id) AS "count!" FROM values
       WHERE value ILIKE $6 AND value IS NOT NULL AND value<>'' GROUP BY kind,value ORDER BY 3 DESC,kind,value LIMIT 20"#,p,prefix).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| Suggestion {
            kind: r.kind,
            value: r.value,
            count: r.count,
        })
        .collect())
}

/// Emits a search event without a user identity or visitor identifier.
///
/// # Errors
/// Returns database failures. Callers must apply their query privacy policy first.
pub async fn event(pool: &PgPool, normalized: &str) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO events(id,kind,visitor_hash,extra) VALUES ($1,'search',$2,$3)",
        Uuid::now_v7(),
        &[] as &[u8],
        serde_json::json!({"query":normalized})
    )
    .execute(pool)
    .await?;
    Ok(())
}
