//! Bounded, allocation-free parser shared with the fuzz target.
/// A non-wrapping rectangle in longitude/latitude, limited to Web Mercator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    /// Western longitude in degrees.
    pub west: f64,
    /// Southern latitude in degrees.
    pub south: f64,
    /// Eastern longitude in degrees.
    pub east: f64,
    /// Northern latitude in degrees.
    pub north: f64,
}
impl Bounds {
    /// Rejects non-finite coordinates, reversed, oversized or degenerate boxes.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() > 128 {
            return None;
        }
        let mut parts = value.split(',');
        let bounds = Self {
            west: parts.next()?.parse().ok()?,
            south: parts.next()?.parse().ok()?,
            east: parts.next()?.parse().ok()?,
            north: parts.next()?.parse().ok()?,
        };
        if parts.next().is_some()
            || ![bounds.west, bounds.south, bounds.east, bounds.north]
                .iter()
                .all(|v| v.is_finite())
            || !(-180.0..=180.0).contains(&bounds.west)
            || !(-180.0..=180.0).contains(&bounds.east)
            || !(-85.051_129..=85.051_129).contains(&bounds.south)
            || !(-85.051_129..=85.051_129).contains(&bounds.north)
            || bounds.west >= bounds.east
            || bounds.south >= bounds.north
        {
            return None;
        }
        Some(bounds)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boxes_are_finite_bounded_and_ordered() {
        assert!(Bounds::parse("-180,-85,180,85").is_some());
        for value in [
            "",
            "0,0,1",
            "0,0,1,1,2",
            "NaN,0,1,1",
            "0,0,inf,1",
            "-181,0,1,1",
            "0,-86,1,1",
            "0,0,0,1",
            "2,0,1,1",
            "0,2,1,1",
        ] {
            assert!(Bounds::parse(value).is_none(), "{value}");
        }
        assert!(Bounds::parse(&"0".repeat(129)).is_none());
    }
}
