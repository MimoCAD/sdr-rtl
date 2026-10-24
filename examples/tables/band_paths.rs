//! The every-path band table from data/step4f2-band-paths-run*.jsonl
//! (findings.md step 4f2): per grid frequency each (RFFILT, RFMUX) at TF
//! 0000/0000 relative to the bypass path, the mean over the runs, the
//! path with the most signal, and the segments of neighbouring
//! frequencies sharing it. This is the table the driver takes its path
//! from (src/band.rs).

use crate::emit::{self, Src, f, opt_f};
use crate::records::{BAND_PATHS_RECORDS, BandPathsRun, PathSettle, PathsFreq, Provenance};
use crate::scale::{MeterScale, RAIL_COUNTS};

/// A front-end path: (RFFILT, RFMUX).
type Path2 = (u8, u8);

/// The bypass path the source was re-levelled at and every reading is
/// relative to (the record's procedure line).
pub const BYPASS: Path2 = (0b00, 0b01);
pub const PATHS: usize = 16;
/// A frequency's sequence: the bypass, the 16 paths, the bypass again.
pub const SEQUENCE_LEN: usize = PATHS + 2;
/// A path whose mean sits within this fraction of the meter's rail passed
/// nothing (the dead paths of `path-settle` averaged 8136-8168 of 8191,
/// a few reads off the rail pulling the mean down; a live path at the
/// re-levelled source reads within the -3000..3000 window).
pub const RAIL_FRACTION: f64 = 0.98;
/// A margin under this many times the path reading's run-to-run standard
/// deviation (pooled over every frequency and live path) is a tie: the
/// two paths are not told apart. Where the path with the most signal ties
/// with the dominant path (the one with the most signal at the most
/// frequencies) and the runs did not all pick it on their own, the
/// dominant path is kept, so the table does not follow noise from one
/// grid point to the next.
pub const TIE_SDS: f64 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub rf_hz: u32,
    /// Per path, the mean over the runs of its dB vs bypass.
    pub db: [Option<f64>; PATHS],
    /// The most signal by the mean, and the next.
    pub best: (u8, u8),
    pub runner_up: (u8, u8),
    pub margin_db: f64,
    /// The path the driver takes: `best`, or the dominant path where
    /// `best` beats it by less than the tie.
    pub chosen: (u8, u8),
    /// `chosen` is the dominant path by the tie rule, not `best`.
    pub tie_kept_dominant: bool,
    /// Each run's own best path.
    pub run_best: Vec<(u8, u8)>,
    /// The best path's largest difference between two runs, dB.
    pub best_run_spread_db: f64,
    /// The largest |first bypass - last bypass| over the runs, dB: the
    /// drift across the frequency's ~20 s.
    pub bypass_drift_db: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub from_hz: u32,
    pub to_hz: u32,
    pub path: (u8, u8),
    pub points: usize,
}

pub struct BandPaths {
    pub points: Vec<Point>,
    pub segments: Vec<Segment>,
    pub runs: usize,
    pub readings: usize,
    pub retried_writes: usize,
    pub source_level_db: i32,
    /// The runs' wait after a path write and averaging window, ms.
    pub settle_ms: f64,
    pub average_ms: f64,
    /// `path-settle` read through the same window: the worst standard
    /// deviation, over its (frequency, path) groups, of a transition's
    /// mean, dB; and the groups it is over (repetitions within seconds).
    pub settle_sd_db: f64,
    pub settle_groups: usize,
    /// The runs' own repeatability: each live path's sample variance
    /// across the runs, pooled over every frequency and path, as a
    /// standard deviation in dB; the cells it is over; and the largest
    /// single cell's.
    pub run_sd_db: f64,
    pub run_cells: usize,
    pub run_sd_worst_db: f64,
    pub tie_db: f64,
    pub dominant: (u8, u8),
}

