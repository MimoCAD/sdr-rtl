//! The IF filter tables from data/step4-filter.jsonl (findings.md step
//! 4g): the upper -3 dB corner per FILT_CODE and per FILT_BW, and the
//! HPF codes' (absence of) effect.

use crate::emit::{self, Src, f};
use crate::records::{Filter, FilterCurve};
use crate::scale::MeterScale;

/// A curve's passband level is the median of its readings with the
/// tone's IF in this span: above the roll-off below 0.5 MHz that every
/// curve shares, below the narrowest corner measured (7.3 MHz).
pub const PASSBAND_LEVEL_SPAN_HZ: (f64, f64) = (1.0e6, 6.0e6);
/// The corner: where the curve is this far below its passband level.
pub const CORNER_DB: f64 = -3.0;
/// Two consecutive tone steps must be past the corner before it counts
/// (single points alternate between two loop states about 1.7 dB
/// apart, findings.md step 4g).
pub const CORNER_CONFIRM_POINTS: usize = 2;
/// Every curve is smoothed by a running median over this many tone
/// steps before any reading is taken from it: a single step that
/// caught the loop in its other state (the reference curve's 8.0 MHz
/// point reads 3.7 dB off its neighbours) is replaced by its
/// neighbours' value; two steps in a row are kept.
pub const MEDIAN_WINDOW: usize = 3;
/// The rejection is read this far past the corner.
pub const PAST_CORNER_HZ: f64 = 2.0e6;
/// The HPF codes are compared with the reference from the lowest tone
/// IF up to the end of the passband-level span: a high-pass corner, if
/// any, shows at the low end, and on the low-pass corner's slope a
/// 0.1 MHz shift already reads as 2 dB. An HPF code has a measurable
/// effect when it departs from the reference by more than the
/// reference's own two-state alternation (`two_state_db`: the spread of
/// the smoothed reference inside the passband-level span) anywhere in
/// that span.
pub const HPF_COMPARE_MAX_HZ: f64 = PASSBAND_LEVEL_SPAN_HZ.1;
/// The record's stage names.
pub const STAGE_REF: &str = "ref";
pub const STAGE_FILT_CODE: &str = "filt_code";
pub const STAGE_FILT_BW: &str = "filt_bw";
pub const STAGE_HPF: &str = "hpf";
pub const FILT_CODES: usize = 16;
pub const FILT_BWS: usize = 4;
pub const HPF_CODES: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Corner {
    pub code: u8,
    pub upper_3db_hz: f64,
    pub db_past_corner: f64,
    pub passband_counts: f64,
}

pub struct FilterTables {
    pub filt_code: Vec<Corner>,
    pub filt_bw: Vec<Corner>,
    pub reference: Corner,
    /// Per HPF code: the largest |deviation| from the reference curve.
    pub hpf: Vec<(u8, f64)>,
    pub hpf_max_db: f64,
    /// The reference's own two-state alternation, dB.
    pub two_state_db: f64,
    /// The FILT_CODE stage's code 0 against the reference curve.
    pub repeat_max_db: f64,
    pub points_per_curve: usize,
    pub if_span_hz: (f64, f64),
}

fn passband_level(c: &FilterCurve) -> f64 {
    let v: Vec<f64> = c.points.iter().filter(|p| p.0 >= PASSBAND_LEVEL_SPAN_HZ.0 && p.0 <= PASSBAND_LEVEL_SPAN_HZ.1).map(|p| p.1 as f64).collect();
    emit::median(&v)
}

