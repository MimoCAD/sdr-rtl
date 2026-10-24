//! Step 22: candidate tap sets for the demodulator's programmable FIR
//! (finite impulse response filter, findings 12.4) for narrowband
//! channels (12.5 kHz, Project 25) anywhere in a 2.4 Msps window,
//! designed against a model built from the step-12 records only
//! (`derivation/step22-fir-design.md` has the derivation and results).
//!
//!   firdesign chain                  the model's tables, one JSON line per point
//!   firdesign validate               the model against every allowed step-12 sweep (text)
//!   firdesign design                 ours and the candidates, one JSON line each
//!   firdesign score c0,c1,...,c15    one set's metrics, one JSON line
//!   firdesign predict c0,c1,...,c15  one set's modelled sweep over ±6 MHz, for the bench
//!   (any: --data DIR, default derivation/data beside this crate)
//!
//! Built with default features (the crate's `taps`, `demod` and the IF
//! filter table only; no USB). From the workspace root:
//!   cargo run --release -p sdr-rtl --example firdesign -- design
//!
//! Clean room: the inputs are this crate's step-12 records and the facts
//! the crate already holds (taps, the DDC's IF, the IF filter corners).
//! The tap sets read from the records are only our design, the chip's
//! power-on image (`step12-poweron.jsonl`) and field lists (`fields:`,
//! `single:`, `pair:`); a record of any other set is skipped unparsed and
//! only counted.
//!
//! The model, at an output rate R (the FIR at f_xtal = 28.8 MHz, the
//! symbol clock f_symbol = 4 R, the digital down-converter's (DDC's)
//! intermediate frequency IF = 3.57 MHz with the local oscillator above
//! the radio frequency (RF)):
//! - A component at RF offset δ reaches the FIR at two frequencies:
//!   ν = δ and ν = 2·IF − δ (the real IF's other side, shifted up by the
//!   DDC), with the tuner's IF filter F(g) at g = IF − δ. So a dense band
//!   puts S(ν) = F(|fold(ν − IF, f_xtal)|) ahead of the FIR.
//! - After the FIR a fixed chain decimates by f_xtal / f_symbol with no
//!   filter of its own, then by 4 through a fixed filter D4 at f_symbol:
//!   D(ν) = D4(fold(ν, f_symbol)). D4 is the c15-only sweep
//!   (`step12-chain.jsonl`) less that set's FIR; the mirror lines of the
//!   same sweep (ν = 2·IF − δ, near f_symbol) test the claim.
//! - Every ν = L + kR (k ≠ 0, one f_xtal period) lands on the channel at
//!   L: alias(L) = Σ S(ν)·|H(ν)/H(0)|²·D(ν)²; the channel itself is
//!   |H(L)/H(0)|²·D(L)². Two floors complete the noise: the analogue-to-
//!   digital converter's (ADC's, ahead of the FIR, shaped like the
//!   signal) and the output's quantisation (after it, flat), each from the
//!   records' tone-off medians, the latter scaled with the output's power
//!   so its composite sits at [`COMPOSITE_DBFS`]. The figure of merit per
//!   channel is its signal to interference-plus-noise ratio (SINR).

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use sdr_rtl::demod::{OUTPUT_DECIMATION, ddc_if_hz};
use sdr_rtl::protocol::{STREAM_ENABLE_FIR_X8, STREAM_ENABLE_VALUE};
use sdr_rtl::tables::if_filter::{FILT_BW_UPPER_3DB, REFERENCE};
use sdr_rtl::taps::{
    self, DESIGN, DESIGN_DC_GAIN, DESIGN_FS_HZ, FINDINGS_10_DC_GAIN, TAPS, field_bits, field_range,
};

/// The FIR's output gain (page 0 0x19 bit 5) the step-12 records were
/// taken with, and the one the stream runs at now (findings 23).
const RECORDED_FIR_GAIN: f64 = 8.0;
fn stream_fir_gain() -> f64 {
    if STREAM_ENABLE_VALUE & STREAM_ENABLE_FIR_X8 != 0 {
        8.0
    } else {
        1.0
    }
}

// ---- The design's setting ----

/// The output rate designed for.
const RATE_HZ: f64 = 2.4e6;
/// The receiver's opening rate (`demod::DEFAULT_OUTPUT_RATE_SPS`): each
/// candidate is also scored there, as one set serves every rate today.
const LOW_RATE_HZ: f64 = 2.048e6;
/// A channel's centre stays this far inside the output's edge (a 12.5 kHz
/// channel whole inside it).
const EDGE_MARGIN_HZ: f64 = 20e3;
/// Channel centres designed for: ±1.18 MHz.
const WINDOW_HZ: f64 = RATE_HZ / 2.0 - EDGE_MARGIN_HZ;
/// The landing grid's step.
const LANDING_STEP_HZ: f64 = 10e3;
/// The 32 taps: field k drives taps k and FIR_LENGTH − 1 − k (findings 12.4).
const FIR_LENGTH: usize = 2 * TAPS;
/// The DC gain the search holds (`--dc-gain`, default taps.rs's).
/// The bench (findings 21, gainsweep) measured the floor ahead of the FIR
/// carried up by the FIR's gain: at one output level a lower DC gain with
/// the analogue gain making up the level reads 1.5–2.3 dB more carrier over
/// the floor, so the DC gain is a design variable, not a convention.
static DC_GAIN: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// The sum of the 16 fields: half the DC gain.
fn field_sum() -> i32 {
    *DC_GAIN.get_or_init(|| DESIGN_DC_GAIN as i32) / 2
}

// ---- The records ----

/// The step-12 sweeps' step.
const TABLE_STEP_HZ: f64 = 50e3;
/// The in-band reference: the mean tone level within ±300 kHz (filterfit's span).
const REF_SPAN_HZ: f64 = 300e3;
/// A model bin this close to the median is the floor, not a tone (filterfit's).
const FLOOR_MARGIN_DB: f64 = 6.0;
/// A comparison counts only where both levels clear the floor by this much (tapfit's).
const COMPARE_MARGIN_DB: f64 = 10.0;
/// A reported line belongs to a model's place within this (the ~4.2 ppm
/// between source and dongle, +1.3 kHz at 300 MHz, plus a few bins).
const LINE_TOL_HZ: f64 = 4e3;
/// Two places closer than this cannot be told apart in a record.
const COINCIDENT_HZ: f64 = 15e3;
/// The tone source's own line 398.7 kHz under the tone, seen with every
/// tap set while the tone is in band (filterfit: "the source's own LO
/// leakage (tone − 400 kHz)"); a mirror line there is not counted.
const SOURCE_COMPANION_HZ: f64 = -398.7e3;
/// A spur of the tone-off reference matches a line within this (filterfit's).
const SPUR_TOL_HZ: f64 = 2e3;
const SPUR_LEVEL_TOL_DB: f64 = 6.0;
/// D4 is read on the RF side where the IF filter is flat: both sides up
/// to here (g ≥ 0.57 MHz, findings 4g), the lower side beyond.
const BOTH_SIDES_FLAT_HZ: f64 = 3.0e6;
/// The c15-only set's name in `step12-chain.jsonl` (findings 12.5).
const CHAIN_SET: &str = "single:15:2047";
/// The design treats D4 as its upper envelope over ±this, so no candidate
/// leans on a measured null narrower than the sweep's step resolves.
const ENVELOPE_HZ: f64 = 100e3;

// ---- The noise (the bench's spectra: Hann, 4,096 points, a full-scale tone on a bin = 0 dB) ----

const FFT_POINTS: f64 = 4096.0;
/// The Hann window's equivalent noise bandwidth, in bins.
const HANN_ENBW_BINS: f64 = 1.5;
/// The dense band's composite level at the output: 12 dB of headroom
/// under full scale for many carriers' peaks.
const COMPOSITE_DBFS: f64 = -12.0;
/// A tap set whose DC gain is the convention's within this counts as
/// "the floor at our gain".
const DC_GAIN_TOL: f64 = 16.0;

// ---- The search ----

