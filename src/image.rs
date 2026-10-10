//! The tuner's register image, R5-R31. Every cell [R820T2] Table 1-2
//! (p. 5) fixes is written as printed; every other field is a choice
//! from its Table 1-3 meaning (pp. 6-11), with the reason recorded per
//! register in `tables::tuner_image::TUNER_IMAGE` (the generated copy of
//! the bench's own image record) — the builder here reproduces that
//! image byte for byte (tested) and varies only the fields the driver
//! controls: the three gain stages and their modes, the detectors, the
//! IF filter, the band path, and the PLL words.
//!
//! The one bench-forced bit: R15 CLK_OUT_ENB stays 0. Writing 1 (clock
//! output off) stopped every USB request until a power cycle, because
//! the demodulator is clocked from the tuner's clock output on this
//! board (findings step 4a; [R820T] 6.3 "Share Crystal", [RTL2832U] 9.10
//! "oscillator output should be connected to the XI pin"). The request
//! layer refuses that write.
//!
//! Read-back: R5-R15 read back exactly as written; R16-R31 cannot be
//! read back, because a 32-byte read returns R0-R15 and then R0-R15
//! again (findings step 4a).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::pll::Pll;

/// The first register of the image: R0-R4 are read-only ([R820T2]
/// Table 1-2 marks them so; [R820T] p. 17 reserves them).
pub const IMAGE_FIRST_REG: u8 = 5;
/// Registers a read returns before it wraps (findings step 4a): only
/// R5-R15 can be verified by read-back.
pub const READBACK_REGS: u8 = 16;

// Register numbers used by name elsewhere.
pub const R5: u8 = 5;
pub const R6: u8 = 6;
pub const R7: u8 = 7;
pub const R10: u8 = 10;
pub const R11: u8 = 11;
pub const R12: u8 = 12;
pub const R15: u8 = 15;
pub const R16: u8 = 16;
pub const R20: u8 = 20;
pub const R21: u8 = 21;
pub const R22: u8 = 22;
pub const R26: u8 = 26;
pub const R27: u8 = 27;

// ---- Field positions, [R820T2] Table 1-2 (p. 5), B7 -> B0 ----