fn corner(c: &FilterCurve, code: u8, scale: &MeterScale) -> Result<Corner, String> {
    let level = passband_level(c);
    // dB relative to the passband level at every point (negative = less signal).
    let rel: Vec<(f64, f64)> = c.points.iter().map(|p| (p.0, (level - p.1 as f64) / scale.counts_per_db)).collect();
    let mut found = None;
    for i in 0..rel.len() {
        if rel[i].0 <= PASSBAND_LEVEL_SPAN_HZ.1 {
            continue;
        }
        let past = (i..i + CORNER_CONFIRM_POINTS).all(|k| rel.get(k).is_some_and(|p| p.1 <= CORNER_DB));
        if past {
            if i == 0 {
                return Err(format!("{} code {code}: the first point is already past the corner", c.stage));
            }
            let (f0, d0) = rel[i - 1];
            let (f1, d1) = rel[i];
            let hz = if d1 == d0 { f1 } else { f0 + (f1 - f0) * (CORNER_DB - d0) / (d1 - d0) };
            found = Some(hz);
            break;
        }
    }
    let upper_3db_hz = found.ok_or_else(|| format!("{} code {code}: no -3 dB corner within the swept IF span", c.stage))?;
    let target = upper_3db_hz + PAST_CORNER_HZ;
    let near = rel.iter().min_by(|a, b| (a.0 - target).abs().partial_cmp(&(b.0 - target).abs()).unwrap_or(std::cmp::Ordering::Equal)).ok_or("empty curve")?;
    Ok(Corner { code, upper_3db_hz: emit::round(upper_3db_hz, -4), db_past_corner: emit::round(near.1, 1), passband_counts: level })
}

/// The largest |dB| between two curves, point by point at equal IFs up
/// to `max_hz`.
fn max_deviation(a: &FilterCurve, b: &FilterCurve, max_hz: f64, scale: &MeterScale) -> Result<f64, String> {
    if a.points.len() != b.points.len() {
        return Err(format!("{} and {} have different point counts", a.stage, b.stage));
    }
    let mut mx: f64 = 0.0;
    for (p, q) in a.points.iter().zip(&b.points) {
        if p.0 != q.0 {
            return Err(format!("{} and {} are sampled at different IFs", a.stage, b.stage));
        }
        if p.0 <= max_hz {
            mx = mx.max(scale.db_more(p.1, q.1).abs());
        }
    }
    Ok(emit::round(mx, 1))
}

/// The reference's two-state alternation: the spread of its smoothed
/// readings inside the passband-level span, in dB.
fn two_state_db(reference: &FilterCurve, scale: &MeterScale) -> f64 {
    let v: Vec<i64> = reference.points.iter().filter(|p| p.0 >= PASSBAND_LEVEL_SPAN_HZ.0 && p.0 <= PASSBAND_LEVEL_SPAN_HZ.1).map(|p| p.1).collect();
    match (v.iter().min(), v.iter().max()) {
        (Some(&lo), Some(&hi)) => emit::round(scale.db_more(hi, lo), 1),
        _ => 0.0,
    }
}

fn curve<'a>(curves: &'a [FilterCurve], file: &str, stage: &str, code: u8, bw: u8, hpf: u8) -> Result<&'a FilterCurve, String> {
    curves.iter().find(|c| c.stage == stage && c.filt_code == code && c.filt_bw == bw && c.hpf == hpf).ok_or_else(|| format!("{file}: no curve {stage} code {code} bw {bw} hpf {hpf}"))
}

/// The curve with every reading replaced by the median of the
/// `MEDIAN_WINDOW` readings centred on it (the ends keep the window
/// clipped to the curve).
fn smooth(c: &FilterCurve) -> FilterCurve {
    let half = MEDIAN_WINDOW / 2;
    let n = c.points.len();
    let points = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(n);
            let window: Vec<f64> = c.points[lo..hi].iter().map(|p| p.1 as f64).collect();
            (c.points[i].0, emit::median(&window).round() as i64)
        })
        .collect();
    FilterCurve { stage: c.stage.clone(), filt_code: c.filt_code, filt_bw: c.filt_bw, hpf: c.hpf, points }
}

