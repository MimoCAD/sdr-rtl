//! The PLL tables from the PLL sweep (data/step4-pll-sweep.jsonl) and
//! the four lo-find records (data/step4-lo-find-sel1xx.jsonl): the VCO
//! range, the divider per SEL_DIV code, the reference rule, the Nint
//! runs that locked, R2 VCO_INDICATOR per setting and per lock state,
//! and the LO spans the usable codes reach.

use crate::emit::{self, Src, f};
use crate::records::{LoFind, PllSweep, Provenance, VcoEdge};
use crate::scale::RAIL_COUNTS;

/// A reading at or below this many counts is the tone received at full
/// strength: in the sweep the full-strength readings are at most 633
/// counts and the partial ones at least 1844 (findings.md step 4c:
/// "full strength is -200...-1350, values of 2000-6000 are partial").
pub const RECEIVED_MAX_COUNTS: i64 = 1000;
/// A reading at or below this many counts is the tone received at
/// least partially (findings.md step 4c: "values of 2000-6000 are
/// partial"); a run of settings may pass through a partial reading, and
/// the run's bounds are its first and last full-strength settings.
pub const PARTIAL_MAX_COUNTS: i64 = 6000;
/// A divider is confirmed by the sweep when the tone is received at
/// full strength at this many settings of one run at least (an
/// isolated hit is a coincidence of a harmonic or an image, findings.md
/// step 4d).
pub const MIN_RUN_SETTINGS: usize = 3;
/// Step 14's tone through the stream (dB over the median bin): at or
/// above this it is received at full strength. In the record the
/// received tones read 58.7-59.1 dB, the absent ones 1.3-4.1 dB, and one
/// setting between (23.8 dB, VCO 3.737 GHz).
pub const EDGE_RECEIVED_MIN_DB: f64 = 50.0;
/// At or above this, received partially.
pub const EDGE_PARTIAL_MIN_DB: f64 = 10.0;
/// The tuner's documented frequency range ([R820T] Table 1-2): a sweep
/// setting whose tone lies outside it says nothing about the lock, so
/// the R2 lock-state table counts only settings inside it.
pub const TUNER_RANGE_HZ: (f64, f64) = (42e6, 1002e6);
/// The lo-find response run must be this many consecutive tone steps
/// at least to be the LO's passband rather than a single hit.
pub const MIN_LO_FIND_RUN_POINTS: usize = 2;
/// The measured divider must be a power of two within this fraction of
/// f_VCO / LO (the IF passband is 8 MHz wide, so the run's centre is
/// known to a few MHz).
pub const DIVIDER_TOLERANCE: f64 = 0.10;
/// The VCO range the driver uses sits this far inside the received
/// bounds (findings.md step 4c: "30 MHz inside the received bounds"),
/// then rounded inward to `VCO_RANGE_ROUND_HZ`.
pub const VCO_RANGE_MARGIN_HZ: f64 = 30e6;
pub const VCO_RANGE_ROUND_HZ: f64 = 10e6;
/// The documented SEL_DIV codes and their dividers ([R820T2] p. 9:
/// 000 /2, 001 /4, 010 /8, 011 /1).
pub const DOCUMENTED_DIVIDERS: [(u8, u32); 4] = [(0b000, 2), (0b001, 4), (0b010, 8), (0b011, 1)];
/// Nint = 4 * Ni2c + Si2c + 13 with Ni2c six bits and Si2c two
/// ([R820T2] p. 9).
pub const NINT_MIN: u16 = 13;
pub const NINT_MAX: u16 = 13 + 4 * 63 + 3;

/// The sweep's run of received settings for one code under one divider.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub sel_div: u8,
    pub divider: u32,
    pub nint: (u16, u16),
    pub settings: usize,
    pub isolated: usize,
    pub skipped: usize,
}

/// The longest run of consecutive `true` values: (start index, length).
fn longest_run(flags: &[bool]) -> (usize, usize) {
    let (mut best, mut cur) = ((0usize, 0usize), (0usize, 0usize));
    for (i, &r) in flags.iter().enumerate() {
        if r {
            cur = if cur.1 == 0 {
                (i, 1)
            } else {
                (cur.0, cur.1 + 1)
            };
            if cur.1 > best.1 {
                best = cur;
            }
        } else {
            cur = (0, 0);
        }
    }
    best
}

