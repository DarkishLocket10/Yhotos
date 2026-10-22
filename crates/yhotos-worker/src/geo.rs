//! Routes `GeoReverse` to the offline repository and preserves existing ingest jobs.
use std::time::Duration;
use yhotos_jobs::{Job, JobCtx, JobError, JobHandler, JobRow};

pub(crate) struct Handler {
    pub(crate) ingest: yhotos_pipeline::Ingest,
    pub(crate) pool: sqlx::PgPool,
}
#[async_trait::async_trait]
impl JobHandler for Handler {
    async fn handle(&self, row: &JobRow, ctx: &JobCtx) -> Result<(), JobError> {
        let job = Job::from_row(row).map_err(|_| JobError::Fatal("invalid job payload".into()))?;
        if let Job::GeoReverse { photo_id } = job {
            let result = async {
                let mut conn = self.pool.acquire().await?;
                yhotos_db::places::reverse(&mut conn, photo_id).await
            }
            .await;
            result.map_err(|_| JobError::Retry {
                after: Duration::from_secs(30),
                reason: "offline place lookup failed".into(),
            })
        } else {
            self.ingest.handle(row, ctx).await
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sqlx::{ConnectOptions, PgPool};
    use yhotos_core::{ShortId, Slug, access::Visibility};
    use yhotos_db::{albums, jobs, photos};
    #[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
    async fn worker_dispatch_resolves_claim_with_worker_privileges(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let album = albums::create(
            &mut conn,
            None,
            &Slug::new("worker-geo").unwrap(),
            "Worker geo",
            Visibility::Public,
        )
        .await
        .unwrap();
        let photo = photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Located photo".into(),
                original_hash: "a".repeat(64),
                original_name: "photo.jpeg".into(),
                original_bytes: 1,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE photos SET gps_lat=43.7002,gps_lon=-79.4164 WHERE id=$1",
            photo.id.0
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES(6167865,'Toronto','08','CA',43.70011,-79.4163,2600000)").execute(&mut *conn).await.unwrap();
        drop(conn);
        let url = pool.connect_options().to_url_lossy().to_string().into();
        let db = yhotos_db::Db::connect(
            &url,
            yhotos_db::Role::Worker,
            2,
            yhotos_db::DatabaseTls::Disable,
        )
        .await
        .unwrap();
        let queue = yhotos_jobs::Queue::new(db.pool().clone());
        queue
            .enqueue(Job::GeoReverse { photo_id: photo.id }, 0, None)
            .await
            .unwrap();
        let (row, _) = jobs::claim(
            &mut db.pool().acquire().await.unwrap(),
            "geo-test",
            &["geo.reverse"],
        )
        .await
        .unwrap()
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ingest = yhotos_pipeline::Ingest::new(
            yhotos_storage::Store::open(dir.path()).unwrap(),
            db.pool().clone(),
            dir.path().join("unused-codec-host"),
        );
        let handler = Handler {
            ingest,
            pool: db.pool().clone(),
        };
        let ctx = JobCtx {
            queue,
            shutdown: tokio_util::sync::CancellationToken::new(),
        };
        handler.handle(&row, &ctx).await.unwrap();
        let mut conn = db.pool().acquire().await.unwrap();
        let first = photos::get_by_short_id(&mut conn, &ShortId::from_photo_id(photo.id))
            .await
            .unwrap()
            .unwrap()
            .place_id;
        assert!(first.is_some());
        drop(conn);
        handler.handle(&row, &ctx).await.unwrap();
        let mut conn = db.pool().acquire().await.unwrap();
        assert_eq!(
            photos::get_by_short_id(&mut conn, &ShortId::from_photo_id(photo.id))
                .await
                .unwrap()
                .unwrap()
                .place_id,
            first
        );
    }
}
