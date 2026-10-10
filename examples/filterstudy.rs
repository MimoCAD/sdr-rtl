//! The filter study: what the 20 tap registers (page 1 0x1C-0x2F) do,
//! where the 2.4 MHz alias comes from, and how tap sets score.
//!
//!   filterstudy poweron
//!   filterstudy linearity [--dagc on|off]
//!   filterstudy sweep --rate SPS --set SPEC [--set SPEC ...]
//!                     [--from HZ --to HZ --step HZ] [--dagc on|off]
//!   filterstudy matrix (the same options as sweep)
//!   (any: --source-cmd "<command line>", --level TXVGA BACKOFF,
//!    --gain LNA MIX VGA codes)
//!
//! Built with `--features bench`; run from the workspace root. One JSON
//! line per measurement on stdout, progress on stderr.
//!
//! - `poweron`: TARGET A power-cycled through the analyzer
//!   (`cynthion-witness vbus off|on`), the dongle opened (`Rtl::open`
//!   writes no tap register), the 20 registers read: the chip's power-on
//!   image.
//! - `linearity`: a +300 kHz tone stepped in level; whether the output
//!   follows dB for dB (no gain loop between the ADC and the samples) or
//!   holds level (one does). With `--dagc off` page 1 0x11 bit 0 (en_dagc,
//!   [RTL2832U] Table 10) is cleared first.
//! - `sweep`: per rate and tap set, a new stream (the whole start
//!   sequence with that set), a tone-off reference, then the tone stepped
//!   over the span: the strongest lines (frequency, dBFS), the median bin,
//!   and the level where two fold models put the tone — folded once at
//!   the output rate, and folded first at 2.4 MHz then at the output rate.
//! - `matrix`: the same records, the loops swapped — the tone stepped once
//!   and, at each step, a new stream per set — since the source's retune
//!   (~1 s) costs more than a stream's start (~0.15 s).
//!
//! SPEC: `design` (our taps), `hex:<40 hex digits>` (a register image),
//! `fields:c0,...,c15` (the 16 fields, `taps::pack`), `single:K:V` (field K
//! = V, the rest 0), `pair:K:V:K2:V2`.
//!
//! Levels are dBFS per bin: a full-scale complex tone on a bin reads
//! 0 dB (Hann window, 4,096 points, 16 segments averaged).

mod bench;

use std::time::Instant;

use bench::*;
use sdr_core::gain::Stage;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::protocol::{DAGC_OFF, DAGC_PAGE, EN_DAGC};
use sdr_rtl::taps::{self, TAP_REGS, TAPS};
use sdr_rtl::{OPEN_RF_HZ, Rtl, RtlStream};

/// Bins a line may sit from where a model puts it: the source and the
/// dongle differ by ~4.2 ppm (findings 11), +1.3 kHz at 300 MHz.
const MODEL_TOL_BINS: usize = 4;
/// The stage rate findings 10.3 inferred (28.8 MHz / 12).
const STAGE_HZ: f64 = 2.4e6;
/// Lines reported per capture.
const LINES: usize = 4;
/// Bins two reported lines must be apart.
const LINE_SEP_BINS: usize = 8;
const LINEARITY_BACKOFFS_DB: [i32; 8] = [-10, -15, -20, -25, -30, -35, -40, -45];
const LINEARITY_OFFSET_HZ: f64 = 300e3;

fn emit(kind: &str, body: &str) {
    println!("{{\"kind\":\"{kind}\",{body}}}");
}

fn hex(r: &[u8]) -> String {
    r.iter().map(|b| format!("{b:02X}")).collect()
}

fn fields_json(r: &[u8; TAP_REGS]) -> String {
    let f = taps::unpack(r);
    format!("[{}]", f.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","))
}

