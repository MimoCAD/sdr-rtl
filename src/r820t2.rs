//! The R820T2's registers, one named field per bit range.
//!
//! Point of origin: Rafael Micro, "R820T2 Register Description" (v1.3,
//! 2014-10-14), Table 1-2 (the register
//! matrix: which bits each field occupies) and Table 1-3 (what each
//! field means). Positions follow the matrix's cell spans and Table
//! 1-3's Bitmap column, which agree everywhere; several field NAMES in
//! the document carry a wrong bracketed width (`LNA_VTHH[4:0]` over
//! [7:4], `SEL_DIV[3:0]` over [7:5], `TF_NCH[1:0]` and `PDET3_GAIN[1:0]`
//! over [7:4]) and are named here by their bits.
//!
//! Bits the document prints as fixed values or leaves blank, but which
//! the driver writes, carry the name librtlsdr's `tuner_r82xx.c` gives
//! them in its comments (cited per field); a bit no source names is
//! named for its register and position. The status registers R2-R4
//! are read as rtl-sdr-rs and librtlsdr read them (lock, gain indices,
//! VCO fine tune, filter calibration code), finer than Table 1-2's
//! `VCO_INDICATOR` / `RF_INDICATOR`.
//!
//! Several "PWD_" (power-down) fields of Table 1-3 are, by its own
//! value column, enables (`PWD_MIX`: 0 off, 1 on); each is named here
//! for what a 1 does.

/// A register address (Table 1-2's "Reg Address"; R5 is 0x05).
pub(crate) type Reg = u8;

pub(crate) const R0: Reg = 0x00;
pub(crate) const R2: Reg = 0x02;
pub(crate) const R3: Reg = 0x03;
pub(crate) const R4: Reg = 0x04;
pub(crate) const R5: Reg = 0x05;
pub(crate) const R6: Reg = 0x06;
pub(crate) const R7: Reg = 0x07;
pub(crate) const R8: Reg = 0x08;
pub(crate) const R9: Reg = 0x09;
pub(crate) const R10: Reg = 0x0a;
pub(crate) const R11: Reg = 0x0b;
pub(crate) const R12: Reg = 0x0c;
pub(crate) const R13: Reg = 0x0d;
pub(crate) const R14: Reg = 0x0e;
pub(crate) const R15: Reg = 0x0f;
pub(crate) const R16: Reg = 0x10;
pub(crate) const R17: Reg = 0x11;
pub(crate) const R18: Reg = 0x12;
pub(crate) const R19: Reg = 0x13;
pub(crate) const R20: Reg = 0x14;
pub(crate) const R21: Reg = 0x15;
pub(crate) const R22: Reg = 0x16;
pub(crate) const R23: Reg = 0x17;
pub(crate) const R24: Reg = 0x18;
pub(crate) const R25: Reg = 0x19;
pub(crate) const R26: Reg = 0x1a;
pub(crate) const R27: Reg = 0x1b;
pub(crate) const R28: Reg = 0x1c;
pub(crate) const R29: Reg = 0x1d;
pub(crate) const R30: Reg = 0x1e;
pub(crate) const R31: Reg = 0x1f;

/// The first writable register: R0-R4 are read-only status.
pub(crate) const FIRST_WRITABLE: Reg = R5;
/// The register count (Table 1-2: "Total 32 registers").
pub(crate) const REGISTER_COUNT: usize = 32;

/// One field: `width` bits of register `reg`, the lowest at `shift`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Field {
    pub reg: Reg,
    pub shift: u8,
    pub width: u8,
}

/// Field values bound for one register: the bits to write and which
/// bits they own (a masked write leaves the rest as cached).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bits {
    pub reg: Reg,
    pub value: u8,
    pub mask: u8,
}

impl Field {
    /// Bits `hi..=lo` of `reg`, as Table 1-2 draws them.
    const fn at(reg: Reg, hi: u8, lo: u8) -> Field {
        Field { reg, shift: lo, width: hi - lo + 1 }
    }

    pub const fn mask(self) -> u8 {
        (((1u16 << self.width) - 1) as u8) << self.shift
    }

    /// The field set to `value` (in field units, its low bits kept).
    pub const fn set(self, value: u8) -> Bits {
        Bits { reg: self.reg, value: (value << self.shift) & self.mask(), mask: self.mask() }
    }

