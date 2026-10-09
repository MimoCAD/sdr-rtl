//! Confirming a scan result from any baseline: one demodulator byte
//! written on top of the baseline, then
//!
//! - (a) rate tracking: from a fresh power-on and the baseline at each of
//!   2.4, 1.2 and 2.048 Msps (rsamp_ratio by the 9.4 formula), a 100 ms
//!   control window, the write, `--windows` 100 ms windows with every byte
//!   kept, the endpoint FIFO's dropped-block counter ([RTL2832U] Table 34,
//!   USB_EPA_FIFO_CFG[31:24], 188-byte blocks dropped because the FIFO was
//!   full) after each window, the restore, three windows after it;
//! - (b) the bytes' statistics per state: length mod 188 and the 188-byte
//!   column structure (Table 34's block size), per-channel statistics of
//!   even/odd bytes (as interleaved unsigned I/Q), and an averaged
//!   spectrum read both as complex I/Q pairs and as one real sequence
//!   (peaks in cycles per sample, so no rate is assumed);
//! - (c) with `--source-cmd` (low-if only): the same at 2.4 Msps with the
//!   tone off, at the tuned RF + 200 kHz, and at the tuned RF - 300 kHz
//!   (a peak that moves with the tone, by the right amount and sign, is
//!   the RF);
//! - (d) minimisation: each baseline item removed in turn (fresh power-on
//!   each), the write kept, five 100 ms windows.

use std::time::Duration;

use crate::baselines::{self, Baseline};
use crate::content::{self, Content};
use crate::json::{J, hex8, hex32, hex_bytes};
use crate::regs::*;
use crate::source::{self, Source};
use crate::step3;
use crate::usb::{self, BulkCounter, Rtl};

pub const USAGE: &str = "confirm-write PAGE OFFSET VALUE [--baseline zero-if|low-if|power-on] [--windows N] [--buf BYTES] [--no-minimise] [--out PATH] [--dump DIR]";

/// `--dump DIR`: each state's raw bytes to DIR/NAME.bin (scratch, never
/// under derivation/data: the record keeps the analysis).
fn dump(dir: &Option<String>, name: &str, data: &[u8]) {
    if let Some(d) = dir {
        let _ = std::fs::create_dir_all(d);
        let p = format!("{d}/{name}.bin");
        if let Err(e) = std::fs::write(&p, data) {
            eprintln!("dump {p}: {e}");
        }
    }
}

/// The tone offsets from the tuned RF for (c): one above, one below, of
/// different sizes, so the sign and the scale of a moving peak both show.
pub const TONE_OFFSETS_HZ: [f64; 2] = [200e3, -300e3];
/// FFT length and the most segments averaged.
const FFT_N: usize = 4096;
const FFT_SEGMENTS: usize = 64;
/// Peaks reported per spectrum, and the bins around a peak excluded from
/// the next.
const PEAKS: usize = 5;
const PEAK_GUARD_BINS: usize = 4;
/// Bytes of each state kept verbatim in the record.
const EXCERPT: usize = 256;

fn num(s: &str) -> Result<u32, String> {
    match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16).map_err(|e| format!("{s}: {e}")),
        None => s.parse().map_err(|e| format!("{s}: {e}")),
    }
}

/// USB_EPA_FIFO_CFG's dropped-block counter, bits 31:24 (Table 34).
fn dropped(rtl: &Rtl) -> Option<u32> {
    rtl.usb_read32(USB_EPA_FIFO_CFG).ok().map(|v| v >> EPA_FIFO_DROP_SHIFT)
}

