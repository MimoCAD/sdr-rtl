//! The tuner's band-dependent front end against alias rejection at a
//! band's centre (findings 21): in the 800 MHz band every FIR tap set is
//! capped at 33–39 dB for a tone 1.6 MHz out (at 300 and 420 MHz the same
//! sets reach 51–58), so something ahead of the FIR limits it there. The
//! band table (findings 4f2) chose each band's RF path for the strongest
//! signal, never for rejection; this measures the rejection of each path.
//!
//!   frontsweep [--center HZ] [--codes L M V] [--source-cmd "<command line>"]
//!
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. The
//! driver tunes --center (default 852.30625 MHz) at 2.4 Msps; then, from the
//! driver's own band path there, one field at a time takes each of its
//! values — RFFILT (R26[1:0]), RFMUX (R26[7:6]), TF_NCH (R27[7:4]), TF_LP
//! (R27[3:0]) — written through the I2C repeater mid-stream, and a tone at
//! each offset in OFFSETS_HZ is read where it lands. One JSON line per
//! (setting, offset); the driver's own path is the line with
//! `"field":"driver"`.

mod bench;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::band::path_for;
use sdr_rtl::blocking::{Link, Rtl};
use sdr_rtl::image::{Front, R26, R27};

/// Tone offsets: the in-band reference first, the window's edges, then its
/// surroundings either side.
const OFFSETS_HZ: [f64; 11] = [
    0.4e6, 1.0e6, -1.0e6, 1.3e6, -1.3e6, 1.6e6, -1.6e6, 2.0e6, -2.0e6, 3.0e6, -3.0e6,
];
const TOL_BINS: usize = 4;
/// RFFILT and RFMUX are 2-bit fields, TF_NCH and TF_LP 4-bit.
const TWO_BIT_VALUES: u8 = 4;
const FOUR_BIT_VALUES: u8 = 16;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn lands_at(off: f64, rate: f64) -> f64 {
    (off + rate / 2.0).rem_euclid(rate) - rate / 2.0
}

fn write_path(link: &Link<'_>, f: &Front) -> Result<(), String> {
    link.with_tuner(|l| {
        l.tuner_write(R26, f.r26())?;
        l.tuner_write(R27, f.r27())
    })
    .map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let center: f64 = arg(&args, "--center")
        .and_then(|v| v.parse().ok())
        .unwrap_or(852.30625e6);
    let cmd = arg(&args, "--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    let codes: Option<[usize; 3]> = args.iter().position(|a| a == "--codes").map(|i| {
        let c = |k: usize| args.get(i + k).and_then(|v| v.parse().ok()).unwrap_or(0);
        [c(1), c(2), c(3)]
    });
    let mut src = Source::spawn(&cmd, 0, -20)?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    rtl.set_rate(2.4e6).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(center))
        .map_err(|e| e.to_string())?;
    if let Some([l, m, v]) = codes {
        for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
            rtl.set_stage_level(stage, code)
                .map_err(|e| e.to_string())?;
        }
    }
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let path = path_for(center).ok_or("no band path")?;
    let base = Front {
        rffilt: path.rffilt,
        rfmux: path.rfmux,
        tf_nch: path.tf_nch,
        tf_lp: path.tf_lp,
        ..Front::BRING_UP
    };
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, 128)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    println!(
        "{{\"kind\":\"header\",\"center\":{center},\"rate\":{rate},\"codes\":{codes:?},\"driver_path\":[{},{},{},{}],\"line_legend\":\"field varied, its value, tone offset, landing, power there dB\"}}",
        base.rffilt, base.rfmux, base.tf_nch, base.tf_lp
    );
    let mut variants: Vec<(&str, u8, Front)> = vec![("driver", 0, base)];
    for v in 0..TWO_BIT_VALUES {
        variants.push(("rffilt", v, Front { rffilt: v, ..base }));
        variants.push(("rfmux", v, Front { rfmux: v, ..base }));
    }
    for v in 0..FOUR_BIT_VALUES {
        variants.push(("tf_nch", v, Front { tf_nch: v, ..base }));
        variants.push(("tf_lp", v, Front { tf_lp: v, ..base }));
    }
    variants.push(("driver", 0, base));
    for (field, value, front) in variants {
        write_path(&rtl.link(), &front)?;
        for off in OFFSETS_HZ {
            src.tone(center + off)?;
            let c = spectrum_capture(&mut s, &mut buf)?;
            let p = spectrum(&c.samples).ok_or("short capture")?;
            let at = lands_at(off, rate);
            let k = near(&p, bin_of(at, rate), TOL_BINS);
            println!(
                "{{\"field\":\"{field}\",\"value\":{value},\"offset_hz\":{off},\"lands_at_hz\":{at},\"bin_db\":{:.1},\"median_db\":{:.1}}}",
                db(p[k]),
                db(median(&p))
            );
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("frontsweep: {e}");
        std::process::exit(1);
    }
}
