//! The demodulator: the bring-up state, the DDC at the tuner's IF with
//! the spectrum inverted for the high-side LO, and the resampler with
//! the output-rate rule the bench measured — [RTL2832U] 9.3, 9.4 and
//! Tables 2, 6, 7, 10, 15, with findings steps 4 and 10. The writes that
//! start, stop and pause the raw-sample stream are in `stream`.

use alloc::format;
use alloc::string::String;
use core::time::Duration;

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::math::floor;
use crate::protocol::*;
use crate::tables::pll::XTAL_HZ;
#[cfg(feature = "std")]
use crate::usb::Link;

/// pset_iffreq = −floor(f_IF / f_xtal · 2^22) ([RTL2832U] 9.3, p. 19;
/// the document's example: 4.57 MHz → −665554, zero IF → 0).
pub fn pset_iffreq(if_hz: f64) -> i32 {
    -(floor(if_hz / XTAL_HZ * TWO_POW_22) as i32)
}

/// The 22-bit two's complement over {0x19, 0x1A, 0x1B}, MSB first
/// (Table 6; 0x19 holds bits 21:16).
pub fn pset_iffreq_bytes(if_hz: f64) -> [u8; 3] {
    let u = (pset_iffreq(if_hz) as u32) & ((1 << IFFREQ_BITS) - 1);
    [(u >> 16) as u8, (u >> 8) as u8, u as u8]
}

/// The IF a pset_iffreq word shifts by: −word / 2^22 · f_xtal (9.3).
pub fn iffreq_hz_of(word: i32) -> f64 {
    -(word as f64) / TWO_POW_22 * XTAL_HZ
}

/// The IF: the LO sits this far above the RF (high-side mixing, the
/// wanted sideband below the LO, findings 4h) and the DDC shifts it to
/// 0 Hz with Spec_inv = 1 (9.3: "with a normal high-side-mixing tuner the
/// demodulator requires an inverse spectrum"). 3.570 MHz is the value of
/// the stream's sequence: pset_iffreq 0x381112 (findings 9b item 10),
/// written in every step-10 session and confirmed there by a cabled tone
/// landing at its predicted bin with the predicted sign (findings 10.2b:
/// the LO at RF + 3.570 MHz, tones at ±0.1…0.9 MHz within 3 bins).
/// Findings 4h's tuner measurements used 4.57 MHz; the tuner's IF is
/// the driver's choice (LO − RF), and the stream was benched at 3.570.
pub const DDC_IF_HZ: f64 = 3.57e6;

