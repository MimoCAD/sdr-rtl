//! The sample clock against the host's: loss neither the chip's counter
//! nor the host's ledger sees (findings 27).
//!
//! Findings 27: two dongles streaming at once at 2.56 and 2.88 Msps lost
//! single 94-sample blocks, hundreds a minute in bursts minutes apart,
//! and neither the chip's dropped-block counter (USB_EPA_FIFO_CFG[31:24])
//! nor the engine's ledger counted them — the ledger's floor creeps
//! toward a slow deficit, taking it for clock drift. They do show in the
//! samples' count against the time: the stream falls behind the clock and
//! never catches up.
//!
//! What this watches is the engine's raw deficit at each completion,
//! elapsed time × rate − samples taken off the wire
//! (`LinkStats::ledger_deficit_samples`). Its floor — the least of each
//! second, when a transfer had just completed — follows a straight line
//! while nothing is lost: the dongle's crystal against the host's clock,
//! a few ppm, moving slowly. Loss lifts the floor above the line for good.
//! So: a line fitted over a warm-up, then followed slowly while the floor
//! stays near it; a floor that stays above it is loss, counted and the
//! line moved up to it; a floor that stays below it (the host's clock
//! slewed, or the fit was off) moves the line down without counting.
//!
//! Not seen: loss that hides inside the band (under [`BAND_SAMPLES`] in
//! all) or a loss rate steady from the stream's start and under
//! [`MAX_DRIFT`] (100 ppm of the rate), which the warm-up
//! fits as drift. A host clock slewed forward faster than the samples
//! (NTP stepping in) would read as loss.

use alloc::vec::Vec;

use crate::math::round;

/// One floor reading per this much host time.
pub const BUCKET_US: u64 = 1_000_000;
/// Floors left out after a start or a re-base: the start's transient.
pub const SKIP_BUCKETS: usize = 2;
/// Floors the first line is fitted to.
pub const WARMUP_BUCKETS: usize = 10;
/// A floor within this many samples of the line is the line (the floor's
/// own jitter); 3 blocks of 94.
pub const BAND_SAMPLES: f64 = 300.0;
/// A floor this far above the line, for [`CONFIRM_BUCKETS`] floors, is
/// loss at once.
pub const STEP_SAMPLES: f64 = 1_000.0;
pub const CONFIRM_BUCKETS: usize = 3;
/// A floor above the band but under [`STEP_SAMPLES`] is loss once it has
/// stayed there this many floors; one below the band moves the line down
/// after as many.
pub const SETTLE_BUCKETS: usize = 10;
/// How fast the line follows the floor while nothing is lost: the
/// intercept by this share of each floor's residual, the slope by
/// [`SLOPE_GAIN`] of it per bucket.
pub const INTERCEPT_GAIN: f64 = 0.1;
pub const SLOPE_GAIN: f64 = 0.002;
/// The steepest rise a crystal against the host's clock can give the
/// deficit, as a share of the rate: 100 ppm, beyond a cheap dongle's
/// crystal (±25-50 ppm) uncorrected. A warm-up line steeper than this is
/// loss from the start (findings 27: a dongle at 2.88 Msps beside one at
/// 2.0 lost ~400 ppm of its samples from its first milliseconds): the
/// excess is counted and the line held at this slope, so loss that goes
/// on is counted as it goes. The count is then a lower bound — how much
/// of the slope is the crystal cannot be told — but the loss is flagged.
pub const MAX_DRIFT: f64 = 100e-6;

/// The line: the deficit at `t0_us`, and its rise per microsecond.
#[derive(Clone, Copy, Debug)]
struct Line {
    t0_us: f64,
    d0: f64,
    slope: f64,
}

impl Line {
    fn at(&self, t_us: f64) -> f64 {
        self.d0 + self.slope * (t_us - self.t0_us)
    }
}

/// The watch over one run of the stream (re-based at every break).
#[derive(Clone, Debug, Default)]
pub struct ClockWatch {
    /// The open bucket: its start, its least deficit and when.
    bucket_start_us: Option<u64>,
    bucket_min: Option<(u64, f64)>,
    /// Floors seen since the last re-base.
    floors: usize,
    warmup: Vec<(f64, f64)>,
    line: Option<Line>,
    over: usize,
    under: usize,
    /// Samples judged lost since the stream was made.
    lost: u64,
    /// Times loss was judged.
    events: u64,
    /// The stream's rate, samples a second (0: no [`MAX_DRIFT`] bound).
    rate: f64,
}

