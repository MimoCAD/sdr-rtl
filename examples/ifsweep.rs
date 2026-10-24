//! The tuner's IF filter, every FILT_CODE × FILT_BW combination, judged by
//! what a receiver needs (findings 21): how far a tone outside the
//! stream's window is held down where it aliases in, and how flat the
//! window stays. Step 4g measured only the 19 settings with one of the
//! two at its widest (corners 7.3-10.8 MHz); the old driver's lopsided
//! alias rejection (65 dB at +1.6 MHz, 40 at −1.6) points at an analog
//! filter narrower than any of those.
//!
//!   ifsweep [--center HZ] [--rate SPS] [--codes L M V] [--pw-filt N]
//!           [--source-cmd "<command line>"] [--fields c0,…,c15] [--if-default]
//!
//! `--fields`: the FIR taps for the stream (step 22's candidates);
//! `--if-default`: no IF filter sweep, one pass at the driver's setting
//! (filt_code and filt_bw then print as 0 and mean "the driver's").
//! `--regs FILE --group NAME …` (with `--except`, `--base`,
//! `--freq-dependent`, `--after-start`): a register group file's groups
//! applied on top of the driver's setup (`groups/mod.rs`), written back
//! after the sweep; use with `--if-default`.
//!
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. The
//! stream runs at the rate (default 2.4 Msps) centred at --center
//! (default 852.30625 MHz), gains as `--codes` (default the driver's).
//! For each FILT_CODE 0-15 × FILT_BW 0-3 (R10[3:0], R11[6:5]; PW_FILT
//! R10[6:5] as given, default 0; HPF 1111 as the driver keeps it), written
//! through the I2C repeater mid-stream, a tone at each offset in OFFSETS_HZ:
//! its power at the bin it lands on inside the window (the offset itself,
//! or folded by the rate), dB on the spectrum's scale. One JSON line per
//! (setting, offset). The driver's own setting is restored at the end.

mod bench;
mod groups;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::image::{FILT_BW_SHIFT, HPF_LOWEST, PWD_FILT, R10, R10_FIXED, R11};
use sdr_rtl::taps;

/// Tone offsets from the centre: inside the window (its flatness) and
/// outside it (what aliases in).
const OFFSETS_HZ: [f64; 13] = [
    -1.0e6, -0.6e6, 0.4e6, 0.8e6, 1.0e6, 1.6e6, 2.0e6, 3.0e6, -1.6e6, -2.0e6, -3.0e6, 1.3e6, -1.3e6,
];
/// Bins the measured tone may sit from where it should.
const TOL_BINS: usize = 4;
/// R10's PW_FILT field (bits 6:5).
const PW_FILT_SHIFT: u8 = 5;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Where a tone at `off` lands in a window of `rate`: folded into ±rate/2.
fn lands_at(off: f64, rate: f64) -> f64 {
    (off + rate / 2.0).rem_euclid(rate) - rate / 2.0
}

