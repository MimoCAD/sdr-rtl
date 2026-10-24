//! How long the chip refuses a control request while it streams (findings
//! 21): on one Linux host every finish's halt write was refused through
//! the whole retry budget, then accepted ~12 ms after the first attempt.
//!
//!   refusalcheck [--trials N] [--rate SPS]
//!                [--before none|demod|demod-p1|usb|sys] [--gap-us US]
//!                [--no-wake]
//!
//! `--no-wake`: the request layer's wake reads off (`DEMOD_WRITE_READ_
//! WITHIN`), so the refusals they prevent are seen. The last line counts
//! the wake reads sent.
//!
//! `--before`: each write attempt is preceded by one read (the written
//! register, page 1 0x01, the USB block's counter, the system block's
//! first register), `--gap-us` before the write.
//!
//! Run from the workspace root (`cargo run --release -p sdr-rtl
//! --features bench --example refusalcheck`), one dongle attached; no
//! tone needed. The raw stream runs 128 transfers deep; between probes it
//! is read for 300 ms. Each probe is one request, repeated 1 ms apart
//! until the chip accepts it: the refusals it met (the request layer's
//! own retries included) and the time from the first attempt to the
//! accepting one. The probes, `--trials` each, interleaved:
//! - `usb_sysctl_rewrite`, `demod_ctl_rewrite`: USB_SYSCTL and DEMOD_CTL
//!   written with the values they held once the stream ran;
//! - `start_rewrite`: page 0 0x06 written with the start bit it holds;
//! - `start_read`: page 0 0x06 read;
//! - `counter_read`: the USB block's dropped-block counter read;
//! - `halt`: page 0 0x06 ← 0 (the stream stops; it is restarted after).
//!
//! Then the reads and the halt with the stream stopped,
//! 20 ms apart. One JSON line a probe
//! kind and state: refusals per probe, and the window's largest.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sdr_rtl::blocking::{Link, Rtl};
use sdr_rtl::protocol::{
    ACCESS_SET, BLK_DROP_COUNTER, BLOCK_SYS, DEMOD_CTL, START_BIT, START_OFF, START_PAGE,
    USB_SYSCTL, windex, wvalue_sys,
};

/// Transfers in flight: the session's own depth.
const DEPTH: usize = 128;
const TRANSFER_BYTES: usize = 16_384;
/// Read between probes.
const READ_BETWEEN: Duration = Duration::from_millis(300);
/// Give up on one probe after this long.
const PROBE_LIMIT: Duration = Duration::from_millis(500);
const NAP: Duration = Duration::from_millis(1);
/// `--before`: what each write attempt is preceded by.
static BEFORE: AtomicU8 = AtomicU8::new(BEFORE_NONE);
const BEFORE_NONE: u8 = 0;
/// The written register (page 0 0x06) read.
const BEFORE_DEMOD: u8 = 1;
/// Another page's demodulator register (page 1 0x01) read.
const BEFORE_DEMOD_P1: u8 = 2;
/// The USB block's dropped-block counter read.
const BEFORE_USB: u8 = 3;
/// The system block's first register read.
const BEFORE_SYS: u8 = 4;
/// `--gap-us`: the wait between that request and the write.
static GAP_US: AtomicU64 = AtomicU64::new(0);
/// USB_SYSCTL and DEMOD_CTL as read once the stream runs: their rewrites
/// change nothing.
static USB_SYSCTL_NOW: AtomicU8 = AtomicU8::new(0);
static DEMOD_CTL_NOW: AtomicU8 = AtomicU8::new(0);

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy, PartialEq)]
enum Probe {
    UsbRewrite,
    SysRewrite,
    StartRewrite,
    StartRead,
    CounterRead,
    Halt,
}