impl ClockWatch {
    /// A watch over a stream at `rate` samples a second.
    pub fn new(rate: f64) -> ClockWatch {
        ClockWatch {
            rate,
            ..ClockWatch::default()
        }
    }

    /// The steepest drift the line may take, samples per µs.
    fn max_slope(&self) -> f64 {
        if self.rate > 0.0 {
            MAX_DRIFT * self.rate / 1e6
        } else {
            f64::INFINITY
        }
    }

    /// Samples judged lost since the stream was made.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Times loss was judged.
    pub fn events(&self) -> u64 {
        self.events
    }

    /// Forget the line and the open bucket (a start, a break: time and
    /// samples part there by design), keeping the count.
    pub fn rebase(&mut self) {
        *self = ClockWatch {
            lost: self.lost,
            events: self.events,
            rate: self.rate,
            ..ClockWatch::default()
        };
    }

    /// One completion: its time (µs since the engine's start) and the
    /// deficit then. Returns the samples judged lost now (0 mostly).
    pub fn feed(&mut self, t_us: u64, deficit: i64) -> u64 {
        let d = deficit as f64;
        let start = *self.bucket_start_us.get_or_insert(t_us);
        if t_us.saturating_sub(start) < BUCKET_US {
            if self.bucket_min.is_none_or(|(_, m)| d < m) {
                self.bucket_min = Some((t_us, d));
            }
            return 0;
        }
        // The bucket closed: its floor, then this completion opens the next.
        let floor = self.bucket_min.take();
        self.bucket_start_us = Some(t_us);
        self.bucket_min = Some((t_us, d));
        match floor {
            Some((ft, fd)) => self.floor(ft as f64, fd),
            None => 0,
        }
    }

    fn floor(&mut self, t: f64, d: f64) -> u64 {
        self.floors += 1;
        if self.floors <= SKIP_BUCKETS {
            return 0;
        }
        let max = self.max_slope();
        let Some(line) = self.line.as_mut() else {
            self.warmup.push((t, d));
            if self.warmup.len() < WARMUP_BUCKETS {
                return 0;
            }
            let mut l = fit(&self.warmup);
            let (first, last) = (self.warmup[0].0, t);
            self.warmup.clear();
            // Steeper than any crystal: loss from the start. The excess
            // over the warm-up is counted; the line, held at the bound,
            // stands where the floor is now.
            let mut lost = 0;
            if l.slope > max {
                let n = round((l.slope - max) * (last - first)) as u64;
                l = Line {
                    t0_us: last,
                    d0: l.at(last),
                    slope: max,
                };
                self.lost += n;
                self.events += 1;
                lost = n;
            } else if l.slope < -max {
                l = Line {
                    t0_us: last,
                    d0: l.at(last),
                    slope: -max,
                };
            }
            self.line = Some(l);
            return lost;
        };
        let r = d - line.at(t);
        if r.abs() <= BAND_SAMPLES {
            self.over = 0;
            self.under = 0;
            // Follow the drift: move the line toward the floor, never
            // steeper than a crystal can make it.
            let bucket = BUCKET_US as f64;
            line.d0 = line.at(t) + INTERCEPT_GAIN * r;
            line.slope = (line.slope + SLOPE_GAIN * r / bucket).clamp(-max, max);
            line.t0_us = t;
            return 0;
        }
        if r > 0.0 {
            self.under = 0;
            self.over += 1;
            if (r > STEP_SAMPLES && self.over >= CONFIRM_BUCKETS) || self.over >= SETTLE_BUCKETS {
                // Loss: never paid back. Count it and stand the line on it.
                line.d0 = line.at(t) + r;
                line.t0_us = t;
                self.over = 0;
                let n = round(r) as u64;
                self.lost += n;
                self.events += 1;
                return n;
            }
        } else {
            self.over = 0;
            self.under += 1;
            if self.under >= SETTLE_BUCKETS {
                line.d0 = line.at(t) + r;
                line.t0_us = t;
                self.under = 0;
            }
        }
        0
    }
}

