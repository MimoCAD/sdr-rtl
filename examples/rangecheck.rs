//! The tuner outside its documented 42-1002 MHz (findings 26): at each RF
//! the driver's tuning (divider, VCO, R2, lock), a tone 300 kHz above the
//! RF — where it lands against where the words put it, to a fraction of a
//! hertz, and its level — and, on request, every front-end path's level
//! and the image's.
//!
//!   rangecheck [--from MHz --to MHz --step MHz | --list MHz,MHz,…]
//!              [--paths] [--image] [--rate SPS] [--offset HZ]
//!              [--codes L M V] [--txvga N] [--backoff DB] [--ppm P]
//!              [--source-cmd "<command line>" | --no-source]
//!
//! `--divs a,b [--repeat N]`: at each RF, after the driver's tuning, the
//! synthesizer rewritten under each divider in turn (N rounds), the tone
//! read each time: one line per (RF, divider, round) with the error
//! against the words' LO and against the delivered one
//! (`Pll::lo_delivered_hz`). Back to back, the crystal cannot move
//! between them, so the errors' difference is the synthesizer's offset.
//!
//! `--ppm`: the driver's crystal correction (`Rtl::set_ppm`); the RF the
//! words give, the expected tone and the rate the samples are read at are
//! then all under it.
//!
//! Built with `--features bench` (the RF range is opened with
//! `set_bench_rf_range`); `SDR_BENCH_RTL` names the dongle. Gains fixed
//! (default LNA 8, mixer 8, VGA 2, as `vcoedge`). One JSON line per RF:
//! - `divider`, `vco_hz`, `r2`, `locked`, `fell_back` (the first candidate
//!   divider did not lock and `set_freq` took the next);
//! - `tone_dbfs` (the tone's amplitude from the samples mixed down by its
//!   fine frequency and averaged), `tone_over_median_db`;
//! - `expected_hz` (tone − the RF the words give), `tone_hz` (a 0.25 s
//!   phase-slope fit), `error_hz` = tone_hz − expected_hz;
//! - with `--paths`: `paths`, [RFFILT, RFMUX, tone dBFS] for all 16
//!   (RFFILT and RFMUX are 2-bit fields), then the driver's path again;
//! - with `--image`: the tone moved to the image of the same baseband
//!   offset (RF + 2·IF − offset), its level at that offset
//!   (`image_dbfs`), and the strongest bin outside DC then.

mod bench;

use std::f64::consts::TAU;
use std::time::Duration;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::band::path_for;
use sdr_rtl::blocking::{Link, Rtl};
use sdr_rtl::demod::ddc_if_hz;
use sdr_rtl::image::{Front, R16, R20, R21, R22, R26};
use sdr_rtl::pll::{Pll, VCO_INDICATOR_MASK, VCO_OFFSET_HZ, usable_codes};

const DEFAULT_OFFSET_HZ: f64 = 300e3;
const DEFAULT_RATE: f64 = 2.4e6;
/// The synthesizer's reach with /64 at the bottom and /2 at the top
/// (`pll::lo_reach`: LO 26.72-1825 MHz), as RF, and a little beyond each
/// so the refusals are recorded too.
const BENCH_RANGE_HZ: (f64, f64) = (20e6, 1830e6);
const TOL_BINS: usize = 6;
/// The fine-frequency capture and its averaging block.
const FINE_CAPTURE: Duration = Duration::from_millis(250);
const FINE_BLOCK_S: f64 = 1e-3;
const TWO_BIT_VALUES: u8 = 4;
/// The settle after a synthesizer write (`vcoedge`'s).
const PLL_SETTLE: Duration = Duration::from_millis(20);

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The synthesizer's words written (R16, R20-R22), R0-R4 read after the
/// settle.
fn program(link: &Link<'_>, pll: &Pll) -> Result<[u8; 5], String> {
    link.with_tuner(|l| {
        for (reg, v) in [
            (R16, pll.r16()),
            (R20, pll.r20()),
            (R21, pll.r21()),
            (R22, pll.r22()),
        ] {
            l.tuner_write(reg, v)?;
        }
        std::thread::sleep(PLL_SETTLE);
        l.tuner_status()
    })
    .map_err(|e| e.to_string())
}