/// R5: PWD_LT [7], fixed 0 [6], PWD_LNA1 [5], LNA_GAIN_MODE [4], LNA_GAIN [3:0].
pub const PWD_LT: u8 = 1 << 7;
pub const PWD_LNA1: u8 = 1 << 5;
pub const LNA_GAIN_MODE: u8 = 1 << 4;
/// R6: PWD_PDET1 [7], PWD_PDET3 [6], FILT_3DB [5], fixed 1 0 [4:3], PW_LNA [2:0].
pub const PWD_PDET1: u8 = 1 << 7;
pub const PWD_PDET3: u8 = 1 << 6;
pub const FILT_3DB: u8 = 1 << 5;
pub const R6_FIXED: u8 = 0x10;
/// R7: fixed 0 [7], PWD_MIX [6], PW0_MIX [5], MIXGAIN_MODE [4], MIX_GAIN [3:0].
pub const PWD_MIX: u8 = 1 << 6;
pub const PW0_MIX: u8 = 1 << 5;
pub const MIXGAIN_MODE: u8 = 1 << 4;
/// R8: PWD_AMP [7], PW0_AMP [6], IMR_G [5:0]. R9: PWD_IFFILT [7],
/// PW1_IFFILT [6], IMR_P [5:0].
pub const PWD_AMP: u8 = 1 << 7;
pub const PW0_AMP: u8 = 1 << 6;
pub const PWD_IFFILT: u8 = 1 << 7;
pub const PW1_IFFILT: u8 = 1 << 6;
/// R10: PWD_FILT [7], PW_FILT [6:5], fixed 1 [4], FILT_CODE [3:0].
pub const PWD_FILT: u8 = 1 << 7;
pub const R10_FIXED: u8 = 0x10;
/// R11: fixed 0 [7], FILT_BW [6:5], fixed 0 [4], HPF [3:0].
pub const FILT_BW_SHIFT: u8 = 5;
/// R12: fixed 1 [7], PWD_VGA [6], fixed 1 [5], VGA_MODE [4], VGA_CODE [3:0].
pub const R12_FIXED: u8 = 0xA0;
pub const PWD_VGA: u8 = 1 << 6;
pub const VGA_MODE: u8 = 1 << 4;
/// R15: fixed 0 0 1 [7:5], CLK_OUT_ENB [4], fixed 1 0 [3:2], CLK_AGC_ENB [1], fixed 0 [0].
pub const R15_FIXED: u8 = 0x28;
pub const CLK_OUT_ENB: u8 = 1 << 4;
pub const CLK_AGC_ENB: u8 = 1 << 1;
/// R16: SEL_DIV [7:5], REF_DIV2 [4], fixed 0 1 [3:2], CAPX [1:0].
pub const SEL_DIV_SHIFT: u8 = 5;
pub const REF_DIV2_BIT: u8 = 1 << 4;
pub const R16_FIXED: u8 = 0x04;
/// R17: PW_LDO_A [7:6], fixed 0 0 0 0 1 1 [5:0].
pub const PW_LDO_A_SHIFT: u8 = 6;
pub const R17_FIXED: u8 = 0x03;
/// R18: fixed 1 [7], fixed 0 [6:4], PW_SDM [3], fixed 0 [2:0].
pub const R18_FIXED: u8 = 0x80;
pub const PW_SDM: u8 = 1 << 3;
/// R19: all cells fixed 0.
pub const R19_FIXED: u8 = 0x00;
/// R20: SI2C [7:6], NI2C [5:0]. R21: SDM_IN [8:1]. R22: SDM_IN [16:9].
pub const SI2C_SHIFT: u8 = 6;
pub const NI2C_MASK: u8 = 0x3F;
/// R23: PW_LDO_D [7:6], fixed 1 1 [5:4], OPEN_D [3], fixed 1 0 0 [2:0].
pub const PW_LDO_D_SHIFT: u8 = 6;
pub const R23_FIXED: u8 = 0x34;
pub const OPEN_D: u8 = 1 << 3;
/// R24: fixed 0 1 [7:6], the rest unspecified.
pub const R24_FIXED: u8 = 0x40;
/// R25: PWD_RFFILT [7], fixed 0 0 [6:5], SW_AGC [4], fixed 1 1 [3:2], unspecified [1:0].
pub const PWD_RFFILT: u8 = 1 << 7;
pub const SW_AGC: u8 = 1 << 4;
pub const R25_FIXED: u8 = 0x0C;
/// R26: RFMUX [7:6], fixed 1 0 [5:4], PLL_AUTO_CLK [3:2], RFFILT [1:0].
pub const RFMUX_SHIFT: u8 = 6;
pub const R26_FIXED: u8 = 0x20;
pub const PLL_AUTO_CLK_SHIFT: u8 = 2;
/// R27: TF_NCH [7:4], TF_LP [3:0].
pub const TF_NCH_SHIFT: u8 = 4;
/// R28: PDET3_GAIN [7:4], fixed 0 1 [3:2], unspecified [1], fixed 0 [0].
pub const PDET3_GAIN_SHIFT: u8 = 4;
pub const R28_FIXED: u8 = 0x04;
/// R29: fixed 1 1 [7:6], PDET1_GAIN [5:3], PDET2_GAIN [2:0].
pub const R29_FIXED: u8 = 0xC0;
pub const PDET1_GAIN_SHIFT: u8 = 3;
/// R30: fixed 0 [7], B6 fixed 1 by Table 1-2 (Table 1-3 reads it as
/// FILTER_EXT; the matrix's value is kept, the bench never tried 0),
/// PDET_CLK below.
pub const R30_B6: u8 = 1 << 6;
/// R31: fixed 1 1 0 0 0 0 [7:2], unspecified [1:0].
pub const R31_FIXED: u8 = 0xC0;