pub fn analyse(rec: &Filter, scale: &MeterScale) -> Result<FilterTables, String> {
    let curves: Vec<FilterCurve> = rec.curves.iter().map(smooth).collect();
    let file = rec.provenance.file.as_str();
    // The reference's own codes: FILT_CODE 0, FILT_BW 00, HPF 1111 (the
    // record's candidate space).
    let r = curves.iter().find(|c| c.stage == STAGE_REF).ok_or("filter: no reference curve")?;
    let (ref_code, ref_bw, ref_hpf) = (r.filt_code, r.filt_bw, r.hpf);
    let points_per_curve = r.points.len();
    if curves.iter().any(|c| c.points.len() != points_per_curve) {
        return Err("filter: curves of unequal length".into());
    }
    let reference = corner(r, ref_code, scale)?;
    let mut filt_code = Vec::new();
    for code in 0..FILT_CODES as u8 {
        filt_code.push(corner(curve(&curves, file, STAGE_FILT_CODE, code, ref_bw, ref_hpf)?, code, scale)?);
    }
    let mut filt_bw = Vec::new();
    for bw in 0..FILT_BWS as u8 {
        filt_bw.push(corner(curve(&curves, file, STAGE_FILT_BW, ref_code, bw, ref_hpf)?, bw, scale)?);
    }
    let mut hpf = Vec::new();
    for h in 0..HPF_CODES as u8 {
        hpf.push((h, max_deviation(r, curve(&curves, file, STAGE_HPF, ref_code, ref_bw, h)?, HPF_COMPARE_MAX_HZ, scale)?));
    }
    let hpf_max_db = hpf.iter().map(|h| h.1).fold(0.0, f64::max);
    let two_state_db = two_state_db(r, scale);
    let repeat_max_db = max_deviation(r, curve(&curves, file, STAGE_FILT_CODE, ref_code, ref_bw, ref_hpf)?, HPF_COMPARE_MAX_HZ, scale)?;
    let if_span_hz = (r.points[0].0, r.points[points_per_curve - 1].0);
    Ok(FilterTables { filt_code, filt_bw, reference, hpf, hpf_max_db, two_state_db, repeat_max_db, points_per_curve, if_span_hz })
}