    /// The field's value in a byte read from (or cached for) its register.
    pub const fn get(self, byte: u8) -> u8 {
        (byte & self.mask()) >> self.shift
    }
}

impl Bits {
    /// These bits and `other`'s, one write to their shared register.
    pub const fn with(self, other: Bits) -> Bits {
        assert!(self.reg == other.reg, "fields of different registers");
        assert!(self.mask & other.mask == 0, "overlapping fields");
        Bits { reg: self.reg, value: self.value | other.value, mask: self.mask | other.mask }
    }

    /// The whole register's byte; every bit must be bound.
    pub const fn byte(self) -> u8 {
        assert!(self.mask == 0xff, "a register value must bind all eight bits");
        self.value
    }
}

// ---- R0-R4, read only -------------------------------------------------------

/// R0: the chip's fixed check value. Table 1-2 prints %10010110 (0x96),
/// sent LSB first; a plain one-byte read returns it reversed, 0x69.
pub(crate) const CHIP_ID: Field = Field::at(R0, 7, 0);
/// R2 bit 6 (inside Table 1-2's `VCO_INDICATOR[6:0]`): the PLL is locked.
pub(crate) const PLL_LOCKED: Field = Field::at(R2, 6, 6);
/// R3 (Table 1-2's `RF_INDICATOR[7:0]`): the LNA's live gain index…
pub(crate) const LNA_GAIN_INDICATOR: Field = Field::at(R3, 3, 0);
/// …and the mixer's, valid under AGC too.
pub(crate) const MIXER_GAIN_INDICATOR: Field = Field::at(R3, 7, 4);
/// R4 (blank in Table 1-2): the chip's VCO band estimate…
pub(crate) const VCO_FINE_TUNE: Field = Field::at(R4, 5, 4);
/// …and the IF filter calibration result.
pub(crate) const FILTER_CALIBRATION_CODE: Field = Field::at(R4, 3, 0);

// ---- R5 ---------------------------------------------------------------------

/// `PWD_LT`: loop-through output, 1 = off.
pub(crate) const LOOP_THROUGH_OFF: Field = Field::at(R5, 7, 7);
/// Table 1-2 prints 0; librtlsdr's `air_cable1_in` (with bit 5) selects
/// the R828D's cable-1 input.
pub(crate) const CABLE1_INPUT: Field = Field::at(R5, 6, 6);
/// `PWD_LNA1`: LNA 1, 1 = off — on the R828D, the air input's LNA.
pub(crate) const LNA1_OFF: Field = Field::at(R5, 5, 5);
/// `LNA_GAIN_MODE`: 0 auto, 1 manual.
pub(crate) const LNA_GAIN_MANUAL: Field = Field::at(R5, 4, 4);
/// `LNA_GAIN[3:0]`: manual LNA gain, 15 max.
pub(crate) const LNA_GAIN: Field = Field::at(R5, 3, 0);

// ---- R6 ---------------------------------------------------------------------

/// `PWD_PDET1`: power detector 1, 1 = off.
pub(crate) const PDET1_OFF: Field = Field::at(R6, 7, 7);
/// `PWD_PDET3`: power detector 3, 1 = ON (Table 1-3: 0 off, 1 on);
/// librtlsdr's "PRE_DECT".
pub(crate) const PDET3_ON: Field = Field::at(R6, 6, 6);
/// `FILT_3DB`: filter gain, 1 = +3 dB.
pub(crate) const FILT_3DB: Field = Field::at(R6, 5, 5);
/// Table 1-2 prints 1; librtlsdr's "V6MHz" (its `filt_gain` 0x10 sets
/// this bit and clears `FILT_3DB`, though its comment reads "+3db").
pub(crate) const V6MHZ: Field = Field::at(R6, 4, 4);
/// Table 1-2 prints 0; librtlsdr's `cable2_in`, the R828D's cable-2 input.
pub(crate) const CABLE2_INPUT: Field = Field::at(R6, 3, 3);
/// `PW_LNA[2:0]`: LNA power, 000 max, 111 min.
pub(crate) const PW_LNA: Field = Field::at(R6, 2, 0);

// ---- R7 ---------------------------------------------------------------------

