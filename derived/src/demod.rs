//! The demodulator's documented settings: the low-IF input mode, the
//! DDC at the tuner's IF with the spectrum inverted for the high-side
//! LO, the resampler, and the analog AGC loops — [RTL2832U] 9.1, 9.3,
//! 9.4 and Tables 2, 4, 6, 7, 10 — in the order and with the read-backs
//! the bench applied them (`examples/harness/tuner.rs` `setup_demod`,
//! findings step 4). The raw-sample output mode is NOT here: the
//! document has none and the bench has not found it (findings step 3).

use std::time::Duration;

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::protocol::*;
use crate::tables::band::IF_HZ;
use crate::tables::pll::XTAL_HZ;
use crate::usb::Link;

/// pset_iffreq = −floor(f_IF / f_xtal · 2^22) ([RTL2832U] 9.3, p. 19;
/// the document's example: 4.57 MHz → −665554, zero IF → 0).
pub fn pset_iffreq(if_hz: f64) -> i32 {
    -((if_hz / XTAL_HZ * TWO_POW_22).floor() as i32)
}

/// The 22-bit two's complement over {0x19, 0x1A, 0x1B}, MSB first
/// (Table 6; 0x19 holds bits 21:16).
pub fn pset_iffreq_bytes(if_hz: f64) -> [u8; 3] {
    let u = (pset_iffreq(if_hz) as u32) & ((1 << IFFREQ_BITS) - 1);
    [(u >> 16) as u8, (u >> 8) as u8, u as u8]
}

/// The resampler field's largest value: 26 bits (Table 7, [27:2]).
pub const RSAMP_MAX: u32 = (1 << RSAMP_BITS) - 1;

/// rsamp_ratio = floor(f_xtal / f_s · 2^22) ([RTL2832U] 9.4, Table 7,
/// p. 20; the document's examples at 28.8 MHz: 64/7 MHz → 0xC99999,
/// 8 MHz → 0xE66666, 48/7 MHz → 0x10CCCCC).
pub fn rsamp_ratio(rate_hz: f64) -> u32 {
    (XTAL_HZ / rate_hz * TWO_POW_22).floor() as u32
}

/// The rate a ratio gives back: f_xtal · 2^22 / ratio.
pub fn rate_of_ratio(ratio: u32) -> f64 {
    XTAL_HZ * TWO_POW_22 / ratio as f64
}

/// The rates the register can express: the 26-bit field bounds the low
/// end (ratio ≤ 2^26 − 1 → f_s ≥ 1.8 MHz), and the ADC clock, 28.8 MHz
/// from the internal PLL ([RTL2832U] p. 10), bounds the high end (a
/// ratio below 2^22 would ask the resampler for more samples than the
/// ADC takes). This is the register's reach; what the USB pipe sustains
/// is not measured here (no bytes have streamed, findings step 3).
pub fn rate_range_hz() -> (f64, f64) {
    (rate_of_ratio(RSAMP_MAX), XTAL_HZ)
}

/// The four bytes rsamp_ratio occupies, MSB first, with the ratio in
/// [27:2] and the other bits (cfreq_off_ratio's low nibble in 0x9F,
/// bits 1:0 of 0xA2) left to the masks.
pub fn rsamp_bytes(ratio: u32) -> [u8; 4] {
    ((ratio & RSAMP_MAX) << RSAMP_SHIFT).to_be_bytes()
}

/// The ratio back from the four bytes read at 0x9F.
pub fn rsamp_from_bytes(b: [u8; 4]) -> u32 {
    (u32::from_be_bytes(b) >> RSAMP_SHIFT) & RSAMP_MAX
}

/// The PLL locking time, 5 ms max ([R820T] Table 1-2); the bench waited
/// twice that after every PLL or image write.
pub const PLL_SETTLE: Duration = Duration::from_millis(10);
/// The settle the bench gave the tuner's power pin.
pub const TUNER_POWER_SETTLE: Duration = Duration::from_millis(50);

fn unsupported(what: String) -> Error {
    Error::Unsupported { backend: BACKEND, what }
}

/// The documented demodulator state for a tuner measurement, every write
/// read back (the order the bench used): clocks, tuner power, the I2C
/// repeater, the low-IF input, the DDC, the AGC loops.
pub fn setup(link: &Link) -> Result<()> {
    // Table 15: the PLL (what tuner access needs, findings step 2), both
    // ADCs, the reset released.
    link.sys_rmw8(DEMOD_CTL, 0, DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE)?;
    // Tables 16-19 (p. 29): GPIO3 as an output driven 0 = tuner power on.
    // GPO's read-back is shown (findings step 3's block scan: every
    // value reads back); GPD's and GPOE's are not, so those two are read
    // back without being judged.
    link.sys_rmw8_unjudged(GPD, GPIO_TUNER_POWER, 0)?;
    link.sys_rmw8_unjudged(GPOE, 0, GPIO_TUNER_POWER)?;
    link.sys_rmw8(GPO, GPIO_TUNER_POWER, 0)?;
    std::thread::sleep(TUNER_POWER_SETTLE);
    // Table 3: IIC_repeat, the other thing tuner access needs (step 2).
    link.demod_rmw8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT, IIC_REPEAT_BIT)?;
    // 9.1, low-IF input: AD_EN_reg1 = 1, AD_EN_reg = 0, en_bbin = 0 — the
    // tuner's output is one differential IF ([R820T] Figure D, 4.3).
    link.demod_rmw8(AD_EN_PAGE, AD_EN_OFF, AD_EN_REG | AD_EN_REG1, AD_EN_REG1)?;
    link.demod_rmw8(BBIN_PAGE, BBIN_OFF, EN_BBIN, 0)?;
    // 9.3: the DDC at the tuner's IF, the spectrum inverted for the
    // high-side LO (findings step 4h: the wanted sideband is below the LO).
    set_ddc(link, IF_HZ, true)?;
    // Table 2: both analog loops enabled and not held (their resets);
    // Table 10: the digital AGC enabled (its reset); Table 2: the pins'
    // polarity 0 (findings step 4b: polarity 0 closes the IF loop through
    // the VAGC pin, 1 is positive feedback).
    link.demod_rmw8(AAGC_PAGE, AAGC_OFF, EN_IF_AGC | EN_RF_AGC | AAGC_HOLD, EN_IF_AGC | EN_RF_AGC)?;
    link.demod_rmw8(DAGC_PAGE, DAGC_OFF, EN_DAGC, EN_DAGC)?;
    link.demod_rmw8(AGC_POLARITY_PAGE, AGC_POLARITY_OFF, POLAR_RF_AGC | POLAR_IF_AGC, 0)?;
    Ok(())
}

