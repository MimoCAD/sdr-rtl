//! The power meter: every documented readback that could reflect signal
//! level, read together as one [`Snapshot`], and the re-levelling that
//! keeps a chosen readback mid-range during a gain sweep.
//!
//! The candidate readbacks (all read-only in their tables):
//!
//! | readback | where | document |
//! |---|---|---|
//! | `dagc_val` | page 3, 0x05 [7:0] | [RTL2832U] Table 10: "Gain of DAGC", linear voltage gain = dagc_val / 16; needs en_dagc (page 1 0x11[0]) |
//! | `if_agc_val` | page 3, {0x59, 0x5A} [13:0], s(14,13f) | Table 2 "IF AAGC Value"; the loop needs en_if_agc (page 1 0x04[7]) and aagc_hold = 0; its output is the AGC_IF sigma-delta pin (polarity polar_if_agc, page 0 0x0E[0]) |
//! | `rf_agc_val` | page 3, {0x5B, 0x5C} [13:0], s(14,13f) | Table 2 "RF AAGC Value"; en_rf_agc (page 1 0x04[6]) |
//! | `aagc_lock` | page 3, 0x50 [0] | Table 2 |
//! | `sfreq_off` | page 3, {0x18, 0x19} [13:0], s(14,24f) | Table 12, 9.9: sampling-frequency offset estimate (an OFDM quantity; recorded as a null) |
//! | `cfreq_off` | page 3, {0x5F, 0x61} [17:0], s(18,7f) | Table 12: carrier-frequency offset estimate (OFDM; a null) |
//! | `inc_det_cnt` | page 3, {0x28, 0x29} [8:0] | Table 9: impulse-noise event count |
//! | `Est_kq` | page 1, {0x66, 0x67} [11:0], u(12,11f) | Table 5: IQ gain-mismatch estimate (zero-IF; en_iq_est) |
//! | `Est_sin` | page 1, {0x68, 0x69} [11:0], s(12,10f) | Table 5: IQ phase-mismatch estimate |
//! | R1, R2, R3, R4 | the tuner's read-only registers | [R820T2] Table 1-2: R2 = VCO_INDICATOR[6:0], R3 = RF_INDICATOR[7:0] (named, never described), R1 and R4 unspecified |
//!
//! Table 2's offset column is headed {LSB, MSB}; every other table's
//! {MSB, LSB}. Both byte orders are decoded and recorded for the AGC
//! values; the bench decides which is right. Page 3 can also be dumped
//! whole (`--page3`) so any undocumented byte that moves with the tone
//! shows up; a meter chosen from there is declared as "p3:0xNN".

use std::time::{Duration, Instant};

use crate::json::{J, hex8, hex_bytes};
use crate::source::{self, Level, Source};
use crate::tuner;
use crate::usb::Rtl;

/// Snapshots per reading and the spacing between them: the AGC values
/// move, so each reading is three snapshots and the record keeps all.
pub const SNAPSHOTS: usize = 3;
pub const SNAPSHOT_GAP: Duration = Duration::from_millis(60);

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub t_ms: f64,
    pub dagc: Option<u8>,
    /// Page 3 0x59, 0x5A, 0x5B, 0x5C.
    pub agc: Option<[u8; 4]>,
    pub lock: Option<u8>,
    pub sfreq: Option<[u8; 2]>,
    pub cfreq: Option<[u8; 3]>,
    pub inc: Option<[u8; 2]>,
    /// Page 1 0x66..=0x69.
    pub est: Option<[u8; 4]>,
    /// R0..R4, bit-reversed.
    pub tuner: Option<[u8; 5]>,
    /// The single page-3 byte a "p3:0xNN" meter reads.
    pub p3_byte: Option<(u8, u8)>,
    pub page3: Option<Vec<u8>>,
    pub errors: Vec<String>,
}

