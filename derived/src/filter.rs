//! The IF filter's bandwidth: FILT_CODE (R10[3:0]) and FILT_BW
//! (R11[6:5]) from the upper −3 dB corners the bench measured
//! (`tables::if_filter`, findings step 4g: the LO fixed at 304.57 MHz,
//! the tone stepped through the IF band, the DDC tracking it).
//!
//! The signal sits at the 4.57 MHz IF (`tables::band::IF_HZ`), so a
//! bandwidth B needs the corner at or above IF + B/2; the setting chosen
//! is the narrowest measured one that still meets that. Nineteen
//! settings were measured: FILT_CODE 0-15 at FILT_BW 00 (10.82 down to
//! 7.75 MHz) and FILT_BW 01, 10, 11 at FILT_CODE 0 (8.73, 8.70, 7.28 MHz);
//! combinations of a narrow code with a narrow coarse setting were not,
//! and are not used. The HPF codes have no measurable effect above
//! 0.1 MHz and stay at 1111.
//!
//! Not the tuner's to set: the passband also falls below about 0.5 MHz
//! for every code (the roll-off is common to all and is not the tuner's
//! HPF), which bounds the usable bandwidth at about 2 · (4.57 − 0.5) ≈
//! 8 MHz whatever the upper corner; and the corners carry the
//! demodulator's own response at the tone.

use crate::tables::band::IF_HZ;
use crate::tables::if_filter::{FILT_BW_UPPER_3DB, FILT_CODE_UPPER_3DB};

/// One measured IF filter setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterSetting {
    pub filt_code: u8,
    pub filt_bw: u8,
    pub upper_3db_hz: f64,
}

/// Every measured setting, in the records' order.
pub fn settings() -> Vec<FilterSetting> {
    let mut v: Vec<FilterSetting> =
        FILT_CODE_UPPER_3DB.iter().map(|c| FilterSetting { filt_code: c.code, filt_bw: 0, upper_3db_hz: c.upper_3db_hz }).collect();
    v.extend(FILT_BW_UPPER_3DB.iter().filter(|c| c.code != 0).map(|c| FilterSetting { filt_code: 0, filt_bw: c.code, upper_3db_hz: c.upper_3db_hz }));
    v
}

/// The widest measured setting (FILT_CODE 0000, FILT_BW 00).
pub fn widest() -> FilterSetting {
    settings().into_iter().max_by(|a, b| a.upper_3db_hz.total_cmp(&b.upper_3db_hz)).expect("the table is not empty")
}

/// The bandwidth an upper corner admits about the IF: 2 · (corner − IF).
pub fn bandwidth_of(upper_3db_hz: f64) -> f64 {
    2.0 * (upper_3db_hz - IF_HZ)
}

/// The narrowest measured setting whose corner is at or above
/// IF + hz/2; the widest when none is (the request is clamped, and the
/// corner reached is readable through the status).
pub fn setting_for_bandwidth(hz: f64) -> FilterSetting {
    let need = IF_HZ + hz / 2.0;
    settings()
        .into_iter()
        .filter(|s| s.upper_3db_hz >= need)
        .min_by(|a, b| a.upper_3db_hz.total_cmp(&b.upper_3db_hz))
        .unwrap_or_else(widest)
}

/// The corner a (FILT_CODE, FILT_BW) pair was measured at, if it was.
pub fn corner_of(filt_code: u8, filt_bw: u8) -> Option<f64> {
    settings().into_iter().find(|s| s.filt_code == filt_code && s.filt_bw == filt_bw).map(|s| s.upper_3db_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nineteen_settings_widest_first() {
        let s = settings();
        assert_eq!(s.len(), 19);
        assert_eq!(widest(), FilterSetting { filt_code: 0, filt_bw: 0, upper_3db_hz: 10.82e6 });
        assert_eq!(corner_of(15, 0), Some(7.75e6));
        assert_eq!(corner_of(0, 3), Some(7.28e6));
        assert_eq!(corner_of(15, 3), None, "unmeasured combinations are not offered");
    }

    #[test]
    fn bandwidth_to_code() {
        // 12.5 MHz needs the widest corner exactly (10.82 = 4.57 + 6.25).
        assert_eq!(setting_for_bandwidth(12.5e6), FilterSetting { filt_code: 0, filt_bw: 0, upper_3db_hz: 10.82e6 });
        // Wider than any corner: clamped to the widest.
        assert_eq!(setting_for_bandwidth(20e6), widest());
        // 6 MHz needs 7.57 MHz: FILT_CODE 15 (7.75), not FILT_BW 11 (7.28).
        assert_eq!(setting_for_bandwidth(6e6), FilterSetting { filt_code: 15, filt_bw: 0, upper_3db_hz: 7.75e6 });
        // 5 MHz needs 7.07 MHz: the coarse narrowest, 7.28.
        assert_eq!(setting_for_bandwidth(5e6), FilterSetting { filt_code: 0, filt_bw: 3, upper_3db_hz: 7.28e6 });
        // 8 MHz needs 8.57 MHz: code 10 (8.68) is the nearest above.
        assert_eq!(setting_for_bandwidth(8e6), FilterSetting { filt_code: 10, filt_bw: 0, upper_3db_hz: 8.68e6 });
        // Monotonic: a wider request never gets a lower corner.
        let mut last = 0.0;
        let mut hz = 1e6;
        while hz <= 14e6 {
            let c = setting_for_bandwidth(hz).upper_3db_hz;
            assert!(c >= last, "{hz}");
            last = c;
            hz += 0.1e6;
        }
        assert!((bandwidth_of(10.82e6) - 12.5e6).abs() < 1.0);
    }
}
