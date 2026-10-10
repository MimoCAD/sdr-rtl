//! Fits the single-field sweeps (`filterstudy matrix … --set single:K:V`)
//! to a symmetric FIR model.
//!
//!   tapfit FILE.jsonl [--max-hz HZ]
//!   tapfit --predict FOLD.jsonl [--fs HZ] [--narrow W]
//!   tapfit --residual FOLD.jsonl [--fs HZ] [--narrow W] [--grid HZ]
//!
//! Model: field k drives taps k and L − 1 − k of an L-tap symmetric FIR
//! at Fs, so one field alone answers 2·c·|cos(π f (L − 1 − 2k) / Fs)|,
//! times a response G(f) every field shares (the rest of the chain).
//! Dividing each field's sweep by field 15's cancels G; the ratio is
//! fitted over Fs (a grid), L (31, 32, 33) and one scale per field, in
//! dB, points within 25 dB of a model null left out. Reported: the best
//! (L, Fs) and the residual, each field's scale per unit field value
//! (the LSB weight), and G(f) recovered from field 15.

use std::collections::BTreeMap;
use std::f64::consts::PI;

const FIELDS: usize = 16;
const NEAR_NULL_DB: f64 = -25.0;
const LENGTHS: [usize; 3] = [31, 32, 33];

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

/// Spacing between the two taps field k drives.
fn spacing(len: usize, k: usize) -> f64 {
    (len as f64 - 1.0) - 2.0 * k as f64
}

/// |cos(π f s / Fs)| in dB, with the odd-length centre tap (spacing 0)
/// flat.
fn pair_db(f: f64, s: f64, fs: f64) -> f64 {
    20.0 * (PI * f * s / fs).cos().abs().max(1e-6).log10()
}

/// |H(f)| of a 32-tap symmetric FIR from the 16 fields at Fs, the 8-bit
/// fields weighted `narrow`.
fn fir_db(fields: &[i32; FIELDS], f: f64, fs: f64, narrow: f64) -> f64 {
    let h: f64 = (0..FIELDS)
        .map(|k| {
            let w = if k < FIELDS / 2 { narrow } else { 1.0 };
            2.0 * w * fields[k] as f64 * (PI * f * spacing(32, k) / fs).cos()
        })
        .sum();
    20.0 * h.abs().max(1e-9).log10()
}

