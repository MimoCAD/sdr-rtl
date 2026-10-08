//! The front-end path per frequency: RFFILT and RFMUX (R26) from the
//! band table's path segments (`tables::band::BAND_PATH_SEGMENTS`,
//! findings step 4f: 87 grid frequencies, 42-1002 MHz in 10 MHz steps,
//! the (RFFILT, RFMUX) that passed a cabled tone best by stage B), with
//! the tracking-filter corner codes (R27) at 0000/0000.
//!
//! Why the path segments and not `BAND_SEGMENTS`: the corner codes of a
//! single-tone, single-snapshot reading move the best path's reading by
//! a median 0.6 dB (at most 3.8 dB) and are set by the loop's two-state
//! alternation more than by the filter (the generated table's own
//! caveat), so `BAND_PATH_SEGMENTS` is the robust part of the record;
//! 0000/0000 is the highest (least attenuating) corner of both the notch
//! and the low-pass ([R820T2] Table 1-3) and the setting every path was
//! ranked at. Their rejection of out-of-band signals is unmeasured.
//!
//! Between two grid points the boundary is unknown (the grid is 10 MHz),
//! so a frequency takes the nearest measured segment; across the
//! unmeasured 112-202 MHz span (no divider reaches it, findings step 4d)
//! the PLL refuses first.

use crate::tables::band::BAND_PATH_SEGMENTS;
use crate::tables::types::FrontPath;

/// The distance from `f` to the closed span [from, to], 0 inside it.
fn distance(f: f64, from: f64, to: f64) -> f64 {
    if f < from {
        from - f
    } else if f > to {
        f - to
    } else {
        0.0
    }
}

/// The measured path nearest an RF, or None if the table is empty.
/// Equidistant segments (a frequency midway between two grid points)
/// go to the lower one.
pub fn path_for(rf_hz: f64) -> Option<FrontPath> {
    BAND_PATH_SEGMENTS
        .iter()
        .filter_map(|s| s.path.map(|p| (distance(rf_hz, s.from_hz, s.to_hz), p)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(rffilt: u8, rfmux: u8) -> FrontPath {
        FrontPath { rffilt, rfmux, tf_nch: 0, tf_lp: 0 }
    }

    /// Segment edges from the table: 42-62 (10,01), 72-82 (00,00),
    /// 92-102 (10,01), 212-242 (10,01), 252 (00,01), 1002 (10,01).
    #[test]
    fn segment_edges() {
        assert_eq!(path_for(42e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(62e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(72e6), Some(path(0b00, 0b00)));
        assert_eq!(path_for(82e6), Some(path(0b00, 0b00)));
        assert_eq!(path_for(92e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(212e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(242e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(252e6), Some(path(0b00, 0b01)));
        assert_eq!(path_for(262e6), Some(path(0b01, 0b01)));
        assert_eq!(path_for(1002e6), Some(path(0b10, 0b01)));
    }

    /// Midway between grid points the lower segment wins; just past it
    /// the upper one does.
    #[test]
    fn between_grid_points_the_nearest_segment_wins() {
        assert_eq!(path_for(66.9e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(67.0e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(67.1e6), Some(path(0b00, 0b00)));
        assert_eq!(path_for(247.0e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(247.1e6), Some(path(0b00, 0b01)));
        // Below and above the grid: the end segments.
        assert_eq!(path_for(40e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(1010e6), Some(path(0b10, 0b01)));
        // Inside the unmeasured span: the nearer measured neighbour.
        assert_eq!(path_for(105e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(208e6), Some(path(0b10, 0b01)));
    }

    /// The corner codes of every path segment are 0000/0000.
    #[test]
    fn path_segments_carry_no_corner_codes() {
        for s in BAND_PATH_SEGMENTS {
            if let Some(p) = s.path {
                assert_eq!((p.tf_nch, p.tf_lp), (0, 0));
                assert!(p.rffilt <= 0b10, "RFFILT 11 is dead (step 4f)");
            }
        }
    }
}
