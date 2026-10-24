//! The band table from data/step4-band-table.jsonl (findings.md step
//! 4f): per grid frequency the best (RFFILT, RFMUX) path and the best
//! (TF_NCH, TF_LP) corner codes with their readings relative to the
//! bypass path, then the segments of neighbouring frequencies sharing
//! one setting.

use crate::emit::{self, Src, f, opt_f};
use crate::records::{BandFreq, BandReading, BandTable, Provenance};
use crate::scale::{MeterScale, RAIL_COUNTS};

/// The bypass path every frequency's readings are relative to: the
/// path the source was re-levelled at (RFMUX 01 bypass, RFFILT 00,
/// TF 0000/0000; the record's procedure line).
pub const BYPASS_RFFILT: u8 = 0b00;
pub const BYPASS_RFMUX: u8 = 0b01;
pub const RFFILT_CODES: u8 = 4;
pub const RFMUX_CODES: u8 = 4;
pub const TF_CODES: u8 = 16;
/// Stage A measures every path once at TF 0000/0000; stage B every
/// corner code on the two best.
pub const STAGE_A_READINGS: usize = RFFILT_CODES as usize * RFMUX_CODES as usize;
pub const STAGE_B_PATHS: usize = 2;
pub const STAGE_B_READINGS_PER_PATH: usize = TF_CODES as usize * TF_CODES as usize;
/// Neighbouring grid frequencies merge into one corner-code segment
/// while some corner code reads within this many dB of every member's
/// own best: the loop's two-state alternation (findings.md step 4g,
/// about 1.7 dB), which a single-snapshot reading cannot resolve below.
pub const SEGMENT_TOLERANCE_DB: f64 = 1.7;
/// The repeatability findings.md step 4f claims for a path reading
/// ("0.2-0.5 dB"); the segment count at this tolerance is reported for
/// comparison.
pub const REPEATABILITY_DB: f64 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub struct PathResult {
    pub rffilt: u8,
    pub rfmux: u8,
    pub tf_nch: u8,
    pub tf_lp: u8,
    pub best_db: f64,
    pub at_0000_db: f64,
    /// Every corner code's reading in dB vs bypass, indexed nch * 16 + lp.
    pub corners_db: Vec<Option<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Measured {
        best: PathResult,
        runner_up: Option<PathResult>,
        margin_db: Option<f64>,
        spread_db: f64,
        stage_a_agrees: bool,
        stage_a_db: Box<[Option<f64>; STAGE_A_READINGS]>,
        repeat_db: Vec<f64>,
    },
    Unmeasured(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub rf_hz: u32,
    pub entry: Entry,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub from_hz: u32,
    pub to_hz: u32,
    pub path: Option<(u8, u8, u8, u8)>,
    pub points: usize,
}

pub struct BandTables {
    pub points: Vec<Point>,
    /// Segments by path and corner codes at `SEGMENT_TOLERANCE_DB`.
    pub segments: Vec<Segment>,
    /// How many the same rule gives at `REPEATABILITY_DB`.
    pub segments_at_repeatability: usize,
    /// Segments by (RFFILT, RFMUX) alone.
    pub path_segments: Vec<Segment>,
    pub stage_a_disagrees: usize,
    pub measured: usize,
    pub readings: usize,
    pub source_level_db: i32,
    /// Stage A vs stage B at TF 0000/0000 on the staged paths: |dB|
    /// differences, sorted.
    pub repeat_db_sorted: Vec<f64>,
    pub corner_gain_db_sorted: Vec<f64>,
    pub spread_db_sorted: Vec<f64>,
    /// The best path's margin over the runner-up, sorted.
    pub margin_db_sorted: Vec<f64>,
    pub dead_paths: [usize; STAGE_A_READINGS],
    pub best_path_counts: Vec<((u8, u8), usize)>,
}

fn db_vs(bypass: i64, reading: i64, scale: &MeterScale) -> Option<f64> {
    if reading >= RAIL_COUNTS {
        None
    } else {
        Some(emit::round(scale.db_more(bypass, reading), 1))
    }
}

fn path_result(
    block: &[&BandReading],
    bypass: i64,
    scale: &MeterScale,
) -> Result<PathResult, String> {
    let (rffilt, rfmux) = (block[0].rffilt, block[0].rfmux);
    if block.len() != STAGE_B_READINGS_PER_PATH {
        return Err(format!(
            "path ({rffilt:02b}, {rfmux:02b}): {} stage-B readings, not {STAGE_B_READINGS_PER_PATH}",
            block.len()
        ));
    }
    let mut corners: Vec<Option<(i64, f64)>> = vec![None; STAGE_B_READINGS_PER_PATH];
    for r in block {
        let i = (r.tf_nch * TF_CODES + r.tf_lp) as usize;
        if corners[i].is_some() {
            return Err(format!(
                "path ({rffilt:02b}, {rfmux:02b}): corner ({}, {}) measured twice in stage B",
                r.tf_nch, r.tf_lp
            ));
        }
        corners[i] = Some((r.reading, scale.db_more(bypass, r.reading)));
    }
    let corners: Vec<(i64, f64)> = corners
        .into_iter()
        .map(|c| c.ok_or("a corner code missing"))
        .collect::<Result<_, _>>()?;
    // The best corner: the most signal (lowest counts); ties to the
    // lowest (TF_NCH, TF_LP) — index order.
    let (best_i, best) = corners
        .iter()
        .enumerate()
        .fold((0usize, corners[0]), |acc, (i, c)| {
            if c.0 < acc.1.0 { (i, *c) } else { acc }
        });
    if best.0 >= RAIL_COUNTS {
        return Err(format!(
            "path ({rffilt:02b}, {rfmux:02b}): nothing passed at any corner code"
        ));
    }
    Ok(PathResult {
        rffilt,
        rfmux,
        tf_nch: (best_i / TF_CODES as usize) as u8,
        tf_lp: (best_i % TF_CODES as usize) as u8,
        best_db: emit::round(best.1, 1),
        at_0000_db: emit::round(corners[0].1, 1),
        corners_db: corners
            .iter()
            .map(|c| {
                if c.0 >= RAIL_COUNTS {
                    None
                } else {
                    Some(emit::round(c.1, 1))
                }
            })
            .collect(),
    })
}

fn analyse_freq(fq: &BandFreq, scale: &MeterScale) -> Result<Entry, String> {
    if let Some(e) = &fq.error {
        return Ok(Entry::Unmeasured(e.clone()));
    }
    let reference = fq.reference.as_ref().ok_or("no ref line")?;
    let bypass = reference.reading;
    if fq.readings.len() < STAGE_A_READINGS {
        return Err(format!(
            "{} readings, fewer than stage A's {STAGE_A_READINGS}",
            fq.readings.len()
        ));
    }
    // Stage A: the first 16 lines, every path once at TF 0000/0000.
    let (stage_a, stage_b) = fq.readings.split_at(STAGE_A_READINGS);
    let mut stage_a_db = [None; STAGE_A_READINGS];
    let mut stage_a_counts = [None; STAGE_A_READINGS];
    for r in stage_a {
        if r.tf_nch != 0 || r.tf_lp != 0 {
            return Err("a stage-A line with corner codes other than 0000/0000".into());
        }
        let i = (r.rffilt * RFMUX_CODES + r.rfmux) as usize;
        if stage_a_counts[i].is_some() {
            return Err(format!(
                "path ({:02b}, {:02b}) measured twice in stage A",
                r.rffilt, r.rfmux
            ));
        }
        stage_a_counts[i] = Some(r.reading);
        stage_a_db[i] = db_vs(bypass, r.reading, scale);
    }
    if stage_a_counts.iter().any(Option::is_none) {
        return Err("stage A did not cover every path".into());
    }
    let live: Vec<f64> = stage_a_db.iter().flatten().copied().collect();
    let spread_db = if live.is_empty() {
        0.0
    } else {
        emit::round(
            live.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - live.iter().cloned().fold(f64::INFINITY, f64::min),
            1,
        )
    };

    // Stage B: the two best paths of stage A, 256 corner codes each.
    if fq.stage_a_best.len() != STAGE_B_PATHS {
        return Err(format!(
            "{} staged paths, not {STAGE_B_PATHS}",
            fq.stage_a_best.len()
        ));
    }
    let mut results = Vec::new();
    let mut repeat_db = Vec::new();
    for &(rffilt, rfmux) in &fq.stage_a_best {
        let block: Vec<&BandReading> = stage_b
            .iter()
            .filter(|r| r.rffilt == rffilt && r.rfmux == rfmux)
            .collect();
        match path_result(&block, bypass, scale) {
            Ok(p) => {
                let a =
                    stage_a_counts[(rffilt * RFMUX_CODES + rfmux) as usize].unwrap_or(RAIL_COUNTS);
                let b = block
                    .iter()
                    .find(|r| r.tf_nch == 0 && r.tf_lp == 0)
                    .map(|r| r.reading)
                    .unwrap_or(RAIL_COUNTS);
                repeat_db.push(emit::round(scale.db_more(a, b).abs(), 1));
                results.push(p);
            }
            Err(e) if e.ends_with("nothing passed at any corner code") => {}
            Err(e) => return Err(e),
        }
    }
    if results.is_empty() {
        return Err("neither staged path passed the tone at any corner code".into());
    }
    // The winner: the most signal at its best corner; ties to the lower
    // (RFFILT, RFMUX).
    results.sort_by(|a, b| {
        b.best_db
            .partial_cmp(&a.best_db)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then((a.rffilt, a.rfmux).cmp(&(b.rffilt, b.rfmux)))
    });
    let best = results[0].clone();
    let runner_up = results.get(1).cloned();
    let margin_db = runner_up
        .as_ref()
        .map(|r| emit::round(best.best_db - r.best_db, 1));
    let stage_a_agrees = fq.stage_a_best[0] == (best.rffilt, best.rfmux);
    Ok(Entry::Measured {
        best,
        runner_up,
        margin_db,
        spread_db,
        stage_a_agrees,
        stage_a_db: Box::new(stage_a_db),
        repeat_db,
    })
}

pub fn analyse(rec: &BandTable, scale: &MeterScale) -> Result<BandTables, String> {
    if !rec.staged {
        return Err("the band table record is not the staged search this generator reads".into());
    }
    let mut points = Vec::new();
    let mut source_level = None;
    let mut readings = 0;
    for fq in &rec.freqs {
        if let Some(r) = &fq.reference {
            match source_level {
                None => source_level = Some(r.source_level_db),
                Some(l) if l != r.source_level_db => {
                    return Err(format!(
                        "{} MHz: the source level changed to {} dB (was {l})",
                        fq.rf_hz / 1_000_000,
                        r.source_level_db
                    ));
                }
                _ => {}
            }
        }
        readings += fq.readings.len();
        let entry =
            analyse_freq(fq, scale).map_err(|e| format!("band table at {} Hz: {e}", fq.rf_hz))?;
        points.push(Point {
            rf_hz: fq.rf_hz,
            entry,
        });
    }

    // Statistics over the measured points.
    let mut repeat = Vec::new();
    let mut corner_gain = Vec::new();
    let mut spreads = Vec::new();
    let mut margins = Vec::new();
    let mut dead = [0usize; STAGE_A_READINGS];
    let mut best_counts: Vec<((u8, u8), usize)> = Vec::new();
    let mut measured = 0;
    for p in &points {
        if let Entry::Measured {
            best,
            margin_db,
            spread_db,
            stage_a_db,
            repeat_db,
            ..
        } = &p.entry
        {
            measured += 1;
            repeat.extend(repeat_db.iter().copied());
            corner_gain.push(emit::round(best.best_db - best.at_0000_db, 1));
            spreads.push(*spread_db);
            margins.extend(*margin_db);
            for (i, d) in stage_a_db.iter().enumerate() {
                if d.is_none() {
                    dead[i] += 1;
                }
            }
            let key = (best.rffilt, best.rfmux);
            match best_counts.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => best_counts.push((key, 1)),
            }
        }
    }
    let sortf =
        |v: &mut Vec<f64>| v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sortf(&mut repeat);
    sortf(&mut corner_gain);
    sortf(&mut spreads);
    sortf(&mut margins);
    best_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let segments = corner_segments(&points, SEGMENT_TOLERANCE_DB);
    let segments_at_repeatability = corner_segments(&points, REPEATABILITY_DB).len();
    let path_segments = path_segments(&points);
    let stage_a_disagrees = points
        .iter()
        .filter(|p| {
            matches!(
                p.entry,
                Entry::Measured {
                    stage_a_agrees: false,
                    ..
                }
            )
        })
        .count();

    Ok(BandTables {
        points,
        segments,
        segments_at_repeatability,
        path_segments,
        stage_a_disagrees,
        measured,
        readings,
        source_level_db: source_level.ok_or("no measured frequency")?,
        repeat_db_sorted: repeat,
        corner_gain_db_sorted: corner_gain,
        spread_db_sorted: spreads,
        margin_db_sorted: margins,
        dead_paths: dead,
        best_path_counts: best_counts,
    })
}

