//! The gain codes against the stream's floor (findings 21): in the 800 MHz
//! band the A/B's tones read ~1 dB less over the floor through this driver
//! than through the earlier one at the same tone level, and its decode loss
//! there is spread over every channel, the strongest included. This reads,
//! for every LNA × mixer × VGA code, a tone at the A/B's level: its power,
//! the floor beside it, and how far it stands over that floor.
//!
//!   gainsweep [--center HZ] [--lna a,b,…] [--vga a,b,…] [--fields c0,…,c15]
//!             [--source-cmd "<command line>"]
//!
//! `--fields`: the FIR's taps for the stream instead of the driver's (the
//! design scaled, to see whether the FIR's gain costs the ADC headroom).
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. The
//! driver tunes --center (default 852.30625 MHz) at 2.4 Msps; the tone sits
//! TONE_OFFSET_HZ above it at the source's TONE_TXVGA / TONE_BACKOFF_DB (the
//! A/B's tone). Per code set, after DISCARD: the tone's bin (dBFS per bin,
//! the bench spectrum), the median of the bins FLOOR_NEAR_HZ to FLOOR_FAR_HZ
//! either side of it (the floor beside it, clear of the tone's skirt and of
//! DC), the median of the whole window, and the fraction of clipped
//! samples. One JSON line per code set; VGA defaults to every code.

mod bench;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::taps;

/// The A/B's calibration tone (examples/rtl_ab_run.rs in mimosdr).
const TONE_OFFSET_HZ: f64 = 300e3;
const TONE_TXVGA: u16 = 0;
const TONE_BACKOFF_DB: i32 = -20;
/// The floor beside the tone: bins this far from it, either side.
const FLOOR_NEAR_HZ: f64 = 50e3;
const FLOOR_FAR_HZ: f64 = 250e3;
const TOL_BINS: usize = 4;
/// LNA and mixer codes 0-15; VGA 0-15.
const CODES: usize = 16;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The median of the bins between `near` and `far` Hz from `k`, either side.
fn floor_beside(p: &[f64], k: usize, rate: f64) -> f64 {
    let hz_per_bin = rate / FFT_N as f64;
    let (lo, hi) = (
        (FLOOR_NEAR_HZ / hz_per_bin) as usize,
        (FLOOR_FAR_HZ / hz_per_bin) as usize,
    );
    let mut v: Vec<f64> = (lo..=hi)
        .flat_map(|d| [(k + d) % FFT_N, (k + FFT_N - d) % FFT_N])
        .filter(|&j| !is_dc(j))
        .map(|j| p[j])
        .collect();
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let center: f64 = arg(&args, "--center")
        .and_then(|v| v.parse().ok())
        .unwrap_or(852.30625e6);
    let cmd = arg(&args, "--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    let list = |name: &str| -> Vec<usize> {
        arg(&args, name)
            .map(|l| l.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_else(|| (0..CODES).collect())
    };
    let (lnas, vgas) = (list("--lna"), list("--vga"));
    let fields: Option<[i32; taps::TAPS]> = arg(&args, "--fields").map(|l| {
        let v: Vec<i32> = l.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        v.try_into()
            .unwrap_or_else(|_| panic!("--fields needs {} numbers", taps::TAPS))
    });
    let mut src = Source::spawn(&cmd, TONE_TXVGA, TONE_BACKOFF_DB)?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    rtl.set_rate(2.4e6).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(center))
        .map_err(|e| e.to_string())?;
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let mut s = match fields {
        Some(c) => rtl.rx_stream_with_taps(
            TRANSFER_BYTES,
            128,
            taps::pack(&c).map_err(|e| e.to_string())?,
        ),
        None => rtl.rx_stream(TRANSFER_BYTES, 128),
    }
    .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    src.tone(center + TONE_OFFSET_HZ)?;
    println!(
        "{{\"kind\":\"header\",\"center\":{center},\"rate\":{rate},\"tone_offset_hz\":{TONE_OFFSET_HZ},\"txvga\":{TONE_TXVGA},\"backoff\":{TONE_BACKOFF_DB},\"fft_n\":{FFT_N},\"fields\":{fields:?},\"line_legend\":\"codes, tone dBFS per bin, floor beside it, tone over it, whole-window median, clipped fraction\"}}"
    );
    for &lna in &lnas {
        for mixer in 0..CODES {
            for &vga in &vgas {
                for (stage, code) in [(Stage::Lna, lna), (Stage::Mixer, mixer), (Stage::Vga, vga)] {
                    rtl.set_stage_level(stage, code)
                        .map_err(|e| e.to_string())?;
                }
                discard(&mut s, &mut buf, DISCARD)?;
                let c = spectrum_capture(&mut s, &mut buf)?;
                let p = spectrum(&c.samples).ok_or("short capture")?;
                let k = near(&p, bin_of(TONE_OFFSET_HZ, rate), TOL_BINS);
                let floor = floor_beside(&p, k, rate);
                println!(
                    "{{\"lna\":{lna},\"mixer\":{mixer},\"vga\":{vga},\"tone_db\":{:.2},\"floor_db\":{:.2},\"cnr_db\":{:.2},\"median_db\":{:.2},\"clipped\":{:.5}}}",
                    db(p[k]),
                    db(floor),
                    db(p[k] / floor),
                    db(median(&p)),
                    clipped(&c.samples)
                );
            }
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("gainsweep: {e}");
        std::process::exit(1);
    }
}
