//! Review A3 on Linux: the kernel's DVB driver loaded, a receiver opened
//! again and again, two streams each with a rate change between them —
//! the window in which the kernel driver used to bind again.
//!
//!   a3loop [--runs N] [--seconds S] [--source-cmd "<command line>"]
//!
//! Run from the workspace root (`cargo run --release -p sdr-rtl
//! --features bench --example a3loop`), one dongle attached, the tone
//! source cabled in through the pad. Each run: open, 300 MHz, 2.048 Msps,
//! a stream of S seconds (the tone 300 kHz above), finish, the rate set to
//! 2.4 Msps, a second stream of S seconds, finish, close. The tone's level
//! over the median bin is read in each stream; whether a kernel driver is
//! bound to the dongle's interface 0 is read from sysfs mid-stream and
//! between the two streams (Linux only; null elsewhere). One JSON line per
//! run; the last line compares every run's levels with the first run's.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::blocking::Rtl;

const RF_HZ: f64 = 300e6;
const TONE_OFFSET_HZ: f64 = 300e3;
const RATE_A: f64 = 2.048e6;
const RATE_B: f64 = 2.4e6;
/// Bins the peak may sit from the predicted bin (`hwcheck`'s tolerance).
const PEAK_TOL_BINS: usize = 4;
/// The review's pass line: every run within this of the first.
const LEVEL_TOL_DB: f64 = 1.0;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Whether a kernel driver is bound to interface 0 of the RTL2832U
/// (0bda:2838) under test: Some(name) bound, Some("") unbound, None where
/// there is no sysfs or no such device. With `SDR_BENCH_RTL=serial=…` the
/// dongle of that serial, else the first found (a host with two read the
/// wrong one's state that way).
fn kernel_driver() -> Option<String> {
    let want = rtl_spec().strip_prefix("serial=").map(str::to_string);
    let dir = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for e in dir.flatten() {
        let p = e.path();
        let read = |f: &str| {
            std::fs::read_to_string(p.join(f))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        if read("idVendor") == "0bda"
            && read("idProduct") == "2838"
            && want.as_ref().is_none_or(|w| read("serial") == *w)
        {
            let name = e.file_name().to_string_lossy().to_string();
            let iface = p.with_file_name(format!("{name}:1.0"));
            return Some(
                std::fs::read_link(iface.join("driver"))
                    .ok()
                    .and_then(|l| l.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_default(),
            );
        }
    }
    None
}

fn json_opt(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("{s:?}"),
        None => "null".into(),
    }
}

/// The tone's level over the median bin and whether its peak sat at the
/// predicted bin.
fn tone(x: &[Iq], rate: f64) -> (f64, bool) {
    let Some(p) = spectrum(x) else {
        return (f64::NAN, false);
    };
    let med = median(&p);
    let predicted = bin_of(TONE_OFFSET_HZ, rate);
    let k = near(&p, predicted, PEAK_TOL_BINS);
    let peak = (0..FFT_N)
        .filter(|k| !is_dc(*k))
        .max_by(|a, b| p[*a].total_cmp(&p[*b]))
        .unwrap_or(0);
    (
        10.0 * (p[k] / med).log10(),
        peak.abs_diff(predicted) <= PEAK_TOL_BINS,
    )
}

struct Leg {
    level_db: f64,
    at_bin: bool,
    driver_mid: Option<String>,
    dropped_blocks: u64,
}

/// One stream of `secs` from `rtl`: the tone read after the discard, the
/// kernel driver read mid-stream; the receiver handed back.
fn leg(rtl: Rtl, secs: f64) -> Result<(Rtl, Leg), String> {
    let rate = rtl.rate_set();
    let mut s = rtl.into_stream(0, 0).map_err(|e| e.error.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let t = Instant::now();
    let c = spectrum_capture(&mut s, &mut buf)?;
    let (level_db, at_bin) = tone(&c.samples, rate);
    let driver_mid = kernel_driver();
    let rest = Duration::from_secs_f64(secs).saturating_sub(t.elapsed());
    discard(&mut s, &mut buf, rest)?;
    let f = s.finish().map_err(|e| e.to_string())?;
    Ok((
        f.rtl,
        Leg {
            level_db,
            at_bin,
            driver_mid,
            dropped_blocks: f.report.dropped_blocks,
        },
    ))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runs: usize = arg(&args, "--runs", 100);
    let secs: f64 = arg(&args, "--seconds", 3.0);
    let source_cmd = arg(&args, "--source-cmd", SOURCE_CMD.to_string());
    let mut src = Source::spawn(&source_cmd, 0, -20)?;
    src.tone(RF_HZ + TONE_OFFSET_HZ)?;
    std::thread::sleep(SOURCE_SETTLE);
    let mut first: Option<(f64, f64)> = None;
    let (mut worst, mut failed) = (0.0f64, 0usize);
    for run in 0..runs {
        let before_open = kernel_driver();
        let t = Instant::now();
        let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| format!("run {run}: open: {e}"))?;
        let open_ms = t.elapsed().as_secs_f64() * 1e3;
        rtl.set_freq(&TuneRequest::auto(RF_HZ))
            .map_err(|e| e.to_string())?;
        rtl.set_rate(RATE_A).map_err(|e| e.to_string())?;
        let (mut rtl, a) = leg(rtl, secs)?;
        let between = kernel_driver();
        rtl.set_rate(RATE_B).map_err(|e| e.to_string())?;
        let (rtl, b) = leg(rtl, secs)?;
        let refusals = rtl.link().stall_retries();
        let wake_reads = rtl.link().wake_reads();
        drop(rtl);
        let (ra, rb) = *first.get_or_insert((a.level_db, b.level_db));
        let dev = (a.level_db - ra).abs().max((b.level_db - rb).abs());
        worst = worst.max(dev);
        let ours = |d: &Option<String>| d.as_deref().is_none_or(|n| n.is_empty() || n == "usbfs");
        let pass = dev <= LEVEL_TOL_DB
            && a.at_bin
            && b.at_bin
            && ours(&a.driver_mid)
            && ours(&between)
            && ours(&b.driver_mid);
        failed += !pass as usize;
        println!(
            "{{\"run\":{run},\"pass\":{pass},\"driver_before_open\":{},\"open_ms\":{open_ms:.1},\"level_a_db\":{:.1},\"level_b_db\":{:.1},\"at_bin\":[{},{}],\"dev_from_first_db\":{dev:.2},\"driver_mid_a\":{},\"driver_between\":{},\"driver_mid_b\":{},\"dropped_blocks\":[{},{}],\"refusals\":{refusals},\"wake_reads\":{wake_reads}}}",
            json_opt(&before_open),
            a.level_db,
            b.level_db,
            a.at_bin,
            b.at_bin,
            json_opt(&a.driver_mid),
            json_opt(&between),
            json_opt(&b.driver_mid),
            a.dropped_blocks,
            b.dropped_blocks
        );
    }
    src.off()?;
    println!("{{\"runs\":{runs},\"failed\":{failed},\"worst_dev_from_first_db\":{worst:.2}}}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        println!("{{\"error\":{e:?}}}");
    }
}