/// Append an unmeasured grid point to the segment list.
fn push_unmeasured(out: &mut Vec<Segment>, rf_hz: u32) {
    match out.last_mut() {
        Some(s) if s.path.is_none() => {
            s.to_hz = rf_hz;
            s.points += 1;
        }
        _ => out.push(Segment {
            from_hz: rf_hz,
            to_hz: rf_hz,
            path: None,
            points: 1,
        }),
    }
}

/// A segment being extended: the corner codes still within tolerance
/// of every member's best, each with its worst shortfall so far.
struct Open {
    seg: Segment,
    /// Per corner code (index nch * 16 + lp): Some(the smallest margin
    /// over the members, in dB at or below 0) while the code is still a
    /// candidate; None once a member put it outside the tolerance.
    margins: Vec<Option<f64>>,
}

fn close(o: Open) -> Segment {
    // The candidate whose worst shortfall is the smallest; ties to the
    // lowest code.
    let mut best: Option<(usize, f64)> = None;
    for (i, m) in o.margins.iter().enumerate() {
        if let Some(m) = m
            && best.is_none_or(|(_, b)| *m > b)
        {
            best = Some((i, *m));
        }
    }
    let (i, _) = best.expect("an open segment keeps at least one candidate");
    let Some((rffilt, rfmux, _, _)) = o.seg.path else {
        unreachable!("an open segment has a path")
    };
    Segment {
        path: Some((
            rffilt,
            rfmux,
            (i / TF_CODES as usize) as u8,
            (i % TF_CODES as usize) as u8,
        )),
        ..o.seg
    }
}

