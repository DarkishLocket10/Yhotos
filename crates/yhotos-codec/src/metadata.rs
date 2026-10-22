use crate::{CodecError, Limits, headers, native};
use serde::{Deserialize, Serialize};
use std::path::Path;
/// Typed, bounded metadata; GPS is extracted for storage and never copied to derivatives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    /// Camera manufacturer.
    pub make: Option<String>,
    /// Camera model.
    pub model: Option<String>,
    /// Lens model.
    pub lens: Option<String>,
    /// Focal length in millimetres.
    pub focal_mm: Option<f64>,
    /// F-number.
    pub aperture: Option<f64>,
    /// Exposure time in seconds.
    pub shutter: Option<f64>,
    /// ISO sensitivity.
    pub iso: Option<u32>,
    /// EXIF `DateTimeOriginal` wall-clock text, paired with `OffsetTimeOriginal`.
    pub taken_at: Option<String>,
    /// EXIF `OffsetTimeOriginal`, such as +05:30; absent means an unknown timezone.
    pub offset_time_original: Option<String>,
    /// Latitude and longitude in degrees.
    pub gps: Option<(f64, f64)>,
    /// EXIF orientation 1..=8.
    pub orientation: u8,
    /// XMP rating -1..=5.
    pub rating: Option<i8>,
    /// XMP title, at most 256 UTF-8 bytes.
    pub title: Option<String>,
    /// ICC description, at most 256 UTF-8 bytes.
    pub icc_desc: Option<String>,
}
pub(crate) fn cap(s: &str) -> String {
    let mut end = s.len().min(256);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
fn exif(image: &native::Image, name: &str) -> Option<String> {
    image.string(name).map(|s| exif_text(&s))
}
fn exif_text(s: &str) -> String {
    // libvips appends a duplicate pretty value plus its EXIF type. Parentheses
    // in camera/lens names belong to the value, not necessarily that suffix.
    if s.contains(", ASCII,") {
        for (index, _) in s.match_indices(" (") {
            let Some(value) = s.get(..index) else {
                continue;
            };
            let Some(pretty) = s.get(index + 2..) else {
                continue;
            };
            if pretty
                .strip_prefix(value)
                .is_some_and(|rest| rest.starts_with(", ASCII,"))
            {
                return cap(value);
            }
        }
        return cap(s);
    }
    if s.contains(", Rational,") || s.contains(", Short,") || s.contains(", Long,") {
        cap(s.split(" (").next().unwrap_or(s))
    } else {
        cap(s)
    }
}
fn number(s: &str) -> Option<f64> {
    let value = s.split_whitespace().next()?;
    let n = if let Some((a, b)) = value.split_once('/') {
        a.parse::<f64>().ok()? / b.parse::<f64>().ok()?
    } else {
        value.parse().ok()?
    };
    n.is_finite().then_some(n)
}
fn coordinate(image: &native::Image, name: &str, reference: &str, max: f64) -> Option<f64> {
    let value = exif(image, name)?;
    let parts = value
        .split_whitespace()
        .map(number)
        .collect::<Option<Vec<_>>>()?;
    let [deg, min, sec] = parts.as_slice() else {
        return None;
    };
    let mut n = deg + min / 60.0 + sec / 3600.0;
    let r = exif(image, reference)?;
    if r.starts_with('S') || r.starts_with('W') {
        n = -n;
    }
    (n.is_finite() && n.abs() <= max).then_some(n)
}
fn xml_value<'a>(x: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let attr = format!("{name}={quote}");
        if let Some((_, rest)) = x.split_once(&attr) {
            return rest.split(quote).next();
        }
    }
    let tag = format!("<{name}");
    let (_, after) = x.split_once(&tag)?;
    if !after.starts_with(|c: char| c == '>' || c.is_ascii_whitespace()) {
        return None;
    }
    let (_, body) = after.split_once('>')?;
    let end = format!("</{name}>");
    let (body, _) = body.split_once(&end)?;
    // A dc:title usually contains rdf:Alt/rdf:li, not direct text. Stay
    // within that field, so a keyword elsewhere cannot become the title.
    let body = if name == "dc:title" {
        if let Some((_, li)) = body.split_once("<rdf:li") {
            li.split_once('>')?.1
        } else {
            body
        }
    } else {
        body
    };
    let value = body.split('<').next()?.trim();
    (!value.is_empty()).then_some(value)
}
fn xml_text(value: &str) -> String {
    cap(&value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&"))
}
/// Read a fixed metadata subset without evaluating pixels or following XMP links.
///
/// # Errors
/// Returns a typed error when options are invalid or native processing fails.
pub fn metadata(path: &Path) -> Result<Metadata, CodecError> {
    let kind = headers::inspect(path, &Limits::default())?;
    if matches!(kind, crate::Kind::Video { .. }) {
        return Ok(Metadata {
            orientation: 1,
            ..Metadata::default()
        });
    }
    #[cfg(feature = "raw")]
    if crate::raw::candidate(path, &kind) {
        return crate::raw::metadata(path);
    }
    let image = native::Image::load(path, &kind)?;
    Ok(from_image(&image))
}
pub(crate) fn from_image(image: &native::Image) -> Metadata {
    let field = |name| exif(image, name);
    let lat = coordinate(
        image,
        "exif-ifd3-GPSLatitude",
        "exif-ifd3-GPSLatitudeRef",
        90.0,
    );
    let lon = coordinate(
        image,
        "exif-ifd3-GPSLongitude",
        "exif-ifd3-GPSLongitudeRef",
        180.0,
    );
    let xmp = image
        .blob("xmp-data")
        .filter(|b| b.len() <= 1 << 20)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Metadata {
        make: field("exif-ifd0-Make"),
        model: field("exif-ifd0-Model"),
        lens: field("exif-ifd2-LensModel"),
        focal_mm: field("exif-ifd2-FocalLength").as_deref().and_then(number),
        aperture: field("exif-ifd2-FNumber").as_deref().and_then(number),
        shutter: field("exif-ifd2-ExposureTime").as_deref().and_then(number),
        iso: field("exif-ifd2-ISOSpeedRatings")
            .as_deref()
            .and_then(|s| s.parse().ok()),
        taken_at: field("exif-ifd2-DateTimeOriginal"),
        offset_time_original: field("exif-ifd2-OffsetTimeOriginal"),
        gps: lat.zip(lon),
        orientation: u8::try_from(image.orientation())
            .ok()
            .filter(|n| (1..=8).contains(n))
            .unwrap_or(1),
        rating: xml_value(&xmp, "xmp:Rating")
            .and_then(|s| s.parse::<i8>().ok())
            .filter(|n| (-1..=5).contains(n)),
        title: xml_value(&xmp, "dc:title").map(xml_text),
        icc_desc: image
            .blob("icc-profile-data")
            .as_deref()
            .and_then(native::profile_description),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn cap_is_utf8_safe_and_bounded() {
        assert_eq!(super::cap(&"é".repeat(200)).len(), 256);
        assert_eq!(super::cap(&"x".repeat(257)).len(), 256);
    }
}

#[cfg(test)]
mod xmp_tests {
    #[test]
    fn nested_title_and_quoted_rating_are_field_scoped() {
        let xmp = "<rdf:li>keyword</rdf:li><rdf:Description xmp:Rating='4'><dc:title><rdf:Alt><rdf:li xml:lang='x-default'>A &amp; B</rdf:li></rdf:Alt></dc:title></rdf:Description>";
        assert_eq!(super::xml_value(xmp, "xmp:Rating"), Some("4"));
        assert_eq!(
            super::xml_value(xmp, "dc:title")
                .map(super::xml_text)
                .as_deref(),
            Some("A & B")
        );
        assert_eq!(
            super::xml_value("<rdf:li>keyword</rdf:li>", "dc:title"),
            None
        );
    }
}

#[cfg(test)]
mod exif_tests {
    #[test]
    fn camera_parentheses_and_length_caps_preserve_the_value() {
        assert_eq!(
            super::exif_text("Camera (II) (Camera (II), ASCII, 12 components, 12 bytes)"),
            "Camera (II)"
        );
        let value = "x".repeat(200);
        let formatted = format!("{value} ({value}, ASCII, 201 components, 201 bytes)");
        assert_eq!(super::exif_text(&formatted), value);
        assert_eq!(super::exif_text(&"x".repeat(400)).len(), 256);
    }
}
