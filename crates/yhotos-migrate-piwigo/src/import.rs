use crate::{
    AlbumProgress, Error, Options, Report, Result,
    plan::{self, Plan, problem},
    source::Image,
};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};
use tokio::task::JoinSet;
use yhotos_core::{AlbumId, PhotoId, access::ShareTarget};
use yhotos_db::{
    Db, albums,
    migration::{self, ImportPhoto},
    photos::{MediaKind, NewPhoto},
    tags::{self, TagSource},
};
use yhotos_storage::{Area, ContentHash, SourceTree, Store};

const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
struct Prepared {
    image: Image,
    hash: ContentHash,
    bytes: u64,
    mime: &'static str,
    reused: bool,
}
type FileResult = std::result::Result<Prepared, (i64, &'static str)>;

fn source_relative(path: &str) -> &str {
    path.strip_prefix("./upload/")
        .or_else(|| path.strip_prefix("upload/"))
        .unwrap_or(path)
}

fn hash_source(
    tree: &SourceTree,
    image: &Image,
) -> std::result::Result<(std::fs::File, ContentHash, u64, &'static str), &'static str> {
    let source = tree
        .open_file(source_relative(&image.path))
        .map_err(|e| match e {
            yhotos_storage::StorageError::NotFound => "missing source file",
            _ => "unsafe or unreadable source file",
        })?;
    let length = source.len();
    if length == 0 || length > MAX_BYTES {
        return Err("source file empty or exceeds 2 GiB");
    }
    let mut file = source.into_file();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut total = 0_u64;
    let mut mime = None;
    loop {
        let count = file.read(&mut buffer).map_err(|_| "source read failed")?;
        if count == 0 {
            break;
        }
        let chunk = buffer.get(..count).ok_or("source read failed")?;
        if total == 0 {
            mime = if chunk.starts_with(&[0xff, 0xd8, 0xff]) {
                Some("image/jpeg")
            } else if chunk.starts_with(b"\x89PNG\r\n\x1a\n") {
                Some("image/png")
            } else {
                None
            };
        }
        total += u64::try_from(count).map_err(|_| "source read failed")?;
        if total > MAX_BYTES {
            return Err("source file exceeds 2 GiB");
        }
        hasher.update(chunk);
    }
    if total != length {
        return Err("source changed during read");
    }
    let mime = mime.ok_or("unsupported original format (expected JPEG or PNG)")?;
    let hash = hasher
        .finalize()
        .to_hex()
        .as_str()
        .parse()
        .map_err(|_| "hash failed")?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "source seek failed")?;
    Ok((file, hash, total, mime))
}

async fn prepare(tree: Arc<SourceTree>, image: Image, store: Option<Store>) -> FileResult {
    let id = image.id;
    let (image, read) = tokio::task::spawn_blocking(move || {
        let read = hash_source(&tree, &image);
        (image, read)
    })
    .await
    .map_err(|_| (id, "source worker failed"))?;
    let (file, hash, bytes, mime) = read.map_err(|reason| (id, reason))?;
    let mut reused = false;
    if let Some(store) = store {
        if store.exists(Area::Originals, &hash, None).await {
            reused = true;
        } else {
            let put = store
                .put_original(tokio::fs::File::from_std(file), MAX_BYTES)
                .await
                .map_err(|_| (id, "original publication failed"))?;
            if put.hash != hash || put.bytes != bytes {
                return Err((id, "source changed during copy"));
            }
            reused = put.existed;
        }
    }
    Ok(Prepared {
        image,
        hash,
        bytes,
        mime,
        reused,
    })
}

fn validate_image(image: &Image, report: &mut Report) -> bool {
    if image.id <= 0
        || image.level < 0
        || image.filename.is_empty()
        || image.filename.len() > 4096
        || image.filename.contains(['\0', '/', '\\'])
    {
        report
            .skipped
            .push(problem(Some(image.id), "invalid image metadata"));
        return false;
    }
    true
}

#[allow(clippy::too_many_lines, clippy::print_stdout)]
pub(crate) async fn execute(
    options: &Options,
    tree: Arc<SourceTree>,
    mut plan: Plan,
    destination: Option<&(Db, Store)>,
    report: &mut Report,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut prepared = Vec::new();
    for image in std::mem::take(&mut plan.images) {
        if !validate_image(&image, report) {
            continue;
        }
        if !plan.members.contains_key(&image.id) {
            report
                .skipped
                .push(problem(Some(image.id), "image has no supported category"));
            continue;
        }
        let store = destination.map(|(_, store)| store.clone());
        tasks.spawn(prepare(tree.clone(), image, store));
        if tasks.len() >= options.concurrency {
            collect_one(&mut tasks, &mut prepared, report).await?;
        }
    }
    while !tasks.is_empty() {
        collect_one(&mut tasks, &mut prepared, report).await?;
    }
    prepared.sort_by_key(|p| p.image.id);
    let mut album_counts = BTreeMap::new();
    for p in &prepared {
        report.bytes += p.bytes;
        report.reused_originals += usize::from(p.reused);
        for (category, _) in plan.members.get(&p.image.id).into_iter().flatten() {
            *album_counts.entry(*category).or_insert(0) += 1;
            report.photo_rows += 1;
        }
    }
    for album in &plan.albums {
        let photos = album_counts.get(&album.source.id).copied().unwrap_or(0);
        println!("album {}: {photos} photos", album.source.id);
        report.album_progress.push(AlbumProgress {
            id: album.source.id,
            photos,
        });
    }
    // Validate metadata in both dry and real runs, never emitting untrusted text.
    for p in &mut prepared {
        let image = &mut p.image;
        if !matches!((image.lat,image.lon),(Some(a),Some(b)) if a.is_finite() && b.is_finite() && (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&b))
        {
            if image.lat.is_some() || image.lon.is_some() {
                report
                    .unsupported
                    .push(problem(Some(image.id), "invalid or incomplete GPS omitted"));
            }
            image.lat = None;
            image.lon = None;
        }
        if image.width.is_some_and(|n| n <= 0) || image.height.is_some_and(|n| n <= 0) {
            report.unsupported.push(problem(
                Some(image.id),
                "invalid dimensions ignored; ingest will measure originals",
            ));
        }
    }
    for album in &plan.albums {
        if !album
            .source
            .cover
            .is_some_and(|cover| prepared.iter().any(|p| p.image.id == cover))
        {
            report.unsupported.push(problem(
                Some(album.source.id),
                "album cover missing or skipped",
            ));
        }
    }
    let Some((db, _)) = destination else {
        return Ok(());
    };
    let mut tx = db.pool().begin().await?;
    // Serialize import identities across concurrent invocations.
    sqlx::query!("SELECT pg_advisory_xact_lock(220022)")
        .execute(&mut *tx)
        .await?;
    let album_sources: Vec<_> = plan.albums.iter().map(|a| a.source.id).collect();
    let tag_sources: Vec<_> = plan.tags.keys().copied().collect();
    migration::prepare_slugs(&mut tx, &album_sources, &tag_sources).await?;
    migration::reconcile_redirects(&mut tx).await?;
    let mut album_ids = BTreeMap::<i64, AlbumId>::new();
    for a in &plan.albums {
        let parent = a
            .source
            .parent
            .map(|p| album_ids.get(&p).copied().ok_or(Error::SourceData))
            .transpose()?;
        let id = migration::upsert_album(
            &mut tx,
            a.source.id,
            parent,
            &a.slug,
            &a.title,
            &a.description,
            a.order,
            a.source.status != "public",
        )
        .await?;
        album_ids.insert(a.source.id, id);
        migration::insert_redirect(
            &mut tx,
            &format!("/category/{}", a.source.id),
            ShareTarget::Album(id),
        )
        .await?;
        migration::insert_redirect(
            &mut tx,
            &format!("/index.php?/category/{}", a.source.id),
            ShareTarget::Album(id),
        )
        .await?;
        // Exact numeric/url-name links remain valid even if a name is ambiguous.
        let url_name = a
            .source
            .url_name
            .as_deref()
            .filter(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            })
            .unwrap_or(a.slug.as_str());
        migration::insert_redirect(
            &mut tx,
            &format!("/index.php?/category/{}-{url_name}", a.source.id),
            ShareTarget::Album(id),
        )
        .await?;
    }
    for (name, source_id) in &plan.aliases {
        let id = album_ids.get(source_id).ok_or(Error::SourceData)?;
        let mut paths = vec![format!("/legacy/{name}")];
        // Existing application routes keep their meaning. Do not store bare
        // aliases that cannot reach the authorized legacy handler.
        if matches!(
            name.as_str(),
            "." | ".." | "map" | "calendar" | "healthz" | "login" | "logout"
        ) {
            report.collisions.push(problem(
                Some(*source_id),
                "bare album alias conflicts with an application path; prefixed alias retained",
            ));
        } else {
            paths.push(format!("/{name}"));
        }
        // The PHP parser rejects malformed/zero numeric prefixes. Such names
        // remain reachable through their bare and /legacy/ aliases.
        let prefix = name.split('-').next().unwrap_or("");
        if !prefix.bytes().all(|b| b.is_ascii_digit())
            || prefix.parse::<u64>().is_ok_and(|id| id > 0)
        {
            paths.push(format!("/index.php?/category/{name}"));
        }
        for path in paths {
            migration::insert_redirect(&mut tx, &path, ShareTarget::Album(*id)).await?;
        }
    }
    let mut tag_ids = BTreeMap::new();
    for (source, (slug, name)) in &plan.tags {
        let id = migration::upsert_tag(&mut tx, *source, slug, name).await?;
        tag_ids.insert(*source, id);
    }
    let mut photo_ids = BTreeMap::<(i64, i64), PhotoId>::new();
    for p in prepared {
        let image = p.image;
        let title = image
            .name
            .as_deref()
            .map(plan::text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                plan::text(
                    image
                        .filename
                        .rsplit_once('.')
                        .map_or(image.filename.as_str(), |(stem, _)| stem),
                )
            });
        let mut batch = Vec::new();
        for (category, order) in plan.members.get(&image.id).into_iter().flatten() {
            batch.push(ImportPhoto {
                image_id: image.id,
                category_id: *category,
                original: NewPhoto {
                    album_id: *album_ids.get(category).ok_or(Error::SourceData)?,
                    title: title.clone(),
                    original_hash: p.hash.to_string(),
                    original_name: image.filename.clone(),
                    original_bytes: i64::try_from(p.bytes).map_err(|_| Error::SourceData)?,
                    mime: p.mime.into(),
                    kind: MediaKind::Image,
                },
                description: plan::plain(image.description.as_deref().unwrap_or("")),
                author: image.author.as_deref().map(plan::text),
                taken_at: image
                    .taken
                    .map(|date| date.assume_offset(options.source_offset)),
                taken_at_local: image.taken,
                posted_at: image.posted.assume_offset(options.source_offset),
                sort_order: *order,
                restricted: image.level > 0,
                gps_lat: image.lat,
                gps_lon: image.lon,
            });
        }
        let ids = migration::upsert_photos(&mut tx, &batch).await?;
        for (row, id) in batch.iter().zip(&ids) {
            migration::clear_manual_tags(&mut tx, *id).await?;
            for tag in plan.tag_links.get(&image.id).into_iter().flatten() {
                tags::attach(
                    &mut tx,
                    *id,
                    *tag_ids.get(tag).ok_or(Error::SourceData)?,
                    TagSource::Manual,
                    None,
                )
                .await?;
            }
            photo_ids.insert((image.id, row.category_id), *id);
            migration::insert_redirect(
                &mut tx,
                &format!("/picture.php?/{}/category/{}", image.id, row.category_id),
                ShareTarget::Photo(*id),
            )
            .await?;
        }
        if let Some(first) = ids.first() {
            migration::insert_redirect(
                &mut tx,
                &format!("/picture/{}", image.id),
                ShareTarget::Photo(*first),
            )
            .await?;
            migration::insert_redirect(
                &mut tx,
                &format!("/picture.php?/{}", image.id),
                ShareTarget::Photo(*first),
            )
            .await?;
        }
    }
    for a in &plan.albums {
        let source_id = a.source.id;
        let cover = a.source.cover.and_then(|image| {
            photo_ids.get(&(image, source_id)).copied().or_else(|| {
                photo_ids
                    .iter()
                    .find_map(|((id, _), photo)| (*id == image).then_some(*photo))
            })
        });
        albums::set_cover(
            &mut tx,
            *album_ids.get(&source_id).ok_or(Error::SourceData)?,
            cover,
        )
        .await?;
    }
    // Retire every previously imported copy absent from this successful plan,
    // including skipped/missing originals. Retained rows and retirement commit
    // together, so a removed public membership cannot outlive new restrictions.
    let retained: Vec<_> = photo_ids.values().copied().collect();
    migration::reconcile_photos(&mut tx, &retained).await?;
    tx.commit().await?;
    Ok(())
}

async fn collect_one(
    tasks: &mut JoinSet<FileResult>,
    prepared: &mut Vec<Prepared>,
    report: &mut Report,
) -> Result<()> {
    match tasks
        .join_next()
        .await
        .ok_or(Error::Worker)?
        .map_err(|_| Error::Worker)?
    {
        Ok(file) => prepared.push(file),
        Err((id, reason)) => report.skipped.push(problem(Some(id), reason)),
    }
    Ok(())
}