/// pset_iffreq and Spec_inv (Table 6, 9.3), verified.
pub fn set_ddc(link: &Link, if_hz: f64, spec_inv: bool) -> Result<()> {
    let b = pset_iffreq_bytes(if_hz);
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB, IFFREQ_MSB_MASK, b[0])?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])?;
    link.demod_rmw8(IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])?;
    link.demod_rmw8(SPEC_INV_PAGE, SPEC_INV_OFF, SPEC_INV, if spec_inv { SPEC_INV } else { 0 })?;
    Ok(())
}

/// The resampler for a rate (Table 7), each byte verified under its mask.
pub fn set_rate(link: &Link, rate_hz: f64) -> Result<()> {
    let (lo, hi) = rate_range_hz();
    if !rate_hz.is_finite() || rate_hz < lo || rate_hz > hi {
        return Err(unsupported(format!(
            "rate {rate_hz} Hz is outside the resampler's reach {lo:.1}-{hi:.1} Hz (rsamp_ratio, [RTL2832U] Table 7: 26 bits; the ADC clock 28.8 MHz)"
        )));
    }
    let bytes = rsamp_bytes(rsamp_ratio(rate_hz));
    for (k, (mask, v)) in RSAMP_MASKS.iter().zip(bytes).enumerate() {
        link.demod_rmw8(RSAMP_PAGE, RSAMP_MSB + k as u8, *mask, v)?;
    }
    Ok(())
}

/// The rate the resampler register holds (one auto-incrementing read of
/// the four bytes, findings step 1D).
pub fn read_rate(link: &Link) -> Result<f64> {
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

    /// Table 7's three examples, the bench's 2.4 Msps (0x3000000, step 3)
    /// and 2.048 Msps (0x3840000).
    #[test]
    fn rsamp_ratio_matches_table_7_and_the_bench() {
        assert_eq!(rsamp_ratio(64.0e6 / 7.0), 0xC99999);
        assert_eq!(rsamp_ratio(8.0e6), 0xE66666);
        assert_eq!(rsamp_ratio(48.0e6 / 7.0), 0x10CCCCC);
        assert_eq!(rsamp_ratio(2.4e6), 0x3000000);
        assert_eq!(rsamp_ratio(2.048e6), 0x3840000);
    }

    /// The bytes over 0x9F..0xA2 for 2.4 Msps are `?C 00 00 00` with the
    /// upper nibble of 0x9F left to cfreq_off_ratio (the bench read back
    /// `FC 00 00 00`, findings step 3 baseline), and the reset 0xC99999
    /// packs as Tables 7/12 print (`F3 26 66 64`, step 1D).
    #[test]
    fn rsamp_bytes_pack_into_bits_27_2() {
        let b = rsamp_bytes(0x3000000);
        assert_eq!(b, [0x0C, 0x00, 0x00, 0x00]);
        assert_eq!(RSAMP_MASKS, [0x0F, 0xFF, 0xFF, 0xFC]);
        let merged = [0xF0 | (b[0] & RSAMP_MASKS[0]), b[1], b[2], b[3]];
        assert_eq!(merged, [0xFC, 0x00, 0x00, 0x00]);
        assert_eq!(rsamp_from_bytes(merged), 0x3000000);
        let reset = rsamp_bytes(0xC99999);
        assert_eq!([0xF0 | reset[0], reset[1], reset[2], reset[3]], [0xF3, 0x26, 0x66, 0x64]);
        assert_eq!(rsamp_from_bytes([0xF3, 0x26, 0x66, 0x64]), 0xC99999);
    }

    #[test]
    fn rate_range_is_the_fields_reach() {
        let (lo, hi) = rate_range_hz();
        assert!((lo - 1.8e6).abs() < 1.0, "{lo}");
        assert_eq!(hi, XTAL_HZ);
        assert!(rsamp_ratio(lo) <= RSAMP_MAX);
        assert_eq!(rsamp_ratio(hi), 1 << 22);
        // The rate read back from a ratio is the ratio's own rate.
        for r in [2.4e6, 2.048e6, 1.2e6 * 2.0, 8.0e6, 64.0e6 / 7.0] {
            let back = rate_of_ratio(rsamp_ratio(r));
            assert!(back >= r && back - r < r * 1e-6, "{r} -> {back}");
        }
    }
}
