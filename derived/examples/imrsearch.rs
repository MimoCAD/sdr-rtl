//! The image-rejection search: every IMR_G × IMR_P ([R820T] R8[5:0],
//! R9[5:0]; documents.md: "image gain 0 min … 63 max", "image phase 0 min
//! … 63 max", no value given) at one RF, against a tone on the image.
//!
//!   imrsearch [--rf HZ] [--step N] [--backoff DB]
//!   imrsearch --profile [--rf HZ] [--backoff DB] [--boost DB] [--from HZ --to HZ --xstep HZ]
//!
//! Built with `--features bench`. The candidate space, declared before
//! the run and covered in plain order (G ascending, then P ascending):
//! G, P ∈ {0, step, 2·step, …} ≤ 63 (step 1: all 4,096).
//!
//! With the high-side LO (findings 4h) the wanted RF is LO − IF and the
//! image LO + IF: a tone at RF + 2·IF − 300 kHz converts like a wanted
//! tone at RF + 300 kHz. First the reference — the tone at RF + 300 kHz,
//! IMR at the driver's 32/32 — then the tone moved to the image and held
//! there while R8/R9 step through the space; per candidate the level at
//! +300 kHz (dBFS per bin) and the median bin. The rejection is the
//! reference over the image level. R8's PWD_AMP and R9's PWD_IFFILT /
//! PW1_IFFILT keep the driver's values (`image::Front::image`); each write
//! is read back (R5-R15 read back exactly, findings 4a). One JSON line per
//! candidate; the driver's 32/32 restored at the end.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, RxSource, TuneRequest};
use sdr_rtl_derived::Rtl;
use sdr_rtl_derived::demod::ddc_if_hz;
use sdr_rtl_derived::image::IMR_MIDDLE;

