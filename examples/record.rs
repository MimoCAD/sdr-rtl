//! A SigMF capture through the driver, for the side-by-side benchmark
//! (findings 13): ci16_le SigMF, the format the earlier driver's captures
//! were taken in, so both go through one offline decoder.
//!
//!   record --freq HZ --seconds S --out NAME [--rate SPS] [--gain LNA MIX VGA]
//!          [--transfer BYTES] [--depth N]   (default 16 KiB × 16)
//!          [--dagc-off]   (the demodulator's digital AGC disabled)
//!          [--fields c0,…,c15]   (the FIR's taps instead of the driver's)
//!          [--if HZ]   (the IF instead of the driver's 3.57 MHz)
//!          [--rate-floor SPS]   (the chip itself at rates the stream would
//!          decimate)
//!          [--demod page:reg:value]   (a demodulator write inside the
//!          stream's start sequence, repeatable; findings 21)
//!          [--regs FILE --group NAME …]   (a register group file's groups
//!          on top of the driver's setup, `groups/mod.rs`; with `--except`,
//!          `--base`, `--freq-dependent`, `--after-start`)
//!
//! Built with `--features bench` (the drop counter is read through the
//! request layer). Writes `NAME.sigmf-data` (ci16_le, the stream's 8-bit
//! samples widened as `WireSample::OffsetU8` does) and `NAME.sigmf-meta`,
//! and prints one JSON line: samples delivered against the rate × the
//! time, the engine's overflow flags, and USB_EPA_FIFO_CFG[31:24] — the
//! chip's count of 188-byte blocks dropped with the FIFO full ([RTL2832U]
//! Table 32, `derivation/documents.md`), read before and after (an 8-bit
//! counter, so the difference is mod 256).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

mod groups;

use sdr_core::{Iq, TuneRequest};
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{ACCESS_GET, BLOCK_USB, windex, wvalue_usb};

/// USB_EPA_FIFO_CFG, offset 0x160 in the USB block; bits 31:24 are its
/// fourth byte (32-bit registers read little-endian, findings step 1A).
const EPA_FIFO_CFG: u16 = 0x160;
const DROP_COUNTER_BYTE: usize = 3;
const TRANSFER_BYTES: usize = 16 * 1024;
const QUEUE_DEPTH: usize = 16;
const RECV_TIMEOUT_S: f64 = 0.25;

fn drop_counter(rtl: &Rtl) -> Result<u8, String> {
    let v = rtl
        .link()
        .get(
            wvalue_usb(EPA_FIFO_CFG),
            windex(BLOCK_USB, ACCESS_GET, 0),
            4,
            "USB_EPA_FIFO_CFG read",
        )
        .map_err(|e| e.to_string())?;
    Ok(v[DROP_COUNTER_BYTE])
}

