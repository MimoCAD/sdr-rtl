//! Step 8: watching the replayed FIFO pattern over time
//! (`harness fifo-watch`).
//!
//! In the zero-if-open held state the endpoint replays the FIFO memory's
//! 3,760 bytes (20 x 188) at the bus's rate, as in low-if-open; but its
//! solo tone sweep (data/step8-scan-zero-if-open.jsonl, header) showed the
//! pattern's statistics change a few times in ~12 s, where in low-if the
//! pattern stayed the same through a power session. So something writes
//! new bytes into the FIFO memory now and then. This command measures
//! when (with the tone off and on, alternating) and what: every 100 ms
//! window's first 3,760 bytes are aligned by rotation to the previous
//! window's pattern, and the bytes that differ are recorded as spans, with
//! their bytes and the transport-stream test (0x47, the MPEG-2 sync byte,
//! at a fixed column every 188 bytes: [RTL2832U] Table 34 counts the FIFO
//! in 188-byte TS blocks, §7.11 "a standard Transport Stream").

use std::time::Instant;

use crate::baselines::{self, Baseline};
use crate::content::{self, Content};
use crate::json::{J, hex_bytes};
use crate::source::{self, Source};
use crate::step3;
use crate::usb;

pub const USAGE: &str = "fifo-watch [--baseline B] [--seconds S] [--record NAME]";

/// The MPEG-2 transport-stream packet's sync byte (ISO/IEC 13818-1).
pub const TS_SYNC: u8 = 0x47;
/// Spans closer than this are merged into one.
const SPAN_MERGE_GAP: usize = 8;
/// Bytes of one changed span kept in the record.
const SPAN_KEEP: usize = 2 * step3::BLOCK;

/// The rotation s that best maps `new` onto `prev` (new[(i + s) % N] ==
/// prev[i]) and its match count.
fn align(prev: &[u8], new: &[u8]) -> (usize, usize) {
    let n = prev.len();
    let mut best = (0, 0);
    for s in 0..n {
        let m = (0..n).filter(|&i| new[(i + s) % n] == prev[i]).count();
        if m > best.1 {
            best = (s, m);
        }
    }
    best
}

/// For each column c of 188, the rows (of the 20) holding 0x47 there; the
/// best column and its count.
fn ts_columns(p: &[u8]) -> (usize, usize) {
    let rows = p.len() / step3::BLOCK;
    (0..step3::BLOCK)
        .map(|c| {
            (
                c,
                (0..rows)
                    .filter(|r| p[r * step3::BLOCK + c] == TS_SYNC)
                    .count(),
            )
        })
        .max_by_key(|x| x.1)
        .unwrap_or((0, 0))
}

