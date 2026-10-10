//! A retune bracketed by the stream's pause bit: is it reliable, and does
//! it leave the books exact? Per cycle: `RtlStream::pause` (page 1 0x01
//! bit 2), the dropped-block counter read (a USB-block read with the
//! output paused), `Rtl::set_freq` (alternating 300.0 / 300.1 MHz), the
//! counter read again, cleared, `RtlStream::resume`.
//!
//!   bracket [--rate SPS] [--cycles N] [--every-ms M] [--blind]
//!
//! Built with `--features bench`. From a power cycle, gains 8/8/2, the
//! stream read without pause between cycles. Reported: errors (an
//! endpoint stall is the risk, findings 16), the counter at each read
//! (before the retune: what the pause write cost; after: what the retune
//! cost while paused), the time paused (host clock), and the samples
//! received against rate × (time − time paused) — the shortfall the
//! counted blocks should explain. One JSON line.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::protocol::{BLK_DROP_COUNTER, PAGE1_0X01_POWER_ON, PAUSE_BIT, PAUSE_OFF, PAUSE_PAGE};
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::blocking::Rtl;

const RETUNE_STEP_HZ: f64 = 100e3;
const BLOCK_SAMPLES: f64 = 94.0;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(1.8e6);
    let cycles: u32 = get("--cycles").and_then(|v| v.parse().ok()).unwrap_or(50);
    let every = Duration::from_millis(get("--every-ms").and_then(|v| v.parse().ok()).unwrap_or(200));
    // `--blind`: pause and resume as one write each of the whole register
    // (its power-on 0x10 with or without bit 2; the I²C repeater is closed
    // between operations) in place of the read, write and read-back.
    let blind = args.iter().any(|a| a == "--blind");
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (mut n, mut overflows, mut done) = (0u64, 0usize, 0u32);
    let (mut before_pause, mut during_retune) = (0u64, 0u64);
    let (mut max_before, mut max_during) = (0u8, 0u8);
    let mut paused = Duration::ZERO;
    let mut errors: Vec<String> = Vec::new();
    let mut next = every;
    while done < cycles {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        n += info.n_samples as u64;
        overflows += info.overflow as usize;
        if t.elapsed() < next {
            continue;
        }
        let t_p = Instant::now();
        let r = (|| -> Result<(u8, u8), String> {
            if blind {
                rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[PAGE1_0X01_POWER_ON | PAUSE_BIT]).map_err(|e| format!("pause: {e}"))?;
            } else {
                s.pause().map_err(|e| format!("pause: {e}"))?;
            }
            let c1 = rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(|e| format!("read 1: {e}"))?;
            rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ + RETUNE_STEP_HZ * (done % 2) as f64)).map_err(|e| format!("retune: {e}"))?;
            let c2 = rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(|e| format!("read 2: {e}"))?;
            rtl.link().usb_write8(BLK_DROP_COUNTER, 0).map_err(|e| format!("clear: {e}"))?;
            if blind {
                rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[PAGE1_0X01_POWER_ON]).map_err(|e| format!("resume: {e}"))?;
            } else {
                s.resume().map_err(|e| format!("resume: {e}"))?;
            }
            Ok((c1, c2))
        })();
        paused += t_p.elapsed();
        match r {
            Ok((c1, c2)) => {
                before_pause += c1 as u64;
                during_retune += c2.saturating_sub(c1) as u64;
                max_before = max_before.max(c1);
                max_during = max_during.max(c2.saturating_sub(c1));
            }
            Err(e) => {
                errors.push(e);
                if errors.len() > 3 {
                    break;
                }
            }
        }
        done += 1;
        next += every;
    }
    // Read on so the last resume's samples are in.
    let t_end = t.elapsed() + every;
    while t.elapsed() < t_end {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        n += info.n_samples as u64;
    }
    let elapsed = t.elapsed().as_secs_f64();
    s.stop().map_err(|e| e.to_string())?;
    let at_stop = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    let streaming_s = elapsed - paused.as_secs_f64();
    println!(
        "{{\"cycle\":\"{cycle}\",\"rate\":{rate},\"blind\":{blind},\"cycles\":{done},\"errors\":{errors:?},\"seconds\":{elapsed:.3},\"paused_s\":{:.3},\"paused_ms_mean\":{:.2},\"samples\":{n},\"overflows\":{overflows},\"shortfall_vs_streaming_time\":{:.0},\"blocks_before_pause_total\":{before_pause},\"blocks_before_pause_max\":{max_before},\"blocks_during_retune_total\":{during_retune},\"blocks_during_retune_max\":{max_during},\"blocks_at_stop\":{},\"counted_samples\":{:.0}}}",
        paused.as_secs_f64(),
        paused.as_secs_f64() * 1e3 / done.max(1) as f64,
        rate * streaming_s - n as f64,
        at_stop.blocks,
        (before_pause + during_retune + at_stop.blocks as u64) as f64 * BLOCK_SAMPLES
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("bracket: {e}");
        std::process::exit(1);
    }
}