/// UTC now as ISO 8601 (`core:datetime`, which the decoder requires), to
/// the microsecond (a recording's calls are matched against another
/// receiver's log by their start times): days since 1970 to a civil date
/// (the proleptic Gregorian calendar).
fn utc_at(when: std::time::SystemTime) -> String {
    let now = when
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, micros) = (now.as_secs() as i64, now.subsec_micros());
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn run() -> Result<(), String> {
    let (group_args, args) = groups::Args::take(&std::env::args().skip(1).collect::<Vec<String>>());
    let (mut freq, mut secs, mut out, mut rate) = (None, None, None, 2.4e6);
    let (mut transfer, mut depth) = (TRANSFER_BYTES, QUEUE_DEPTH);
    let mut codes: Option<[usize; 3]> = None;
    let mut dagc_off = false;
    let mut fields: Option<[i32; sdr_rtl::taps::TAPS]> = None;
    let mut demod_writes: Vec<(u8, u8, u8)> = Vec::new();
    let mut it = args.iter();
    let num = |v: Option<&String>| v.and_then(|s| s.parse::<f64>().ok()).ok_or("bad number");
    while let Some(a) = it.next() {
        match a.as_str() {
            "--freq" => freq = Some(num(it.next())?),
            "--seconds" => secs = Some(num(it.next())?),
            "--rate" => rate = num(it.next())?,
            "--out" => out = it.next().cloned(),
            "--transfer" => transfer = num(it.next())? as usize,
            "--depth" => depth = num(it.next())? as usize,
            "--dagc-off" => dagc_off = true,
            // The IF in place of the driver's (findings 21, `iftest`).
            "--if" => sdr_rtl::demod::set_bench_if_hz(Some(num(it.next())?)),
            // Rates under the chip's direct spans at the chip itself
            // (`demod::set_bench_rate_floor`), for chip-level records.
            "--rate-floor" => sdr_rtl::demod::set_bench_rate_floor(Some(num(it.next())?)),
            "--fields" => {
                let v: Vec<i32> = it
                    .next()
                    .ok_or("--fields c0,…,c15")?
                    .split(',')
                    .filter_map(|x| x.trim().parse().ok())
                    .collect();
                fields = Some(
                    v.try_into()
                        .map_err(|_| format!("--fields needs {} numbers", sdr_rtl::taps::TAPS))?,
                );
            }
            // A demodulator write inside the stream's start sequence
            // (`stream::set_bench_start_writes`, as ifsweep's `--demod`).
            "--demod" => {
                let spec = it.next().ok_or("--demod page:reg:value")?;
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
            "--gain" => {
                let mut c = [0usize; 3];
                for v in &mut c {
                    *v = num(it.next())? as usize;
                }
                codes = Some(c);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (freq, secs, out) = (
        freq.ok_or("--freq")?,
        secs.ok_or("--seconds")?,
        out.ok_or("--out")?,
    );
    sdr_rtl::stream::set_bench_start_writes(&demod_writes);
    // `SDR_BENCH_RTL`: which dongle on a host with more than one.
    let mut plan = groups::Plan::new(&group_args, freq)?;
    plan.before_open();
    let mut rtl = Rtl::open(&std::env::var("SDR_BENCH_RTL").unwrap_or_default())
        .map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    // The raw stream is the chip's samples at the chip's rate: for a rate
    // the driver's Stream makes by decimation, k times the rate asked for.
    // Everything below (the timing, the expected count, `core:sample_rate`)
    // is at that rate; the sidecar keeps the rate asked for and k beside
    // it.
    let (asked, decimation) = (rate, rtl.decimation());
    let rate = rtl.chip_rate();
    let tuned = rtl
        .set_freq(&TuneRequest::auto(freq))
        .map_err(|e| e.to_string())?;
    if let Some([l, m, v]) = codes {
        for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
            rtl.set_stage_level(stage, code)
                .map_err(|e| e.to_string())?;
        }
    }
    // `--dagc-off`: the demodulator's digital AGC (page 1 0x11 bit 0)
    // disabled after the open, so the output's gain stays fixed whatever
    // the window holds (findings 21).
    if dagc_off {
        use sdr_rtl::protocol::{DAGC_OFF, DAGC_PAGE, EN_DAGC};
        rtl.link()
            .demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, 0)
            .map_err(|e| e.to_string())?;
    }
    plan.apply(&rtl)?;
    let before = drop_counter(&rtl)?;
    let tap_regs = match (fields, plan.taps) {
        (Some(c), _) => Some(sdr_rtl::taps::pack(&c).map_err(|e| e.to_string())?),
        (None, t) => t,
    };
    let mut s = match tap_regs {
        Some(r) => rtl.rx_stream_with_taps(transfer, depth, r),
        None => rtl.rx_stream(transfer, depth),
    }
    .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut w =
        BufWriter::new(File::create(format!("{out}.sigmf-data")).map_err(|e| e.to_string())?);
    s.start().map_err(|e| e.to_string())?;
    if group_args.file.is_some() {
        let again = if group_args.after_start {
            plan.after_start(&rtl)?
        } else {
            0
        };
        eprintln!("{}", plan.summary());
        eprintln!(
            "{{\"kind\":\"regs_check\",\"start_owned\":{:?},\"rewritten_after_start\":{again}}}",
            plan.start_owned_writes()
        );
    }
    let t = Instant::now();
    let (mut n, mut overflows, mut timeouts) = (0usize, 0usize, 0usize);
    // `core:datetime` is sample 0's time: the clock when the first samples
    // arrive, less their own duration (stamping before the open and the
    // stream's start put every derived time ~0.5 s early, measured
    // 2026-10-16 against the earlier driver's recordings of the same air).
    let mut started: Option<String> = None;
    while t.elapsed().as_secs_f64() < secs {
        let info = s
            .recv_into(&mut buf, RECV_TIMEOUT_S)
            .map_err(|e| e.to_string())?;
        if started.is_none() && info.n_samples > 0 {
            let lead = std::time::Duration::from_secs_f64(info.n_samples as f64 / rate);
            started = Some(utc_at(std::time::SystemTime::now() - lead));
        }
        overflows += info.overflow as usize;
        timeouts += info.timed_out as usize;
        for v in &buf[..info.n_samples] {
            w.write_all(&v.i.to_le_bytes())
                .and_then(|_| w.write_all(&v.q.to_le_bytes()))
                .map_err(|e| e.to_string())?;
        }
        n += info.n_samples;
    }
    let elapsed = t.elapsed().as_secs_f64();
    drop(s);
    w.flush().map_err(|e| e.to_string())?;
    let after = drop_counter(&rtl)?;
    plan.restore(&rtl)?;
    // `core:frequency` is the centre asked for (what every other recorder
    // writes): the PLL's actual tuning (~10-25 Hz off it) as the centre put
    // the decoder's channel grid off and sent it down its slower legacy path,
    // which missed short keyups the same air decoded on the grid (measured
    // 2026-10-16). The tuning is kept beside it.
    let started = started.unwrap_or_else(|| utc_at(std::time::SystemTime::now()));
    // Decimated rates: what the record is, in words, beside the numbers.
    let description = if decimation > 1 {
        format!(
            ",\n    \"core:description\": \"the chip's raw samples at {:.6} Msps; {:.6} Msps was asked for, which the driver's Stream makes by decimating these by {decimation}\"",
            rate / 1e6,
            asked / 1e6
        )
    } else {
        String::new()
    };
    let meta = format!(
        "{{\n  \"global\": {{\n    \"core:datatype\": \"ci16_le\",\n    \"core:sample_rate\": {rate},\n    \"core:version\": \"1.0.0\",\n    \"core:hw\": \"RTL-SDR (RTL2832U), sdr-rtl\",\n    \"core:recorder\": \"sdr-rtl record example\",\n    \"sdr_rtl:rate_asked\": {asked},\n    \"sdr_rtl:decimation\": {decimation}{description}\n  }},\n  \"captures\": [\n    {{ \"core:sample_start\": 0, \"core:frequency\": {freq}, \"core:datetime\": \"{started}\", \"sdr_rtl:tuned_hz\": {} }}\n  ],\n  \"annotations\": []\n}}\n",
        tuned.actual_rf_freq
    );
    std::fs::write(format!("{out}.sigmf-meta"), meta).map_err(|e| e.to_string())?;
    println!(
        "{{\"out\":\"{out}\",\"freq\":{},\"rate\":{rate},\"rate_asked\":{asked},\"decimation\":{decimation},\"gain_db\":{:.1},\"seconds\":{elapsed:.3},\"samples\":{n},\"expected\":{:.0},\"delivered_ratio\":{:.6},\"overflows\":{overflows},\"timeouts\":{timeouts},\"fifo_drops_before\":{before},\"fifo_drops_after\":{after},\"fifo_drops\":{}}}",
        tuned.actual_rf_freq,
        rtl.gain(),
        rate * elapsed,
        n as f64 / (rate * elapsed),
        after.wrapping_sub(before)
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("record: {e}");
        std::process::exit(1);
    }
}
