//! Can the endpoint FIFO be made larger, so a control transfer during
//! the stream no longer overflows it? USB_EPA_FIFO_CFG ([RTL2832U] Table
//! 34, p. 40): "EPA FIFO Size … in 188-byte blocks. Valid data range is
//! 1~10", reset Ah — yet it reads 0x14 (20) after every power cycle
//! (findings 1A, 16). Findings 16: every control transfer while streaming
//! loses ~950 samples (a USB-block read) to ~2,100 (a demodulator read).
//!
//!   fifosize [--values a,b,…] [--polls N] [--rate SPS]
//!
//! Built with `--features bench`. The candidate space, declared here and
//! covered in this order: the register's low byte ∈ {0x05, 0x0A, 0x14,
//! 0x18, 0x1C, 0x1F, 0x20, 0x28, 0x30, 0x40, 0x60, 0x80, 0xFF}; smaller
//! first, to see the field act as a size (more loss), then the power-on
//! value, then larger. For each, from a power cycle:
//! 1. the register read; the stream stopped (never started), the four
//!    bytes written with the candidate low byte and the counter byte 0
//!    (bytes 1-2 as read), read back;
//! 2. a tone at +300 kHz, the stream started (`RtlStream::start` clears
//!    the counter), 1 s read without control traffic: samples against
//!    rate × time, phase jumps, the tone over the median bin (the content
//!    intact);
//! 3. `--polls` (default 50) USB-block reads of the counter, one every
//!    100 ms, the stream read without pause: phase jumps, how many within
//!    20 ms after a read, samples short of rate × time;
//! 4. the stream stopped and the counter taken (free when stopped).
//!
//! Every write is inside USB_EPA_FIFO_CFG (0x160-0x163); USB block 0x1C is
//! refused by the request layer. One JSON line per candidate.

mod bench;

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, RxSource};
use sdr_rtl_derived::protocol::{ACCESS_GET, ACCESS_SET, BLK_DROP_COUNTER, BLOCK_USB, USB_EPA_FIFO_CFG, windex, wvalue_usb};
use sdr_rtl_derived::{OPEN_RF_HZ, Rtl, RtlStream};

const CANDIDATES: [u8; 13] = [0x05, 0x0A, 0x14, 0x18, 0x1C, 0x1F, 0x20, 0x28, 0x30, 0x40, 0x60, 0x80, 0xFF];
/// The output rate unless `--rate` says otherwise.
const DEFAULT_RATE_SPS: f64 = 2.4e6;
const OFFSET_HZ: f64 = 300e3;
const JUMP_RAD: f64 = 0.5;
const POLL: Duration = Duration::from_millis(100);
/// A jump within this long after a read is counted as near it.
const NEAR_POLL_S: f64 = 0.02;
const CLEAN: Duration = Duration::from_secs(1);
const TONE_TOL_BINS: usize = 16;

fn read_cfg(rtl: &Rtl) -> Result<[u8; 4], String> {
    let v = rtl.link().get(wvalue_usb(USB_EPA_FIFO_CFG), windex(BLOCK_USB, ACCESS_GET, 0), 4, "USB_EPA_FIFO_CFG read").map_err(|e| e.to_string())?;
    Ok([v[0], v[1], v[2], v[3]])
}

fn write_cfg(rtl: &Rtl, b: [u8; 4]) -> Result<(), String> {
    rtl.link().set(wvalue_usb(USB_EPA_FIFO_CFG), windex(BLOCK_USB, ACCESS_SET, 0), &b, "USB_EPA_FIFO_CFG write").map_err(|e| e.to_string())
}

fn wrap(a: f64) -> f64 {
    (a + PI).rem_euclid(2.0 * PI) - PI
}

/// The tone's phase steps, tracked across calls.
struct Phase {
    prev: Option<Iq>,
    step: f64,
    n: u64,
    jumps: u64,
    near: u64,
    last_poll: Option<u64>,
    near_window: u64,
}

impl Phase {
    fn feed(&mut self, x: &[Iq]) {
        for v in x {
            if let Some(p) = self.prev {
                let (a, b) = ((v.i as f64, v.q as f64), (p.i as f64, p.q as f64));
                let s = (a.1 * b.0 - a.0 * b.1).atan2(a.0 * b.0 + a.1 * b.1);
                let e = wrap(s - self.step);
                if e.abs() > JUMP_RAD {
                    self.jumps += 1;
                    if self.last_poll.is_some_and(|lp| self.n >= lp && self.n - lp <= self.near_window) {
                        self.near += 1;
                    }
                } else {
                    self.step += 1e-4 * e;
                }
            }
            self.prev = Some(*v);
            self.n += 1;
        }
    }
}