/// The least-squares line through the points.
fn fit(points: &[(f64, f64)]) -> Line {
    let n = points.len() as f64;
    let mt = points.iter().map(|p| p.0).sum::<f64>() / n;
    let md = points.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for &(t, d) in points {
        num += (t - mt) * (d - md);
        den += (t - mt) * (t - mt);
    }
    let slope = if den > 0.0 { num / den } else { 0.0 };
    Line {
        t0_us: mt,
        d0: md,
        slope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic jitter: completions late by 0-60 µs at 2.88 Msps
    /// (0-173 samples), the floor near the low end.
    fn jitter(k: u64) -> f64 {
        ((k.wrapping_mul(2_654_435_761) >> 7) % 173) as f64
    }

    /// `secs` of completions every 2,844 µs (a 16 KiB transfer at 2.88
    /// Msps), the deficit drifting by `ppm` of the rate, plus `loss(t_s)`.
    fn run(w: &mut ClockWatch, secs: f64, ppm: f64, loss: impl Fn(f64) -> f64) -> u64 {
        let rate = 2.88e6;
        let step_us = 2_844u64;
        let mut lost = 0;
        let mut t = 0u64;
        let mut k = 0u64;
        while (t as f64) < secs * 1e6 {
            let ts = t as f64 / 1e6;
            let d = 100.0 + ppm * 1e-6 * rate * ts + loss(ts) + jitter(k);
            lost += w.feed(t, d as i64);
            t += step_us;
            k += 1;
        }
        lost
    }

    #[test]
    fn steady_drift_is_not_loss() {
        for ppm in [-50.0, -3.8, 0.0, 1.0, 25.0] {
            let mut w = ClockWatch::new(2.88e6);
            assert_eq!(run(&mut w, 600.0, ppm, |_| 0.0), 0, "{ppm} ppm");
        }
    }

    /// Findings 27's burst: ~600 samples a second for 3 minutes after 5
    /// minutes clean — about 108,000 samples, counted within a band.
    #[test]
    fn a_burst_of_lost_blocks_is_counted() {
        let mut w = ClockWatch::new(2.88e6);
        let loss = |t: f64| 600.0 * (t - 300.0).clamp(0.0, 180.0);
        let lost = run(&mut w, 600.0, -3.8, loss);
        assert!(
            (lost as f64 - 108_000.0).abs() < 2.0 * BAND_SAMPLES + STEP_SAMPLES,
            "{lost}"
        );
        assert!(w.events() > 10);
    }

    /// Findings 27's mixed-rate case: ~400 ppm of the samples lost steadily
    /// from the first milliseconds. Past the drift bound, counted — the
    /// warm-up's excess at once, the rest as it goes — to within a few
    /// percent; 90 ppm of drift alone is not.
    #[test]
    fn steady_loss_from_the_start_is_counted() {
        let mut w = ClockWatch::new(2.88e6);
        let lost = run(&mut w, 300.0, 400.0, |_| 0.0);
        let truth = (400.0 - MAX_DRIFT * 1e6) * 1e-6 * 2.88e6 * 300.0;
        assert!(
            (lost as f64 - truth).abs() < 0.05 * truth,
            "{lost} against {truth}"
        );
        let mut quiet = ClockWatch::new(2.88e6);
        assert_eq!(run(&mut quiet, 300.0, 90.0, |_| 0.0), 0);
    }

    /// One lump of 2,000 samples, and one of 500 (under the step, over
    /// the band): both counted, the second once it has stayed.
    #[test]
    fn single_steps_are_counted() {
        let mut w = ClockWatch::new(2.88e6);
        let loss =
            |t: f64| if t > 100.0 { 2_000.0 } else { 0.0 } + if t > 200.0 { 500.0 } else { 0.0 };
        let lost = run(&mut w, 300.0, 1.0, loss);
        assert!((lost as f64 - 2_500.0).abs() < BAND_SAMPLES, "{lost}");
    }

    /// A rebase (a break) forgets the line, keeps the count, and the
    /// jump in time it covers is not loss.
    #[test]
    fn a_rebase_forgets_the_line() {
        let mut w = ClockWatch::new(2.88e6);
        let _ = run(&mut w, 60.0, 0.0, |t| if t > 30.0 { 3_000.0 } else { 0.0 });
        let before = w.lost();
        w.rebase();
        assert_eq!(w.lost(), before);
        // After the rebase the deficit sits 50,000 higher (a pause's time):
        // a new line, nothing counted.
        let mut t = 0u64;
        let mut fresh = 0;
        while t < 60_000_000 {
            fresh += w.feed(t, 50_000 + jitter(t) as i64);
            t += 2_844;
        }
        assert_eq!(fresh, 0);
    }
}