impl Probe {
    fn name(self) -> &'static str {
        match self {
            Probe::UsbRewrite => "usb_sysctl_rewrite",
            Probe::SysRewrite => "demod_ctl_rewrite",
            Probe::StartRewrite => "start_rewrite",
            Probe::StartRead => "start_read",
            Probe::CounterRead => "counter_read",
            Probe::Halt => "halt",
        }
    }

    /// Probed with the stream stopped too: not the rewrites of values
    /// read while it ran.
    fn when_stopped(self) -> bool {
        matches!(self, Probe::StartRead | Probe::CounterRead | Probe::Halt)
    }

    fn once(self, link: &Link) -> sdr_core::Result<()> {
        let write = matches!(
            self,
            Probe::UsbRewrite | Probe::SysRewrite | Probe::StartRewrite | Probe::Halt
        );
        if write {
            match BEFORE.load(Ordering::Relaxed) {
                BEFORE_DEMOD => drop(link.demod_read8(START_PAGE, START_OFF)?),
                BEFORE_DEMOD_P1 => drop(link.demod_read8(1, 0x01)?),
                BEFORE_USB => drop(link.usb_read8(BLK_DROP_COUNTER)?),
                BEFORE_SYS => drop(link.sys_read8(0)?),
                _ => {}
            }
            let gap = GAP_US.load(Ordering::Relaxed);
            if gap > 0 {
                std::thread::sleep(Duration::from_micros(gap));
            }
        }
        match self {
            Probe::UsbRewrite => {
                link.usb_write8(USB_SYSCTL, USB_SYSCTL_NOW.load(Ordering::Relaxed))
            }
            Probe::SysRewrite => link.set(
                wvalue_sys(DEMOD_CTL),
                windex(BLOCK_SYS, ACCESS_SET, 0),
                &[DEMOD_CTL_NOW.load(Ordering::Relaxed)],
                "system register write",
            ),
            Probe::StartRewrite => link.demod_write(START_PAGE, START_OFF, &[START_BIT]),
            Probe::StartRead => link.demod_read8(START_PAGE, START_OFF).map(drop),
            Probe::CounterRead => link.usb_read8(BLK_DROP_COUNTER).map(drop),
            Probe::Halt => link.demod_write(START_PAGE, START_OFF, &[0]),
        }
    }
}

/// One probe until accepted: (refusals met, ms from the first attempt to
/// the accepting one), or None if it never was.
fn probe(link: &Link, p: Probe) -> (usize, Option<f64>) {
    let before = link.stall_retries();
    let t = Instant::now();
    let mut failed_calls = 0usize;
    loop {
        match p.once(link) {
            Ok(()) => {
                let refusals = (link.stall_retries() - before) as usize + failed_calls;
                return (refusals, Some(t.elapsed().as_secs_f64() * 1e3));
            }
            Err(_) if t.elapsed() < PROBE_LIMIT => {
                failed_calls += 1;
                std::thread::sleep(NAP);
            }
            Err(_) => {
                return (
                    (link.stall_retries() - before) as usize + failed_calls,
                    None,
                );
            }
        }
    }
}

fn read_for(s: &mut sdr_rtl::blocking::RtlStream, d: Duration) -> Result<usize, String> {
    let mut buf = vec![0u8; TRANSFER_BYTES];
    let (mut n, t) = (0usize, Instant::now());
    while t.elapsed() < d {
        n += s.recv_bytes(&mut buf, 0.25).map_err(|e| e.to_string())?.0;
    }
    Ok(n)
}

#[derive(Default)]
struct Tally {
    probes: usize,
    refused_probes: usize,
    refusals: usize,
    never: usize,
    window_ms: Vec<f64>,
}