fn one(src: &mut Source, value: u8, polls: u32, rate: f64) -> Result<String, String> {
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let before = read_cfg(&rtl)?;
    let wrote = [value, before[1], before[2], 0];
    write_cfg(&rtl, wrote)?;
    let back = read_cfg(&rtl)?;
    src.tone(OPEN_RF_HZ + OFFSET_HZ)?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut ph = Phase { prev: None, step: 2.0 * PI * OFFSET_HZ / rate, n: 0, jumps: 0, near: 0, last_poll: None, near_window: (rate * NEAR_POLL_S) as u64 };
    if let Err(e) = s.start() {
        return Ok(format!("\"cycle\":\"{cycle}\",\"before\":{before:?},\"wrote\":{wrote:?},\"read_back\":{back:?},\"start_error\":\"{e}\""));
    }
    // 2. Clean: no control traffic.
    let t = Instant::now();
    let mut clean: Vec<Iq> = Vec::new();
    let mut timeouts = 0usize;
    while t.elapsed() < CLEAN {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        timeouts += info.timed_out as usize;
        ph.feed(&buf[..info.n_samples]);
        if clean.len() < FFT_N * SEGMENTS {
            clean.extend_from_slice(&buf[..info.n_samples]);
        }
    }
    let clean_s = t.elapsed().as_secs_f64();
    let (clean_n, clean_jumps) = (ph.n, ph.jumps);
    let tone_db = spectrum(&clean)
        .map(|p| {
            let k = near(&p, bin_of(OFFSET_HZ, rate), TONE_TOL_BINS);
            db(p[k] / median(&p))
        })
        .unwrap_or(f64::NAN);
    // 3. Polled: one USB-block read every 100 ms.
    let t = Instant::now();
    let n0 = ph.n;
    let j0 = ph.jumps;
    let mut next = POLL;
    let mut done = 0u32;
    while done < polls {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        timeouts += info.timed_out as usize;
        ph.feed(&buf[..info.n_samples]);
        if t.elapsed() >= next {
            rtl.link().usb_read8(BLK_DROP_COUNTER).map_err(|e| e.to_string())?;
            ph.last_poll = Some(ph.n);
            done += 1;
            next += POLL;
        }
    }
    // Read on for one more poll interval so the last read's loss is in.
    let t_end = t.elapsed() + POLL;
    while t.elapsed() < t_end {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        ph.feed(&buf[..info.n_samples]);
    }
    let polled_s = t.elapsed().as_secs_f64();
    let polled_n = ph.n - n0;
    s.stop().map_err(|e| e.to_string())?;
    let d = take(&mut s)?;
    let after = read_cfg(&rtl)?;
    Ok(format!(
        "\"cycle\":\"{cycle}\",\"before\":{before:?},\"wrote\":{wrote:?},\"read_back\":{back:?},\"clean_s\":{clean_s:.3},\"clean_samples\":{clean_n},\"clean_rate\":{:.0},\"clean_jumps\":{clean_jumps},\"tone_db\":{tone_db:.1},\"timeouts\":{timeouts},\"polls\":{polls},\"polled_s\":{polled_s:.3},\"polled_samples\":{polled_n},\"polled_shortfall\":{:.0},\"polled_jumps\":{},\"jumps_near_a_poll\":{},\"counter_blocks\":{},\"counter_saturated\":{},\"after\":{after:?}",
        clean_n as f64 / clean_s,
        rate * polled_s - polled_n as f64,
        ph.jumps - j0,
        ph.near,
        d.0,
        d.1
    ))
}

fn take(s: &mut RtlStream) -> Result<(u8, bool), String> {
    let d = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    Ok((d.blocks, d.saturated))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let values: Vec<u8> = match args.iter().position(|a| a == "--values").and_then(|i| args.get(i + 1)) {
        Some(list) => list.split(',').filter_map(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok()).collect(),
        None => CANDIDATES.to_vec(),
    };
    let polls: u32 = args.iter().position(|a| a == "--polls").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(50);
    let rate: f64 = args.iter().position(|a| a == "--rate").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_RATE_SPS);
    let mut src = Source::spawn(SOURCE_CMD, 0, -15)?;
    for v in values {
        eprintln!("candidate 0x{v:02X}");
        match one(&mut src, v, polls, rate) {
            Ok(body) => println!("{{\"value\":{v},\"rate\":{rate},{body}}}"),
            Err(e) => println!("{{\"value\":{v},\"error\":\"{e}\"}}"),
        }
    }
    src.off()?;
    // Leave the dongle at its power-on state.
    power_cycle()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fifosize: {e}");
        std::process::exit(1);
    }
}