/// The fixed cells of [R820T2] Table 1-2 for R5-R31 as (register, mask,
/// value), R30's B7/B6 as the matrix prints them. Every image built here
/// satisfies all nineteen (tested).
pub const FIXED: [(u8, u8, u8); 19] = [
    (5, 0x40, 0x00),
    (6, 0x18, 0x10),
    (7, 0x80, 0x00),
    (10, 0x10, 0x10),
    (11, 0x90, 0x00),
    (12, 0xA0, 0xA0),
    (15, 0xED, 0x28),
    (16, 0x0C, 0x04),
    (17, 0x3F, 0x03),
    (18, 0xF7, 0x80),
    (19, 0xFF, 0x00),
    (23, 0x37, 0x34),
    (24, 0xC0, 0x40),
    (25, 0x6C, 0x0C),
    (26, 0x30, 0x20),
    (28, 0x0D, 0x04),
    (29, 0xC0, 0xC0),
    (30, 0xC0, 0x40),
    (31, 0xFC, 0xC0),
];

// ---- The choices that never change (reasons: TUNER_IMAGE's `why`) ----

/// IMR_G / IMR_P = 32, the middle of 0..=63: no documented value, and the
/// image-rejection calibration is an open search (findings step 4h:
/// 10-20 dB uncalibrated).
pub const IMR_MIDDLE: u8 = 32;
/// R13 / R14: LNA_VTH_H = MIX_VTH_H = 1010 (about 1.34 V), the low
/// thresholds 0101 (about 0.84 V) — a window symmetric about the middle
/// of the documented 0.34-1.94 V range (Table 1-3); used only in auto
/// gain.
pub const VTH_WINDOW: u8 = 0xA5;
/// PW_LDO_A = 10, 2.0 V: the middle of the three documented voltages.
pub const PW_LDO_A_2V0: u8 = 0b10;
/// PW_LDO_D = 00, 1.8 V at 8 mA: the lower documented voltage at the
/// higher current (11 is off).
pub const PW_LDO_D_1V8_8MA: u8 = 0b00;
/// PLL_AUTO_CLK = 00, 128 kHz: the fastest documented auto-tune clock.
pub const PLL_AUTO_CLK_128K: u8 = 0b00;
/// CAPX = 00: no internal crystal capacitance; the crystal's 16 pF load
/// is on the board ([R820T] Table 6-1).
pub const CAPX_NONE: u8 = 0b00;
/// PDET3_GAIN = 0111, the middle of 0 highest ..= 15 lowest.
pub const PDET3_GAIN_MIDDLE: u8 = 0b0111;
/// PDET1_GAIN = 011, the middle of the 3-bit field; PDET2_GAIN = 100,
/// the middle of 0 lowest ..= 7 highest.
pub const PDET1_GAIN_MIDDLE: u8 = 0b011;
pub const PDET2_GAIN_MIDDLE: u8 = 0b100;
/// PDET_CLK = 000000, the minimum in both the 5-bit and the 6-bit
/// reading of Table 1-2 / 1-3's contradiction.
pub const PDET_CLK_MIN: u8 = 0;
/// HPF = 1111, the lowest corner: no HPF code has a measurable effect
/// above 0.1 MHz (findings step 4g, `tables::if_filter::HPF_EFFECT`),
/// and the lowest corner passes the most of the IF band.
pub const HPF_LOWEST: u8 = 0b1111;