/// Table 1-2 prints 0; librtlsdr's `img_r` ("image negative" when 0).
pub(crate) const IMAGE_SIDE: Field = Field::at(R7, 7, 7);
/// `PWD_MIX`: mixer power, 1 = ON.
pub(crate) const MIXER_ON: Field = Field::at(R7, 6, 6);
/// `PW0_MIX`: mixer current, 0 max, 1 normal.
pub(crate) const PW0_MIX: Field = Field::at(R7, 5, 5);
/// `MIXGAIN_MODE`: 0 manual, 1 auto.
pub(crate) const MIXER_GAIN_AUTO: Field = Field::at(R7, 4, 4);
/// `MIX_GAIN[3:0]`: manual mixer gain, 1111 max.
pub(crate) const MIX_GAIN: Field = Field::at(R7, 3, 0);

// ---- R8, R9 -----------------------------------------------------------------

/// `PWD_AMP`: mixer buffer, 1 = ON.
pub(crate) const MIXER_BUFFER_ON: Field = Field::at(R8, 7, 7);
/// `PW0_AMP`: mixer buffer current, 0 high, 1 low.
pub(crate) const PW0_AMP: Field = Field::at(R8, 6, 6);
/// `IMR_G[5:0]`: image rejection gain adjustment.
pub(crate) const IMR_G: Field = Field::at(R8, 5, 0);
/// `PWD_IFFILT`: IF filter, 1 = off.
pub(crate) const IF_FILTER_OFF: Field = Field::at(R9, 7, 7);
/// `PW1_IFFILT`: IF filter current, 0 high, 1 low.
pub(crate) const PW1_IFFILT: Field = Field::at(R9, 6, 6);
/// `IMR_P[5:0]`: image rejection phase adjustment.
pub(crate) const IMR_P: Field = Field::at(R9, 5, 0);

// ---- R10, R11: the channel filter ------------------------------------------

/// `PWD_FILT`: channel filter, 1 = ON.
pub(crate) const CHANNEL_FILTER_ON: Field = Field::at(R10, 7, 7);
/// `PW_FILT[1:0]`: filter power, 00 highest, 11 lowest.
pub(crate) const PW_FILT: Field = Field::at(R10, 6, 5);
/// Table 1-2 prints 1; librtlsdr's `filt_q` ("r10[4]: low q").
pub(crate) const FILT_Q: Field = Field::at(R10, 4, 4);
/// `FILT_CODE[3:0]`: bandwidth fine tune, 0000 widest, 1111 narrowest.
pub(crate) const FILT_CODE: Field = Field::at(R10, 3, 0);
/// Table 1-2 prints 0. librtlsdr's `hp_cor` comment reads "1.7m disable"
/// with it clear; its narrow-bandwidth path (below the 1.7 MHz low-pass)
/// sets it. Named for that path; no source names it more exactly.
pub(crate) const NARROW_BANDWIDTH_PATH: Field = Field::at(R11, 7, 7);
/// `FILT_BW`: bandwidth coarse tune, 00 widest, 11 narrowest
/// (librtlsdr's "filt_cap").
pub(crate) const FILT_BW: Field = Field::at(R11, 6, 5);
/// Table 1-2 prints 0; librtlsdr's filter calibration "Start/Stop Trigger".
pub(crate) const FILTER_CALIBRATION_TRIGGER: Field = Field::at(R11, 4, 4);
/// `HPF[3:0]`: high-pass corner, 0000 highest, 1111 lowest.
pub(crate) const HPF: Field = Field::at(R11, 3, 0);

// ---- R12: the IF VGA ---------------------------------------------------------

/// Table 1-2 prints 1; the gain writes (librtlsdr's mask 0x9f) clear it.
pub(crate) const R12_BIT7: Field = Field::at(R12, 7, 7);
/// `PWD_VGA`: VGA power, 1 = ON.
pub(crate) const VGA_ON: Field = Field::at(R12, 6, 6);
/// Table 1-2 prints 1.
pub(crate) const R12_BIT5: Field = Field::at(R12, 5, 5);
/// `VGA_MODE`: 1 = the vagc pin sets the gain, 0 = `VGA_CODE`.
pub(crate) const VGA_PIN_CONTROL: Field = Field::at(R12, 4, 4);
/// `VGA_CODE[3:0]`: IF VGA gain, 3.5 dB steps.
pub(crate) const VGA_CODE: Field = Field::at(R12, 3, 0);

