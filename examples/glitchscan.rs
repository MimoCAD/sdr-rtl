//! Which output rates put small periodic phase steps into the samples?
//! Findings 16.5: at exactly 1.2 Msps a tone's phase, read in 64-sample
//! windows, steps ~30-48° about every 256 samples (one 512-byte packet);
//! at 1.024, 1.1 and 1.152 Msps it never does.
//!
//!   glitchscan [--rates a,b,…] [--seconds S]
//!
//! Built with `--features bench`. Per rate, from a power cycle (gains
//! 8/8/2, 300 MHz): a tone at +300 kHz (or +rate/4 if that is lower), S
//! seconds of the raw stream with no control traffic, the samples kept;
//! the tone's frequency from an FFT; then the phase of consecutive
//! 64-sample windows against it: steps above 0.5 rad counted, their sizes
//! and spacing summarised. One JSON line per rate.

mod bench;

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::blocking::Rtl;

const WIN: usize = 64;
const JUMP_RAD: f64 = 0.5;
const SKIP: usize = 50_000;
/// The start transient is counted apart over this long.
const EARLY_S: f64 = 0.010;

fn one(src: &mut Source, rate: f64, secs: f64) -> Result<String, String> {
    // `--key-early`: the tone keyed before the power cycle (as tiercheck
    // keys it), not just before the capture.
    let early = std::env::args().any(|a| a == "--key-early");
    let offset = (300e3_f64).min(rate / 4.0);
    if early {
        src.tone(OPEN_RF_HZ + offset)?;
    }
    power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ)).map_err(|e| e.to_string())?;
    if !early {
        src.tone(OPEN_RF_HZ + offset)?;
    }
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let mut all: Vec<Iq> = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs_f64(secs) {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        all.extend_from_slice(&buf[..info.n_samples]);
    }
    s.stop().map_err(|e| e.to_string())?;
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    // The tone from the settled part; the phase from the first sample.
    let tone = spectrum(&all[SKIP.min(all.len())..]).map(|p| peak_hz(&p, near(&p, bin_of(offset, rate), 32), rate)).unwrap_or(offset);
    let x = &all[..];
    let early_end = (rate * EARLY_S) as usize;
    let mut early_jumps = 0usize;
    let phase_of = |a: usize| -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in x[a..a + WIN].iter().enumerate() {
            let ph = -2.0 * PI * tone * (a + n) as f64 / rate;
            let (c, sn) = (ph.cos(), ph.sin());
            re += v.i as f64 * c - v.q as f64 * sn;
            im += v.i as f64 * sn + v.q as f64 * c;
        }
        im.atan2(re)
    };
    let (mut jumps, mut windows, mut sizes, mut at) = (0usize, 0usize, Vec::new(), Vec::new());
    let mut prev: Option<f64> = None;
    let mut w = 0;
    while w + WIN <= x.len() {
        let ph = phase_of(w);
        if let Some(p) = prev {
            let dd = (ph - p + PI).rem_euclid(2.0 * PI) - PI;
            if dd.abs() > JUMP_RAD {
                jumps += 1;
                if w < early_end {
                    early_jumps += 1;
                }
                sizes.push(dd.to_degrees().abs());
                at.push(w);
            }
        }
        prev = Some(ph);
        windows += 1;
        w += WIN;
    }
    sizes.sort_by(f64::total_cmp);
    let mut gaps: Vec<usize> = at.windows(2).map(|g| g[1] - g[0]).collect();
    gaps.sort();
    let med = |v: &[usize]| v.get(v.len() / 2).copied().unwrap_or(0);
    Ok(format!(
        "\"rate\":{rate},\"f_symbol_ratio\":{:.4},\"tone_hz\":{tone:.0},\"samples\":{},\"dropped_blocks\":{},\"windows\":{windows},\"jumps\":{jumps},\"jumps_first_10ms\":{early_jumps},\"jump_deg_median\":{:.0},\"jump_spacing_median\":{}",
        28.8e6 / (4.0 * rate),
        x.len(),
        d.blocks,
        sizes.get(sizes.len() / 2).copied().unwrap_or(0.0),
        med(&gaps)
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let rates: Vec<f64> = get("--rates")
        .unwrap_or_else(|| "1024000,1152000,1200000,1440000,1600000,1800000,2048000,2400000,2880000".into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let mut src = Source::spawn(SOURCE_CMD, 0, -15)?;
    for rate in rates {
        match one(&mut src, rate, secs) {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"rate\":{rate},\"error\":\"{e}\"}}"),
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("glitchscan: {e}");
        std::process::exit(1);
    }
}