/// In-place radix-2 FFT.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// An averaged, Hann-windowed power spectrum of `data` read as complex
/// interleaved unsigned I/Q (`complex`) or as one real unsigned sequence;
/// the mean removed. Peaks in cycles per sample (−0.5..0.5 complex,
/// 0..0.5 real) with their power over the median bin in dB.
fn spectrum(data: &[u8], complex: bool) -> J {
    let per = if complex { 2 } else { 1 };
    let seg_bytes = FFT_N * per;
    let segs = (data.len() / seg_bytes).min(FFT_SEGMENTS);
    if segs == 0 {
        return J::from("too few bytes");
    }
    let mean = |phase: usize| -> f64 {
        let v: Vec<f64> = data[..segs * seg_bytes].iter().skip(phase).step_by(per).map(|b| *b as f64).collect();
        v.iter().sum::<f64>() / v.len() as f64
    };
    let (mi, mq) = (mean(0), if complex { mean(1) } else { 0.0 });
    let win: Vec<f64> = (0..FFT_N).map(|k| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / FFT_N as f64).cos()).collect();
    let mut power = vec![0f64; FFT_N];
    for s in 0..segs {
        let d = &data[s * seg_bytes..(s + 1) * seg_bytes];
        let mut re = vec![0f64; FFT_N];
        let mut im = vec![0f64; FFT_N];
        for k in 0..FFT_N {
            if complex {
                re[k] = (d[2 * k] as f64 - mi) * win[k];
                im[k] = (d[2 * k + 1] as f64 - mq) * win[k];
            } else {
                re[k] = (d[k] as f64 - mi) * win[k];
            }
        }
        fft(&mut re, &mut im);
        for k in 0..FFT_N {
            power[k] += re[k] * re[k] + im[k] * im[k];
        }
    }
    let bins: Vec<usize> = if complex { (0..FFT_N).collect() } else { (0..=FFT_N / 2).collect() };
    let mut sorted: Vec<f64> = bins.iter().map(|&k| power[k]).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2].max(f64::MIN_POSITIVE);
    let mut taken: Vec<usize> = Vec::new();
    let mut peaks = J::Arr(Vec::new());
    for _ in 0..PEAKS {
        let best = bins
            .iter()
            .copied()
            .filter(|&k| taken.iter().all(|&t| (k as i64 - t as i64).unsigned_abs() as usize > PEAK_GUARD_BINS && (FFT_N - (k as i64 - t as i64).unsigned_abs() as usize) > PEAK_GUARD_BINS))
            .max_by(|&a, &b| power[a].partial_cmp(&power[b]).unwrap_or(std::cmp::Ordering::Equal));
        let Some(k) = best else { break };
        taken.push(k);
        let f = if k < FFT_N / 2 { k as f64 } else { k as f64 - FFT_N as f64 } / FFT_N as f64;
        peaks.push(J::obj().with("bin", k).with("cycles_per_sample", f).with("db_over_median", 10.0 * (power[k] / median).log10()));
    }
    J::obj()
        .with("read_as", if complex { "complex: even bytes I, odd bytes Q, unsigned" } else { "real: every byte one sample, unsigned" })
        .with("fft_n", FFT_N)
        .with("segments", segs)
        .with("mean_removed", J::Arr(vec![J::from(mi), J::from(mq)]))
        .with("peaks", peaks)
}

/// Bytes and periods the periodicity test uses.
const PERIOD_SPAN: usize = 32 * 1024;
const PERIOD_MAX: usize = 16 * 1024;
const PERIOD_MATCH: f64 = 0.99;

/// The smallest period P in 1..=PERIOD_MAX with data[i] == data[i + P] for
/// at least PERIOD_MATCH of the first PERIOD_SPAN bytes, and the match
/// fraction at the best P; then the same test along the whole capture at
/// that P (cut by any dropped block). Samples of a live signal have no
/// such period; a buffer replayed has one.
fn periodicity(data: &[u8]) -> J {
    if data.len() < PERIOD_SPAN + PERIOD_MAX {
        return J::from("too few bytes");
    }
    let frac = |p: usize, span: usize| -> f64 { (0..span).filter(|&i| data[i] == data[i + p]).count() as f64 / span as f64 };
    let mut best = (0usize, 0f64);
    let mut first = None;
    for p in 1..=PERIOD_MAX {
        let f = frac(p, PERIOD_SPAN);
        if f > best.1 {
            best = (p, f);
        }
        if first.is_none() && f >= PERIOD_MATCH {
            first = Some((p, f));
            break;
        }
    }
    let whole = first.map(|(p, _)| frac(p, data.len() - p));
    J::obj()
        .with("test", format!("data[i] == data[i + P] for >= {PERIOD_MATCH} of the first {PERIOD_SPAN} bytes, P in 1..={PERIOD_MAX}"))
        .with("period", first.map(|(p, _)| p))
        .with("period_in_188_byte_blocks", first.map(|(p, _)| p as f64 / step3::BLOCK as f64))
        .with("match_fraction", first.map(|(_, f)| f))
        .with("match_fraction_whole_capture", whole)
        .with("best_if_none", J::obj().with("period", best.0).with("match_fraction", best.1))
}