/// Pair moves (one field up, another down by the same) keep the DC gain.
const STEPS: [i32; 8] = [128, 64, 32, 16, 8, 4, 2, 1];
/// Basin hops from the best found: random pair moves, then the search again.
const HOPS: usize = 24;
const HOP_MOVES: usize = 6;
const HOP_MAX_STEP: u32 = 24;
/// The soft minimum's exponent for the alias-only objective.
const SOFT_MIN_POWER: f64 = 8.0;
/// Seeds: windowed sincs over these cutoffs and windows.
const SEED_CUTOFFS_HZ: [f64; 9] = [
    0.8e6, 0.9e6, 1.0e6, 1.1e6, 1.2e6, 1.3e6, 1.4e6, 1.5e6, 1.6e6,
];
const HAMMING_A: f64 = 0.54;
const HAMMING_B: f64 = 0.46;
const KAISER_BETAS: [f64; 3] = [4.0, 6.0, 8.0];

// ---- The 2026-10-12 tone test (the caller's bench, 852.30625 MHz, 2.4 Msps) ----

/// The tones (MHz from the centre) whose landings were measured.
const TONE_TEST_MHZ: [f64; 8] = [1.3, 1.6, 2.0, -1.6, -2.0, 3.0, -3.0, -1.3];

fn fold(x: f64, period: f64) -> f64 {
    x - (x / period).round() * period
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

fn lin(db: f64) -> f64 {
    10f64.powf(db / 10.0)
}

// ---- The FIR (findings 12.4) ----

/// The 16 cosines of field k's tap pair at ν: |H(ν)| = |Σ c_k · basis_k(ν)|.
fn basis(nu: f64) -> [f64; TAPS] {
    let mut b = [0.0; TAPS];
    for (k, v) in b.iter_mut().enumerate() {
        let spacing = (FIR_LENGTH - 1 - 2 * k) as f64;
        *v = 2.0 * (PI * nu * spacing / DESIGN_FS_HZ).cos();
    }
    b
}

fn response(c: &[i32; TAPS], nu: f64) -> f64 {
    basis(nu).iter().zip(c).map(|(b, &v)| b * v as f64).sum()
}

/// |H(ν)/H(0)|² in dB.
fn fir_db(c: &[i32; TAPS], nu: f64) -> f64 {
    let h0 = response(c, 0.0);
    db((response(c, nu) / h0).powi(2))
}

fn fields_json(c: &[i32; TAPS]) -> String {
    format!(
        "[{}]",
        c.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn hex(c: &[i32; TAPS]) -> String {
    taps::pack(c)
        .map(|r| r.iter().map(|b| format!("{b:02X}")).collect())
        .unwrap_or_default()
}

// ---- Records ----

fn num(l: &str, key: &str) -> Option<f64> {
    let k = format!("\"{key}\":");
    let i = l.find(&k)? + k.len();
    let rest = &l[i..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

fn text(l: &str, key: &str) -> Option<String> {
    let k = format!("\"{key}\":\"");
    let i = l.find(&k)? + k.len();
    let end = l[i..].find('"')?;
    Some(l[i..i + end].to_string())
}

/// `"key":[[a,b],[c,d],...]`.
fn pairs(l: &str, key: &str) -> Vec<(f64, f64)> {
    let k = format!("\"{key}\":[");
    let Some(i) = l.find(&k).map(|i| i + k.len()) else {
        return Vec::new();
    };
    let end = l[i..].find("]]").map(|e| i + e + 1).unwrap_or(i);
    l[i..end]
        .split("],")
        .filter_map(|p| {
            let p = p.trim_matches(|c| c == '[' || c == ']');
            let mut it = p.split(',');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// `"key":[1,2,...]` as the 16 fields.
fn int_list(l: &str, key: &str) -> Option<[i32; TAPS]> {
    let k = format!("\"{key}\":[");
    let i = l.find(&k)? + k.len();
    let end = l[i..].find(']')? + i;
    let v: Vec<i32> = l[i..end]
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    v.try_into().ok()
}

struct Point {
    delta: f64,
    plain_db: f64,
    median_db: f64,
    lines: Vec<(f64, f64)>,
}

struct Sweep {
    file: String,
    rate: f64,
    set: String,
    fields: [i32; TAPS],
    off_median_db: f64,
    off_lines: Vec<(f64, f64)>,
    points: Vec<Point>,
}

impl Sweep {
    /// The mean tone level within ±[`REF_SPAN_HZ`].
    fn reference(&self) -> f64 {
        let v: Vec<f64> = self
            .points
            .iter()
            .filter(|p| p.delta.abs() <= REF_SPAN_HZ)
            .map(|p| p.plain_db)
            .collect();
        v.iter().sum::<f64>() / v.len().max(1) as f64
    }

    fn is_spur(&self, hz: f64, level: f64) -> bool {
        self.off_lines
            .iter()
            .any(|(f, p)| (f - hz).abs() < SPUR_TOL_HZ && (p - level).abs() < SPUR_LEVEL_TOL_DB)
    }
}

/// The step-12 records this design may read.
const RECORDS: [&str; 6] = [
    "step12-chain.jsonl",
    "step12-fold-clean.jsonl",
    "step12-source8-check.jsonl",
    "step12-candidates.jsonl",
    "step12-candidates-round2.jsonl",
    "step12-narrow-weight.jsonl",
];
const POWER_ON_RECORD: &str = "step12-poweron.jsonl";

struct Records {
    sweeps: Vec<Sweep>,
    skipped: usize,
}

/// A set this design may read: ours, the power-on image, field lists.
fn allowed(set: &str, hex: &str, power_on_hex: &str) -> bool {
    set == "design"
        || set.starts_with("fields:")
        || set.starts_with("single:")
        || set.starts_with("pair:")
        || (set.starts_with("hex:") && hex == power_on_hex)
}

fn load(dir: &Path) -> Result<Records, String> {
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map_err(|e| format!("{}: {e}", dir.join(name).display()))
    };
    let power_on = read(POWER_ON_RECORD)?;
    let power_on_hex = power_on
        .lines()
        .find_map(|l| text(l, "hex"))
        .ok_or("no power-on image")?;
    let mut sweeps: Vec<Sweep> = Vec::new();
    let mut skipped = 0;
    for name in RECORDS {
        // (rate, set) → hex, from the heads: a sweep line carries no hex.
        let mut head_hex: BTreeMap<(i64, String), String> = BTreeMap::new();
        for l in read(name)?.lines() {
            let (Some(set), Some(rate)) = (text(l, "set"), num(l, "rate")) else {
                continue;
            };
            let key = (rate as i64, set.clone());
            if l.contains("\"kind\":\"sweep_head\"") {
                let h = text(l, "hex").unwrap_or_default();
                head_hex.insert(key, h.clone());
                if !allowed(&set, &h, &power_on_hex) {
                    skipped += 1;
                    continue;
                }
                let Some(fields) = int_list(l, "fields") else {
                    continue;
                };
                sweeps.push(Sweep {
                    file: name.to_string(),
                    rate,
                    set,
                    fields,
                    off_median_db: num(l, "off_median_dbfs").unwrap_or(f64::NAN),
                    off_lines: pairs(l, "off_lines"),
                    points: Vec::new(),
                });
            } else if l.contains("\"kind\":\"sweep\"") {
                let h = head_hex.get(&key).cloned().unwrap_or_default();
                if !allowed(&set, &h, &power_on_hex) {
                    skipped += 1;
                    continue;
                }
                let Some(s) = sweeps
                    .iter_mut()
                    .rev()
                    .find(|s| s.file == name && s.rate == rate && s.set == set)
                else {
                    continue;
                };
                s.points.push(Point {
                    delta: num(l, "delta").unwrap_or(f64::NAN),
                    plain_db: num(l, "plain_dbfs").unwrap_or(f64::NAN),
                    median_db: num(l, "median_dbfs").unwrap_or(f64::NAN),
                    lines: pairs(l, "lines"),
                });
            }
        }
    }
    Ok(Records { sweeps, skipped })
}

// ---- The fixed chain after the FIR ----

/// D4 on a [`TABLE_STEP_HZ`] grid from 0 to f_symbol / 2 (it is even),
/// dB re 0 Hz; `bound` where the tone was at the floor (the value is then
/// an upper bound, used as the value).
struct Chain {
    rate: f64,
    symbol_hz: f64,
    d4: Vec<(f64, bool)>,
}

impl Chain {
    fn from_sweep(s: &Sweep) -> Chain {
        let reference = s.reference();
        let symbol_hz = OUTPUT_DECIMATION * s.rate;
        let at = |delta: f64| s.points.iter().find(|p| (p.delta - delta).abs() < 1.0);
        // The tone's level at δ less the c15-only FIR, re the reference.
        let read = |delta: f64| {
            at(delta).map(|p| {
                let v =
                    p.plain_db - reference - (fir_db(&s.fields, delta) - fir_db(&s.fields, 0.0));
                (v, p.plain_db < p.median_db + FLOOR_MARGIN_DB)
            })
        };
        let n = (symbol_hz / 2.0 / TABLE_STEP_HZ).round() as usize;
        let mut d4 = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let x = i as f64 * TABLE_STEP_HZ;
            let lower = read(-x);
            let upper = if x <= BOTH_SIDES_FLAT_HZ {
                read(x)
            } else {
                None
            };
            let v = match (lower, upper) {
                (Some((a, false)), Some((b, false))) => ((a + b) / 2.0, false),
                (Some((a, false)), _) => (a, false),
                (_, Some((b, false))) => (b, false),
                (Some((a, true)), Some((b, true))) => (a.max(b), true),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => d4.last().copied().unwrap_or((0.0, true)),
            };
            d4.push(v);
        }
        Chain {
            rate: s.rate,
            symbol_hz,
            d4,
        }
    }

    /// D4 at |x| (linear between grid points, in dB).
    fn d4_db(&self, x: f64) -> f64 {
        let t = x.abs() / TABLE_STEP_HZ;
        let i = (t.floor() as usize).min(self.d4.len() - 1);
        let j = (i + 1).min(self.d4.len() - 1);
        let f = t - i as f64;
        self.d4[i].0 * (1.0 - f) + self.d4[j].0 * f
    }

    /// D4's upper envelope over ±[`ENVELOPE_HZ`].
    fn d4_envelope_db(&self, x: f64) -> f64 {
        let steps = (ENVELOPE_HZ / TABLE_STEP_HZ).round() as i32;
        (-steps..=steps)
            .map(|s| {
                self.d4_db(
                    (x.abs() + s as f64 * TABLE_STEP_HZ)
                        .abs()
                        .min(self.symbol_hz / 2.0),
                )
            })
            .fold(f64::MIN, f64::max)
    }

    /// The whole chain after the FIR at ν: the undecimated fold to f_symbol, then D4.
    fn d_db(&self, nu: f64, envelope: bool) -> f64 {
        let x = fold(nu, self.symbol_hz);
        if envelope {
            self.d4_envelope_db(x)
        } else {
            self.d4_db(x)
        }
    }
}

// ---- The tuner's IF filter (findings 4g, `tables::if_filter`) ----

/// The IF passband falls below this for every code (findings 4g).
const IF_LOW_EDGE_HZ: f64 = 0.5e6;
/// The level at a −3 dB corner.
const CORNER_DB: f64 = -3.0;
/// The table's "dB 2 MHz past the corner" is read over this span.
const SKIRT_SPAN_HZ: f64 = 2.0e6;
/// No skirt is modelled deeper than this.
const ANALOG_FLOOR_DB: f64 = -60.0;

/// The A/B harness's beds (docs/rtl-ab-bench.md): nine of ten recorded
/// at 4.8 Msps and replayed at that rate, so carriers out to ±2.4 MHz
/// (the tenth, 24 Msps, is the dense case).
const BED_SPAN_HZ: f64 = 2.4e6;
/// A band with nothing beyond the output's own ±R/2: the bound where no
/// alias exists and only the passband's shape against the floors counts.
const NO_SPILL_SPAN_HZ: f64 = RATE_HZ / 2.0;

#[derive(Clone, Copy)]
struct Analog {
    corner_hz: f64,
    /// dB per Hz beyond each edge.
    skirt: f64,
    /// The RF occupied: carriers within ±this of the centre (infinite for
    /// a dense band out to the IF filter's reach).
    span_hz: f64,
}

impl Analog {
    fn of(corner_hz: f64, db_2mhz_past_corner: f64) -> Analog {
        Analog {
            corner_hz,
            skirt: (CORNER_DB - db_2mhz_past_corner) / SKIRT_SPAN_HZ,
            span_hz: f64::INFINITY,
        }
    }

    /// The same filter with the band occupied only within ±`span_hz`.
    fn limited(self, span_hz: f64) -> Analog {
        Analog { span_hz, ..self }
    }

    /// F(g) in dB at IF g ≥ 0: flat between the low edge and the corner,
    /// −3 dB at each, the measured skirt beyond.
    fn f_db(&self, g: f64) -> f64 {
        let v = if g < IF_LOW_EDGE_HZ {
            CORNER_DB - self.skirt * (IF_LOW_EDGE_HZ - g)
        } else if g > self.corner_hz {
            CORNER_DB - self.skirt * (g - self.corner_hz)
        } else {
            0.0
        };
        v.max(ANALOG_FLOOR_DB)
    }

    /// A dense band's level ahead of the FIR at ν, re a channel in the
    /// window: the RF that lands there through the IF, either side (the
    /// RF at IF − g, g the IF frequency ν stands for).
    fn occupancy_db(&self, nu: f64) -> f64 {
        let g = fold(nu - ddc_if_hz(), DESIGN_FS_HZ).abs();
        if (ddc_if_hz() - g).abs() > self.span_hz {
            return f64::NEG_INFINITY;
        }
        self.f_db(g)
    }
}

// ---- The noise floors (records' tone-off medians) ----

#[derive(Clone, Copy)]
struct Floors {
    /// The output's own floor (H ≈ 0), dBFS per bin.
    output_db: f64,
    /// The ADC's floor through a DC-gain-4,096 set, dBFS per bin.
    adc_db: f64,
    /// Both re a channel's density in a dense band at [`COMPOSITE_DBFS`].
    q: f64,
    n: f64,
}

fn floors(rec: &Records) -> Floors {
    let at_rate: Vec<&Sweep> = rec.sweeps.iter().filter(|s| s.rate == RATE_HZ).collect();
    let quietest = at_rate
        .iter()
        .min_by(|a, b| {
            response(&a.fields, 0.0)
                .abs()
                .total_cmp(&response(&b.fields, 0.0).abs())
        })
        .expect("sweeps at the rate");
    let output_db = quietest.off_median_db;
    let mut ours: Vec<f64> = at_rate
        .iter()
        .filter(|s| (response(&s.fields, 0.0) - FINDINGS_10_DC_GAIN).abs() <= DC_GAIN_TOL)
        .map(|s| s.off_median_db)
        .collect();
    ours.sort_by(f64::total_cmp);
    let total_db = ours[ours.len() / 2];
    let adc_db = db(lin(total_db) - lin(output_db));
    let composite_per_bin = COMPOSITE_DBFS + db(HANN_ENBW_BINS / FFT_POINTS);
    Floors {
        output_db,
        adc_db,
        q: lin(output_db - composite_per_bin),
        n: lin(adc_db - composite_per_bin),
    }
}

// ---- The scene: every landing and the frequencies that fold onto it ----

/// The far stopband every candidate keeps: |H/H(0)| ≤ [`GUARD_DB`] from
/// here to f_xtal / 2. Beyond 3.6 MHz the model's D4 is a floor-limited
/// bound, and other rates fold these frequencies elsewhere (findings
/// 12.7: side lobes at 4–6 MHz cost 12–30 dB at 2.88 Msps); our design
/// holds −51.6 dB there.
const GUARD_FROM_HZ: f64 = 3.6e6;
const GUARD_DB: f64 = -45.0;
/// The objective loses this per dB a guard point stands over [`GUARD_DB`].
const GUARD_PENALTY_PER_DB: f64 = 10.0;

struct Scene {
    landings: Vec<f64>,
    folds: usize,
    /// Per landing then fold (fold 0 = the landing itself), then the
    /// guard points: the basis.
    basis: Vec<[f64; TAPS]>,
    /// Where the guard points start in `basis`.
    guard_at: usize,
    /// The chain after the FIR, D(ν)², per landing and fold.
    chain: Vec<f64>,
    /// The dense band ahead of the FIR plus the ADC's floor, per landing and fold.
    occupancy: Vec<f64>,
    q: f64,
    n: f64,
}

impl Scene {
    fn new(
        chain: &Chain,
        analog: &Analog,
        floors: &Floors,
        window_hz: f64,
        envelope: bool,
        q_shift_db: f64,
    ) -> Scene {
        let folds = (DESIGN_FS_HZ / chain.rate).round() as usize;
        let n_l = (2.0 * window_hz / LANDING_STEP_HZ).round() as usize + 1;
        let landings: Vec<f64> = (0..n_l)
            .map(|i| -window_hz + i as f64 * LANDING_STEP_HZ)
            .collect();
        let (mut b, mut d, mut s) = (Vec::new(), Vec::new(), Vec::new());
        let k_lo = -(folds as i64 / 2 - 1);
        for &l in &landings {
            // Fold 0 first, then k = k_lo..=folds/2 without 0: one f_xtal period.
            let ks = std::iter::once(0).chain((k_lo..=folds as i64 / 2).filter(|k| *k != 0));
            for k in ks {
                let nu = l + k as f64 * chain.rate;
                b.push(basis(nu));
                d.push(lin(chain.d_db(nu, envelope)));
                s.push(lin(analog.occupancy_db(nu)) + floors.n);
            }
        }
        let guard_at = b.len();
        let n_g = ((DESIGN_FS_HZ / 2.0 - GUARD_FROM_HZ) / TABLE_STEP_HZ).round() as usize;
        b.extend((0..=n_g).map(|i| basis(GUARD_FROM_HZ + i as f64 * TABLE_STEP_HZ)));
        Scene {
            landings,
            folds,
            basis: b,
            guard_at,
            chain: d,
            occupancy: s,
            q: floors.q * lin(q_shift_db),
            n: floors.n,
        }
    }

    fn responses(&self, c: &[i32; TAPS]) -> Vec<f64> {
        self.basis
            .iter()
            .map(|b| b.iter().zip(c).map(|(x, &v)| x * v as f64).sum())
            .collect()
    }

    /// dB over [`GUARD_DB`] summed over the guard points.
    fn guard_excess_db(&self, h: &[f64], h0: f64) -> f64 {
        h[self.guard_at..]
            .iter()
            .map(|x| (db((x / h0).powi(2)) - GUARD_DB).max(0.0))
            .sum()
    }
}

/// Per landing: the channel's gain G, the alias A (both re H(0)², D(0)),
/// the worst single alias, and the SINR.
struct Landing {
    hz: f64,
    gain: f64,
    alias: f64,
    worst: f64,
    sinr: f64,
    /// The SINR with the output [`Q_SENSITIVITY_DB`] under its target.
    sinr_low: f64,
}

/// The gain ahead of the output is set so the composite sits at
/// [`COMPOSITE_DBFS`]: the quantisation floor, re a channel, grows with
/// the output's mean power (channel, aliases and the ADC's floor, each
/// re a unity-gain channel), so a filter cannot buy SINR with gain.
fn landings(scene: &Scene, h: &[f64], h0: f64) -> Vec<Landing> {
    let f = scene.folds;
    let mut ls: Vec<Landing> = scene
        .landings
        .iter()
        .enumerate()
        .map(|(i, &hz)| {
            let o = i * f;
            let gain = (h[o] / h0).powi(2) * scene.chain[o];
            let (mut alias, mut worst) = (0.0, 0.0f64);
            let folds = o + 1..o + f;
            let terms = scene.occupancy[folds.clone()]
                .iter()
                .zip(&h[folds.clone()])
                .zip(&scene.chain[folds]);
            for ((occupancy, hj), chain) in terms {
                let a = occupancy * (hj / h0).powi(2) * chain;
                alias += a;
                worst = worst.max(a);
            }
            Landing {
                hz,
                gain,
                alias,
                worst,
                sinr: 0.0,
                sinr_low: 0.0,
            }
        })
        .collect();
    // The ADC's floor was measured through a DC-gain-4,096 set with the
    // FIR's ×8 set; at one output level a set of DC gain h0 at the
    // stream's FIR gain g lets the analogue gain rise by 4,096·8 / (h0·g),
    // so the signal at the ADC rises over the ADC's own noise by that,
    // squared (power).
    let n = scene.n * (h0 * stream_fir_gain() / (FINDINGS_10_DC_GAIN * RECORDED_FIR_GAIN)).powi(2);
    let output = ls.iter().map(|l| l.gain * (1.0 + n) + l.alias).sum::<f64>() / ls.len() as f64;
    for l in &mut ls {
        l.sinr = l.gain / (l.alias + n * l.gain + scene.q * output);
        l.sinr_low = l.gain / (l.alias + n * l.gain + scene.q * lin(Q_SENSITIVITY_DB) * output);
    }
    ls
}

#[derive(Clone, Copy, Debug)]
enum Objective {
    /// The mean SINR (dB) over the whole window.
    Balanced,
    /// The mean SINR over 0.6 ≤ |L| ≤ 1.18 MHz.
    Edge,
    /// The mean SINR over |L| ≤ 0.8 MHz.
    Centre,
    /// The worst single alias re the landing, inside ±u (a soft minimum):
    /// a tone at R − u is the case the bench's tone test reads.
    AliasOnly(f64),
    /// The mean over the window of the SINR at the target composite and
    /// with the output [`Q_SENSITIVITY_DB`] under it, averaged: a set that
    /// does not lose when the gain ahead of the output runs low.
    Hedged,
}

const EDGE_FROM_HZ: f64 = 0.6e6;
const CENTRE_TO_HZ: f64 = 0.8e6;

impl Objective {
    fn name(&self) -> String {
        match self {
            Objective::Balanced => "balanced: mean SINR over |L| <= 1.18 MHz".into(),
            Objective::Edge => "edge: mean SINR over 0.6 <= |L| <= 1.18 MHz".into(),
            Objective::Centre => "centre: mean SINR over |L| <= 0.8 MHz".into(),
            Objective::AliasOnly(u) => format!("alias-only: soft minimum of the worst single alias re the landing over |L| <= {:.2} MHz", u / 1e6),
            Objective::Hedged => "hedged: mean SINR over |L| <= 1.18 MHz, averaged over the output at its target and 10 dB under it".into(),
        }
    }

    /// Larger is better.
    fn score(&self, ls: &[Landing]) -> f64 {
        let mean = |f: &dyn Fn(f64) -> bool| {
            let v: Vec<f64> = ls
                .iter()
                .filter(|l| f(l.hz.abs()))
                .map(|l| db(l.sinr))
                .collect();
            v.iter().sum::<f64>() / v.len().max(1) as f64
        };
        match self {
            Objective::Balanced => mean(&|_| true),
            Objective::Edge => mean(&|x| x >= EDGE_FROM_HZ),
            Objective::Centre => mean(&|x| x <= CENTRE_TO_HZ),
            Objective::AliasOnly(u) => {
                let v: Vec<f64> = ls
                    .iter()
                    .filter(|l| l.hz.abs() <= *u)
                    .map(|l| (l.worst / l.gain).powf(SOFT_MIN_POWER))
                    .collect();
                -db((v.iter().sum::<f64>() / v.len().max(1) as f64).powf(1.0 / SOFT_MIN_POWER))
            }
            Objective::Hedged => {
                ls.iter()
                    .map(|l| (db(l.sinr) + db(l.sinr_low)) / 2.0)
                    .sum::<f64>()
                    / ls.len().max(1) as f64
            }
        }
    }
}

// ---- Seeds ----

/// I0, the modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

#[derive(Clone, Copy)]
enum Window {
    Hamming,
    Kaiser(f64),
}

/// A windowed sinc (taps.rs's construction, any window), scaled to the
/// DC gain, rounded, the rounding's remainder put on c15.
fn windowed_sinc(cutoff_hz: f64, w: Window) -> [i32; TAPS] {
    let x = 2.0 * cutoff_hz / DESIGN_FS_HZ;
    let centre = (FIR_LENGTH as f64 - 1.0) / 2.0;
    let half: Vec<f64> = (0..TAPS)
        .map(|n| {
            let t = PI * x * (centre - n as f64);
            let sinc = if t == 0.0 { 1.0 } else { t.sin() / t };
            let win = match w {
                Window::Hamming => {
                    HAMMING_A - HAMMING_B * (2.0 * PI * n as f64 / (FIR_LENGTH as f64 - 1.0)).cos()
                }
                Window::Kaiser(beta) => {
                    let r = (n as f64 - centre) / centre;
                    bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
                }
            };
            x * sinc * win
        })
        .collect();
    let sum: f64 = half.iter().sum();
    let mut c = [0i32; TAPS];
    for (k, h) in half.iter().enumerate() {
        c[k] = (h / sum * field_sum() as f64).round() as i32;
    }
    c[TAPS - 1] += field_sum() - c.iter().sum::<i32>();
    clamp(&mut c);
    c
}

/// Each field into its width (findings 10.3), the excess onto c15.
fn clamp(c: &mut [i32; TAPS]) {
    for (k, v) in c.iter_mut().enumerate() {
        let (lo, hi) = field_range(field_bits(k));
        *v = (*v).clamp(lo, hi);
    }
    c[TAPS - 1] += field_sum() - c.iter().sum::<i32>();
}

fn seeds() -> Vec<[i32; TAPS]> {
    // Ours at the DC gain held: the design itself, or its sinc rescaled.
    let ours = if field_sum() == DESIGN_DC_GAIN as i32 / 2 {
        DESIGN
    } else {
        windowed_sinc(taps::DESIGN_CUTOFF_HZ, Window::Hamming)
    };
    let mut v = vec![ours];
    for &fc in &SEED_CUTOFFS_HZ {
        v.push(windowed_sinc(fc, Window::Hamming));
        for &b in &KAISER_BETAS {
            v.push(windowed_sinc(fc, Window::Kaiser(b)));
        }
    }
    v
}

// ---- The search: integer pattern search over pair moves ----

fn fits(k: usize, v: i32) -> bool {
    let (lo, hi) = field_range(field_bits(k));
    v >= lo && v <= hi
}

fn evaluate(scene: &Scene, obj: Objective, h: &[f64], h0: f64) -> f64 {
    obj.score(&landings(scene, h, h0)) - GUARD_PENALTY_PER_DB * scene.guard_excess_db(h, h0)
}

fn search(scene: &Scene, obj: Objective, start: [i32; TAPS]) -> ([i32; TAPS], f64) {
    let mut c = start;
    let h0 = response(&c, 0.0);
    let mut h = scene.responses(&c);
    let mut best = evaluate(scene, obj, &h, h0);
    let mut trial = h.clone();
    for &s in &STEPS {
        loop {
            let mut found: Option<(usize, usize, f64)> = None;
            for a in 0..TAPS {
                for b in 0..TAPS {
                    if a == b || !fits(a, c[a] + s) || !fits(b, c[b] - s) {
                        continue;
                    }
                    for (i, t) in trial.iter_mut().enumerate() {
                        *t = h[i] + s as f64 * (scene.basis[i][a] - scene.basis[i][b]);
                    }
                    let v = evaluate(scene, obj, &trial, h0);
                    if v > found.map_or(best, |f| f.2) + 1e-9 {
                        found = Some((a, b, v));
                    }
                }
            }
            let Some((a, b, v)) = found else { break };
            c[a] += s;
            c[b] -= s;
            for (i, x) in h.iter_mut().enumerate() {
                *x += s as f64 * (scene.basis[i][a] - scene.basis[i][b]);
            }
            best = v;
        }
    }
    (c, best)
}

/// xorshift64: a deterministic spread for the hops.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const RNG_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

fn optimise(scene: &Scene, obj: Objective) -> ([i32; TAPS], f64) {
    let mut best: Option<([i32; TAPS], f64)> = None;
    for seed in seeds() {
        let r = search(scene, obj, seed);
        if best.is_none_or(|b| r.1 > b.1) {
            best = Some(r);
        }
    }
    let mut best = best.expect("seeds");
    let mut rng = Rng(RNG_SEED);
    for _ in 0..HOPS {
        let mut c = best.0;
        for _ in 0..HOP_MOVES {
            let a = rng.below(TAPS as u64) as usize;
            let b = rng.below(TAPS as u64) as usize;
            let s = 1 + rng.below(HOP_MAX_STEP as u64) as i32;
            if a != b && fits(a, c[a] + s) && fits(b, c[b] - s) {
                c[a] += s;
                c[b] -= s;
            }
        }
        let r = search(scene, obj, c);
        if r.1 > best.1 + 1e-6 {
            best = r;
        }
    }
    best
}

// ---- Metrics ----

struct Model {
    chain: Chain,
    /// The same at 2.048 Msps, the receiver's opening rate, where the
    /// mirror check also holds (validate (b)); 2.88 Msps fails it and is
    /// not modelled.
    chain_low: Chain,
    wide: Analog,
    narrow: Analog,
    floors: Floors,
    /// Ours at 2.4 Msps: the floor under the reference (findings 12.6).
    floor_under_ref_db: f64,
}

/// What filterfit reports, from the model: over tones every
/// [`TABLE_STEP_HZ`] in ±4 MHz beyond Nyquist, the worst landing inside
/// ±u re the centre (above the floor), and the spread over ±0.4 R.
/// INFINITY where every alias is under the floor.
fn step12_metrics(m: &Model, chain: &Chain, c: &[i32; TAPS]) -> (f64, f64, f64, f64) {
    let r = chain.rate;
    let level = |delta: f64| {
        chain.d_db(delta, false) + fir_db(c, delta) + m.wide.f_db((ddc_if_hz() - delta).abs())
    };
    let n = (4.0e6 / TABLE_STEP_HZ).round() as i64;
    let mut worst = [f64::INFINITY; 3];
    let insides = [0.7e6, 0.8e6, 1.0e6];
    for i in -n..=n {
        let d = i as f64 * TABLE_STEP_HZ;
        if d.abs() <= r / 2.0 || d > ddc_if_hz() {
            continue;
        }
        let rej = -level(d);
        if rej > m.floor_under_ref_db - FLOOR_MARGIN_DB {
            continue;
        }
        let land = fold(d, r).abs();
        for (w, u) in worst.iter_mut().zip(insides) {
            if land <= u {
                *w = w.min(rej);
            }
        }
    }
    let spread_span = 0.4 * r;
    let g: Vec<f64> = (-((spread_span / TABLE_STEP_HZ) as i64)
        ..=((spread_span / TABLE_STEP_HZ) as i64))
        .map(|i| level(i as f64 * TABLE_STEP_HZ))
        .collect();
    let spread =
        g.iter().cloned().fold(f64::MIN, f64::max) - g.iter().cloned().fold(f64::MAX, f64::min);
    (worst[0], worst[1], worst[2], spread)
}

/// A tone at δ: where it lands and how far under the channel there (and
/// under the centre); its mirror copy (ν = 2·IF − δ) likewise.
fn tone_json(m: &Model, c: &[i32; TAPS], delta: f64) -> String {
    let r = m.chain.rate;
    let g = |nu: f64| m.chain.d_db(nu, false) + fir_db(c, nu);
    let analog = m.wide.f_db((ddc_if_hz() - delta).abs());
    let land = fold(delta, r);
    let mirror = 2.0 * ddc_if_hz() - delta;
    let mland = fold(mirror, r);
    format!(
        "{{\"tone_mhz\":{:.2},\"lands_mhz\":{:.3},\"rej_re_landing_db\":{:.1},\"rej_re_centre_db\":{:.1},\"mirror_lands_mhz\":{:.3},\"mirror_rej_re_landing_db\":{:.1}}}",
        delta / 1e6,
        land / 1e6,
        g(land) - (g(delta) + analog),
        -(g(delta) + analog),
        mland / 1e6,
        g(mland) - (g(mirror) + analog)
    )
}

/// The FIR alone: its largest |H/H(0)| over a band, dB.
fn stopband_db(c: &[i32; TAPS], from: f64, to: f64) -> f64 {
    let n = ((to - from) / LANDING_STEP_HZ).round() as i64;
    (0..=n)
        .map(|i| fir_db(c, from + i as f64 * LANDING_STEP_HZ))
        .fold(f64::MIN, f64::max)
}

/// Bands over |L| the summary reports.
const BANDS_HZ: [(f64, f64); 4] = [
    (0.0, 0.3e6),
    (0.3e6, 0.6e6),
    (0.6e6, 0.9e6),
    (0.9e6, 1.18e6),
];
/// The stopbands the summary reports: the ÷4 stage's transition and its
/// reach, and the band the undecimated fold to f_symbol passes whole.
const STOPBANDS_HZ: [(f64, f64); 4] = [
    (1.6e6, 3.6e6),
    (3.6e6, 8.4e6),
    (8.4e6, 10.8e6),
    (10.8e6, 14.4e6),
];
/// Profile points (|L|, MHz).
const PROFILE_HZ: [f64; 12] = [
    0.0, 0.1e6, 0.2e6, 0.3e6, 0.4e6, 0.5e6, 0.6e6, 0.7e6, 0.8e6, 0.9e6, 1.0e6, 1.1e6,
];
const PERCENTILE: f64 = 0.10;

struct Summary {
    mean: f64,
    p10: f64,
    bands: Vec<f64>,
}

fn summary(ls: &[Landing]) -> Summary {
    let mut v: Vec<f64> = ls.iter().map(|l| db(l.sinr)).collect();
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    v.sort_by(f64::total_cmp);
    let p10 = v[(PERCENTILE * v.len() as f64) as usize];
    let bands = BANDS_HZ
        .iter()
        .map(|(a, b)| {
            let w: Vec<f64> = ls
                .iter()
                .filter(|l| l.hz.abs() >= *a && l.hz.abs() <= *b)
                .map(|l| db(l.sinr))
                .collect();
            w.iter().sum::<f64>() / w.len().max(1) as f64
        })
        .collect();
    Summary { mean, p10, bands }
}

/// A worst-alias figure for JSON: `"floor"` where every alias is under the
/// bench's floor (filterfit would report none).
fn floor_or(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.1}")
    } else {
        "\"floor\"".into()
    }
}

fn json_list(v: &[f64]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|x| format!("{x:.2}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn metrics_json(m: &Model, c: &[i32; TAPS]) -> String {
    let h0 = response(c, 0.0);
    let raw = Scene::new(&m.chain, &m.wide, &m.floors, WINDOW_HZ, false, 0.0);
    let ls = landings(&raw, &raw.responses(c), h0);
    let s = summary(&ls);
    let at = |hz: f64| {
        ls.iter()
            .min_by(|a, b| (a.hz.abs() - hz).abs().total_cmp(&(b.hz.abs() - hz).abs()))
            .expect("landings")
    };
    // Both signs averaged in dB at each |L|.
    let profile = |f: &dyn Fn(&Landing) -> f64| -> Vec<f64> {
        PROFILE_HZ
            .iter()
            .map(|&x| {
                let p: Vec<&Landing> = ls
                    .iter()
                    .filter(|l| (l.hz.abs() - x).abs() < LANDING_STEP_HZ / 2.0)
                    .collect();
                if p.is_empty() {
                    f(at(x))
                } else {
                    p.iter().map(|l| f(l)).sum::<f64>() / p.len() as f64
                }
            })
            .collect()
    };
    let gain = profile(&|l| db(l.gain));
    let fir_only: Vec<f64> = PROFILE_HZ.iter().map(|&x| fir_db(c, x)).collect();
    let alias = profile(&|l| db(l.alias / l.gain));
    let sinr = profile(&|l| db(l.sinr));
    let (w07, w08, w10, spread) = step12_metrics(m, &m.chain, c);
    let (l07, l08, _, l_spread) = step12_metrics(m, &m.chain_low, c);
    let low = Scene::new(
        &m.chain_low,
        &m.wide,
        &m.floors,
        LOW_RATE_HZ / 2.0 - EDGE_MARGIN_HZ,
        false,
        0.0,
    );
    let low = summary(&landings(&low, &low.responses(c), h0));
    let tones: Vec<String> = TONE_TEST_MHZ
        .iter()
        .map(|&t| tone_json(m, c, t * 1e6))
        .collect();
    let stops: Vec<String> = STOPBANDS_HZ
        .iter()
        .map(|(a, b)| {
            format!(
                "[{:.1},{:.1},{:.1}]",
                a / 1e6,
                b / 1e6,
                stopband_db(c, *a, *b)
            )
        })
        .collect();
    // Sensitivity: the same set under other assumptions.
    let other = |analog: &Analog, envelope: bool, q_shift: f64| {
        let sc = Scene::new(&m.chain, analog, &m.floors, WINDOW_HZ, envelope, q_shift);
        summary(&landings(&sc, &sc.responses(c), h0))
    };
    let narrow = other(&m.narrow, false, 0.0);
    let envelope = other(&m.wide, true, 0.0);
    let q_up = other(&m.wide, false, Q_SENSITIVITY_DB);
    let q_down = other(&m.wide, false, -Q_SENSITIVITY_DB);
    let bed = other(&m.wide.limited(BED_SPAN_HZ), false, 0.0);
    let bed_low = other(&m.wide.limited(BED_SPAN_HZ), false, Q_SENSITIVITY_DB);
    let spill = other(&m.wide.limited(NO_SPILL_SPAN_HZ), false, 0.0);
    let spill_low = other(&m.wide.limited(NO_SPILL_SPAN_HZ), false, Q_SENSITIVITY_DB);
    format!(
        "\"fields\":{},\"hex\":\"{}\",\"dc_gain\":{},\"modeled\":{{\"sinr_mean_db\":{:.2},\"sinr_p10_db\":{:.2},\"sinr_band_mean_db\":{{\"bands_mhz\":[[0,0.3],[0.3,0.6],[0.6,0.9],[0.9,1.18]],\"db\":{}}},\"profile_mhz\":{},\"gain_db\":{},\"fir_only_gain_db\":{},\"alias_re_channel_db\":{},\"sinr_db\":{},\"step12\":{{\"worst_alias_inside_0p7_db\":{},\"worst_alias_inside_0p8_db\":{},\"worst_alias_inside_1p0_db\":{},\"spread_0p96_db\":{:.1}}},\"tones\":[{}],\"fir_stopband_max_db\":[{}],\"at_2p048_msps\":{{\"sinr_mean_db\":{:.2},\"sinr_p10_db\":{:.2},\"worst_alias_inside_0p7_db\":{},\"worst_alias_inside_0p8_db\":{},\"spread_0p82_db\":{:.1}}},\"sensitivity\":{{\"narrow_if_filter\":{{\"mean\":{:.2},\"p10\":{:.2}}},\"chain_envelope\":{{\"mean\":{:.2},\"p10\":{:.2}}},\"q_plus_10db\":{{\"mean\":{:.2},\"p10\":{:.2}}},\"q_minus_10db\":{{\"mean\":{:.2},\"p10\":{:.2}}},\"ab_beds_2p4mhz\":{{\"mean\":{:.2},\"p10\":{:.2},\"band_mean_db\":{}}},\"ab_beds_2p4mhz_q_plus_10db\":{{\"mean\":{:.2},\"p10\":{:.2},\"band_mean_db\":{}}},\"no_spill_1p2mhz\":{{\"mean\":{:.2},\"p10\":{:.2},\"band_mean_db\":{}}},\"no_spill_1p2mhz_q_plus_10db\":{{\"mean\":{:.2},\"p10\":{:.2},\"band_mean_db\":{}}}}}}}",
        fields_json(c),
        hex(c),
        2 * c.iter().sum::<i32>(),
        s.mean,
        s.p10,
        json_list(&s.bands),
        json_list(&PROFILE_HZ.iter().map(|x| x / 1e6).collect::<Vec<_>>()),
        json_list(&gain),
        json_list(&fir_only),
        json_list(&alias),
        json_list(&sinr),
        floor_or(w07),
        floor_or(w08),
        floor_or(w10),
        spread,
        tones.join(","),
        stops.join(","),
        low.mean,
        low.p10,
        floor_or(l07),
        floor_or(l08),
        l_spread,
        narrow.mean,
        narrow.p10,
        envelope.mean,
        envelope.p10,
        q_up.mean,
        q_up.p10,
        q_down.mean,
        q_down.p10,
        bed.mean,
        bed.p10,
        json_list(&bed.bands),
        bed_low.mean,
        bed_low.p10,
        json_list(&bed_low.bands),
        spill.mean,
        spill.p10,
        json_list(&spill.bands),
        spill_low.mean,
        spill_low.p10,
        json_list(&spill_low.bands)
    )
}

/// The quantisation floor moved by this either way for the sensitivity rows.
const Q_SENSITIVITY_DB: f64 = 10.0;

fn model(rec: &Records) -> Result<Model, String> {
    let chain_sweep = rec
        .sweeps
        .iter()
        .find(|s| s.set == CHAIN_SET && s.rate == RATE_HZ)
        .ok_or("no c15-only sweep at the rate")?;
    let ours = rec
        .sweeps
        .iter()
        .filter(|s| s.set == "design" && s.rate == RATE_HZ)
        .map(|s| s.reference() - s.off_median_db)
        .collect::<Vec<_>>();
    let narrowest = FILT_BW_UPPER_3DB
        .iter()
        .min_by(|a, b| a.upper_3db_hz.total_cmp(&b.upper_3db_hz))
        .ok_or("no corners")?;
    let low_sweep = rec
        .sweeps
        .iter()
        .find(|s| s.set == CHAIN_SET && s.rate == LOW_RATE_HZ)
        .ok_or("no c15-only sweep at 2.048 Msps")?;
    Ok(Model {
        chain: Chain::from_sweep(chain_sweep),
        chain_low: Chain::from_sweep(low_sweep),
        wide: Analog::of(REFERENCE.upper_3db_hz, REFERENCE.db_2mhz_past_corner),
        narrow: Analog::of(narrowest.upper_3db_hz, narrowest.db_2mhz_past_corner),
        floors: floors(rec),
        floor_under_ref_db: ours.iter().sum::<f64>() / ours.len().max(1) as f64,
    })
}

// ---- Commands ----

fn cmd_chain(rec: &Records) -> Result<(), String> {
    for rate in [2.048e6, RATE_HZ, 2.88e6] {
        let Some(s) = rec
            .sweeps
            .iter()
            .find(|s| s.set == CHAIN_SET && s.rate == rate)
        else {
            continue;
        };
        let ch = Chain::from_sweep(s);
        for (i, (v, bound)) in ch.d4.iter().enumerate() {
            println!(
                "{{\"kind\":\"d4\",\"rate\":{rate},\"symbol_hz\":{},\"x_hz\":{},\"db\":{v:.2},\"bound\":{bound},\"envelope_db\":{:.2}}}",
                ch.symbol_hz,
                i as f64 * TABLE_STEP_HZ,
                ch.d4_envelope_db(i as f64 * TABLE_STEP_HZ)
            );
        }
    }
    let m = model(rec)?;
    let f = m.floors;
    println!(
        "{{\"kind\":\"floors\",\"rate\":{RATE_HZ},\"output_floor_dbfs_per_bin\":{:.2},\"adc_floor_dbfs_per_bin\":{:.2},\"composite_dbfs\":{COMPOSITE_DBFS},\"q_db_re_channel\":{:.2},\"n_db_re_channel\":{:.2},\"floor_under_ref_db\":{:.2}}}",
        f.output_db,
        f.adc_db,
        db(f.q),
        db(f.n),
        m.floor_under_ref_db
    );
    for (name, a) in [("wide", m.wide), ("narrow", m.narrow)] {
        let n = (DESIGN_FS_HZ / 2.0 / TABLE_STEP_HZ) as i64;
        for i in -n..=n {
            let nu = i as f64 * TABLE_STEP_HZ;
            if i % 4 == 0 {
                println!(
                    "{{\"kind\":\"occupancy\",\"if_filter\":\"{name}\",\"corner_hz\":{},\"nu_hz\":{nu},\"db\":{:.2}}}",
                    a.corner_hz,
                    a.occupancy_db(nu)
                );
            }
        }
    }
    Ok(())
}

struct Residuals {
    v: Vec<f64>,
}

impl Residuals {
    fn line(&self) -> String {
        if self.v.is_empty() {
            return "n 0".into();
        }
        let n = self.v.len() as f64;
        let mean = self.v.iter().sum::<f64>() / n;
        let rms = (self.v.iter().map(|x| x * x).sum::<f64>() / n).sqrt();
        let worst = self
            .v
            .iter()
            .cloned()
            .fold(0.0f64, |a, b| if b.abs() > a.abs() { b } else { a });
        format!(
            "n {} mean {mean:+.2} rms {rms:.2} worst {worst:+.1} dB",
            self.v.len()
        )
    }
}

/// The chain table of a sweep's rate (its own c15-only sweep).
fn chain_for(rec: &Records, rate: f64) -> Option<Chain> {
    rec.sweeps
        .iter()
        .find(|s| s.set == CHAIN_SET && s.rate == rate)
        .map(Chain::from_sweep)
}

/// (a) The direct path: every allowed sweep's tone where it lands, against
/// the c15-only sweep's chain at the same δ plus the set's FIR. (b) The
/// mirror path: the line at fold(2·IF − δ), against D4(fold(ν, f_symbol))
/// plus the set's FIR (the claim that the chain does not filter before
/// f_symbol). (c) filterfit's figures, model against record.
fn cmd_validate(rec: &Records) -> Result<(), String> {
    println!(
        "records read: {} sweeps; {} lines of sets outside the allow-list skipped unparsed",
        rec.sweeps.len(),
        rec.skipped
    );
    let m = model(rec)?;
    let analog = m.wide;
    println!(
        "\n(a) direct path: measured − model at the tone's landing (both ≥ {COMPARE_MARGIN_DB} dB over the floor)"
    );
    for s in &rec.sweeps {
        let Some(ch_sweep) = rec
            .sweeps
            .iter()
            .find(|c| c.set == CHAIN_SET && c.rate == s.rate)
        else {
            continue;
        };
        if s.set == CHAIN_SET || s.points.len() < 3 || s.file.contains("narrow-weight") {
            continue;
        }
        let ch_ref = ch_sweep.reference();
        let reference = s.reference();
        let (mut inband, mut beyond) = (Residuals { v: Vec::new() }, Residuals { v: Vec::new() });
        for p in &s.points {
            let Some(cp) = ch_sweep
                .points
                .iter()
                .find(|c| (c.delta - p.delta).abs() < 1.0)
            else {
                continue;
            };
            if cp.plain_db < cp.median_db + COMPARE_MARGIN_DB {
                continue;
            }
            let chain = cp.plain_db
                - ch_ref
                - (fir_db(&ch_sweep.fields, p.delta) - fir_db(&ch_sweep.fields, 0.0));
            let predicted = reference + chain + fir_db(&s.fields, p.delta);
            if p.plain_db < p.median_db + COMPARE_MARGIN_DB
                || predicted < p.median_db + COMPARE_MARGIN_DB
            {
                continue;
            }
            let r = p.plain_db - predicted;
            if p.delta.abs() < s.rate / 2.0 {
                inband.v.push(r)
            } else {
                beyond.v.push(r)
            }
        }
        println!(
            "  {} {:.3} Msps {}: in band {}; beyond Nyquist {}",
            s.file,
            s.rate / 1e6,
            short(&s.set),
            inband.line(),
            beyond.line()
        );
    }
    println!(
        "\n(b) mirror path (ν = 2·IF − δ, D = D4(fold(ν, f_symbol))): measured − model where a line is reported; bounds where none is"
    );
    for s in &rec.sweeps {
        let Some(ch) = chain_for(rec, s.rate) else {
            continue;
        };
        if s.points.len() < 3 || s.file.contains("narrow-weight") {
            continue;
        }
        let reference = s.reference();
        let mut seen = Residuals { v: Vec::new() };
        let (mut consistent, mut violated) = (0usize, Vec::new());
        for p in &s.points {
            // Only tones on the wanted side, where the IF filter is flat: a
            // tone above the LO's image point (δ > IF) is the tuner's image
            // (findings 15: ≥ 44 dB down), not this path.
            let g = ddc_if_hz() - p.delta;
            if g < 0.0 || analog.f_db(g) < 0.0 {
                continue;
            }
            let nu = 2.0 * ddc_if_hz() - p.delta;
            let land = fold(nu, s.rate);
            let tone = fold(p.delta, s.rate);
            let companion = fold(p.delta + SOURCE_COMPANION_HZ, s.rate);
            let near = |a: f64, b: f64| fold(a - b, s.rate).abs() < COINCIDENT_HZ;
            if near(land, tone) || (p.delta.abs() < s.rate / 2.0 && near(land, companion)) {
                continue;
            }
            let predicted = reference + ch.d_db(nu, false) + fir_db(&s.fields, nu);
            let line = p
                .lines
                .iter()
                .filter(|(f, _)| fold(f - land, s.rate).abs() < LINE_TOL_HZ)
                .map(|l| l.1)
                .fold(f64::NAN, f64::max);
            if line.is_finite() && !s.is_spur(land, line) {
                if predicted > p.median_db + FLOOR_MARGIN_DB {
                    seen.v.push(line - predicted);
                }
            } else {
                let weakest = p.lines.iter().map(|l| l.1).fold(f64::MAX, f64::min);
                if predicted <= weakest + FLOOR_MARGIN_DB {
                    consistent += 1;
                } else {
                    violated.push(format!(
                        "{:+.2}→{:+.3}:{:.1}>{:.1}",
                        p.delta / 1e6,
                        land / 1e6,
                        predicted,
                        weakest
                    ));
                }
            }
        }
        println!(
            "  {} {:.3} Msps {}: lines {}; none reported and model under the weakest reported line + {FLOOR_MARGIN_DB} dB: {consistent}; model above it: {} {}",
            s.file,
            s.rate / 1e6,
            short(&s.set),
            seen.line(),
            violated.len(),
            violated
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    println!(
        "\n(c) filterfit's figures at 2.4 Msps (worst alias re the centre inside ±0.7 / ±0.8 / ±1.0 MHz, spread over ±0.96 MHz): model | record"
    );
    for s in rec.sweeps.iter().filter(|s| {
        s.rate == RATE_HZ
            && s.set != CHAIN_SET
            && !s.file.contains("narrow-weight")
            && !s.file.contains("chain")
    }) {
        let (a, b, c, d) = step12_metrics(&m, &m.chain, &s.fields);
        let (ra, rb, rc, rd) = record_metrics(s);
        println!(
            "  {} {}: model {a:.1} / {b:.1} / {c:.1} / {d:.1} | record {ra:.1} / {rb:.1} / {rc:.1} / {rd:.1}",
            s.file,
            short(&s.set)
        );
    }
    println!(
        "\n(d) the 2026-10-12 tone test, ours (model; the bench read +1.6→−0.8: 43–44, +2.0→−0.4: 52–59, +1.3→−1.1: ~16, −1.6→+0.8: 37–42 dB under the in-band reference)"
    );
    for t in TONE_TEST_MHZ {
        println!("  {}", tone_json(&m, &DESIGN, t * 1e6));
    }
    Ok(())
}

/// filterfit's figures from a record (plain landings at 2.4 Msps, where
/// plain and staged coincide), the floor rule as filterfit's.
fn record_metrics(s: &Sweep) -> (f64, f64, f64, f64) {
    let mut refs: Vec<f64> = s
        .points
        .iter()
        .filter(|p| p.delta.abs() <= REF_SPAN_HZ)
        .map(|p| p.plain_db)
        .collect();
    refs.sort_by(f64::total_cmp);
    let reference = refs[refs.len() / 2];
    let mut worst = [f64::INFINITY; 3];
    for p in s.points.iter().filter(|p| p.delta.abs() > s.rate / 2.0) {
        let land = fold(p.delta, s.rate).abs();
        if p.plain_db < p.median_db + FLOOR_MARGIN_DB
            || s.is_spur(fold(p.delta, s.rate), p.plain_db)
        {
            continue;
        }
        for (w, u) in worst.iter_mut().zip([0.7e6, 0.8e6, 1.0e6]) {
            if land <= u {
                *w = w.min(reference - p.plain_db);
            }
        }
    }
    let g: Vec<f64> = s
        .points
        .iter()
        .filter(|p| p.delta.abs() <= 0.4 * s.rate)
        .map(|p| p.plain_db - reference)
        .collect();
    let spread =
        g.iter().cloned().fold(f64::MIN, f64::max) - g.iter().cloned().fold(f64::MAX, f64::min);
    (worst[0], worst[1], worst[2], spread)
}

fn short(set: &str) -> String {
    if set.starts_with("hex:") {
        "power-on".into()
    } else {
        set.chars().take(40).collect()
    }
}

/// The band a candidate is designed for.
enum Band {
    Dense,
    Beds,
    NoSpill,
}

fn cmd_design(rec: &Records) -> Result<(), String> {
    let m = model(rec)?;
    let dense = Scene::new(&m.chain, &m.wide, &m.floors, WINDOW_HZ, true, 0.0);
    let beds = Scene::new(
        &m.chain,
        &m.wide.limited(BED_SPAN_HZ),
        &m.floors,
        WINDOW_HZ,
        true,
        0.0,
    );
    let no_spill = Scene::new(
        &m.chain,
        &m.wide.limited(NO_SPILL_SPAN_HZ),
        &m.floors,
        WINDOW_HZ,
        true,
        0.0,
    );
    println!(
        "{{\"kind\":\"ours\",\"name\":\"ours\",{}}}",
        metrics_json(&m, &DESIGN)
    );
    // (name, objective, the band)
    let runs = [
        ("B1", Objective::Balanced, Band::Dense),
        ("E1", Objective::Edge, Band::Dense),
        ("M1", Objective::Centre, Band::Dense),
        ("A08", Objective::AliasOnly(0.8e6), Band::Dense),
        ("A10", Objective::AliasOnly(1.0e6), Band::Dense),
        ("H1", Objective::Hedged, Band::Dense),
        ("AB1", Objective::Hedged, Band::Beds),
        ("F1", Objective::Hedged, Band::NoSpill),
    ];
    for (name, obj, b) in runs {
        let (scene, band) = match b {
            Band::Dense => (&dense, " (dense band)"),
            Band::Beds => (&beds, " (the A/B beds: carriers to +-2.4 MHz)"),
            Band::NoSpill => (&no_spill, " (nothing beyond +-1.2 MHz)"),
        };
        let (c, score) = optimise(scene, obj);
        let ours = evaluate(scene, obj, &scene.responses(&DESIGN), DESIGN_DC_GAIN);
        eprintln!("{name}: {}{band} → {score:.2} (ours {ours:.2})", obj.name());
        println!(
            "{{\"kind\":\"candidate\",\"name\":\"{name}\",\"objective\":\"{}{band}\",\"objective_value\":{score:.3},\"objective_value_ours\":{ours:.3},{}}}",
            obj.name(),
            metrics_json(&m, &c)
        );
    }
    Ok(())
}

/// The span `predict` covers: filterfit's sweeps (12.7, ±6 MHz).
const PREDICT_SPAN_HZ: f64 = 6.0e6;

/// One line per tone every [`TABLE_STEP_HZ`] over ±[`PREDICT_SPAN_HZ`]:
/// where it lands and its level re the centre's reference (what a
/// `filterstudy` sweep's `plain_dbfs` less its reference should read),
/// and the same for its mirror copy, for the bench to hold against.
/// Tones above the IF are the tuner's image (findings 15) and have no
/// direct line.
fn cmd_predict(rec: &Records, list: &str) -> Result<(), String> {
    let c = parse_fields(list)?;
    let m = model(rec)?;
    let r = m.chain.rate;
    let level = |nu: f64, delta: f64| {
        m.chain.d_db(nu, false) + fir_db(&c, nu) + m.wide.f_db((ddc_if_hz() - delta).abs())
    };
    let n = (PREDICT_SPAN_HZ / TABLE_STEP_HZ).round() as i64;
    for i in -n..=n {
        let d = i as f64 * TABLE_STEP_HZ;
        let mirror = 2.0 * ddc_if_hz() - d;
        let direct = if d < ddc_if_hz() {
            format!("{:.1}", level(d, d))
        } else {
            "null".into()
        };
        println!(
            "{{\"kind\":\"predict\",\"rate\":{r},\"fields\":{},\"delta\":{d},\"lands_hz\":{:.0},\"db_re_ref\":{direct},\"mirror_lands_hz\":{:.0},\"mirror_db_re_ref\":{:.1}}}",
            fields_json(&c),
            fold(d, r),
            fold(mirror, r),
            level(mirror, d)
        );
    }
    Ok(())
}

fn parse_fields(list: &str) -> Result<[i32; TAPS], String> {
    let v: Vec<i32> = list
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<i32>()
                .map_err(|_| format!("bad field {s}"))
        })
        .collect::<Result<_, _>>()?;
    let c: [i32; TAPS] = v.try_into().map_err(|_| format!("need {TAPS} fields"))?;
    taps::pack(&c).map_err(|e| e.to_string())?;
    Ok(c)
}

fn cmd_score(rec: &Records, list: &str) -> Result<(), String> {
    let c = parse_fields(list)?;
    let m = model(rec)?;
    println!("{{\"kind\":\"score\",{}}}", metrics_json(&m, &c));
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("derivation")
        .join("data");
    if let Some(i) = args.iter().position(|a| a == "--data") {
        dir = PathBuf::from(args.get(i + 1).ok_or("--data DIR")?);
    }
    if let Some(i) = args.iter().position(|a| a == "--dc-gain") {
        let g: i32 = args
            .get(i + 1)
            .and_then(|v| v.parse().ok())
            .ok_or("--dc-gain N (even)")?;
        DC_GAIN.set(g).map_err(|_| "--dc-gain set twice")?;
    }
    let rec = load(&dir)?;
    match args.first().map(String::as_str) {
        Some("chain") => cmd_chain(&rec),
        Some("validate") => cmd_validate(&rec),
        Some("design") => cmd_design(&rec),
        Some("score") => cmd_score(&rec, args.get(1).ok_or("score c0,c1,...,c15")?),
        Some("predict") => cmd_predict(&rec, args.get(1).ok_or("predict c0,c1,...,c15")?),
        _ => Err("firdesign chain | validate | design | score c0,...,c15 | predict c0,...,c15 [--data DIR]".into()),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("firdesign: {e}");
        std::process::exit(1);
    }
}
