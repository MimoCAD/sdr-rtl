//! The stream kinds on the bench, through the public API only.
//!
//!   tiercheck [--retunes N] [--rate SPS] [--lossless-only] [--drain-ms MS (with --features bench)]
//!
//! A tone at 300.3 MHz (the source at 8 Msps, backoff −15 dB). Three runs,
//! each from a power cycle (gains 8/8/2):
//! 1. `LosslessStream` at 1.2 Msps, centred on 300.0 MHz: N retunes
//!    (default 20) every 150 ms, alternating 300.1 and 300.0 MHz, so the
//!    tone sits at +300 kHz in one configuration and +200 kHz in the other.
//!    The samples are kept. Each configuration's tone is read off an FFT
//!    of its first segment; at each break a 64-sample window slides across
//!    it, correlated against both tones, and the first centre where the new
//!    one outweighs the old is the break as the samples see it: its offset
//!    from `at_sample` is 0 if the break is exact. Inside each segment the
//!    windows' phase against the segment's tone steps smoothly; a lost run
//!    (a 94-sample block here: ~210°) is a jump — 0 expected, and the
//!    report's dropped blocks 0. (Per-sample frequency is too noisy at
//!    this SNR: it spreads 200-400 kHz around a tone at 301 kHz.)
//! 2. `AccountedStream` at 1.8 Msps, 3 s, then the report.
//! 3. `RestrictedStream` at 2.4 Msps, 3 s, then the report. Before it, a
//!    `LosslessStream` asked for at 2.4 Msps: refused, the receiver back.
//!
//! One JSON line per run.

mod bench;

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, RxSource, TuneRequest};
use sdr_rtl_derived::{OPEN_RF_HZ, Rtl, StreamReport};

const TONE_OFFSET_HZ: f64 = 300e3;
const RETUNE_STEP_HZ: f64 = 100e3;
const EVERY: Duration = Duration::from_millis(150);
/// Half the correlation window (samples): 64-sample windows lift the
/// tone ~18 dB over the per-sample noise.
const HALF: usize = 32;
/// How far either side of a break the window slides.
const SEARCH: usize = 20_000;
/// Samples kept clear of a segment's ends for its tone and phase reads.
const GUARD: usize = 500;
/// A window's phase step against its tone above this is a lost run.
const JUMP_RAD: f64 = 0.5;

fn open(rate: f64) -> Result<Rtl, String> {
    power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ)).map_err(|e| e.to_string())?;
    Ok(rtl)
}

fn report_json(r: &StreamReport, tail: usize) -> String {
    format!(
        "\"tier\":\"{:?}\",\"samples\":{},\"tail\":{tail},\"dropped_blocks\":{},\"saturated\":{},\"breaks\":{}",
        r.tier,
        r.samples,
        r.dropped_blocks,
        r.saturated,
        r.breaks.len()
    )
}

