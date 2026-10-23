//! MariaDB access contains only SELECT statements over the six Piwigo tables.
use crate::{Error, Result};
use sqlx::{
    Connection, MySqlPool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode},
};
use time::PrimitiveDateTime;

pub(crate) struct Album {
    pub id: i64,
    pub parent: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub global_rank: Option<String>,
    pub cover: Option<i64>,
    pub status: String,
    pub url_name: Option<String>,
}
pub(crate) struct Image {
    pub id: i64,
    pub filename: String,
    pub path: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub taken: Option<PrimitiveDateTime>,
    pub posted: PrimitiveDateTime,
    pub level: i64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}
pub(crate) struct Membership {
    pub image: i64,
    pub category: i64,
    pub photo_order: Option<i64>,
}
pub(crate) struct Tag {
    pub id: i64,
    pub name: String,
}
pub(crate) struct TagLink {
    pub image: i64,
    pub tag: i64,
}
pub(crate) struct Snapshot {
    pub albums: Vec<Album>,
    pub images: Vec<Image>,
    pub memberships: Vec<Membership>,
    pub tags: Vec<Tag>,
    pub tag_links: Vec<TagLink>,
}

pub(crate) async fn connect(url: &str) -> Result<MySqlPool> {
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let parsed = url::Url::parse(url).map_err(|_| Error::Configuration)?;
    let local = parsed.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    let options = MySqlConnectOptions::from_str(url)
        .map_err(|_| Error::Configuration)?
        .ssl_mode(if local {
            MySqlSslMode::Preferred
        } else {
            MySqlSslMode::VerifyIdentity
        })
        .disable_statement_logging();
    Ok(MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?)
}

pub(crate) async fn snapshot(pool: &MySqlPool) -> Result<Snapshot> {
    // Repeatable read protects transactional tables only. The runbook requires
    // all writers stopped while creating DB/upload snapshots (Piwigo uses MyISAM).
    // The SELECT-only user is the write boundary; dry-run uses the same SELECTs.
    let mut conn = pool.acquire().await?;
    sqlx::query_file!("queries/read_only.sql")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let config = sqlx::query_file!("queries/config.sql")
        .fetch_all(&mut *tx)
        .await?;
    if !config.iter().any(|row| {
        row.value
            .as_deref()
            .is_some_and(|v| v == "16" || v.starts_with("16."))
    }) {
        return Err(Error::SourceSchema);
    }
    let albums = sqlx::query_file_as!(Album, "queries/albums.sql")
        .fetch_all(&mut *tx)
        .await?;
    let images = sqlx::query_file_as!(Image, "queries/images.sql")
        .fetch_all(&mut *tx)
        .await?;
    let memberships = sqlx::query_file_as!(Membership, "queries/memberships.sql")
        .fetch_all(&mut *tx)
        .await?;
    let tags = sqlx::query_file_as!(Tag, "queries/tags.sql")
        .fetch_all(&mut *tx)
        .await?;
    let tag_links = sqlx::query_file_as!(TagLink, "queries/tag_links.sql")
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(Snapshot {
        albums,
        images,
        memberships,
        tags,
        tag_links,
    })
}