fn parse_set(spec: &str) -> Result<[u8; TAP_REGS], String> {
    let num = |s: &str| s.parse::<i32>().map_err(|_| format!("bad number {s} in {spec}"));
    let field = |s: &str| s.parse::<usize>().ok().filter(|k| *k < TAPS).ok_or(format!("bad field {s} in {spec}"));
    let pack = |c: [i32; TAPS]| taps::pack(&c).map_err(|e| format!("{spec}: {e}"));
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        ["design"] => Ok(taps::design_registers()),
        ["hex", h] if h.len() == 2 * TAP_REGS => {
            let mut r = [0u8; TAP_REGS];
            for (k, b) in r.iter_mut().enumerate() {
                *b = u8::from_str_radix(&h[2 * k..2 * k + 2], 16).map_err(|_| format!("bad hex in {spec}"))?;
            }
            Ok(r)
        }
        ["fields", list] => {
            let v: Vec<i32> = list.split(',').map(num).collect::<Result<_, _>>()?;
            let c: [i32; TAPS] = v.try_into().map_err(|_| format!("{spec}: need {TAPS} fields"))?;
            pack(c)
        }
        ["single", k, v] => {
            let mut c = [0; TAPS];
            c[field(k)?] = num(v)?;
            pack(c)
        }
        ["pair", k, v, k2, v2] => {
            let mut c = [0; TAPS];
            c[field(k)?] = num(v)?;
            c[field(k2)?] = num(v2)?;
            pack(c)
        }
        _ => Err(format!("unknown set {spec}")),
    }
}

/// Fold `hz` into (−rate/2, rate/2].
fn fold(hz: f64, rate: f64) -> f64 {
    hz - (hz / rate).round() * rate
}

fn poweron() -> Result<(), String> {
    let cycle = power_cycle()?;
    let rtl = Rtl::open("").map_err(|e| e.to_string())?;
    let r = rtl.taps().map_err(|e| e.to_string())?;
    emit("poweron", &format!("\"cycle\":\"{cycle}\",\"hex\":\"{}\",\"fields\":{}", hex(&r), fields_json(&r)));
    Ok(())
}

fn set_dagc(rtl: &Rtl, on: bool) -> Result<u8, String> {
    rtl.link().demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, if on { EN_DAGC } else { 0 }).map_err(|e| e.to_string())
}

fn open_stream(rtl: &mut Rtl, regs: [u8; TAP_REGS]) -> Result<(RtlStream, Vec<Iq>), String> {
    let mut s = rtl.rx_stream_with_taps(TRANSFER_BYTES, QUEUE_DEPTH, regs).map_err(|e| e.to_string())?;
    let buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    Ok((s, buf))
}

use sdr_core::RxSource;

fn linearity(rtl: &mut Rtl, src: &mut Source, dagc: Option<bool>) -> Result<(), String> {
    if let Some(on) = dagc {
        set_dagc(rtl, on)?;
    }
    let rate = rtl.rate().map_err(|e| e.to_string())?;
    let (mut s, mut buf) = open_stream(rtl, taps::design_registers())?;
    let k = bin_of(LINEARITY_OFFSET_HZ, rate);
    src.off()?;
    let c = spectrum_capture(&mut s, &mut buf)?;
    let p = spectrum(&c.samples).ok_or("short capture")?;
    emit("linearity", &format!("\"dagc\":\"{dagc:?}\",\"backoff_db\":null,\"tone_dbfs\":{:.2},\"median_dbfs\":{:.2}", db(p[near(&p, k, MODEL_TOL_BINS)]), db(median(&p))));
    for b in LINEARITY_BACKOFFS_DB {
        src.tone_at(OPEN_RF_HZ + LINEARITY_OFFSET_HZ, src.txvga, b)?;
        let c = spectrum_capture(&mut s, &mut buf)?;
        let p = spectrum(&c.samples).ok_or("short capture")?;
        emit(
            "linearity",
            &format!(
                "\"dagc\":\"{dagc:?}\",\"backoff_db\":{b},\"tone_dbfs\":{:.2},\"median_dbfs\":{:.2},\"clipped\":{:.5}",
                db(p[near(&p, k, MODEL_TOL_BINS)]),
                db(median(&p)),
                clipped(&c.samples)
            ),
        );
    }
    src.off()?;
    Ok(())
}