/// Segments from the lowest frequency up: a frequency joins the open
/// segment when its best path is the segment's and at least one corner
/// code reads within `tolerance_db` of every member's own best; the
/// segment's corner codes are the surviving candidate with the smallest
/// worst shortfall.
fn corner_segments(points: &[Point], tolerance_db: f64) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut open: Option<Open> = None;
    for p in points {
        let best = match &p.entry {
            Entry::Unmeasured(_) => {
                if let Some(o) = open.take() {
                    out.push(close(o));
                }
                push_unmeasured(&mut out, p.rf_hz);
                continue;
            }
            Entry::Measured { best, .. } => best,
        };
        // This point's margin per corner code: reading minus its best,
        // at or below 0; None where the code is outside the tolerance
        // or passed nothing.
        let own: Vec<Option<f64>> = best
            .corners_db
            .iter()
            .map(|d| {
                d.filter(|d| best.best_db - d <= tolerance_db)
                    .map(|d| d - best.best_db)
            })
            .collect();
        let mut joined = false;
        if let Some(o) = &mut open
            && o.seg.path.map(|(rf, mx, _, _)| (rf, mx)) == Some((best.rffilt, best.rfmux))
        {
            let merged: Vec<Option<f64>> = o
                .margins
                .iter()
                .zip(&own)
                .map(|(a, b)| a.zip(*b).map(|(a, b)| a.min(b)))
                .collect();
            if merged.iter().any(Option::is_some) {
                o.margins = merged;
                o.seg.to_hz = p.rf_hz;
                o.seg.points += 1;
                joined = true;
            }
        }
        if !joined {
            if let Some(o) = open.take() {
                out.push(close(o));
            }
            open = Some(Open {
                seg: Segment {
                    from_hz: p.rf_hz,
                    to_hz: p.rf_hz,
                    path: Some((best.rffilt, best.rfmux, 0, 0)),
                    points: 1,
                },
                margins: own,
            });
        }
    }
    if let Some(o) = open.take() {
        out.push(close(o));
    }
    out
}

