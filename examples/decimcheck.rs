//! Rates the chip does not reach, as a program sees them: through the
//! public `Stream`, which runs the chip at 2, 3 or 4 times the rate and
//! decimates (findings 25, `decim`).
//!
//!   decimcheck [--rates a,b,…] [--seconds S] [--folding F] [--source-cmd "<command line>"]
//!
//! Built with `--features bench` (the bench helpers). Per rate, opened
//! fresh (gains 8/8/2, 300 MHz), one JSON line:
//! - **In band**: a tone at +¼ of the rate, S seconds received through
//!   the stream after a discard: the samples against the rate × the time,
//!   the tone's frequency and its level over the median bin, and the
//!   phase of consecutive 64-sample windows against it (steps above 0.5
//!   rad counted, as `glitchscan`).
//! - **Folding in**: the same tone level at +0.7 of the rate, past the
//!   decimator's stopband edge (`decim::STOP_EDGE`), which folds onto −0.3
//!   of the rate: the power there against the in-band tone's, and the
//!   median bin's — the alias rejection, or the floor it is under.

mod bench;

use std::f64::consts::PI;
use std::time::Duration;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::blocking::Rtl;
use sdr_rtl::{OPEN_RF_HZ, Stage};

const WIN: usize = 64;
const JUMP_RAD: f64 = 0.5;
/// The in-band tone, a fraction of the rate.
const IN_BAND: f64 = 0.25;
/// The out-of-band tone, a fraction of the rate: folds onto it − 1.
const FOLDING: f64 = 0.7;
const DEFAULT_RATES: &str = "300500,310000,350000,400000,450000,500000,600000,700000,800000,900000";
/// A sample this far under the median |x| is a dip.
const DIP_FRACTION: f64 = 0.6;
/// The fold's bin is searched this many bins either side.
const SEARCH_BINS: usize = 8;

fn phase_jumps(x: &[Iq], tone: f64, rate: f64) -> usize {
    let phase_of = |a: usize| -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in x[a..a + WIN].iter().enumerate() {
            let ph = -2.0 * PI * tone * (a + n) as f64 / rate;
            re += v.i as f64 * ph.cos() - v.q as f64 * ph.sin();
            im += v.i as f64 * ph.sin() + v.q as f64 * ph.cos();
        }
        im.atan2(re)
    };
    let (mut jumps, mut prev, mut w): (usize, Option<f64>, usize) = (0, None, 0);
    while w + WIN <= x.len() {
        let ph = phase_of(w);
        if let Some(p) = prev {
            let d = (ph - p + PI).rem_euclid(2.0 * PI) - PI;
            if d.abs() > JUMP_RAD {
                jumps += 1;
            }
        }
        prev = Some(ph);
        w += WIN;
    }
    jumps
}

fn one(src: &mut Source, rate: f64, secs: f64, folding: f64) -> Result<String, String> {
    power_cycle()?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let decimation = rtl.decimation();
    let readback = rtl.rate().map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    src.tone(OPEN_RF_HZ + IN_BAND * rate)?;
    let mut s = rtl
        .into_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.error.to_string())?;
    let reported = s.rate();
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    discard(&mut s, &mut buf, Duration::from_millis(500))?;
    let c = window(&mut s, &mut buf, Duration::from_secs_f64(secs))?;
    let delivered = c.samples.len() as f64 / (rate * c.secs);
    let p = spectrum(&c.samples).ok_or("no spectrum")?;
    let k = near(&p, bin_of(IN_BAND * rate, rate), SEARCH_BINS);
    let tone = peak_hz(&p, k, rate);
    let tone_db = db(p[k]);
    let floor_db = db(median(&p));
    let jumps = phase_jumps(&c.samples, tone, rate);
    // Single-sample dips in the tone's level: samples under DIP_FRACTION of
    // the median |x| (the tone dominates every sample at this SNR).
    let mut mags: Vec<f64> = c
        .samples
        .iter()
        .map(|v| (v.i as f64).hypot(v.q as f64))
        .collect();
    let mut sorted = mags.clone();
    sorted.sort_by(f64::total_cmp);
    let med = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    let dips = mags.iter().filter(|m| **m < DIP_FRACTION * med).count();
    mags.sort_by(f64::total_cmp);
    let lowest = mags.first().copied().unwrap_or(0.0) / med.max(1.0);
    // The folding tone, the same level.
    src.tone(OPEN_RF_HZ + folding * rate)?;
    discard(&mut s, &mut buf, Duration::from_millis(300))?;
    let f = capture(&mut s, &mut buf, FFT_N * SEGMENTS, Duration::from_secs(3))?;
    let pf = spectrum(&f.samples).ok_or("no spectrum (folding)")?;
    let kf = near(&pf, bin_of((folding - 1.0) * rate, rate), SEARCH_BINS);
    let fold_db = db(pf[kf]);
    let fold_floor_db = db(median(&pf));
    // The default fold's bin whatever the tone's place: what stays there
    // when the tone moves is not the tone's.
    let fixed_db = db(pf[near(&pf, bin_of((FOLDING - 1.0) * rate, rate), SEARCH_BINS)]);
    let fin = s.finish().map_err(|e| e.to_string())?;
    Ok(format!(
        "\"rate\":{rate},\"decimation\":{decimation},\"rate_readback\":{readback:.1},\"rate_reported\":{reported:.1},\"delivered_ratio\":{delivered:.5},\"dropped_blocks\":{},\"tone_hz\":{tone:.0},\"tone_expected_hz\":{:.0},\"tone_over_median_db\":{:.1},\"phase_jumps\":{jumps},\"samples\":{},\"dips\":{dips},\"lowest_over_median\":{lowest:.3},\"folding\":{folding},\"fold_hz\":{:.0},\"fold_below_tone_db\":{:.1},\"fold_over_median_db\":{:.1},\"default_fold_bin_over_median_db\":{:.1}",
        fin.report.dropped_blocks,
        IN_BAND * rate,
        tone_db - floor_db,
        c.samples.len(),
        (folding - 1.0) * rate,
        tone_db - fold_db,
        fold_db - fold_floor_db,
        fixed_db - fold_floor_db
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rates: Vec<f64> = get("--rates")
        .unwrap_or_else(|| DEFAULT_RATES.into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(5.0);
    // `--folding F`: the out-of-band tone at F of the rate (default 0.7).
    let folding: f64 = get("--folding")
        .and_then(|v| v.parse().ok())
        .unwrap_or(FOLDING);
    let cmd = get("--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    let mut src = Source::spawn(&cmd, 0, -15)?;
    for rate in rates {
        match one(&mut src, rate, secs, folding) {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"rate\":{rate},\"error\":\"{e}\"}}"),
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("decimcheck: {e}");
        std::process::exit(1);
    }
}
