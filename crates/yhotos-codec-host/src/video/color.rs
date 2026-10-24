//! Validated H.273 colour descriptions, with explicit defaults for missing tags.
use super::{Failure, rejected};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Primaries {
    Bt709 = 1,
    Bt470bg = 5,
    Smpte170m = 6,
    Smpte240m = 7,
    Bt2020 = 9,
    DisplayP3 = 12,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Transfer {
    Bt709 = 1,
    Gamma22 = 4,
    Gamma28 = 5,
    Smpte170m = 6,
    Smpte240m = 7,
    Linear = 8,
    Srgb = 13,
    Bt2020Ten = 14,
    Bt2020Twelve = 15,
    Pq = 16,
    Hlg = 18,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Matrix {
    Bt709 = 1,
    Bt470bg = 5,
    Smpte170m = 6,
    Smpte240m = 7,
    Bt2020Ncl = 9,
    Bt2020Cl = 10,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Color {
    primaries: Primaries,
    transfer: Transfer,
    matrix: Matrix,
    full_range: bool,
}
fn specified(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !matches!(*s, "unknown" | "unspecified"))
}
impl Color {
    pub const BT709: Self = Self {
        primaries: Primaries::Bt709,
        transfer: Transfer::Bt709,
        matrix: Matrix::Bt709,
        full_range: false,
    };

    // Untagged camera YCbCr defaults: BT.601 for SD (525/625-line), BT.709
    // for HD. Explicit tags always win, including partially tagged P3/HDR.
    // No metadata can recover an entirely untagged wide-gamut recording.
    pub fn parse(
        width: u32,
        height: u32,
        primaries: Option<&str>,
        transfer: Option<&str>,
        matrix: Option<&str>,
        range: Option<&str>,
    ) -> Result<Self, Failure> {
        let transfer = match specified(transfer) {
            None => None,
            Some("bt709") => Some(Transfer::Bt709),
            Some("bt470m") => Some(Transfer::Gamma22),
            Some("bt470bg") => Some(Transfer::Gamma28),
            Some("smpte170m") => Some(Transfer::Smpte170m),
            Some("smpte240m") => Some(Transfer::Smpte240m),
            Some("linear") => Some(Transfer::Linear),
            Some("iec61966-2-1") => Some(Transfer::Srgb),
            Some("bt2020-10") => Some(Transfer::Bt2020Ten),
            Some("bt2020-12") => Some(Transfer::Bt2020Twelve),
            Some("smpte2084") => Some(Transfer::Pq),
            Some("arib-std-b67") => Some(Transfer::Hlg),
            _ => return Err(rejected()),
        };
        let matrix = match specified(matrix) {
            None => None,
            Some("bt709") => Some(Matrix::Bt709),
            Some("bt470bg") => Some(Matrix::Bt470bg),
            Some("smpte170m") => Some(Matrix::Smpte170m),
            Some("smpte240m") => Some(Matrix::Smpte240m),
            Some("bt2020nc") => Some(Matrix::Bt2020Ncl),
            Some("bt2020c") => Some(Matrix::Bt2020Cl),
            _ => return Err(rejected()),
        };
        let default_primaries = match matrix {
            Some(Matrix::Bt470bg) => Primaries::Bt470bg,
            Some(Matrix::Smpte170m) => Primaries::Smpte170m,
            Some(Matrix::Smpte240m) => Primaries::Smpte240m,
            Some(Matrix::Bt2020Ncl | Matrix::Bt2020Cl) => Primaries::Bt2020,
            None if matches!(transfer, Some(Transfer::Pq | Transfer::Hlg)) => Primaries::Bt2020,
            None if width <= 1024 && height <= 486 => Primaries::Smpte170m,
            None if width <= 1024 && height <= 576 => Primaries::Bt470bg,
            Some(Matrix::Bt709) | None => Primaries::Bt709,
        };
        let primaries = match specified(primaries) {
            None => default_primaries,
            Some("bt709") => Primaries::Bt709,
            Some("bt470bg") => Primaries::Bt470bg,
            Some("smpte170m") => Primaries::Smpte170m,
            Some("smpte240m") => Primaries::Smpte240m,
            Some("bt2020") => Primaries::Bt2020,
            Some("smpte432") => Primaries::DisplayP3,
            _ => return Err(rejected()),
        };
        Ok(Self {
            primaries,
            transfer: transfer.unwrap_or(match primaries {
                Primaries::DisplayP3 => Transfer::Srgb,
                Primaries::Bt2020 => Transfer::Bt2020Ten,
                _ => Transfer::Bt709,
            }),
            matrix: matrix.unwrap_or(match primaries {
                Primaries::Bt709 | Primaries::DisplayP3 => Matrix::Bt709,
                Primaries::Bt470bg => Matrix::Bt470bg,
                Primaries::Smpte170m => Matrix::Smpte170m,
                Primaries::Smpte240m => Matrix::Smpte240m,
                Primaries::Bt2020 => Matrix::Bt2020Ncl,
            }),
            full_range: match specified(range) {
                None | Some("tv") => false,
                Some("pc") => true,
                _ => return Err(rejected()),
            },
        })
    }

    pub fn hdr(self) -> bool {
        matches!(self.transfer, Transfer::Pq | Transfer::Hlg)
    }

    // Only enum discriminants enter the filter graph, never ffprobe strings.
    pub fn filter(self) -> String {
        if self == Self::BT709 {
            return "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=limited,"
                .into();
        }
        if self.hdr() {
            let input = format!(
                "pin={}:tin={}:min={}:rin={}",
                self.primaries as u8,
                self.transfer as u8,
                self.matrix as u8,
                if self.full_range { "full" } else { "limited" }
            );
            format!(
                "zscale={input}:t=linear:npl=100,format=gbrpf32le,tonemap=tonemap=hable:desat=0,zscale=p=bt709:t=bt709:m=bt709:r=tv,"
            )
        } else {
            // colorspace uses the camera OETF for SDR, including the sRGB
            // toe. zscale's display-referred BT.1886 curve is kept for HDR's
            // existing tone-mapping path only.
            format!(
                "colorspace=iprimaries={}:itrc={}:ispace={}:irange={}:all=bt709:range=tv:format=yuv420p,",
                self.primaries as u8,
                self.transfer as u8,
                self.matrix as u8,
                if self.full_range { "pc" } else { "tv" }
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn untagged_camera_defaults_and_partial_tags_are_explicit() {
        for (width, height, primaries) in [
            (720, 480, Primaries::Smpte170m),
            (720, 576, Primaries::Bt470bg),
            (1920, 1080, Primaries::Bt709),
        ] {
            let color = Color::parse(width, height, None, None, None, None)
                .ok()
                .unwrap();
            assert_eq!(color.primaries, primaries);
            assert_eq!(color.transfer, Transfer::Bt709);
            assert!(!color.full_range);
        }
        let p3 = Color::parse(1920, 1080, Some("smpte432"), None, None, None)
            .ok()
            .unwrap();
        assert_eq!(p3.transfer, Transfer::Srgb);
        assert_eq!(p3.matrix, Matrix::Bt709);
        let sd = Color::parse(1920, 1080, None, None, Some("smpte170m"), None)
            .ok()
            .unwrap();
        assert_eq!(sd.primaries, Primaries::Smpte170m);
        assert_eq!(
            Color::parse(
                1920,
                1080,
                Some("unknown"),
                Some("unknown"),
                Some("unknown"),
                Some("unknown")
            )
            .ok(),
            Some(Color::BT709)
        );
    }

    #[test]
    fn unsupported_tags_cannot_enter_a_filter_graph() {
        for tag in ["", "reserved", "bt709,format=gray", "999"] {
            assert!(Color::parse(1920, 1080, Some(tag), None, None, None).is_err());
            assert!(Color::parse(1920, 1080, None, Some(tag), None, None).is_err());
            assert!(Color::parse(1920, 1080, None, None, Some(tag), None).is_err());
            assert!(Color::parse(1920, 1080, None, None, None, Some(tag)).is_err());
        }
    }

    #[test]
    fn bt709_passes_through_and_hdr_keeps_hable_tone_mapping() {
        assert!(!Color::BT709.filter().contains("zscale"));
        for transfer in ["smpte2084", "arib-std-b67"] {
            let hdr = Color::parse(1920, 1080, None, Some(transfer), None, None)
                .ok()
                .unwrap();
            assert!(hdr.hdr());
            assert_eq!(hdr.primaries, Primaries::Bt2020);
            assert!(hdr.filter().contains("tonemap=tonemap=hable:desat=0"));
            assert!(
                hdr.filter()
                    .ends_with("zscale=p=bt709:t=bt709:m=bt709:r=tv,")
            );
        }
    }
}
