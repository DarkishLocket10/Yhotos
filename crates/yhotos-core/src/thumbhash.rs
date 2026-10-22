//! Bounded `ThumbHash` decoding, following the reference LPQA/DCT algorithm:
//! <https://github.com/evanw/thumbhash>. No image codec or IO is involved.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::indexing_slicing
)]

use std::f64::consts::PI;

/// Invalid, truncated or oversized untrusted placeholder bytes.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid thumbhash")]
pub struct ThumbHashError;

/// A decoded placeholder with each dimension at most 32 pixels.
#[derive(Clone, Debug)]
pub struct Placeholder {
    /// Pixel width, in 1..=32.
    width: u32,
    /// Pixel height, in 1..=32.
    height: u32,
    /// Unpremultiplied row-major RGBA8 pixels.
    rgba: Vec<u8>,
}
struct Channel {
    dc: f64,
    terms: Vec<(usize, usize, f64)>,
}
impl Channel {
    fn read(
        hash: &[u8],
        start: usize,
        index: &mut usize,
        nx: usize,
        ny: usize,
        dc: f64,
        scale: f64,
    ) -> Result<Self, ThumbHashError> {
        let mut terms = Vec::with_capacity(nx * ny / 2);
        for y in 0..ny {
            for x in usize::from(y == 0)..nx {
                if x * ny >= nx * (ny - y) {
                    break;
                }
                let byte = hash.get(start + *index / 2).ok_or(ThumbHashError)?;
                let nibble = (byte >> ((*index % 2) * 4)) & 15;
                *index += 1;
                terms.push((x, y, (f64::from(nibble) / 7.5 - 1.0) * scale));
            }
        }
        Ok(Self { dc, terms })
    }
    fn value(&self, x: &[f64; 7], y: &[f64; 7]) -> f64 {
        self.terms
            .iter()
            .fold(self.dc, |value, &(cx, cy, coefficient)| {
                value + coefficient * x[cx] * y[cy] * 2.0
            })
    }
}

/// Validates dimensions and coefficient length without allocating or decoding pixels.
///
/// # Errors
/// Rejects zero aspect dimensions, truncated coefficients and hashes over 64 bytes.
pub fn validate(hash: &[u8]) -> Result<(), ThumbHashError> {
    if !(5..=64).contains(&hash.len()) {
        return Err(ThumbHashError);
    }
    let alpha = hash[2] & 128 != 0;
    let landscape = hash[4] & 128 != 0;
    let short = usize::from(hash[3] & 7);
    if short == 0 {
        return Err(ThumbHashError);
    }
    let long = if alpha { 5 } else { 7 };
    let (nx, ny) = if landscape {
        (long, short)
    } else {
        (short, long)
    };
    let coefficients =
        |nx: usize, ny: usize| (0..ny).map(|y| (nx * (ny - y)).div_ceil(ny)).sum::<usize>() - 1;
    let count = coefficients(nx.max(3), ny.max(3))
        + 2 * coefficients(3, 3)
        + if alpha { coefficients(5, 5) } else { 0 };
    let required = if alpha { 6 } else { 5 } + count.div_ceil(2);
    if hash.len() < required {
        return Err(ThumbHashError);
    }
    Ok(())
}

/// Decodes a valid complete hash; rejects malformed input before pixel work.
///
/// # Errors
/// Rejects zero aspect dimensions, truncated coefficients and hashes over 64 bytes.
pub fn decode(hash: &[u8]) -> Result<Placeholder, ThumbHashError> {
    validate(hash)?;
    let header = u32::from_le_bytes([hash[0], hash[1], hash[2], 0]);
    let header16 = u16::from_le_bytes([hash[3], hash[4]]);
    let alpha = header & (1 << 23) != 0;
    let landscape = header16 & (1 << 15) != 0;
    let short = usize::from(header16 & 7);
    if short == 0 {
        return Err(ThumbHashError);
    }
    let long = if alpha { 5 } else { 7 };
    let (lx, ly) = if landscape {
        (long, short)
    } else {
        (short, long)
    };
    let ratio = lx as f64 / ly as f64;
    let width = (if ratio > 1.0 { 32.0 } else { 32.0 * ratio }).round() as u32;
    let height = (if ratio > 1.0 { 32.0 / ratio } else { 32.0 }).round() as u32;
    let mut index = 0;
    let start = if alpha { 6 } else { 5 };
    let l = Channel::read(
        hash,
        start,
        &mut index,
        lx.max(3),
        ly.max(3),
        f64::from(header & 63) / 63.0,
        f64::from((header >> 18) & 31) / 31.0,
    )?;
    let p = Channel::read(
        hash,
        start,
        &mut index,
        3,
        3,
        f64::from((header >> 6) & 63) / 31.5 - 1.0,
        f64::from((header16 >> 3) & 63) / 63.0 * 1.25,
    )?;
    let q = Channel::read(
        hash,
        start,
        &mut index,
        3,
        3,
        f64::from((header >> 12) & 63) / 31.5 - 1.0,
        f64::from((header16 >> 9) & 63) / 63.0 * 1.25,
    )?;
    let a = if alpha {
        let byte = *hash.get(5).ok_or(ThumbHashError)?;
        Some(Channel::read(
            hash,
            start,
            &mut index,
            5,
            5,
            f64::from(byte & 15) / 15.0,
            f64::from(byte >> 4) / 15.0,
        )?)
    } else {
        None
    };
    let cosines = |size: u32| -> Vec<[f64; 7]> {
        (0..size)
            .map(|v| {
                std::array::from_fn(|c| {
                    (PI / f64::from(size) * (f64::from(v) + 0.5) * c as f64).cos()
                })
            })
            .collect()
    };
    let xs = cosines(width);
    let ys = cosines(height);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in &ys {
        for x in &xs {
            let luminance = l.value(x, y);
            let blue = luminance - 2.0 / 3.0 * p.value(x, y);
            let chroma = q.value(x, y);
            let red = f64::midpoint(3.0 * luminance - blue, chroma);
            for channel in [
                red,
                red - chroma,
                blue,
                a.as_ref().map_or(1.0, |a| a.value(x, y)),
            ] {
                rgba.push((255.0 * channel.clamp(0.0, 1.0)) as u8);
            }
        }
    }
    Ok(Placeholder {
        width,
        height,
        rgba,
    })
}

