//! Levels of a ci16_le SigMF capture, for the side-by-side benchmark.
//!
//!   iqlevel FILE.sigmf-data RATE OFFSET_HZ [BW_HZ]
//!
//! One JSON line: the RMS level (dBFS), the fraction of samples at the
//! 8-bit rails, and the signal at OFFSET_HZ from the centre — the mean
//! power per bin within ± BW_HZ / 2 (default 10 kHz, a P25 channel) over
//! the median bin (the noise), averaged over every whole FFT in the file.

mod bench;

use bench::*;
use sdr_core::Iq;

const DEFAULT_BW_HZ: f64 = 10e3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: iqlevel FILE.sigmf-data RATE OFFSET_HZ [BW_HZ]");
        std::process::exit(2);
    }
    let (path, rate, offset) = (
        &args[0],
        args[1].parse::<f64>().unwrap_or(2.4e6),
        args[2].parse::<f64>().unwrap_or(0.0),
    );
    let bw = args
        .get(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BW_HZ);
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let x: Vec<Iq> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| Iq {
            i: i16::from_le_bytes([c[0], c[1]]),
            q: i16::from_le_bytes([c[2], c[3]]),
        })
        .collect();
    let rms = x
        .iter()
        .map(|v| (v.i as f64).powi(2) + (v.q as f64).powi(2))
        .sum::<f64>()
        / x.len().max(1) as f64;
    let mut acc = vec![0.0; FFT_N];
    let mut blocks = 0usize;
    for chunk in x.as_chunks::<{ FFT_N * SEGMENTS }>().0 {
        if let Some(p) = spectrum(chunk) {
            for (a, v) in acc.iter_mut().zip(&p) {
                *a += v;
            }
            blocks += 1;
        }
    }
    let half = ((bw / 2.0) / rate * FFT_N as f64).round() as i64;
    let centre = bin_of(offset, rate) as i64;
    let band: Vec<f64> = (-half..=half)
        .map(|d| acc[(centre + d).rem_euclid(FFT_N as i64) as usize])
        .collect();
    let signal = band.iter().sum::<f64>() / band.len() as f64;
    let noise = median(&acc);
    // The strongest bin in the band, its frequency interpolated: a tone's
    // measured offset.
    let peak = near(&acc, centre as usize, half as usize);
    println!(
        "{{\"file\":\"{path}\",\"samples\":{},\"rms_dbfs\":{:.2},\"clipped\":{:.6},\"offset_hz\":{offset},\"bw_hz\":{bw},\"signal_over_noise_db\":{:.2},\"peak_over_noise_db\":{:.2},\"peak_hz\":{:.1},\"noise_dbfs_per_bin\":{:.2},\"blocks\":{blocks}}}",
        x.len(),
        db(rms / (FULL_SCALE * FULL_SCALE)),
        clipped(&x),
        db(signal / noise),
        db(acc[peak] / noise),
        peak_hz(&acc, peak, rate),
        db(noise / blocks.max(1) as f64)
    );
}