fn lines_json(p: &[f64], rate: f64) -> String {
    let v: Vec<String> = peaks(p, LINES, LINE_SEP_BINS).iter().map(|(k, pw)| format!("[{:.0},{:.2}]", peak_hz(p, *k, rate), db(*pw))).collect();
    format!("[{}]", v.join(","))
}

struct Span {
    from: f64,
    to: f64,
    step: f64,
}

/// The tone-off reference of one set at one rate.
fn head(s: &mut RtlStream, buf: &mut [Iq], src: &Source, rate: f64, name: &str, regs: &[u8; TAP_REGS], span: &Span) -> Result<(), String> {
    let c = spectrum_capture(s, buf)?;
    let p = spectrum(&c.samples).ok_or("short capture")?;
    emit(
        "sweep_head",
        &format!(
            "\"rate\":{rate},\"set\":\"{name}\",\"hex\":\"{}\",\"fields\":{},\"rf\":{OPEN_RF_HZ},\"from\":{},\"to\":{},\"step\":{},\"off_median_dbfs\":{:.2},\"off_lines\":{},\"level\":[{},{}]",
            hex(regs),
            fields_json(regs),
            span.from,
            span.to,
            span.step,
            db(median(&p)),
            lines_json(&p, rate),
            src.txvga,
            src.backoff
        ),
    );
    Ok(())
}

/// One capture with the tone at RF + `d`.
fn point(s: &mut RtlStream, buf: &mut [Iq], rate: f64, name: &str, d: f64) -> Result<(), String> {
    let c = spectrum_capture(s, buf)?;
    let p = spectrum(&c.samples).ok_or("short capture")?;
    let plain = near(&p, bin_of(fold(d, rate), rate), MODEL_TOL_BINS);
    let staged = near(&p, bin_of(fold(fold(d, STAGE_HZ), rate), rate), MODEL_TOL_BINS);
    emit(
        "sweep",
        &format!(
            "\"rate\":{rate},\"set\":\"{name}\",\"delta\":{d},\"plain_hz\":{:.0},\"plain_dbfs\":{:.2},\"staged_hz\":{:.0},\"staged_dbfs\":{:.2},\"median_dbfs\":{:.2},\"lines\":{},\"clipped\":{:.5},\"overflows\":{}",
            fold(d, rate),
            db(p[plain]),
            fold(fold(d, STAGE_HZ), rate),
            db(p[staged]),
            db(median(&p)),
            lines_json(&p, rate),
            clipped(&c.samples),
            c.overflows
        ),
    );
    Ok(())
}

fn offsets(span: &Span) -> Vec<f64> {
    let n = ((span.to - span.from) / span.step).round() as i64;
    (0..=n).map(|i| span.from + i as f64 * span.step).collect()
}

/// One set at one rate: one stream, the tone stepped.
fn sweep(rtl: &mut Rtl, src: &mut Source, rate: f64, name: &str, regs: [u8; TAP_REGS], span: &Span) -> Result<(), String> {
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let (mut s, mut buf) = open_stream(rtl, regs)?;
    src.off()?;
    head(&mut s, &mut buf, src, rate, name, &regs, span)?;
    let t = Instant::now();
    let ds = offsets(span);
    for &d in &ds {
        src.tone(OPEN_RF_HZ + d)?;
        point(&mut s, &mut buf, rate, name, d)?;
    }
    eprintln!("sweep {name} at {rate}: {} points in {:.0} s", ds.len(), t.elapsed().as_secs_f64());
    src.off()?;
    Ok(())
}

