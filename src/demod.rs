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

/// The IF the DDC actually removes: [`DDC_IF_HZ`] through the field's
/// floor, 519918 / 2^22 · 28.8 MHz = 3,569,993.6 Hz. The tuner's LO is
/// placed this far above the wanted centre so the DDC's 0 Hz lands on it.
pub fn ddc_if_hz() -> f64 {
    iffreq_hz_of(pset_iffreq(DDC_IF_HZ))
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

/// The four bytes rsamp_ratio occupies, MSB first, the ratio in [27:2].
/// The other bits are 0: the upper nibble of 0x9F (cfreq_off_ratio's
/// [3:0], Table 12) and bits 1:0 of 0xA2 — as the vendor driver wrote
/// them (findings 9b item 12: 03 84 00 00) and the bench's sequence did
/// (`examples/harness/stream_confirm.rs` `rsamp_bytes`).
pub fn rsamp_bytes(ratio: u32) -> [u8; 4] {
    ((ratio & RSAMP_MAX) << RSAMP_SHIFT).to_be_bytes()
}

/// The ratio back from the four bytes read at 0x9F.
pub fn rsamp_from_bytes(b: [u8; 4]) -> u32 {
    (u32::from_be_bytes(b) >> RSAMP_SHIFT) & RSAMP_MAX
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

/// The output rates every fresh run delivered: 1.024 Msps (0x1C20000)
/// to 2.88 Msps (0xA00000), with 2.048 (0xE10000) and 2.4 (0xC00000)
/// between (findings 10.2a). A request inside the span is accepted; the
/// points between the four measured ratios were not measured singly.
/// "Delivered" is the rate only: at 2.88 Msps the samples carry periodic
/// ~46° phase steps (findings 16.5; see `Rtl::set_rate`).
pub const OUTPUT_RATE_GOOD_SPS: (f64, f64) = (1.024e6, 2.88e6);

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
    Error::Unsupported { backend: BACKEND, what }
}

/// The ratio for an output rate: rsamp_ratio(4 · rate). Only the
/// measured-good span is accepted ([`OUTPUT_RATE_GOOD_SPS`]); the reason
/// names the record for everything else.
pub fn ratio_for_output_rate(sps: f64) -> Result<u32> {
    let (lo, hi) = OUTPUT_RATE_GOOD_SPS;
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
        return Err(unsupported(format!(
            "rate {:.6} Msps is below {:.3} Msps, the lowest the bench measured (findings 10.2a)",
            sps / 1e6,
            lo / 1e6
        )));
    }
    Ok(rsamp_ratio(f_symbol))
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
pub fn setup(link: &Link) -> Result<()> {
    // Table 15: the PLL (what tuner access needs, findings step 2), both
    // ADCs (without them the stream is all 0x80, findings 10.4), the
    // reset released — 0xE8 from the power-on 0x20.
    link.sys_rmw8(DEMOD_CTL, 0, DEMOD_CTL_STREAM)?;
    // Tables 16-19 (p. 29): GPIO3 as an output driven 0 = tuner power on.
    // GPO's read-back is shown (findings step 3's block scan: every
    // value reads back); GPD's and GPOE's are not, so those two are read
    // back without being judged.
    link.sys_rmw8_unjudged(GPD, GPIO_TUNER_POWER, 0)?;
    link.sys_rmw8_unjudged(GPOE, 0, GPIO_TUNER_POWER)?;
    link.sys_rmw8(GPO, GPIO_TUNER_POWER, 0)?;
    std::thread::sleep(TUNER_POWER_SETTLE);
    // 9.3: the DDC at the IF, the spectrum inverted for the high-side LO.
    // The stream's sequence writes both again in its own form.
    set_ddc(link, DDC_IF_HZ, true)?;
    // Table 2: both analog loops enabled and not held (their resets);
    // Table 10: the digital AGC enabled (its reset); Table 2: the pins'
    // polarity 0 (findings step 4b: polarity 0 closes the IF loop through
    // the VAGC pin, 1 is positive feedback).
    link.demod_rmw8(AAGC_PAGE, AAGC_OFF, EN_IF_AGC | EN_RF_AGC | AAGC_HOLD, EN_IF_AGC | EN_RF_AGC)?;
    link.demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, EN_DAGC)?;
    link.demod_rmw8(AGC_POLARITY_PAGE, AGC_POLARITY_OFF, POLAR_RF_AGC | POLAR_IF_AGC, 0)?;
    // A stream an earlier process left running or paused is stopped and
    // unpaused: both bits to their power-on 0 (findings 9b: page 0 0x06
    // power-on 0x00; step 2: page 1 0x01 power-on 0x10).
    link.demod_rmw8(START_PAGE, START_OFF, START_BIT, 0)?;
    link.demod_rmw8(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, 0)?;
    Ok(())
}