/// Bench only: an IF in place of [`DDC_IF_HZ`] for every receiver opened
/// after it is set (the open's DDC, the stream's sequence and the tuner's
/// LO all follow it), to measure what the IF does (findings 21). Its f64
/// bits; 0 = none.
#[cfg(feature = "bench")]
static BENCH_IF_HZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bench only: the IF for receivers opened from now on; None for
/// [`DDC_IF_HZ`].
#[cfg(feature = "bench")]
pub fn set_bench_if_hz(hz: Option<f64>) {
    BENCH_IF_HZ.store(
        hz.map_or(0, f64::to_bits),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The IF the receiver is built on: [`DDC_IF_HZ`] (or the bench's).
pub fn if_hz() -> f64 {
    #[cfg(feature = "bench")]
    {
        let b = BENCH_IF_HZ.load(std::sync::atomic::Ordering::Relaxed);
        if b != 0 {
            return f64::from_bits(b);
        }
    }
    DDC_IF_HZ
}

/// The IF the DDC actually removes: [`if_hz`] through the field's floor
/// (at [`DDC_IF_HZ`], 519918 / 2^22 · 28.8 MHz = 3,569,993.6 Hz). The
/// tuner's LO is placed this far above the wanted centre so the DDC's 0
/// Hz lands on it.
pub fn ddc_if_hz() -> f64 {
    iffreq_hz_of(pset_iffreq(if_hz()))
}

/// The resampler field's largest value: 26 bits (Table 7, [27:2]).
pub const RSAMP_MAX: u32 = (1 << RSAMP_BITS) - 1;

/// rsamp_ratio = floor(f_xtal / f_s · 2^22) ([RTL2832U] 9.4, Table 7,
/// p. 20; the document's examples at 28.8 MHz: 64/7 MHz → 0xC99999,
/// 8 MHz → 0xE66666, 48/7 MHz → 0x10CCCCC). Findings 10.2a calls the
/// rate f_s here f_symbol.
pub fn rsamp_ratio(rate_hz: f64) -> u32 {
    floor(XTAL_HZ / rate_hz * TWO_POW_22) as u32
}

/// The rate a ratio gives back: f_xtal · 2^22 / ratio (f_symbol).
pub fn rate_of_ratio(ratio: u32) -> f64 {
    XTAL_HZ * TWO_POW_22 / ratio as f64
}

/// The field's top bit (bit 25). With it set the chip resamples as if the
/// ratio were the field + 2^26 (findings 25: that rule predicts the rate
/// delivered at 24 of the 26 rates measured, 0.2-1.024 Msps, within
/// 0.03 %; 0.9 Msps, field 0x2000000, delivered 0.3 Msps as 0.3 Msps
/// does). So the ratios the chip reaches are those under 2^25 and those
/// from 3 · 2^25 to 2^27; a ratio between is resampled as another.
pub const RSAMP_TOP_BIT: u32 = 1 << (RSAMP_BITS - 1);
/// What the chip adds to a field with [`RSAMP_TOP_BIT`] set.
pub const RSAMP_TOP_WEIGHT: u32 = 1 << RSAMP_BITS;

/// The field that makes the chip resample at `ratio`: the ratio itself
/// under 2^25; ratio − 2^26 from 3 · 2^25 to 2^27 (findings 25). `ratio`
/// must be one the chip reaches ([`ratio_for_output_rate`] gives only
/// those).
pub fn rsamp_field(ratio: u32) -> u32 {
    if ratio >= RSAMP_TOP_WEIGHT {
        ratio - RSAMP_TOP_WEIGHT
    } else {
        ratio & RSAMP_MAX
    }
}

/// The ratio the chip resamples at for a field (findings 25).
pub fn ratio_of_field(field: u32) -> u32 {
    if field & RSAMP_TOP_BIT != 0 {
        field + RSAMP_TOP_WEIGHT
    } else {
        field
    }
}

/// The four bytes rsamp_ratio occupies, MSB first, its field in [27:2]
/// ([`rsamp_field`]). The other bits are 0: the upper nibble of 0x9F
/// (cfreq_off_ratio's [3:0], Table 12) and bits 1:0 of 0xA2 — as the
/// vendor driver wrote them (findings 9b item 12: 03 84 00 00) and the
/// bench's sequence did (`examples/harness/stream_confirm.rs`
/// `rsamp_bytes`).
pub fn rsamp_bytes(ratio: u32) -> [u8; 4] {
    (rsamp_field(ratio) << RSAMP_SHIFT).to_be_bytes()
}

/// The ratio the chip resamples at, from the four bytes read at 0x9F
/// ([`ratio_of_field`]).
pub fn rsamp_from_bytes(b: [u8; 4]) -> u32 {
    ratio_of_field((u32::from_be_bytes(b) >> RSAMP_SHIFT) & RSAMP_MAX)
}

/// rsamp_ratio as two 2-byte transfers, 0x9F-0xA0 then 0xA1-0xA2, as
/// observed (findings 9b item 12) and benched (stream_confirm.rs).
pub fn rsamp_transfers(ratio: u32) -> [(u8, [u8; 2]); 2] {
    let b = rsamp_bytes(ratio);
    [(RSAMP_MSB, [b[0], b[1]]), (RSAMP_MSB + 2, [b[2], b[3]])]
}

// ---- The output rate (findings 10.2a) ----

/// Output samples per f_symbol: the output rate is f_symbol / 4, the
/// bytes 2 × f_symbol / 4 (findings 9f, measured in 10.2a at every
/// ratio below).
pub const OUTPUT_DECIMATION: f64 = 4.0;

/// The main span of output rates the chip is used at directly: 0.905 Msps
/// to 2.88 Msps (0xA00000).
/// - 1.024-2.88 Msps: every fresh run delivered the rate, with 2.048
///   (0xE10000) and 2.4 (0xC00000) between (findings 10.2a). Clean all
///   through (findings 19, the USRP as the tone source): no phase steps
///   in the raw stream at 1.024, 1.152, 1.2, 1.44, 1.6, 1.8, 2.048, 2.4 or
///   2.88 Msps, and every break placed at `at_sample` with the signal
///   whole. (16.5's steps at 2.88 Msps were the HackRF source's.)
/// - 0.905-1.024 Msps (findings 25, both dongles, the USRP tone): the
///   rate delivered, the tone on its bin and no phase steps at 0.905,
///   0.91, 0.92, 0.93, 0.94, 0.95, 0.96 and 1.0 Msps; at 0.905, 0.96 and
///   1.0 every break placed at `at_sample` with the signal whole. 0.9 Msps
///   (f_symbol f_xtal / 8) and every rate measured under it down to 0.35
///   are broken.
/// - (A line at f − rate/3, 42-46 dB under a tone at 0.905-0.93 Msps, was
///   the tone source's: with the USRP's LO 137 kHz from the tone instead
///   of 400 it was gone on one dongle and not rate-bound on the other;
///   findings 25.)
///
/// - Over long runs (findings 27, 20 minutes each): two dongles on one
///   hub streaming above 2.4 Msps lost single 94-sample blocks in bursts
///   minutes apart, which the chip's counter does not count — ~440 a
///   dongle in 20 minutes at 2.72, 1,100-1,900 at 2.88. None at 2.4 in
///   four runs, none with one dongle alone at 2.88. The rates stay
///   accepted; the stream's `clockwatch` reports such loss
///   (`StreamReport::clock_lost_samples`, each receive's `overflow`).
///
/// A request inside the span is accepted; the points between the measured
/// rates were not measured singly.
pub const OUTPUT_RATE_GOOD_SPS: (f64, f64) = (0.905e6, 2.88e6);

/// A second, low span measured good: 0.24-0.30 Msps (findings 25, both
/// dongles, the USRP tone): the rate delivered, the tone on its bin and
/// no phase steps at 0.24, 0.25, 0.288 and 0.30 Msps; at 0.24, 0.288 and
/// 0.30 every break placed at `at_sample` with the signal whole. Either
/// side is broken: 0.2 and 0.225 Msps (9× and half the rate), 0.35 and
/// 0.4 (4.5×), and everything measured from there to 0.9 Msps.
pub const OUTPUT_RATE_LOW_SPS: (f64, f64) = (0.24e6, 0.30e6);

/// The rates measured broken on either side of [`OUTPUT_RATE_LOW_SPS`]
/// and under [`OUTPUT_RATE_GOOD_SPS`], for the refusals' reasons
/// (findings 25): 0.35-0.9 Msps delivered 0.33-4.5× the rate with
/// tens of thousands of phase steps.
const OUTPUT_RATE_GAP_BROKEN_SPS: (f64, f64) = (0.35e6, 0.9e6);

/// 3.2 Msps (0x900000, f_symbol 12.8 MHz) streamed at the rate in 6 runs
/// of 8 and at half the rate in 2, with no error either way (findings
/// 10.2a). A rate that is silently half of what was asked is worse than
/// a refusal, so the driver refuses it and everything above 2.88 Msps.
pub const OUTPUT_RATE_UNRELIABLE_SPS: f64 = 3.2e6;

/// f_symbol at f_xtal / 2 = 14.4 MHz and above is broken: 0x800000 gave
/// every byte 0x80, 0x708000 (16.384 MHz) bytes of the wrong rate and
/// spread (findings 10.2a).
pub const F_SYMBOL_BROKEN_HZ: f64 = XTAL_HZ / 2.0;

/// The rate the receiver opens at: 2.048 Msps, the ratio of the observed
/// sequence (0xE10000) and the most-measured on the bench (findings 9b,
/// 10.1, 10.2a: 7 runs; every tone, minimisation and restart test).
pub const DEFAULT_OUTPUT_RATE_SPS: f64 = 2.048e6;

/// The output rate a ratio gives: f_symbol / 4.
pub fn output_rate_of_ratio(ratio: u32) -> f64 {
    rate_of_ratio(ratio) / OUTPUT_DECIMATION
}

fn unsupported(what: String) -> Error {
    Error::Unsupported {
        backend: BACKEND,
        what,
    }
}

/// Bench only: a lower bound in place of [`OUTPUT_RATE_GOOD_SPS`]'s, to
/// measure rates under it the way the span's rates were measured. Its f64
/// bits; 0 = none.
#[cfg(feature = "bench")]
static BENCH_RATE_FLOOR_SPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Bench only: accept output rates down to `sps` (None: the measured span
/// again). The upper bound and the f_symbol limit stay.
#[cfg(feature = "bench")]
pub fn set_bench_rate_floor(sps: Option<f64>) {
    BENCH_RATE_FLOOR_SPS.store(
        sps.map_or(0, f64::to_bits),
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// The lowest rate accepted: [`OUTPUT_RATE_GOOD_SPS`]'s (or the bench's).
fn rate_floor() -> f64 {
    #[cfg(feature = "bench")]
    {
        let b = BENCH_RATE_FLOOR_SPS.load(core::sync::atomic::Ordering::Relaxed);
        if b != 0 {
            return f64::from_bits(b);
        }
    }
    OUTPUT_RATE_GOOD_SPS.0
}

/// The ratio for an output rate: rsamp_ratio(4 · rate). Only the
/// measured-good spans are accepted ([`OUTPUT_RATE_GOOD_SPS`],
/// [`OUTPUT_RATE_LOW_SPS`]); the reason names the record for everything
/// else.
pub fn ratio_for_output_rate(sps: f64) -> Result<u32> {
    let (lo, hi) = (rate_floor(), OUTPUT_RATE_GOOD_SPS.1);
    let low = OUTPUT_RATE_LOW_SPS;
    if sps.is_finite() && sps >= low.0 && sps <= low.1 {
        return Ok(rsamp_ratio(sps * OUTPUT_DECIMATION));
    }
    if !sps.is_finite() || sps <= 0.0 {
        return Err(unsupported(format!("rate {sps} sps is not a rate")));
    }
    let f_symbol = sps * OUTPUT_DECIMATION;
    if f_symbol >= F_SYMBOL_BROKEN_HZ {
        return Err(unsupported(format!(
            "rate {:.6} Msps needs f_symbol {:.3} MHz; at f_xtal / 2 = {:.1} MHz and above the output is dead or broken (findings 10.2a)",
            sps / 1e6,
            f_symbol / 1e6,
            F_SYMBOL_BROKEN_HZ / 1e6
        )));
    }
    if sps > hi {
        return Err(unsupported(format!(
            "rate {:.6} Msps is above the {:.3} Msps every fresh run delivered; {:.1} Msps gave half the rate, silently, in 2 runs of 8 (findings 10.2a)",
            sps / 1e6,
            hi / 1e6,
            OUTPUT_RATE_UNRELIABLE_SPS / 1e6
        )));
    }
    if sps < lo {
        let (gap_lo, gap_hi) = OUTPUT_RATE_GAP_BROKEN_SPS;
        return Err(unsupported(format!(
            "rate {:.6} Msps is outside the measured-good spans {:.3}-{:.3} and {:.3}-{:.3} Msps; {:.2}-{:.2} Msps delivered 0.33-4.5× the rate with phase steps, 0.2 and 0.225 Msps 9× and half (findings 25)",
            sps / 1e6,
            low.0 / 1e6,
            low.1 / 1e6,
            lo / 1e6,
            hi / 1e6,
            gap_lo / 1e6,
            gap_hi / 1e6
        )));
    }
    Ok(rsamp_ratio(f_symbol))
}

/// Every output rate the driver hands out: the chip's own two spans, and
/// between them decimation of a rate inside one ([`plan_for_output_rate`],
/// the `decim` module).
pub const OUTPUT_RATE_SPAN_SPS: (f64, f64) = (OUTPUT_RATE_LOW_SPS.0, OUTPUT_RATE_GOOD_SPS.1);
/// The largest decimation used: every rate from 0.24 to
/// [`OVERSAMPLE_UP_TO_SPS`] (1.2 Msps) is a whole 2, 3 or 4 times under
/// one in [`OUTPUT_RATE_GOOD_SPS`] (×2 covers 0.4525-1.2, ×3
/// 0.30167-0.4525, ×4 0.24-0.30167).
pub const MAX_DECIMATION: u32 = 4;

/// How an output rate is made: the chip resampling at `ratio`, every
/// `decimation`-th sample of the `decim` low-pass handed out (1: every
/// sample as the chip sends it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RatePlan {
    pub ratio: u32,
    pub decimation: u32,
}

impl RatePlan {
    /// The rate the chip delivers.
    pub fn chip_rate(&self) -> f64 {
        output_rate_of_ratio(self.ratio)
    }

    /// The rate handed out.
    pub fn output_rate(&self) -> f64 {
        self.chip_rate() / self.decimation as f64
    }

    /// The plan for `sps` with the crystal running `scale` times its
    /// nominal 28.8 MHz: the same decimation, the ratio for the chip rate
    /// sps × decimation / scale, so the chip delivers sps × decimation in
    /// true time. `scale` 1 gives the plan back.
    pub fn corrected(self, sps: f64, scale: f64) -> RatePlan {
        if scale == 1.0 {
            return self;
        }
        RatePlan {
            ratio: rsamp_ratio(sps * self.decimation as f64 * OUTPUT_DECIMATION / scale),
            ..self
        }
    }
}

/// The fastest chip rate shown lossless with another dongle streaming on
/// the same bus (findings 27: 2.4 Msps and under clean in every pairing
/// tried, alike or not; above it the faster chip shed uncounted blocks —
/// in bursts beside a stream at its own rate, ~1,400 a minute beside one
/// at another). Native rates above it stay accepted, with that loss
/// reported (`clockwatch`); the decimation plan keeps the chip at it or
/// under.
pub const CHIP_RATE_SHARED_BUS_SAFE_SPS: f64 = 2.4e6;

/// Output rates up to this one are made by running the chip at twice
/// the rate or more and decimating, never by the chip alone: native
/// rates fold what lies just outside the window onto its edges, 17-20 dB
/// under at 0.42 of the rate and 10-12 at 0.45 (findings 26, 1.5-2.4
/// Msps), where a decimated rate's edge reads 54-70 dB out to 0.42. Half
/// of [`CHIP_RATE_SHARED_BUS_SAFE_SPS`], so the chip never runs above it
/// for a decimated rate: 1.3 and 1.44 Msps made at ÷2 (chip 2.6, 2.88)
/// shed blocks beside another dongle (findings 27), so from 1.2 to 1.44
/// Msps the chip runs at the rate itself (2026-10-18).
pub const OVERSAMPLE_UP_TO_SPS: f64 = CHIP_RATE_SHARED_BUS_SAFE_SPS / 2.0;

/// The plan for an output rate inside [`OUTPUT_RATE_SPAN_SPS`]: up to
/// [`OVERSAMPLE_UP_TO_SPS`], the smallest decimation 2-[`MAX_DECIMATION`]
/// whose chip rate lies in [`OUTPUT_RATE_GOOD_SPS`]; above it, the chip's
/// own rate. Anything else gets [`ratio_for_output_rate`]'s refusal. With
/// the bench's rate floor set (`set_bench_rate_floor`) a rate the chip
/// reaches itself is run at directly, as the bench measured them.
pub fn plan_for_output_rate(sps: f64) -> Result<RatePlan> {
    if bench_runs_natively()
        && let Ok(ratio) = ratio_for_output_rate(sps)
    {
        return Ok(RatePlan {
            ratio,
            decimation: 1,
        });
    }
    let inside = sps.is_finite() && sps >= OUTPUT_RATE_SPAN_SPS.0 && sps <= OUTPUT_RATE_SPAN_SPS.1;
    if inside && sps > OVERSAMPLE_UP_TO_SPS {
        return Ok(RatePlan {
            ratio: ratio_for_output_rate(sps)?,
            decimation: 1,
        });
    }
    if inside {
        let good = OUTPUT_RATE_GOOD_SPS;
        for k in 2..=MAX_DECIMATION {
            let chip = sps * k as f64;
            if chip >= good.0 && chip <= good.1 {
                return Ok(RatePlan {
                    ratio: ratio_for_output_rate(chip)?,
                    decimation: k,
                });
            }
        }
    }
    ratio_for_output_rate(sps).map(|ratio| RatePlan {
        ratio,
        decimation: 1,
    })
}

/// The clean part of a decimated rate's window, either side of the
/// centre, as a fraction of the rate: the decimator's pass edge (`decim::
/// PASS_EDGE`, flat within 0.1 dB). Out to it, what lies just outside the
/// window (one rate away) reads 54-70 dB down; at 0.45 it is 28-29 dB,
/// the decimator's transition (findings 26).
///
/// What a decimated rate does not remove is the chip's own fold, made
/// before the decimator: a signal two output rates from the centre (one
/// chip rate) lands inside the window as the chip folds it, 28-41 dB
/// down at ÷2 (findings 26). That is the window's interior, the centre
/// included — no edge margin moves it.
pub const DECIMATED_CLEAN_FRACTION: f64 = 0.42;

/// The clean part of a native rate's window, either side of the centre,
/// as a fraction of the rate (findings 26, every 0.01 of the rate from
/// 0.20 to 0.50 at 1.5, 1.8, 2.048, 2.4 and 2.88 Msps, both dongles,
/// signals one and two rates out on either side): out to 0.38 the worst
/// fold reads 30-38 dB at 0.38 and nowhere less than the window's
/// interior (27-36 dB at 0.30-0.34 to 2.4 Msps, 35-44 at 2.88); past it
/// the edge falls away on both dongles at every rate: 26-29 dB at 0.39,
/// 22-25 at 0.40, 16-20 at 0.42, 10-13 at 0.45.
pub const NATIVE_CLEAN_FRACTION: f64 = 0.38;

/// How far either side of the centre a channel is clean at an output
/// rate the driver hands out: as far as the window's edge folds no worse
/// than its interior. [`DECIMATED_CLEAN_FRACTION`] of the rate up to
/// [`OVERSAMPLE_UP_TO_SPS`], [`NATIVE_CLEAN_FRACTION`] above. Outside
/// [`OUTPUT_RATE_SPAN_SPS`], or for a rate the chip is made to run at
/// natively on the bench, the native fraction.
pub fn clean_half_span_hz(sps: f64) -> f64 {
    match plan_for_output_rate(sps) {
        Ok(p) if p.decimation > 1 => DECIMATED_CLEAN_FRACTION * sps,
        _ => NATIVE_CLEAN_FRACTION * sps,
    }
}

/// Whether the bench's rate floor is set: rates the chip reaches are then
/// run at directly.
fn bench_runs_natively() -> bool {
    #[cfg(feature = "bench")]
    {
        BENCH_RATE_FLOOR_SPS.load(core::sync::atomic::Ordering::Relaxed) != 0
    }
    #[cfg(not(feature = "bench"))]
    {
        false
    }
}

/// The PLL locking time, 5 ms max ([R820T] Table 1-2); the bench waited
/// twice that after every PLL or image write.
pub const PLL_SETTLE: Duration = Duration::from_millis(10);
/// The settle the bench gave the tuner's power pin.
pub const TUNER_POWER_SETTLE: Duration = Duration::from_millis(50);

/// The demodulator brought up for the tuner and the stream, every write
/// read back: clocks and ADCs, tuner power, the DDC, the AGC loops, and
/// the stream quiet.
///
/// Not written, by choice: the documented input-mode bits (AD_EN page 0
/// 0x08, en_bbin page 1 0xB1, 9.1). The vendor driver never writes them
/// (findings 9c, 9g) and every bench run that streamed had them at their
/// power-on values (findings 10), so the driver leaves them there. The
/// I2C repeater is not left open either: each tuner access opens and
/// closes it (`usb::Link::with_tuner`).
#[cfg(feature = "std")]
pub async fn setup(link: &Link) -> Result<()> {
    // Table 15: the PLL (what tuner access needs, findings step 2), both
    // ADCs (without them the stream is all 0x80, findings 10.4), the
    // reset released — 0xE8 from the power-on 0x20.
    link.sys_rmw8(DEMOD_CTL, 0, DEMOD_CTL_STREAM).await?;
    // Tables 16-19 (p. 29): GPIO3 as an output driven 0 = tuner power on.
    // GPO's read-back is shown (findings step 3's block scan: every
    // value reads back); GPD's and GPOE's are not, so those two are read
    // back without being judged.
    link.sys_rmw8_unjudged(GPD, GPIO_TUNER_POWER, 0).await?;
    link.sys_rmw8_unjudged(GPOE, 0, GPIO_TUNER_POWER).await?;
    link.sys_rmw8(GPO, GPIO_TUNER_POWER, 0).await?;
    sdr_core::time::sleep(TUNER_POWER_SETTLE).await;
    // 9.3: the DDC at the IF, the spectrum inverted for the high-side LO.
    // The stream's sequence writes both again in its own form.
    set_ddc(link, if_hz(), true).await?;
    // No automatic gain anywhere by default: the receiver's gain is the
    // codes the caller set, and nothing rescales the output by what the
    // window holds (findings 21). Table 2: both analog loops disabled and
    // not held (their resets enable them); Table 10: the digital AGC
    // disabled (its reset enables it); Table 2: the pins' polarity 0
    // (findings step 4b: polarity 0 closes the IF loop through the VAGC
    // pin, 1 is positive feedback), for `Rtl::set_agc`, which alone turns
    // the IF loop on.
    link.demod_rmw8(AAGC_PAGE, AAGC_OFF, EN_IF_AGC | EN_RF_AGC | AAGC_HOLD, 0)
        .await?;
    link.demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, 0).await?;
    link.demod_rmw8(
        AGC_POLARITY_PAGE,
        AGC_POLARITY_OFF,
        POLAR_RF_AGC | POLAR_IF_AGC,
        0,
    )
    .await?;
    // A stream an earlier process left running or paused is stopped and
    // unpaused: both bits to their power-on 0 (findings 9b: page 0 0x06
    // power-on 0x00; step 2: page 1 0x01 power-on 0x10).
    link.demod_rmw8(START_PAGE, START_OFF, START_BIT, 0).await?;
    link.demod_rmw8(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, 0).await?;
    Ok(())
}

/// pset_iffreq and Spec_inv (Table 6, 9.3), verified.
#[cfg(feature = "std")]
pub async fn set_ddc(link: &Link, if_hz: f64, spec_inv: bool) -> Result<()> {
    let b = pset_iffreq_bytes(if_hz);
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB, IFFREQ_MSB_MASK, b[0])
        .await?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])
        .await?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])
        .await?;
    link.demod_rmw8(
        SPEC_INV_PAGE,
        SPEC_INV_OFF,
        SPEC_INV,
        if spec_inv { SPEC_INV } else { 0 },
    )
    .await?;
    Ok(())
}

