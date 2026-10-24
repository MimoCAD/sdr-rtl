//! The front-end path per frequency: RFFILT and RFMUX (R26) from the
//! every-path band table's segments (`tables::band_paths::
//! BAND_PATH_SEGMENTS`, findings step 4f2: 97 grid frequencies, 42-1002
//! MHz in 10 MHz steps, every (RFFILT, RFMUX) compared in three runs, each
//! reading settled 300 ms and averaged over 700 ms), with the
//! tracking-filter corner codes (R27) at 0000/0000.
//!
//! Corner codes 0000/0000 are the highest (least attenuating) corner of
//! both the notch and the low-pass ([R820T2] Table 1-3) and the setting
//! every path was compared at; the corner codes themselves move a reading
//! by about a decibel and their rejection of out-of-band signals is
//! unmeasured. Step 4f2 supersedes step 4f's staged table
//! (`derivation/generated/band.rs`), whose 20 ms single snapshots read inside a path
//! change's transient.
//!
//! Between two grid points the boundary is unknown (the grid is 10 MHz),
//! so a frequency takes the nearest segment. Above the grid, to the top
//! of `RF_RANGE_HZ`, that is the last segment's (10, 01): findings 26
//! compared all 16 paths every 10 MHz from 1010 to 1720 MHz on both bench
//! dongles and (10, 01) was the strongest at every step, by 1.1-1.8 dB.

use crate::tables::band_paths::BAND_PATH_SEGMENTS;
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
        FrontPath {
            rffilt,
            rfmux,
            tf_nch: 0,
            tf_lp: 0,
        }
    }

    /// Segment edges from the table (findings step 4f2): 42-242 (10,01),
    /// 252-272 (10,00), 282-1002 (10,01); the former 112-202 MHz gap is
    /// measured like the rest.
    #[test]
    fn segment_edges() {
        assert_eq!(path_for(42e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(152e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(242e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(252e6), Some(path(0b10, 0b00)));
        assert_eq!(path_for(272e6), Some(path(0b10, 0b00)));
        assert_eq!(path_for(282e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(1002e6), Some(path(0b10, 0b01)));
    }

    /// Midway between grid points the lower segment wins; just past it
    /// the upper one does.
    #[test]
    fn between_grid_points_the_nearest_segment_wins() {
        assert_eq!(path_for(247.0e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(247.1e6), Some(path(0b10, 0b00)));
        assert_eq!(path_for(277.0e6), Some(path(0b10, 0b00)));
        assert_eq!(path_for(277.1e6), Some(path(0b10, 0b01)));
        // Below and above the grid: the end segments.
        assert_eq!(path_for(40e6), Some(path(0b10, 0b01)));
        assert_eq!(path_for(1010e6), Some(path(0b10, 0b01)));
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