/// Everything (b) says about one capture.
fn analyse(data: &[u8]) -> J {
    J::obj()
        .with("bytes", data.len())
        .with("periodicity", periodicity(data))
        .with("content_c_without_rate", Content::of(data, 0.0).compact())
        .with("content_legend", content::LEGEND)
        .with("excerpt_first_bytes", hex_bytes(&data[..data.len().min(EXCERPT)]))
        .with("block_structure", strip_columns(step3::block_structure(data)))
        .with("iq_stats", step3::iq_stats(data))
        .with("spectrum_complex", spectrum(data, true))
        .with("spectrum_real", spectrum(data, false))
}

/// The block structure without its 188 per-column entries (the constant
/// columns are kept).
fn strip_columns(j: J) -> J {
    match j {
        J::Obj(f) => J::Obj(f.into_iter().filter(|(k, _)| k != "columns").collect()),
        other => other,
    }
}

/// `n` windows with every byte kept, the drop counter after each.
fn capture(rtl: &Rtl, bulk: &mut BulkCounter, n: usize, data: &mut Vec<u8>) -> (J, usize, f64) {
    let mut ws = J::Arr(Vec::new());
    let (mut total, mut secs) = (0usize, 0f64);
    for _ in 0..n {
        let w = bulk.measure(step3::WINDOW, Some(data));
        total += w.bytes + w.stale_bytes;
        secs += w.elapsed.as_secs_f64();
        ws.push(
            J::obj()
                .with("bytes", w.bytes)
                .with("stale", w.stale_bytes)
                .with("elapsed_s", w.elapsed.as_secs_f64())
                .with("bytes_per_s", w.rate())
                .with("completions", w.completions)
                .with("sizes_first", J::Arr(w.sizes.iter().take(16).map(|s| J::from(*s)).collect()))
                .with("dropped_blocks_after", dropped(rtl))
                .with("bulk_check", w.check.compact()),
        );
    }
    (ws, total, if secs > 0.0 { total as f64 / secs } else { 0.0 })
}

fn write_verify(rtl: &Rtl, page: u8, off: u8, val: u8) -> (J, Option<u8>) {
    let orig = rtl.demod_read8(page, off).ok();
    let r = rtl.demod_write8_verify(page, off, val);
    (
        J::obj().with("original", orig.map(hex8)).with("written", hex8(val)).with("readback", r.as_ref().ok().map(|b| hex8(*b))).with("error", r.err().map(|e| e.to_string())),
        orig,
    )
}

/// The confirmation's options (`run` parses them; the scan uses the
/// defaults).
#[derive(Clone, Debug)]
pub struct Opts {
    pub windows: usize,
    pub buf: usize,
    pub minimise: bool,
    pub out: Option<String>,
    pub dump: Option<String>,
}

