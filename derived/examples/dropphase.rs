//! Are the blocks USB_EPA_FIFO_CFG[31:24] counts samples lost, and what
//! loses them? A steady tone through the stream: a lost run of samples
//! shows as a jump in the tone's phase. A 188-byte block is 94 samples;
//! at +300 kHz and 2.4 Msps that is 11.75 cycles, so k lost blocks jump
//! the phase by −k·270° ≡ +k·90°. A glitch in the source has no such
//! signature.
//!
//!   dropphase [--seconds S] [--poll-ms P] [--poll-kind take|usbread|usbwrite|demodread|demodwrite]
//!
//! Built with `--features bench`. Power cycle, open (300 MHz), the tone at
//! +300 kHz (backoff −15 dB), the stream started (`RtlStream::start`
//! clears the chip's counter) and read without pause. With `--poll-ms`
//! the driver's `take_dropped_blocks` runs every P ms while streaming
//! (control traffic during the stream); without it, once after the stop.
//! Per sample the phase step arg(x[n]·x*[n−1]); a step more than
//! `JUMP_RAD` from the tone's own step is a jump, its excess angle binned
//! in 30° bins. One JSON line: the blocks taken in all, the samples they
//! stand for, the samples short of rate × time, and the jumps.

mod bench;

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, RxSource};
use sdr_rtl_derived::protocol::{BLK_DROP_COUNTER, PAUSE_OFF, PAUSE_PAGE};
use sdr_rtl_derived::{OPEN_RF_HZ, Rtl};

const RATE_SPS: f64 = 2.4e6;
const OFFSET_HZ: f64 = 300e3;
/// A phase step this far from the tone's is a jump (the tone's own
/// noise at ~57 dB SNR is a few milliradians).
const JUMP_RAD: f64 = 0.5;
/// Samples after a poll inside which a jump is counted as near it.
const NEAR_POLL_SAMPLES: u64 = (RATE_SPS * 0.02) as u64;
const BINS: usize = 12;

fn wrap(a: f64) -> f64 {
    (a + PI).rem_euclid(2.0 * PI) - PI
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str, d: u64| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (secs, poll_ms) = (get("--seconds", 10), get("--poll-ms", 0));
    let kind = args.iter().position(|a| a == "--poll-kind").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "take".into());
    let cycle = power_cycle()?;
    let mut src = Source::spawn(SOURCE_CMD, 0, -15)?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_rate(RATE_SPS).map_err(|e| e.to_string())?;
    src.tone(OPEN_RF_HZ + OFFSET_HZ)?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let pause_reg = rtl.link().demod_read8(PAUSE_PAGE, PAUSE_OFF).map_err(|e| e.to_string())?;
    let t_start = Instant::now();
    let (mut taken, mut saturated) = (0u64, false);
    let expected = 2.0 * PI * OFFSET_HZ / RATE_SPS;
    let t = Instant::now();
    let (mut n, mut jumps, mut overflows, mut polls, mut near_poll) = (0u64, 0u64, 0usize, 0u64, 0u64);
    let mut bins = [0u64; BINS];
    let mut prev: Option<Iq> = None;
    let mut step_est = expected;
    let mut last_poll_n: Option<u64> = None;
    let mut next_poll = Duration::from_millis(poll_ms);
    while t.elapsed() < Duration::from_secs(secs) {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        overflows += info.overflow as usize;
        for v in &buf[..info.n_samples] {
            if let Some(p) = prev {
                let (a, b) = ((v.i as f64, v.q as f64), (p.i as f64, p.q as f64));
                let step = (a.1 * b.0 - a.0 * b.1).atan2(a.0 * b.0 + a.1 * b.1);
                let excess = wrap(step - step_est);
                if excess.abs() > JUMP_RAD {
                    jumps += 1;
                    let deg = excess.to_degrees().rem_euclid(360.0);
                    bins[((deg / (360.0 / BINS as f64)) as usize).min(BINS - 1)] += 1;
                    if last_poll_n.is_some_and(|lp| n >= lp && n - lp <= NEAR_POLL_SAMPLES) {
                        near_poll += 1;
                    }
                } else {
                    // Track the tone's own step (the source/dongle offset).
                    step_est += 1e-4 * excess;
                }
            }
            prev = Some(*v);
            n += 1;
        }
        if poll_ms > 0 && t.elapsed() >= next_poll {
            match kind.as_str() {
                "take" => {
                    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
                    taken += d.blocks as u64;
                    saturated |= d.saturated;
                }
                "usbread" => drop(rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(|e| e.to_string())?),
                "usbwrite" => rtl.link().usb_write8(BLK_DROP_COUNTER, 0).map_err(|e| e.to_string())?,
                "demodread" => drop(rtl.link().demod_read8(PAUSE_PAGE, PAUSE_OFF).map_err(|e| e.to_string())?),
                // The pause register written with the value it holds.
                "demodwrite" => rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[pause_reg]).map_err(|e| e.to_string())?,
                other => return Err(format!("unknown --poll-kind {other}")),
            }
            polls += 1;
            // The samples in hand at the read; what the read disturbs
            // arrives after them.
            last_poll_n = Some(n);
            next_poll += Duration::from_millis(poll_ms);
        }
    }
    let elapsed = t_start.elapsed().as_secs_f64();
    s.stop().map_err(|e| e.to_string())?;
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    taken += d.blocks as u64;
    saturated |= d.saturated;
    let shortfall = RATE_SPS * elapsed - n as f64;
    println!(
        "{{\"cycle\":\"{cycle}\",\"seconds\":{secs},\"poll_ms\":{poll_ms},\"poll_kind\":\"{kind}\",\"polls\":{polls},\"samples\":{n},\"overflows\":{overflows},\"dropped_blocks\":{taken},\"any_take_saturated\":{saturated},\"dropped_samples\":{},\"shortfall_samples\":{shortfall:.0},\"jumps\":{jumps},\"jumps_within_20ms_after_a_poll\":{near_poll},\"jump_angle_bins_30deg\":{bins:?},\"tone_step_rad\":{step_est:.6},\"expected_step_rad\":{expected:.6}}}",
        taken * 94
    );
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dropphase: {e}");
        std::process::exit(1);
    }
}
