//! The gain tables: dB per code for LNA_GAIN (R5[3:0]), MIX_GAIN
//! (R7[3:0]) and VGA_CODE (R12[3:0]) from the comparator record
//! (data/step4-gain-ladder-comparator.json, findings.md step 4j), with
//! the earlier loop-meter record (data/step4-gain-ladder.json, step 4i)
//! as a second column.

use crate::emit::{self, Src, f, opt_f};
use crate::records::{GainLadder, GainStage};
use crate::scale::MeterScale;

/// The three stages, in record order, with the register each sets and
/// the name of its generated table.
pub const STAGES: [(&str, u8, &str); 3] = [("LNA_GAIN", 5, "LNA_GAIN_DB"), ("MIX_GAIN", 7, "MIX_GAIN_DB"), ("VGA_CODE", 12, "VGA_GAIN_DB")];
pub const CODES: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub code: u8,
    pub comparator_db: Option<i16>,
    pub comparator_at_least_db: Option<i16>,
    pub flip_level_db: Option<i16>,
    pub loop_meter_db: Option<f64>,
    pub comparator_run1_db: Option<i16>,
}

pub struct GainTables {
    /// Per stage: the rows, and the loop record's method for that stage.
    pub stages: Vec<(String, u8, Vec<Row>, String)>,
}

fn stage(rec: &GainLadder, register: u8) -> Result<&GainStage, String> {
    let s = rec.stages.iter().find(|s| s.register == register).ok_or_else(|| format!("{}: no stage for R{register}", rec.provenance.file))?;
    if s.codes.len() != CODES || s.codes.iter().enumerate().any(|(i, c)| c.code as usize != i) {
        return Err(format!("{}: R{register} does not hold codes 0..={}", rec.provenance.file, CODES - 1));
    }
    Ok(s)
}

/// One code's comparator result: (gain dB, lower bound dB, flip level dB).
type ComparatorCell = (Option<i16>, Option<i16>, Option<i16>);

/// Comparator gains relative to code 0: minus the flip-level difference.
/// A code whose search stopped below the source's floor gets a lower
/// bound from that floor.
fn comparator_column(s: &GainStage) -> Result<Vec<ComparatorCell>, String> {
    let flip0 = s.codes[0].threshold_level_db.ok_or_else(|| format!("{}: code 0 has no flip level", s.name))?;
    let mut out = Vec::new();
    for c in &s.codes {
        match c.threshold_level_db {
            Some(t) => out.push((Some((flip0 - t) as i16), None, Some(t as i16))),
            None => {
                let stop = c.stop.as_deref().unwrap_or("");
                let (lo, _) = c.probed_levels_db.ok_or_else(|| format!("{}: code {} has neither a flip level nor probed levels", s.name, c.code))?;
                if !stop.starts_with("already above the target at the lowest level") {
                    return Err(format!("{}: code {} stopped for a reason this generator does not read: {stop}", s.name, c.code));
                }
                // The flip is below `lo`, so the gain is more than flip0 - lo.
                out.push((None, Some((flip0 - lo) as i16), None));
            }
        }
    }
    Ok(out)
}

/// Loop-meter gains relative to code 0: (meter difference) / slope +
/// (source difference) (METHODS.md §8; the record's procedure line).
fn loop_column(s: &GainStage, scale: &MeterScale) -> Result<Vec<f64>, String> {
    let m0 = s.codes[0].meter_value.ok_or_else(|| format!("{}: code 0 has no meter value", s.name))?;
    let l0 = s.codes[0].source_level_db.ok_or_else(|| format!("{}: code 0 has no source level", s.name))?;
    let mut out = Vec::new();
    for c in &s.codes {
        let m = c.meter_value.ok_or_else(|| format!("{}: code {} has no meter value", s.name, c.code))?;
        let l = c.source_level_db.ok_or_else(|| format!("{}: code {} has no source level", s.name, c.code))?;
        out.push(emit::round(scale.db_more(m0, m) + (l0 - l) as f64, 1));
    }
    Ok(out)
}