pub fn emit(t: &FilterTables, rec: &Filter, scale: &MeterScale) -> String {
    let mut s = Src::new(&[&rec.provenance, &scale.provenance]);
    s.module_doc(&format!(
        "The tuner's IF filter: the upper -3 dB corner per FILT_CODE (R10[3:0])
and per FILT_BW (R11[6:5]), and the HPF codes (R11[3:0]).

Method (findings.md step 4g): the LO fixed, the tone stepped {:.1}-{:.1} MHz
below it in {:.0} kHz steps ({} points per curve), `if_agc_val`
(MSB-first) read in the vagc configuration, the source fixed after one
re-level; the demodulator's DDC {} so that its own channel filter does
not shape the curve. Every curve is first smoothed by a running median
over {} tone steps (a single step caught in the loop's other state is
replaced by its neighbours'). A curve's passband level is the median of
its readings between {:.1} and {:.1} MHz; its corner is where it first falls
{:.0} dB below that level for {} consecutive steps, interpolated between
the two steps either side; counts become dB over the loop meter's
{:.1} counts per dB.

Resolution: the tone step ({:.0} kHz) and the loop's two-state
alternation (about 1.7 dB, which moves a corner by up to one step).

Caveats: the curve still carries the demodulator's response at the
tone (the reference curve, the widest codes, is 3 dB down at
{:.2} MHz: the tuner's widest setting or the demodulator's edge, not
told apart); single-snapshot readings; the corners are of the IF
filter alone at one LO (300 MHz + IF, /8). The FILT_CODE stage's code 0
repeated the reference to within {:.1} dB over the passband compared for
the HPF codes.",
        t.if_span_hz.0 / 1e6,
        t.if_span_hz.1 / 1e6,
        rec.if_step_hz / 1e3,
        t.points_per_curve,
        if rec.ddc_track { "tracked the tone (pset_iffreq 1 MHz below it)" } else { "stayed fixed" },
        MEDIAN_WINDOW,
        PASSBAND_LEVEL_SPAN_HZ.0 / 1e6,
        PASSBAND_LEVEL_SPAN_HZ.1 / 1e6,
        CORNER_DB,
        CORNER_CONFIRM_POINTS,
        scale.counts_per_db,
        rec.if_step_hz / 1e3,
        t.reference.upper_3db_hz / 1e6,
        t.repeat_max_db
    ));
    s.blank();
    s.line("use super::types::*;");
    s.blank();
    s.doc("The reference curve (FILT_CODE 0000, FILT_BW 00, HPF 1111): its corner
and rejection by the same method, for comparison.");
    s.line(&format!(
        "pub const REFERENCE: FiltCorner = FiltCorner {{ code: {}, upper_3db_hz: {}, db_2mhz_past_corner: {} }};",
        t.reference.code,
        f(t.reference.upper_3db_hz, 1),
        f(t.reference.db_past_corner, 1)
    ));
    s.blank();
    s.doc("FILT_CODE 0000 (widest) ..= 1111 (narrowest) with FILT_BW 00 and HPF
1111: the upper -3 dB corner, and the curve 2 MHz past it.");
    s.line(&format!("pub const FILT_CODE_UPPER_3DB: [FiltCorner; {}] = [", t.filt_code.len()));
    for c in &t.filt_code {
        s.line(&format!("    FiltCorner {{ code: 0b{:04b}, upper_3db_hz: {}, db_2mhz_past_corner: {} }},", c.code, f(c.upper_3db_hz, 1), f(c.db_past_corner, 1)));
    }
    s.line("];");
    s.blank();
    s.doc("FILT_BW 00 (widest), 01 and 10 (middle), 11 (narrowest) with FILT_CODE
0000 and HPF 1111.");
    s.line(&format!("pub const FILT_BW_UPPER_3DB: [FiltCorner; {}] = [", t.filt_bw.len()));
    for c in &t.filt_bw {
        s.line(&format!("    FiltCorner {{ code: 0b{:02b}, upper_3db_hz: {}, db_2mhz_past_corner: {} }},", c.code, f(c.upper_3db_hz, 1), f(c.db_past_corner, 1)));
    }
    s.line("];");
    s.blank();
    let no_effect = t.hpf_max_db <= t.two_state_db;
    s.doc(&format!(
        "HPF 0000 (highest corner) ..= 1111 (lowest) with FILT_CODE 0000 and
FILT_BW 00: each code's largest departure from the reference curve at
any tone step from {:.1} to {:.1} MHz (a high-pass corner would show at the
low end; on the low-pass corner's slope a 0.1 MHz shift already reads
as 2 dB, so the slope is left out). The largest of all is {:.1} dB, {}
the reference's own two-state alternation of {:.1} dB (the spread of its
smoothed readings between {:.1} and {:.1} MHz): {}. The roll-off below
0.5 MHz is common to every code, so it is not the tuner's HPF.",
        t.if_span_hz.0 / 1e6,
        HPF_COMPARE_MAX_HZ / 1e6,
        t.hpf_max_db,
        if no_effect { "within" } else { "above" },
        t.two_state_db,
        PASSBAND_LEVEL_SPAN_HZ.0 / 1e6,
        PASSBAND_LEVEL_SPAN_HZ.1 / 1e6,
        if no_effect { "no HPF code has a measurable effect above 0.1 MHz in this configuration" } else { "some HPF code departs from the reference by more than its alternation" }
    ));
    s.line(&format!("pub const HPF_EFFECT: [HpfEffect; {}] = [", t.hpf.len()));
    for (code, dev) in &t.hpf {
        s.line(&format!("    HpfEffect {{ code: 0b{code:04b}, max_deviation_from_reference_db: {} }},", f(*dev, 1)));
    }
    s.line("];");
    s.doc("The reference curve's two-state alternation, the threshold above.");
    s.line(&format!("pub const REFERENCE_TWO_STATE_DB: f64 = {};", f(t.two_state_db, 1)));
    s.line(&format!("pub const HPF_HAS_NO_MEASURABLE_EFFECT: bool = {no_effect};"));
    s.finish()
}