const R8: u8 = 8;
const R9: u8 = 9;
const IMR_MASK: u8 = 0x3F;
const OFFSET_HZ: f64 = 300e3;
/// Bins a line may sit from where it is placed: the source and the dongle
/// differ by ~4.3 ppm (findings 11), +4.3 kHz at 1 GHz; 16 bins is
/// ±9.4 kHz at 2.048 Msps. (6 bins, ±3.5 kHz, lost the tone above
/// ~760 MHz.)
const TOL_BINS: usize = 16;
/// After an R8/R9 write: the transfers queued before it drain first.
const SETTLE: Duration = Duration::from_millis(40);
const CAPTURE_SEGMENTS: usize = 8;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str, d: f64| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (rf, step, backoff) = (get("--rf", 300e6), get("--step", 1.0) as u8, get("--backoff", -10.0) as i32);
    let mut src = Source::spawn(SOURCE_CMD, 0, backoff)?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    let tuned = rtl.set_freq(&TuneRequest::auto(rf)).map_err(|e| e.to_string())?;
    let actual = tuned.actual_rf_freq;
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let (r8, r9) = rtl.link().with_tuner(|l| Ok((l.tuner_read(10)?[R8 as usize], l.tuner_read(10)?[R9 as usize]))).map_err(|e| e.to_string())?;
    let (r8_keep, r9_keep) = (r8 & !IMR_MASK, r9 & !IMR_MASK);
    let write = |rtl: &Rtl, g: u8, p: u8| {
        rtl.link()
            .with_tuner(|l| {
                l.tuner_write(R8, r8_keep | g)?;
                l.tuner_write(R9, r9_keep | p)?;
                let back = l.tuner_read(10)?;
                Ok(back[R8 as usize] == r8_keep | g && back[R9 as usize] == r9_keep | p)
            })
            .map_err(|e| e.to_string())
    };
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let k = bin_of(OFFSET_HZ, rate);
    let level = |s: &mut RtlStream, buf: &mut [Iq], n: usize| -> Result<(f64, f64), String> {
        discard(s, buf, SETTLE)?;
        let c = capture(s, buf, FFT_N * n, Duration::from_secs(2))?;
        let p = spectrum(&c.samples).ok_or("short capture")?;
        Ok((db(p[near(&p, k, TOL_BINS)]), db(median(&p))))
    };
    write(&rtl, IMR_MIDDLE, IMR_MIDDLE)?;
    if args.iter().any(|a| a == "--profile") {
        // The rejection across the passband at the driver's 32/32: per
        // offset x the wanted tone at RF + x (the backoff) and the image
        // tone at RF + 2·IF − x, `--boost` dB stronger for range; both
        // read at +x; the rejection is wanted − (image − boost).
        let boost = get("--boost", 10.0) as i32;
        let (lo, hi, st) = (get("--from", -1.1e6), get("--to", 1.1e6), get("--xstep", 100e3));
        let n = ((hi - lo) / st).round() as i64;
        for i in 0..=n {
            let x = lo + i as f64 * st;
            let kx = bin_of(x, rate);
            let at = |s: &mut RtlStream, buf: &mut [Iq]| -> Result<(f64, f64), String> {
                discard(s, buf, SETTLE)?;
                let c = capture(s, buf, FFT_N * SEGMENTS, Duration::from_secs(2))?;
                let p = spectrum(&c.samples).ok_or("short capture")?;
                Ok((db(p[near(&p, kx, TOL_BINS)]), db(median(&p))))
            };
            src.tone_at(actual + x, 0, backoff)?;
            let (w, _) = at(&mut s, &mut buf)?;
            let image_rf = actual + 2.0 * ddc_if_hz() - x;
            src.tone_at(image_rf, 0, backoff + boost)?;
            let (img, med) = at(&mut s, &mut buf)?;
            println!(
                "{{\"kind\":\"profile\",\"rf\":{actual:.1},\"x\":{x},\"wanted_dbfs\":{w:.2},\"image_tone_hz\":{image_rf:.1},\"boost_db\":{boost},\"image_dbfs\":{img:.2},\"median_dbfs\":{med:.2},\"rejection_db\":{:.2},\"floor_limited\":{}}}",
                w - (img - boost as f64),
                img < med + 6.0
            );
        }
        src.off()?;
        return Ok(());
    }
    // `--boost`: the image tone that many dB stronger than the wanted one
    // (for range); `wanted` below is the wanted level referred to the
    // image tone's, so every rejection is wanted − image as before.
    let boost = get("--boost", 0.0) as i32;
    src.tone(actual + OFFSET_HZ)?;
    let (wanted_measured, wmed) = level(&mut s, &mut buf, SEGMENTS)?;
    let wanted = wanted_measured + boost as f64;
    let image_rf = actual + 2.0 * ddc_if_hz() - OFFSET_HZ;
    src.tone_at(image_rf, 0, backoff + boost)?;
    let (image_mid, _) = level(&mut s, &mut buf, SEGMENTS)?;
    src.off()?;
    let (off_level, off_med) = level(&mut s, &mut buf, SEGMENTS)?;
    println!(
        "{{\"kind\":\"head\",\"rf\":{actual:.1},\"if_hz\":{:.1},\"image_tone_hz\":{image_rf:.1},\"offset_hz\":{OFFSET_HZ},\"backoff_db\":{backoff},\"boost_db\":{boost},\"wanted_measured_dbfs\":{wanted_measured:.2},\"gain\":[8,8,2],\"r8\":{r8},\"r9\":{r9},\"space\":\"G,P in 0..=63 step {step} (or --values), G then P ascending\",\"wanted_dbfs\":{wanted:.2},\"wanted_median_dbfs\":{wmed:.2},\"image_at_32_32_dbfs\":{image_mid:.2},\"rejection_at_32_32_db\":{:.2},\"tone_off_dbfs\":{off_level:.2},\"tone_off_median_dbfs\":{off_med:.2}}}",
        ddc_if_hz(),
        wanted - image_mid
    );
    src.tone_at(image_rf, 0, backoff + boost)?;
    let t = Instant::now();
    // `--values a,b,…`: the candidate values for both fields in place of
    // the stepped range (declared in the head line the same way).
    let values: Vec<u8> = match args.iter().position(|a| a == "--values").and_then(|i| args.get(i + 1)) {
        Some(list) => list.split(',').filter_map(|v| v.parse().ok()).filter(|v| *v <= IMR_MASK).collect(),
        None => (0..=IMR_MASK).step_by(step.max(1) as usize).collect(),
    };
    for &g in &values {
        for &p in &values {
            let ok = write(&rtl, g, p)?;
            let (img, med) = level(&mut s, &mut buf, CAPTURE_SEGMENTS)?;
            println!("{{\"g\":{g},\"p\":{p},\"read_back\":{ok},\"image_dbfs\":{img:.2},\"median_dbfs\":{med:.2},\"rejection_db\":{:.2},\"floor_limited\":{}}}", wanted - img, img < med + 6.0);
        }
        eprintln!("G {g} done at {:.0} s", t.elapsed().as_secs_f64());
    }
    src.off()?;
    write(&rtl, IMR_MIDDLE, IMR_MIDDLE)?;
    Ok(())
}

use sdr_rtl_derived::RtlStream;

fn main() {
    if let Err(e) = run() {
        eprintln!("imrsearch: {e}");
        std::process::exit(1);
    }
}