pub fn analyse(comparator: &GainLadder, loop_rec: &GainLadder, scale: &MeterScale) -> Result<GainTables, String> {
    if comparator.config != "manual" {
        return Err(format!("{}: configuration {} is not the comparator's (manual)", comparator.provenance.file, comparator.config));
    }
    if loop_rec.config != "vagc" {
        return Err(format!("{}: configuration {} is not the loop meter's (vagc)", loop_rec.provenance.file, loop_rec.config));
    }
    let mut stages = Vec::new();
    for (name, register, _) in STAGES {
        let cs = stage(comparator, register)?;
        if !cs.comparator {
            return Err(format!("{}: {name} was not measured by the comparator", comparator.provenance.file));
        }
        let comp = comparator_column(cs)?;
        let ls = stage(loop_rec, register)?;
        let (loop_db, run1): (Vec<Option<f64>>, Vec<Option<i16>>) = if ls.comparator {
            let c = comparator_column(ls)?;
            (vec![None; CODES], c.into_iter().map(|(g, _, _)| g).collect())
        } else {
            (loop_column(ls, scale)?.into_iter().map(Some).collect(), vec![None; CODES])
        };
        let rows = (0..CODES)
            .map(|i| Row { code: i as u8, comparator_db: comp[i].0, comparator_at_least_db: comp[i].1, flip_level_db: comp[i].2, loop_meter_db: loop_db[i], comparator_run1_db: run1[i] })
            .collect();
        stages.push((name.to_string(), register, rows, ls.method.clone()));
    }
    Ok(GainTables { stages })
}

pub fn emit(t: &GainTables, comparator: &GainLadder, loop_rec: &GainLadder, scale: &MeterScale) -> String {
    let mut s = Src::new(&[&comparator.provenance, &loop_rec.provenance, &scale.provenance]);
    s.module_doc(&format!(
        "Gain per code for the three manual gain stages, relative to code 0 of
each stage, at {:.0} MHz with the other two stages at codes LNA {}, MIX
{}, VGA {}.

Method (findings.md step 4j, the first column): the comparator. With
VGA_MODE = 0 the demodulator's IF AGC loop drives nothing and
`if_agc_val` (MSB-first) rails at +8191 below the loop's target and at
-8192 above it; the source's relative level was bisected in 1 dB steps
to the flip, and a code's gain relative to code 0 is minus the
difference of flip levels. Resolution 1 dB (the bisection step; levels
above 0 dB rest on the source's nominal 1 dB txvga steps, levels at or
below 0 dB on its exact digital backoff).

The second column (findings.md step 4i): the LNA and the mixer through
the closed loop (VGA_MODE = 1), the source re-levelled to keep the
meter in its window, gain = (meter difference) / {:.1} counts per dB +
(source difference); the slope is the least-squares fit of the meter
step's vagc ladder ({} points, largest residual {:.0} counts: the txvga
steps are nominal). The VGA cannot be read through the loop it sits
in, so its second column is that record's own comparator run.

Caveats: the loop meter alternates between two states about 1.7 dB
apart, so its column can step slightly negative; the mixer's codes
12-15 are flat within the 1 dB resolution; VGA code 15's flip lay below
the source's floor, so only a lower bound is known for it.",
        comparator.freq_hz / 1e6,
        comparator.fixed_codes.0,
        comparator.fixed_codes.1,
        comparator.fixed_codes.2,
        scale.counts_per_db,
        scale.points,
        scale.max_residual_counts
    ));
    s.blank();
    s.line("use super::types::*;");
    s.blank();
    s.doc("The loop meter's scale, fitted from the meter step's vagc ladder:
`if_agc_val` (MSB-first) falls by this many counts per dB of signal.");
    s.line(&format!("pub const LOOP_METER_COUNTS_PER_DB: f64 = {};", f(scale.counts_per_db, 1)));
    s.doc("The comparator's resolution: the bisection step.");
    s.line("pub const COMPARATOR_RESOLUTION_DB: i16 = 1;");
    s.blank();
    for (name, register, rows, loop_method) in &t.stages {
        let table = STAGES.iter().find(|(n, _, _)| n == name).map(|(_, _, t)| *t).unwrap_or(name);
        s.doc(&format!("{name} (R{register}[3:0]), dB relative to code 0. Second record's method for
this stage: {loop_method}"));
        s.line(&format!("pub const {table}: [GainCode; {CODES}] = ["));
        for r in rows {
            s.line(&format!(
                "    GainCode {{ code: {}, comparator_db: {}, comparator_at_least_db: {}, comparator_flip_level_db: {}, loop_meter_db: {}, comparator_run1_db: {} }},",
                r.code,
                opt_i16(r.comparator_db),
                opt_i16(r.comparator_at_least_db),
                opt_i16(r.flip_level_db),
                opt_f(r.loop_meter_db, 1),
                opt_i16(r.comparator_run1_db)
            ));
        }
        s.line("];");
        s.blank();
    }
    s.finish()
}

fn opt_i16(x: Option<i16>) -> String {
    match x {
        Some(v) => format!("Some({v})"),
        None => "None".into(),
    }
}
