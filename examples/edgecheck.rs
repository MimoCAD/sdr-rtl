//! How much of each rate's window is clean (findings 26): through the
//! public `Stream` (decimating where the plan does), a tone placed at each
//! position g of the window — its in-band level there — and then outside
//! the window at g + m × rate for m = ±1, ±2, where it would fold onto g —
//! the level that lands on g. The rejection at g is the in-band level
//! minus the folded one: what a signal outside the window puts on a
//! channel at g against what the same signal inside it would.
//!
//!   edgecheck [--rates a,b,…] [--from F --to F --step F] [--folds 1,-1,2,-2] [--ppm P]
//!             [--source-cmd "<command line>"]
//!
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. Per
//! rate, opened fresh (gains 8/8/2, 300 MHz). g runs over ±[from, to] of
//! the rate (default 0.20-0.50 in 0.01 steps, both signs). One JSON line
//! per (rate, g): `inband_dbfs` (the strongest bin within SEARCH_HZ of
//! g, the tone at g; `inband_off_hz` how far off g it was), `median_dbfs`,
//! and per m `fold_m<m>_dbfs` (that bin, ± FOLD_BINS, with the tone at
//! g + m × rate) and `rejection_m<m>_db`.

mod bench;

use std::time::Duration;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::blocking::Rtl;
use sdr_rtl::{OPEN_RF_HZ, Stage};

const DEFAULT_RATES: &str =
    "240000,500000,1000000,1200000,1440000,1500000,1800000,2048000,2400000,2880000";
/// The in-band tone is searched this far either side of where it belongs
/// (the crystals' error against the source's, after `--ppm`, is a few
/// tens of Hz at 300 MHz; without it, up to ~300 Hz), at least 2 bins.
const SEARCH_HZ: f64 = 400.0;
/// The folded tone is read within this many bins of the in-band tone's
/// bin: it lands where the in-band tone did, give or take the rate's own
/// share.
const FOLD_BINS: usize = 1;
const DISCARD_AFTER_TONE: Duration = Duration::from_millis(60);

/// A spectrum after the discard, and its median bin.
fn spectrum_now(
    s: &mut impl sdr_core::RxSource,
    buf: &mut [Iq],
) -> Result<(Vec<f64>, f64), String> {
    discard(s, buf, DISCARD_AFTER_TONE)?;
    let c = capture(s, buf, FFT_N * SEGMENTS, Duration::from_secs(3))?;
    let p = spectrum(&c.samples).ok_or("no spectrum")?;
    let m = db(median(&p));
    Ok((p, m))
}

fn one(src: &mut Source, rate: f64, gs: &[f64], folds: &[i32], ppm: f64) -> Result<(), String> {
    power_cycle()?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    rtl.set_ppm(ppm).map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let decimation = rtl.decimation();
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    let mut s = rtl
        .into_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.error.to_string())?;
    let rate = s.rate();
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    src.tone(OPEN_RF_HZ)?;
    discard(&mut s, &mut buf, Duration::from_millis(500))?;
    let search_bins = ((SEARCH_HZ / (rate / FFT_N as f64)).ceil() as usize).max(2);
    for &g in gs {
        let at = g * rate;
        src.tone(OPEN_RF_HZ + at)?;
        let (p, med) = spectrum_now(&mut s, &mut buf)?;
        let k = near(&p, bin_of(at, rate), search_bins);
        let inband = db(p[k]);
        let mut line = format!(
            "{{\"rate\":{rate},\"decimation\":{decimation},\"g\":{g:.3},\"inband_dbfs\":{inband:.2},\"inband_off_hz\":{:.0},\"median_dbfs\":{med:.2}",
            (signed_bin(k) - signed_bin(bin_of(at, rate))) * rate / FFT_N as f64
        );
        for &m in folds {
            src.tone(OPEN_RF_HZ + at + m as f64 * rate)?;
            let (pf, fmed) = spectrum_now(&mut s, &mut buf)?;
            let fold = db(pf[near(&pf, k, FOLD_BINS)]);
            line += &format!(
                ",\"fold_m{m}_dbfs\":{fold:.2},\"fold_m{m}_median_dbfs\":{fmed:.2},\"rejection_m{m}_db\":{:.2}",
                inband - fold
            );
        }
        line += "}";
        println!("{line}");
    }
    let fin = s.finish().map_err(|e| e.to_string())?;
    println!(
        "{{\"rate\":{rate},\"dropped_blocks\":{}}}",
        fin.report.dropped_blocks
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let num = |k: &str, d: f64| get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(d);
    let rates: Vec<f64> = get("--rates")
        .unwrap_or_else(|| DEFAULT_RATES.into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let (from, to, step) = (num("--from", 0.20), num("--to", 0.50), num("--step", 0.01));
    let folds: Vec<i32> = get("--folds")
        .unwrap_or_else(|| "1,-1,2,-2".into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let n = ((to - from) / step).round() as i64;
    let mut gs: Vec<f64> = Vec::new();
    for i in 0..=n {
        let g = from + i as f64 * step;
        gs.push(g);
        gs.push(-g);
    }
    let cmd = get("--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    // `--ppm`: the dongle's crystal correction, so the tone lands on its bin.
    let ppm = num("--ppm", 0.0);
    let mut src = Source::spawn(&cmd, 0, -15)?;
    println!(
        "{{\"kind\":\"header\",\"step\":\"26\",\"program\":\"examples/edgecheck.rs\",\"from\":{from},\"to\":{to},\"step_g\":{step},\"folds\":{folds:?},\"ppm\":{ppm},\"source\":{cmd:?}}}"
    );
    for rate in rates {
        if let Err(e) = one(&mut src, rate, &gs, &folds, ppm) {
            println!("{{\"rate\":{rate},\"error\":{:?}}}", e);
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("edgecheck: {e}");
        std::process::exit(1);
    }
}
