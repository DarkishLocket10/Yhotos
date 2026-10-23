use super::{Failure, rejected};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Info {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub hdr: bool,
}
#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Format,
}
#[derive(Deserialize)]
struct Format {
    duration: String,
}
#[derive(Deserialize)]
struct Stream {
    codec_type: String,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    color_transfer: Option<String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}
#[derive(Deserialize)]
struct SideData {
    rotation: Option<i32>,
}
// The encoder fixes Main profile, Main tier and 8-bit 4:2:0. Read the actual
// sequence level from the confined probe rather than guessing from dimensions.
pub(super) fn av1_level(bytes: &[u8]) -> Result<u8, Failure> {
    #[derive(Deserialize)]
    struct RenditionProbe {
        streams: Vec<Rendition>,
    }
    #[derive(Deserialize)]
    struct Rendition {
        profile: String,
        level: u8,
        pix_fmt: String,
    }
    let probe: RenditionProbe = serde_json::from_slice(bytes).map_err(|_| rejected())?;
    let [stream] = probe.streams.as_slice() else {
        return Err(rejected());
    };
    if stream.profile != "Main" || stream.pix_fmt != "yuv420p" || stream.level > 23 {
        return Err(rejected());
    }
    Ok(stream.level)
}
pub(super) fn parse(bytes: &[u8], max_duration: u32) -> Result<Info, Failure> {
    let p: Probe = serde_json::from_slice(bytes).map_err(|_| rejected())?;
    let duration: f64 = p.format.duration.parse().map_err(|_| rejected())?;
    if !duration.is_finite()
        || duration <= 0.0
        || duration > f64::from(max_duration)
        || p.streams.is_empty()
        || p.streams.len() > 4
    {
        return Err(rejected());
    }
    let mut video = None;
    for s in p.streams {
        match (s.codec_type.as_str(), s.codec_name.as_deref()) {
            ("video", Some("h264" | "hevc")) if video.is_none() => {
                let (mut width, mut height) = s.width.zip(s.height).ok_or_else(rejected)?;
                if width == 0
                    || height == 0
                    || width > 7680
                    || height > 7680
                    || u64::from(width) * u64::from(height) > 7680 * 4320
                {
                    return Err(rejected());
                }
                for side in s.side_data_list {
                    if let Some(rotation) = side.rotation {
                        if rotation % 90 != 0 {
                            return Err(rejected());
                        }
                        if rotation.rem_euclid(180) != 0 {
                            std::mem::swap(&mut width, &mut height);
                        }
                    }
                }
                video = Some(Info {
                    width,
                    height,
                    duration_ms: u64::try_from(
                        std::time::Duration::try_from_secs_f64(duration)
                            .map_err(|_| rejected())?
                            .as_nanos()
                            .div_ceil(1_000_000),
                    )
                    .map_err(|_| rejected())?,
                    hdr: matches!(
                        s.color_transfer.as_deref(),
                        Some("smpte2084" | "arib-std-b67")
                    ),
                });
            }
            ("audio", Some("aac" | "pcm_s16le" | "pcm_s24le" | "alac")) => {}
            _ => return Err(rejected()),
        }
    }
    video.ok_or_else(rejected)
}
#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn rendition_codec_metadata_requires_main_eight_bit_and_a_valid_level() {
        let valid =
            serde_json::json!({"streams":[{"profile":"Main","level":12,"pix_fmt":"yuv420p"}]});
        assert_eq!(av1_level(valid.to_string().as_bytes()).ok(), Some(12));
        for (field, value) in [
            ("profile", serde_json::json!("High")),
            ("level", serde_json::json!(24)),
            ("pix_fmt", serde_json::json!("yuv420p10le")),
        ] {
            let mut invalid = valid.clone();
            invalid["streams"][0][field] = value;
            assert!(av1_level(invalid.to_string().as_bytes()).is_err());
        }
        assert!(av1_level(br#"{"streams":[]}"#).is_err());
    }
    #[test]
    fn strict_duration_codecs_streams_dimensions_and_rotation() {
        let valid = serde_json::json!({"format":{"duration":"1.5"},"streams":[{"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"side_data_list":[{"rotation":90}]}]});
        let parse_value = |v: &serde_json::Value| parse(v.to_string().as_bytes(), 1800);
        assert_eq!(
            parse_value(&valid).ok().map(|i| (i.width, i.height)),
            Some((1080, 1920))
        );
        for duration in ["NaN", "inf", "-1", "0", "1801"] {
            let mut v = valid.clone();
            v["format"]["duration"] = duration.into();
            assert!(parse_value(&v).is_err());
        }
        for (field, value) in [
            ("codec_name", serde_json::json!("vp9")),
            ("width", serde_json::json!(8000)),
            ("codec_type", serde_json::json!("data")),
        ] {
            let mut v = valid.clone();
            v["streams"][0][field] = value;
            assert!(parse_value(&v).is_err());
        }
        assert!(parse(b"broken", 1800).is_err());
    }
}
