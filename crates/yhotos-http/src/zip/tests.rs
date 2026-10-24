#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::*;
use axum::{extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;
use yhotos_core::{
    ShareLinkId, Slug,
    access::{ShareTarget, Visibility},
};
use yhotos_db::{albums, photos};
use yhotos_storage::Store;
struct Fixture {
    dir: tempfile::TempDir,
    state: AppState,
    album: AlbumId,
    public: photos::PhotoRow,
    restricted: photos::PhotoRow,
    bytes: Vec<u8>,
}
impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let bytes = b"original checksum bytes".to_vec();
        let put = store.put_original(bytes.as_slice(), 1000).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let album = albums::create(
            &mut conn,
            None,
            &Slug::new("zip-album").unwrap(),
            "Zip album",
            Visibility::Public,
        )
        .await
        .unwrap();
        let data = photos::NewPhoto {
            album_id: album.id,
            title: "../../unsafe\\name\n".into(),
            original_hash: put.hash.to_string(),
            original_name: "../original.jpg".into(),
            original_bytes: i64::try_from(bytes.len()).unwrap(),
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        };
        let public = photos::create(&mut conn, &data).await.unwrap();
        let mut data = data;
        data.title = "Secret photo".into();
        let restricted = photos::create(&mut conn, &data).await.unwrap();
        photos::set_visibility(&mut conn, &[restricted.id], Visibility::Restricted)
            .await
            .unwrap();
        let state = AppState::new(
            crate::tests::state(false, 100_000).config,
            pool,
            store,
            None,
        );
        Self {
            dir,
            state,
            album: album.id,
            public,
            restricted,
            bytes,
        }
    }
    async fn get(&self) -> Response {
        let req = Request::builder()
            .uri(format!("/zip?album={}&size=original", self.album))
            .extension(ConnectInfo(
                "127.0.0.1:8080".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        crate::public_router(self.state.clone())
            .oneshot(req)
            .await
            .unwrap()
    }
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn shared_file_routes_preserve_zip_visibility_and_archive_contents(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let request = Request::builder()
        .uri(format!("/zip?album={}&size=original", f.album))
        .extension(ConnectInfo(
            "127.0.0.1:8080".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .extension(SessionIdentity::Anonymous)
        .extension(Links::default())
        .extension(ClientIp("127.0.0.1".parse().unwrap()))
        .body(Body::empty())
        .unwrap();
    // Both listeners compose these routes after their session middleware has
    // established the identity. Admin authentication itself is covered separately.
    let response = crate::file_routes()
        .with_state(f.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
    let shared_archive = response.into_body().collect().await.unwrap().to_bytes();
    let public_archive = f
        .get()
        .await
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(shared_archive, public_archive);
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn zip_visibility_safe_unique_names_unzip_checksums_and_analytics(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let response = f.get().await;
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"zip-album.zip\""
    );
    let archive = response.into_body().collect().await.unwrap().to_bytes();
    let path = f.dir.path().join("download.zip");
    std::fs::write(&path, &archive).unwrap();
    let test = std::process::Command::new("unzip")
        .arg("-t")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "{}",
        String::from_utf8_lossy(&test.stdout)
    );
    let listing = std::process::Command::new("unzip")
        .args(["-Z1"])
        .arg(&path)
        .output()
        .unwrap();
    let listing = String::from_utf8(listing.stdout).unwrap();
    let names = listing.lines().collect::<Vec<_>>();
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("001-"));
    assert!(!names[0].contains(['/', '\\']));
    assert!(!names[0].contains(".."));
    assert!(!listing.contains("Secret"));
    let extracted = std::process::Command::new("unzip")
        .arg("-p")
        .arg(&path)
        .arg(names[0])
        .output()
        .unwrap();
    assert!(extracted.status.success());
    assert_eq!(extracted.stdout, f.bytes);
    let event = sqlx::query!("SELECT kind,extra FROM events WHERE album_id=$1", f.album.0)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(event.kind, "zip_download");
    assert_eq!(event.extra["count"], 1);
}
#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn share_capability_selection_filter_sizes_and_concurrency(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let links = |download| {
        Links(vec![crate::share::ScopedLink {
            target: ShareTarget::Album(f.album),
            link_id: ShareLinkId::new(),
            caps: yhotos_db::share_links::LinkFlags {
                view: true,
                download,
                like: false,
                flag: false,
            },
            passphrase_version: 0,
        }])
    };
    let ip = "127.0.0.1".parse().unwrap();
    let options = || Options {
        album: Some(f.album),
        size: Size::Original,
    };
    assert!(matches!(
        serve(
            f.state.clone(),
            SessionIdentity::Anonymous,
            links(false),
            ip,
            options(),
            vec![]
        )
        .await,
        Err(HttpError::NotFound)
    ));
    let response = serve(
        f.state.clone(),
        SessionIdentity::Anonymous,
        links(true),
        ip,
        options(),
        vec![],
    )
    .await
    .unwrap();
    assert!(matches!(
        serve(
            f.state.clone(),
            SessionIdentity::Anonymous,
            Links::default(),
            ip,
            options(),
            vec![]
        )
        .await,
        Err(HttpError::RateLimited)
    ));
    drop(response);
    assert!(f.get().await.status().is_success());
    let response = serve(
        f.state.clone(),
        SessionIdentity::Anonymous,
        Links::default(),
        ip,
        Options::default(),
        vec![
            f.public.short_id.to_string(),
            f.restricted.short_id.to_string(),
            f.public.short_id.to_string(),
        ],
    )
    .await
    .unwrap();
    let archive = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        archive.windows(4).filter(|b| *b == b"PK\x03\x04").count(),
        1
    );
    let response = serve(
        f.state.clone(),
        SessionIdentity::Anonymous,
        Links::default(),
        ip,
        Options {
            album: Some(f.album),
            size: Size::Medium,
        },
        vec![],
    )
    .await;
    assert!(matches!(response, Err(HttpError::NotFound)));
    for name in ["large", "medium"] {
        let dir = f.state.store.tmp_dir().unwrap();
        let src = dir.path().join("image");
        tokio::fs::write(&src, b"generated").await.unwrap();
        let hash: ContentHash = blake3::hash(b"generated")
            .to_hex()
            .as_str()
            .parse()
            .unwrap();
        f.state
            .store
            .put_derivative(
                &DerivativeKey {
                    hash: &hash,
                    variant: name,
                    format: "jpeg",
                    hdr: false,
                },
                &src,
            )
            .await
            .unwrap();
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES($1,$2,$3,'jpeg',false,$4,1,1,9)",uuid::Uuid::now_v7(),f.public.id.0,name,hash.to_string()).execute(&f.state.pool).await.unwrap();
    }
    for size in [Size::Large, Size::Medium] {
        let response = serve(
            f.state.clone(),
            SessionIdentity::Anonymous,
            Links::default(),
            ip,
            Options {
                album: Some(f.album),
                size,
            },
            vec![],
        )
        .await
        .unwrap();
        let archive = response.into_body().collect().await.unwrap().to_bytes();
        assert!(archive.windows(9).any(|b| b == b"generated"));
    }
}
#[tokio::test]
async fn writer_rejects_unsafe_names_truncation_growth_and_byte_limits() {
    for name in ["../escape", "a/b", "a\\b", "..", "a\n"] {
        let mut w = writer::ZipWriter::new(tokio::io::sink(), 1000);
        assert!(w.entry(name.into(), 1, &b"x"[..]).await.is_err());
    }
    for (bytes, size) in [(&b"x"[..], 2), (&b"xxx"[..], 2)] {
        let mut w = writer::ZipWriter::new(tokio::io::sink(), 1000);
        assert!(w.entry("safe".into(), size, bytes).await.is_err());
    }
    let mut w = writer::ZipWriter::new(tokio::io::sink(), 20);
    assert!(w.entry("safe".into(), 1, &b"x"[..]).await.is_err());
}
#[tokio::test]
async fn large_entries_keep_crc_and_lengths_across_short_reads() {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, ReadBuf};
    struct Fragmented<'a>(&'a [u8]);
    impl AsyncRead for Fragmented<'_> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let n = out.remaining().min(self.0.len()).min(65_537);
            out.put_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Poll::Ready(Ok(()))
        }
    }
    let data: Vec<_> = (0..writer::COPY_BYTES + 137)
        .map(|n| n.to_le_bytes()[0])
        .collect();
    let mut archive = Vec::new();
    let mut zip = writer::ZipWriter::new(&mut archive, 2 * data.len() as u64 + 1024);
    for name in ["001-first.bin", "002-second.bin"] {
        zip.entry(name.into(), data.len() as u64, Fragmented(&data))
            .await
            .unwrap();
    }
    zip.finish().await.unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), archive).unwrap();
    let checked = std::process::Command::new("unzip")
        .arg("-t")
        .arg(file.path())
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    for name in ["001-first.bin", "002-second.bin"] {
        let extracted = std::process::Command::new("unzip")
            .arg("-p")
            .arg(file.path())
            .arg(name)
            .output()
            .unwrap();
        assert!(extracted.status.success());
        assert_eq!(extracted.stdout, data);
    }
}
#[tokio::test]
async fn zip64_sparse_file_records_64_bit_sizes_offsets_and_end_directory() {
    use std::{
        io::{Seek, Write},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::AsyncWrite;
    struct Sink {
        bytes: u64,
        headers: Vec<u8>,
        archive: std::fs::File,
    }
    impl AsyncWrite for Sink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.bytes += buf.len() as u64;
            if buf.len() < 512 {
                self.headers.extend(buf);
            }
            // The test input is a zero-filled sparse file. Preserve its large
            // zero copy buffers as holes, while writing real ZIP metadata/data.
            let result = if buf.len() >= 512 {
                self.archive
                    .seek(std::io::SeekFrom::Current(
                        i64::try_from(buf.len()).unwrap(),
                    ))
                    .map(|_| buf.len())
            } else {
                self.archive.write_all(buf).map(|()| buf.len())
            };
            Poll::Ready(result)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    let file = tempfile::tempfile().unwrap();
    let size = u64::from(u32::MAX) + 37;
    file.set_len(size).unwrap();
    let archive = tempfile::NamedTempFile::new().unwrap();
    let mut sink = Sink {
        bytes: 0,
        headers: Vec::new(),
        archive: archive.reopen().unwrap(),
    };
    let mut w = writer::ZipWriter::new(&mut sink, size + 1024);
    w.entry(
        "001-large.bin".into(),
        size,
        tokio::fs::File::from_std(file),
    )
    .await
    .unwrap();
    w.entry("002-small.bin".into(), 1, &b"x"[..]).await.unwrap();
    w.finish().await.unwrap();
    let central = sink
        .headers
        .windows(4)
        .position(|b| b == b"PK\x01\x02")
        .unwrap();
    let u64at = |i| u64::from_le_bytes(sink.headers[i..i + 8].try_into().unwrap());
    assert_eq!(u64at(central + 46 + 13 + 4), size);
    assert_eq!(u64at(central + 46 + 13 + 4 + 16), 0);
    let end = sink
        .headers
        .windows(4)
        .position(|b| b == b"PK\x06\x06")
        .unwrap();
    assert_eq!(u64at(end + 24), 2);
    assert!(u64at(end + 48) > u64::from(u32::MAX));
    assert!(sink.bytes > size);
    let checked = std::process::Command::new("unzip")
        .arg("-t")
        .arg(archive.path())
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn archive_global_cap_size_cap_and_selection_count(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let mut bodies = Vec::new();
    for n in 1..=4 {
        bodies.push(
            serve(
                f.state.clone(),
                SessionIdentity::Anonymous,
                Links::default(),
                format!("127.0.0.{n}").parse().unwrap(),
                Options {
                    album: Some(f.album),
                    size: Size::Original,
                },
                vec![],
            )
            .await
            .unwrap(),
        );
    }
    assert!(matches!(
        serve(
            f.state.clone(),
            SessionIdentity::Anonymous,
            Links::default(),
            "127.0.0.5".parse().unwrap(),
            Options {
                album: Some(f.album),
                size: Size::Original
            },
            vec![]
        )
        .await,
        Err(HttpError::RateLimited)
    ));
    drop(bodies);
    let result = selection(
        State(f.state.clone()),
        Extension(SessionIdentity::Anonymous),
        Extension(Links::default()),
        Extension(ClientIp("127.0.0.1".parse().unwrap())),
        Query(Options::default()),
        Json(vec![f.public.short_id.to_string(); 501]),
    )
    .await;
    assert!(matches!(result, Err(HttpError::BadRequest)));
    sqlx::query!(
        "UPDATE photos SET original_bytes=$2 WHERE id=$1",
        f.public.id.0,
        21_i64 << 30
    )
    .execute(&f.state.pool)
    .await
    .unwrap();
    assert!(matches!(
        serve(
            f.state.clone(),
            SessionIdentity::Anonymous,
            Links::default(),
            "127.0.0.1".parse().unwrap(),
            Options {
                album: Some(f.album),
                size: Size::Original
            },
            vec![]
        )
        .await,
        Err(HttpError::BadRequest)
    ));
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn unavailable_file_aborts_the_archive_body_and_releases_its_slot(pool: PgPool) {
    let f = Fixture::new(pool).await;
    f.state
        .store
        .remove(
            Area::Originals,
            &f.public.original_hash.parse().unwrap(),
            None,
        )
        .await
        .unwrap();
    let response = f.get().await;
    assert_eq!(response.status(), 200);
    assert!(response.into_body().collect().await.is_err());
    let _permit = f
        .state
        .zip_streams()
        .acquire("127.0.0.1".parse().unwrap())
        .unwrap();
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn post_selection_streams_through_the_public_router_with_same_origin_csrf_proof(
    pool: PgPool,
) {
    let f = Fixture::new(pool).await;
    let csrf = crate::middleware::csrf::CsrfToken::generate().unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/zip?size=original")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://example.test")
        .header("sec-fetch-site", "same-origin")
        .header("x-csrf-token", csrf.form_value())
        .header(
            header::COOKIE,
            format!("__Host-yhotos-csrf={}", csrf.form_value()),
        )
        .extension(ConnectInfo(
            "127.0.0.1:8080".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(
            serde_json::to_vec(&vec![
                f.public.short_id.to_string(),
                f.restricted.short_id.to_string(),
            ])
            .unwrap(),
        ))
        .unwrap();
    let response = crate::public_router(f.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.windows(4).filter(|b| *b == b"PK\x03\x04").count(), 1);
}