/// Two's complement of the low `bits` of `v`.
pub fn signed(v: u32, bits: u32) -> i32 {
    let m = 1u32 << bits;
    let v = v & (m - 1);
    if v >= m / 2 { v as i32 - m as i32 } else { v as i32 }
}

/// A 14-bit two's-complement AGC value from its two bytes in a given order.
pub fn agc14(lsb: u8, msb: u8) -> i32 {
    signed((msb as u32 & 0x3F) << 8 | lsb as u32, 14)
}

impl Snapshot {
    pub fn read(rtl: &Rtl, start: Instant, page3: bool, p3_off: Option<u8>) -> Snapshot {
        let mut s = Snapshot { t_ms: start.elapsed().as_secs_f64() * 1e3, ..Snapshot::default() };
        let mut rd = |page: u8, off: u8, len: u16, name: &str| -> Option<Vec<u8>> {
            match rtl.demod_read(page, off, len) {
                Ok(v) => Some(v),
                Err(e) => {
                    s.errors.push(format!("{name}: {e}"));
                    None
                }
            }
        };
        let dagc = rd(3, 0x05, 1, "dagc_val").map(|v| v[0]);
        let agc = rd(3, 0x59, 4, "if/rf_agc_val").map(|v| [v[0], v[1], v[2], v[3]]);
        let lock = rd(3, 0x50, 1, "aagc_lock").map(|v| v[0]);
        let sfreq = rd(3, 0x18, 2, "sfreq_off").map(|v| [v[0], v[1]]);
        let cfreq = rd(3, 0x5F, 3, "cfreq_off").map(|v| [v[0], v[1], v[2]]);
        let inc = rd(3, 0x28, 2, "inc_det_cnt").map(|v| [v[0], v[1]]);
        let est = rd(1, 0x66, 4, "Est_kq/Est_sin").map(|v| [v[0], v[1], v[2], v[3]]);
        let p3_byte = p3_off.and_then(|o| rd(3, o, 1, "p3 meter byte").map(|v| (o, v[0])));
        let page3 = if page3 {
            // Two 128-byte reads (multi-byte demodulator reads auto-increment,
            // step 1D); on refusal, byte by byte.
            let mut all = Vec::with_capacity(256);
            let mut ok = true;
            for base in [0x00u8, 0x80] {
                match rtl.demod_read(3, base, 128) {
                    Ok(v) => all.extend(v),
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                all.clear();
                for off in 0..=255u8 {
                    all.push(rtl.demod_read8(3, off).unwrap_or(0));
                }
                s.errors.push("page 3 read byte by byte (a 128-byte read was refused); failed bytes recorded as 0".into());
            }
            Some(all)
        } else {
            None
        };
        s.dagc = dagc;
        s.agc = agc;
        s.lock = lock;
        s.sfreq = sfreq;
        s.cfreq = cfreq;
        s.inc = inc;
        s.est = est;
        s.p3_byte = p3_byte;
        s.page3 = page3;
        match tuner::read_status(rtl) {
            Ok(t) => s.tuner = Some(t),
            Err(e) => s.errors.push(format!("tuner status: {e}")),
        }
        s
    }

    /// Every raw byte and every decoding.
    pub fn full(&self) -> J {
        let mut j = J::obj().with("t_ms", (self.t_ms * 10.0).round() / 10.0);
        j.set("dagc_val", self.dagc);
        j.set("dagc_gain_linear", self.dagc.map(|v| v as f64 / 16.0));
        if let Some(a) = self.agc {
            j.set("agc_bytes_0x59_0x5C", hex_bytes(&a));
            j.set("if_agc_val_lsb_first", agc14(a[0], a[1]));
            j.set("if_agc_val_msb_first", agc14(a[1], a[0]));
            j.set("rf_agc_val_lsb_first", agc14(a[2], a[3]));
            j.set("rf_agc_val_msb_first", agc14(a[3], a[2]));
        }
        j.set("aagc_lock", self.lock.map(|v| v & 1));
        j.set("aagc_lock_byte", self.lock.map(hex8));
        if let Some(b) = self.sfreq {
            j.set("sfreq_off_bytes", hex_bytes(&b));
            j.set("sfreq_off_msb_first", signed((b[0] as u32) << 8 | b[1] as u32, 14));
        }
        if let Some(b) = self.cfreq {
            j.set("cfreq_off_bytes", hex_bytes(&b));
            j.set("cfreq_off_msb_first", signed((b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32, 18));
        }
        if let Some(b) = self.inc {
            j.set("inc_det_cnt", (b[0] as u32 & 1) << 8 | b[1] as u32);
        }
        if let Some(b) = self.est {
            j.set("est_bytes_0x66_0x69", hex_bytes(&b));
            j.set("est_kq", (b[0] as u32 & 0x0F) << 8 | b[1] as u32);
            j.set("est_sin", signed((b[2] as u32) << 8 | b[3] as u32, 12));
        }
        if let Some(t) = self.tuner {
            j.set("tuner", tuner::status_record(&t));
        }
        if let Some((o, v)) = self.p3_byte {
            j.set("p3_meter_byte", J::obj().with("offset", hex8(o)).with("value", v));
        }
        if let Some(p) = &self.page3 {
            j.set("page3", hex_bytes(p));
        }
        if !self.errors.is_empty() {
            j.set("errors", J::Arr(self.errors.iter().map(|e| J::from(e.as_str())).collect()));
        }
        j
    }

    /// The compact form for sweep lines: d dagc_val; a the four AGC bytes
    /// 0x59..0x5C; l aagc_lock byte; t R0..R4 bit-reversed; p the p3
    /// meter byte; e errors.
    pub fn compact(&self) -> J {
        let mut j = J::obj();
        j.set("d", self.dagc);
        j.set("a", self.agc.map(|a| J::Arr(a.iter().map(|b| J::from(*b)).collect())));
        j.set("l", self.lock);
        j.set("t", self.tuner.map(|t| J::Arr(t.iter().map(|b| J::from(*b)).collect())));
        if let Some((_, v)) = self.p3_byte {
            j.set("p", v);
        }
        if !self.errors.is_empty() {
            j.set("e", J::Arr(self.errors.iter().map(|e| J::from(e.as_str())).collect()));
        }
        j
    }

    /// The value of one candidate meter.
    pub fn value(&self, m: Meter) -> Option<f64> {
        match m {
            Meter::Dagc => self.dagc.map(f64::from),
            Meter::IfAgcLsbFirst => self.agc.map(|a| agc14(a[0], a[1]) as f64),
            Meter::IfAgcMsbFirst => self.agc.map(|a| agc14(a[1], a[0]) as f64),
            Meter::RfAgcLsbFirst => self.agc.map(|a| agc14(a[2], a[3]) as f64),
            Meter::RfAgcMsbFirst => self.agc.map(|a| agc14(a[3], a[2]) as f64),
            Meter::Tuner(i) => self.tuner.map(|t| if i == 2 { (t[2] & 0x7F) as f64 } else { t[i as usize] as f64 }),
            Meter::Page3(_) => self.p3_byte.map(|(_, v)| v as f64),
        }
    }
}

/// The readbacks a meter can be chosen from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meter {
    Dagc,
    IfAgcLsbFirst,
    IfAgcMsbFirst,
    RfAgcLsbFirst,
    RfAgcMsbFirst,
    /// R1..R4 (R2 masked to VCO_INDICATOR[6:0]).
    Tuner(u8),
    /// Any page-3 byte, declared by offset.
    Page3(u8),
}

/// Every named meter, for the verdict table of the meter step.
pub const ALL: [(Meter, &str); 9] = [
    (Meter::Dagc, "dagc_val"),
    (Meter::IfAgcLsbFirst, "if_agc_val"),
    (Meter::IfAgcMsbFirst, "if_agc_val_msb_first"),
    (Meter::RfAgcLsbFirst, "rf_agc_val"),
    (Meter::RfAgcMsbFirst, "rf_agc_val_msb_first"),
    (Meter::Tuner(1), "r1"),
    (Meter::Tuner(2), "r2"),
    (Meter::Tuner(3), "r3"),
    (Meter::Tuner(4), "r4"),
];

impl Meter {
    pub fn parse(s: &str) -> Result<Meter, String> {
        if let Some((m, _)) = ALL.iter().find(|(_, n)| *n == s) {
            return Ok(*m);
        }
        if let Some(h) = s.strip_prefix("p3:0x") {
            return u8::from_str_radix(h, 16).map(Meter::Page3).map_err(|e| format!("{s}: {e}"));
        }
        Err(format!("meter {s}: one of {} or p3:0xNN", ALL.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(" | ")))
    }

