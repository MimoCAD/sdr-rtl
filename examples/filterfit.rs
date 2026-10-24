//! Reads `filterstudy sweep` records and scores each (rate, tap set).
//!
//!   filterfit FILE.jsonl [--grid HZ] [--table]
//!
//! Per sweep: the tone's level where each model lands it — `plain`
//! (folded once at the output rate) or `staged` (folded at 2.4 MHz, then
//! at the output rate), the stronger taken, `floor` when neither stands
//! 6 dB over the median bin — and its level relative to the
//! in-band reference (the median level over |Δ| ≤ 300 kHz). Then:
//! - passband: the response at a few in-band offsets, and the spread
//!   (max − min) over |Δ| ≤ 0.4 × rate and over |Δ| ≤ 1.0 MHz;
//! - aliases: the strongest response with |Δ| beyond the output's
//!   Nyquist, overall and over the part that lands inside ±1.0 MHz, in
//!   dB below the reference (the rejection);
//! - images (rates above 2.4 Msps): for in-band tones, a reported line at
//!   Δ ∓ 2.4 MHz inside the output, its level below the tone.
//!
//! A table of every `--grid` Hz (default 250 kHz) follows each summary.

use std::collections::BTreeMap;

const STAGE_HZ: f64 = 2.4e6;
/// Hz a line may sit from a model's place (~4.2 ppm source vs dongle,
/// plus a bin or two).
const TOL_HZ: f64 = 3.0e3;
/// Off-reference lines closer than this to a line exclude it.
const SPUR_TOL_HZ: f64 = 2.0e3;
/// A model bin this close to the median is no line at all.
const FLOOR_MARGIN_DB: f64 = 6.0;
const REF_SPAN_HZ: f64 = 300e3;
const USABLE_HZ: f64 = 1.0e6;
/// Where aliases are judged: landing inside these.
const INSIDE_HZ: [f64; 6] = [0.6e6, 0.7e6, 0.8e6, 0.9e6, 1.0e6, 1.1e6];
const PASSBAND_PROBES_HZ: [f64; 8] = [-1.1e6, -1.0e6, -0.8e6, -0.5e6, 0.5e6, 0.8e6, 1.0e6, 1.1e6];

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

fn fold(hz: f64, rate: f64) -> f64 {
    hz - (hz / rate).round() * rate
}

struct Point {
    delta: f64,
    hz: f64,
    dbfs: f64,
    model: &'static str,
    lines: Vec<(f64, f64)>,
}

struct Sweep {
    rate: f64,
    set: String,
    off_median: f64,
    off_lines: Vec<(f64, f64)>,
    points: Vec<Point>,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn spread(v: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = v.collect();
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: filterfit FILE.jsonl [--grid HZ]");
        std::process::exit(2);
    };
    let grid: f64 = args
        .iter()
        .position(|a| a == "--grid")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(250e3);
    let data = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let mut sweeps: Vec<Sweep> = Vec::new();
    for l in data.lines() {
        match text(l, "kind").as_deref() {
            Some("sweep_head") => sweeps.push(Sweep {
                rate: num(l, "rate").unwrap_or(0.0),
                set: text(l, "set").unwrap_or_default(),
                off_median: num(l, "off_median_dbfs").unwrap_or(f64::NAN),
                off_lines: pairs(l, "off_lines"),
                points: Vec::new(),
            }),
            Some("sweep") => {
                // A matrix run interleaves the sets: match by rate and set.
                let (rate, set) = (
                    num(l, "rate").unwrap_or(0.0),
                    text(l, "set").unwrap_or_default(),
                );
                let Some(s) = sweeps
                    .iter_mut()
                    .rev()
                    .find(|s| s.rate == rate && s.set == set)
                else {
                    continue;
                };
                let delta = num(l, "delta").unwrap_or(0.0);
                let lines = pairs(l, "lines");
                let floor = num(l, "median_dbfs").unwrap_or(f64::NAN);
                // The tone's level is read where each model lands it (the
                // record's plain / staged bins): the source's own LO leakage
                // (tone − 400 kHz) and the dongle's spurs are other lines.
                let (plain, staged) = (fold(delta, s.rate), fold(fold(delta, STAGE_HZ), s.rate));
                let (pd, sd) = (
                    num(l, "plain_dbfs").unwrap_or(f64::NAN),
                    num(l, "staged_dbfs").unwrap_or(f64::NAN),
                );
                let is_spur = |hz: f64, db: f64| {
                    s.off_lines
                        .iter()
                        .any(|(f, p)| (f - hz).abs() < SPUR_TOL_HZ && (p - db).abs() < 6.0)
                };
                let (hz, dbfs, mut model) = if (plain - staged).abs() < TOL_HZ {
                    (plain, pd, "both")
                } else if pd >= sd {
                    (plain, pd, "plain")
                } else {
                    (staged, sd, "staged")
                };
                if dbfs < floor + FLOOR_MARGIN_DB || is_spur(hz, dbfs) {
                    model = "floor";
                }
                s.points.push(Point {
                    delta,
                    hz,
                    dbfs,
                    model,
                    lines,
                });
            }
            _ => {}
        }
    }
    if args.iter().any(|a| a == "--table") {
        table(&sweeps, grid);
        return;
    }
    for s in &sweeps {
        report(s, grid);
    }
}

