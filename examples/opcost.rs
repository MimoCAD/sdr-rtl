//! What each operation made while the stream runs costs, by rate: the
//! samples the chip drops (its counter, read after the stop, when a read
//! costs nothing) and the samples short of rate × time.
//!
//!   opcost [--rate SPS] [--ops none,usbread,demodread,retune,gain] [--every-ms N] [--seconds S] [--depth N]
//!
//! Built with `--features bench`. Per operation, from a power cycle: open
//! (300 MHz, gains 8/8/2), the stream started (the counter cleared by
//! `RtlStream::start`), read without pause for S seconds while the
//! operation runs every N ms (default 100; `usbread` at 5 ms by default —
//! about one USB transfer at 1.8 Msps); then the stream stopped and the
//! counter taken. The operations:
//! - `none`: nothing (the baseline: the start's and the stop's loss);
//! - `usbread`: one USB-block read (the counter's byte);
//! - `demodread`: one demodulator read (page 1 0x01);
//! - `retune`: `Rtl::set_freq`, alternating 300.0 and 300.1 MHz (the
//!   I²C repeater opened and closed, R26, R27, R16, R20-R22 written, R0-R4
//!   read, 10 ms settle);
//! - `gain`: `Rtl::set_stage_level` VGA, alternating codes 2 and 3 (R12
//!   written, R5-R15 read back).
//!
//! One JSON line per operation.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::protocol::{BLK_DROP_COUNTER, PAUSE_OFF, PAUSE_PAGE};
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::blocking::Rtl;

const RETUNE_STEP_HZ: f64 = 100e3;
const VGA_CODES: [usize; 2] = [2, 3];

fn one(op: &str, rate: f64, every: Duration, secs: f64, depth: usize) -> Result<String, String> {
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, depth).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (mut n, mut overflows, mut ops, mut op_time) = (0u64, 0usize, 0u64, Duration::ZERO);
    let mut next = every;
    let mut error = String::new();
    while t.elapsed().as_secs_f64() < secs {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        n += info.n_samples as u64;
        overflows += info.overflow as usize;
        if op != "none" && error.is_empty() && t.elapsed() >= next {
            let t_op = Instant::now();
            let r = match op {
                "usbread" => rtl.link().usb_read8(BLK_DROP_COUNTER).map(drop),
                "demodread" => rtl.link().demod_read8(PAUSE_PAGE, PAUSE_OFF).map(drop),
                "retune" => rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ + RETUNE_STEP_HZ * (ops % 2) as f64)).map(drop),
                "gain" => rtl.set_stage_level(Stage::Vga, VGA_CODES[(ops % 2) as usize]),
                other => return Err(format!("unknown op {other}")),
            };
            op_time += t_op.elapsed();
            if let Err(e) = r {
                error = e.to_string();
            }
            ops += 1;
            next += every;
        }
    }
    let elapsed = t.elapsed().as_secs_f64();
    s.stop().map_err(|e| e.to_string())?;
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    Ok(format!(
        "\"cycle\":\"{cycle}\",\"op\":\"{op}\",\"rate\":{rate},\"depth\":{depth},\"every_ms\":{},\"ops\":{ops},\"op_ms_mean\":{:.3},\"seconds\":{elapsed:.3},\"samples\":{n},\"shortfall\":{:.0},\"overflows\":{overflows},\"dropped_blocks\":{},\"saturated\":{},\"dropped_samples\":{},\"error\":\"{error}\"",
        every.as_millis(),
        if ops > 0 { op_time.as_secs_f64() * 1e3 / ops as f64 } else { 0.0 },
        rate * elapsed - n as f64,
        d.blocks,
        d.saturated,
        d.samples()
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(1.8e6);
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(5.0);
    let every_ms: Option<u64> = get("--every-ms").and_then(|v| v.parse().ok());
    let ops = get("--ops").unwrap_or_else(|| "none,usbread,demodread,retune,gain".into());
    let depth: usize = get("--depth").and_then(|v| v.parse().ok()).unwrap_or(QUEUE_DEPTH);
    for op in ops.split(',') {
        let default_ms = if op == "usbread" { 5 } else { 100 };
        let every = Duration::from_millis(every_ms.unwrap_or(default_ms));
        match one(op, rate, every, secs, depth) {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"op\":\"{op}\",\"rate\":{rate},\"error\":\"{e}\"}}"),
        }
    }
    power_cycle()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("opcost: {e}");
        std::process::exit(1);
    }
}