/// The fields the driver varies. Everything else in the image is a
/// fixed cell or a constant choice above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Front {
    /// R5 LNA_GAIN_MODE: 1 manual (LNA_GAIN used), 0 auto (Table 1-3).
    pub lna_manual: bool,
    /// R5 LNA_GAIN [3:0]: 15 max ... 0 min.
    pub lna_gain: u8,
    /// R7 MIXGAIN_MODE: 0 manual (MIX_GAIN used), 1 auto — the opposite
    /// polarity to the LNA's (Table 1-3).
    pub mix_manual: bool,
    /// R7 MIX_GAIN [3:0]: 0000 min ... 1111 max.
    pub mix_gain: u8,
    /// R12 VGA_MODE: 1 = gain from the VAGC pin, 0 = from VGA_CODE.
    pub vga_from_pin: bool,
    /// R12 VGA_CODE [3:0]: 0000 = -12.0 dB ... 1111 = +40.5 dB (Table 1-3;
    /// the measured steps are in `tables::gain::VGA_GAIN_DB`).
    pub vga_code: u8,
    /// R6 PWD_PDET1 (0 on / 1 off) and PWD_PDET3 (0 off / 1 on): the
    /// tuner's own power detectors, needed only in auto gain.
    pub pdet1_on: bool,
    pub pdet3_on: bool,
    /// R10 FILT_CODE [3:0]: 0000 widest ... 1111 narrowest.
    pub filt_code: u8,
    /// R11 FILT_BW [1:0]: 00 widest, 01/10 middle, 11 narrowest.
    pub filt_bw: u8,
    /// R26 RFMUX [1:0]: 00 tracking filter on, 01 bypass.
    pub rfmux: u8,
    /// R26 RFFILT [1:0]: 00 highest band, 01 middle, 10 low.
    pub rffilt: u8,
    /// R27 TF_NCH [3:0] (notch corner, 0000 highest) and TF_LP [3:0]
    /// (low-pass corner, 0000 highest).
    pub tf_nch: u8,
    pub tf_lp: u8,
}

impl Front {
    /// The bring-up front end every bench record was taken with: the
    /// three gain stages manual at code 8 (the middle of 0..=15), the
    /// detectors off, the IF filter at its widest codes, the tracking
    /// filter bypassed with band code 00 (the `BYPASS` path of
    /// `tables::band`, which every band reading is relative to).
    pub const BRING_UP: Front = Front {
        lna_manual: true,
        lna_gain: 8,
        mix_manual: true,
        mix_gain: 8,
        vga_from_pin: false,
        vga_code: 8,
        pdet1_on: false,
        pdet3_on: false,
        filt_code: 0,
        filt_bw: 0,
        rfmux: 0b01,
        rffilt: 0b00,
        tf_nch: 0,
        tf_lp: 0,
    };

    /// R5: loop-through off (PWD_LT = 1: the board has no loop-through
    /// output, and [R820T] Table 1-1 lists it as a standby current),
    /// LNA on (PWD_LNA1 = 0), the gain mode and code.
    pub fn r5(&self) -> u8 {
        PWD_LT | if self.lna_manual { LNA_GAIN_MODE } else { 0 } | (self.lna_gain & 0x0F)
    }

    /// R6: the detectors per their (opposite) polarities, FILT_3DB = 0
    /// (no extra filter gain), PW_LNA = 000 max current (the 3.5 dB noise
    /// figure is specified at max gain, [R820T] Table 1-2).
    pub fn r6(&self) -> u8 {
        (if self.pdet1_on { 0 } else { PWD_PDET1 }) | (if self.pdet3_on { PWD_PDET3 } else { 0 }) | R6_FIXED
    }

    /// R7: mixer on (PWD_MIX = 1), PW0_MIX = 1 "normal current", the gain
    /// mode (0 manual) and code.
    pub fn r7(&self) -> u8 {
        PWD_MIX | PW0_MIX | if self.mix_manual { 0 } else { MIXGAIN_MODE } | (self.mix_gain & 0x0F)
    }

    /// R10: channel filter on (PWD_FILT = 1), PW_FILT = 00 highest power,
    /// the fixed bit, FILT_CODE.
    pub fn r10(&self) -> u8 {
        PWD_FILT | R10_FIXED | (self.filt_code & 0x0F)
    }

