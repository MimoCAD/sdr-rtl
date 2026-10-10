//! The driver on the bench: the first run of `derived/` against a
//! dongle, with the HackRF tone source cabled in through the pad.
//!
//!   hwcheck [--source-cmd "<command line>"] [--level TXVGA BACKOFF]
//!
//! Run from the workspace root (`cargo run --release --manifest-path
//! crates/rtl/derived/Cargo.toml --example hwcheck`); the default source
//! is `bench::SOURCE_CMD` (the tone source at 8 Msps) there.
//! Every measurement is one JSON line on stdout; progress goes to stderr.
//!
//! The checks, in order:
//! - `tone`: the stream as `Rtl::open` leaves it, the tuner programmed
//!   before the start (the bench programmed it after); a tone at each
//!   offset lands at offset / rate;
//! - `rate`: per rate, the stream dropped, the rate set, a new stream
//!   started (stop → rate change → start); delivered samples per second
//!   against the setting, and the tone at offset / rate;
//! - `restart`: stop and start within one stream (the start bit alone);
//! - `pause`: samples delivered while paused, then after the resume;
//! - `tune`: RF 100-220 MHz in 10 MHz steps (the /16 span), the tone
//!   300 kHz above each.
//!
//! FFT power spectra (complex, Hann window, segments averaged); SNR is
//! the predicted bin over the median bin; the frequency error is the
//! peak's parabolic interpolation against the predicted offset.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, RxSource, TuneRequest};
use sdr_rtl_derived::{Rtl, RtlStream};

/// Bins the peak may sit from the predicted bin: the source and the
/// dongle disagreed by ~+3 bins at 300 MHz (findings 10.2b).
const PEAK_TOL_BINS: usize = 4;
/// How long the delivered rate is counted.
const RATE_WINDOW: Duration = Duration::from_secs(2);

const TONE_RF_HZ: f64 = 300e6;
const TONE_OFFSETS_HZ: [f64; 6] = [100e3, 300e3, -200e3, -500e3, 700e3, -900e3];
const RATE_TONE_OFFSET_HZ: f64 = 300e3;
const RATES_SPS: [f64; 5] = [2.048e6, 1.024e6, 2.4e6, 2.88e6, 2.048e6];
const TUNE_GRID_MHZ: std::ops::RangeInclusive<u32> = 10..=22;
const TUNE_STEP_MHZ: f64 = 10.0;

/// The tone at `offset_hz` read off a capture at `rate`, as one JSON
/// object body (no braces).
fn tone_fields(x: &[Iq], offset_hz: f64, rate: f64) -> String {
    let Some(p) = spectrum(x) else {
        return format!("\"error\":\"{} samples, fewer than one FFT\"", x.len());
    };
    let med = median(&p);
    let db = |v: f64| 10.0 * (v / med).log10();
    let peak = (0..FFT_N).filter(|k| !is_dc(*k)).max_by(|a, b| p[*a].total_cmp(&p[*b])).unwrap_or(0);
    let predicted = ((offset_hz / rate * FFT_N as f64).round() as i64).rem_euclid(FFT_N as i64) as usize;
    let near = (predicted.saturating_sub(PEAK_TOL_BINS)..=(predicted + PEAK_TOL_BINS).min(FFT_N - 1))
        .max_by(|a, b| p[*a].total_cmp(&p[*b]))
        .unwrap_or(predicted);
    let mirror = (FFT_N - predicted) % FFT_N;
    // Parabolic interpolation of the peak in log power.
    let l = |k: usize| p[k % FFT_N].max(f64::MIN_POSITIVE).ln();
    let (a, b, c) = (l(peak + FFT_N - 1), l(peak), l(peak + 1));
    let frac = if a - 2.0 * b + c != 0.0 { 0.5 * (a - c) / (a - 2.0 * b + c) } else { 0.0 };
    let peak_hz = (signed_bin(peak) + frac) / FFT_N as f64 * rate;
    format!(
        "\"offset_hz\":{offset_hz},\"predicted_bin\":{predicted},\"peak_bin\":{peak},\"peak_at_predicted\":{},\"snr_db\":{:.1},\"mirror_db\":{:.1},\"peak_hz\":{peak_hz:.0},\"error_hz\":{:.0},\"clipped\":{:.5}",
        (peak as i64 - predicted as i64).unsigned_abs() as usize <= PEAK_TOL_BINS,
        db(p[near]),
        db(p[mirror]),
        peak_hz - offset_hz,
        clipped(x)
    )
}

