//! The meter's scale: counts of `if_agc_val` (MSB-first) per dB of
//! signal, fitted from the meter step's level ladder, and the rail it
//! reads with nothing passing. Every dB in the generated tables that is
//! not a comparator flip level is a count difference over this slope.

use crate::records::{Meter, Provenance};

/// `if_agc_val` is s(14,13f) ([RTL2832U] Table 2): a 14-bit two's
/// complement integer, so its positive rail is 2^13 - 1.
pub const IF_AGC_VAL_BITS: u32 = 14;
pub const RAIL_COUNTS: i64 = (1 << (IF_AGC_VAL_BITS - 1)) - 1;

/// The configuration whose ladder is the meter (findings.md step 4b):
/// the VGA taking its gain from the VAGC pin, the demodulator's AGC pin
/// polarity 0.
pub const METER_CONFIG: &str = "vagc";
pub const METER_POLAR: bool = false;

#[derive(Clone, Debug)]
pub struct MeterScale {
    pub provenance: Provenance,
    /// Counts per dB, positive: the meter FALLS by this much per dB of
    /// signal (the fitted slope is negative).
    pub counts_per_db: f64,
    /// The source level at which the fit crosses zero counts.
    pub zero_at_db: f64,
    /// The fit's largest residual, in counts.
    pub max_residual_counts: f64,
    pub points: usize,
}

impl MeterScale {
    /// How many dB more signal reading `b` shows than reading `a`.
    pub fn db_more(&self, a: i64, b: i64) -> f64 {
        (a - b) as f64 / self.counts_per_db
    }
}

/// A least-squares line through (level, counts) over the ladder's
/// tone-on states, sorted by level (METHODS.md §8: the ladder is taken
/// coarse-outer and must be analysed sorted).
pub fn fit(meter: &Meter) -> Result<MeterScale, String> {
    let ladder = meter
        .ladders
        .iter()
        .find(|l| l.config == METER_CONFIG && l.polar == METER_POLAR)
        .ok_or("meter record: no vagc / polar 0 ladder")?;
    // The positive control's null: with the tone off the meter sits on
    // its rail.
    if ladder.off_reading < RAIL_COUNTS {
        return Err(format!(
            "meter record: the tone-off reading ({}) is not on the rail {RAIL_COUNTS}",
            ladder.off_reading
        ));
    }
    let mut pts: Vec<(f64, f64)> = ladder
        .states
        .iter()
        .map(|&(level, m)| (level as f64, m as f64))
        .collect();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let n = pts.len() as f64;
    if n < 2.0 {
        return Err("meter record: fewer than two ladder states".into());
    }
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    if slope >= 0.0 {
        return Err(format!(
            "meter record: the fitted slope is {slope}, not falling with signal"
        ));
    }
    let max_residual = pts
        .iter()
        .map(|p| (p.1 - (intercept + slope * p.0)).abs())
        .fold(0.0, f64::max);
    Ok(MeterScale {
        provenance: meter.provenance.clone(),
        counts_per_db: -slope,
        zero_at_db: -intercept / slope,
        max_residual_counts: max_residual,
        points: pts.len(),
    })
}