// ---- R13, R14: AGC detector thresholds (0.34 V + ~0.1 V per step) -----------

/// Table 1-3 `LNA_VTHH`, printed `[4:0]` over [7:4].
pub(crate) const LNA_VTH_HIGH: Field = Field::at(R13, 7, 4);
pub(crate) const LNA_VTH_LOW: Field = Field::at(R13, 3, 0);
/// Table 1-3 `MIX_VTH_H`, printed `[4:0]` over [7:4].
pub(crate) const MIX_VTH_HIGH: Field = Field::at(R14, 7, 4);
pub(crate) const MIX_VTH_LOW: Field = Field::at(R14, 3, 0);

// ---- R15 --------------------------------------------------------------------

/// Table 1-2 prints 0; librtlsdr's `flt_ext_widest` ("r15[7]").
pub(crate) const FILTER_EXTENSION_WIDEST: Field = Field::at(R15, 7, 7);
/// Table 1-2 prints 0.
pub(crate) const R15_BIT6: Field = Field::at(R15, 6, 6);
/// Table 1-2 prints 1.
pub(crate) const R15_BIT5: Field = Field::at(R15, 5, 5);
/// `CLK_OUT_ENB`: clock-out pin, 1 = off.
pub(crate) const CLOCK_OUT_OFF: Field = Field::at(R15, 4, 4);
/// Table 1-2 prints 1.
pub(crate) const R15_BIT3: Field = Field::at(R15, 3, 3);
/// Table 1-2 prints 0; librtlsdr's "cali clk", on while the IF filter
/// calibrates.
pub(crate) const CALIBRATION_CLOCK: Field = Field::at(R15, 2, 2);
/// `CLK_AGC_ENB`: internal AGC clock, 1 = off.
pub(crate) const AGC_CLOCK_OFF: Field = Field::at(R15, 1, 1);
/// Table 1-2 prints 0.
pub(crate) const R15_BIT0: Field = Field::at(R15, 0, 0);

// ---- R16-R22: the PLL --------------------------------------------------------

/// `SEL_DIV`, printed `[3:0]` over [7:5]: the mixer divider, 000 = VCO/2,
/// each step doubling.
pub(crate) const SEL_DIV: Field = Field::at(R16, 7, 5);
/// `REFDIV`: 1 = the reference is the crystal / 2.
pub(crate) const REF_DIV2: Field = Field::at(R16, 4, 4);
/// Table 1-2 prints 0; librtlsdr's 30 pF crystal-cap value sets it with
/// `CAPX` = 11.
pub(crate) const XTAL_CAP_EXTRA: Field = Field::at(R16, 3, 3);
/// Table 1-2 prints 1.
pub(crate) const R16_BIT2: Field = Field::at(R16, 2, 2);
/// `CAPX[1:0]`: internal crystal cap, 00 none, 01 10 pF, 10 20 pF, 11 30 pF.
pub(crate) const CAPX: Field = Field::at(R16, 1, 0);
/// `PW_LDO_A[1:0]`: PLL analog regulator, 00 off, 01 2.1 V, 10 2.0 V, 11 1.9 V.
pub(crate) const PW_LDO_A: Field = Field::at(R17, 7, 6);
/// Table 1-2 prints 000; librtlsdr's `cp_cur` (charge pump, 111 = auto).
pub(crate) const CHARGE_PUMP_CURRENT: Field = Field::at(R17, 5, 3);
/// Table 1-2 prints 011.
pub(crate) const R17_LOW_BITS: Field = Field::at(R17, 2, 0);
/// Table 1-2 prints 100; librtlsdr's "VCO current".
pub(crate) const VCO_CURRENT: Field = Field::at(R18, 7, 5);
/// Table 1-2 prints 0.
pub(crate) const R18_BIT4: Field = Field::at(R18, 4, 4);
/// `PW_SDM`: the fractional (sigma-delta) divider's power switch.
pub(crate) const PW_SDM: Field = Field::at(R18, 3, 3);
/// Table 1-2 prints 000.
pub(crate) const R18_LOW_BITS: Field = Field::at(R18, 2, 0);
/// Table 1-2 prints 00.
pub(crate) const R19_HIGH_BITS: Field = Field::at(R19, 7, 6);
/// Table 1-2 prints zeros; librtlsdr's "version".
pub(crate) const VERSION: Field = Field::at(R19, 5, 0);
/// `SI2C`: Nint = 4·Ni2c + Si2c + 13.
pub(crate) const SI2C: Field = Field::at(R20, 7, 6);
/// `NI2C[5:0]` (the matrix labels it `N_I2C[4:0]` over six bits).
pub(crate) const NI2C: Field = Field::at(R20, 5, 0);
/// `SDM_IN[8:1]`: the fractional divider word's low octet…
pub(crate) const SDM_IN_LOW: Field = Field::at(R21, 7, 0);
/// …and `SDM_IN[16:9]`, its high octet (Nfra = SDM / 2^16).
pub(crate) const SDM_IN_HIGH: Field = Field::at(R22, 7, 0);