impl std::default::Default for Opts {
    fn default() -> Opts {
        Opts { windows: 10, buf: step3::BUF_LEN, minimise: true, out: None, dump: None }
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let d = Opts::default();
    let (mut baseline, mut n_windows, mut buf, mut minimise, mut out) = (Baseline::LowIf, d.windows, d.buf, d.minimise, d.out);
    let mut dump_dir: Option<String> = None;
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--baseline" => {
                baseline = Baseline::parse(args.get(i + 1).ok_or(USAGE)?)?;
                i += 1;
            }
            "--windows" => {
                n_windows = num(args.get(i + 1).ok_or(USAGE)?)? as usize;
                i += 1;
            }
            "--buf" => {
                buf = num(args.get(i + 1).ok_or(USAGE)?)? as usize;
                i += 1;
            }
            "--dump" => {
                dump_dir = Some(args.get(i + 1).ok_or(USAGE)?.clone());
                i += 1;
            }
            "--out" => {
                out = Some(args.get(i + 1).ok_or(USAGE)?.clone());
                i += 1;
            }
            "--no-minimise" => minimise = false,
            a => pos.push(a.to_string()),
        }
        i += 1;
    }
    if pos.len() != 3 {
        return Err(USAGE.into());
    }
    let (page, off, val) = (num(&pos[0])? as u8, num(&pos[1])? as u8, num(&pos[2])? as u8);
    confirm(baseline, page, off, val, &Opts { windows: n_windows, buf, minimise, out, dump: dump_dir }).map(|_| ())
}

