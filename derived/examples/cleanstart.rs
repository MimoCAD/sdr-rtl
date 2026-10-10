//! The stream's own start and stop: what they drop, and whether starting
//! paused and stopping from a pause drops nothing. Findings 16.3's
//! baselines (no operation during the stream) dropped 10 blocks at
//! 1.024 Msps to 73 at 2.048, every run.
//!
//!   cleanstart [--rates a,b,…] [--reps N] [--seconds S]
//!
//! Built with `--features bench`. Per rate, mode and repetition, from a
//! power cycle (gains 8/8/2): the stream started, read without pause for
//! S seconds (no control traffic), stopped, and the dropped-block counter
//! taken after the stop (free). The modes:
//! - `plain`: `RtlStream::start` and `RtlStream::stop` as they are (the
//!   stop is a read-modify-write of page 0 0x06 while the samples flow);
//! - `clean_start`: page 1 0x01 written 0x14 (the pause bit, one write,
//!   the stream not yet started) before `start`, so the chip starts
//!   paused; then one write of 0x10 resumes it;
//! - `clean_stop`: one write of 0x14 pauses the stream, 5 ms for the
//!   FIFO to drain, then `stop` (its writes made with nothing flowing);
//! - `both`.
//!
//! In `both` the counter is also read once the stream is paused for the
//! stop, before the stop's own writes: the start's and the run's drops
//! apart from the stop's (the read costs nothing while paused).
//! One JSON line per run.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, RxSource};
use sdr_rtl_derived::protocol::{BLK_DROP_COUNTER, PAGE1_0X01_POWER_ON, PAUSE_BIT, PAUSE_OFF, PAUSE_PAGE};
use sdr_rtl_derived::Rtl;

const MODES: [&str; 4] = ["plain", "clean_start", "clean_stop", "both"];
const DRAIN: Duration = Duration::from_millis(5);

fn one(mode: &str, rate: f64, secs: f64) -> Result<String, String> {
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let clean_start = mode == "clean_start" || mode == "both";
    let clean_stop = mode == "clean_stop" || mode == "both";
    let pause = |rtl: &Rtl, on: bool| {
        let v = if on { PAGE1_0X01_POWER_ON | PAUSE_BIT } else { PAGE1_0X01_POWER_ON };
        rtl.link().demod_write(PAUSE_PAGE, PAUSE_OFF, &[v]).map_err(|e| e.to_string())
    };
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    if clean_start {
        pause(&rtl, true)?;
    }
    s.start().map_err(|e| e.to_string())?;
    if clean_start {
        pause(&rtl, false)?;
    }
    let t = Instant::now();
    let mut n = 0u64;
    let mut first_ms = f64::NAN;
    while t.elapsed().as_secs_f64() < secs {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        if n == 0 && info.n_samples > 0 {
            first_ms = t.elapsed().as_secs_f64() * 1e3;
        }
        n += info.n_samples as u64;
    }
    let mut before_stop = None;
    if clean_stop {
        pause(&rtl, true)?;
        std::thread::sleep(DRAIN);
        if mode == "both" {
            before_stop = Some(rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(|e| e.to_string())?);
        }
    }
    s.stop().map_err(|e| e.to_string())?;
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    if clean_stop {
        pause(&rtl, false)?;
    }
    Ok(format!(
        "\"cycle\":\"{cycle}\",\"mode\":\"{mode}\",\"rate\":{rate},\"seconds\":{secs},\"samples\":{n},\"first_samples_ms\":{first_ms:.2},\"blocks_before_stop\":{},\"blocks_total\":{},\"saturated\":{}",
        before_stop.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
        d.blocks,
        d.saturated
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let rates: Vec<f64> = get("--rates").unwrap_or_else(|| "1024000,1200000,1800000,2400000".into()).split(',').filter_map(|v| v.parse().ok()).collect();
    let reps: u32 = get("--reps").and_then(|v| v.parse().ok()).unwrap_or(3);
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(3.0);
    for &rate in &rates {
        for mode in MODES {
            for _ in 0..reps {
                match one(mode, rate, secs) {
                    Ok(body) => println!("{{{body}}}"),
                    Err(e) => println!("{{\"mode\":\"{mode}\",\"rate\":{rate},\"error\":\"{e}\"}}"),
                }
            }
        }
    }
    power_cycle()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("cleanstart: {e}");
        std::process::exit(1);
    }
}