/// rsamp_ratio in the stream sequence's form ([`rsamp_transfers`]), each
/// transfer read back.
#[cfg(feature = "std")]
pub async fn write_rsamp(link: &Link, ratio: u32) -> Result<()> {
    for (reg, pair) in rsamp_transfers(ratio) {
        link.demod_write_verified(RSAMP_PAGE, reg, &pair).await?;
    }
    Ok(())
}

/// The f_symbol the resampler register holds (one auto-incrementing
/// read of the four bytes, findings step 1D).
#[cfg(feature = "std")]
pub async fn read_f_symbol(link: &Link) -> Result<f64> {
    let b = link.demod_read(RSAMP_PAGE, RSAMP_MSB, 4).await?;
    let ratio = rsamp_from_bytes([b[0], b[1], b[2], b[3]]);
    if ratio == 0 {
        return Err(Error::Device {
            backend: BACKEND,
            context: "resampler read",
            message: "rsamp_ratio reads 0".into(),
        });
    }
    Ok(rate_of_ratio(ratio))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [RTL2832U] 9.3's example and the bytes the bench wrote.
    #[test]
    fn pset_iffreq_matches_the_documents_example() {
        assert_eq!(pset_iffreq(4.57e6), -665554);
        assert_eq!(pset_iffreq_bytes(4.57e6), [0x35, 0xD8, 0x2E]);
        assert_eq!(pset_iffreq(0.0), 0);
        assert_eq!(pset_iffreq_bytes(0.0), [0, 0, 0]);
        // The demodulator's power-on value 0x35D75D = -665763 (findings
        // step 1D) is an IF of 665763 / 2^22 · 28.8 MHz = 4.571428 MHz,
        // 32/7 MHz to within one count of the field (the formula's floor
        // gives 665762 for exactly 32/7 MHz).
        let reset_if_hz = 665763.0 / TWO_POW_22 * XTAL_HZ;
        assert!((reset_if_hz - 32.0e6 / 7.0).abs() < 10.0, "{reset_if_hz}");
        assert_eq!(pset_iffreq_bytes(32.0e6 / 7.0), [0x35, 0xD7, 0x5E]);
    }

    /// Findings 9b item 10: the stream's IF is pset_iffreq 0x381112 =
    /// −519918, 3.570 MHz; the DDC removes 3,569,993.6 Hz.
    #[test]
    fn the_stream_if_is_the_benched_word() {
        assert_eq!(pset_iffreq(DDC_IF_HZ), -519918);
        assert_eq!(pset_iffreq_bytes(DDC_IF_HZ), [0x38, 0x11, 0x12]);
        assert!((ddc_if_hz() - 519918.0 / TWO_POW_22 * XTAL_HZ).abs() < 1e-6);
        assert!((ddc_if_hz() - 3_569_993.6).abs() < 0.1, "{}", ddc_if_hz());
        assert!((iffreq_hz_of(-665554) - 4.57e6).abs() < 7.0);
    }

    /// Table 7's three examples, 2.4 Msps (0x3000000, step 3) and
    /// 2.048 Msps (0x3840000) as rates of the resampler itself.
    #[test]
    fn rsamp_ratio_matches_table_7_and_the_bench() {
        assert_eq!(rsamp_ratio(64.0e6 / 7.0), 0xC99999);
        assert_eq!(rsamp_ratio(8.0e6), 0xE66666);
        assert_eq!(rsamp_ratio(48.0e6 / 7.0), 0x10CCCCC);
        assert_eq!(rsamp_ratio(2.4e6), 0x3000000);
        assert_eq!(rsamp_ratio(2.048e6), 0x3840000);
    }

    /// Findings 9b item 12: 0xE10000 is 03 84 00 00 over 0x9F-0xA2; the
    /// power-on F3 26 66 64 decodes to 0xC99999 (step 1D).
    #[test]
    fn rsamp_bytes_pack_into_bits_27_2() {
        assert_eq!(rsamp_bytes(0xE10000), [0x03, 0x84, 0x00, 0x00]);
        assert_eq!(
            rsamp_transfers(0xE10000),
            [(0x9F, [0x03, 0x84]), (0xA1, [0x00, 0x00])]
        );
        assert_eq!(rsamp_bytes(0x3000000), [0x0C, 0x00, 0x00, 0x00]);
        assert_eq!(rsamp_from_bytes([0xF3, 0x26, 0x66, 0x64]), 0xC99999);
        assert_eq!(
            rsamp_from_bytes([0xF4, 0x00, 0x00, 0x00]),
            0x1000000,
            "the high nibble is cfreq_off_ratio's"
        );
        // The field 0x3000000 has the top bit: the chip's ratio is
        // 0x7000000 (findings 25).
        assert_eq!(rsamp_from_bytes([0xFC, 0x00, 0x00, 0x00]), 0x7000000);
        // Every ratio the chip reaches: under 2^25, and 3 · 2^25 to 2^27
        // (the low span's, written as ratio − 2^26).
        for r in [
            0x1C20000u32,
            0xE10000,
            0xC00000,
            0xA00000,
            RSAMP_TOP_BIT - 1,
            0x6000000,
            0x7800000,
            2 * RSAMP_TOP_WEIGHT - 1,
        ] {
            assert_eq!(rsamp_from_bytes(rsamp_bytes(r)), r);
            assert_eq!(
                rsamp_bytes(r)[0] & !RSAMP_MASKS[0],
                0,
                "cfreq_off_ratio's nibble written 0"
            );
        }
    }

    /// Findings 10.2a's table: each measured ratio and its output rate.
    #[test]
    fn the_output_rate_is_f_symbol_over_4() {
        let measured = [
            (1.024e6, 0x1C20000u32, 4.096e6),
            (2.048e6, 0xE10000, 8.192e6),
            (2.4e6, 0xC00000, 9.6e6),
            (2.88e6, 0xA00000, 11.52e6),
        ];
        for (sps, ratio, f_symbol) in measured {
            assert_eq!(ratio_for_output_rate(sps).unwrap(), ratio, "{sps}");
            assert!((rate_of_ratio(ratio) - f_symbol).abs() < 1e-6);
            assert!((output_rate_of_ratio(ratio) - sps).abs() < 1e-6);
            // The bytes per second the bench predicted: 2 × f_symbol / 4.
            assert!((2.0 * output_rate_of_ratio(ratio) - 2.0 * f_symbol / 4.0).abs() < 1e-6);
        }
        assert_eq!(
            ratio_for_output_rate(DEFAULT_OUTPUT_RATE_SPS).unwrap(),
            0xE10000
        );
        // Between the measured points the floor gives a rate at or just
        // above the request.
        for sps in [1.2e6, 1.5e6, 1.8e6, 2.0e6, 2.5e6, 2.7e6] {
            let got = output_rate_of_ratio(ratio_for_output_rate(sps).unwrap());
            assert!(got >= sps && got - sps < sps * 1e-6, "{sps} -> {got}");
        }
    }

    /// The refusals: 3.2 Msps (unreliable), between 2.88 and 3.2, the
    /// broken f_symbol ≥ 14.4 MHz, below 1.024 Msps, and non-rates.
    #[test]
    fn rates_outside_the_measured_span_are_refused() {
        let refused = |sps: f64| match ratio_for_output_rate(sps) {
            Err(Error::Unsupported { what, .. }) => what,
            other => panic!("{sps}: {other:?}"),
        };
        assert!(refused(OUTPUT_RATE_UNRELIABLE_SPS).contains("half the rate"));
        assert!(refused(3.0e6).contains("half the rate"));
        assert!(refused(2.880001e6).contains("half the rate"));
        assert!(
            refused(3.6e6).contains("dead or broken"),
            "f_symbol 14.4 MHz"
        );
        assert!(
            refused(4.096e6).contains("dead or broken"),
            "f_symbol 16.384 MHz"
        );
        // Findings 25: between and under the measured spans.
        assert!(refused(0.9e6).contains("measured-good spans"));
        assert!(refused(0.5e6).contains("measured-good spans"));
        assert!(refused(0.35e6).contains("measured-good spans"));
        assert!(refused(0.301e6).contains("measured-good spans"));
        assert!(refused(0.225e6).contains("measured-good spans"));
        assert!(refused(0.2e6).contains("measured-good spans"));
        refused(0.0);
        refused(-2.048e6);
        refused(f64::NAN);
        refused(f64::INFINITY);
        assert!(ratio_for_output_rate(OUTPUT_RATE_GOOD_SPS.0).is_ok());
        assert!(ratio_for_output_rate(OUTPUT_RATE_GOOD_SPS.1).is_ok());
        assert_eq!(F_SYMBOL_BROKEN_HZ, 14.4e6);
    }

    /// Findings 25: the rates measured good under 1.024 Msps are accepted
    /// at the ratio for their f_symbol and give the rate back; the plan
    /// makes them by decimation all the same (they are under
    /// `OVERSAMPLE_UP_TO_SPS`).
    #[test]
    fn the_measured_rates_under_1024_ksps_are_accepted() {
        for sps in [
            0.24e6, 0.25e6, 0.288e6, 0.30e6, 0.905e6, 0.91e6, 0.92e6, 0.93e6, 0.94e6, 0.95e6,
            0.96e6, 1.0e6,
        ] {
            let ratio = ratio_for_output_rate(sps).unwrap_or_else(|e| panic!("{sps}: {e}"));
            // Written and read back, the chip's ratio is the one asked for.
            assert_eq!(
                rsamp_from_bytes(rsamp_bytes(ratio)),
                ratio,
                "{sps}: ratio {ratio:#x}"
            );
            let got = output_rate_of_ratio(ratio);
            assert!(got >= sps && got - sps < sps * 1e-6, "{sps} -> {got}");
            assert!(plan_for_output_rate(sps).unwrap().decimation >= 2, "{sps}");
        }
        assert!(ratio_for_output_rate(OUTPUT_RATE_LOW_SPS.0).is_ok());
        assert!(ratio_for_output_rate(OUTPUT_RATE_LOW_SPS.1).is_ok());
    }

    /// Every rate from 0.24 to 2.88 Msps, in 1 kHz steps, has a plan: up
    /// to 1.2 Msps a decimation of 2-4 whose chip rate lies in the main
    /// measured span and never above 2.4 Msps (findings 27), above it the
    /// chip's own rate; the rate handed out is the one asked for (the
    /// ratio's floor puts it at or just above).
    #[test]
    fn every_rate_in_the_span_has_a_plan() {
        let (lo, hi) = OUTPUT_RATE_SPAN_SPS;
        let mut sps = lo;
        while sps <= hi {
            let p = plan_for_output_rate(sps).unwrap_or_else(|e| panic!("{sps}: {e}"));
            assert!((1..=MAX_DECIMATION).contains(&p.decimation), "{sps}: {p:?}");
            let chip = p.chip_rate();
            assert!(
                chip >= OUTPUT_RATE_GOOD_SPS.0 && chip <= OUTPUT_RATE_GOOD_SPS.1 * (1.0 + 1e-6),
                "{sps}: chip {chip}"
            );
            assert!(
                p.output_rate() >= sps && p.output_rate() - sps < sps * 1e-6,
                "{sps}: {}",
                p.output_rate()
            );
            assert_eq!(
                p.decimation > 1,
                sps <= OVERSAMPLE_UP_TO_SPS,
                "{sps}: {p:?}"
            );
            if p.decimation > 1 {
                assert!(
                    chip <= CHIP_RATE_SHARED_BUS_SAFE_SPS * (1.0 + 1e-6),
                    "{sps}: chip {chip}"
                );
            }
            sps += 1e3;
        }
        assert_eq!(
            plan_for_output_rate(1.2e6).unwrap(),
            RatePlan {
                ratio: 0xC00000,
                decimation: 2
            }
        );
        assert_eq!(plan_for_output_rate(1.201e6).unwrap().decimation, 1);
        assert_eq!(plan_for_output_rate(1.3e6).unwrap().decimation, 1);
        assert_eq!(plan_for_output_rate(1.44e6).unwrap().decimation, 1);
        assert_eq!(plan_for_output_rate(0.5e6).unwrap().decimation, 2);
        assert_eq!(plan_for_output_rate(0.35e6).unwrap().decimation, 3);
        assert_eq!(plan_for_output_rate(0.301e6).unwrap().decimation, 4);
        assert_eq!(plan_for_output_rate(0.24e6).unwrap().decimation, 4);
        assert_eq!(
            plan_for_output_rate(2.4e6).unwrap(),
            RatePlan {
                ratio: 0xC00000,
                decimation: 1
            }
        );
        assert!(plan_for_output_rate(0.2e6).is_err());
        assert!(plan_for_output_rate(3.0e6).is_err());
    }

    /// The clean half-span follows the plan: the decimator's pass edge up
    /// to 1.2 Msps, the native edge above it (findings 26, 27).
    #[test]
    fn the_clean_span_follows_the_plan() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(close(clean_half_span_hz(0.5e6), 210e3));
        assert!(close(clean_half_span_hz(1.2e6), 504e3));
        assert!(close(clean_half_span_hz(1.3e6), 494e3));
        assert!(close(clean_half_span_hz(1.44e6), 547.2e3));
        assert!(close(clean_half_span_hz(1.5e6), 570e3));
        assert!(close(clean_half_span_hz(2.4e6), 912e3));
        assert!(close(clean_half_span_hz(2.88e6), 1094.4e3));
        for sps in [0.24e6, 0.5e6, 1.0e6, 1.44e6, 1.5e6, 2.048e6, 2.4e6, 2.88e6] {
            assert!(clean_half_span_hz(sps) < sps / 2.0);
        }
    }

    /// A corrected plan keeps the decimation and delivers the rate in true
    /// time: the ratio's rate × scale is the chip rate asked for, to the
    /// ratio's resolution (one count, under 0.12 ppm at 2.88 Msps).
    #[test]
    fn a_corrected_plan_delivers_the_rate_in_true_time() {
        for sps in [0.24e6, 0.5e6, 1.2e6, 1.44e6, 2.048e6, 2.4e6, 2.88e6] {
            for ppm in [-100.0, -1.0, 0.97, 50.0] {
                let scale = 1.0 + ppm * 1e-6;
                let plain = plan_for_output_rate(sps).unwrap();
                let p = plain.corrected(sps, scale);
                assert_eq!(p.decimation, plain.decimation);
                let truth = p.output_rate() * scale;
                assert!(
                    ((truth - sps) / sps).abs() < 0.12e-6,
                    "{sps} at {ppm} ppm: {truth}"
                );
                assert_eq!(
                    rsamp_from_bytes(rsamp_bytes(p.ratio)),
                    p.ratio,
                    "{sps} at {ppm} ppm: reachable"
                );
            }
            assert_eq!(
                plan_for_output_rate(sps).unwrap().corrected(sps, 1.0),
                plan_for_output_rate(sps).unwrap()
            );
        }
    }

    /// Findings 25's rule against its record: the rate the chip delivered
    /// for a request is the field's ratio ([`ratio_of_field`]) over 4, the
    /// field being the 26 bits the request's ratio left in the register.
    /// Delivered / requested, as measured (both dongles alike).
    #[test]
    fn the_top_bit_rule_predicts_the_rates_delivered() {
        let measured = [
            (0.2e6, 9.0022),
            (0.24e6, 1.0003),
            (0.3e6, 1.0002),
            (0.35e6, 4.5011),
            (0.5e6, 0.4738),
            (0.6e6, 0.4287),
            (0.7e6, 0.3914),
            (0.8e6, 0.3601),
            (0.9e6, 0.3334),
            (0.905e6, 1.0003),
            (1.024e6, 1.0003),
        ];
        for (sps, delivered) in measured {
            let field = rsamp_ratio(sps * OUTPUT_DECIMATION) & RSAMP_MAX;
            let predicted = output_rate_of_ratio(ratio_of_field(field)) / sps;
            assert!(
                (predicted - delivered).abs() < delivered * 1e-3,
                "{sps}: predicted {predicted:.4}, measured {delivered}"
            );
        }
    }
}