fn report(state: &str, p: Probe, t: &Tally) {
    let max = t.window_ms.iter().cloned().fold(0.0f64, f64::max);
    let mut w = t.window_ms.clone();
    w.sort_by(f64::total_cmp);
    let med = w.get(w.len() / 2).copied().unwrap_or(0.0);
    println!(
        "{{\"state\":\"{state}\",\"probe\":\"{}\",\"probes\":{},\"refused_probes\":{},\"refusals\":{},\"never_accepted\":{},\"window_ms_median\":{med:.2},\"window_ms_max\":{max:.2}}}",
        p.name(),
        t.probes,
        t.refused_probes,
        t.refusals,
        t.never
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trials: usize = arg(&args, "--trials", 20);
    let rate: f64 = arg(&args, "--rate", 2.048e6);
    let before = match arg(&args, "--before", String::from("none")).as_str() {
        "none" => BEFORE_NONE,
        "demod" => BEFORE_DEMOD,
        "demod-p1" => BEFORE_DEMOD_P1,
        "usb" => BEFORE_USB,
        "sys" => BEFORE_SYS,
        other => {
            println!("{{\"error\":\"--before {other}: none | demod | demod-p1 | usb | sys\"}}");
            return;
        }
    };
    BEFORE.store(before, Ordering::Relaxed);
    GAP_US.store(arg(&args, "--gap-us", 0u64), Ordering::Relaxed);
    let no_wake = args.iter().any(|a| a == "--no-wake");
    let probes = [
        Probe::UsbRewrite,
        Probe::SysRewrite,
        Probe::StartRewrite,
        Probe::StartRead,
        Probe::CounterRead,
        Probe::Halt,
    ];
    let r = (|| -> Result<(), String> {
        // `SDR_BENCH_RTL`: which dongle on a host with more than one.
        let mut rtl = Rtl::open(&std::env::var("SDR_BENCH_RTL").unwrap_or_default())
            .map_err(|e| format!("open: {e}"))?;
        rtl.set_rate(rate).map_err(|e| format!("rate: {e}"))?;
        let mut s = rtl
            .rx_stream(TRANSFER_BYTES, DEPTH)
            .map_err(|e| format!("stream: {e}"))?;
        s.start().map_err(|e| format!("start: {e}"))?;
        let link = rtl.link();
        link.set_wake_reads(!no_wake);
        USB_SYSCTL_NOW.store(
            link.usb_read8(USB_SYSCTL).map_err(|e| e.to_string())?,
            Ordering::Relaxed,
        );
        DEMOD_CTL_NOW.store(
            link.sys_read8(DEMOD_CTL).map_err(|e| e.to_string())?,
            Ordering::Relaxed,
        );
        read_for(&mut s, Duration::from_secs(1))?;
        let mut streaming: Vec<Tally> = probes.iter().map(|_| Tally::default()).collect();
        for _ in 0..trials {
            for (k, &p) in probes.iter().enumerate() {
                read_for(&mut s, READ_BETWEEN)?;
                let (refusals, window) = probe(&rtl.link(), p);
                eprintln!(
                    "streaming {}: {refusals} refused, accepted after {window:?} ms",
                    p.name()
                );
                let t = &mut streaming[k];
                t.probes += 1;
                t.refusals += refusals;
                t.refused_probes += (refusals > 0) as usize;
                match window {
                    Some(ms) => t.window_ms.push(ms),
                    None => t.never += 1,
                }
                if p == Probe::Halt {
                    let (refused, back) = probe(&rtl.link(), Probe::StartRewrite);
                    eprintln!("streaming restart: {refused} refused, accepted after {back:?} ms");
                    if back.is_none() {
                        for (k, &p) in probes.iter().enumerate() {
                            report("streaming", p, &streaming[k]);
                        }
                        return Err("the restart after a halt was never accepted".into());
                    }
                }
            }
        }
        for (k, &p) in probes.iter().enumerate() {
            report("streaming", p, &streaming[k]);
        }
        s.stop().map_err(|e| format!("stop: {e}"))?;
        let mut stopped: Vec<Tally> = probes.iter().map(|_| Tally::default()).collect();
        for _ in 0..trials {
            // Not the start rewrite: it would set the stream running with
            // no transfers to take it. The halt is the stop's write again.
            for (k, &p) in probes.iter().enumerate().filter(|(_, p)| p.when_stopped()) {
                std::thread::sleep(Duration::from_millis(20));
                let (refusals, window) = probe(&rtl.link(), p);
                let t = &mut stopped[k];
                t.probes += 1;
                t.refusals += refusals;
                t.refused_probes += (refusals > 0) as usize;
                match window {
                    Some(ms) => t.window_ms.push(ms),
                    None => t.never += 1,
                }
            }
        }
        for (k, &p) in probes.iter().enumerate().filter(|(_, p)| p.when_stopped()) {
            report("stopped", p, &stopped[k]);
        }
        println!(
            "{{\"wake_reads\":{},\"no_wake\":{no_wake}}}",
            rtl.link().wake_reads()
        );
        Ok(())
    })();
    if let Err(e) = r {
        println!("{{\"error\":{e:?}}}");
    }
}
