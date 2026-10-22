//! Validated presentation values. Only generated numeric/base64 CSS bypasses escaping.
use super::grid::justified_rows;
use crate::{error::HttpError, files::valid_derivative_selector};
use askama::Template;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::fmt::Write;
use yhotos_core::Slug;
use yhotos_db::{
    albums::{AlbumCard, AlbumPage, Breadcrumb},
    photos::{PageDerivative, PagePhoto, PageSort, PhotoPage},
};

/// Trusted request context and compiled asset URLs used by every public template.
pub struct PageContext<'a> {
    /// CSP nonce created by the security middleware.
    pub nonce: &'a str,
    /// Content-addressed local stylesheet URL.
    pub stylesheet: &'a str,
    /// Content-addressed `ThumbHash` island URL.
    pub script: &'a str,
    /// Validated deployment URL for absolute Open Graph images.
    pub public_base_url: &'a str,
}
/// Capabilities of the independently verified scope used to view this photo.
pub(crate) struct PhotoControls<'a> {
    pub csrf: &'a str,
    pub download: bool,
    pub like: bool,
    pub flag: bool,
}
struct Actions<'a> {
    photo: yhotos_core::PhotoId,
    csrf: &'a str,
    like: bool,
    flag: bool,
}
struct OpenGraph {
    image: String,
    width: u32,
    height: u32,
    kind: &'static str,
}
struct Image {
    src: String,
    jpeg: Sources,
    avif: Sources,
    jxl: Sources,
    hdr_jpeg: Sources,
    hdr_avif: Sources,
    hdr_jxl: Sources,
    sizes: String,
    width: u32,
    height: u32,
    alt: String,
    thumbhash: String,
    loading: &'static str,
}
struct Sources {
    all: String,
    mobile: String,
}
struct Cell {
    index: usize,
    href: String,
    image: Image,
    badge: &'static str,
}
struct Row {
    cells: Vec<Cell>,
}
struct Card {
    href: String,
    title: String,
    description: String,
    count: u64,
    image: Option<Image>,
}
struct Crumb {
    href: String,
    title: String,
}
struct Download {
    href: String,
    label: String,
}

