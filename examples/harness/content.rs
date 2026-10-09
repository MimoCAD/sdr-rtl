//! The content detector for the scan from the held-open data path
//! (`scan --baseline low-if-open`). With page 0 0x07 bit 7 set the
//! endpoint delivers the FIFO memory's 3,760 bytes over and over at the
//! bus's rate (data/step3-confirm-lowif-p0-07-bc.json). A register that
//! routes the sampled signal into the FIFO would change what is delivered,
//! not only how much: the replay's period would break. Per candidate,
//! from the window's bytes (computed on the fly, never stored):
//!
//! - bytes/s;
//! - the period score: the fraction of i with data[i] == data[i + 3760]
//!   over the window (1.0 for the replay, about 1/256 for a live signal);
//! - a content signature: the byte histogram's entropy (bits), and the
//!   mean and variance of even and odd bytes taken as 8-bit I/Q, unsigned
//!   and as two's complement.
//!
//! The thresholds come from the held state's own solo windows and are
//! written into the record's header before the scan starts.

use crate::json::J;
use crate::step3;

/// The replay's period: 20 blocks of 188 bytes (the FIFO size,
/// USB_EPA_FIFO_CFG 0x14; confirm-write's periodicity test).
pub const PERIOD: usize = 20 * step3::BLOCK;
/// Fewer bytes than this and the period score is not computed.
pub const MIN_BYTES_FOR_PERIOD: usize = 4 * PERIOD;
/// The period score is "broken" this far below the solo minimum.
pub const PERIOD_MARGIN: f64 = 0.01;
/// The rate band "toward 2 x sample rate": from half to twice 2 x rate.
pub const RATE_BAND: (f64, f64) = (0.5, 2.0);
/// A rate is "changed" outside this factor of the solo range.
pub const RATE_CHANGE: f64 = 0.2;
/// The entropy is "changed" this far outside the solo range (bits).
pub const ENTROPY_MARGIN: f64 = 0.05;

#[derive(Clone, Debug)]
pub struct Content {
    pub bytes: usize,
    pub secs: f64,
    pub period_score: Option<f64>,
    pub entropy_bits: f64,
    /// [even, odd] bytes, unsigned and two's complement.
    pub mean_u: [f64; 2],
    pub var_u: [f64; 2],
    pub mean_s: [f64; 2],
    pub var_s: [f64; 2],
}

fn mean_var(v: impl Iterator<Item = f64>) -> (f64, f64) {
    let (mut n, mut s, mut ss) = (0f64, 0f64, 0f64);
    for x in v {
        n += 1.0;
        s += x;
        ss += x * x;
    }
    if n == 0.0 {
        return (0.0, 0.0);
    }
    let m = s / n;
    (m, ss / n - m * m)
}

impl Content {
    pub fn of(data: &[u8], secs: f64) -> Content {
        let period_score = (data.len() >= MIN_BYTES_FOR_PERIOD).then(|| {
            let span = data.len() - PERIOD;
            (0..span).filter(|&i| data[i] == data[i + PERIOD]).count() as f64 / span as f64
        });
        let mut hist = [0u64; 256];
        for b in data {
            hist[*b as usize] += 1;
        }
        let n = data.len().max(1) as f64;
        let entropy_bits = -hist.iter().filter(|c| **c > 0).map(|c| *c as f64 / n).map(|p| p * p.log2()).sum::<f64>();
        let mut c = Content { bytes: data.len(), secs, period_score, entropy_bits, mean_u: [0.0; 2], var_u: [0.0; 2], mean_s: [0.0; 2], var_s: [0.0; 2] };
        for ph in 0..2 {
            (c.mean_u[ph], c.var_u[ph]) = mean_var(data.iter().skip(ph).step_by(2).map(|b| *b as f64));
            (c.mean_s[ph], c.var_s[ph]) = mean_var(data.iter().skip(ph).step_by(2).map(|b| *b as i8 as f64));
        }
        c
    }

    pub fn rate(&self) -> f64 {
        if self.secs > 0.0 { self.bytes as f64 / self.secs } else { 0.0 }
    }

    /// Compact line form: [bytes/s, period score, entropy, mean_u I, Q,
    /// var_u I, Q, mean_s I, Q, var_s I, Q] (legend in the header).
    pub fn compact(&self) -> J {
        let r = |x: f64| J::from((x * 1e4).round() / 1e4);
        J::Arr(vec![
            J::from(self.rate().round()),
            self.period_score.map(r).unwrap_or(J::Null),
            r(self.entropy_bits),
            r(self.mean_u[0]),
            r(self.mean_u[1]),
            r(self.var_u[0]),
            r(self.var_u[1]),
            r(self.mean_s[0]),
            r(self.mean_s[1]),
            r(self.var_s[0]),
            r(self.var_s[1]),
        ])
    }
}

pub const LEGEND: &str = "c = [bytes/s, period score (fraction of i with data[i] == data[i+3760] over the window; null under 15040 bytes), byte-histogram entropy (bits), mean I, mean Q (unsigned), var I, var Q (unsigned), mean I, mean Q (two's complement), var I, var Q (two's complement)], I = even bytes, Q = odd bytes";