fn lossless(retunes: u32, rate: f64, drain_ms: Option<u64>) -> Result<String, String> {
    let rtl = open(rate)?;
    // `--fifo N` (bench): the endpoint FIFO's size in blocks (findings
    // 16.1: USB_EPA_FIFO_CFG low byte, 1-21 usable), written before the
    // stream exists.
    #[cfg(feature = "bench")]
    if let Some(n) = std::env::args().skip_while(|a| a != "--fifo").nth(1).and_then(|v| v.parse::<u8>().ok()) {
        use sdr_rtl_derived::protocol::{ACCESS_GET, ACCESS_SET, BLOCK_USB, USB_EPA_FIFO_CFG, windex, wvalue_usb};
        let v = rtl.link().get(wvalue_usb(USB_EPA_FIFO_CFG), windex(BLOCK_USB, ACCESS_GET, 0), 4, "FIFO_CFG read").map_err(|e| e.to_string())?;
        rtl.link().set(wvalue_usb(USB_EPA_FIFO_CFG), windex(BLOCK_USB, ACCESS_SET, 0), &[n, v[1], v[2], 0], "FIFO_CFG write").map_err(|e| e.to_string())?;
        eprintln!("FIFO size {n} (was {})", v[0]);
    }
    #[allow(unused_mut)]
    let mut s = rtl.into_lossless(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.error.to_string())?;
    #[cfg(feature = "bench")]
    if let Some(ms) = drain_ms {
        s.set_drain(Duration::from_millis(ms));
    }
    #[cfg(feature = "bench")]
    s.set_quiet_by_stop(std::env::args().any(|a| a == "--quiet-by-stop"));
    #[cfg(feature = "bench")]
    s.set_squeeze(std::env::args().skip_while(|a| a != "--squeeze").nth(1).and_then(|v| v.parse().ok()));
    #[cfg(not(feature = "bench"))]
    let _ = drain_ms;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut all: Vec<Iq> = Vec::new();
    let t = Instant::now();
    let mut next = EVERY;
    let mut done = 0u32;
    let mut quiet_ms = Vec::new();
    while done < retunes || t.elapsed() < next {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        all.extend_from_slice(&buf[..info.n_samples]);
        if done < retunes && t.elapsed() >= next {
            let rf = OPEN_RF_HZ + RETUNE_STEP_HZ * ((done + 1) % 2) as f64;
            let (_, b) = s.retune(&TuneRequest::auto(rf)).map_err(|e| e.to_string())?;
            quiet_ms.push(b.quiet.as_secs_f64() * 1e3);
            done += 1;
            next += EVERY;
        }
    }
    let breaks = s.breaks().to_vec();
    let fin = s.finish().map_err(|e| e.to_string())?;
    all.extend_from_slice(&fin.tail);
    // Segment boundaries: 0, each break, the end. Segment k is centred on
    // 300.0 MHz when k is even, 300.1 when odd.
    let mut edges: Vec<usize> = vec![0];
    edges.extend(breaks.iter().map(|b| b.at_sample as usize));
    edges.push(all.len());
    // Each configuration's tone, from the first segment of it (the
    // source/dongle ppm included): the FFT peak near where it belongs.
    let tone_of = |k: usize, nominal: f64| -> f64 {
        let (a, b) = (edges[k] + GUARD, edges[k + 1].saturating_sub(GUARD));
        let seg = &all[a.min(b)..b];
        spectrum(seg).map(|p| peak_hz(&p, near(&p, bin_of(nominal, rate), 32), rate)).unwrap_or(nominal)
    };
    let tones = [tone_of(0, TONE_OFFSET_HZ), tone_of(1.min(edges.len() - 2), TONE_OFFSET_HZ - RETUNE_STEP_HZ)];
    // |Σ x[n]·e^{−j2πfn/fs}| over [a, b).
    let corr = |a: usize, b: usize, f: f64| -> (f64, f64) {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in all[a..b].iter().enumerate().map(|(i, v)| (a + i, v)) {
            let ph = -2.0 * PI * f * n as f64 / rate;
            let (c, s) = (ph.cos(), ph.sin());
            re += v.i as f64 * c - v.q as f64 * s;
            im += v.i as f64 * s + v.q as f64 * c;
        }
        (re, im)
    };
    let mag = |z: (f64, f64)| (z.0 * z.0 + z.1 * z.1).sqrt();
    // Per break: slide a 2·HALF window across it; the first centre where
    // the new tone outweighs the old, less the break's position.
    let mut offsets: Vec<i64> = Vec::new();
    for (k, b) in breaks.iter().enumerate() {
        let at = b.at_sample as usize;
        let (old, new) = (tones[k % 2], tones[(k + 1) % 2]);
        let lo = at.saturating_sub(SEARCH).max(HALF);
        let hi = (at + SEARCH).min(all.len().saturating_sub(HALF));
        let mut found = None;
        for c in lo..hi {
            if mag(corr(c - HALF, c + HALF, new)) > mag(corr(c - HALF, c + HALF, old)) {
                found = Some(c as i64 - at as i64);
                break;
            }
        }
        offsets.push(found.unwrap_or(i64::MAX));
    }
    // Is what follows `at_sample` (until the new tone takes over) the old
    // segment going on seamlessly? The old tone's window phase just
    // before `at_sample`, against the window just after it and the window
    // ending where the takeover window begins (degrees; ~0 if seamless).
    let phase = |z: (f64, f64)| z.1.atan2(z.0);
    let wrap_deg = |d: f64| ((d + PI).rem_euclid(2.0 * PI) - PI).to_degrees();
    let mut cont_start: Vec<i64> = Vec::new();
    let mut cont_end: Vec<i64> = Vec::new();
    for (k, (b, off)) in breaks.iter().zip(&offsets).enumerate() {
        let (at, old) = (b.at_sample as usize, tones[k % 2]);
        if *off == i64::MAX || at < 2 * HALF {
            continue;
        }
        let before = phase(corr(at - 2 * HALF, at, old));
        cont_start.push(wrap_deg(phase(corr(at, at + 2 * HALF, old)) - before).round() as i64);
        let end = (at as i64 + off) as usize - HALF;
        if end >= at + 2 * HALF {
            cont_end.push(wrap_deg(phase(corr(end - 2 * HALF, end, old)) - before).round() as i64);
        }
    }
    // Segments as the samples see them: each break moved by its offset.
    let mut edges: Vec<usize> = vec![0];
    edges.extend(breaks.iter().zip(&offsets).map(|(b, o)| if *o == i64::MAX { b.at_sample as usize } else { (b.at_sample as i64 + o) as usize }));
    edges.push(all.len());
    // Lost runs inside segments: the phase of consecutive windows against
    // the segment's tone; a lost block steps it by ~210° here.
    let mut jumps = 0usize;
    // The first jumps: (samples after the segment's start, degrees).
    let mut jump_at: Vec<(i64, i64)> = Vec::new();
    let mut windows = 0usize;
    for k in 0..edges.len() - 1 {
        let f = tones[k % 2];
        let (a, b) = (edges[k] + GUARD, edges[k + 1].saturating_sub(GUARD));
        let mut prev: Option<f64> = None;
        let mut w = a;
        while w + 2 * HALF <= b {
            let z = corr(w, w + 2 * HALF, f);
            let ph = z.1.atan2(z.0);
            if let Some(p) = prev {
                let d = (ph - p + PI).rem_euclid(2.0 * PI) - PI;
                if d.abs() > JUMP_RAD {
                    jumps += 1;
                    if jump_at.len() < 24 {
                        jump_at.push((w as i64 - edges[k] as i64, d.to_degrees().round() as i64));
                    }
                }

            }
            prev = Some(ph);
            windows += 1;
            w += 2 * HALF;
        }
    }
    let exact = offsets.iter().filter(|o| o.abs() <= 1).count();
    let quiet_mean = quiet_ms.iter().sum::<f64>() / quiet_ms.len().max(1) as f64;
    Ok(format!(
        "\"run\":\"lossless\",\"rate\":{rate},{},\"kept\":{},\"retunes\":{done},\"quiet_ms_mean\":{quiet_mean:.1},\"tones_hz\":[{:.0},{:.0}],\"break_offsets\":{offsets:?},\"breaks_within_1_sample\":{exact},\"phase_windows\":{windows},\"phase_jumps\":{jumps},\"first_jumps_from_segment_start\":{jump_at:?},\"continuity_at_break_deg\":{cont_start:?},\"continuity_to_takeover_deg\":{cont_end:?},\"drain_ms\":{drain_ms:?},\"break_dropped\":{:?}",
        report_json(&fin.report, fin.tail.len()),
        all.len(),
        tones[0],
        tones[1],
        breaks.iter().map(|b| b.dropped_before.blocks).collect::<Vec<_>>()
    ))
}