fn invalid() -> HttpError {
    HttpError::Internal(anyhow::anyhow!("invalid page metadata"))
}
fn path(value: &str) -> Result<String, HttpError> {
    if value.is_empty() || value.len() > 4096 || !value.split('/').all(|s| Slug::new(s).is_ok()) {
        return Err(invalid());
    }
    Ok(format!("/a/{value}"))
}
fn derivative_url(d: &PageDerivative) -> Option<String> {
    if d.hash.len() != 64
        || !d
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !valid_derivative_selector(&d.variant, &d.format)
        || d.w == 0
        || d.h == 0
        || d.w > 100_000
        || d.h > 100_000
    {
        return None;
    }
    Some(format!(
        "/i/{}/{}.{}{}",
        d.hash,
        d.variant,
        d.format,
        if d.hdr { ".hdr" } else { "" }
    ))
}
fn dimensions(photo: &PagePhoto) -> (u32, u32) {
    photo
        .width
        .zip(photo.height)
        .filter(|&(w, h)| w > 0 && h > 0 && w <= 100_000 && h <= 100_000)
        .map(|(w, h)| {
            if matches!(photo.orientation, Some(5..=8)) {
                (h, w)
            } else {
                (w, h)
            }
        })
        .or_else(|| {
            photo
                .derivatives
                .iter()
                .filter(|d| derivative_url(d).is_some())
                // Without original dimensions, prefer non-square representations
                // over square thumbnails and break ties independently of row order.
                .max_by_key(|d| (d.w != d.h, d.w.max(d.h), d.w, d.h, &d.variant, &d.hash))
                .map(|d| (d.w, d.h))
        })
        .unwrap_or((4, 3))
}
fn full_frame((w, h): (u32, u32), d: &PageDerivative) -> bool {
    // Resizing may round either dimension by one pixel. Cropped representations
    // must never share a source set or fallback with the full-frame image.
    (u64::from(d.w) * u64::from(h)).abs_diff(u64::from(d.h) * u64::from(w)) <= u64::from(w.max(h))
}
fn source(
    photo: &PagePhoto,
    format: &str,
    hdr: bool,
    dimensions: (u32, u32),
    cap_mobile: bool,
) -> Sources {
    let mut derivatives: Vec<_> = photo
        .derivatives
        .iter()
        .filter(|d| d.format == format && d.hdr == hdr && full_frame(dimensions, d))
        .filter_map(|d| derivative_url(d).map(|url| (d, url)))
        .collect();
    // Validate and construct each URL once, sharing it across mobile and desktop.
    // Variant and digest break width ties independently of database row order.
    derivatives.sort_unstable_by_key(|(d, _)| (d.w, &d.variant, d.h, &d.hash));
    derivatives.dedup_by_key(|(d, _)| d.w);
    let mut sources = Sources {
        all: String::new(),
        mobile: String::new(),
    };
    for (d, url) in derivatives {
        if !sources.all.is_empty() {
            sources.all.push_str(", ");
        }
        let _ = write!(sources.all, "{url} {}w", d.w);
        // Keep accurate slot sizes at high DPR without allowing album images
        // on narrow screens to exceed the 640-pixel image transfer budget.
        if cap_mobile && d.w <= 640 {
            if !sources.mobile.is_empty() {
                sources.mobile.push_str(", ");
            }
            let _ = write!(sources.mobile, "{url} {}w", d.w);
        }
    }
    sources
}
fn image(photo: &PagePhoto, sizes: String, eager: bool, cap_mobile: bool) -> Image {
    let (width, height) = dimensions(photo);
    let jpeg = source(photo, "jpeg", false, (width, height), cap_mobile);
    let src = photo.derivatives.iter().filter(|d| d.format == "jpeg" && !d.hdr && full_frame((width, height), d) && derivative_url(d).is_some())
        .min_by_key(|d| (d.w.abs_diff(1280), d.w, &d.variant, d.h, &d.hash)).and_then(derivative_url).unwrap_or_else(|| "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='4'%20height='3'%3E%3Cpath%20fill='%23777'%20d='M0%200h4v3H0z'/%3E%3C/svg%3E".into());
    let thumbhash = photo
        .thumbhash
        .as_deref()
        .filter(|v| v.len() <= 88)
        .and_then(|v| STANDARD.decode(v).ok())
        .filter(|v| yhotos_core::thumbhash::validate(v).is_ok())
        .map_or_else(String::new, |v| STANDARD.encode(v));
    Image {
        src,
        jpeg,
        avif: source(photo, "avif", false, (width, height), cap_mobile),
        jxl: source(photo, "jxl", false, (width, height), cap_mobile),
        hdr_jpeg: source(photo, "jpeg", true, (width, height), cap_mobile),
        hdr_avif: source(photo, "avif", true, (width, height), cap_mobile),
        hdr_jxl: source(photo, "jxl", true, (width, height), cap_mobile),
        sizes,
        width,
        height,
        alt: photo.title.clone(),
        thumbhash,
        loading: if eager { "eager" } else { "lazy" },
    }
}
fn og(
    photo: Option<&PagePhoto>,
    context: &PageContext<'_>,
    kind: &'static str,
) -> Option<OpenGraph> {
    let d = photo?
        .derivatives
        .iter()
        .filter(|d| d.format == "jpeg" && !d.hdr && derivative_url(d).is_some())
        // Prefer the sharing variant even when a larger JPEG exists. Older or
        // incomplete derivative plans can still supply their largest SDR JPEG.
        .max_by_key(|d| {
            (
                d.variant == "large",
                d.w.max(d.h),
                d.w,
                d.h,
                &d.variant,
                &d.hash,
            )
        })?;
    Some(OpenGraph {
        image: format!(
            "{}{}",
            context.public_base_url.trim_end_matches('/'),
            derivative_url(d)?
        ),
        width: d.w,
        height: d.h,
        kind,
    })
}
fn cards(albums: &[AlbumCard]) -> Result<Vec<Card>, HttpError> {
    albums
        .iter()
        .enumerate()
        .map(|(index, a)| {
            Ok(Card {
                href: path(&a.path)?,
                title: a.title.clone(),
                description: a.description.clone(),
                count: a.count,
                image: a.cover.as_ref().map(|p| {
                    image(
                        p,
                        "(max-width: 600px) calc(100vw - 32px), (max-width: 1232px) 50vw, 600px"
                            .into(),
                        index < 2,
                        true,
                    )
                }),
            })
        })
        .collect()
}
fn card_styles(albums: &[AlbumCard]) -> String {
    let mut styles = String::new();
    for (index, album) in albums.iter().enumerate() {
        if let Some(photo) = &album.cover {
            let _ = write!(
                styles,
                ".album-card:nth-child({}) img{{{}}}",
                index + 1,
                background(photo, index < 2)
            );
        }
    }
    styles
}
fn crumbs(chain: &[Breadcrumb]) -> Result<Vec<Crumb>, HttpError> {
    chain
        .iter()
        .map(|a| {
            Ok(Crumb {
                href: path(&a.path)?,
                title: a.title.clone(),
            })
        })
        .collect()
}
fn color(photo: &PagePhoto) -> String {
    format!(
        "#{:06x}",
        photo
            .dominant_colors
            .first()
            .copied()
            .filter(|n| (0..=0x00ff_ffff).contains(n))
            .unwrap_or(0x0077_7777)
    )
}
fn background(photo: &PagePhoto, eager: bool) -> String {
    let mut css = format!("background-color:{};", color(photo));
    if eager
        && let Some(hash) = photo
            .thumbhash
            .as_deref()
            .filter(|v| v.len() <= 88)
            .and_then(|v| STANDARD.decode(v).ok())
        && let Ok(decoded) = yhotos_core::thumbhash::decode(&hash)
    {
        let _ = write!(
            css,
            "background-image:url(data:image/png;base64,{});background-size:cover;",
            STANDARD.encode(decoded.png())
        );
    }
    css
}
#[derive(Template)]
#[template(path = "default/home.html")]
struct HomeTemplate<'a> {
    context: &'a PageContext<'a>,
    title: &'static str,
    description: &'static str,
    styles: String,
    albums: Vec<Card>,
}
/// Renders visible top-level albums with no client-side navigation dependency.
///
/// # Errors
/// Rejects malformed stored paths or a template rendering failure.
pub fn render_home(albums: &[AlbumCard], context: &PageContext<'_>) -> Result<String, HttpError> {
    HomeTemplate {
        context,
        title: "Yhotos photo gallery",
        description: "Browse the photo gallery",
        styles: card_styles(albums),
        albums: cards(albums)?,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))
}
#[derive(Template)]
#[template(path = "default/album.html")]
struct AlbumTemplate<'a> {
    context: &'a PageContext<'a>,
    title: &'a str,
    description: &'a str,
    og: Option<OpenGraph>,
    styles: String,
    albums: Vec<Card>,
    chain: Vec<Crumb>,
    rows: Vec<Row>,
    total: u64,
    number: u32,
    previous: Option<String>,
    next: Option<String>,
    sort: &'static str,
    island: String,
}
/// Renders a sixty-photo page including O(n) grid math and first-row PNG placeholders.
///
/// # Errors
/// Rejects malformed stored paths and template/JSON serialization failures.
pub fn render_album(
    page: &AlbumPage,
    number: u32,
    sort: PageSort,
    context: &PageContext<'_>,
) -> Result<String, HttpError> {
    let ratios: Vec<_> = page
        .photos
        .iter()
        .map(|p| {
            let (w, h) = dimensions(p);
            (f64::from(w) / f64::from(h)).clamp(0.1, 10.0)
        })
        .collect();
    let mut styles = card_styles(&page.children);
    let rows = justified_rows(&ratios)
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let cells = page
                .photos
                .iter()
                .enumerate()
                .take(row.end)
                .skip(row.start)
                .map(|(index, p)| {
                    let ratio = ratios.get(index).copied().unwrap_or(1.0);
                    let percent = ratio / row.ratio * 100.0;
                    let (w, h) = dimensions(p);
                    let _ = write!(
                        styles,
                        "#cell{index}{{flex:{ratio:.5} 1 {percent:.5}%;aspect-ratio:{w}/{h};{}}}",
                        background(p, row_index == 0)
                    );
                    Cell {
                        index,
                        href: format!("/p/{}?sort={}", p.short_id, sort.as_str()),
                        image: image(
                            p,
                            format!(
                                "(max-width: 1232px) calc((100vw - 32px) * {:.6}), {:.0}px",
                                percent / 100.0,
                                1200.0 * percent / 100.0
                            ),
                            row_index == 0,
                            true,
                        ),
                        badge: match p.kind.as_str() {
                            "raw" => "RAW",
                            "video" => "Play video",
                            _ => "",
                        },
                    }
                })
                .collect();
            Row { cells }
        })
        .collect();
    let href = path(&page.album.path)?;
    let page_url = |number| format!("{href}?page={number}&sort={}", sort.as_str());
    let island=serde_json::to_string(&serde_json::json!({"photos":page.photos.iter().map(|p|serde_json::json!({"id":p.short_id,"title":p.title})).collect::<Vec<_>>()})).map_err(|e|HttpError::Internal(e.into()))?.replace("</","<\\/")
        .replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    AlbumTemplate {
        context,
        title: &page.album.title,
        description: &page.album.description,
        og: og(page.album.cover.as_ref(), context, "website"),
        styles,
        albums: cards(&page.children)?,
        chain: crumbs(&page.chain)?,
        rows,
        total: page.total,
        number,
        previous: (number > 1).then(|| page_url(number - 1)),
        next: (u64::from(number) * 60 < page.total).then(|| page_url(number + 1)),
        sort: sort.as_str(),
        island,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))
}
#[derive(Template)]
#[template(path = "default/photo.html")]
struct PhotoTemplate<'a> {
    context: &'a PageContext<'a>,
    title: &'a str,
    description: &'a str,
    og: Option<OpenGraph>,
    styles: String,
    chain: Vec<Crumb>,
    image: Image,
    previous: Option<String>,
    next: Option<String>,
    downloads: Vec<Download>,
    actions: Option<Actions<'a>>,
    exif: Vec<(&'static str, String)>,
    map: Option<String>,
    badge: &'static str,
}
fn filename(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= 180)
        .last()
        .unwrap_or(0);
    let limited = if value.len() <= 180 {
        value
    } else {
        value.get(..end).unwrap_or("")
    };
    for b in limited.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            encoded.push(char::from(b));
        } else {
            let _ = write!(encoded, "%{b:02X}");
        }
    }
    if encoded.is_empty() {
        "photo".into()
    } else {
        encoded
    }
}
fn downloads(p: &PagePhoto) -> Vec<Download> {
    let mut downloads = vec![Download {
        href: format!("/o/{}/{}", p.original_hash, filename(&p.original_name)),
        label: "Download original".into(),
    }];
    let mut sizes: Vec<_> = p
        .derivatives
        .iter()
        .filter(|d| derivative_url(d).is_some())
        .collect();
    // One link per size, preferring SDR JPEG, then AVIF, then JXL. Retain sizes
    // available only in a modern encoding or HDR, with a deterministic tie break.
    sizes.sort_unstable_by_key(|d| {
        (
            &d.variant,
            d.hdr,
            match d.format.as_str() {
                "jpeg" => 0,
                "avif" => 1,
                _ => 2,
            },
            d.w,
            d.h,
            &d.hash,
        )
    });
    sizes.dedup_by_key(|d| &d.variant);
    sizes.sort_unstable_by_key(|d| (d.w, d.h, &d.variant));
    downloads.extend(sizes.into_iter().filter_map(|d| {
        derivative_url(d).map(|url| Download {
            href: url,
            label: format!("Download {} ({} × {})", d.variant, d.w, d.h),
        })
    }));
    downloads
}
/// Renders a viewport-sized photo with metadata, downloads and visible neighbours.
///
/// # Errors
/// Rejects malformed stored paths, content digests or template rendering failure.
pub fn render_photo(
    page: &PhotoPage,
    sort: PageSort,
    context: &PageContext<'_>,
) -> Result<String, HttpError> {
    render_photo_with_controls(page, sort, context, None)
}
pub(crate) fn render_scoped_photo(
    page: &PhotoPage,
    sort: PageSort,
    context: &PageContext<'_>,
    controls: PhotoControls<'_>,
) -> Result<String, HttpError> {
    render_photo_with_controls(page, sort, context, Some(controls))
}
fn render_photo_with_controls(
    page: &PhotoPage,
    sort: PageSort,
    context: &PageContext<'_>,
    controls: Option<PhotoControls<'_>>,
) -> Result<String, HttpError> {
    let p = &page.photo;
    if p.original_hash.len() != 64
        || !p
            .original_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let mut downloads = downloads(p);
    if controls.as_ref().is_some_and(|c| !c.download) {
        downloads.clear();
    }
    let mut exif = Vec::new();
    for (label, value) in [
        ("Date taken", &p.taken_at),
        ("Camera manufacturer", &p.camera_make),
        ("Camera model", &p.camera_model),
        ("Lens", &p.lens),
        ("Exposure", &p.shutter),
    ] {
        if let Some(value) = value {
            exif.push((label, value.clone()));
        }
    }
    for (label, value, suffix) in [
        ("Focal length", p.focal_mm, " mm"),
        ("Aperture", p.aperture, ""),
    ] {
        if let Some(value) = value.filter(|n| n.is_finite() && *n > 0.0) {
            exif.push((label, format!("{value}{suffix}")));
        }
    }
    if let Some(iso) = p.iso.filter(|n| *n > 0) {
        exif.push(("ISO sensitivity", iso.to_string()));
    }
    let map = p
        .gps_lat
        .zip(p.gps_lon)
        .filter(|(lat, lon)| {
            lat.is_finite()
                && lon.is_finite()
                && (-90.0..=90.0).contains(lat)
                && (-180.0..=180.0).contains(lon)
        })
        .map(|(lat, lon)| {
            format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=14/{lat}/{lon}")
        });
    PhotoTemplate {
        context,
        title: &p.title,
        description: &p.description,
        og: og(Some(p), context, "article"),
        styles: format!(".photo-image{{{}}}", background(p, true)),
        chain: crumbs(&page.chain)?,
        image: image(p, "100vw".into(), true, false),
        previous: page
            .previous
            .map(|id| format!("/p/{id}?sort={}", sort.as_str())),
        next: page
            .next
            .map(|id| format!("/p/{id}?sort={}", sort.as_str())),
        downloads,
        actions: controls.map(|c| Actions {
            photo: p.id,
            csrf: c.csrf,
            like: c.like,
            flag: c.flag,
        }),
        exif,
        map,
        badge: match p.kind.as_str() {
            "raw" => "RAW",
            "video" => "Play video",
            _ => "",
        },
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))
}