fn emit(check: &str, body: &str) {
    println!("{{\"check\":\"{check}\",{body}}}");
}

fn check_tone(rtl: &mut Rtl, src: &mut Source) -> Result<(), String> {
    eprintln!("tone: RF {} MHz, the tuner programmed at open, before the start", TONE_RF_HZ / 1e6);
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let t = Instant::now();
    s.start().map_err(|e| e.to_string())?;
    let first = capture(&mut s, &mut buf, 1, Duration::from_millis(500))?;
    emit(
        "tone_start",
        &format!("\"rate_sps\":{rate},\"first_samples_ms\":{:.2},\"got\":{}", t.elapsed().as_secs_f64() * 1e3, first.samples.len()),
    );
    src.off()?;
    let c = spectrum_capture(&mut s, &mut buf)?;
    emit("tone_off", &format!("{},\"overflows\":{}", tone_fields(&c.samples, TONE_OFFSETS_HZ[0], rate), c.overflows));
    for off in TONE_OFFSETS_HZ {
        src.tone(TONE_RF_HZ + off)?;
        let c = spectrum_capture(&mut s, &mut buf)?;
        emit("tone", &format!("{},\"overflows\":{}", tone_fields(&c.samples, off, rate), c.overflows));
    }
    src.off()?;
    Ok(())
}

/// Delivered samples per second over the rate window, after a discard.
fn measure_rate(s: &mut RtlStream, buf: &mut [Iq]) -> Result<(f64, Capture), String> {
    discard(s, buf, DISCARD * 3)?;
    let c = window(s, buf, RATE_WINDOW)?;
    Ok((c.samples.len() as f64 / c.secs, c))
}

fn check_rate(rtl: &mut Rtl, src: &mut Source) -> Result<(), String> {
    src.tone(TONE_RF_HZ + RATE_TONE_OFFSET_HZ)?;
    for sps in RATES_SPS {
        eprintln!("rate: {} Msps, a new stream after the last was dropped", sps / 1e6);
        rtl.set_rate(sps).map_err(|e| e.to_string())?;
        let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
        let mut buf = vec![Iq::default(); s.max_num_samps()];
        s.start().map_err(|e| e.to_string())?;
        let (got, c) = measure_rate(&mut s, &mut buf)?;
        emit(
            "rate",
            &format!(
                "\"set_sps\":{sps},\"register_sps\":{},\"delivered_sps\":{got:.0},\"ratio\":{:.5},\"overflows\":{},\"timeouts\":{},{}",
                s.rate(),
                got / sps,
                c.overflows,
                c.timeouts,
                tone_fields(&c.samples, RATE_TONE_OFFSET_HZ, sps)
            ),
        );
    }
    src.off()?;
    Ok(())
}

