//! `Stream` as `std::io::Read`, split across two threads: the reading half
//! on one, the control on another retuning while it reads.
//!
//!   readsplit [--rate SPS] [--seconds S] [--retunes N] [--out PATH]
//!
//! From a power cycle (gains 8/8/2, 300 MHz): the stream split; a thread
//! reads the wire bytes with `io::Read` in 4,095-byte reads (odd on
//! purpose: a read need not end on a sample) for S seconds (default 5),
//! into PATH if given (unsigned 8-bit I/Q, the layout RTL-SDR tools
//! write); meanwhile N retunes (default 6) alternate 300.1 and 300.0 MHz,
//! evenly spaced. Then the halves reunite and the stream finishes. One JSON
//! line: bytes read against rate × 2 × time, the breaks, the report.

mod bench;

use std::fs::File;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::TuneRequest;
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::Stage;
use sdr_rtl::blocking::{ReadHalf, Rtl, Stream};

const READ_BYTES: usize = 4095;
const RETUNE_STEP_HZ: f64 = 100e3;

fn run() -> Result<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(2.4e6);
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(5.0);
    let retunes: u32 = get("--retunes").and_then(|v| v.parse().ok()).unwrap_or(6);
    let out = get("--out");

    power_cycle()?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    let (mut reader, control) = rtl
        .into_stream(0, 0)
        .map_err(|e| e.error.to_string())?
        .split();

    let t = Instant::now();
    let reading = std::thread::spawn(move || -> Result<(ReadHalf, u64), String> {
        let mut sink: Box<dyn Write> = match &out {
            Some(p) => Box::new(File::create(p).map_err(|e| e.to_string())?),
            None => Box::new(std::io::sink()),
        };
        let mut buf = vec![0u8; READ_BYTES];
        let mut total = 0u64;
        while t.elapsed() < Duration::from_secs_f64(secs) {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            sink.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            total += n as u64;
        }
        Ok((reader, total))
    });

    let every = Duration::from_secs_f64(secs / (retunes + 1) as f64);
    for k in 0..retunes {
        if let Some(wait) = (every * (k + 1)).checked_sub(t.elapsed()) {
            std::thread::sleep(wait);
        }
        let rf = OPEN_RF_HZ + RETUNE_STEP_HZ * ((k + 1) % 2) as f64;
        control
            .retune(&TuneRequest::auto(rf))
            .map_err(|e| e.to_string())?;
    }
    let (reader, bytes) = reading
        .join()
        .map_err(|_| "the reading thread panicked".to_string())??;
    let elapsed = t.elapsed().as_secs_f64();
    let breaks = control.breaks();
    let stream =
        Stream::reunite(reader, control).map_err(|_| "the halves did not reunite".to_string())?;
    let fin = stream.finish().map_err(|e| e.to_string())?;
    let r = &fin.report;
    power_cycle()?;
    Ok(format!(
        "{{\"rate\":{rate},\"seconds\":{elapsed:.3},\"bytes_read\":{bytes},\"bytes_expected_approx\":{:.0},\"tail_bytes\":{},\"samples\":{},\"dropped_blocks\":{},\"saturated\":{},\"stall_retries\":{},\"breaks\":{},\"break_at\":{:?},\"break_dropped\":{:?},\"quiet_ms\":{:?}}}",
        rate * 2.0 * elapsed,
        fin.tail.len(),
        r.samples,
        r.dropped_blocks,
        r.saturated,
        r.stall_retries,
        breaks.len(),
        breaks.iter().map(|b| b.at_sample).collect::<Vec<_>>(),
        breaks
            .iter()
            .map(|b| b.dropped_before.blocks)
            .collect::<Vec<_>>(),
        breaks
            .iter()
            .map(|b| (b.quiet.as_secs_f64() * 1e3).round() as u64)
            .collect::<Vec<_>>()
    ))
}

fn main() {
    match run() {
        Ok(line) => println!("{line}"),
        Err(e) => {
            eprintln!("readsplit: {e}");
            std::process::exit(1);
        }
    }
}