/// The repeatability of a reading taken the runs' way, from `path-settle`:
/// each unstalled transition to a live path averaged over [settle,
/// settle + average) ms; per (frequency, path) the sample standard
/// deviation of those means across repetitions and preceding paths; the
/// worst, in dB, and the number of groups.
fn repeatability(
    settle: &PathSettle,
    settle_ms: f64,
    average_ms: f64,
    scale: &MeterScale,
) -> Result<(f64, usize), String> {
    let mut groups: Vec<((u32, Path2), Vec<f64>)> = Vec::new();
    for tr in settle.transitions.iter().filter(|t| !t.stalled) {
        let v: Vec<f64> = tr
            .t_ms
            .iter()
            .zip(&tr.meter)
            .filter(|(t, _)| **t >= settle_ms && **t < settle_ms + average_ms)
            .filter_map(|(_, m)| *m)
            .collect();
        if v.is_empty() {
            return Err(format!(
                "{}: a transition with no reads in {settle_ms}..{} ms",
                settle.provenance.file,
                settle_ms + average_ms
            ));
        }
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        if dead(mean) {
            continue;
        }
        match groups.iter_mut().find(|(k, _)| *k == (tr.rf_hz, tr.to)) {
            Some((_, g)) => g.push(mean),
            None => groups.push(((tr.rf_hz, tr.to), vec![mean])),
        }
    }
    let sds: Vec<f64> = groups
        .iter()
        .filter(|(_, g)| g.len() > 1)
        .map(|(_, g)| {
            let mu = g.iter().sum::<f64>() / g.len() as f64;
            (g.iter().map(|x| (x - mu) * (x - mu)).sum::<f64>() / (g.len() - 1) as f64).sqrt()
                / scale.counts_per_db
        })
        .collect();
    if sds.is_empty() {
        return Err(format!(
            "{}: no live path measured twice",
            settle.provenance.file
        ));
    }
    Ok((sds.iter().cloned().fold(0.0, f64::max), sds.len()))
}

fn index((rffilt, rfmux): (u8, u8)) -> usize {
    (rffilt * 4 + rfmux) as usize
}

fn path_of(i: usize) -> (u8, u8) {
    ((i / 4) as u8, (i % 4) as u8)
}

fn dead(mean: f64) -> bool {
    mean >= RAIL_FRACTION * RAIL_COUNTS as f64
}

/// One run at one frequency: (each path's dB vs bypass, the bypass drift
/// in dB). The bypass is the mean of the sequence's first and last.
fn run_db(fq: &PathsFreq, scale: &MeterScale) -> Result<([Option<f64>; PATHS], f64), String> {
    if fq.readings.len() != SEQUENCE_LEN {
        return Err(format!(
            "{} Hz: {} path readings, not {SEQUENCE_LEN}",
            fq.rf_hz,
            fq.readings.len()
        ));
    }
    for (i, r) in fq.readings.iter().enumerate() {
        if r.i != i {
            return Err(format!(
                "{} Hz: reading {i} is sequence place {}",
                fq.rf_hz, r.i
            ));
        }
    }
    let (first, last) = (&fq.readings[0], &fq.readings[SEQUENCE_LEN - 1]);
    if (first.rffilt, first.rfmux) != BYPASS || (last.rffilt, last.rfmux) != BYPASS {
        return Err(format!(
            "{} Hz: the sequence does not open and close at the bypass",
            fq.rf_hz
        ));
    }
    if dead(first.mean) || dead(last.mean) {
        return Err(format!("{} Hz: the bypass passed nothing", fq.rf_hz));
    }
    let bypass = (first.mean + last.mean) / 2.0;
    let mut db = [None; PATHS];
    for r in &fq.readings[1..=PATHS] {
        let i = index((r.rffilt, r.rfmux));
        if db[i].is_some() {
            return Err(format!(
                "{} Hz: path ({:02b}, {:02b}) read twice",
                fq.rf_hz, r.rffilt, r.rfmux
            ));
        }
        db[i] = Some(if dead(r.mean) {
            None
        } else {
            Some((bypass - r.mean) / scale.counts_per_db)
        });
    }
    let db = db.map(|d| d.flatten());
    Ok((db, (first.mean - last.mean).abs() / scale.counts_per_db))
}

