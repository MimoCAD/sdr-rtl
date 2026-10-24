//! R2 (VCO_INDICATOR) against the tone across the bottom of the VCO's
//! range, where `Pll::program` hands over from /16 to /8 (LO ≈ 213.75 MHz,
//! findings 11: RF 220 MHz at /8 read R2 = 69, one under the 70-127 span).
//!
//!   vcoedge [--from MHz --to MHz --step MHz]
//!
//! Built with `--features bench`. At each RF the driver tunes (the band
//! path), then the synthesizer is rewritten under SEL_DIV 010 (/8) and 011
//! (/16) in turn (`Pll::with_divider`, R16 and R20-R22 through the I²C
//! repeater), R0-R4 read after the settle, and a tone 300 kHz above the
//! RF measured: its level over the median bin at the bin the words put
//! it. One JSON line per (RF, divider).

mod bench;

use std::time::Duration;

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::{Link, Rtl};
use sdr_rtl::demod::ddc_if_hz;
use sdr_rtl::image::{R16, R20, R21, R22};
use sdr_rtl::pll::{Pll, VCO_INDICATOR_MASK, r2_says_locked};

const TONE_OFFSET_HZ: f64 = 300e3;
const SEL_DIVS: [(u8, u32); 2] = [(0b010, 8), (0b011, 16)];
const PLL_SETTLE: Duration = Duration::from_millis(20);
const TOL_BINS: usize = 6;

fn program(link: &Link<'_>, pll: &Pll) -> Result<[u8; 5], String> {
    link.with_tuner(|l| {
        for (reg, v) in [
            (R16, pll.r16()),
            (R20, pll.r20()),
            (R21, pll.r21()),
            (R22, pll.r22()),
        ] {
            l.tuner_write(reg, v)?;
        }
        std::thread::sleep(PLL_SETTLE);
        l.tuner_status()
    })
    .map_err(|e| e.to_string())
}

/// The record header's `harness` (the generator's provenance): the
/// sdr-rtl commit and whether the work tree differs from it.
fn harness_json() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let head = git(&["rev-parse", "--short", "HEAD"]);
    let dirty = !git(&["status", "--porcelain", "--", "."]).is_empty();
    format!(
        "{{\"crates_rtl_head\":\"{head}\",\"harness_uncommitted\":{dirty},\"program\":\"examples/vcoedge.rs\"}}"
    )
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str, d: f64| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (from, to, step) = (get("--from", 200.0), get("--to", 232.0), get("--step", 1.0));
    // `--driver`: at each RF the driver's own divider (`Pll::program`, the
    // smallest whose VCO is in range) instead of /8 and /16 — the
    // production view of a band's edge. `--source-cmd`: another source.
    let driver = args.iter().any(|a| a == "--driver");
    // `--divs 2,4`: these dividers at each RF (codes from the driver's
    // table) instead of /8 and /16.
    let forced: Option<Vec<(u8, u32)>> = args
        .iter()
        .position(|a| a == "--divs")
        .and_then(|i| args.get(i + 1))
        .map(|v| {
            v.split(',')
                .filter_map(|d| d.parse::<u32>().ok())
                .filter_map(|d| {
                    sdr_rtl::pll::usable_codes()
                        .into_iter()
                        .find(|&(_, x)| x == d)
                })
                .collect()
        });
    let cmd = args
        .iter()
        .position(|a| a == "--source-cmd")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| SOURCE_CMD.into());
    let mut src = Source::spawn(&cmd, 0, -15)?;
    let mut rtl = Rtl::open(&rtl_spec()).map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    println!(
        "{{\"kind\":\"header\",\"step\":\"14\",\"measured\":\"R2 (VCO_INDICATOR) and a tone 300 kHz above the RF, RF {from}-{to} MHz in {step} MHz steps, dividers {}\",\"harness\":{},\"bench\":\"gains LNA 8 mixer 8 VGA 2, {rate} sps, txvga 0, backoff -15 dB\",\"source\":{:?},\"line_legend\":\"rf Hz, divider, vco_hz, r2 = R2 & 0x7F read 20 ms after the PLL write, r2_says_locked, tone_over_median_db at the bin the words put the tone, tone_hz\"}}",
        if driver {
            "the driver's".to_string()
        } else {
            format!(
                "{:?}",
                forced
                    .as_ref()
                    .map(|f| f.iter().map(|d| d.1).collect::<Vec<_>>())
                    .unwrap_or_else(|| SEL_DIVS.iter().map(|d| d.1).collect())
            )
        },
        harness_json(),
        cmd
    );
    let n = ((to - from) / step).round() as i64;
    for i in 0..=n {
        let rf = (from + i as f64 * step) * 1e6;
        // The driver refusing the tuning (no lock) is a reading, not the
        // end of the sweep: it is recorded, and the forced dividers are
        // still written and read at this RF (the band path stays the last
        // one the driver set, a step away).
        if let Err(e) = rtl.set_freq(&TuneRequest::auto(rf)) {
            println!(
                "{{\"rf\":{rf},\"divider\":\"driver\",\"refused\":{:?}}}",
                e.to_string()
            );
            if driver {
                continue;
            }
        }
        src.tone(rf + TONE_OFFSET_HZ)?;
        let lo = rf + ddc_if_hz();
        if driver {
            // What `set_freq` settled on (it falls back to the next divider
            // when the first does not lock, findings 21): read, not rewritten.
            let st = rtl.tuner_status().map_err(|e| e.to_string())?;
            let c = spectrum_capture(&mut s, &mut buf)?;
            let p = spectrum(&c.samples).ok_or("short capture")?;
            let actual = st.rf_hz;
            let k = near(&p, bin_of(rf + TONE_OFFSET_HZ - actual, rate), TOL_BINS);
            println!(
                "{{\"rf\":{rf},\"divider\":{},\"vco_hz\":{:.0},\"r2\":{},\"r2_says_locked\":{},\"tone_over_median_db\":{:.1},\"tone_hz\":{:.0}}}",
                st.pll.divider,
                st.pll.vco_hz,
                st.pll.vco_indicator,
                st.pll.locked,
                db(p[k] / median(&p)),
                peak_hz(&p, k, rate)
            );
            continue;
        }
        let divs: Vec<(u8, u32)> = if let Some(f) = &forced {
            f.clone()
        } else {
            SEL_DIVS.to_vec()
        };
        for (code, div) in divs {
            let pll = match Pll::with_divider(lo, code) {
                Ok(p) => p,
                Err(e) => {
                    println!("{{\"rf\":{rf},\"divider\":{div},\"error\":\"{e}\"}}");
                    continue;
                }
            };
            let st = program(&rtl.link(), &pll)?;
            let c = spectrum_capture(&mut s, &mut buf)?;
            let p = spectrum(&c.samples).ok_or("short capture")?;
            let actual = pll.lo_hz() - ddc_if_hz();
            let k = near(&p, bin_of(rf + TONE_OFFSET_HZ - actual, rate), TOL_BINS);
            let r2 = st[2] & VCO_INDICATOR_MASK;
            println!(
                "{{\"rf\":{rf},\"divider\":{div},\"vco_hz\":{:.0},\"r2\":{r2},\"r2_says_locked\":{},\"tone_over_median_db\":{:.1},\"tone_hz\":{:.0}}}",
                pll.vco_hz(),
                r2_says_locked(st[2]),
                db(p[k] / median(&p)),
                peak_hz(&p, k, rate)
            );
        }
    }
    src.off()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("vcoedge: {e}");
        std::process::exit(1);
    }
}