/// Many sets at one rate: the tone stepped once, and at each step a new
/// stream per set (the whole start sequence with that set's taps).
fn matrix(rtl: &mut Rtl, src: &mut Source, rate: f64, sets: &[(String, [u8; TAP_REGS])], span: &Span) -> Result<(), String> {
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    src.off()?;
    for (name, regs) in sets {
        let (mut s, mut buf) = open_stream(rtl, *regs)?;
        head(&mut s, &mut buf, src, rate, name, regs, span)?;
    }
    let t = Instant::now();
    let ds = offsets(span);
    for &d in &ds {
        src.tone(OPEN_RF_HZ + d)?;
        for (name, regs) in sets {
            let (mut s, mut buf) = open_stream(rtl, *regs)?;
            point(&mut s, &mut buf, rate, name, d)?;
        }
    }
    eprintln!("matrix of {} sets at {rate}: {} points in {:.0} s", sets.len(), ds.len(), t.elapsed().as_secs_f64());
    src.off()?;
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().ok_or("filterstudy poweron | linearity | sweep")?;
    let mut source_cmd = SOURCE_CMD.to_string();
    let (mut txvga, mut backoff) = (0u16, -20i32);
    let mut dagc = None;
    let mut codes: Option<[u8; 3]> = None;
    let mut rates = Vec::new();
    let mut sets = Vec::new();
    let mut span = Span { from: -4e6, to: 4e6, step: 50e3 };
    let mut it = args.iter().skip(1);
    let val = |it: &mut std::iter::Skip<std::slice::Iter<String>>, what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
    let f = |s: String| s.parse::<f64>().map_err(|_| format!("bad number {s}"));
    while let Some(a) = it.next() {
        match a.as_str() {
            "--source-cmd" => source_cmd = val(&mut it, a)?,
            "--level" => {
                txvga = val(&mut it, a)?.parse().map_err(|_| "--level TXVGA BACKOFF")?;
                backoff = val(&mut it, a)?.parse().map_err(|_| "--level TXVGA BACKOFF")?;
            }
            "--dagc" => dagc = Some(val(&mut it, a)? == "on"),
            "--gain" => {
                let mut c = [0u8; 3];
                for v in &mut c {
                    *v = val(&mut it, a)?.parse().map_err(|_| "--gain LNA MIX VGA (codes 0-15)")?;
                }
                codes = Some(c);
            }
            "--rate" => rates.push(f(val(&mut it, a)?)?),
            "--set" => sets.push(val(&mut it, a)?),
            "--from" => span.from = f(val(&mut it, a)?)?,
            "--to" => span.to = f(val(&mut it, a)?)?,
            "--step" => span.step = f(val(&mut it, a)?)?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if cmd == "poweron" {
        return poweron();
    }
    let mut src = Source::spawn(&source_cmd, txvga, backoff)?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ)).map_err(|e| e.to_string())?;
    if let Some([l, m, v]) = codes {
        for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
            rtl.set_stage_level(stage, code as usize).map_err(|e| e.to_string())?;
        }
    }
    eprintln!("gain codes {:?} = {:.1} dB", rtl.stage_readings(), rtl.gain());
    match cmd.as_str() {
        "linearity" => linearity(&mut rtl, &mut src, dagc),
        "sweep" => {
            if let Some(on) = dagc {
                let v = set_dagc(&rtl, on)?;
                eprintln!("page 1 0x11 now 0x{v:02X}");
            }
            for &rate in &rates {
                for spec in &sets {
                    sweep(&mut rtl, &mut src, rate, spec, parse_set(spec)?, &span)?;
                }
            }
            Ok(())
        }
        "matrix" => {
            let parsed: Vec<(String, [u8; TAP_REGS])> = sets.iter().map(|s| parse_set(s).map(|r| (s.clone(), r))).collect::<Result<_, _>>()?;
            for &rate in &rates {
                matrix(&mut rtl, &mut src, rate, &parsed, &span)?;
            }
            Ok(())
        }
        other => Err(format!("unknown command {other}")),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("filterstudy: {e}");
        std::process::exit(1);
    }
}