/// Segments by (RFFILT, RFMUX) alone, corner codes left at 0000/0000.
fn path_segments(points: &[Point]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for p in points {
        match &p.entry {
            Entry::Unmeasured(_) => push_unmeasured(&mut out, p.rf_hz),
            Entry::Measured { best, .. } => match out.last_mut() {
                Some(s)
                    if s.path
                        .is_some_and(|(rf, mx, _, _)| (rf, mx) == (best.rffilt, best.rfmux)) =>
                {
                    s.to_hz = p.rf_hz;
                    s.points += 1;
                }
                _ => out.push(Segment {
                    from_hz: p.rf_hz,
                    to_hz: p.rf_hz,
                    path: Some((best.rffilt, best.rfmux, 0, 0)),
                    points: 1,
                }),
            },
        }
    }
    out
}

fn pct(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    v[i]
}

fn path_lit(p: &PathResult) -> String {
    format!(
        "BandPath {{ path: FrontPath {{ rffilt: 0b{:02b}, rfmux: 0b{:02b}, tf_nch: 0b{:04b}, tf_lp: 0b{:04b} }}, best_db_vs_bypass: {}, at_tf_0000_db_vs_bypass: {} }}",
        p.rffilt,
        p.rfmux,
        p.tf_nch,
        p.tf_lp,
        f(p.best_db, 1),
        f(p.at_0000_db, 1)
    )
}

