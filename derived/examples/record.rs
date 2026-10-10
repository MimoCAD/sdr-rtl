//! A SigMF capture through the driver, for the side-by-side benchmark:
//! the same file format `mimosdr record` writes, so both drivers'
//! captures go through one offline decoder.
//!
//!   record --freq HZ --seconds S --out NAME [--rate SPS] [--gain LNA MIX VGA]
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

use sdr_core::gain::Stage;
use sdr_core::{Iq, RxSource, TuneRequest};
use sdr_rtl_derived::Rtl;
use sdr_rtl_derived::protocol::{ACCESS_GET, BLOCK_USB, windex, wvalue_usb};

/// USB_EPA_FIFO_CFG, offset 0x160 in the USB block; bits 31:24 are its
/// fourth byte (32-bit registers read little-endian, findings step 1A).
const EPA_FIFO_CFG: u16 = 0x160;
const DROP_COUNTER_BYTE: usize = 3;
const TRANSFER_BYTES: usize = 16 * 1024;
const QUEUE_DEPTH: usize = 16;
const RECV_TIMEOUT_S: f64 = 0.25;

fn drop_counter(rtl: &Rtl) -> Result<u8, String> {
    let v = rtl.link().get(wvalue_usb(EPA_FIFO_CFG), windex(BLOCK_USB, ACCESS_GET, 0), 4, "USB_EPA_FIFO_CFG read").map_err(|e| e.to_string())?;
    Ok(v[DROP_COUNTER_BYTE])
}

/// UTC now as ISO 8601 (`core:datetime`, which the decoder requires):
/// days since 1970 to a civil date (the proleptic Gregorian calendar).
fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
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
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut freq, mut secs, mut out, mut rate) = (None, None, None, 2.4e6);
    let mut codes: Option<[usize; 3]> = None;
    let mut it = args.iter();
    let num = |v: Option<&String>| v.and_then(|s| s.parse::<f64>().ok()).ok_or("bad number");
    while let Some(a) = it.next() {
        match a.as_str() {
            "--freq" => freq = Some(num(it.next())?),
            "--seconds" => secs = Some(num(it.next())?),
            "--rate" => rate = num(it.next())?,
            "--out" => out = it.next().cloned(),
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
    let (freq, secs, out) = (freq.ok_or("--freq")?, secs.ok_or("--seconds")?, out.ok_or("--out")?);
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    rtl.set_rate(rate).map_err(|e| e.to_string())?;
    let tuned = rtl.set_freq(&TuneRequest::auto(freq)).map_err(|e| e.to_string())?;
    if let Some([l, m, v]) = codes {
        for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
            rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
        }
    }
    let before = drop_counter(&rtl)?;
    let started = utc_now();
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut w = BufWriter::new(File::create(format!("{out}.sigmf-data")).map_err(|e| e.to_string())?);
    s.start().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (mut n, mut overflows, mut timeouts) = (0usize, 0usize, 0usize);
    while t.elapsed().as_secs_f64() < secs {
        let info = s.recv_into(&mut buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        overflows += info.overflow as usize;
        timeouts += info.timed_out as usize;
        for v in &buf[..info.n_samples] {
            w.write_all(&v.i.to_le_bytes()).and_then(|_| w.write_all(&v.q.to_le_bytes())).map_err(|e| e.to_string())?;
        }
        n += info.n_samples;
    }
    let elapsed = t.elapsed().as_secs_f64();
    drop(s);
    w.flush().map_err(|e| e.to_string())?;
    let after = drop_counter(&rtl)?;
    let meta = format!(
        "{{\n  \"global\": {{\n    \"core:datatype\": \"ci16_le\",\n    \"core:sample_rate\": {rate},\n    \"core:version\": \"1.0.0\",\n    \"core:hw\": \"RTL-SDR (RTL2832U), sdr-rtl-derived\",\n    \"core:recorder\": \"sdr-rtl-derived record example\"\n  }},\n  \"captures\": [\n    {{ \"core:sample_start\": 0, \"core:frequency\": {}, \"core:datetime\": \"{started}\" }}\n  ],\n  \"annotations\": []\n}}\n",
        tuned.actual_rf_freq
    );
    std::fs::write(format!("{out}.sigmf-meta"), meta).map_err(|e| e.to_string())?;
    println!(
        "{{\"out\":\"{out}\",\"freq\":{},\"rate\":{rate},\"gain_db\":{:.1},\"seconds\":{elapsed:.3},\"samples\":{n},\"expected\":{:.0},\"delivered_ratio\":{:.6},\"overflows\":{overflows},\"timeouts\":{timeouts},\"fifo_drops_before\":{before},\"fifo_drops_after\":{after},\"fifo_drops\":{}}}",
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