/// The differing positions grouped into spans (start, len), gaps under
/// SPAN_MERGE_GAP merged.
fn spans(a: &[u8], b: &[u8]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for i in (0..a.len()).filter(|&i| a[i] != b[i]) {
        match out.last_mut() {
            Some((s, l)) if i < *s + *l + SPAN_MERGE_GAP => *l = i + 1 - *s,
            _ => out.push((i, 1)),
        }
    }
    out
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (mut baseline, mut secs, mut record) = (
        Baseline::ZeroIfOpen,
        10u64,
        "step8-fifo-watch.json".to_string(),
    );
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).ok_or(USAGE)?;
        match args[i].as_str() {
            "--baseline" => baseline = Baseline::parse(v)?,
            "--seconds" => secs = v.parse().map_err(|e| format!("--seconds {v}: {e}"))?,
            "--record" => record = v.clone(),
            a => return Err(format!("{a}: {USAGE}")),
        }
        i += 2;
    }
    usb::guard()?;
    let n = content::PERIOD;
    let rf = baseline.ddc_centre_rf_hz();
    // Off and on alternating; the on states at the positive control's RF
    // and two offsets from it.
    let mut plan: Vec<(String, Option<f64>)> = vec![("tone off".into(), None)];
    if source::command().is_some() {
        for o in [0.0, -200e3, 1.0e6] {
            plan.push((format!("tone at {:.4} MHz", (rf + o) / 1e6), Some(rf + o)));
            plan.push(("tone off".into(), None));
        }
    } else {
        plan.push(("tone off".into(), None));
        plan.push(("tone off".into(), None));
    }
    let mut rec = crate::header_with_source("8-fifo-watch", "the replayed FIFO pattern window by window in a held-open state: when it changes and what the new bytes are")
        .with("baseline_definition", baseline.definition())
        .with(
            "method",
            format!(
                "per 100 ms window: bytes/s, the period score (content.rs), and the window's first {n} bytes aligned by rotation to the previous window's pattern (the rotation with the most equal bytes); the bytes that still differ grouped into spans (gaps under {SPAN_MERGE_GAP} merged), each with up to {SPAN_KEEP} of its new bytes and its 0x47 count; the pattern's best 0x47 column (rows of 188 holding 0x47 at one column, of 20). States of {secs} s each, tone off and on alternating"
            ),
        );
    let mut src = match source::command() {
        Some(_) => Some(Source::open()?),
        None => None,
    };
    if let Some(s) = &src {
        rec.set("source", s.describe());
    }
    let (rtl, mut bulk, setup) = baseline.fresh()?;
    rec.set("setup", setup);
    let start = Instant::now();
    let mut prev: Option<Vec<u8>> = None;
    let mut states = J::Arr(Vec::new());
    let mut first_pattern = J::Null;
    for (name, hz) in &plan {
        let s = match (&mut src, hz) {
            (Some(src), Some(h)) => src.level(*h, baselines::CONTROL_LEVEL)?,
            (Some(src), None) => src.off()?,
            _ => J::from("no source"),
        };
        let mut windows = J::Arr(Vec::new());
        let (mut events, mut changed_bytes) = (0usize, 0usize);
        let t_state = Instant::now();
        while t_state.elapsed().as_secs() < secs {
            let mut data = Vec::new();
            let w = bulk.measure(step3::WINDOW, Some(&mut data));
            let c = Content::of(&data, w.elapsed.as_secs_f64());
            let mut line = J::obj()
                .with("t_s", (start.elapsed().as_secs_f64() * 1e3).round() / 1e3)
                .with("c", c.compact());
            if data.len() >= 2 * n {
                let q = &data[..n];
                match &prev {
                    None => {
                        let (col, rows) = ts_columns(q);
                        first_pattern = J::obj()
                            .with("bytes_0x47", q.iter().filter(|b| **b == TS_SYNC).count())
                            .with("best_0x47_column", col)
                            .with("rows_with_0x47_there", rows);
                        prev = Some(q.to_vec());
                    }
                    Some(p) => {
                        let (shift, matched) = align(p, q);
                        let norm: Vec<u8> = (0..n).map(|i| q[(i + shift) % n]).collect();
                        let sp = spans(p, &norm);
                        if !sp.is_empty() {
                            events += 1;
                            let diff = n - matched;
                            changed_bytes += diff;
                            let (col, rows) = ts_columns(&norm);
                            line.set("differing_bytes", diff);
                            line.set(
                                "spans",
                                J::Arr(
                                    sp.iter()
                                        .map(|&(s, l)| {
                                            let b = &norm[s..s + l];
                                            J::obj()
                                                .with("start", s)
                                                .with("len", l)
                                                .with(
                                                    "bytes_0x47",
                                                    b.iter().filter(|x| **x == TS_SYNC).count(),
                                                )
                                                .with(
                                                    "new_bytes",
                                                    hex_bytes(&b[..l.min(SPAN_KEEP)]),
                                                )
                                        })
                                        .collect(),
                                ),
                            );
                            line.set(
                                "pattern_best_0x47_column_after",
                                J::Arr(vec![J::from(col), J::from(rows)]),
                            );
                            println!(
                                "{name} t={:.1}s: {diff} bytes differ in {} spans; 0x47 column {col}: {rows}/20",
                                start.elapsed().as_secs_f64(),
                                sp.len()
                            );
                        }
                        prev = Some(norm);
                    }
                }
            }
            windows.push(line);
        }
        println!("{name}: {events} windows with a changed pattern, {changed_bytes} bytes");
        states.push(
            J::obj()
                .with("state", name.as_str())
                .with("rf_hz", *hz)
                .with("source", s)
                .with("windows_with_change", events)
                .with("changed_bytes", changed_bytes)
                .with("windows", windows),
        );
    }
    if let Some(s) = src {
        s.quit();
    }
    bulk.close();
    drop(rtl);
    rec.set("first_pattern", first_pattern);
    rec.set("states", states);
    crate::write_record(&record, &rec);
    Ok(())
}
