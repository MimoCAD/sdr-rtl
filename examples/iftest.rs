//! The intermediate frequency (IF) against alias rejection (findings 21):
//! the tuner's IF passband falls below about 0.5 MHz for every filter
//! setting (findings 4g), and with the LO above the RF a component δ
//! from the centre sits at IF − δ, so a lower IF puts one side of the
//! window's surroundings onto that roll-off. The old driver rejects a tone
//! 1.6 MHz above the centre by ~65 dB and one 1.6 MHz below by ~40, ours
//! both by ~40; no tap set and no IF filter setting changed ours (findings
//! 21, step 22). This measures what the IF does.
//!
//!   iftest [--center HZ] [--rate SPS] [--codes L M V] [--ifs a,b,…]
//!          [--hpf a,b,…] [--source-cmd "<command line>"]
//!
//! `--hpf`: at each IF, each high-pass code (R11[3:0]) in turn; findings
//! 4g found none had an effect at the driver's IF, but at a low IF the
//! side above the centre sits on the high-pass edge.
//!
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. The
//! stream runs at the rate (default 2.4 Msps), centred at --center (default
//! 852.30625 MHz). For each IF (default 3.57, 3.2, 2.9, 2.6, 2.3, 2.0, 1.8
//! MHz): a receiver opened under it (`demod::set_bench_if_hz`: the open's
//! DDC, the stream's sequence and the LO follow it — the DDC's IF register
//! written into a running or stopped stream takes no effect, STATUS
//! "Writes while streaming", and a first try that way lost the tone at
//! every IF but 3.57); then a tone at each offset in OFFSETS_HZ, its power
//! where it lands in the window and, for the in-band reference, where it
//! actually landed (a wrong IF shows as a moved tone). One JSON line per
//! (IF, offset).

mod bench;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::demod::set_bench_if_hz;
use sdr_rtl::image::R11;

/// Tone offsets from the centre: the in-band reference first, then the
/// window's edges and its surroundings either side.
const OFFSETS_HZ: [f64; 11] = [
    0.4e6, -0.4e6, 1.0e6, -1.0e6, 1.3e6, -1.3e6, 1.6e6, -1.6e6, 2.0e6, -2.0e6, 3.0e6,
];
const DEFAULT_IFS_HZ: [f64; 7] = [3.57e6, 3.2e6, 2.9e6, 2.6e6, 2.3e6, 2.0e6, 1.8e6];
const TOL_BINS: usize = 6;
/// R11's high-pass corner field, bits 3:0 (0000 highest corner … 1111
/// lowest, findings 4g).
const HPF_MASK: u8 = 0x0F;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn lands_at(off: f64, rate: f64) -> f64 {
    (off + rate / 2.0).rem_euclid(rate) - rate / 2.0
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let center: f64 = arg(&args, "--center")
        .and_then(|v| v.parse().ok())
        .unwrap_or(852.30625e6);
    let rate: f64 = arg(&args, "--rate")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.4e6);
    let cmd = arg(&args, "--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    let ifs: Vec<f64> = arg(&args, "--ifs")
        .map(|l| l.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| DEFAULT_IFS_HZ.to_vec());
    let hpfs: Vec<Option<u8>> = arg(&args, "--hpf")
        .map(|l| {
            l.split(',')
                .filter_map(|x| x.parse().ok())
                .map(Some)
                .collect()
        })
        .unwrap_or_else(|| vec![None]);
    let codes: Option<[usize; 3]> = args.iter().position(|a| a == "--codes").map(|i| {
        let c = |k: usize| args.get(i + k).and_then(|v| v.parse().ok()).unwrap_or(0);
        [c(1), c(2), c(3)]
    });
    let mut src = Source::spawn(&cmd, 0, -20)?;
    println!(
        "{{\"kind\":\"header\",\"center\":{center},\"rate\":{rate},\"codes\":{codes:?},\"source\":{cmd:?},\"line_legend\":\"IF Hz, divider, tone offset, where it should land, power there dB, over the median dB, measured frequency of the in-band reference\"}}"
    );
    for if_hz in ifs {
        // A receiver opened under the IF: the open's DDC, the stream's
        // sequence and the LO all follow it (`demod::set_bench_if_hz`).
        set_bench_if_hz(Some(if_hz));
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
        let pll = rtl.tuner_status().map_err(|e| e.to_string())?.pll;
        let rate = rtl.rate().map_err(|e| e.to_string())?;
        let mut s = rtl
            .rx_stream(TRANSFER_BYTES, 128)
            .map_err(|e| e.to_string())?;
        let mut buf = vec![Iq::default(); s.max_num_samps()];
        s.start().map_err(|e| e.to_string())?;
        // `--hpf a,b,…`: each high-pass code (R11[3:0]) in turn at this
        // IF, the rest of R11 as the driver left it; without it, the
        // driver's own (1111) alone.
        let r11 = rtl
            .link()
            .with_tuner(|l| l.tuner_read(R11 as u16 + 1))
            .map_err(|e| e.to_string())?[R11 as usize];
        for &hpf in &hpfs {
            if let Some(code) = hpf {
                rtl.link()
                    .with_tuner(|l| l.tuner_write(R11, (r11 & !HPF_MASK) | (code & HPF_MASK)))
                    .map_err(|e| e.to_string())?;
            }
            for off in OFFSETS_HZ {
                src.tone(center + off)?;
                let c = spectrum_capture(&mut s, &mut buf)?;
                let p = spectrum(&c.samples).ok_or("short capture")?;
                let at = lands_at(off, rate);
                let k = near(&p, bin_of(at, rate), TOL_BINS);
                println!(
                    "{{\"if_hz\":{if_hz},\"hpf\":{},\"divider\":{},\"offset_hz\":{off},\"lands_at_hz\":{at},\"bin_db\":{:.1},\"over_median_db\":{:.1},\"measured_hz\":{:.0}}}",
                    hpf.map_or("\"driver\"".to_string(), |c| c.to_string()),
                    pll.divider,
                    db(p[k]),
                    db(p[k] / median(&p)),
                    peak_hz(&p, k, rate)
                );
            }
        }
        s.stop().map_err(|e| e.to_string())?;
    }
    set_bench_if_hz(None);
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("iftest: {e}");
        std::process::exit(1);
    }
}