// ---- R23-R31 -----------------------------------------------------------------

/// `PW_LDO_D[1:0]`: PLL digital regulator, 11 = off.
pub(crate) const PW_LDO_D: Field = Field::at(R23, 7, 6);
/// Table 1-2 prints 11; librtlsdr's `div_buf_cur` (11 = 150 µA).
pub(crate) const DIVIDER_BUFFER_CURRENT: Field = Field::at(R23, 5, 4);
/// `OPEN_D`: open drain, 0 high-Z, 1 low-Z (the Blog V4's notch control).
pub(crate) const OPEN_D: Field = Field::at(R23, 3, 3);
/// Table 1-2 prints 100.
pub(crate) const R23_LOW_BITS: Field = Field::at(R23, 2, 0);
/// Table 1-2 prints 01 and six blanks.
pub(crate) const R24_BITS: Field = Field::at(R24, 7, 0);
/// `PWD_RFFILT`: RF filter power, 1 = ON.
pub(crate) const RF_FILTER_ON: Field = Field::at(R25, 7, 7);
/// Table 1-2 prints 00; librtlsdr's `polyfil_cur` ("r25[6:5]", 11 = min).
pub(crate) const RF_POLY_FILTER_CURRENT: Field = Field::at(R25, 6, 5);
/// `SW_AGC`: 0 agc = agc_in, 1 agc = agc_in2.
pub(crate) const SW_AGC: Field = Field::at(R25, 4, 4);
/// Table 1-2 prints 11 and two blanks.
pub(crate) const R25_LOW_BITS: Field = Field::at(R25, 3, 0);
/// `RFMUX[1:0]`: tracking filter, 00 on, 01 bypass.
pub(crate) const RFMUX: Field = Field::at(R26, 7, 6);
/// Table 1-2 prints 10; librtlsdr's "agc clk".
pub(crate) const AGC_CLOCK: Field = Field::at(R26, 5, 4);
/// `PLL_AUTO_CLK[1:0]`: PLL autotune clock, 00 128 kHz, 01 32 kHz, 10 8 kHz.
pub(crate) const PLL_AUTO_CLK: Field = Field::at(R26, 3, 2);
/// `RFFILT[1:0]`: RF filter band, 00 highest, 01 middle, 10 low.
pub(crate) const RFFILT: Field = Field::at(R26, 1, 0);
/// `TF_NCH`, printed `[1:0]` over [7:4]: the tracking notch corner.
pub(crate) const TF_NCH: Field = Field::at(R27, 7, 4);
/// `TF_LP[3:0]`: the tracking low-pass corner.
pub(crate) const TF_LP: Field = Field::at(R27, 3, 0);
/// `PDET3_GAIN`, printed `[1:0]` over [7:4]: detector 3 take-off point,
/// librtlsdr's "mixer top".
pub(crate) const PDET3_GAIN: Field = Field::at(R28, 7, 4);
/// Table 1-2 prints 0.
pub(crate) const R28_BIT3: Field = Field::at(R28, 3, 3);
/// Table 1-2 prints 1; librtlsdr's "discharge mode" (0 = normal).
pub(crate) const DISCHARGE_MODE: Field = Field::at(R28, 2, 2);
/// Table 1-2 prints a blank and 0.
pub(crate) const R28_LOW_BITS: Field = Field::at(R28, 1, 0);
/// Table 1-2 prints 11; librtlsdr's "detect bw".
pub(crate) const DETECT_BANDWIDTH: Field = Field::at(R29, 7, 6);
/// `PDET1_GAIN[2:0]`: detector 1 take-off point, librtlsdr's "LNA TOP".
pub(crate) const PDET1_GAIN: Field = Field::at(R29, 5, 3);
/// `PDET2_GAIN[2:0]`: detector 2 take-off point, librtlsdr's "predet top".
pub(crate) const PDET2_GAIN: Field = Field::at(R29, 2, 0);
/// Table 1-2 prints 0.
pub(crate) const R30_BIT7: Field = Field::at(R30, 7, 7);
/// Table 1-3's `FILTER_EXT` (filter extension under weak signal; the
/// matrix prints this bit fixed 1); librtlsdr's "r30[6]=1 ext enable".
pub(crate) const FILTER_EXTENSION: Field = Field::at(R30, 6, 6);
/// librtlsdr's "r30[5]: 1 ext at lna max-1" (Table 1-3 folds this bit
/// into `PDET_CLK[5:0]`).
pub(crate) const FILTER_EXTENSION_AT_LNA_MAX_LESS_1: Field = Field::at(R30, 5, 5);
/// The matrix's `PDET_CLK[4:0]` (power detector timing); librtlsdr's
/// "LNA discharge current".
pub(crate) const LNA_DISCHARGE_CURRENT: Field = Field::at(R30, 4, 0);
/// Table 1-2 prints 1; librtlsdr's `lt_att` ("r31[7], lt att enable").
pub(crate) const LOOP_THROUGH_ATTENUATION: Field = Field::at(R31, 7, 7);
/// Table 1-2 prints 1000 0 and two blanks.
pub(crate) const R31_LOW_BITS: Field = Field::at(R31, 6, 0);