impl Placeholder {
    /// Width guaranteed to be in 1..=32.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }
    /// Height guaranteed to be in 1..=32.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Unpremultiplied row-major RGBA8, exactly width * height * 4 bytes.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Encodes the bounded pixels as a PNG with one uncompressed DEFLATE block.
    /// Returns no external resources and uses no general-purpose image dependency.
    #[must_use]
    pub fn png(&self) -> Vec<u8> {
        fn chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
            output.extend_from_slice(&(data.len() as u32).to_be_bytes());
            output.extend_from_slice(&kind);
            output.extend_from_slice(data);
            let mut crc = !0_u32;
            for &byte in kind.iter().chain(data) {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
                }
            }
            output.extend_from_slice(&(!crc).to_be_bytes());
        }
        let mut filtered = Vec::with_capacity(self.rgba.len() + self.height as usize);
        for row in self.rgba.chunks(self.width as usize * 4) {
            filtered.push(0);
            filtered.extend_from_slice(row);
        }
        let len = filtered.len() as u16;
        let mut zlib = vec![0x78, 0x01, 0x01];
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        let (mut a, mut b) = (1_u32, 0_u32);
        for &byte in &filtered {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend_from_slice(&filtered);
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::from(self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, *b"IHDR", &ihdr);
        chunk(&mut png, *b"IDAT", &zlib);
        chunk(&mut png, *b"IEND", &[]);
        png
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn rejects_bad_headers_and_truncated_coefficients() {
        for len in 0..5 {
            assert!(decode(&vec![0; len]).is_err());
        }
        assert!(decode(&[0; 64]).is_err());
        assert!(decode(&[1; 65]).is_err());
        assert!(decode(&[0, 0, 0, 7, 0]).is_err());
    }
    #[test]
    fn reference_vectors_match_every_decoded_pixel() {
        // Generated from synthetic black, white, alpha and gradient pixels with
        // the upstream encoder/decoder. FNV-1a checks every RGBA byte.
        let vectors: &[(&[u8], u32, u32, u32)] = &[
            (
                &[
                    0, 8, 2, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ],
                32,
                32,
                1_986_340_293,
            ),
            (
                &[
                    63, 8, 2, 7, 0, 104, 137, 78, 137, 120, 136, 133, 136, 167, 136, 136, 135, 127,
                    4, 0, 0, 0, 0, 0,
                ],
                32,
                32,
                2_705_403_333,
            ),
            (
                &[
                    0, 8, 130, 5, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 190, 112, 135, 128,
                    167, 121,
                ],
                32,
                32,
                928_054_725,
            ),
            (
                &[
                    226, 39, 142, 19, 150, 43, 144, 135, 129, 136, 120, 110, 144, 24, 249, 118,
                    128, 135, 136, 136, 120, 136, 119,
                ],
                32,
                19,
                3_157_676_160,
            ),
            (
                &[
                    33, 40, 142, 19, 20, 43, 145, 144, 119, 120, 136, 144, 111, 8, 249, 134, 128,
                    135, 136, 136, 135, 136, 120,
                ],
                19,
                32,
                2_574_598_679,
            ),
        ];
        for &(hash, width, height, checksum) in vectors {
            assert!(validate(hash).is_ok());
            let image = decode(hash).unwrap();
            assert_eq!((image.width(), image.height()), (width, height));
            let actual = image.rgba().iter().fold(2_166_136_261_u32, |sum, byte| {
                (sum ^ u32::from(*byte)).wrapping_mul(16_777_619)
            });
            assert_eq!(actual, checksum);
            assert!(image.png().starts_with(b"\x89PNG\r\n\x1a\n"));
            for len in 0..hash.len() {
                assert!(decode(&hash[..len]).is_err());
                assert!(validate(&hash[..len]).is_err());
            }
        }
    }
}