    pub fn name(self) -> String {
        match self {
            Meter::Page3(o) => format!("p3:0x{o:02X}"),
            m => ALL.iter().find(|(x, _)| *x == m).map(|(_, n)| n.to_string()).unwrap_or_default(),
        }
    }

    pub fn page3_offset(self) -> Option<u8> {
        match self {
            Meter::Page3(o) => Some(o),
            _ => None,
        }
    }
}

/// The meter a gain sweep keeps mid-range: which readback, the window,
/// and whether it rises with signal (the meter step tells).
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub meter: Meter,
    pub lo: f64,
    pub hi: f64,
    pub rises_with_signal: bool,
}

impl Spec {
    /// `--meter NAME LO HI up|down`.
    pub fn parse(args: &[String]) -> Result<Spec, String> {
        const USAGE: &str = "--meter NAME LO HI up|down (up = the reading rises with signal)";
        let meter = Meter::parse(args.first().ok_or(USAGE)?)?;
        let num = |i: usize| -> Result<f64, String> { args.get(i).ok_or(USAGE)?.parse().map_err(|e| format!("{USAGE}: {e}")) };
        let lo = num(1)?;
        let hi = num(2)?;
        let rises = match args.get(3).map(String::as_str) {
            Some("up") => true,
            Some("down") => false,
            _ => return Err(USAGE.into()),
        };
        if lo >= hi {
            return Err(format!("{USAGE}: LO must be below HI"));
        }
        Ok(Spec { meter, lo, hi, rises_with_signal: rises })
    }