/// `--predict FOLD.jsonl`: each set's aliases beyond 1.2 MHz predicted as
/// the in-band level where the alias lands plus |H(Δ)| − |H(landing)| of
/// the FIR model (the FIR ahead of the 2.4 MHz stage, the rest shared).
fn predict(path: &str, fs: f64, narrow: f64) {
    let data = std::fs::read_to_string(path).unwrap_or_default();
    let mut fields_of: BTreeMap<String, [i32; FIELDS]> = BTreeMap::new();
    let mut inband: BTreeMap<(i64, String), BTreeMap<i64, f64>> = BTreeMap::new();
    let mut rows = Vec::new();
    for l in data.lines() {
        let (Some(set), Some(rate)) = (text(l, "set"), num(l, "rate")) else { continue };
        if l.contains("\"kind\":\"sweep_head\"") {
            let Some(h) = text(l, "hex") else { continue };
            let mut r = [0u8; 20];
            for (k, b) in r.iter_mut().enumerate() {
                *b = u8::from_str_radix(&h[2 * k..2 * k + 2], 16).unwrap_or(0);
            }
            fields_of.insert(set, sdr_rtl::taps::unpack(&r));
            continue;
        }
        let (Some(d), Some(st), Some(sd)) = (num(l, "delta"), num(l, "staged_hz"), num(l, "staged_dbfs")) else { continue };
        if d.abs() < rate / 2.0 - 25e3 {
            inband.entry((rate as i64, set.clone())).or_default().insert(d as i64, num(l, "plain_dbfs").unwrap_or(f64::NAN));
        } else if d.abs() > 1.25e6 {
            rows.push((rate, set, d, st, sd, num(l, "median_dbfs").unwrap_or(f64::NAN)));
        }
    }
    // Model A (the FIR, then a 2.4 MHz fold): the in-band level at the
    // landing plus |H(Δ)| − |H(landing)|. Model B (the tone moved by
    // n × 2.4 MHz ahead of the FIR): the in-band level at the landing
    // less a constant — the constant is the mean, the spread its rms.
    let mut a: BTreeMap<(i64, String, bool), Vec<f64>> = BTreeMap::new();
    let mut b: BTreeMap<(i64, String, bool), Vec<f64>> = BTreeMap::new();
    for (rate, set, d, land, measured, floor) in rows {
        let (Some(fields), Some(ib)) = (fields_of.get(&set), inband.get(&(rate as i64, set.clone()))) else { continue };
        let Some((_, l_land)) = ib.iter().min_by_key(|(k, _)| (**k - land as i64).abs()) else { continue };
        let predicted = l_land + fir_db(fields, d, fs, narrow) - fir_db(fields, land, fs, narrow);
        let key = (rate as i64, set, d > 0.0);
        if measured > floor + 10.0 && predicted > floor + 10.0 {
            a.entry(key.clone()).or_default().push(measured - predicted);
        }
        if measured > floor + 10.0 {
            b.entry(key).or_default().push(measured - l_land);
        }
    }
    let stats = |v: &[f64]| {
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        (mean, (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt())
    };
    println!("aliases beyond ±1.25 MHz; A: FIR then fold (narrow weight {narrow:.3}), B: shift ahead of the FIR — mean and spread of measured − model (dB):");
    for (key, va) in &a {
        let (ma, sa) = stats(va);
        let (mb, sb) = b.get(key).map(|v| stats(v)).unwrap_or((f64::NAN, f64::NAN));
        println!(
            "  {:.3} Msps {} Δ{}: A n {} mean {ma:+.2} sd {sa:.2} | B n {} level {mb:+.2} sd {sb:.2}",
            key.0 as f64 / 1e6,
            key.1,
            if key.2 { ">0" } else { "<0" },
            va.len(),
            b.get(key).map(Vec::len).unwrap_or(0)
        );
    }
}

/// `--residual FOLD.jsonl`: per sweep point, the measured response (the
/// tone's level where it lands, re the sweep's in-band reference) less
/// the FIR model's |H(Δ)| − |H(0)|: what the rest of the chain does to a
/// tone at Δ. If the FIR model is the whole of the taps' effect, this is
/// the same for every tap set. One row per `grid` of Δ, one column per
/// set, a table per rate; `.` where the tone is under the floor.
fn residual(path: &str, fs: f64, narrow: f64, grid: f64) {
    let data = std::fs::read_to_string(path).unwrap_or_default();
    let mut fields_of: BTreeMap<String, [i32; FIELDS]> = BTreeMap::new();
    // (rate, set) → Δ → (dBFS, floor)
    let mut pts: BTreeMap<(i64, String), BTreeMap<i64, (f64, f64)>> = BTreeMap::new();
    for l in data.lines() {
        let (Some(set), Some(rate)) = (text(l, "set"), num(l, "rate")) else { continue };
        if l.contains("\"kind\":\"sweep_head\"") {
            let Some(h) = text(l, "hex") else { continue };
            let mut r = [0u8; 20];
            for (k, b) in r.iter_mut().enumerate() {
                *b = u8::from_str_radix(&h[2 * k..2 * k + 2], 16).unwrap_or(0);
            }
            fields_of.insert(set, sdr_rtl::taps::unpack(&r));
            continue;
        }
        let (Some(d), Some(p), Some(s), Some(fl)) = (num(l, "delta"), num(l, "plain_dbfs"), num(l, "staged_dbfs"), num(l, "median_dbfs")) else { continue };
        pts.entry((rate as i64, set)).or_default().insert(d as i64, (p.max(s), fl));
    }
    let rates: Vec<i64> = pts.keys().map(|k| k.0).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    for rate in rates {
        let sets: Vec<&String> = pts.keys().filter(|k| k.0 == rate).map(|k| &k.1).collect();
        println!("## {:.3} Msps: rest of the chain (dB re 0 Hz), measured − FIR model", rate as f64 / 1e6);
        println!("| Δ MHz | {} |", sets.iter().map(|s| s.chars().take(12).collect::<String>()).collect::<Vec<_>>().join(" | "));
        println!("|---|{}", "---|".repeat(sets.len()));
        let refs: Vec<f64> = sets
            .iter()
            .map(|s| {
                let m = &pts[&(rate, (*s).clone())];
                let v: Vec<f64> = m.iter().filter(|(d, _)| d.abs() <= 300_000).map(|(_, (x, _))| *x).collect();
                v.iter().sum::<f64>() / v.len().max(1) as f64
            })
            .collect();
        let deltas: Vec<i64> = pts[&(rate, sets[0].clone())].keys().copied().collect();
        let mut next = deltas.first().copied().unwrap_or(0) as f64;
        for d in deltas {
            if (d as f64) + 1.0 < next {
                continue;
            }
            next = d as f64 + grid;
            let cells: Vec<String> = sets
                .iter()
                .zip(&refs)
                .map(|(s, r)| match (pts[&(rate, (*s).clone())].get(&d), fields_of.get(*s)) {
                    (Some((x, fl)), Some(f)) if *x > fl + 6.0 => format!("{:+.1}", (x - r) - (fir_db(f, d as f64, fs, narrow) - fir_db(f, 0.0, fs, narrow))),
                    (Some(_), _) => ".".into(),
                    _ => "".into(),
                })
                .collect();
            println!("| {:+.3} | {} |", d as f64 / 1e6, cells.join(" | "));
        }
        println!();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--residual") {
        let fs: f64 = args.iter().position(|a| a == "--fs").and_then(|j| args.get(j + 1)).and_then(|v| v.parse().ok()).unwrap_or(28.8e6);
        let narrow: f64 = args.iter().position(|a| a == "--narrow").and_then(|j| args.get(j + 1)).and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let grid: f64 = args.iter().position(|a| a == "--grid").and_then(|j| args.get(j + 1)).and_then(|v| v.parse().ok()).unwrap_or(200e3);
        residual(args.get(i + 1).map(String::as_str).unwrap_or(""), fs, narrow, grid);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--predict") {
        let fs: f64 = args.iter().position(|a| a == "--fs").and_then(|j| args.get(j + 1)).and_then(|v| v.parse().ok()).unwrap_or(28.8e6);
        let narrow: f64 = args.iter().position(|a| a == "--narrow").and_then(|j| args.get(j + 1)).and_then(|v| v.parse().ok()).unwrap_or(1.0);
        predict(args.get(i + 1).map(String::as_str).unwrap_or(""), fs, narrow);
        return;
    }
    let Some(path) = args.first() else {
        eprintln!("usage: tapfit FILE.jsonl [--max-hz HZ]");
        std::process::exit(2);
    };
    let max_hz: f64 = args.iter().position(|a| a == "--max-hz").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(1.15e6);
    let data = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    // level[k][Δ] in dBFS, and each field's value.
    let mut level: Vec<BTreeMap<i64, f64>> = vec![BTreeMap::new(); FIELDS];
    let mut value = [0f64; FIELDS];
    for l in data.lines().filter(|l| l.contains("\"kind\":\"sweep\"")) {
        let Some(set) = text(l, "set") else { continue };
        let parts: Vec<&str> = set.split(':').collect();
        let (Some(k), Some(v)) = (parts.get(1).and_then(|s| s.parse::<usize>().ok()), parts.get(2).and_then(|s| s.parse::<f64>().ok())) else { continue };
        let (Some(d), Some(db)) = (num(l, "delta"), num(l, "plain_dbfs")) else { continue };
        if d.abs() <= max_hz && k < FIELDS {
            level[k].insert(d as i64, db);
            value[k] = v;
        }
    }
    let reference = FIELDS - 1;
    let deltas: Vec<i64> = level[reference].keys().copied().collect();
    let mut best: Option<(f64, usize, f64, [f64; FIELDS])> = None;
    for len in LENGTHS {
        let mut fs = 20.0e6;
        while fs <= 40.0e6 {
            let (mut sse, mut n) = (0.0, 0usize);
            let mut scales = [0f64; FIELDS];
            for k in 0..FIELDS {
                let mut resid = Vec::new();
                for &d in &deltas {
                    let (Some(a), Some(r)) = (level[k].get(&d), level[reference].get(&d)) else { continue };
                    let f = d as f64;
                    let model = pair_db(f, spacing(len, k), fs) - pair_db(f, spacing(len, reference), fs);
                    if pair_db(f, spacing(len, k), fs) < NEAR_NULL_DB {
                        continue;
                    }
                    resid.push((a - r) - model);
                }
                if resid.is_empty() {
                    continue;
                }
                let scale = resid.iter().sum::<f64>() / resid.len() as f64;
                scales[k] = scale;
                sse += resid.iter().map(|x| (x - scale).powi(2)).sum::<f64>();
                n += resid.len();
            }
            let rms = (sse / n.max(1) as f64).sqrt();
            if best.as_ref().is_none_or(|b| rms < b.0) {
                best = Some((rms, len, fs, scales));
            }
            fs += 0.05e6;
        }
        if let Some(b) = &best
            && b.1 == len
        {
            println!("L {len}: best Fs {:.2} MHz, residual {:.2} dB rms", b.2 / 1e6, b.0);
        }
    }
    // The per-length bests, each re-searched (the loop above keeps only
    // the overall best), for the record.
    for len in LENGTHS {
        let mut b: Option<(f64, f64)> = None;
        let mut fs = 20.0e6;
        while fs <= 40.0e6 {
            let (mut sse, mut n) = (0.0, 0usize);
            for k in 0..FIELDS {
                let mut resid = Vec::new();
                for &d in &deltas {
                    let (Some(a), Some(r)) = (level[k].get(&d), level[reference].get(&d)) else { continue };
                    let f = d as f64;
                    if pair_db(f, spacing(len, k), fs) < NEAR_NULL_DB {
                        continue;
                    }
                    resid.push((a - r) - (pair_db(f, spacing(len, k), fs) - pair_db(f, spacing(len, reference), fs)));
                }
                if resid.is_empty() {
                    continue;
                }
                let m = resid.iter().sum::<f64>() / resid.len() as f64;
                sse += resid.iter().map(|x| (x - m).powi(2)).sum::<f64>();
                n += resid.len();
            }
            let rms = (sse / n.max(1) as f64).sqrt();
            if b.is_none_or(|x| rms < x.0) {
                b = Some((rms, fs));
            }
            fs += 0.05e6;
        }
        if let Some((rms, fs)) = b {
            println!("length {len}: Fs {:.2} MHz, residual {rms:.2} dB rms", fs / 1e6);
        }
    }
    let Some((rms, len, fs, scales)) = best else { return };
    println!("best: L {len}, Fs {:.2} MHz, residual {rms:.2} dB rms", fs / 1e6);
    println!("field weights (dB re field 15, per unit value; 0 = the same LSB weight):");
    for k in 0..FIELDS {
        let per_unit = scales[k] - 20.0 * (value[k] / value[reference]).log10();
        println!("  c{k}: value {} spacing {} → {per_unit:+.2} dB", value[k], spacing(len, k));
    }
    println!("G(f), the shared response (dB re 0 Hz), from field 15 over its pair term:");
    let g0 = level[reference].get(&0).copied().unwrap_or(0.0);
    for (&d, &a) in &level[reference] {
        if d % 100_000 == 0 {
            println!("  {:+.2} MHz: {:+.2}", d as f64 / 1e6, a - g0 - pair_db(d as f64, spacing(len, reference), fs));
        }
    }
}