/// Every sweep side by side: one row per `grid` step of Δ, one column per
/// sweep, the tone's level in dBFS where the models land it (`.` under
/// the floor).
fn table(sweeps: &[Sweep], grid: f64) {
    let Some(first) = sweeps.first() else { return };
    let names: Vec<String> = sweeps
        .iter()
        .map(|s| {
            s.set
                .rsplit(':')
                .nth(1)
                .map(|f| format!("c{f}"))
                .unwrap_or_else(|| s.set.clone())
        })
        .collect();
    println!("| Δ MHz | {} |", names.join(" | "));
    println!("|---|{}", "---|".repeat(sweeps.len()));
    let mut next = first.points.first().map(|p| p.delta).unwrap_or(0.0);
    for p0 in &first.points {
        if p0.delta + 1.0 < next {
            continue;
        }
        next = p0.delta + grid;
        let cells: Vec<String> = sweeps
            .iter()
            .map(
                |s| match s.points.iter().find(|p| (p.delta - p0.delta).abs() < 1.0) {
                    Some(p) if p.model != "floor" => format!("{:.1}", p.dbfs),
                    Some(_) => ".".into(),
                    None => "".into(),
                },
            )
            .collect();
        println!("| {:+.3} | {} |", p0.delta / 1e6, cells.join(" | "));
    }
}

fn report(s: &Sweep, grid: f64) {
    let reference = median(
        s.points
            .iter()
            .filter(|p| p.delta.abs() <= REF_SPAN_HZ)
            .map(|p| p.dbfs)
            .collect(),
    );
    let rel = |p: &Point| p.dbfs - reference;
    let nyq = s.rate / 2.0;
    println!("## rate {} Msps, set {}", s.rate / 1e6, s.set);
    println!(
        "reference {:.1} dBFS (|Δ| ≤ {} kHz); tone-off median {:.1} dBFS; floor {:.1} dB under the reference",
        reference,
        REF_SPAN_HZ / 1e3,
        s.off_median,
        reference - s.off_median
    );
    let at = |hz: f64| {
        s.points
            .iter()
            .min_by(|a, b| (a.delta - hz).abs().total_cmp(&(b.delta - hz).abs()))
    };
    let probes: Vec<String> = PASSBAND_PROBES_HZ
        .iter()
        .filter(|h| h.abs() < nyq)
        .filter_map(|h| at(*h).map(|p| format!("{:+.1}:{:+.1}", h / 1e6, rel(p))))
        .collect();
    println!("passband (MHz:dB) {}", probes.join(" "));
    println!(
        "spread {:.1} dB over |Δ| ≤ {:.2} MHz; {:.1} dB over |Δ| ≤ 1.0 MHz",
        spread(
            s.points
                .iter()
                .filter(|p| p.delta.abs() <= 0.4 * s.rate)
                .map(rel)
        ),
        0.4 * s.rate / 1e6,
        spread(
            s.points
                .iter()
                .filter(|p| p.delta.abs() <= USABLE_HZ.min(nyq))
                .map(rel)
        )
    );
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in s.points.iter().filter(|p| p.delta.abs() > nyq) {
        *counts.entry(p.model).or_default() += 1;
    }
    println!("beyond Nyquist, landings: {counts:?}");
    let worst = |f: &dyn Fn(&Point) -> bool| {
        s.points
            .iter()
            .filter(|p| p.delta.abs() > nyq && p.model != "floor" && f(p))
            .max_by(|a, b| a.dbfs.total_cmp(&b.dbfs))
    };
    if let Some(p) = worst(&|_| true) {
        println!(
            "worst alias {:.1} dB down: Δ {:+.3} MHz lands at {:+.3} MHz",
            -rel(p),
            p.delta / 1e6,
            p.hz / 1e6
        );
    }
    for inside in INSIDE_HZ {
        if let Some(p) = worst(&|p| p.hz.abs() <= inside) {
            let staged: Vec<f64> = s
                .points
                .iter()
                .filter(|p| p.delta.abs() > nyq && p.model == "staged" && p.hz.abs() <= inside)
                .map(|p| -rel(p))
                .collect();
            println!(
                "inside ±{:.1} MHz: worst alias {:.1} dB down (Δ {:+.3} → {:+.3} MHz); median of the 2.4 MHz-stage aliases {:.1} dB down (n {})",
                inside / 1e6,
                -rel(p),
                p.delta / 1e6,
                p.hz / 1e6,
                median(staged.clone()),
                staged.len()
            );
        }
    }
    if s.rate > STAGE_HZ {
        let mut found = Vec::new();
        for p in s
            .points
            .iter()
            .filter(|p| p.delta.abs() < STAGE_HZ / 2.0 && p.model != "floor")
        {
            let image = p.delta - STAGE_HZ * p.delta.signum();
            if image.abs() >= nyq {
                continue;
            }
            let level = p
                .lines
                .iter()
                .filter(|(hz, _)| (hz - image).abs() < TOL_HZ)
                .map(|(_, db)| *db)
                .fold(f64::NAN, f64::max);
            found.push((p.delta, image, p.dbfs - level));
        }
        let seen: Vec<String> = found
            .iter()
            .filter(|f| f.2.is_finite())
            .map(|(d, i, r)| format!("{:+.2}→{:+.2}:{:.1}", d / 1e6, i / 1e6, r))
            .collect();
        println!(
            "images (Δ→image MHz: dB under the tone), {} of {} in-band tones with a reported image line: {}",
            seen.len(),
            found.len(),
            seen.join(" ")
        );
    }
    println!("| Δ MHz | lands MHz | model | dB re ref |");
    println!("|---|---|---|---|");
    let mut next = s.points.first().map(|p| p.delta).unwrap_or(0.0);
    for p in &s.points {
        if p.delta + 1.0 >= next {
            println!(
                "| {:+.3} | {:+.3} | {} | {:+.1} |",
                p.delta / 1e6,
                p.hz / 1e6,
                p.model,
                rel(p)
            );
            next = p.delta + grid;
        }
    }
    println!();
}
