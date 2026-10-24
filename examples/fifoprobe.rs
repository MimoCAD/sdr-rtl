//! Does any register tell how many blocks the endpoint FIFO holds? Findings
//! 16.5: with the output paused the FIFO keeps its fill — ~9 blocks at
//! 1.152 Msps (size 20 or 10), 5 with size 5, ~6 at 1.024 Msps — and hands
//! it out after the resume, so a break is exact only if that fill is known.
//!
//!   fifoprobe [--rate SPS] [--fifo N]
//!
//! Built with `--features bench`. Read only: from a power cycle, the FIFO
//! size optionally written (USB_EPA_FIFO_CFG low byte) before the stream;
//! the stream run 0.5 s; paused with one write, 5 ms; every byte of the
//! USB block 0x000-0x1FF read except 0x01C (the test-mode register, never
//! touched, derivation/HAZARD-usb-0x1C.md); resumed, 0.5 s, read again
//! (running). One JSON line: the two images.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::Iq;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{
    ACCESS_GET, ACCESS_SET, BLOCK_USB, USB_EPA_FIFO_CFG, USB_HAZARD_OFF, windex, wvalue_usb,
};

const SPAN: u16 = 0x200;

fn image(rtl: &Rtl) -> Result<Vec<i32>, String> {
    (0..SPAN)
        .map(|off| {
            if off == USB_HAZARD_OFF {
                Ok(-1)
            } else {
                rtl.link()
                    .usb_read8(off)
                    .map(i32::from)
                    .map_err(|e| e.to_string())
            }
        })
        .collect()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rate: f64 = get("--rate")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.152e6);
    let fifo: Option<u8> = get("--fifo").and_then(|v| v.parse().ok());
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    if let Some(n) = fifo {
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
    }
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let run_for =
        |s: &mut sdr_rtl::blocking::RtlStream, buf: &mut [Iq], d: Duration| -> Result<(), String> {
            let t = Instant::now();
            while t.elapsed() < d {
                s.recv_into(buf, RECV_TIMEOUT_S)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        };
    run_for(&mut s, &mut buf, Duration::from_millis(500))?;
    s.pause_blind().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(5));
    let paused = image(&rtl)?;
    s.resume_blind().map_err(|e| e.to_string())?;
    run_for(&mut s, &mut buf, Duration::from_millis(500))?;
    let running = image(&rtl)?;
    s.stop().map_err(|e| e.to_string())?;
    println!(
        "{{\"cycle\":\"{cycle}\",\"rate\":{rate},\"fifo\":{fifo:?},\"paused\":{paused:?},\"running\":{running:?}}}"
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fifoprobe: {e}");
        std::process::exit(1);
    }
}
