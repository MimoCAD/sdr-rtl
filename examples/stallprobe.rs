//! Which access after the pause makes the chip refuse a demodulator
//! write ("endpoint stalled", findings 16.4: a counter read between the
//! pause and the stop, then the stop's write, failed in 7 runs of 9)?
//!
//!   stallprobe [--rate SPS] [--runs N] [--seqs a,b,…]
//!
//! Built with `--features bench`. Per run: open (a power cycle first,
//! and again after any failure), the stream started, 0.5 s read, then one
//! sequence of accesses; the first failing step is recorded, the stream
//! dropped. The sequences (`pause` = page 1 0x01 ← 0x14, one write, then
//! 5 ms; `usbread` = the counter's byte; `stop` = `RtlStream::stop`, the
//! read-modify-write of page 0 0x06):
//! - `s1`: pause, usbread, stop — 16.4's failing order;
//! - `s2`: pause, usbread, demod read of page 0 0x06;
//! - `s3`: pause, usbread, demod write of page 0 0x06 (its value);
//! - `s4`: pause, usbread, demod write of page 1 0x01 (0x14);
//! - `s5`: pause, demod read of page 0 0x06, demod write of it (control:
//!   no USB-block access);
//! - `s6`: no pause; usbread, stop (the samples flowing);
//! - `s7`: pause, 50 ms, usbread, stop (time after the pause);
//! - `s8`: pause, stop, usbread (16.4's order that never failed).
//!
//! One JSON line per sequence: runs, failures, the failing step.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::Iq;
use sdr_rtl::protocol::{BLK_DROP_COUNTER, PAGE1_0X01_POWER_ON, PAUSE_BIT, PAUSE_OFF, PAUSE_PAGE, START_OFF, START_PAGE};
use sdr_rtl::blocking::{Rtl, RtlStream};

const DRAIN: Duration = Duration::from_millis(5);
const RUN_FOR: Duration = Duration::from_millis(500);

fn steps(seq: &str) -> &'static [&'static str] {
    match seq {
        "s1" => &["pause", "usbread", "stop"],
        "s2" => &["pause", "usbread", "demodread"],
        "s3" => &["pause", "usbread", "demodwrite_p0"],
        "s4" => &["pause", "usbread", "demodwrite_p1"],
        "s5" => &["pause", "demodread", "demodwrite_p0"],
        "s6" => &["usbread", "stop"],
        "s7" => &["pause", "wait50", "usbread", "stop"],
        "s8" => &["pause", "stop", "usbread"],
        _ => &[],
    }
}

fn step(rtl: &Rtl, s: &mut RtlStream, name: &str, p0_06: u8) -> Result<(), String> {
    let e = |e: sdr_core::Error| e.to_string();
    match name {
        "pause" => {
            rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[PAGE1_0X01_POWER_ON | PAUSE_BIT]).map_err(e)?;
            std::thread::sleep(DRAIN);
        }
        "usbread" => drop(rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(e)?),
        "stop" => s.stop().map_err(e)?,
        "demodread" => drop(rtl.link().demod_read8(START_PAGE, START_OFF).map_err(e)?),
        "demodwrite_p0" => rtl.link().demod_write(START_PAGE, START_OFF, &[p0_06]).map_err(e)?,
        "demodwrite_p1" => rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[PAGE1_0X01_POWER_ON | PAUSE_BIT]).map_err(e)?,
        "wait50" => std::thread::sleep(Duration::from_millis(50)),
        other => return Err(format!("unknown step {other}")),
    }
    Ok(())
}

/// One run: Ok(None) when every step passed, Ok(Some((index, error)))
/// at the first failing step.
fn run_once(seq: &str, rate: f64) -> Result<Option<(usize, String)>, String> {
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let p0_06 = rtl.link().demod_read8(START_PAGE, START_OFF).map_err(|e| e.to_string())?;
    let t = Instant::now();
    while t.elapsed() < RUN_FOR {
        s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
    }
    for (i, name) in steps(seq).iter().enumerate() {
        if let Err(e) = step(&rtl, &mut s, name, p0_06) {
            return Ok(Some((i, format!("{name}: {e}"))));
        }
    }
    Ok(None)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(1.2e6);
    let runs: u32 = get("--runs").and_then(|v| v.parse().ok()).unwrap_or(10);
    let seqs = get("--seqs").unwrap_or_else(|| "s1,s2,s3,s4,s5,s6,s7,s8".into());
    power_cycle()?;
    for seq in seqs.split(',') {
        let (mut failures, mut firsts) = (0u32, Vec::new());
        for _ in 0..runs {
            match run_once(seq, rate) {
                Ok(None) => {}
                Ok(Some((i, e))) => {
                    failures += 1;
                    if firsts.len() < 3 {
                        firsts.push(format!("step {i} {e}"));
                    }
                    power_cycle()?;
                }
                Err(e) => {
                    failures += 1;
                    if firsts.len() < 3 {
                        firsts.push(format!("setup: {e}"));
                    }
                    power_cycle()?;
                }
            }
        }
        println!("{{\"seq\":\"{seq}\",\"steps\":{:?},\"rate\":{rate},\"runs\":{runs},\"failures\":{failures},\"first\":{firsts:?}}}", steps(seq));
    }
    power_cycle()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("stallprobe: {e}");
        std::process::exit(1);
    }
}