fn write_path(link: &Link<'_>, f: &Front) -> Result<(), String> {
    link.with_tuner(|l| l.tuner_write(R26, f.r26()))
        .map_err(|e| e.to_string())
}

/// A tone's frequency near `f0` (Hz at `rate`) by the slope of its phase:
/// the samples mixed down by `f0`, averaged in blocks, the block phases
/// unwrapped and fitted by least squares. Also its amplitude against full
/// scale (the mixed-down mean at the fitted frequency).
fn fine_tone(x: &[Iq], f0: f64, rate: f64) -> (f64, f64) {
    let m = ((rate * FINE_BLOCK_S) as usize).max(1);
    let mix = |f: f64| -> Vec<(f64, f64)> {
        let step = -TAU * f / rate;
        x.chunks_exact(m)
            .enumerate()
            .map(|(b, blk)| {
                let (mut sr, mut si) = (0.0, 0.0);
                for (j, v) in blk.iter().enumerate() {
                    let ph = step * (b * m + j) as f64;
                    let (c, s) = (ph.cos(), ph.sin());
                    let (i, q) = (v.i as f64, v.q as f64);
                    sr += i * c - q * s;
                    si += i * s + q * c;
                }
                (sr / m as f64, si / m as f64)
            })
            .collect()
    };
    let blocks = mix(f0);
    let n = blocks.len();
    if n < 3 {
        return (f0, f64::NAN);
    }
    let mut phases = Vec::with_capacity(n);
    let mut prev = 0.0;
    let mut acc = 0.0;
    for (k, (r, i)) in blocks.iter().enumerate() {
        let p = i.atan2(*r);
        if k > 0 {
            let mut d = p - prev;
            d -= TAU * (d / TAU).round();
            acc += d;
        }
        prev = p;
        phases.push(acc);
    }
    let mean_t = (n - 1) as f64 / 2.0;
    let mean_p = phases.iter().sum::<f64>() / n as f64;
    let (mut num, mut den) = (0.0, 0.0);
    for (k, p) in phases.iter().enumerate() {
        let dt = k as f64 - mean_t;
        num += dt * (p - mean_p);
        den += dt * dt;
    }
    let slope = num / den;
    let f = f0 + slope / (TAU * m as f64 / rate);
    let (sr, si) = mix(f)
        .iter()
        .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
    let amp = (sr * sr + si * si).sqrt() / n as f64;
    (f, 20.0 * (amp / FULL_SCALE).max(1e-15).log10())
}