    pub fn record(&self) -> J {
        J::obj().with("meter", self.meter.name()).with("window_lo", self.lo).with("window_hi", self.hi).with("rises_with_signal", self.rises_with_signal)
    }
}

/// One reading: [`SNAPSHOTS`] snapshots, with the median of a meter.
pub struct Reading {
    pub snapshots: Vec<Snapshot>,
}

impl Reading {
    pub fn take(rtl: &Rtl, start: Instant, page3: bool, p3_off: Option<u8>) -> Reading {
        Reading::take_n(rtl, start, page3, p3_off, SNAPSHOTS)
    }

    /// `count` snapshots [`SNAPSHOT_GAP`] apart; the page-3 dump, if
    /// asked for, only in the first.
    pub fn take_n(rtl: &Rtl, start: Instant, page3: bool, p3_off: Option<u8>, count: usize) -> Reading {
        let count = count.max(1);
        let mut snapshots = Vec::with_capacity(count);
        for n in 0..count {
            if n > 0 {
                std::thread::sleep(SNAPSHOT_GAP);
            }
            snapshots.push(Snapshot::read(rtl, start, page3 && n == 0, p3_off));
        }
        Reading { snapshots }
    }

    pub fn median(&self, m: Meter) -> Option<f64> {
        let mut v: Vec<f64> = self.snapshots.iter().filter_map(|s| s.value(m)).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[v.len() / 2])
    }

    pub fn spread(&self, m: Meter) -> Option<f64> {
        let v: Vec<f64> = self.snapshots.iter().filter_map(|s| s.value(m)).collect();
        let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
        if v.is_empty() { None } else { Some(max - min) }
    }

    pub fn full(&self) -> J {
        J::Arr(self.snapshots.iter().map(Snapshot::full).collect())
    }

    pub fn compact(&self) -> J {
        J::Arr(self.snapshots.iter().map(Snapshot::compact).collect())
    }

    /// Medians of every named meter (and the p3 byte), for a line.
    pub fn medians(&self) -> J {
        let mut j = J::obj();
        for (m, n) in ALL {
            j.set(n, self.median(m));
        }
        j
    }
}