/// The sweep's run for `sel_div` under hypothesis `div`: the longest
/// stretch of consecutive settings reading at least partially, bounded
/// by its first and last full-strength settings, and counted by them.
fn sweep_run(sweep: &PllSweep, sel_div: u8, div: u32) -> Option<Run> {
    let mut lines: Vec<(u16, Option<i64>)> = sweep
        .lines
        .iter()
        .filter(|l| l.sel_div == sel_div)
        .filter_map(|l| {
            l.on.iter()
                .find(|h| h.div == div)
                .map(|h| (l.nint, h.reading))
        })
        .collect();
    lines.sort_by_key(|l| l.0);
    if lines.is_empty() {
        return None;
    }
    let full: Vec<bool> = lines
        .iter()
        .map(|l| l.1.is_some_and(|r| r <= RECEIVED_MAX_COUNTS))
        .collect();
    let partial: Vec<bool> = lines
        .iter()
        .map(|l| l.1.is_some_and(|r| r <= PARTIAL_MAX_COUNTS))
        .collect();
    let skipped = lines.iter().filter(|l| l.1.is_none()).count();
    // Every stretch of partial-or-better settings, scored by the
    // full-strength settings it holds; the best stretch wins.
    let mut best: Option<(usize, usize, usize)> = None;
    let mut i = 0;
    while i < partial.len() {
        if !partial[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < partial.len() && partial[i] {
            i += 1;
        }
        let fulls: Vec<usize> = (start..i).filter(|&k| full[k]).collect();
        if let (Some(&a), Some(&b)) = (fulls.first(), fulls.last())
            && best.is_none_or(|(_, _, n)| fulls.len() > n)
        {
            best = Some((a, b, fulls.len()));
        }
    }
    let total = full.iter().filter(|&&r| r).count();
    match best {
        Some((a, b, n)) => Some(Run {
            sel_div,
            divider: div,
            nint: (lines[a].0, lines[b].0),
            settings: n,
            isolated: total - n,
            skipped,
        }),
        None => Some(Run {
            sel_div,
            divider: div,
            nint: (lines[0].0, lines[0].0),
            settings: 0,
            isolated: 0,
            skipped,
        }),
    }
}

/// What lo-find says about one undescribed code.
#[derive(Clone, Debug, PartialEq)]
pub struct LoFindResult {
    pub run_rf_hz: (f64, f64),
    pub run_points: usize,
    pub lo_hz: f64,
    pub divider: u32,
    pub isolated: usize,
}

fn lo_find_result(lf: &LoFind) -> Result<LoFindResult, String> {
    if lf.off_reading < RAIL_COUNTS {
        return Err(format!(
            "lo-find SEL_DIV {:03b}: the tone-off reading ({}) is not on the rail",
            lf.sel_div, lf.off_reading
        ));
    }
    let received: Vec<bool> = lf
        .points
        .iter()
        .map(|p| p.1 <= RECEIVED_MAX_COUNTS)
        .collect();
    let best = longest_run(&received);
    if best.1 < MIN_LO_FIND_RUN_POINTS {
        return Err(format!(
            "lo-find SEL_DIV {:03b}: no run of {MIN_LO_FIND_RUN_POINTS} received points",
            lf.sel_div
        ));
    }
    let run = (lf.points[best.0].0, lf.points[best.0 + best.1 - 1].0);
    // The wanted sideband is below the LO (findings.md step 4h), so the
    // LO is the run's centre plus the IF.
    let lo_hz = (run.0 + run.1) / 2.0 + lf.if_hz;
    let ratio = lf.vco_hz / lo_hz;
    let divider = 2f64.powf(ratio.log2().round()) as u32;
    if (ratio / divider as f64 - 1.0).abs() > DIVIDER_TOLERANCE {
        return Err(format!(
            "lo-find SEL_DIV {:03b}: f_VCO / LO = {ratio:.2} is not a power of two within {DIVIDER_TOLERANCE}",
            lf.sel_div
        ));
    }
    let total = received.iter().filter(|&&r| r).count();
    Ok(LoFindResult {
        run_rf_hz: run,
        run_points: best.1,
        lo_hz,
        divider,
        isolated: total - best.1,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    DocumentedConfirmed(Run),
    DocumentedUntested(String),
    Measured {
        lf: LoFindResult,
        sweep: Option<Run>,
    },
    Unusable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub code: u8,
    pub divider: Option<u32>,
    pub status: Status,
}

pub struct PllTables {
    pub entries: Vec<Entry>,
    pub vco_received_hz: (f64, f64),
    pub vco_received_nint: (u16, u16),
    pub vco_range_hz: (f64, f64),
    pub reach: Vec<(u8, u32, f64, f64)>,
    pub unreachable: Vec<(f64, f64)>,
    pub r2_rows: Vec<(u16, f64, [u8; 8])>,
    pub r2_10ms_differs: usize,
    /// Per distinct R2 value: how many settings read it with the tone
    /// received at full strength, partially, and not at all.
    pub r2_by_value: Vec<(u8, usize, usize, usize)>,
    pub r2_settings_counted: usize,
    /// Step 14's settings counted into `r2_by_value`.
    pub r2_edge_counted: usize,
    pub sweep_runs: Vec<Run>,
    pub nint_step: u16,
}

pub fn analyse(sweep: &PllSweep, lo_finds: &[LoFind], edge: &VcoEdge) -> Result<PllTables, String> {
    let mut entries = Vec::new();
    let mut sweep_runs = Vec::new();
    for code in 0..8u8 {
        let documented = DOCUMENTED_DIVIDERS
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, d)| *d);
        let entry = match documented {
            Some(div) => {
                let run = sweep_run(sweep, code, div)
                    .ok_or_else(|| format!("sweep: no lines for SEL_DIV {code:03b}"))?;
                if run.settings >= MIN_RUN_SETTINGS {
                    sweep_runs.push(run.clone());
                    Entry {
                        code,
                        divider: Some(div),
                        status: Status::DocumentedConfirmed(run),
                    }
                } else {
                    let reason = format!(
                        "the tone was received at no {MIN_RUN_SETTINGS} consecutive Nint settings under /{div} ({} received in all, {} settings skipped with the tone above the source's range)",
                        run.settings + run.isolated,
                        run.skipped
                    );
                    Entry {
                        code,
                        divider: Some(div),
                        status: Status::DocumentedUntested(reason),
                    }
                }
            }
            None => {
                let lf = lo_finds
                    .iter()
                    .find(|l| l.sel_div == code)
                    .ok_or_else(|| format!("no lo-find record for SEL_DIV {code:03b}"))?;
                let r = lo_find_result(lf)?;
                let tested = sweep
                    .lines
                    .iter()
                    .filter(|l| l.sel_div == code)
                    .any(|l| l.on.iter().any(|h| h.div == r.divider));
                let run = if tested {
                    sweep_run(sweep, code, r.divider)
                } else {
                    None
                };
                match run {
                    Some(run) if run.settings < MIN_RUN_SETTINGS => {
                        let reason = format!(
                            "lo-find answers only around f_VCO / {} at Nint {} ({:.0}-{:.0} MHz), and the sweep received the /{} prediction at {} Nint setting(s) in a row ({} in all): not a divider across the VCO range",
                            r.divider,
                            lf.nint,
                            r.run_rf_hz.0 / 1e6,
                            r.run_rf_hz.1 / 1e6,
                            r.divider,
                            run.settings,
                            run.settings + run.isolated
                        );
                        Entry {
                            code,
                            divider: None,
                            status: Status::Unusable(reason),
                        }
                    }
                    run => {
                        if let Some(run) = &run {
                            sweep_runs.push(run.clone());
                        }
                        Entry {
                            code,
                            divider: Some(r.divider),
                            status: Status::Measured { lf: r, sweep: run },
                        }
                    }
                }
            }
        };
        entries.push(entry);
    }

    // The VCO range from the documented codes' runs: f_VCO = f_ref * 2 * Nint.
    let runs: Vec<&Run> = entries
        .iter()
        .filter_map(|e| {
            if let Status::DocumentedConfirmed(r) = &e.status {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    if runs.is_empty() {
        return Err("no documented divider confirmed: no VCO range".into());
    }
    let nint_lo = runs.iter().map(|r| r.nint.0).min().unwrap();
    let nint_hi = runs.iter().map(|r| r.nint.1).max().unwrap();
    let vco = |nint: u16| sweep.f_ref_hz * 2.0 * nint as f64;
    let vco_received_hz = (vco(nint_lo), vco(nint_hi));
    let vco_range_hz = (
        ((vco_received_hz.0 + VCO_RANGE_MARGIN_HZ) / VCO_RANGE_ROUND_HZ).ceil()
            * VCO_RANGE_ROUND_HZ,
        ((vco_received_hz.1 - VCO_RANGE_MARGIN_HZ) / VCO_RANGE_ROUND_HZ).floor()
            * VCO_RANGE_ROUND_HZ,
    );

    // LO reach per usable code, and the gaps between the spans.
    let mut reach: Vec<(u8, u32, f64, f64)> = entries
        .iter()
        .filter_map(|e| {
            e.divider.map(|d| {
                (
                    e.code,
                    d,
                    vco_range_hz.0 / d as f64,
                    vco_range_hz.1 / d as f64,
                )
            })
        })
        .collect();
    reach.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut unreachable = Vec::new();
    let mut covered_to = reach[0].3;
    for r in &reach[1..] {
        if r.2 > covered_to {
            unreachable.push((covered_to, r.2));
        }
        covered_to = covered_to.max(r.3);
    }

    // R2 per Nint, per code.
    let mut nints: Vec<u16> = sweep.lines.iter().map(|l| l.nint).collect();
    nints.sort_unstable();
    nints.dedup();
    let mut r2_rows = Vec::new();
    let mut r2_10ms_differs = 0;
    for &nint in &nints {
        let mut row = [0u8; 8];
        for code in 0..8u8 {
            let l = sweep
                .lines
                .iter()
                .find(|l| l.nint == nint && l.sel_div == code)
                .ok_or_else(|| format!("sweep: SEL_DIV {code:03b} has no line at Nint {nint}"))?;
            row[code as usize] = l.r2_at_60ms;
            if l.r2_at_10ms != l.r2_at_60ms {
                r2_10ms_differs += 1;
            }
        }
        r2_rows.push((nint, vco(nint), row));
    }
    let nint_step = nints.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(0);

    // R2 by lock state: the usable codes under their dividers, the
    // settings whose tone lay inside the tuner's range.
    let mut r2_by_value: Vec<(u8, usize, usize, usize)> = Vec::new();
    let mut r2_settings_counted = 0;
    for e in &entries {
        let usable = matches!(
            e.status,
            Status::DocumentedConfirmed(_) | Status::Measured { .. }
        );
        let Some(div) = e.divider.filter(|_| usable) else {
            continue;
        };
        for l in sweep.lines.iter().filter(|l| l.sel_div == e.code) {
            let Some(h) = l.on.iter().find(|h| h.div == div) else {
                continue;
            };
            let Some(r) = h.reading else { continue };
            if h.rf_hz < TUNER_RANGE_HZ.0 || h.rf_hz > TUNER_RANGE_HZ.1 {
                continue;
            }
            r2_settings_counted += 1;
            let i = match r2_by_value.iter().position(|v| v.0 == l.r2_at_60ms) {
                Some(i) => i,
                None => {
                    r2_by_value.push((l.r2_at_60ms, 0, 0, 0));
                    r2_by_value.len() - 1
                }
            };
            if r <= RECEIVED_MAX_COUNTS {
                r2_by_value[i].1 += 1;
            } else if r <= PARTIAL_MAX_COUNTS {
                r2_by_value[i].2 += 1;
            } else {
                r2_by_value[i].3 += 1;
            }
        }
    }
    // Step 14 (the VCO's ends at 1 MHz RF steps, through the stream)
    // counted into the same table, classified by its own measure.
    let mut r2_edge_counted = 0;
    for l in edge
        .lines
        .iter()
        .filter(|l| l.rf_hz >= TUNER_RANGE_HZ.0 && l.rf_hz <= TUNER_RANGE_HZ.1)
    {
        r2_edge_counted += 1;
        let i = match r2_by_value.iter().position(|v| v.0 == l.r2) {
            Some(i) => i,
            None => {
                r2_by_value.push((l.r2, 0, 0, 0));
                r2_by_value.len() - 1
            }
        };
        if l.tone_db >= EDGE_RECEIVED_MIN_DB {
            r2_by_value[i].1 += 1;
        } else if l.tone_db >= EDGE_PARTIAL_MIN_DB {
            r2_by_value[i].2 += 1;
        } else {
            r2_by_value[i].3 += 1;
        }
    }
    r2_by_value.sort_by_key(|v| v.0);

    Ok(PllTables {
        entries,
        vco_received_hz,
        vco_received_nint: (nint_lo, nint_hi),
        vco_range_hz,
        reach,
        unreachable,
        r2_rows,
        r2_10ms_differs,
        r2_by_value,
        r2_settings_counted,
        r2_edge_counted,
        sweep_runs,
        nint_step,
    })
}

pub fn emit(t: &PllTables, sweep: &PllSweep, lo_finds: &[LoFind], edge: &VcoEdge) -> String {
    let mut provs: Vec<&Provenance> = vec![&sweep.provenance];
    provs.extend(lo_finds.iter().map(|l| &l.provenance));
    provs.push(&edge.provenance);
    let mut s = Src::new(&provs);
    s.module_doc(&format!(
        "The PLL: VCO range, SEL_DIV dividers, the reference rule, the Nint
runs that locked, R2 VCO_INDICATOR, and the LO spans reachable.

Method (findings.md steps 4c, 4d): the sweep programmed every SEL_DIV
code x Nint {}..={} in steps of {} (Nfra 0, REF_DIV2 {}), read R0-R4,
then placed the tone at the RF each divider hypothesis predicts
(f_LO = f_ref * 2 * (Nint + Nfra) / divider, RF = LO - {:.2} MHz) and
read `if_agc_val` MSB-first in the vagc configuration. A setting is
\"received\" when that reading is at or below {} counts (the full-strength
readings are at most 633, the partial ones at least 1844). The
undescribed codes 100-111 had the PLL held at Nint {} and the tone
stepped across {:.0}-{:.0} MHz in {:.0} MHz steps (lo-find); the LO is the
full-strength run's centre plus the IF (the wanted sideband is below
the LO, step 4h) and the divider is f_VCO / LO rounded to a power of
two. A sweep run may pass through a setting that read partially (at
most {} counts); its bounds are its first and last full-strength
settings.

Resolution: the sweep's Nint step is {} (f_VCO steps of {:.0} MHz), so a
run's edges are known to one step; lo-find's RF step is {:.0} MHz.

Caveats: the /1 code could not be tested (its LO is above the source's
6 GHz); the VCO range is the documented codes' received bounds and the
driver's `VCO_RANGE_HZ` sits {:.0} MHz inside them by choice; no SEL_DIV
code reaches the LO span in `LO_UNREACHABLE_HZ` (RF about 110-208 MHz
with a 4.57 MHz IF) — whether the VCO runs lower under settings the
sweep did not vary (R24/R25/R28/R31 unspecified cells, the LDO
voltages, PW_SDM, CAPX) is an open search, not assumed either way.",
        NINT_MIN,
        NINT_MAX,
        t.nint_step,
        sweep.ref_div2 as u8,
        sweep.if_hz / 1e6,
        RECEIVED_MAX_COUNTS,
        lo_finds.first().map(|l| l.nint).unwrap_or(0),
        lo_finds
            .first()
            .and_then(|l| l.points.first())
            .map(|p| p.0 / 1e6)
            .unwrap_or(0.0),
        lo_finds
            .first()
            .and_then(|l| l.points.last())
            .map(|p| p.0 / 1e6)
            .unwrap_or(0.0),
        lo_finds.first().map(|l| l.rf_step_hz / 1e6).unwrap_or(0.0),
        PARTIAL_MAX_COUNTS,
        t.nint_step,
        sweep.f_ref_hz * 2.0 * t.nint_step as f64 / 1e6,
        lo_finds.first().map(|l| l.rf_step_hz / 1e6).unwrap_or(0.0),
        VCO_RANGE_MARGIN_HZ / 1e6
    ));
    s.blank();
    s.line("use super::types::*;");
    s.blank();

    s.doc(&format!(
        "The crystal and the reference: REF_DIV2 (R16[4]) = 1 halves the
crystal \"for Xtal > 24 MHz\" ([R820T2] p. 9); the crystal is {:.1} MHz,
so f_ref = {:.1} MHz. The sweep ran with REF_DIV2 = {} and every table
here holds for that setting.",
        sweep.f_ref_hz * if sweep.ref_div2 { 2.0 } else { 1.0 } / 1e6,
        sweep.f_ref_hz / 1e6,
        sweep.ref_div2 as u8
    ));
    s.line(&format!(
        "pub const XTAL_HZ: f64 = {};",
        f(sweep.f_ref_hz * if sweep.ref_div2 { 2.0 } else { 1.0 }, 1)
    ));
    s.line(&format!("pub const REF_DIV2: bool = {};", sweep.ref_div2));
    s.line(&format!(
        "pub const F_REF_HZ: f64 = {};",
        f(sweep.f_ref_hz, 1)
    ));
    s.blank();
    s.doc(&format!(
        "Nint = 4 * Ni2c + Si2c + 13 (R20, [R820T2] p. 9): {}..={}. The sweep
covered it in steps of {}.",
        NINT_MIN, NINT_MAX, t.nint_step
    ));
    s.line(&format!(
        "pub const NINT_RANGE: (u16, u16) = ({NINT_MIN}, {NINT_MAX});"
    ));
    s.blank();
    s.doc(&format!(
        "The VCO span over which the documented dividers received the tone:
Nint {}..={} (f_VCO = f_ref * 2 * Nint), with no reception one sweep step
outside either end.",
        t.vco_received_nint.0, t.vco_received_nint.1
    ));
    s.line(&format!(
        "pub const VCO_RECEIVED_NINT: (u16, u16) = ({}, {});",
        t.vco_received_nint.0, t.vco_received_nint.1
    ));
    s.line(&format!(
        "pub const VCO_RECEIVED_HZ: (f64, f64) = ({}, {});",
        f(t.vco_received_hz.0, 1),
        f(t.vco_received_hz.1, 1)
    ));
    s.blank();
    s.doc(&format!(
        "The VCO range the driver programs within: `VCO_RECEIVED_HZ` moved
{:.0} MHz inward and rounded inward to {:.0} MHz (a choice, findings.md
step 4c).",
        VCO_RANGE_MARGIN_HZ / 1e6,
        VCO_RANGE_ROUND_HZ / 1e6
    ));
    s.line(&format!(
        "pub const VCO_RANGE_HZ: (f64, f64) = ({}, {});",
        f(t.vco_range_hz.0, 1),
        f(t.vco_range_hz.1, 1)
    ));
    s.blank();

    s.doc(&format!(
        "SEL_DIV (R16[7:5]) code -> the mixer divider: the documented codes
as the sweep confirmed them, the undescribed codes as lo-find read them
(a code is unusable when the sweep received its lo-find divider at fewer
than {} consecutive Nint settings).",
        MIN_RUN_SETTINGS
    ));
    s.line("pub const SEL_DIV: [SelDivEntry; 8] = [");
    for e in &t.entries {
        let status = match &e.status {
            Status::DocumentedConfirmed(r) => format!(
                "DividerStatus::DocumentedConfirmed {{ nint_received: ({}, {}) }}",
                r.nint.0, r.nint.1
            ),
            Status::DocumentedUntested(reason) => format!(
                "DividerStatus::DocumentedUntested {{ reason: {} }}",
                emit::s(reason)
            ),
            Status::Measured { lf, sweep } => format!(
                "DividerStatus::Measured {{ lo_find_run_rf_hz: ({}, {}), lo_hz: {}, sweep_nint_received: {}, lo_find_isolated_hits: {} }}",
                f(lf.run_rf_hz.0, 1),
                f(lf.run_rf_hz.1, 1),
                f(lf.lo_hz, 1),
                sweep
                    .as_ref()
                    .map(|r| format!("Some(({}, {}))", r.nint.0, r.nint.1))
                    .unwrap_or_else(|| "None".into()),
                lf.isolated
            ),
            Status::Unusable(reason) => {
                format!("DividerStatus::Unusable {{ reason: {} }}", emit::s(reason))
            }
        };
        let div = e
            .divider
            .map(|d| format!("Some({d})"))
            .unwrap_or_else(|| "None".into());
        s.line(&format!(
            "    SelDivEntry {{ code: 0b{:03b}, divider: {div}, status: {status} }},",
            e.code
        ));
    }
    s.line("];");
    s.blank();

    s.doc(
        "The sweep's contiguous run of received Nint settings per confirmed
code under its divider; `isolated_hits` are received settings outside
the run (single hits of a harmonic or an image, not a lock range).",
    );
    s.line(&format!(
        "pub const SWEEP_RUNS: [SweepRun; {}] = [",
        t.sweep_runs.len()
    ));
    for r in &t.sweep_runs {
        s.line(&format!(
            "    SweepRun {{ sel_div: 0b{:03b}, divider: {}, nint_received: ({}, {}), settings_received: {}, isolated_hits: {} }},",
            r.sel_div, r.divider, r.nint.0, r.nint.1, r.settings, r.isolated
        ));
    }
    s.line("];");
    s.blank();

    s.doc(
        "The LO span each usable code reaches with the VCO inside
`VCO_RANGE_HZ` (LO = VCO / divider), ascending.",
    );
    s.line(&format!(
        "pub const LO_REACH: [LoReach; {}] = [",
        t.reach.len()
    ));
    for (code, div, lo, hi) in &t.reach {
        s.line(&format!("    LoReach {{ sel_div: 0b{code:03b}, divider: {div}, lo_min_hz: {}, lo_max_hz: {} }},", f(*lo, 1), f(*hi, 1)));
    }
    s.line("];");
    s.blank();
    s.doc(
        "LO spans between the lowest and the highest reachable LO that no
usable code covers.",
    );
    s.line(&format!(
        "pub const LO_UNREACHABLE_HZ: [(f64, f64); {}] = [",
        t.unreachable.len()
    ));
    for (lo, hi) in &t.unreachable {
        s.line(&format!("    ({}, {}),", f(*lo, 1), f(*hi, 1)));
    }
    s.line("];");
    s.blank();

    s.doc(&format!(
        "R2 VCO_INDICATOR[6:0] (named, never described by [R820T2]) read 60 ms
after the PLL words were written, per sweep Nint and SEL_DIV code
(index = code). The 10 ms read differed from the 60 ms read in {} of the
{} settings. Bit 7 read 1 throughout (the matrix prints it 0).",
        t.r2_10ms_differs,
        t.r2_rows.len() * 8
    ));
    s.line(&format!(
        "pub const R2_VCO_INDICATOR: [R2Row; {}] = [",
        t.r2_rows.len()
    ));
    for (nint, vco, row) in &t.r2_rows {
        let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        s.line(&format!(
            "    R2Row {{ nint: {nint}, vco_hz: {}, r2_by_sel_div: [{}] }},",
            f(*vco, 1),
            cells.join(", ")
        ));
    }
    s.line("];");
    s.blank();
    s.doc(&format!(
        "R2 by lock state: for each distinct R2 value, how many
settings read it with the tone received at full strength,
partially, and not at all. Two records: the step 4c sweep's
settings of the usable codes under their dividers ({}
settings; the meter at most {} counts full strength, at most
{} partial), and step 14's ends of the VCO at 1 MHz RF steps
through the raw-sample stream ({} settings; the tone at least
{} dB over the median bin full strength, at least {} partial)
— those whose tone lay inside the tuner's {:.0}-{:.0} MHz
range. Step 4c's Nint steps of 5 never read 64-69; step 14
reads them, received, at the bottom 90 MHz of the VCO. 0 and
63 are the range's ends.",
        t.r2_settings_counted,
        RECEIVED_MAX_COUNTS,
        PARTIAL_MAX_COUNTS,
        t.r2_edge_counted,
        EDGE_RECEIVED_MIN_DB,
        EDGE_PARTIAL_MIN_DB,
        TUNER_RANGE_HZ.0 / 1e6,
        TUNER_RANGE_HZ.1 / 1e6
    ));
    s.line(&format!(
        "pub const R2_BY_LOCK_STATE: [R2LockCount; {}] = [",
        t.r2_by_value.len()
    ));
    for (r2, full, partial, none) in &t.r2_by_value {
        s.line(&format!("    R2LockCount {{ r2: {r2}, full_strength: {full}, partial: {partial}, not_received: {none} }},"));
    }
    s.line("];");
    s.finish()
}