/// The most signal and the next, as (path, dB) pairs.
fn top_two(db: &[Option<f64>; PATHS]) -> Option<(Path2, f64, Path2, f64)> {
    let mut live: Vec<(usize, f64)> = db
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.map(|d| (i, d)))
        .collect();
    live.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    match live.as_slice() {
        [a, b, ..] => Some((path_of(a.0), a.1, path_of(b.0), b.1)),
        _ => None,
    }
}

pub fn analyse(
    runs: &[BandPathsRun],
    settle: &PathSettle,
    scale: &MeterScale,
) -> Result<BandPaths, String> {
    let first = runs.first().ok_or("no band-paths run")?;
    let grid: Vec<u32> = first.freqs.iter().map(|f| f.rf_hz).collect();
    let mut source_level: Option<i32> = None;
    for run in runs {
        let g: Vec<u32> = run.freqs.iter().map(|f| f.rf_hz).collect();
        if g != grid {
            return Err(format!(
                "{}: its grid differs from {}'s",
                run.provenance.file, first.provenance.file
            ));
        }
        if run.if_hz != first.if_hz
            || run.settle_ms != first.settle_ms
            || run.average_ms != first.average_ms
        {
            return Err(format!(
                "{}: IF, settle or averaging window differs",
                run.provenance.file
            ));
        }
        for fq in &run.freqs {
            match source_level {
                None => source_level = Some(fq.source_level_db),
                Some(l) if l != fq.source_level_db => {
                    return Err(format!(
                        "{} {} Hz: the source level changed to {} dB (was {l})",
                        run.provenance.file, fq.rf_hz, fq.source_level_db
                    ));
                }
                _ => {}
            }
        }
    }
    let mut points = Vec::new();
    // Per (frequency, live path) the sample variance across the runs, dB².
    let mut variances: Vec<f64> = Vec::new();
    for (k, &rf_hz) in grid.iter().enumerate() {
        let per_run: Vec<([Option<f64>; PATHS], f64)> = runs
            .iter()
            .map(|r| run_db(&r.freqs[k], scale))
            .collect::<Result<_, _>>()?;
        if runs.len() > 1 {
            for i in 0..PATHS {
                if let Some(v) = per_run
                    .iter()
                    .map(|(d, _)| d[i])
                    .collect::<Option<Vec<f64>>>()
                {
                    let mu = v.iter().sum::<f64>() / v.len() as f64;
                    variances.push(
                        v.iter().map(|x| (x - mu) * (x - mu)).sum::<f64>() / (v.len() - 1) as f64,
                    );
                }
            }
        }
        // A path counts as live only where it passed in every run.
        let mut db = [None; PATHS];
        for (i, cell) in db.iter_mut().enumerate() {
            let v: Option<Vec<f64>> = per_run.iter().map(|(d, _)| d[i]).collect();
            *cell = v.map(|v| v.iter().sum::<f64>() / v.len() as f64);
        }
        let (best, best_db, runner_up, runner_db) = top_two(&db)
            .ok_or_else(|| format!("{rf_hz} Hz: fewer than two paths passed in every run"))?;
        let run_best: Vec<(u8, u8)> = per_run
            .iter()
            .map(|(d, _)| top_two(d).map(|t| t.0))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{rf_hz} Hz: a run with fewer than two live paths"))?;
        let best_vals: Vec<f64> = per_run.iter().filter_map(|(d, _)| d[index(best)]).collect();
        let spread = best_vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - best_vals.iter().cloned().fold(f64::INFINITY, f64::min);
        points.push(Point {
            rf_hz,
            db: db.map(|d| d.map(|d| emit::round(d, 2))),
            best,
            runner_up,
            margin_db: emit::round(best_db - runner_db, 2),
            chosen: best,
            tie_kept_dominant: false,
            run_best,
            best_run_spread_db: emit::round(spread, 2),
            bypass_drift_db: emit::round(per_run.iter().map(|(_, d)| *d).fold(0.0, f64::max), 2),
        });
    }
    if variances.is_empty() {
        return Err("one run: no run-to-run repeatability, so no tie can be judged".into());
    }
    let run_sd_db = (variances.iter().sum::<f64>() / variances.len() as f64).sqrt();
    let run_sd_worst_db = variances.iter().cloned().fold(0.0, f64::max).sqrt();
    let tie_db = TIE_SDS * run_sd_db;
    // The dominant path: the most signal at the most frequencies (ties
    // between counts to the lower index).
    let mut counts = [0usize; PATHS];
    for p in &points {
        counts[index(p.best)] += 1;
    }
    let dominant = path_of(
        (0..PATHS)
            .max_by(|&a, &b| counts[a].cmp(&counts[b]).then(b.cmp(&a)))
            .unwrap_or(0),
    );
    for p in &mut points {
        // Every run picking the same path on its own is evidence a margin
        // cannot give: with the two truly equal, each run would favour
        // either with even odds.
        let unanimous = p.run_best.iter().all(|b| *b == p.best);
        if p.best != dominant
            && !unanimous
            && let Some(d) = p.db[index(dominant)]
            && let Some(b) = p.db[index(p.best)]
            && b - d < tie_db
        {
            p.chosen = dominant;
            p.tie_kept_dominant = true;
        }
    }
    let mut segments: Vec<Segment> = Vec::new();
    for p in &points {
        match segments.last_mut() {
            Some(s) if s.path == p.chosen => {
                s.to_hz = p.rf_hz;
                s.points += 1;
            }
            _ => segments.push(Segment {
                from_hz: p.rf_hz,
                to_hz: p.rf_hz,
                path: p.chosen,
                points: 1,
            }),
        }
    }
    let (settle_sd_db, settle_groups) =
        repeatability(settle, first.settle_ms, first.average_ms, scale)?;
    Ok(BandPaths {
        points,
        segments,
        runs: runs.len(),
        readings: runs
            .iter()
            .map(|r| r.freqs.iter().map(|f| f.readings.len()).sum::<usize>())
            .sum(),
        retried_writes: runs
            .iter()
            .map(|r| {
                r.freqs
                    .iter()
                    .flat_map(|f| &f.readings)
                    .filter(|r| r.attempts > 1)
                    .count()
            })
            .sum(),
        source_level_db: source_level.ok_or("no frequency")?,
        settle_ms: first.settle_ms,
        average_ms: first.average_ms,
        settle_sd_db: emit::round(settle_sd_db, 2),
        settle_groups,
        run_sd_db: emit::round(run_sd_db, 2),
        run_cells: variances.len(),
        run_sd_worst_db: emit::round(run_sd_worst_db, 2),
        tie_db: emit::round(tie_db, 2),
        dominant,
    })
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        0.0
    } else {
        v[((v.len() - 1) as f64 * q).round() as usize]
    }
}