    /// R11: FILT_BW and the HPF at its lowest corner.
    pub fn r11(&self) -> u8 {
        (self.filt_bw & 0b11) << FILT_BW_SHIFT | HPF_LOWEST
    }

    /// R12: the fixed bits, VGA on (PWD_VGA = 1), the mode and code.
    pub fn r12(&self) -> u8 {
        R12_FIXED | PWD_VGA | if self.vga_from_pin { VGA_MODE } else { 0 } | (self.vga_code & 0x0F)
    }

    /// R26: RFMUX, the fixed bits, PLL_AUTO_CLK 128 kHz, RFFILT.
    pub fn r26(&self) -> u8 {
        (self.rfmux & 0b11) << RFMUX_SHIFT | R26_FIXED | PLL_AUTO_CLK_128K << PLL_AUTO_CLK_SHIFT | (self.rffilt & 0b11)
    }

    /// R27: the tracking filter's notch and low-pass corner codes.
    pub fn r27(&self) -> u8 {
        (self.tf_nch & 0x0F) << TF_NCH_SHIFT | (self.tf_lp & 0x0F)
    }

    /// The 32-byte register image for this front end and PLL. R0-R4 are
    /// read-only and left 0; only R5-R31 are written.
    pub fn image(&self, pll: &Pll) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[5] = self.r5();
        r[6] = self.r6();
        r[7] = self.r7();
        // Mixer buffer on, high current (it drives the IF filter), IMR_G.
        r[8] = PWD_AMP | IMR_MIDDLE;
        // IF filter on (PWD_IFFILT = 0), high current (PW1_IFFILT = 0), IMR_P.
        r[9] = IMR_MIDDLE;
        r[10] = self.r10();
        r[11] = self.r11();
        r[12] = self.r12();
        r[13] = VTH_WINDOW;
        r[14] = VTH_WINDOW;
        // Clock output ON (CLK_OUT_ENB = 0, findings step 4a), internal
        // AGC clock on (CLK_AGC_ENB = 0: the detectors need it in auto).
        r[15] = R15_FIXED;
        r[16] = pll.r16();
        r[17] = PW_LDO_A_2V0 << PW_LDO_A_SHIFT | R17_FIXED;
        // PW_SDM = 0: undescribed, left clear unless a sweep says otherwise.
        r[18] = R18_FIXED;
        r[19] = R19_FIXED;
        r[20] = pll.r20();
        r[21] = pll.r21();
        r[22] = pll.r22();
        // OPEN_D = 0 High-Z: not driving an undescribed open-drain pin.
        r[23] = PW_LDO_D_1V8_8MA << PW_LDO_D_SHIFT | R23_FIXED;
        r[24] = R24_FIXED;
        // RF filter on (PWD_RFFILT = 1), SW_AGC = 0 agc_in.
        r[25] = PWD_RFFILT | R25_FIXED;
        r[26] = self.r26();
        r[27] = self.r27();
        r[28] = PDET3_GAIN_MIDDLE << PDET3_GAIN_SHIFT | R28_FIXED;
        r[29] = R29_FIXED | PDET1_GAIN_MIDDLE << PDET1_GAIN_SHIFT | PDET2_GAIN_MIDDLE;
        r[30] = R30_B6 | PDET_CLK_MIN;
        r[31] = R31_FIXED;
        r
    }
}

