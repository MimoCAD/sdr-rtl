//! The demodulator's sampling-frequency-offset readback against the rate
//! the stream delivers (findings 29). `sfreq_off`, page 3 {0x18, 0x19}
//! [13:0], s(14,24f): the offset in parts of 2²⁴ ([RTL2832U] Table 12,
//! p. 24; §9.9, p. 23; `derivation/documents.md`), its full scale ±8192 =
//! ±2⁻¹¹ = ±488.28 ppm. Figure 1 draws a Timing Error loop from
//! Synchronizations into the Resampler; if the readback is that loop's
//! state, the delivered rate follows it.
//!
//!   sfreqwatch [--seconds S] [--freq HZ] [--every-ms MS] [--rate SPS]
//!              [--demod page:reg:value]… [--reset-pulse]
//!
//! Built with `--features bench`; `SDR_BENCH_RTL` names the dongle. Open,
//! the rate (default 2.4 Msps), the RF (default 300 MHz), gains 8/8/2;
//! the readback once idle, then the raw stream read without pause and the
//! readback every MS (default 1000), each with the samples delivered in
//! that interval against the rate × its time. One JSON line per reading.
//! Each read while streaming is a control transfer, which costs the
//! stream ~950 samples at 2.4 Msps (findings 16); the chip's dropped-block
//! counter is read at the end, so those are counted, not hidden.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::OPEN_RF_HZ;
use sdr_rtl::Stage;
use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{
    ACCESS_SET, BLOCK_SYS, DEMOD_CTL, DEMOD_CTL_RESET_RELEASE, windex, wvalue_sys,
};

/// `sfreq_off`: page 3, MSB at 0x18, LSB at 0x19 (Table 12's {MSB, LSB}).
const SFREQ_OFF_PAGE: u8 = 3;
const SFREQ_OFF_MSB: u8 = 0x18;
const SFREQ_OFF_BITS: u32 = 14;
/// s(14,24f): the value over 2²⁴.
const SFREQ_OFF_FRACTION_BITS: i32 = 24;

fn sfreq_off(rtl: &Rtl) -> Result<(i32, [u8; 2]), String> {
    let v = rtl
        .link()
        .demod_read(SFREQ_OFF_PAGE, SFREQ_OFF_MSB, 2)
        .map_err(|e| e.to_string())?;
    let raw = [v[0], v[1]];
    let word = (u16::from_be_bytes(raw) as u32) & ((1 << SFREQ_OFF_BITS) - 1);
    // Sign-extend the 14-bit field.
    let value = ((word << (32 - SFREQ_OFF_BITS)) as i32) >> (32 - SFREQ_OFF_BITS);
    Ok((value, raw))
}