/// Confirm one byte value on a baseline; returns the record's name.
pub fn confirm(baseline: Baseline, page: u8, off: u8, val: u8, o: &Opts) -> Result<String, String> {
    let (n_windows, buf, minimise, out, dump_dir) = (o.windows, o.buf, o.minimise, o.out.clone(), o.dump.clone());
    usb::guard()?;
    let mut rec = crate::header_with_source("3-confirm-write", "one demodulator byte written on top of a baseline: bytes/s against the resampler rate, the FIFO's dropped blocks, the bytes' structure and spectrum, tone off/on, and the baseline minimised")
        .with("candidate", J::obj().with("page", page).with("offset", hex8(off)).with("value", hex8(val)))
        .with("baseline_definition", baseline.definition())
        .with("windows_per_state", n_windows)
        .with("detector", BulkCounter::describe(buf, "per window: bytes, stale (completed before the window), elapsed, completions, the first 16 completion sizes, USB_EPA_FIFO_CFG[31:24] after the window, bulk_check"));
    let rates: Vec<f64> = if baseline == Baseline::PowerOn { vec![baseline.rate_hz()] } else { step3::CONFIRM_RATES_HZ.to_vec() };
    // (a) + (b)
    let mut tracking = J::Arr(Vec::new());
    for rate in rates {
        let (rtl, mut bulk, setup) = baseline.fresh_at(rate, &[], buf)?;
        let drop0 = dropped(&rtl);
        let control = bulk.measure(step3::WINDOW, None);
        let (w, orig) = write_verify(&rtl, page, off, val);
        let mut data = Vec::new();
        let (ws, total, bps) = capture(&rtl, &mut bulk, n_windows, &mut data);
        let restore = orig.map(|o| write_verify(&rtl, page, off, o).0);
        let mut after = Vec::new();
        let (wa, total_after, _) = capture(&rtl, &mut bulk, 3, &mut after);
        dump(&dump_dir, &format!("rate-{rate}"), &data);
        let analysis = analyse(&data);
        let period = match &analysis {
            J::Obj(f) => f.iter().find(|(k, _)| k == "periodicity").map(|(_, v)| v.render()).unwrap_or_default(),
            _ => String::new(),
        };
        println!(
            "rate {rate}: {total} B in {n_windows} windows = {bps:.0} B/s = {:.3} x 2 x rate; dropped blocks {:?} -> {:?}; after restore {total_after} B; periodicity {period}",
            bps / step3::expected_bytes_per_s(rate),
            drop0,
            dropped(&rtl)
        );
        tracking.push(
            J::obj()
                .with("rate_hz", rate)
                .with("rsamp_ratio", hex32(rsamp_ratio(rate)))
                .with("setup", setup)
                .with("dropped_blocks_before", drop0)
                .with("control_window_before_write", step3::window_record(&control, rate))
                .with("write", w)
                .with("windows", ws)
                .with("bytes_total", total)
                .with("bytes_per_s", bps)
                .with("ratio_to_2_x_rate", bps / step3::expected_bytes_per_s(rate))
                .with("analysis", analysis)
                .with("restore", restore)
                .with("windows_after_restore", wa)
                .with("bytes_after_restore", total_after),
        );
        bulk.close();
    }
    rec.set("rate_tracking", tracking);
    // (c)
    if matches!(baseline, Baseline::LowIf | Baseline::LowIfOpen) && source::command().is_some() {
        let rate = step3::RATE_HZ;
        let mut src = Source::open()?;
        rec.set("source", src.describe());
        let (rtl, mut bulk, setup) = baseline.fresh_at(rate, &[], buf)?;
        let (w, _) = write_verify(&rtl, page, off, val);
        let mut states = J::Arr(Vec::new());
        let mut plan: Vec<(String, Option<f64>)> = vec![("tone off".into(), None)];
        for o in TONE_OFFSETS_HZ {
            plan.push((format!("tone at the tuned RF {o:+} Hz"), Some(baselines::LOWIF_RF_HZ + o)));
        }
        plan.push(("tone off again".into(), None));
        for (name, hz) in plan {
            let s = match hz {
                Some(h) => src.level(h, baselines::CONTROL_LEVEL)?,
                None => src.off()?,
            };
            let mut data = Vec::new();
            let (ws, total, bps) = capture(&rtl, &mut bulk, n_windows, &mut data);
            dump(&dump_dir, &format!("tone-{}", name.replace(' ', "_")), &data);
            let a = analyse(&data);
            let top = |k: &str| match &a {
                J::Obj(f) => f.iter().find(|(n, _)| n == k).map(|(_, v)| v.render()).unwrap_or_default(),
                _ => String::new(),
            };
            println!("{name}: {total} B, {bps:.0} B/s; periodicity {}", top("periodicity"));
            states.push(J::obj().with("state", name.as_str()).with("source", s).with("windows", ws).with("bytes_total", total).with("bytes_per_s", bps).with("analysis", a));
        }
        src.quit();
        bulk.close();
        rec.set(
            "tone",
            J::obj()
                .with("rate_hz", rate)
                .with("tuned_rf_hz", baselines::LOWIF_RF_HZ)
                .with("expected", "for raw I/Q at the resampler's rate after the DDC: a peak at offset / rate cycles per sample (+0.0833 and -0.125 at 2.4 Msps), its sign flipped if the spectrum is inverted; any other reading of the bytes shows the peak elsewhere or not at all")
                .with("setup", setup)
                .with("write", w)
                .with("states", states),
        );
    }
    // (d)
    if minimise {
        let rate = baseline.rate_hz();
        let mut mini = J::Arr(Vec::new());
        for item in baseline.item_names() {
            let (rtl, mut bulk, setup) = baseline.fresh_at(rate, std::slice::from_ref(&item), buf)?;
            let (w, _) = write_verify(&rtl, page, off, val);
            let mut data = Vec::new();
            let (ws, total, bps) = capture(&rtl, &mut bulk, 5, &mut data);
            let c = Content::of(&data, 0.0);
            println!("without {item}: {total} B, {bps:.0} B/s, period score {:?}", c.period_score);
            mini.push(
                J::obj()
                    .with("removed", item.as_str())
                    .with("content_c_without_rate", c.compact())
                    .with("setup", setup)
                    .with("write", w)
                    .with("windows", ws)
                    .with("bytes_total", total)
                    .with("bytes_per_s", bps)
                    .with("needed_for_any_bytes", total == 0),
            );
            bulk.close();
            std::thread::sleep(Duration::from_millis(100));
        }
        rec.set("minimisation", mini);
    }
    let name = format!("step3-confirm-{}-p{page}-{off:02x}-{val:02x}.json", baseline.name().replace('-', ""));
    match out {
        Some(p) => {
            std::fs::write(&p, rec.render() + "\n").map_err(|e| e.to_string())?;
            println!("wrote {p}");
        }
        None => crate::write_record(&name, &rec),
    }
    Ok(name)
}
