//! The stream on the bench, through the public API only. (Written for
//! the three stream kinds of findings 16.5-16.8 — `LosslessStream`,
//! `AccountedStream`, `RestrictedStream`; since one `Stream`.)
//!
//!   tiercheck [--retunes N] [--rate SPS] [--rate-floor SPS] [--lossless-only] [--depth N]
//!             [--drain-ms MS] [--no-fifo-reset]   (these two with --features bench)
//!             [--no-source]   (no tone: the loss counts only)
//!             [--source-cmd "<command line>"]   (default `bench::SOURCE_CMD`)
//!             [--first-ms N]   (the first retune asked for N ms after the
//!                               start; the stream holds any change until
//!                               `session::CHANGE_AFTER_START`)
//!
//! A tone at 300.3 MHz (the source at 8 Msps, backoff −15 dB). Three runs,
//! each from a power cycle (gains 8/8/2):
//! 1. A `Stream` at 1.2 Msps (`--rate`), centred on 300.0 MHz: N retunes
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
//! 2. A `Stream` at 1.8 Msps with no change, 3 s, then the report.
//! 3. The same at 2.4 Msps.
//!
//! One JSON line per run.

mod bench;

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::{OPEN_RF_HZ, StreamReport};

/// The tone's offset and the retune's step, as fractions of the rate: at
/// the default 1.2 Msps, +300 kHz and 100 kHz (the tone at +300 and +200
/// kHz); at any rate inside the window (rates under 1.024 Msps, findings
/// 25).
const TONE_OFFSET_RATES: f64 = 0.25;
const RETUNE_STEP_RATES: f64 = 1.0 / 12.0;
/// `--rate`'s default.
const DEFAULT_RATE: f64 = 1.2e6;

fn tone_offset(rate: f64) -> f64 {
    TONE_OFFSET_RATES * rate
}

fn retune_step(rate: f64) -> f64 {
    RETUNE_STEP_RATES * rate
}
const EVERY: Duration = Duration::from_millis(150);
/// Half the correlation window (samples), `--half` or 32: 64-sample
/// windows lift the tone ~18 dB over the per-sample noise. A longer window
/// tells the two tones apart more finely (their 100 kHz spacing is fewer
/// of a short window's bins at a high rate) at the cost of placement
/// resolution.
static HW_SET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
fn half() -> usize {
    *HW_SET.get().unwrap_or(&32)
}
/// How far either side of a break the window slides.
const SEARCH: usize = 20_000;
/// Samples kept clear of a segment's ends for its tone and phase reads.
const GUARD: usize = 500;
/// A segment with this many jumps gets its block map on stderr (JUMPMAP).
const JUMP_RUN_MAP: usize = 8;
/// The map's block and how many blocks after the segment's start.
const MAP_BLOCK: usize = 1_024;
const MAP_BLOCKS: usize = 64;
/// A window's phase step against its tone above this is a lost run.
const JUMP_RAD: f64 = 0.5;