// ---- field values with meanings ----------------------------------------------

/// `RFMUX`: tracking filter in circuit / bypassed.
pub(crate) const RFMUX_TRACKING_FILTER: u8 = 0b00;
pub(crate) const RFMUX_BYPASS: u8 = 0b01;
/// `RFFILT` bands.
pub(crate) const RFFILT_HIGHEST: u8 = 0b00;
pub(crate) const RFFILT_MIDDLE: u8 = 0b01;
pub(crate) const RFFILT_LOW: u8 = 0b10;
/// `FILT_BW` coarse bandwidth (Table 1-3: 00 widest, 01 or 10 middle, 11
/// narrowest).
pub(crate) const FILT_BW_WIDEST: u8 = 0b00;
pub(crate) const FILT_BW_MIDDLE: u8 = 0b01;
pub(crate) const FILT_BW_NARROWEST: u8 = 0b11;
/// `PLL_AUTO_CLK` rates.
pub(crate) const PLL_AUTO_CLK_128KHZ: u8 = 0b00;
pub(crate) const PLL_AUTO_CLK_8KHZ: u8 = 0b10;
/// `AGC_CLOCK` rates (librtlsdr).
pub(crate) const AGC_CLOCK_250HZ: u8 = 0b11;
pub(crate) const AGC_CLOCK_60HZ: u8 = 0b10;
/// `CHARGE_PUMP_CURRENT` auto (librtlsdr "111, auto").
pub(crate) const CHARGE_PUMP_AUTO: u8 = 0b111;
/// `DIVIDER_BUFFER_CURRENT` 150 µA (librtlsdr "11, 150u").
pub(crate) const DIVIDER_BUFFER_150UA: u8 = 0b11;
/// `PW_FILT` low (librtlsdr "10, low").
pub(crate) const PW_FILT_LOW: u8 = 0b10;
/// `RF_POLY_FILTER_CURRENT` minimum (librtlsdr "r25[6:5]: min").
pub(crate) const RF_POLY_FILTER_CURRENT_MIN: u8 = 0b11;
/// A `FILTER_CALIBRATION_CODE` of all ones: the calibration saturated.
pub(crate) const FILTER_CALIBRATION_SATURATED: u8 = 0b1111;
/// `FILT_CODE` 0000: Table 1-3's widest (librtlsdr's fallback, which
/// its comment calls "narrowest").
pub(crate) const FILT_CODE_WIDEST: u8 = 0b0000;