fn ppm_of(value: i32) -> f64 {
    value as f64 / 2f64.powi(SFREQ_OFF_FRACTION_BITS) * 1e6
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f64>().ok())
    };
    let secs = get("--seconds").unwrap_or(60.0);
    let freq = get("--freq").unwrap_or(OPEN_RF_HZ);
    let every = Duration::from_millis(get("--every-ms").unwrap_or(1000.0) as u64);
    let rate_asked = get("--rate").unwrap_or(2.4e6);
    // `--demod page:reg:value`, repeatable: a demodulator write inside the
    // stream's start sequence (`stream::set_bench_start_writes`, as
    // record's).
    let num = |s: &str| {
        if let Some(h) = s.strip_prefix("0x") {
            u8::from_str_radix(h, 16).ok()
        } else {
            s.parse().ok()
        }
    };
    let mut demod_writes: Vec<(u8, u8, u8)> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--demod" {
            let spec = args.get(i + 1).ok_or("--demod page:reg:value")?;
            let v: Vec<u8> = spec.split(':').filter_map(num).collect();
            let [page, reg, value] = v[..] else {
                return Err(format!("--demod {spec}: page:reg:value"));
            };
            demod_writes.push((page, reg, value));
        }
    }
    sdr_rtl::stream::set_bench_start_writes(&demod_writes);
    let reset_pulse = args.iter().any(|a| a == "--reset-pulse");
    let spec = rtl_spec();
    let mut rtl = Rtl::open(&spec).map_err(|e| e.to_string())?;
    rtl.set_rate(rate_asked).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(freq))
        .map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    let chip_rate = rtl.chip_rate();
    let (v, raw) = sfreq_off(&rtl)?;
    println!(
        "{{\"kind\":\"idle\",\"dongle\":{spec:?},\"freq\":{freq},\"chip_rate\":{chip_rate},\"sfreq_off\":{v},\"raw\":[{},{}],\"sfreq_ppm\":{:.2},\"demod_writes\":{demod_writes:?},\"reset_pulse\":{reset_pulse}}}",
        raw[0],
        raw[1],
        ppm_of(v)
    );
    // `--reset-pulse`: DEMOD_CTL's reset bit held (0) and released, idle.
    if reset_pulse {
        let ctl = rtl.link().sys_read8(DEMOD_CTL).map_err(|e| e.to_string())?;
        let index = windex(BLOCK_SYS, ACCESS_SET, 0);
        rtl.link()
            .set(
                wvalue_sys(DEMOD_CTL),
                index,
                &[ctl & !DEMOD_CTL_RESET_RELEASE],
                "reset held",
            )
            .map_err(|e| e.to_string())?;
        rtl.link()
            .set(
                wvalue_sys(DEMOD_CTL),
                index,
                &[ctl | DEMOD_CTL_RESET_RELEASE],
                "reset released",
            )
            .map_err(|e| e.to_string())?;
        let (v, raw) = sfreq_off(&rtl)?;
        println!(
            "{{\"kind\":\"after_reset\",\"demod_ctl\":{ctl},\"sfreq_off\":{v},\"raw\":[{},{}],\"sfreq_ppm\":{:.2}}}",
            raw[0],
            raw[1],
            ppm_of(v)
        );
    }
    let mut s = rtl
        .rx_stream(TRANSFER_BYTES, QUEUE_DEPTH)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let (mut n, mut n_mark, mut t_mark) = (0u64, 0u64, Duration::ZERO);
    let mut next = every;
    while t0.elapsed() < Duration::from_secs_f64(secs) {
        let info = s
            .recv_into(&mut buf, RECV_TIMEOUT_S)
            .map_err(|e| e.to_string())?;
        n += info.n_samples as u64;
        let now = t0.elapsed();
        if now >= next {
            let (v, raw) = sfreq_off(&rtl)?;
            let dt = (now - t_mark).as_secs_f64();
            let delivered_ppm = ((n - n_mark) as f64 / (chip_rate * dt) - 1.0) * 1e6;
            println!(
                "{{\"kind\":\"reading\",\"secs\":{:.3},\"sfreq_off\":{v},\"raw\":[{},{}],\"sfreq_ppm\":{:.2},\"samples\":{n},\"interval_delivered_ppm\":{delivered_ppm:.0}}}",
                now.as_secs_f64(),
                raw[0],
                raw[1],
                ppm_of(v)
            );
            n_mark = n;
            t_mark = now;
            next += every;
        }
    }
    let total = t0.elapsed().as_secs_f64();
    s.stop().map_err(|e| e.to_string())?;
    let dropped = s.take_dropped_blocks().map_err(|e| e.to_string())?;
    let (v, raw) = sfreq_off(&rtl)?;
    println!(
        "{{\"kind\":\"final\",\"secs\":{total:.3},\"samples\":{n},\"delivered_ppm\":{:.0},\"dropped_blocks\":{},\"saturated\":{},\"sfreq_off_stopped\":{v},\"raw\":[{},{}]}}",
        (n as f64 / (chip_rate * total) - 1.0) * 1e6,
        dropped.blocks,
        dropped.saturated,
        raw[0],
        raw[1]
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        println!("{{\"kind\":\"error\",\"error\":{e:?}}}");
        std::process::exit(1);
    }
}