/// Whether a reading moved between two states by more than their spreads:
/// |median_on - median_off| > 2 * (spread_off + spread_on) + 1.
pub fn moved(off: &Reading, on: &Reading, m: Meter) -> Option<(f64, f64, bool)> {
    let (a, b) = (off.median(m)?, on.median(m)?);
    let s = off.spread(m).unwrap_or(0.0) + on.spread(m).unwrap_or(0.0);
    Some((a, b, (b - a).abs() > 2.0 * s + 1.0))
}

/// The level steps the re-levelling uses: 6 dB, exact on the backoff.
pub const RELEVEL_STEP_DB: i32 = 6;
pub const RELEVEL_TRIES: usize = 12;

/// Keep the chosen meter inside its window by moving the source in
/// [`RELEVEL_STEP_DB`] steps, each step recorded with the reading that
/// prompted it. Returns the final reading and the adjustments made; Err
/// only if the source cannot be commanded. A source at its limit ends the
/// loop with the reading as it is (recorded).
pub fn relevel(src: &mut Source, rtl: &Rtl, start: Instant, spec: &Spec, hz: f64, level: &mut Level, page3: bool) -> Result<(Reading, J), String> {
    let mut log = J::Arr(Vec::new());
    let p3 = spec.meter.page3_offset();
    let mut reading = Reading::take(rtl, start, page3, p3);
    for _ in 0..RELEVEL_TRIES {
        source::check()?;
        let Some(v) = reading.median(spec.meter) else {
            log.push(J::obj().with("stop", "the meter could not be read").with("reading", reading.compact()));
            break;
        };
        if v >= spec.lo && v <= spec.hi {
            break;
        }
        let too_much = (v > spec.hi) == spec.rises_with_signal;
        let delta = if too_much { -RELEVEL_STEP_DB } else { RELEVEL_STEP_DB };
        let before = *level;
        match level.step(delta) {
            Ok(knob) => {
                let t = src.level(hz, *level)?;
                log.push(J::obj().with("meter_value", v).with("source_before", before.tone(hz).record()).with("moved_db", delta).with("knob", knob).with("source", t));
            }
            Err(e) => {
                log.push(J::obj().with("meter_value", v).with("stop", e));
                break;
            }
        }
        reading = Reading::take(rtl, start, page3, p3);
    }
    Ok((reading, log))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_two_complement_fields() {
        assert_eq!(signed(0x3FFF, 14), -1);
        assert_eq!(signed(0x1FFF, 14), 8191);
        assert_eq!(signed(0x2000, 14), -8192);
        assert_eq!(agc14(0x7F, 0x00), 127);
        assert_eq!(agc14(0xFF, 0x3F), -1);
        // The upper two bits of the MSB are outside the 14-bit field.
        assert_eq!(agc14(0x00, 0xC0), 0);
    }

    #[test]
    fn parses_meters() {
        assert_eq!(Meter::parse("dagc_val").unwrap(), Meter::Dagc);
        assert_eq!(Meter::parse("r3").unwrap(), Meter::Tuner(3));
        assert_eq!(Meter::parse("p3:0x5A").unwrap(), Meter::Page3(0x5A));
        assert!(Meter::parse("nope").is_err());
        let s = Spec::parse(&["dagc_val".into(), "40".into(), "200".into(), "down".into()]).unwrap();
        assert!(!s.rises_with_signal);
        assert!(Spec::parse(&["dagc_val".into(), "200".into(), "40".into(), "down".into()]).is_err());
    }
}