/// The writable registers' fields, R5 first — every bit of every
/// register bound exactly once (a test holds it).
#[cfg(test)]
pub(crate) const WRITABLE_FIELDS: [&[Field]; REGISTER_COUNT - FIRST_WRITABLE as usize] = [
    &[LOOP_THROUGH_OFF, CABLE1_INPUT, LNA1_OFF, LNA_GAIN_MANUAL, LNA_GAIN],
    &[PDET1_OFF, PDET3_ON, FILT_3DB, V6MHZ, CABLE2_INPUT, PW_LNA],
    &[IMAGE_SIDE, MIXER_ON, PW0_MIX, MIXER_GAIN_AUTO, MIX_GAIN],
    &[MIXER_BUFFER_ON, PW0_AMP, IMR_G],
    &[IF_FILTER_OFF, PW1_IFFILT, IMR_P],
    &[CHANNEL_FILTER_ON, PW_FILT, FILT_Q, FILT_CODE],
    &[NARROW_BANDWIDTH_PATH, FILT_BW, FILTER_CALIBRATION_TRIGGER, HPF],
    &[R12_BIT7, VGA_ON, R12_BIT5, VGA_PIN_CONTROL, VGA_CODE],
    &[LNA_VTH_HIGH, LNA_VTH_LOW],
    &[MIX_VTH_HIGH, MIX_VTH_LOW],
    &[FILTER_EXTENSION_WIDEST, R15_BIT6, R15_BIT5, CLOCK_OUT_OFF, R15_BIT3, CALIBRATION_CLOCK, AGC_CLOCK_OFF, R15_BIT0],
    &[SEL_DIV, REF_DIV2, XTAL_CAP_EXTRA, R16_BIT2, CAPX],
    &[PW_LDO_A, CHARGE_PUMP_CURRENT, R17_LOW_BITS],
    &[VCO_CURRENT, R18_BIT4, PW_SDM, R18_LOW_BITS],
    &[R19_HIGH_BITS, VERSION],
    &[SI2C, NI2C],
    &[SDM_IN_LOW],
    &[SDM_IN_HIGH],
    &[PW_LDO_D, DIVIDER_BUFFER_CURRENT, OPEN_D, R23_LOW_BITS],
    &[R24_BITS],
    &[RF_FILTER_ON, RF_POLY_FILTER_CURRENT, SW_AGC, R25_LOW_BITS],
    &[RFMUX, AGC_CLOCK, PLL_AUTO_CLK, RFFILT],
    &[TF_NCH, TF_LP],
    &[PDET3_GAIN, R28_BIT3, DISCHARGE_MODE, R28_LOW_BITS],
    &[DETECT_BANDWIDTH, PDET1_GAIN, PDET2_GAIN],
    &[R30_BIT7, FILTER_EXTENSION, FILTER_EXTENSION_AT_LNA_MAX_LESS_1, LNA_DISCHARGE_CURRENT],
    &[LOOP_THROUGH_ATTENUATION, R31_LOW_BITS],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every writable register's fields tile its eight bits: no gap, no
    /// overlap, each field in the register it is listed under.
    #[test]
    fn fields_tile_every_register() {
        for (i, fields) in WRITABLE_FIELDS.iter().enumerate() {
            let reg = FIRST_WRITABLE + i as u8;
            let mut seen = 0u8;
            for f in fields.iter() {
                assert_eq!(f.reg, reg, "{f:?} listed under R{reg}");
                assert_eq!(seen & f.mask(), 0, "{f:?} overlaps");
                seen |= f.mask();
            }
            assert_eq!(seen, 0xff, "R{reg} has unbound bits");
        }
    }

    #[test]
    fn set_and_get_round_trip() {
        assert_eq!(SEL_DIV.mask(), 0b1110_0000);
        assert_eq!(SEL_DIV.set(3), Bits { reg: R16, value: 0b0110_0000, mask: 0b1110_0000 });
        assert_eq!(SEL_DIV.get(0b0110_1100), 3);
        assert_eq!(LNA_VTH_HIGH.set(5).with(LNA_VTH_LOW.set(3)).byte(), 0x53);
    }
}