fn run() -> Result<(), String> {
    let (group_args, args) = groups::Args::take(&std::env::args().skip(1).collect::<Vec<String>>());
    let center: f64 = arg(&args, "--center")
        .and_then(|v| v.parse().ok())
        .unwrap_or(852.30625e6);
    let mut plan = groups::Plan::new(&group_args, center)?;
    plan.before_open();
    let rate: f64 = arg(&args, "--rate")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.4e6);
    let pw_filt: u8 = arg(&args, "--pw-filt")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cmd = arg(&args, "--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    let codes: Option<[usize; 3]> = args.iter().position(|a| a == "--codes").map(|i| {
        let c = |k: usize| args.get(i + k).and_then(|v| v.parse().ok()).unwrap_or(0);
        [c(1), c(2), c(3)]
    });
    let mut src = Source::spawn(&cmd, 0, -20)?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(center))
        .map_err(|e| e.to_string())?;
    if let Some([l, m, v]) = codes {
        for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
            rtl.set_stage_level(stage, code)
                .map_err(|e| e.to_string())?;
        }
    }
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    // `--fields c0,…,c15`: the FIR's taps for this stream instead of the
    // driver's (step 22's candidates); `--if-default`: the IF filter left
    // as the driver set it, one pass (a tap set's tone test).
    let fields: Option<[i32; taps::TAPS]> = arg(&args, "--fields").map(|l| {
        let v: Vec<i32> = l.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        v.try_into()
            .unwrap_or_else(|_| panic!("--fields needs {} numbers", taps::TAPS))
    });
    let if_default = args.iter().any(|a| a == "--if-default");
    // `--demod page:reg:value` (repeatable, decimal or 0x hex): demodulator
    // writes made inside the stream's start sequence, just before the start
    // bit (`stream::set_bench_start_writes`) — documented bits that take no
    // effect in a running stream, tried for their effect (findings 21).
    let mut demod_writes: Vec<(u8, u8, u8)> = Vec::new();
    for (k, a) in args.iter().enumerate() {
        if a != "--demod" {
            continue;
        }
        let spec = args.get(k + 1).ok_or("--demod page:reg:value")?;
        let n = |s: &str| {
            if let Some(h) = s.strip_prefix("0x") {
                u8::from_str_radix(h, 16).ok()
            } else {
                s.parse().ok()
            }
        };
        let v: Vec<u8> = spec.split(':').filter_map(n).collect();
        let [page, reg, value] = v[..] else {
            return Err(format!("--demod {spec}: page:reg:value"));
        };
        demod_writes.push((page, reg, value));
    }
    sdr_rtl::stream::set_bench_start_writes(&demod_writes);
    plan.apply(&rtl)?;
    let tap_regs = match (fields, plan.taps) {
        (Some(c), _) => Some(taps::pack(&c).map_err(|e| e.to_string())?),
        (None, t) => t,
    };
    let mut s = match tap_regs {
        Some(r) => rtl.rx_stream_with_taps(TRANSFER_BYTES, 128, r),
        None => rtl.rx_stream(TRANSFER_BYTES, 128),
    }
    .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    for &(page, reg, _) in &demod_writes {
        eprintln!(
            "demod page {page} 0x{reg:02X} reads back 0x{:02X}",
            rtl.link()
                .demod_read8(page, reg)
                .map_err(|e| e.to_string())?
        );
    }
    if group_args.file.is_some() {
        let again = if group_args.after_start {
            plan.after_start(&rtl)?
        } else {
            0
        };
        println!("{}", plan.summary());
        println!(
            "{{\"kind\":\"regs_check\",\"start_owned\":{:?},\"rewritten_after_start\":{again},\"differ_now\":{:?}}}",
            plan.start_owned_writes(),
            plan.check(&rtl)?
        );
    }
    println!(
        "{{\"kind\":\"header\",\"center\":{center},\"rate\":{rate},\"pw_filt\":{pw_filt},\"codes\":{codes:?},\"source\":{cmd:?},\"line_legend\":\"filt_code, filt_bw, tone offset Hz, where it lands Hz, bin power dB, over the median bin dB\"}}"
    );
    let (codes_range, bw_range) = if if_default {
        (0u8..1, 0u8..1)
    } else {
        (0u8..16, 0u8..4)
    };
    for filt_code in codes_range {
        for filt_bw in bw_range.clone() {
            if !if_default {
                let r10 = PWD_FILT | (pw_filt & 0b11) << PW_FILT_SHIFT | R10_FIXED | filt_code;
                let r11 = filt_bw << FILT_BW_SHIFT | HPF_LOWEST;
                rtl.link()
                    .with_tuner(|l| {
                        l.tuner_write(R10, r10)?;
                        l.tuner_write(R11, r11)
                    })
                    .map_err(|e| e.to_string())?;
            }
            for off in OFFSETS_HZ {
                src.tone(center + off)?;
                let c = spectrum_capture(&mut s, &mut buf)?;
                let p = spectrum(&c.samples).ok_or("short capture")?;
                let at = lands_at(off, rate);
                let k = near(&p, bin_of(at, rate), TOL_BINS);
                println!(
                    "{{\"filt_code\":{filt_code},\"filt_bw\":{filt_bw},\"offset_hz\":{off},\"lands_at_hz\":{at},\"bin_db\":{:.1},\"over_median_db\":{:.1},\"clipped\":{:.5}}}",
                    db(p[k]),
                    db(p[k] / median(&p)),
                    clipped(&c.samples)
                );
            }
        }
    }
    src.off()?;
    drop(s);
    plan.restore(&rtl)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("ifsweep: {e}");
        std::process::exit(1);
    }
}