pub fn emit(t: &BandTables, rec: &BandTable, scale: &MeterScale) -> String {
    let records: Vec<&Provenance> = rec.provenance.iter().chain([&scale.provenance]).collect();
    let mut s = Src::new(&records);
    let unmeasured = t.segments.iter().find(|s| s.path.is_none());
    let gap = match unmeasured {
        Some(u) => format!(
            "; and no setting is measured between the grid\nfrequencies {} and {} MHz: no SEL_DIV code reached the LO when the\ntable was taken (011 was read as the document's /1; findings 10.5\nmeasured /16, and findings 4f2 measures the span)",
            u.from_hz / 1_000_000,
            u.to_hz / 1_000_000
        ),
        None => String::new(),
    };
    let first = t.points.first().map(|p| p.rf_hz).unwrap_or(0);
    let last = t.points.last().map(|p| p.rf_hz).unwrap_or(0);
    s.module_doc(&format!(
        "The front end per frequency: the (RFFILT, RFMUX) path and the
(TF_NCH, TF_LP) corner codes that pass a cabled tone best, {} grid
frequencies from {} to {} MHz in {} MHz steps ({} measured, {} readings),
and the segments of neighbours sharing one setting.

Method (findings.md step 4f; superseded for the driver by step 4f2's
every-path table, `band_paths`): at each frequency the LO was programmed
(IF {:.2} MHz above the RF), the source re-levelled once at the bypass
path (RFFILT {:02b}, RFMUX {:02b}, TF 0000/0000) and then held — it stayed
at {:+} dB relative at every frequency — and `if_agc_val` (MSB-first,
vagc configuration, one snapshot 20 ms after the register write) was
read for every path at TF 0000/0000 (stage A, {} readings), then for
every corner code on stage A's two best paths (stage B, {} readings
each). Readings are in dB relative to the bypass path's `ref` reading
at that frequency ({:.1} counts per dB, positive = more signal); a
reading on the meter's rail (nothing passed) is None.

The best path is decided by stage B (the most signal at any corner
code), not by stage A's ranking; it beats the runner-up staged path by
a median {:.1} dB, and by less than the {:.1} dB two-state alternation at
{} of the {} frequencies with two live staged paths (where the choice
between them is inside the reading's noise). A stage-A reading taken 20 ms after a
strong path can still show that path (`stage_a_agrees` says whether
the two rankings coincide: they differ at {} of the {} measured
frequencies; stage A's TF 0000/0000 reading repeated in stage B to a
median {:.1} dB, 90th percentile {:.1} dB, worst {:.1} dB, over {} pairs,
so stage A's 16-path matrix is indicative, not a measurement to
0.5 dB).

Resolution: 1 dB from the single-snapshot reading and the loop's
two-state alternation; the grid is {} MHz, so a segment boundary lies
somewhere between two grid points.

Caveats: a single tone measures each setting's passband gain at the
tone, not its rejection of anything else — the best corner codes read
above the same path's TF 0000/0000 stage-B reading by a median {:.1} dB
(90th percentile {:.1} dB; the worst, {:.1} dB, is a TF 0000/0000 reading
taken first in its block, right after another path's last corner, the
same settling artefact as stage A's), and their out-of-band rejection
is unmeasured (a second-tone step would measure it); the live-path
spread at one frequency is large (median {:.1} dB, up to {:.1} dB), so
the band code matters even with the tracking filter bypassed; image
rejection is uncalibrated (findings.md step 4h: 10-20 dB with
IMR_G = IMR_P = 32){gap}.",
        t.points.len(),
        first / 1_000_000,
        last / 1_000_000,
        rec.grid_step_hz / 1e6,
        t.measured,
        t.readings,
        rec.if_hz / 1e6,
        BYPASS_RFFILT,
        BYPASS_RFMUX,
        t.source_level_db,
        STAGE_A_READINGS,
        STAGE_B_READINGS_PER_PATH,
        scale.counts_per_db,
        pct(&t.margin_db_sorted, 0.5),
        SEGMENT_TOLERANCE_DB,
        t.margin_db_sorted
            .iter()
            .filter(|m| **m < SEGMENT_TOLERANCE_DB)
            .count(),
        t.margin_db_sorted.len(),
        t.stage_a_disagrees,
        t.measured,
        pct(&t.repeat_db_sorted, 0.5),
        pct(&t.repeat_db_sorted, 0.9),
        pct(&t.repeat_db_sorted, 1.0),
        t.repeat_db_sorted.len(),
        rec.grid_step_hz / 1e6,
        pct(&t.corner_gain_db_sorted, 0.5),
        pct(&t.corner_gain_db_sorted, 0.9),
        pct(&t.corner_gain_db_sorted, 1.0),
        pct(&t.spread_db_sorted, 0.5),
        pct(&t.spread_db_sorted, 1.0),
    ));
    s.blank();
    s.line("use super::types::*;");
    s.blank();
    s.doc("The path the readings are relative to.");
    s.line(&format!("pub const BYPASS: FrontPath = FrontPath {{ rffilt: 0b{BYPASS_RFFILT:02b}, rfmux: 0b{BYPASS_RFMUX:02b}, tf_nch: 0b0000, tf_lp: 0b0000 }};"));
    s.doc("The IF the LO sat above the RF by.");
    s.line(&format!("pub const IF_HZ: f64 = {};", f(rec.if_hz, 1)));
    s.blank();
    s.doc(
        "How often each (RFFILT, RFMUX) passed nothing at TF 0000/0000 in
stage A, indexed RFFILT * 4 + RFMUX, over the measured frequencies.",
    );
    let dead: Vec<String> = t.dead_paths.iter().map(|n| n.to_string()).collect();
    s.line(&format!(
        "pub const STAGE_A_DEAD_COUNT: [u16; {STAGE_A_READINGS}] = [{}];",
        dead.join(", ")
    ));
    s.doc("How often each (RFFILT, RFMUX) is the best path by stage B.");
    s.line(&format!(
        "pub const BEST_PATH_COUNT: [((u8, u8), u16); {}] = [",
        t.best_path_counts.len()
    ));
    for ((rffilt, rfmux), n) in &t.best_path_counts {
        s.line(&format!("    ((0b{rffilt:02b}, 0b{rfmux:02b}), {n}),"));
    }
    s.line("];");
    s.blank();
    s.doc("Per grid frequency.");
    s.line(&format!(
        "pub const BAND_GRID: [BandPoint; {}] = [",
        t.points.len()
    ));
    for p in &t.points {
        let entry = match &p.entry {
            Entry::Measured {
                best,
                runner_up,
                margin_db,
                spread_db,
                stage_a_agrees,
                stage_a_db,
                ..
            } => {
                let cells: Vec<String> = stage_a_db.iter().map(|d| opt_f(*d, 1)).collect();
                format!(
                    "BandEntry::Measured {{ best: {}, runner_up: {}, margin_over_runner_up_db: {}, live_path_spread_db: {}, stage_a_agrees: {stage_a_agrees}, stage_a_paths_db_vs_bypass: [{}] }}",
                    path_lit(best),
                    runner_up
                        .as_ref()
                        .map(|r| format!("Some({})", path_lit(r)))
                        .unwrap_or_else(|| "None".into()),
                    opt_f(*margin_db, 1),
                    f(*spread_db, 1),
                    cells.join(", ")
                )
            }
            Entry::Unmeasured(reason) => {
                format!("BandEntry::Unmeasured {{ reason: {} }}", emit::s(reason))
            }
        };
        s.line(&format!(
            "    BandPoint {{ rf_hz: {}, entry: {entry} }},",
            f(p.rf_hz as f64, 1)
        ));
    }
    s.line("];");
    s.blank();
    let seg_lit = |seg: &Segment| {
        let path = match seg.path {
            Some((rffilt, rfmux, nch, lp)) => format!(
                "Some(FrontPath {{ rffilt: 0b{rffilt:02b}, rfmux: 0b{rfmux:02b}, tf_nch: 0b{nch:04b}, tf_lp: 0b{lp:04b} }})"
            ),
            None => "None".into(),
        };
        format!(
            "    BandSegment {{ from_hz: {}, to_hz: {}, path: {path}, grid_points: {} }},",
            f(seg.from_hz as f64, 1),
            f(seg.to_hz as f64, 1),
            seg.points
        )
    };
    s.doc(&format!(
        "Neighbouring grid frequencies merged by (RFFILT, RFMUX) alone, from
the lowest up (corner codes left at 0000/0000), as the staged search
ranked them. Superseded for the driver by
`band_paths::BAND_PATH_SEGMENTS` (findings.md step 4f2): stage A's 20 ms
single snapshot read inside a path change's transient, so it could leave
the best path unstaged, and a single stage-B snapshot could win on an
outlier. The corner codes moved the best path's reading by a median
{:.1} dB. Unmeasured frequencies form their own segments (`path` None).
`from_hz` and `to_hz` are the first and last grid frequencies in the
segment; the true boundary lies between a segment's `to_hz` and the next
segment's `from_hz`.",
        pct(&t.corner_gain_db_sorted, 0.5)
    ));
    s.line(&format!(
        "pub const STAGED_PATH_SEGMENTS: [BandSegment; {}] = [",
        t.path_segments.len()
    ));
    for seg in &t.path_segments {
        s.line(&seg_lit(seg));
    }
    s.line("];");
    s.blank();
    s.doc(&format!(
        "The same, with corner codes: a frequency joins the open segment when
its best path is the segment's and at least one corner code reads
within {:.1} dB (the loop's two-state alternation) of every member's own
best corner reading; the segment's codes are the surviving candidate
with the smallest worst shortfall (ties to the lowest code). At the
{:.1} dB repeatability findings.md step 4f claims, the same rule gives
{} segments: the best corner codes of a single-tone, single-snapshot
measurement are set by the loop's alternation more than by the filter,
so this table is the record's answer, not a resolved one.",
        SEGMENT_TOLERANCE_DB, REPEATABILITY_DB, t.segments_at_repeatability
    ));
    s.line(&format!(
        "pub const SEGMENT_TOLERANCE_DB: f64 = {};",
        f(SEGMENT_TOLERANCE_DB, 1)
    ));
    s.line(&format!(
        "pub const BAND_SEGMENTS: [BandSegment; {}] = [",
        t.segments.len()
    ));
    for seg in &t.segments {
        s.line(&seg_lit(seg));
    }
    s.line("];");
    s.finish()
}
