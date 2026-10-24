//! A retune's gap in the sample clock, on the bench: does "samples handed
//! out + each break's pause at the rate + samples counted lost" account
//! for the time the stream ran?
//!
//!   gapcheck [--rate SPS] [--seconds S] [--retunes N]
//!
//! Run from the workspace root (`cargo run --release -p sdr-rtl
//! --features bench --example gapcheck`), one dongle attached; no tone
//! needed. A stream at the rate (default 2.048 Msps) is read for S seconds
//! (default 12) while a second thread makes N retunes (default 10),
//! alternating 300.0 / 300.1 MHz, evenly spaced after the first second.
//! The run is timed from just before the first read (the stream starts on
//! it) to just before the finish. One JSON line: the samples the dongle
//! produced in that time (elapsed × rate) against delivered + tail, the
//! breaks' summed pause at the rate, the counted loss (chip and host),
//! and what is left over, in total and per break — without the gaps the
//! left-over would be the pauses themselves (~60-70 ms a break); with
//! them, the FIFO's discarded tail (~0.5-0.9 ms a break, sdr-rtl findings
//! 16.6) plus the start's and finish's edges.

use std::io::Read;
use std::time::{Duration, Instant};

use sdr_core::TuneRequest;
use sdr_rtl::blocking::{Rtl, Stream};
use sdr_rtl::protocol::BLK_DROP_BLOCK_BYTES;

const RF_HZ: [f64; 2] = [300.0e6, 300.1e6];

fn arg(args: &[String], name: &str, default: f64) -> f64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rate = arg(&args, "--rate", 2.048e6);
    let seconds = arg(&args, "--seconds", 12.0);
    let retunes = arg(&args, "--retunes", 10.0) as usize;
    let r = (|| -> Result<String, String> {
        // `SDR_BENCH_RTL`: which dongle on a host with more than one.
        let mut rtl = Rtl::open(&std::env::var("SDR_BENCH_RTL").unwrap_or_default())
            .map_err(|e| e.to_string())?;
        rtl.set_rate(rate).map_err(|e| e.to_string())?;
        rtl.set_freq(&TuneRequest::auto(RF_HZ[0]))
            .map_err(|e| e.to_string())?;
        let s = rtl.into_stream(0, 0).map_err(|e| e.to_string())?;
        let (mut read, control) = s.split();
        let run = Duration::from_secs_f64(seconds);
        let every = (run - Duration::from_secs(1)) / (retunes as u32 + 1);
        let t0 = Instant::now();
        // Each retune at its own time from t0 (a sleep between retunes
        // would add every retune's own ~75 ms, and the last ones would
        // come after the reader stopped: unread, the chip's FIFO
        // overflows and the counter saturates).
        let changer = std::thread::spawn(move || -> Result<sdr_rtl::blocking::Control, String> {
            for k in 0..retunes {
                let due = Duration::from_secs(1) + every * (k as u32 + 1);
                std::thread::sleep(due.saturating_sub(t0.elapsed()));
                control
                    .retune(&TuneRequest::auto(RF_HZ[(k + 1) % 2]))
                    .map_err(|e| e.to_string())?;
            }
            Ok(control)
        });
        let mut buf = vec![0u8; 16_384];
        let mut bytes = 0u64;
        // Read until the run is over and the last retune is done.
        while t0.elapsed() < run || !changer.is_finished() {
            bytes += read.read(&mut buf).map_err(|e| e.to_string())? as u64;
        }
        let control = changer
            .join()
            .map_err(|_| "the retune thread panicked".to_string())??;
        let s = Stream::reunite(read, control).map_err(|_| "reunite".to_string())?;
        let t1 = t0.elapsed();
        let f = s.finish().map_err(|e| e.to_string())?;
        let rep = &f.report;
        let produced = t1.as_secs_f64() * rate;
        let taken = (bytes + f.tail.len() as u64) as f64 / 2.0;
        let gaps: f64 = rep
            .breaks
            .iter()
            .map(|b| b.quiet.as_secs_f64() * rate)
            .sum();
        let chip_lost = (rep.dropped_blocks * BLK_DROP_BLOCK_BYTES as u64 / 2) as f64;
        let lost = chip_lost.max(rep.host_lost_samples as f64);
        let left = produced - taken - gaps - lost;
        let n = rep.breaks_total.max(1) as f64;
        Ok(format!(
            "{{\"rate\":{rate},\"seconds\":{:.3},\"breaks\":{},\"produced\":{produced:.0},\"taken\":{taken:.0},\"gaps\":{gaps:.0},\"gap_ms_mean\":{:.1},\"chip_lost\":{chip_lost:.0},\"host_lost\":{},\"left_over\":{left:.0},\"left_over_ms\":{:.2},\"left_over_ms_per_break\":{:.3},\"without_gaps_ms\":{:.1}}}",
            t1.as_secs_f64(),
            rep.breaks_total,
            gaps / rate * 1e3 / n,
            rep.host_lost_samples,
            left / rate * 1e3,
            left / rate * 1e3 / n,
            (left + gaps) / rate * 1e3,
        ))
    })();
    match r {
        Ok(line) => println!("{line}"),
        Err(e) => println!("{{\"error\":{e:?}}}"),
    }
}