fn check_restart_and_pause(rtl: &mut Rtl, src: &mut Source) -> Result<(), String> {
    src.tone(TONE_RF_HZ + RATE_TONE_OFFSET_HZ)?;
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let (before, _) = measure_rate(&mut s, &mut buf)?;
    eprintln!("restart: stop, then the start bit alone");
    s.stop().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    s.start().map_err(|e| e.to_string())?;
    let first = capture(&mut s, &mut buf, 1, Duration::from_millis(500))?;
    let first_ms = t.elapsed().as_secs_f64() * 1e3;
    let (after, c) = measure_rate(&mut s, &mut buf)?;
    emit(
        "restart",
        &format!(
            "\"before_sps\":{before:.0},\"first_samples_ms\":{first_ms:.2},\"got\":{},\"after_sps\":{after:.0},\"overflows\":{},{}",
            first.samples.len(),
            c.overflows,
            tone_fields(&c.samples, RATE_TONE_OFFSET_HZ, rate)
        ),
    );
    eprintln!("pause / resume");
    s.pause().map_err(|e| e.to_string())?;
    // Transfers already in flight drain first; the second window is the pause.
    let drain = window(&mut s, &mut buf, Duration::from_millis(100))?;
    let paused = window(&mut s, &mut buf, Duration::from_millis(500))?;
    s.resume().map_err(|e| e.to_string())?;
    let (resumed, c) = measure_rate(&mut s, &mut buf)?;
    emit(
        "pause",
        &format!(
            "\"drain_samples_100ms\":{},\"paused_samples_500ms\":{},\"paused_timeouts\":{},\"resumed_sps\":{resumed:.0},\"overflows\":{},{}",
            drain.samples.len(),
            paused.samples.len(),
            paused.timeouts,
            c.overflows,
            tone_fields(&c.samples, RATE_TONE_OFFSET_HZ, rate)
        ),
    );
    drop(s);
    src.off()?;
    Ok(())
}

fn check_tune(rtl: &mut Rtl, src: &mut Source) -> Result<(), String> {
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    for step in TUNE_GRID_MHZ {
        let rf = step as f64 * TUNE_STEP_MHZ * 1e6;
        eprintln!("tune: {} MHz", rf / 1e6);
        let r = rtl.set_freq(&TuneRequest::auto(rf)).map_err(|e| e.to_string())?;
        let st = rtl.tuner_status().map_err(|e| e.to_string())?;
        src.off()?;
        let off = spectrum_capture(&mut s, &mut buf)?;
        let noise = spectrum(&off.samples).map(|p| 10.0 * median(&p).log10()).unwrap_or(f64::NAN);
        src.tone(rf + RATE_TONE_OFFSET_HZ)?;
        let c = spectrum_capture(&mut s, &mut buf)?;
        let nudge = r.actual_rf_freq - rf;
        emit(
            "tune",
            &format!(
                "\"rf_hz\":{rf},\"actual_rf_hz\":{:.1},\"divider\":{},\"vco_indicator\":{},\"locked\":{},\"rffilt\":{},\"rfmux\":{},\"median_db_off\":{noise:.1},\"overflows\":{},{}",
                r.actual_rf_freq,
                st.pll.divider,
                st.pll.vco_indicator,
                st.pll.locked,
                st.rffilt,
                st.rfmux,
                c.overflows,
                tone_fields(&c.samples, RATE_TONE_OFFSET_HZ - nudge, rate)
            ),
        );
    }
    src.off()?;
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut source_cmd = SOURCE_CMD.to_string();
    let (mut txvga, mut backoff) = (0u16, -20i32);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--source-cmd" => source_cmd = it.next().ok_or("--source-cmd needs a value")?.clone(),
            "--level" => {
                txvga = it.next().and_then(|v| v.parse().ok()).ok_or("--level TXVGA BACKOFF")?;
                backoff = it.next().and_then(|v| v.parse().ok()).ok_or("--level TXVGA BACKOFF")?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mut src = Source::spawn(&source_cmd, txvga, backoff)?;
    let t = Instant::now();
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    let st = rtl.tuner_status().map_err(|e| e.to_string())?;
    emit(
        "open",
        &format!(
            "\"label\":\"{}\",\"open_ms\":{:.1},\"chip_id_ok\":{},\"rf_hz\":{:.1},\"divider\":{},\"vco_indicator\":{},\"locked\":{},\"gain_db\":{:.1},\"level\":[{txvga},{backoff}]",
            rtl.label(),
            t.elapsed().as_secs_f64() * 1e3,
            st.chip_id_ok,
            st.rf_hz,
            st.pll.divider,
            st.pll.vco_indicator,
                st.pll.locked,
            rtl.gain()
        ),
    );
    check_tone(&mut rtl, &mut src)?;
    check_rate(&mut rtl, &mut src)?;
    check_restart_and_pause(&mut rtl, &mut src)?;
    check_tune(&mut rtl, &mut src)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("hwcheck: {e}");
        std::process::exit(1);
    }
}
