//! Linear, deterministic justified rows. There is no browser reflow pass.

/// A contiguous row's photo range and total aspect ratio.
#[derive(Clone, Debug)]
pub struct GridRow {
    /// Inclusive photo index.
    pub start: usize,
    /// Exclusive photo index.
    pub end: usize,
    /// Sum used to calculate proportional flex bases.
    pub ratio: f64,
}

/// Partitions bounded, positive aspect ratios into rows near a 280px target height.
/// The server assumes a 1200px container; CSS scales the completed rows proportionally.
#[must_use]
pub fn justified_rows(ratios: &[f64]) -> Vec<GridRow> {
    let mut rows = Vec::with_capacity(ratios.len() / 3 + 1);
    let (mut start, mut sum) = (0, 0.0);
    for (index, ratio) in ratios.iter().enumerate() {
        sum += ratio;
        if sum * 280.0 >= 1200.0 {
            rows.push(GridRow {
                start,
                end: index + 1,
                ratio: sum,
            });
            start = index + 1;
            sum = 0.0;
        }
    }
    if start < ratios.len() {
        rows.push(GridRow {
            start,
            end: ratios.len(),
            ratio: sum,
        });
    }
    rows
}