fn plain(kind: &str, rate: f64) -> Result<String, String> {
    let rtl = open(rate)?;
    let t = Instant::now();
    let (fin, n) = if kind == "accounted" {
        let mut s = rtl.into_accounted(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.error.to_string())?;
        let mut buf = vec![Iq::default(); s.max_num_samps()];
        let mut n = 0usize;
        while t.elapsed() < Duration::from_secs(3) {
            n += s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?.n_samples;
        }
        (s.finish().map_err(|e| e.to_string())?, n)
    } else {
        // First the refusal: a lossless stream at this rate.
        let rtl = match rtl.into_lossless(TRANSFER_BYTES, QUEUE_DEPTH) {
            Ok(_) => return Err("a LosslessStream was made at 2.4 Msps".into()),
            Err(e) => {
                eprintln!("refused as expected: {}", e.error);
                *e.rtl
            }
        };
        let mut s = rtl.into_restricted(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.error.to_string())?;
        let mut buf = vec![Iq::default(); s.max_num_samps()];
        let mut n = 0usize;
        while t.elapsed() < Duration::from_secs(3) {
            n += s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?.n_samples;
        }
        (s.finish().map_err(|e| e.to_string())?, n)
    };
    Ok(format!("\"run\":\"{kind}\",\"rate\":{rate},{},\"received_before_finish\":{n}", report_json(&fin.report, fin.tail.len())))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let retunes: u32 = args.iter().position(|a| a == "--retunes").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(20);
    let mut src = Source::spawn(SOURCE_CMD, 0, -15)?;
    src.tone(OPEN_RF_HZ + TONE_OFFSET_HZ)?;
    let only_lossless = args.iter().any(|a| a == "--lossless-only");
    let rate: f64 = args.iter().position(|a| a == "--rate").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(1.2e6);
    let drain_ms: Option<u64> = args.iter().position(|a| a == "--drain-ms").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let runs = if only_lossless { vec![lossless(retunes, rate, drain_ms)] } else { vec![lossless(retunes, rate, drain_ms), plain("accounted", 1.8e6), plain("restricted", 2.4e6)] };
    for r in runs {
        match r {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"error\":\"{e}\"}}"),
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("tiercheck: {e}");
        std::process::exit(1);
    }
}