/// pset_iffreq and Spec_inv (Table 6, 9.3), verified.
#[cfg(feature = "std")]
pub fn set_ddc(link: &Link, if_hz: f64, spec_inv: bool) -> Result<()> {
    let b = pset_iffreq_bytes(if_hz);
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB, IFFREQ_MSB_MASK, b[0])?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])?;
    link.demod_rmw8(SPEC_INV_PAGE, SPEC_INV_OFF, SPEC_INV, if spec_inv { SPEC_INV } else { 0 })?;
    Ok(())
}

/// rsamp_ratio in the stream sequence's form ([`rsamp_transfers`]), each
/// transfer read back.
#[cfg(feature = "std")]
pub fn write_rsamp(link: &Link, ratio: u32) -> Result<()> {
    for (reg, pair) in rsamp_transfers(ratio) {
        link.demod_write_verified(RSAMP_PAGE, reg, &pair)?;
    }
    Ok(())
}

/// The f_symbol the resampler register holds (one auto-incrementing
/// read of the four bytes, findings step 1D).
#[cfg(feature = "std")]
pub fn read_f_symbol(link: &Link) -> Result<f64> {
    let b = link.demod_read(RSAMP_PAGE, RSAMP_MSB, 4)?;
    let ratio = rsamp_from_bytes([b[0], b[1], b[2], b[3]]);
    if ratio == 0 {
        return Err(Error::Device { backend: BACKEND, context: "resampler read", message: "rsamp_ratio reads 0".into() });
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
        assert_eq!(rsamp_transfers(0xE10000), [(0x9F, [0x03, 0x84]), (0xA1, [0x00, 0x00])]);
        assert_eq!(rsamp_bytes(0x3000000), [0x0C, 0x00, 0x00, 0x00]);
        assert_eq!(rsamp_from_bytes([0xF3, 0x26, 0x66, 0x64]), 0xC99999);
        assert_eq!(rsamp_from_bytes([0xFC, 0x00, 0x00, 0x00]), 0x3000000, "the high nibble is cfreq_off_ratio's");
        for r in [0x1C20000u32, 0xE10000, 0xC00000, 0xA00000, RSAMP_MAX] {
            assert_eq!(rsamp_from_bytes(rsamp_bytes(r)), r);
            assert_eq!(rsamp_bytes(r)[0] & !RSAMP_MASKS[0], 0, "cfreq_off_ratio's nibble written 0");
        }
    }

    /// Findings 10.2a's table: each measured ratio and its output rate.
    #[test]
    fn the_output_rate_is_f_symbol_over_4() {
        let measured = [(1.024e6, 0x1C20000u32, 4.096e6), (2.048e6, 0xE10000, 8.192e6), (2.4e6, 0xC00000, 9.6e6), (2.88e6, 0xA00000, 11.52e6)];
        for (sps, ratio, f_symbol) in measured {
            assert_eq!(ratio_for_output_rate(sps).unwrap(), ratio, "{sps}");
            assert!((rate_of_ratio(ratio) - f_symbol).abs() < 1e-6);
            assert!((output_rate_of_ratio(ratio) - sps).abs() < 1e-6);
            // The bytes per second the bench predicted: 2 × f_symbol / 4.
            assert!((2.0 * output_rate_of_ratio(ratio) - 2.0 * f_symbol / 4.0).abs() < 1e-6);
        }
        assert_eq!(ratio_for_output_rate(DEFAULT_OUTPUT_RATE_SPS).unwrap(), 0xE10000);
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
        assert!(refused(3.6e6).contains("dead or broken"), "f_symbol 14.4 MHz");
        assert!(refused(4.096e6).contains("dead or broken"), "f_symbol 16.384 MHz");
        assert!(refused(1.0e6).contains("lowest"));
        assert!(refused(0.25e6).contains("lowest"));
        refused(0.0);
        refused(-2.048e6);
        refused(f64::NAN);
        refused(f64::INFINITY);
        assert!(ratio_for_output_rate(OUTPUT_RATE_GOOD_SPS.0).is_ok());
        assert!(ratio_for_output_rate(OUTPUT_RATE_GOOD_SPS.1).is_ok());
        assert_eq!(F_SYMBOL_BROKEN_HZ, 14.4e6);
    }
}