fn open(rate: f64) -> Result<Rtl, String> {
    power_cycle()?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    // `--dagc-off` / `--aagc-off`: the demodulator's digital AGC (page 1
    // 0x11 bit 0) or its analog IF and RF loops (page 1 0x04 bits 7-6)
    // disabled after the open — which loop shapes the signal after a break.
    #[cfg(feature = "bench")]
    {
        let args: Vec<String> = std::env::args().collect();
        use sdr_rtl::protocol::{
            AAGC_OFF, AAGC_PAGE, DAGC_OFF, DAGC_PAGE, EN_DAGC, EN_IF_AGC, EN_RF_AGC,
        };
        if args.iter().any(|a| a == "--dagc-off") {
            rtl.link()
                .demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, 0)
                .map_err(|e| e.to_string())?;
        }
        if args.iter().any(|a| a == "--aagc-off") {
            rtl.link()
                .demod_rmw8(AAGC_PAGE, AAGC_OFF, EN_IF_AGC | EN_RF_AGC, 0)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(rtl)
}

fn report_json(r: &StreamReport, tail: usize) -> String {
    format!(
        "\"samples\":{},\"tail\":{tail},\"dropped_blocks\":{},\"saturated\":{},\"breaks\":{},\"stall_retries\":{},\"configuration_unknown\":{}",
        r.samples,
        r.dropped_blocks,
        r.saturated,
        r.breaks.len(),
        r.stall_retries,
        r.configuration_unknown
    )
}

fn lossless(retunes: u32, rate: f64, drain_ms: Option<u64>) -> Result<String, String> {
    let rtl = open(rate)?;
    // `--fifo N` (bench): the endpoint FIFO's size in blocks (findings
    // 16.1: USB_EPA_FIFO_CFG low byte, 1-21 usable), written before the
    // stream exists.
    #[cfg(feature = "bench")]
    if let Some(n) = std::env::args()
        .skip_while(|a| a != "--fifo")
        .nth(1)
        .and_then(|v| v.parse::<u8>().ok())
    {
        use sdr_rtl::protocol::{
            ACCESS_GET, ACCESS_SET, BLOCK_USB, USB_EPA_FIFO_CFG, windex, wvalue_usb,
        };
        let v = rtl
            .link()
            .get(
                wvalue_usb(USB_EPA_FIFO_CFG),
                windex(BLOCK_USB, ACCESS_GET, 0),
                4,
                "FIFO_CFG read",
            )
            .map_err(|e| e.to_string())?;
        rtl.link()
            .set(
                wvalue_usb(USB_EPA_FIFO_CFG),
                windex(BLOCK_USB, ACCESS_SET, 0),
                &[n, v[1], v[2], 0],
                "FIFO_CFG write",
            )
            .map_err(|e| e.to_string())?;
        eprintln!("FIFO size {n} (was {})", v[0]);
    }
    // `--depth N`: transfers in flight (the stream keeps at least
    // `session::MIN_QUEUE_DEPTH`).
    let depth: usize = std::env::args()
        .skip_while(|a| a != "--depth")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(QUEUE_DEPTH);
    let mut s = rtl
        .into_stream(TRANSFER_BYTES, depth)
        .map_err(|e| e.error.to_string())?;
    #[cfg(feature = "bench")]
    if let Some(ms) = drain_ms {
        s.set_drain(Duration::from_millis(ms));
    }
    #[cfg(feature = "bench")]
    s.set_fifo_reset(!std::env::args().any(|a| a == "--no-fifo-reset"));
    // `--pause`: quiesce each change with the pause bit, as before findings
    // 19, instead of the start bit.
    #[cfg(feature = "bench")]
    s.set_quiesce_with_pause(std::env::args().any(|a| a == "--pause"));
    #[cfg(not(feature = "bench"))]
    let _ = drain_ms;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut all: Vec<Iq> = Vec::new();
    let t = Instant::now();
    // `--first-ms N`: the first retune N ms after the start (default
    // EVERY), the rest EVERY apart.
    let first = std::env::args()
        .skip_while(|a| a != "--first-ms")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(EVERY);
    let mut next = first;
    let mut done = 0u32;
    let mut quiet_ms = Vec::new();
    while done < retunes || t.elapsed() < next {
        let info = s
            .recv_into(&mut buf, RECV_TIMEOUT_S)
            .map_err(|e| e.to_string())?;
        all.extend_from_slice(&buf[..info.n_samples]);
        if done < retunes && t.elapsed() >= next {
            let rf = OPEN_RF_HZ + retune_step(rate) * ((done + 1) % 2) as f64;
            // `--change none|gain`: a pause with no change in it, or one
            // writing the gain it already has (no PLL write) — the break
            // without a retune (the tone stays where it was).
            let change = std::env::args().skip_while(|a| a != "--change").nth(1);
            let b = match change.as_deref() {
                Some("none") => {
                    s.reconfigure(async |_| Ok(()))
                        .map_err(|e| e.to_string())?
                        .1
                }
                Some("gain") => {
                    let g = s.gain();
                    s.set_gain(g).map_err(|e| e.to_string())?
                }
                _ => {
                    s.retune(&TuneRequest::auto(rf))
                        .map_err(|e| e.to_string())?
                        .1
                }
            };
            quiet_ms.push(b.quiet.as_secs_f64() * 1e3);
            done += 1;
            next += EVERY;
        }
    }
    let breaks = s.breaks();
    let fin = s.finish().map_err(|e| e.to_string())?;
    all.extend_from_slice(&fin.tail_samples());
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
        spectrum(seg)
            .map(|p| peak_hz(&p, near(&p, bin_of(nominal, rate), 32), rate))
            .unwrap_or(nominal)
    };
    let tones = [
        tone_of(0, tone_offset(rate)),
        tone_of(
            1.min(edges.len() - 2),
            tone_offset(rate) - retune_step(rate),
        ),
    ];
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
    // Each break's search stays between its neighbours: two breaks a few ms
    // apart (two changes queued behind the stream's start hold) put the
    // break before's tone — this break's "new" one — inside ±SEARCH, and an
    // unbounded search took it for the takeover (findings 21: 7 runs, each
    // with this break's offset at −SEARCH, every phase jump of those runs).
    let bounds = |k: usize| -> (usize, usize) {
        let prev = k.checked_sub(1).map_or(0, |i| breaks[i].at_sample as usize);
        let next = breaks
            .get(k + 1)
            .map_or(all.len(), |b| b.at_sample as usize);
        (prev, next)
    };
    // Per break: slide a 2·half() window across it; the first centre where
    // the new tone outweighs the old, less the break's position.
    let mut offsets: Vec<i64> = Vec::new();
    for (k, b) in breaks.iter().enumerate() {
        let at = b.at_sample as usize;
        let (old, new) = (tones[k % 2], tones[(k + 1) % 2]);
        let (prev, next) = bounds(k);
        let lo = at.saturating_sub(SEARCH).max(prev + half()).max(half());
        let hi = (at + SEARCH)
            .min(next)
            .min(all.len().saturating_sub(half()));
        let mut found = None;
        for c in lo..hi {
            if mag(corr(c - half(), c + half(), new)) > mag(corr(c - half(), c + half(), old)) {
                found = Some(c as i64 - at as i64);
                break;
            }
        }
        offsets.push(found.unwrap_or(i64::MAX));
    }
    // The same takeover with no window: each sample's frequency from the
    // phase step since the sample before (x[n]·conj(x[n−1])), averaged over
    // INST_SMOOTH samples; the takeover is the first sample from which the
    // averaged frequency stays nearer the new tone than the old for
    // INST_STAY samples. Needs a clean tone (findings 19: the USRP's, ~70
    // dB over the median bin, a few hundred Hz of per-sample noise against
    // the tones' 100 kHz spacing).
    // 16: the tone's ~70 dB is per FFT bin; per sample it is ~20-30 dB, a
    // few tens of kHz of frequency noise, which 16 samples bring to a few.
    const INST_SMOOTH: usize = 16;
    const INST_STAY: usize = 32;
    const INST_SEARCH: usize = 4_000;
    let inst = |n: usize| -> f64 {
        let (a, b) = (all[n - 1], all[n]);
        let re = b.i as f64 * a.i as f64 + b.q as f64 * a.q as f64;
        let im = b.q as f64 * a.i as f64 - b.i as f64 * a.q as f64;
        im.atan2(re) * rate / (2.0 * PI)
    };
    let mut inst_offsets: Vec<i64> = Vec::new();
    for (k, b) in breaks.iter().enumerate() {
        let at = b.at_sample as usize;
        let (old, new) = (tones[k % 2], tones[(k + 1) % 2]);
        // Centred averages, so the estimate is not shifted by the average.
        let (prev, next) = bounds(k);
        let lo = at
            .saturating_sub(INST_SEARCH)
            .max(prev + INST_SMOOTH)
            .max(1 + INST_SMOOTH / 2);
        let hi = (at + INST_SEARCH)
            .min(next)
            .min(all.len().saturating_sub(INST_SMOOTH + INST_STAY));
        let smooth: Vec<f64> = (lo..hi + INST_STAY)
            .map(|n| {
                (n - INST_SMOOTH / 2..n + INST_SMOOTH / 2)
                    .map(inst)
                    .sum::<f64>()
                    / INST_SMOOTH as f64
            })
            .collect();
        let nearer_new = |i: usize| (smooth[i] - new).abs() < (smooth[i] - old).abs();
        let found = (0..hi.saturating_sub(lo))
            .find(|&i| (i..i + INST_STAY).all(nearer_new))
            .map(|i| (lo + i) as i64 - at as i64);
        inst_offsets.push(found.unwrap_or(i64::MAX));
        // `TIERCHECK_DUMP=N`: the first N breaks' per-sample frequency
        // (unaveraged, kHz) and magnitude from 40 before `at_sample` to
        // 100 after, to stderr — what the takeover looks like.
        if let Some(n) = std::env::var("TIERCHECK_DUMP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            && k < n
            && at > 41
            && at + 101 < all.len()
        {
            let f: Vec<i64> = (at - 40..at + 100)
                .map(|m| (inst(m) / 1e3).round() as i64)
                .collect();
            let a: Vec<i64> = (at - 40..at + 100)
                .map(|m| ((all[m].i as f64).hypot(all[m].q as f64) / 256.0).round() as i64)
                .collect();
            eprintln!(
                "DUMP break {k} at {at} old {old:.0} new {new:.0}\n  kHz from at-40: {f:?}\n  |x|/256: {a:?}"
            );
        }
    }
    // When the signal comes back: after a break the samples drop to near
    // silence (findings 19's dump: |x| ~1-2 LSB against a tone of 15-46
    // ×256) before the new configuration's tone appears. The recovery is
    // the first sample from `at_sample` whose 16-sample average |x| reaches
    // half the segment's steady |x| (the median of 16-sample averages over
    // the segment's middle).
    const AMP_AVG: usize = 16;
    let amp = |a: usize| -> f64 {
        all[a..a + AMP_AVG]
            .iter()
            .map(|v| (v.i as f64).hypot(v.q as f64))
            .sum::<f64>()
            / AMP_AVG as f64
    };
    let mut recover_offsets: Vec<i64> = Vec::new();
    for (k, b) in breaks.iter().enumerate() {
        let at = b.at_sample as usize;
        let end = edges.get(k + 2).copied().unwrap_or(all.len());
        if end <= at + 4 * GUARD + AMP_AVG {
            recover_offsets.push(i64::MAX);
            continue;
        }
        let mut steady: Vec<f64> = (at + 2 * GUARD..end - 2 * GUARD)
            .step_by(AMP_AVG)
            .map(amp)
            .collect();
        steady.sort_by(f64::total_cmp);
        let half_level = steady[steady.len() / 2] / 2.0;
        let found = (at..end - AMP_AVG)
            .find(|&a| amp(a) >= half_level)
            .map(|a| a as i64 - at as i64);
        recover_offsets.push(found.unwrap_or(i64::MAX));
        // `TIERCHECK_DUMP=N`: the first N breaks' |x| profile, 1,024-sample
        // means from 4,096 before `at_sample` to 50 steps after, to stderr.
        if let Some(n) = std::env::var("TIERCHECK_DUMP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            && k < n
            && at > 4_096
        {
            let prof: Vec<i64> = (0..54)
                .filter_map(|j| {
                    let a = at - 4_096 + j * 1_024;
                    (a + 1_024 <= all.len()).then(|| {
                        (all[a..a + 1_024]
                            .iter()
                            .map(|v| (v.i as f64).hypot(v.q as f64))
                            .sum::<f64>()
                            / 1_024.0
                            / 256.0
                            * 10.0)
                            .round() as i64
                    })
                })
                .collect();
            eprintln!(
                "PROFILE break {k}: steady half-level {:.1}, |x|/25.6 per 1,024 samples from at-4096: {prof:?}",
                half_level / 25.6
            );
            // The same blocks split: the new tone's amplitude (|correlation|
            // / N) and the DC (|mean|), both /25.6.
            let (old_f, new_f) = (tones[k % 2], tones[(k + 1) % 2]);
            let split: Vec<(i64, i64, i64)> = (0..54)
                .filter_map(|j| {
                    let a = at - 4_096 + j * 1_024;
                    (a + 1_024 <= all.len()).then(|| {
                        let t_old = mag(corr(a, a + 1_024, old_f)) / 1_024.0 / 25.6;
                        let t_new = mag(corr(a, a + 1_024, new_f)) / 1_024.0 / 25.6;
                        let (si, sq) = all[a..a + 1_024]
                            .iter()
                            .fold((0.0, 0.0), |(i, q), v| (i + v.i as f64, q + v.q as f64));
                        let dc = (si / 1_024.0).hypot(sq / 1_024.0) / 25.6;
                        (
                            t_old.round() as i64,
                            t_new.round() as i64,
                            dc.round() as i64,
                        )
                    })
                })
                .collect();
            eprintln!(
                "SPLIT break {k}: (old tone, new tone, dc) per 1,024 samples from at-4096: {split:?}"
            );
        }
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
        if *off == i64::MAX || at < 2 * half() {
            continue;
        }
        let before = phase(corr(at - 2 * half(), at, old));
        cont_start.push(wrap_deg(phase(corr(at, at + 2 * half(), old)) - before).round() as i64);
        let end = (at as i64 + off) as usize - half();
        if end >= at + 2 * half() {
            cont_end
                .push(wrap_deg(phase(corr(end - 2 * half(), end, old)) - before).round() as i64);
        }
    }
    // Segments as the samples see them: each break moved by its offset.
    let mut edges: Vec<usize> = vec![0];
    edges.extend(breaks.iter().zip(&offsets).map(|(b, o)| {
        if *o == i64::MAX {
            b.at_sample as usize
        } else {
            (b.at_sample as i64 + o) as usize
        }
    }));
    edges.push(all.len());
    // Lost runs inside segments: the phase of consecutive windows against
    // the segment's tone; a lost block steps it by ~210° here.
    let mut jumps = 0usize;
    // The first jumps: (samples after the segment's start, degrees).
    let mut jump_at: Vec<(i64, i64)> = Vec::new();
    let mut windows = 0usize;
    // Segments with a run of jumps: (segment, jumps).
    let mut jump_segments: Vec<(usize, usize)> = Vec::new();
    for k in 0..edges.len() - 1 {
        let f = tones[k % 2];
        let (a, b) = (edges[k] + GUARD, edges[k + 1].saturating_sub(GUARD));
        let jumps_before = jumps;
        let mut prev: Option<f64> = None;
        let mut w = a;
        while w + 2 * half() <= b {
            let z = corr(w, w + 2 * half(), f);
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
            w += 2 * half();
        }
        if jumps - jumps_before >= JUMP_RUN_MAP {
            jump_segments.push((k, jumps - jumps_before));
            // The segment's first MAP_BLOCKS blocks of MAP_BLOCK samples (and
            // the last ones of the segment before), each marked by the tone
            // that dominates it: 'n' this segment's, 'o' the other, '.' at
            // neither (6 dB).
            let mark = |s: usize| {
                let p = |f: f64| {
                    let z = corr(s, s + MAP_BLOCK, f);
                    z.0 * z.0 + z.1 * z.1
                };
                let (mine, other) = (p(f), p(tones[(k + 1) % 2]));
                if mine > 4.0 * other {
                    'n'
                } else if other > 4.0 * mine {
                    'o'
                } else {
                    '.'
                }
            };
            let before: String = (1..=MAP_BLOCKS / 4)
                .rev()
                .filter_map(|i| edges[k].checked_sub(i * MAP_BLOCK))
                .map(mark)
                .collect();
            let after: String = (0..MAP_BLOCKS)
                .map(|i| edges[k] + i * MAP_BLOCK)
                .take_while(|s| s + MAP_BLOCK <= all.len())
                .map(mark)
                .collect();
            let b = k.checked_sub(1).and_then(|i| breaks.get(i));
            eprintln!(
                "JUMPMAP segment {k} ({} jumps): edge {} (break at_sample {:?}, offset {:?}, quiet {:?} ms, dropped {:?}) blocks of {MAP_BLOCK}: {before}|{after}",
                jumps - jumps_before,
                edges[k],
                b.map(|b| b.at_sample),
                k.checked_sub(1).and_then(|i| offsets.get(i)),
                b.map(|b| b.quiet.as_secs_f64() * 1e3),
                b.map(|b| b.dropped_before.blocks)
            );
        }
    }
    // Which tone each segment carries: a segment whose power sits at the
    // other tone is a change that did not take where its break says (a
    // phase "jump" of a whole tone step per window, findings 21).
    // (segment, its first sample, length, expected-over-other dB)
    let mut wrong_tone: Vec<(usize, usize, usize, i64)> = Vec::new();
    for k in 0..edges.len() - 1 {
        let (a, b) = (edges[k] + GUARD, edges[k + 1].saturating_sub(GUARD));
        if b <= a + 2 * half() {
            continue;
        }
        let p = |f: f64| {
            let z = corr(a, b, f);
            z.0 * z.0 + z.1 * z.1
        };
        let (want, other) = (p(tones[k % 2]), p(tones[(k + 1) % 2]));
        if want <= other {
            wrong_tone.push((
                k,
                edges[k],
                edges[k + 1] - edges[k],
                (10.0 * (want / other.max(f64::MIN_POSITIVE)).log10()).round() as i64,
            ));
        }
    }
    let exact = offsets.iter().filter(|o| o.abs() <= 1).count();
    let quiet_mean = quiet_ms.iter().sum::<f64>() / quiet_ms.len().max(1) as f64;
    Ok(format!(
        "\"run\":\"lossless\",\"rate\":{rate},\"depth\":{depth},{},\"kept\":{},\"retunes\":{done},\"quiet_ms_mean\":{quiet_mean:.1},\"tones_hz\":[{:.0},{:.0}],\"break_offsets\":{offsets:?},\"inst_offsets\":{inst_offsets:?},\"recover_offsets\":{recover_offsets:?},\"breaks_within_1_sample\":{exact},\"phase_windows\":{windows},\"phase_jumps\":{jumps},\"first_jumps_from_segment_start\":{jump_at:?},\"wrong_tone_segments\":{wrong_tone:?},\"jump_segments\":{jump_segments:?},\"segments\":{},\"continuity_at_break_deg\":{cont_start:?},\"continuity_to_takeover_deg\":{cont_end:?},\"drain_ms\":{drain_ms:?},\"break_dropped\":{:?}",
        report_json(&fin.report, fin.tail.len()),
        all.len(),
        tones[0],
        tones[1],
        edges.len() - 1,
        breaks
            .iter()
            .map(|b| b.dropped_before.blocks)
            .collect::<Vec<_>>()
    ))
}

/// A stream with no changes: 3 s, then the report.
fn plain(rate: f64) -> Result<String, String> {
    let rtl = open(rate)?;
    let t = Instant::now();
    let mut s = rtl
        .into_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.error.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut n = 0usize;
    while t.elapsed() < Duration::from_secs(3) {
        n += s
            .recv_into(&mut buf, RECV_TIMEOUT_S)
            .map_err(|e| e.to_string())?
            .n_samples;
    }
    let fin = s.finish().map_err(|e| e.to_string())?;
    Ok(format!(
        "\"run\":\"plain\",\"rate\":{rate},{},\"received_before_finish\":{n}",
        report_json(&fin.report, fin.tail.len() / 2)
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let retunes: u32 = args
        .iter()
        .position(|a| a == "--retunes")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    if let Some(h) = args
        .iter()
        .position(|a| a == "--half")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
    {
        let _ = HW_SET.set(h);
    }
    let rate: f64 = args
        .iter()
        .position(|a| a == "--rate")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RATE);
    // `--rate-floor SPS` (with --features bench): rates under the driver's
    // measured span accepted (`demod::set_bench_rate_floor`), to measure
    // them.
    #[cfg(feature = "bench")]
    if let Some(f) = args
        .iter()
        .position(|a| a == "--rate-floor")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
    {
        sdr_rtl::demod::set_bench_rate_floor(Some(f));
    }
    // `--no-source`: no tone (a host without a port for the source); the
    // loss counts hold, the break and phase analysis is of noise.
    let mut src = if args.iter().any(|a| a == "--no-source") {
        None
    } else {
        // `--source-cmd "<command line>"`: the source by another command
        // (a host whose default build lacks a dependency of the source's).
        let cmd = args
            .iter()
            .position(|a| a == "--source-cmd")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .unwrap_or(SOURCE_CMD);
        let mut s = Source::spawn(cmd, 0, -15)?;
        s.tone(OPEN_RF_HZ + tone_offset(rate))?;
        Some(s)
    };
    let only_lossless = args.iter().any(|a| a == "--lossless-only");
    let drain_ms: Option<u64> = args
        .iter()
        .position(|a| a == "--drain-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let runs = if only_lossless {
        vec![lossless(retunes, rate, drain_ms)]
    } else {
        vec![
            lossless(retunes, rate, drain_ms),
            plain(1.8e6),
            plain(2.4e6),
        ]
    };
    for r in runs {
        match r {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"error\":\"{e}\"}}"),
        }
    }
    if let Some(s) = src.as_mut() {
        s.off()?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("tiercheck: {e}");
        std::process::exit(1);
    }
}