#[derive(Clone, Debug)]
pub struct Thresholds {
    pub period_min: f64,
    pub rate_lo: f64,
    pub rate_hi: f64,
    pub entropy_lo: f64,
    pub entropy_hi: f64,
    pub stream_lo: f64,
    pub stream_hi: f64,
}

impl Thresholds {
    /// From the held state's solo windows.
    pub fn from_solo(solo: &[Content], rate_hz: f64) -> Thresholds {
        let min = |f: &dyn Fn(&Content) -> f64| solo.iter().map(f).fold(f64::INFINITY, f64::min);
        let max = |f: &dyn Fn(&Content) -> f64| solo.iter().map(f).fold(f64::NEG_INFINITY, f64::max);
        let two = step3::expected_bytes_per_s(rate_hz);
        Thresholds {
            period_min: min(&|c| c.period_score.unwrap_or(0.0)) - PERIOD_MARGIN,
            rate_lo: min(&|c| c.rate()) * (1.0 - RATE_CHANGE),
            rate_hi: max(&|c| c.rate()) * (1.0 + RATE_CHANGE),
            entropy_lo: min(&|c| c.entropy_bits) - ENTROPY_MARGIN,
            entropy_hi: max(&|c| c.entropy_bits) + ENTROPY_MARGIN,
            stream_lo: two * RATE_BAND.0,
            stream_hi: two * RATE_BAND.1,
        }
    }

    pub fn record(&self) -> J {
        J::obj()
            .with("period_min", self.period_min)
            .with("rate_lo", self.rate_lo)
            .with("rate_hi", self.rate_hi)
            .with("entropy_lo", self.entropy_lo)
            .with("entropy_hi", self.entropy_hi)
            .with("stream_lo", self.stream_lo)
            .with("stream_hi", self.stream_hi)
            .with(
                "rules",
                format!(
                    "interesting = period score < period_min (the solo minimum - {PERIOD_MARGIN}) with at least {MIN_BYTES_FOR_PERIOD} bytes in the window, OR bytes/s in [stream_lo, stream_hi] = [{}, {}] x 2 x rate (toward 2 x sample rate); logged as 'rate changed' outside [rate_lo, rate_hi] (the solo range -/+ {}%), 'signature changed' with the entropy outside [entropy_lo, entropy_hi] (the solo range -/+ {ENTROPY_MARGIN} bits)",
                    RATE_BAND.0,
                    RATE_BAND.1,
                    RATE_CHANGE * 100.0
                ),
            )
    }

    /// Read back from the header line (resuming a record).
    pub fn from_header(line: &str) -> Option<Thresholds> {
        let get = |k: &str| -> Option<f64> {
            let i = line.find(&format!("\"{k}\":"))? + k.len() + 3;
            line[i..].split(|c: char| !(c.is_ascii_digit() || "+-.eE".contains(c))).next()?.parse().ok()
        };
        Some(Thresholds {
            period_min: get("period_min")?,
            rate_lo: get("rate_lo")?,
            rate_hi: get("rate_hi")?,
            entropy_lo: get("entropy_lo")?,
            entropy_hi: get("entropy_hi")?,
            stream_lo: get("stream_lo")?,
            stream_hi: get("stream_hi")?,
        })
    }

    /// (interesting reasons, other flags).
    pub fn judge(&self, c: &Content) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut why = Vec::new();
        let mut note = Vec::new();
        if c.bytes >= MIN_BYTES_FOR_PERIOD && c.period_score.is_some_and(|p| p < self.period_min) {
            why.push("period broken");
        }
        let r = c.rate();
        if r >= self.stream_lo && r <= self.stream_hi {
            why.push("rate toward 2 x sample rate");
        }
        if r < self.rate_lo || r > self.rate_hi {
            note.push("rate changed");
        }
        if c.bytes > 0 && (c.entropy_bits < self.entropy_lo || c.entropy_bits > self.entropy_hi) {
            note.push("signature changed");
        }
        (why, note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_scores_one_and_noise_scores_low() {
        let unit: Vec<u8> = (0..PERIOD).map(|i| (i * 7 + i / 13) as u8).collect();
        let replay: Vec<u8> = unit.iter().cycle().take(PERIOD * 10).copied().collect();
        assert_eq!(Content::of(&replay, 1.0).period_score, Some(1.0));
        let mut x = 12345u32;
        let noise: Vec<u8> = (0..PERIOD * 10)
            .map(|_| {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                (x >> 16) as u8
            })
            .collect();
        assert!(Content::of(&noise, 1.0).period_score.unwrap() < 0.02);
        let t = Thresholds::from_solo(&[Content::of(&replay, 0.1)], 2.4e6);
        let back = Thresholds::from_header(&J::obj().with("detector_thresholds", t.record()).render()).unwrap();
        assert!((back.period_min - t.period_min).abs() < 1e-12 && (back.stream_hi - t.stream_hi).abs() < 1e-6);
        assert_eq!(t.judge(&Content::of(&noise, 0.1)).0, vec!["period broken"]);
    }
}
