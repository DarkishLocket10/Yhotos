// Bounded native buffers and reference numeric kernels use checked dimensions.
#![allow(clippy::cast_possible_truncation, clippy::indexing_slicing)]
use crate::{CodecError, Working};
/// Deterministic median cut over at most 64x64 sRGB pixels, ignoring transparent pixels.
///
/// # Errors
/// Returns a typed error when options are invalid or native processing fails.
pub fn dominant_colors(w: &Working, n: u8) -> Result<Vec<[u8; 3]>, CodecError> {
    if n == 0 || n > 32 {
        return Err(CodecError::Options);
    }
    let (_, _, rgba) = crate::rgba(w, 64)?;
    let pixels: Vec<[u8; 3]> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 0)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    if pixels.is_empty() {
        return Ok(Vec::new());
    }
    let mut boxes = vec![pixels];
    while boxes.len() < usize::from(n) {
        let Some((idx, axis)) = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.len() > 1)
            .map(|(i, b)| {
                let (axis, range) = (0..3)
                    .map(|a| {
                        (
                            a,
                            b.iter().map(|p| p[a]).max().unwrap_or(0)
                                - b.iter().map(|p| p[a]).min().unwrap_or(0),
                        )
                    })
                    .max_by_key(|(_, r)| *r)
                    .unwrap_or((0, 0));
                (i, axis, usize::from(range) * b.len())
            })
            .max_by_key(|(_, _, score)| *score)
            .map(|(i, a, _)| (i, a))
        else {
            break;
        };
        let mut b = boxes.swap_remove(idx);
        b.sort_unstable_by_key(|p| p[axis]);
        let other = b.split_off(b.len() / 2);
        boxes.push(b);
        boxes.push(other);
    }
    boxes.sort_by_key(|b| std::cmp::Reverse(b.len()));
    Ok(boxes
        .into_iter()
        .map(|b| {
            let mut sum = [0u32; 3];
            for p in &b {
                for (a, v) in sum.iter_mut().zip(p) {
                    *a += u32::from(*v);
                }
            }
            sum.map(|s| (s / b.len() as u32) as u8)
        })
        .collect())
}
