//! USB_SYSCTL bit 3, full-packet mode, on the raw stream: how large each
//! completion is, and what the chip drops, with the bit clear (the
//! minimised sequence, findings 10.4) and set (the vendor's 0x09,
//! findings 9b).
//!
//!   fullpacket [--rate SPS] [--seconds S] [--depth N] [--runs N]
//!
//! Per run, from a power cycle: open (300 MHz, gains 8/8/2), the rate,
//! USB_SYSCTL read, then written 0x01 (clear) or 0x09 (set) and read back,
//! the raw stream (16 KiB transfers) for S seconds (default 3), every
//! receive's size counted; then the stop and the dropped-block counter.
//! The modes alternate. One JSON line per run.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::USB_SYSCTL;

fn one(rate: f64, secs: f64, depth: usize, full: bool) -> Result<String, String> {
    power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    let was = rtl
        .link()
        .usb_read8(USB_SYSCTL)
        .map_err(|e| e.to_string())?;
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, depth)
        .map_err(|e| e.to_string())?;
    // The stream's start writes USB_SYSCTL (`RtlStream::set_packing`).
    s.set_full_packet(full);
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let back = rtl
        .link()
        .usb_read8(USB_SYSCTL)
        .map_err(|e| e.to_string())?;
    // Receives by samples: ≤ 94 (a block), ≤ 256 (a packet), ≤ 2048, < a transfer, a transfer.
    let mut sizes = [0u64; 5];
    let (mut n, mut receives) = (0u64, 0u64);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs_f64(secs) {
        let info = s
            .recv_into(&mut buf, RECV_TIMEOUT_S)
            .map_err(|e| e.to_string())?;
        if info.n_samples == 0 {
            continue;
        }
        receives += 1;
        n += info.n_samples as u64;
        sizes[match info.n_samples {
            0..=94 => 0,
            95..=256 => 1,
            257..=2048 => 2,
            x if x < buf.len() => 3,
            _ => 4,
        }] += 1;
    }
    let elapsed = t.elapsed().as_secs_f64();
    s.stop().map_err(|e| e.to_string())?;
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    drop(s);
    Ok(format!(
        "\"full_packet\":{full},\"rate\":{rate},\"depth\":{depth},\"sysctl_was\":{was},\"sysctl_read_back\":{back},\"seconds\":{elapsed:.3},\"samples\":{n},\"receives\":{receives},\"samples_per_receive\":{:.1},\"by_size\":{sizes:?},\"dropped_blocks\":{},\"saturated\":{}",
        n as f64 / receives.max(1) as f64,
        d.blocks,
        d.saturated
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(2.4e6);
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let depth: usize = get("--depth")
        .and_then(|v| v.parse().ok())
        .unwrap_or(QUEUE_DEPTH);
    let runs: usize = get("--runs").and_then(|v| v.parse().ok()).unwrap_or(4);
    for k in 0..runs {
        match one(rate, secs, depth, k % 2 == 1) {
            Ok(body) => println!("{{{body}}}"),
            Err(e) => println!("{{\"error\":\"{e}\"}}"),
        }
    }
    let _ = power_cycle();
}