/// The fixed cells an image violates (empty for every image built here).
pub fn fixed_violations(img: &[u8; 32]) -> Vec<String> {
    FIXED
        .iter()
        .filter(|&&(reg, mask, value)| img[reg as usize] & mask != value)
        .map(|&(reg, mask, value)| format!("R{reg}: 0x{:02X} & 0x{mask:02X} != 0x{value:02X}", img[reg as usize]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::band::IF_HZ;
    use crate::tables::tuner_image::TUNER_IMAGE;

    /// The builder reproduces the bench's recorded image byte for byte:
    /// `tables::tuner_image` was written for the vagc configuration
    /// (VGA_MODE = 1) at the 300 MHz tune (LO 304.57 MHz, /8).
    #[test]
    fn builder_reproduces_the_recorded_image() {
        let pll = Pll::with_divider(300e6 + IF_HZ, 0b010).unwrap();
        let front = Front { vga_from_pin: true, ..Front::BRING_UP };
        let img = front.image(&pll);
        for choice in TUNER_IMAGE {
            assert_eq!(
                img[choice.reg as usize], choice.value,
                "R{}: built 0x{:02X}, recorded 0x{:02X} ({})",
                choice.reg, img[choice.reg as usize], choice.value, choice.why
            );
        }
        // The manual VGA differs from the record in R12's VGA_MODE alone.
        let manual = Front::BRING_UP.image(&pll);
        for reg in IMAGE_FIRST_REG..32 {
            if reg == R12 {
                assert_eq!(manual[reg as usize] ^ img[reg as usize], VGA_MODE);
            } else {
                assert_eq!(manual[reg as usize], img[reg as usize], "R{reg}");
            }
        }
    }

    /// Findings step 4a: R5-R15 read back as `98 90 68 A0 20 90 0F E8 A5
    /// A5 28` for the manual configuration.
    #[test]
    fn manual_image_matches_the_clkout_check_readback() {
        let pll = Pll::with_divider(300e6 + IF_HZ, 0b010).unwrap();
        let img = Front::BRING_UP.image(&pll);
        assert_eq!(&img[5..16], &[0x98, 0x90, 0x68, 0xA0, 0x20, 0x90, 0x0F, 0xE8, 0xA5, 0xA5, 0x28]);
    }

    #[test]
    fn every_image_keeps_every_fixed_cell_and_the_clock_on() {
        let pll = Pll::with_divider(300e6 + IF_HZ, 0b010).unwrap();
        let fronts = [
            Front::BRING_UP,
            Front { lna_manual: false, mix_manual: false, pdet1_on: true, pdet3_on: true, ..Front::BRING_UP },
            Front { vga_from_pin: true, filt_code: 15, filt_bw: 3, rfmux: 0, rffilt: 2, tf_nch: 15, tf_lp: 15, ..Front::BRING_UP },
            Front { lna_gain: 15, mix_gain: 15, vga_code: 15, ..Front::BRING_UP },
            Front { lna_gain: 0, mix_gain: 0, vga_code: 0, ..Front::BRING_UP },
        ];
        for f in fronts {
            let img = f.image(&pll);
            assert!(fixed_violations(&img).is_empty(), "{:?}", fixed_violations(&img));
            assert_eq!(img[R15 as usize] & CLK_OUT_ENB, 0, "the clock output must stay on (step 4a)");
            assert_eq!(img[R15 as usize] & CLK_AGC_ENB, 0);
        }
    }

    /// The gain modes' polarities are opposite (Table 1-3): LNA 1 =
    /// manual, mixer 0 = manual; VGA_MODE 1 = pin.
    #[test]
    fn gain_mode_bits_follow_table_1_3() {
        let auto = Front { lna_manual: false, mix_manual: false, pdet1_on: true, pdet3_on: true, ..Front::BRING_UP };
        assert_eq!(Front::BRING_UP.r5() & LNA_GAIN_MODE, LNA_GAIN_MODE);
        assert_eq!(auto.r5() & LNA_GAIN_MODE, 0);
        assert_eq!(Front::BRING_UP.r7() & MIXGAIN_MODE, 0);
        assert_eq!(auto.r7() & MIXGAIN_MODE, MIXGAIN_MODE);
        assert_eq!(auto.r6() & (PWD_PDET1 | PWD_PDET3), PWD_PDET3, "PDET1 on = 0, PDET3 on = 1");
        assert_eq!(Front::BRING_UP.r6() & (PWD_PDET1 | PWD_PDET3), PWD_PDET1);
        assert_eq!(Front { vga_from_pin: true, ..Front::BRING_UP }.r12() & VGA_MODE, VGA_MODE);
    }
}