fn front(p: (u8, u8)) -> String {
    format!(
        "FrontPath {{ rffilt: 0b{:02b}, rfmux: 0b{:02b}, tf_nch: 0b0000, tf_lp: 0b0000 }}",
        p.0, p.1
    )
}

pub fn emit(
    t: &BandPaths,
    runs: &[BandPathsRun],
    settle: &PathSettle,
    scale: &MeterScale,
) -> String {
    let records: Vec<&Provenance> = runs
        .iter()
        .map(|r| &r.provenance)
        .chain([&settle.provenance, &scale.provenance])
        .collect();
    let mut s = Src::new(&records);
    let agreeing = t
        .points
        .iter()
        .filter(|p| p.run_best.iter().all(|b| *b == p.best))
        .count();
    let ties = t.points.iter().filter(|p| p.margin_db < t.tie_db).count();
    let kept = t.points.iter().filter(|p| p.tie_kept_dominant).count();
    let dominant_best = t.points.iter().filter(|p| p.best == t.dominant).count();
    let mut margins: Vec<f64> = t.points.iter().map(|p| p.margin_db).collect();
    let mut spreads: Vec<f64> = t.points.iter().map(|p| p.best_run_spread_db).collect();
    let mut drifts: Vec<f64> = t.points.iter().map(|p| p.bypass_drift_db).collect();
    let first = t.points.first().map_or(0, |p| p.rf_hz);
    let last = t.points.last().map_or(0, |p| p.rf_hz);
    // Each paragraph joined into one line before the doc wrap, so the
    // numbers filled in do not leave short lines behind.
    let paragraphs = |text: String| {
        text.split("\n\n")
            .map(|p| p.replace('\n', " "))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    s.module_doc(&paragraphs(format!(
        "The front-end path per frequency, every path compared: {} grid
frequencies from {} to {} MHz, each (RFFILT, RFMUX) at TF 0000/0000, in
{} runs ({} path readings; {} writes retried after a stall), and the
segments of neighbours sharing the path the driver takes. The driver
takes its path from `BAND_PATH_SEGMENTS` (src/band.rs).

Method (findings.md step 4f2, harness `band-paths`): at each frequency
the LO was programmed (IF {:.2} MHz above the RF), the source re-levelled
once at the bypass path (RFFILT {:02b}, RFMUX {:02b}) and then held — it
stayed at {:+} dB relative at every frequency of every run — and the
bypass, the 16 paths and the bypass again were each written, left {} ms,
and the meter (`if_agc_val`, MSB-first, vagc configuration) averaged over
{} ms of back-to-back reads. `path-settle` (findings.md step 4f2) found a
path change's transient over well inside that wait and the meter
wandering about 1 dB on its own; read through the same window, its
transitions repeat within seconds to {:.2} dB at worst (standard
deviation, over {} frequency-path groups). Across the runs, taken
minutes to hours apart, a path's reading repeats to {:.2} dB (standard
deviation pooled over {} frequency-path cells; the worst cell {:.2}
dB). Readings are in dB relative to the mean of the frequency's two
bypass readings ({:.1} counts per dB, positive = more signal); a path
whose mean sat within {:.0}% of the rail passed nothing.

The path with the most signal is decided by each path's mean over the
runs. A margin under `TIE_DB` ({} times the runs' pooled standard
deviation) is a tie; where the most-signal path ties with the dominant
path ({:02b}, {:02b}) — the most signal at {} of {} frequencies — and the
runs did not all pick it on their own, the dominant path is kept
(`tie_kept_dominant`), so the table does not follow noise from one grid
point to the next. A path every run picked is kept even inside a tie:
were the two equal, each run would favour either with even odds.

Results: the runs' own best paths agree at {} of {} frequencies; the best
path beats the next by a median {:.2} dB (under {:.2} dB, a tie, at {});
the dominant path is kept over a tying one at {}; the best path's
reading differs between runs by a median {:.2} dB, at most {:.2} dB; the
bypass drifted across a frequency's sequence by a median {:.2} dB, at
most {:.2} dB.

Caveats: a single tone measures each path's passband gain at the tone,
not its rejection of anything else; the corner codes (TF_NCH, TF_LP)
are left at 0000/0000, the least-attenuating, and not measured here; the
grid is {} MHz, so a segment boundary lies somewhere between two grid
points.",
        t.points.len(),
        first / 1_000_000,
        last / 1_000_000,
        t.runs,
        t.readings,
        t.retried_writes,
        runs[0].if_hz / 1e6,
        BYPASS.0,
        BYPASS.1,
        t.source_level_db,
        t.settle_ms,
        t.average_ms,
        t.settle_sd_db,
        t.settle_groups,
        t.run_sd_db,
        t.run_cells,
        t.run_sd_worst_db,
        scale.counts_per_db,
        RAIL_FRACTION * 100.0,
        TIE_SDS,
        t.dominant.0,
        t.dominant.1,
        dominant_best,
        t.points.len(),
        agreeing,
        t.points.len(),
        pct(&mut margins, 0.5),
        t.tie_db,
        ties,
        kept,
        pct(&mut spreads.clone(), 0.5),
        pct(&mut spreads, 1.0),
        pct(&mut drifts.clone(), 0.5),
        pct(&mut drifts, 1.0),
        t.points
            .windows(2)
            .map(|w| (w[1].rf_hz - w[0].rf_hz) / 1_000_000)
            .min()
            .unwrap_or(0),
    )));
    s.blank();
    s.line("use super::types::*;");
    s.blank();
    s.doc("The path every reading is relative to.");
    s.line(&format!("pub const BYPASS: FrontPath = {};", front(BYPASS)));
    s.doc("The IF the LO sat above the RF by.");
    s.line(&format!("pub const IF_HZ: f64 = {};", f(runs[0].if_hz, 1)));
    s.doc("The runs the readings are the mean of.");
    s.line(&format!("pub const RUNS: u8 = {};", t.runs));
    s.doc(&format!("A margin under this is a tie: {TIE_SDS} times a path reading's run-to-run standard deviation ({:.2} dB, pooled).", t.run_sd_db));
    s.line(&format!("pub const TIE_DB: f64 = {};", f(t.tie_db, 2)));
    s.doc("The dominant path: the most signal at the most frequencies; kept where the most-signal path ties with it.");
    s.line(&format!(
        "pub const DOMINANT: FrontPath = {};",
        front(t.dominant)
    ));
    s.blank();
    s.doc("Per grid frequency.");
    s.line(&format!(
        "pub const BAND_PATH_GRID: [BandPathPoint; {}] = [",
        t.points.len()
    ));
    for p in &t.points {
        let cells: Vec<String> = p.db.iter().map(|d| opt_f(*d, 2)).collect();
        let agree = p.run_best.iter().filter(|b| **b == p.best).count();
        s.line(&format!(
            "    BandPathPoint {{ rf_hz: {}, paths_db_vs_bypass: [{}], best: {}, margin_over_runner_up_db: {}, runs_agreeing: {agree}, path: {}, tie_kept_dominant: {} }},",
            f(p.rf_hz as f64, 1),
            cells.join(", "),
            front(p.best),
            f(p.margin_db, 2),
            front(p.chosen),
            p.tie_kept_dominant
        ));
    }
    s.line("];");
    s.blank();
    s.doc(
        "Neighbouring grid frequencies sharing the path the driver takes
(`BandPathPoint::path`), from the lowest up (corner codes 0000/0000).
`from_hz` and `to_hz` are the first and last grid frequencies in the
segment; the true boundary lies between a segment's `to_hz` and the next
segment's `from_hz`.",
    );
    s.line(&format!(
        "pub const BAND_PATH_SEGMENTS: [BandSegment; {}] = [",
        t.segments.len()
    ));
    for seg in &t.segments {
        s.line(&format!(
            "    BandSegment {{ from_hz: {}, to_hz: {}, path: Some({}), grid_points: {} }},",
            f(seg.from_hz as f64, 1),
            f(seg.to_hz as f64, 1),
            front(seg.path),
            seg.points
        ));
    }
    s.line("];");
    s.finish()
}

/// Every run, in [`BAND_PATHS_RECORDS`] order.
pub fn load(dir: &std::path::Path) -> Result<Vec<BandPathsRun>, String> {
    BAND_PATHS_RECORDS
        .iter()
        .map(|file| crate::records::load_band_paths(dir, file))
        .collect()
}