/// The tone's level (dBFS) at `hz` from one spectrum: the peak bin within
/// TOL_BINS, and the median bin.
fn level_at(p: &[f64], hz: f64, rate: f64) -> (f64, f64, usize) {
    let k = near(p, bin_of(hz, rate), TOL_BINS);
    (db(p[k]), db(median(p)), k)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |k: &str, d: f64| {
        arg(&args, k)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(d)
    };
    let rfs: Vec<f64> = if let Some(list) = arg(&args, "--list") {
        list.split(',')
            .filter_map(|v| v.parse::<f64>().ok())
            .map(|v| v * 1e6)
            .collect()
    } else {
        let (from, to, step) = (
            num("--from", 1000.0),
            num("--to", 1100.0),
            num("--step", 10.0),
        );
        let n = ((to - from) / step).round() as i64;
        (0..=n).map(|i| (from + i as f64 * step) * 1e6).collect()
    };
    let divs: Option<Vec<u32>> =
        arg(&args, "--divs").map(|v| v.split(',').filter_map(|d| d.parse().ok()).collect());
    let repeat = num("--repeat", 1.0) as usize;
    let paths = args.iter().any(|a| a == "--paths");
    let image = args.iter().any(|a| a == "--image");
    let rate_asked = num("--rate", DEFAULT_RATE);
    let offset = num("--offset", DEFAULT_OFFSET_HZ);
    let txvga = num("--txvga", 0.0) as u16;
    let backoff = num("--backoff", -15.0) as i32;
    let codes: [usize; 3] = args
        .iter()
        .position(|a| a == "--codes")
        .map(|i| {
            let c = |k: usize, d: usize| args.get(i + k).and_then(|v| v.parse().ok()).unwrap_or(d);
            [c(1, 8), c(2, 8), c(3, 2)]
        })
        .unwrap_or([8, 8, 2]);
    let cmd = arg(&args, "--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());

    sdr_rtl::set_bench_rf_range(Some(BENCH_RANGE_HZ));
    // `--no-source`: nothing keyed (what the dongle reads with the bench
    // transmitter unkeyed, or not even running).
    let no_source = args.iter().any(|a| a == "--no-source");
    let mut src = if no_source {
        None
    } else {
        Some(Source::spawn(&cmd, txvga, backoff)?)
    };
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    rtl.set_rate(rate_asked).map_err(|e| e.to_string())?;
    let ppm = num("--ppm", 0.0);
    rtl.set_ppm(ppm).map_err(|e| e.to_string())?;
    for (stage, code) in [
        (Stage::Lna, codes[0]),
        (Stage::Mixer, codes[1]),
        (Stage::Vga, codes[2]),
    ] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    // The raw stream comes at the chip's rate (the register's, under the
    // correction).
    let rate = rtl.rate().map_err(|e| e.to_string())? * rtl.decimation() as f64;
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    println!(
        "{{\"kind\":\"header\",\"step\":\"26\",\"program\":\"examples/rangecheck.rs\",\"rate\":{rate},\"ppm\":{ppm},\"offset_hz\":{offset},\"codes\":{codes:?},\"txvga\":{txvga},\"backoff\":{backoff},\"if_hz\":{},\"paths\":{paths},\"image\":{image},\"source\":{cmd:?}}}",
        ddc_if_hz()
    );
    for rf in rfs {
        let first = Pll::candidates(rf + ddc_if_hz())
            .ok()
            .and_then(|c| c.first().copied());
        if let Err(e) = rtl.set_freq(&TuneRequest::auto(rf)) {
            println!("{{\"rf\":{rf},\"refused\":{:?}}}", e.to_string());
            continue;
        }
        let st = rtl.tuner_status().map_err(|e| e.to_string())?;
        let tone = rf + offset;
        if let Some(src) = src.as_mut() {
            src.tone(tone)?;
        }
        if let Some(divs) = &divs {
            // The same RF under each divider in turn, back to back: the
            // crystal cannot move between them, so the difference of the
            // errors is the synthesizer's offset alone.
            let scale = 1.0 + ppm * 1e-6;
            for rep in 0..repeat {
                for &d in divs {
                    let code = usable_codes()
                        .into_iter()
                        .find(|&(_, x)| x == d)
                        .map(|c| c.0)
                        .ok_or(format!("no divider /{d}"))?;
                    let lo = rf / scale + ddc_if_hz();
                    let pll = match Pll::with_divider(lo - VCO_OFFSET_HZ / d as f64, code) {
                        Ok(p) => p,
                        Err(e) => {
                            println!(
                                "{{\"rf\":{rf},\"divider\":{d},\"error\":{:?}}}",
                                e.to_string()
                            );
                            continue;
                        }
                    };
                    let regs = program(&rtl.link(), &pll)?;
                    let words_expected = tone - (pll.lo_hz() - ddc_if_hz()) * scale;
                    let delivered_expected = tone - (pll.lo_delivered_hz() - ddc_if_hz()) * scale;
                    let c = spectrum_capture(&mut s, &mut buf)?;
                    let p = spectrum(&c.samples).ok_or("short capture")?;
                    let (_, med, k) = level_at(&p, words_expected, rate);
                    let rough = peak_hz(&p, k, rate);
                    let fine = capture(
                        &mut s,
                        &mut buf,
                        (rate * FINE_CAPTURE.as_secs_f64()) as usize,
                        Duration::from_secs(2),
                    )?;
                    let (tone_hz, tone_dbfs) = fine_tone(&fine.samples, rough, rate);
                    println!(
                        "{{\"rf\":{rf},\"divider\":{d},\"rep\":{rep},\"vco_hz\":{:.0},\"r2\":{},\"tone_dbfs\":{tone_dbfs:.2},\"tone_over_median_db\":{:.1},\"tone_hz\":{tone_hz:.3},\"error_words_hz\":{:.3},\"error_delivered_hz\":{:.3}}}",
                        pll.vco_hz(),
                        regs[2] & VCO_INDICATOR_MASK,
                        tone_dbfs - med,
                        tone_hz - words_expected,
                        tone_hz - delivered_expected
                    );
                }
            }
            continue;
        }
        let expected = tone - st.rf_hz;
        let c = spectrum_capture(&mut s, &mut buf)?;
        let p = spectrum(&c.samples).ok_or("short capture")?;
        let (_, med, k) = level_at(&p, expected, rate);
        let rough = peak_hz(&p, k, rate);
        // The strongest bin outside DC, wherever it is: where the tone went
        // when it is not where the words put it.
        let strongest = peaks(&p, 1, TOL_BINS)
            .first()
            .map(|&(k, pw)| (peak_hz(&p, k, rate), db(pw)))
            .unwrap_or((0.0, f64::NAN));
        let fine = capture(
            &mut s,
            &mut buf,
            (rate * FINE_CAPTURE.as_secs_f64()) as usize,
            Duration::from_secs(2),
        )?;
        let (tone_hz, tone_dbfs) = fine_tone(&fine.samples, rough, rate);
        let mut line = format!(
            "{{\"rf\":{rf},\"rf_words\":{:.3},\"divider\":{},\"vco_hz\":{:.0},\"r2\":{},\"locked\":{},\"fell_back\":{},\"path\":[{},{}],\"tone_dbfs\":{tone_dbfs:.2},\"median_dbfs\":{med:.1},\"tone_over_median_db\":{:.1},\"expected_hz\":{expected:.3},\"tone_hz\":{tone_hz:.3},\"error_hz\":{:.3},\"clipped\":{:.5},\"strongest\":[{:.0},{:.2}]",
            st.rf_hz,
            st.pll.divider,
            st.pll.vco_hz,
            st.pll.vco_indicator,
            st.pll.locked,
            first.map(|f| f.divider != st.pll.divider).unwrap_or(false),
            st.rffilt,
            st.rfmux,
            tone_dbfs - med,
            tone_hz - expected,
            clipped(&fine.samples),
            strongest.0,
            strongest.1
        );
        if paths {
            let base = path_for(rf).ok_or("no band path")?;
            let base = Front {
                rffilt: base.rffilt,
                rfmux: base.rfmux,
                tf_nch: base.tf_nch,
                tf_lp: base.tf_lp,
                ..Front::BRING_UP
            };
            let mut readings = Vec::new();
            for rffilt in 0..TWO_BIT_VALUES {
                for rfmux in 0..TWO_BIT_VALUES {
                    write_path(
                        &rtl.link(),
                        &Front {
                            rffilt,
                            rfmux,
                            ..base
                        },
                    )?;
                    let c = spectrum_capture(&mut s, &mut buf)?;
                    let p = spectrum(&c.samples).ok_or("short capture")?;
                    let (lvl, _, _) = level_at(&p, expected, rate);
                    readings.push(format!("[{rffilt},{rfmux},{lvl:.2}]"));
                }
            }
            write_path(&rtl.link(), &base)?;
            let c = spectrum_capture(&mut s, &mut buf)?;
            let p = spectrum(&c.samples).ok_or("short capture")?;
            let (again, _, _) = level_at(&p, expected, rate);
            line += &format!(
                ",\"paths\":[{}],\"driver_path_again_dbfs\":{again:.2}",
                readings.join(",")
            );
        }
        if image {
            let img = st.rf_hz + 2.0 * ddc_if_hz() - offset;
            if let Some(src) = src.as_mut() {
                src.tone(img)?;
            }
            let c = spectrum_capture(&mut s, &mut buf)?;
            let p = spectrum(&c.samples).ok_or("short capture")?;
            let (lvl, med, _) = level_at(&p, offset, rate);
            let top = peaks(&p, 1, TOL_BINS)
                .first()
                .map(|&(k, pw)| (peak_hz(&p, k, rate), db(pw)))
                .unwrap_or((0.0, f64::NAN));
            line += &format!(
                ",\"image_tone_hz\":{img:.0},\"image_dbfs\":{lvl:.2},\"image_median_dbfs\":{med:.1},\"image_strongest\":[{:.0},{:.2}]",
                top.0, top.1
            );
        }
        line += "}";
        println!("{line}");
    }
    if let Some(src) = src.as_mut() {
        src.off()?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("rangecheck: {e}");
        std::process::exit(1);
    }
}
