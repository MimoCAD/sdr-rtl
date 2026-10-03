//! Rafael Micro R820T/R820T2/R828D tuner driver.
//!
//! Provenance: every register access goes through the named fields of
//! `r820t2` (Rafael Micro's R820T2 Register Description, Tables 1-2/1-3,
//! with librtlsdr's names for the bits it leaves unnamed); the init
//! values, band tables and sequencing follow rtl-sdr-rs (MPL-2.0) and
//! librtlsdr, spelled field by field. The PLL fractional math is computed
//! directly from the hardware model (f_vco = 2·f_ref·(Nint + SDM/2^16),
//! f_lo = f_vco / mix_div) rather than transcribed. The RTL-SDR Blog V4
//! band-switch thresholds are documented dongle behavior (the fork's code is GPL and was not used).
//!
//! Everything the chip lets us read back, we read back: the RW registers
//! are write-only (only status regs 0x00–0x04 read), so the actual tuned
//! frequency is *derived from the exact register values written*, while
//! PLL lock and the live LNA/mixer gain indices come from true hardware
//! status reads.

use sdr_core::{Error, Result};
use crate::r820t2::*;
use crate::usb::{Rtl2832, RtlUsb};

/// The two-wire access the tuner needs: the RTL2832U's I2C repeater in
/// service, a recorder in the tests (which pin the exact register writes
/// every entry point issues).
pub(crate) trait TunerBus {
    fn i2c_write(&self, i2c_addr: u16, buf: &[u8]) -> Result<()>;
    fn i2c_read(&self, i2c_addr: u16, len: u16) -> Result<Vec<u8>>;
    fn i2c_read_reg(&self, i2c_addr: u16, reg: u8) -> Result<u8>;
}

impl TunerBus for RtlUsb {
    fn i2c_write(&self, i2c_addr: u16, buf: &[u8]) -> Result<()> {
        Rtl2832::i2c_write(self, i2c_addr, buf)
    }
    fn i2c_read(&self, i2c_addr: u16, len: u16) -> Result<Vec<u8>> {
        Rtl2832::i2c_read(self, i2c_addr, len)
    }
    fn i2c_read_reg(&self, i2c_addr: u16, reg: u8) -> Result<u8> {
        Rtl2832::i2c_read_reg(self, i2c_addr, reg)
    }
}

/// The 8-bit I2C write address of the R820T/R820T2: the 7-bit ID %0011010
/// with the write bit (R820T datasheet and R820T2 Register Description,
/// Table 3-1 / 1-1).
pub const R820T_I2C_ADDR: u16 = 0x34;
/// The R828D's (no Rafael document for the R828D; rtl-sdr-rs).
pub const R828D_I2C_ADDR: u16 = 0x74;
/// What a one-byte read of R0 returns on the whole family: Table 1-2's
/// fixed %10010110, which the chip sends LSB first.
pub const R82XX_CHIP_ID: u8 = 0b1001_0110u8.reverse_bits();
/// Low-IF used for SDR — the R820T datasheet's Table 4-1 IF for DVB-T in
/// a 6 MHz channel; the RTL2832 shifts it back down digitally, with
/// spectrum inversion.
pub const R82XX_IF_FREQ: u32 = 3_570_000;
/// The R828D's own crystal on everything except the Blog V4 (which feeds
/// it the RTL2832's 28.8 MHz instead).
pub const R828D_XTAL_FREQ: u32 = 16_000_000;

const NUM_CACHE_REGS: usize = REGISTER_COUNT - FIRST_WRITABLE as usize;
/// The RTL2832U's I2C master moves at most eight octets per message:
/// the start register, then up to seven values.
const MAX_I2C_MSG_LEN: usize = 8;
/// librtlsdr's `VER_NUM`, written to R19's `VERSION`.
const VER_NUM: u8 = 49;

/// Blog V4: HF is reached through an onboard 28.8 MHz upconverter.
const BLOG_V4_UPCONVERT_HZ: u32 = 28_800_000;
/// Plain R828D DVB-T sticks switch cable/air inputs at 345 MHz.
const R828D_INPUT_SWITCH_HZ: u32 = 345_000_000;

/// Power-on values for R5..=R31 — librtlsdr's `r82xx_init_array`, each
/// register spelled field by field.
const REG_INIT: [u8; NUM_CACHE_REGS] = [
    LOOP_THROUGH_OFF.set(1).with(CABLE1_INPUT.set(0)).with(LNA1_OFF.set(0)).with(LNA_GAIN_MANUAL.set(0)).with(LNA_GAIN.set(3)).byte(),
    PDET1_OFF.set(0).with(PDET3_ON.set(0)).with(FILT_3DB.set(1)).with(V6MHZ.set(1)).with(CABLE2_INPUT.set(0)).with(PW_LNA.set(2)).byte(),
    IMAGE_SIDE.set(0).with(MIXER_ON.set(1)).with(PW0_MIX.set(1)).with(MIXER_GAIN_AUTO.set(1)).with(MIX_GAIN.set(5)).byte(),
    MIXER_BUFFER_ON.set(1).with(PW0_AMP.set(1)).with(IMR_G.set(0)).byte(),
    IF_FILTER_OFF.set(0).with(PW1_IFFILT.set(1)).with(IMR_P.set(0)).byte(),
    CHANNEL_FILTER_ON.set(1).with(PW_FILT.set(PW_FILT_LOW)).with(FILT_Q.set(1)).with(FILT_CODE.set(6)).byte(),
    NARROW_BANDWIDTH_PATH.set(0).with(FILT_BW.set(FILT_BW_NARROWEST)).with(FILTER_CALIBRATION_TRIGGER.set(0)).with(HPF.set(12)).byte(),
    R12_BIT7.set(1).with(VGA_ON.set(1)).with(R12_BIT5.set(1)).with(VGA_PIN_CONTROL.set(1)).with(VGA_CODE.set(5)).byte(),
    LNA_VTH_HIGH.set(6).with(LNA_VTH_LOW.set(3)).byte(),
    MIX_VTH_HIGH.set(7).with(MIX_VTH_LOW.set(5)).byte(),
    R15_STANDARD.byte(),
    SEL_DIV.set(3).with(REF_DIV2.set(0)).with(XTAL_CAP_EXTRA.set(1)).with(R16_BIT2.set(1)).with(CAPX.set(0)).byte(),
    PW_LDO_A.set(2).with(CHARGE_PUMP_CURRENT.set(0)).with(R17_LOW_BITS.set(3)).byte(),
    VCO_CURRENT.set(4).with(R18_BIT4.set(0)).with(PW_SDM.set(0)).with(R18_LOW_BITS.set(0)).byte(),
    R19_HIGH_BITS.set(0).with(VERSION.set(0)).byte(),
    SI2C.set(0).with(NI2C.set(15)).byte(),
    SDM_IN_LOW.set(0).byte(),
    SDM_IN_HIGH.set(192).byte(),
    PW_LDO_D.set(0).with(DIVIDER_BUFFER_CURRENT.set(DIVIDER_BUFFER_150UA)).with(OPEN_D.set(0)).with(R23_LOW_BITS.set(0)).byte(),
    R24_BITS.set(72).byte(),
    RF_FILTER_ON.set(1).with(RF_POLY_FILTER_CURRENT.set(2)).with(SW_AGC.set(0)).with(R25_LOW_BITS.set(12)).byte(),
    RFMUX.set(RFMUX_BYPASS).with(AGC_CLOCK.set(AGC_CLOCK_60HZ)).with(PLL_AUTO_CLK.set(PLL_AUTO_CLK_128KHZ)).with(RFFILT.set(RFFILT_HIGHEST)).byte(),
    TF_NCH.set(0).with(TF_LP.set(0)).byte(),
    PDET3_GAIN.set(5).with(R28_BIT3.set(0)).with(DISCHARGE_MODE.set(1)).with(R28_LOW_BITS.set(0)).byte(),
    DETECT_BANDWIDTH.set(2).with(PDET1_GAIN.set(5)).with(PDET2_GAIN.set(6)).byte(),
    R30_BIT7.set(0).with(FILTER_EXTENSION.set(1)).with(FILTER_EXTENSION_AT_LNA_MAX_LESS_1.set(0)).with(LNA_DISCHARGE_CURRENT.set(10)).byte(),
    LOOP_THROUGH_ATTENUATION.set(1).with(R31_LOW_BITS.set(64)).byte(),
];

/// R15 as both the init table and standby write it: every named bit off,
/// the three Table 1-2 prints as 1 set.
const R15_STANDARD: Bits = FILTER_EXTENSION_WIDEST
    .set(0)
    .with(R15_BIT6.set(1))
    .with(R15_BIT5.set(1))
    .with(CLOCK_OUT_OFF.set(0))
    .with(R15_BIT3.set(1))
    .with(CALIBRATION_CLOCK.set(0))
    .with(AGC_CLOCK_OFF.set(0))
    .with(R15_BIT0.set(0));

/// Per-step gain increments (tenths of dB) for each hardware stage.
const LNA_STEPS: [i32; 16] = [0, 9, 13, 40, 38, 13, 31, 22, 26, 31, 26, 14, 19, 5, 35, 13];
const MIXER_STEPS: [i32; 16] = [0, 5, 10, 10, 19, 9, 10, 25, 17, 10, 8, 16, 13, 6, 3, -8];
const VGA_STEPS: [i32; 16] = [
    0, 26, 26, 30, 42, 35, 24, 13, 14, 32, 36, 34, 35, 37, 35, 36,
];
const VGA_BASE_TENTH_DB: i32 = -47;
/// `VGA_CODE` librtlsdr fixes for automatic gain (26.5 dB) and for
/// manual gain (16.3 dB).
const VGA_CODE_AUTO_GAIN: u8 = 11;
const VGA_CODE_MANUAL_GAIN: u8 = 8;

/// Per-band front-end settings (librtlsdr's `freq_ranges`): `OPEN_D`,
/// the RF mux and RF filter band, and the tracking filter's notch and
/// low-pass corners. `mhz` is the band's lower edge.
struct FreqRange {
    mhz: u32,
    open_d: u8,
    rf_mux: u8,
    rf_filter: u8,
    notch: u8,
    low_pass: u8,
}

const fn band(mhz: u32, open_d: u8, rf_mux: u8, rf_filter: u8, notch: u8, low_pass: u8) -> FreqRange {
    FreqRange { mhz, open_d, rf_mux, rf_filter, notch, low_pass }
}

const FREQ_RANGES: [FreqRange; 21] = [
    band(0, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 13, 15),
    band(50, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 11, 14),
    band(55, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 8, 11),
    band(60, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 7, 11),
    band(65, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 6, 9),
    band(70, 1, RFMUX_TRACKING_FILTER, RFFILT_LOW, 5, 8),
    band(75, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 4, 4),
    band(80, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 4, 4),
    band(90, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 3, 4),
    band(100, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 3, 4),
    band(110, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 2, 4),
    band(120, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 2, 4),
    band(140, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 1, 4),
    band(180, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 1, 3),
    band(220, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 1, 3),
    band(250, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 1, 1),
    band(280, 0, RFMUX_TRACKING_FILTER, RFFILT_LOW, 0, 0),
    band(310, 0, RFMUX_BYPASS, RFFILT_MIDDLE, 0, 0),
    band(450, 0, RFMUX_BYPASS, RFFILT_MIDDLE, 0, 0),
    band(588, 0, RFMUX_BYPASS, RFFILT_HIGHEST, 0, 0),
    band(650, 0, RFMUX_BYPASS, RFFILT_HIGHEST, 0, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum R82xxVariant {
    R820T,
    R828D { blog_v4: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputSel {
    Air,
    Cable1,
    Cable2,
}

/// The exact PLL programming last written — the source of truth for the
/// actual tuned frequency, since these registers cannot be read back.
#[derive(Clone, Copy, Debug)]
pub struct PllState {
    pub nint: u32,
    pub sdm: u32,
    pub mix_div: u32,
    /// f_lo actually synthesized, from the values above.
    pub actual_lo_hz: f64,
    pub locked: bool,
}

pub(crate) struct R82xx {
    pub variant: R82xxVariant,
    i2c_addr: u16,
    pub xtal_hz: u32,
    regs: [u8; NUM_CACHE_REGS],
    /// Current low-IF ("int_freq"): depends on the bandwidth setting.
    pub if_freq_hz: u32,
    fil_cal_code: u8,
    pub pll: Option<PllState>,
    /// Upconverter offset applied on the last tune (Blog V4 HF), in Hz.
    pub upconvert_hz: u32,
    last_input: Option<InputSel>,
    init_done: bool,
}

impl R82xx {
    pub fn new(variant: R82xxVariant, xtal_hz: u32) -> R82xx {
        R82xx {
            variant,
            i2c_addr: match variant {
                R82xxVariant::R820T => R820T_I2C_ADDR,
                R82xxVariant::R828D { .. } => R828D_I2C_ADDR,
            },
            xtal_hz,
            regs: REG_INIT,
            if_freq_hz: R82XX_IF_FREQ,
            fil_cal_code: 0,
            pll: None,
            upconvert_hz: 0,
            last_input: None,
            init_done: false,
        }
    }

    /// Probe both known I²C addresses for the family chip ID. Caller must
    /// have the repeater on. (The ID compares raw — the bit-reversal quirk
    /// applies to the status-block read protocol, not this single read.)
    pub fn probe(usb: &RtlUsb) -> Option<u16> {
        R82xx::probe_bus(usb)
    }

    pub(crate) fn probe_bus(usb: &dyn TunerBus) -> Option<u16> {
        for addr in [R820T_I2C_ADDR, R828D_I2C_ADDR] {
            if let Ok(id) = usb.i2c_read_reg(addr, CHIP_ID.reg)
                && id == R82XX_CHIP_ID
            {
                return Some(addr);
            }
        }
        None
    }

    // ---- register access -------------------------------------------------

    /// Write registers starting at `reg`, going through the cache (the RW
    /// registers are write-only; the cache is what read-modify-write uses).
    fn write_regs(&mut self, usb: &dyn TunerBus, reg: Reg, val: &[u8]) -> Result<()> {
        let at = (reg - FIRST_WRITABLE) as usize;
        debug_assert!(reg >= FIRST_WRITABLE && at + val.len() <= NUM_CACHE_REGS);
        self.regs[at..at + val.len()].copy_from_slice(val);
        // I²C messages carry the start register then data.
        let mut sent = 0;
        while sent < val.len() {
            let n = (val.len() - sent).min(MAX_I2C_MSG_LEN - 1);
            let mut buf = [0u8; MAX_I2C_MSG_LEN];
            buf[0] = reg + sent as u8;
            buf[1..1 + n].copy_from_slice(&val[sent..sent + n]);
            usb.i2c_write(self.i2c_addr, &buf[..1 + n])?;
            sent += n;
        }
        Ok(())
    }

    /// Write field values into their register: the bits they own replace
    /// the cached ones, the rest are rewritten as cached.
    fn write(&mut self, usb: &dyn TunerBus, bits: Bits) -> Result<()> {
        let cached = self.cached(bits.reg);
        let applied = (cached & !bits.mask) | bits.value;
        self.write_regs(usb, bits.reg, &[applied])
    }

    fn cached(&self, reg: Reg) -> u8 {
        self.regs[(reg - FIRST_WRITABLE) as usize]
    }

    /// Read `n` status registers starting at R0. The chip returns each
    /// byte bit-reversed.
    fn read_status(&self, usb: &dyn TunerBus, n: u16) -> Result<Vec<u8>> {
        usb.i2c_write(self.i2c_addr, &[R0])?;
        let mut data = usb.i2c_read(self.i2c_addr, n)?;
        for b in &mut data {
            *b = b.reverse_bits();
        }
        Ok(data)
    }

    // ---- init ------------------------------------------------------------

    pub fn init(&mut self, usb: &dyn TunerBus) -> Result<()> {
        self.last_input = None;
        let init = REG_INIT;
        self.write_regs(usb, FIRST_WRITABLE, &init)?;
        self.set_tv_standard(usb)?;
        self.sysfreq_sel(usb)?;
        self.init_done = true;
        Ok(())
    }

    /// SDR register plan (the DVB-T "digital TV, BW < 6 MHz" standard):
    /// 3.57 MHz low-IF, filter calibration at 56 MHz, image negative.
    fn set_tv_standard(&mut self, usb: &dyn TunerBus) -> Result<()> {
        const FILT_CAL_LO_KHZ: u32 = 56_000;
        // librtlsdr's hp_cor for this standard ("1.7m disable, +2cap,
        // 1.0mhz"): FILT_BW 11 and HPF 1011, the 1.7 MHz path off.
        const FILTER_BANDWIDTH: u8 = FILT_BW_NARROWEST;
        const HIGH_PASS_CORNER: u8 = 11;

        self.regs = REG_INIT;
        self.if_freq_hz = R82XX_IF_FREQ;

        // librtlsdr: "Init Flag & Xtal_check Result (inits VGA gain)".
        self.write(usb, VGA_CODE.set(0))?;
        self.write(usb, VERSION.set(VER_NUM))?;
        // librtlsdr: "for LT Gain test".
        self.write(usb, PDET1_GAIN.set(0))?;

        // IF filter calibration: run the PLL at 56 MHz, trigger cal, read
        // the code out of the status regs. Two attempts, like librtlsdr.
        for _ in 0..2 {
            self.write(usb, FILT_BW.set(FILTER_BANDWIDTH))?;
            self.write(usb, CALIBRATION_CLOCK.set(1))?;
            // No crystal cap for the PLL during calibration.
            self.write(usb, CAPX.set(0))?;
            self.set_pll(usb, FILT_CAL_LO_KHZ as u64 * 1000)?;
            self.write(usb, FILTER_CALIBRATION_TRIGGER.set(1))?;
            self.write(usb, FILTER_CALIBRATION_TRIGGER.set(0))?;
            self.write(usb, CALIBRATION_CLOCK.set(0))?;

            let data = self.read_status(usb, R4 as u16 + 1)?;
            self.fil_cal_code = FILTER_CALIBRATION_CODE.get(data[R4 as usize]);
            if self.fil_cal_code != FILTER_CALIBRATION_SATURATED {
                break;
            }
            self.fil_cal_code = FILT_CODE_WIDEST; // and retry
        }

        // librtlsdr's filt_q: "low q".
        self.write(usb, FILT_Q.set(1).with(FILT_CODE.set(self.fil_cal_code)))?;
        // Bandwidth, 1.7 MHz path, high-pass corner.
        self.write(
            usb,
            NARROW_BANDWIDTH_PATH.set(0).with(FILT_BW.set(FILTER_BANDWIDTH)).with(HPF.set(HIGH_PASS_CORNER)),
        )?;
        // librtlsdr's img_r: "image negative".
        self.write(usb, IMAGE_SIDE.set(0))?;
        // librtlsdr's filt_gain: FILT_3DB clear, V6MHz set.
        self.write(usb, FILT_3DB.set(0).with(V6MHZ.set(1)))?;
        // librtlsdr's ext_enable: "ext enable; ext at lna max-1".
        self.write(usb, FILTER_EXTENSION.set(1).with(FILTER_EXTENSION_AT_LNA_MAX_LESS_1.set(1)))?;
        // librtlsdr's loop_through 0x01 under mask 0x80 writes this bit 0:
        // loop-through ON (Table 1-3), whatever its "lt off" comment says.
        self.write(usb, LOOP_THROUGH_OFF.set(0))?;
        self.write(usb, LOOP_THROUGH_ATTENUATION.set(0))?;
        self.write(usb, FILTER_EXTENSION_WIDEST.set(0))?;
        self.write(usb, RF_POLY_FILTER_CURRENT.set(RF_POLY_FILTER_CURRENT_MIN))?;
        Ok(())
    }

    /// AGC/detector operating point for the SDR use case (librtlsdr's
    /// digital-TV values; the frequency-specific tweaks in TV drivers
    /// never apply here).
    fn sysfreq_sel(&mut self, usb: &dyn TunerBus) -> Result<()> {
        // librtlsdr's lna_top 0xe5 ("detect bw 3, lna top:4, predet
        // top:2") under mask 0xc7, and mixer_top 0x24 ("mixer top:13,
        // top-1, low-discharge") under 0xf8.
        const DETECT_BANDWIDTH_3: u8 = 3;
        const PREDETECT_TOP: u8 = 5;
        const MIXER_TOP: u8 = 2;
        /// "lna vth 0.84, vtl 0.64" and "mixer vth 1.04, vtl 0.84":
        /// 0.34 V + 0.1 V per step.
        const LNA_VTH: (u8, u8) = (5, 3);
        const MIXER_VTH: (u8, u8) = (7, 5);
        const LNA_TOP_LOWEST: u8 = 0;
        const LNA_TOP_3: u8 = 3;
        const LNA_DISCHARGE: u8 = 14;

        self.write(usb, DETECT_BANDWIDTH.set(DETECT_BANDWIDTH_3).with(PDET2_GAIN.set(PREDETECT_TOP)))?;
        self.write(usb, PDET3_GAIN.set(MIXER_TOP).with(R28_BIT3.set(0)))?;
        self.write(usb, LNA_VTH_HIGH.set(LNA_VTH.0).with(LNA_VTH_LOW.set(LNA_VTH.1)))?;
        self.write(usb, MIX_VTH_HIGH.set(MIXER_VTH.0).with(MIX_VTH_LOW.set(MIXER_VTH.1)))?;
        // Air-in / cable-in defaults (both off; band switching owns these
        // on the R828D).
        self.write(usb, CABLE1_INPUT.set(0).with(LNA1_OFF.set(0)))?;
        self.write(usb, CABLE2_INPUT.set(0))?;
        self.write(usb, CHARGE_PUMP_CURRENT.set(CHARGE_PUMP_AUTO))?;
        self.write(usb, DIVIDER_BUFFER_CURRENT.set(DIVIDER_BUFFER_150UA))?;
        self.write(usb, PW_FILT.set(PW_FILT_LOW))?;

        // LNA TOP lowest, normal discharge, PRE_DECT off, AGC clock 250 Hz.
        self.write(usb, PDET1_GAIN.set(LNA_TOP_LOWEST))?;
        self.write(usb, DISCHARGE_MODE.set(0))?;
        self.write(usb, PDET3_ON.set(0))?;
        self.write(usb, AGC_CLOCK.set(AGC_CLOCK_250HZ))?;
        // LNA TOP 3, discharge mode (librtlsdr takes mixer_top's bit 2
        // here, "the mask here is wrong, but it matches the original
        // driver"), discharge current, AGC clock 60 Hz.
        self.write(usb, PDET1_GAIN.set(LNA_TOP_3))?;
        self.write(usb, DISCHARGE_MODE.set(1))?;
        self.write(usb, LNA_DISCHARGE_CURRENT.set(LNA_DISCHARGE))?;
        self.write(usb, AGC_CLOCK.set(AGC_CLOCK_60HZ))?;
        Ok(())
    }

    // ---- tuning ----------------------------------------------------------

    /// Tune to `freq_hz` (RF, before IF offset). Returns the *actual*
    /// tuned frequency derived from the PLL registers written.
    pub fn set_freq(&mut self, usb: &dyn TunerBus, freq_hz: u32) -> Result<f64> {
        // Blog V4 reaches HF through its 28.8 MHz upconverter.
        self.upconvert_hz = match self.variant {
            R82xxVariant::R828D { blog_v4: true } if freq_hz < BLOG_V4_UPCONVERT_HZ => {
                BLOG_V4_UPCONVERT_HZ
            }
            _ => 0,
        };
        let rf = freq_hz + self.upconvert_hz;
        let lo = rf as u64 + self.if_freq_hz as u64;

        self.set_mux(usb, lo)?;
        let actual_lo = self.set_pll(usb, lo)?;

        if let R82xxVariant::R828D { blog_v4 } = self.variant {
            if blog_v4 {
                self.blog_v4_band_switch(usb, freq_hz)?;
            } else {
                let input = if freq_hz <= R828D_INPUT_SWITCH_HZ {
                    InputSel::Cable1
                } else {
                    InputSel::Air
                };
                if self.last_input != Some(input) {
                    // Cable 1 in, the air LNA off; or both back.
                    let cable1 = u8::from(input == InputSel::Cable1);
                    self.write(usb, CABLE1_INPUT.set(cable1).with(LNA1_OFF.set(cable1)))?;
                    self.last_input = Some(input);
                }
            }
        }

        // What actually landed on the wanted channel: LO minus IF, minus
        // the upconverter shift.
        Ok(actual_lo - self.if_freq_hz as f64 - self.upconvert_hz as f64)
    }

    /// Blog V4 front-end steering: notch filters off inside their bands,
    /// input select HF→cable2 / VHF→cable1 / UHF→air. Thresholds are the
    /// dongle's documented behavior.
    fn blog_v4_band_switch(&mut self, usb: &dyn TunerBus, freq_hz: u32) -> Result<()> {
        let notch_off = matches!(
            freq_hz,
            0..=2_200_000 | 85_000_000..=112_000_000 | 172_000_000..=242_000_000
        );
        // The V4 drives its notch filters from the tuner's open-drain pin.
        self.write(usb, OPEN_D.set(u8::from(!notch_off)))?;

        let input = match freq_hz {
            0..=28_800_000 => InputSel::Cable2,
            28_800_001..=250_000_000 => InputSel::Cable1,
            _ => InputSel::Air,
        };
        if self.last_input != Some(input) {
            let (cable2, cable1, air_off) = match input {
                InputSel::Cable2 => (1, 0, 1),
                InputSel::Cable1 => (0, 1, 1),
                InputSel::Air => (0, 0, 0),
            };
            self.write(usb, CABLE2_INPUT.set(cable2))?;
            self.write(usb, CABLE1_INPUT.set(cable1))?;
            self.write(usb, LNA1_OFF.set(air_off))?;
            self.last_input = Some(input);
        }
        Ok(())
    }

    /// Per-band front-end mux/tracking-filter settings.
    fn set_mux(&mut self, usb: &dyn TunerBus, lo_hz: u64) -> Result<()> {
        let mhz = (lo_hz / 1_000_000) as u32;
        let range = FREQ_RANGES
            .iter()
            .take_while(|r| r.mhz <= mhz)
            .last()
            .unwrap_or(&FREQ_RANGES[0]);

        self.write(usb, OPEN_D.set(range.open_d))?;
        self.write(usb, RFMUX.set(range.rf_mux).with(RFFILT.set(range.rf_filter)))?;
        self.write(usb, TF_NCH.set(range.notch).with(TF_LP.set(range.low_pass)))?;
        // Crystal cap: none (librtlsdr's choice for the R820T without an
        // xtal check).
        self.write(usb, XTAL_CAP_EXTRA.set(0).with(CAPX.set(0)))?;
        self.write(usb, IMR_G.set(0))?;
        self.write(usb, IMR_P.set(0))?;
        Ok(())
    }

    /// Program the fractional-N PLL for `lo_hz` and return the exact LO
    /// frequency the written registers synthesize.
    ///
    /// Hardware model: f_vco = 2·f_ref·(Nint + SDM/65536), f_lo = f_vco/D
    /// with D = mix_div ∈ {2,4,…,64} chosen so f_vco lands in
    /// [1.77, 3.54] GHz. Nint = 4·Ni2c + Si2c + 13 (Table 1-3, R20).
    fn set_pll(&mut self, usb: &dyn TunerBus, lo_hz: u64) -> Result<f64> {
        /// Nint's offset in its R20 encoding.
        const NINT_OFFSET: i32 = 13;
        /// librtlsdr's VCO current while hunting, and its "Didn't lock.
        /// Increase VCO current" retry.
        const VCO_CURRENT_HUNT: u8 = 4;
        const VCO_CURRENT_RETRY: u8 = 3;
        let ref_hz = self.xtal_hz as u64;

        // Reference divider off; PLL autotune 128 kHz while hunting.
        self.write(usb, REF_DIV2.set(0))?;
        self.write(usb, PLL_AUTO_CLK.set(PLL_AUTO_CLK_128KHZ))?;
        self.write(usb, VCO_CURRENT.set(VCO_CURRENT_HUNT))?;

        // Pick the mixer divider so the VCO lands in range.
        const VCO_MIN_HZ: u64 = 1_770_000_000;
        const VCO_MAX_HZ: u64 = 2 * VCO_MIN_HZ;
        let mut mix_div: u32 = 2;
        let mut div_num: u8 = 0;
        while mix_div <= 64 {
            let vco = lo_hz * mix_div as u64;
            if (VCO_MIN_HZ..VCO_MAX_HZ).contains(&vco) {
                break;
            }
            mix_div <<= 1;
            div_num += 1;
        }
        if mix_div > 64 {
            return Err(Error::Device {
                backend: "rtl",
                context: "r82xx set_pll",
                message: format!("no valid mixer divider for {lo_hz} Hz"),
            });
        }

        // The chip's own VCO band estimate nudges the divider register
        // (not the math — librtlsdr behavior, kept bit-identical).
        let vco_power_ref: u8 = match self.variant {
            R82xxVariant::R828D { .. } => 1,
            R82xxVariant::R820T => 2,
        };
        let status = self.read_status(usb, R4 as u16 + 1)?;
        let vco_fine_tune = VCO_FINE_TUNE.get(status[R4 as usize]);
        let reg_div_num = if vco_fine_tune > vco_power_ref {
            div_num.saturating_sub(1)
        } else if vco_fine_tune < vco_power_ref {
            div_num + 1
        } else {
            div_num
        };
        self.write(usb, SEL_DIV.set(reg_div_num))?;

        // Integer and fractional parts.
        let vco_hz = lo_hz * mix_div as u64;
        let nint = (vco_hz / (2 * ref_hz)) as u32;
        let frac_hz = vco_hz - 2 * ref_hz * nint as u64;
        let max_nint = (128 / vco_power_ref as u32) - 1;
        if nint > max_nint {
            return Err(Error::Device {
                backend: "rtl",
                context: "r82xx set_pll",
                message: format!("Nint {nint} out of range for {lo_hz} Hz"),
            });
        }

        // SDM word: frac/(2·ref) in 1/65536 steps, rounded to nearest.
        let mut nint = nint;
        let mut sdm = ((frac_hz * 65536 + ref_hz) / (2 * ref_hz)) as u32;
        if sdm >= 65536 {
            nint += 1;
            sdm = 0;
        }

        let ni = ((nint as i32 - NINT_OFFSET).div_euclid(4)) as u8;
        let si = (nint as i32 - NINT_OFFSET - 4 * ni as i32) as u8;
        self.write(usb, SI2C.set(si).with(NI2C.set(ni)))?;

        // The fractional divider powered only when there is a fraction.
        self.write(usb, PW_SDM.set(u8::from(sdm == 0)))?;
        let [sdm_high, sdm_low] = (sdm as u16).to_be_bytes();
        self.write(usb, SDM_IN_HIGH.set(sdm_high))?;
        self.write(usb, SDM_IN_LOW.set(sdm_low))?;

        // Wait for lock; one retry with more VCO current.
        let mut locked = false;
        for attempt in 0..2 {
            let data = self.read_status(usb, R2 as u16 + 1)?;
            if PLL_LOCKED.get(data[R2 as usize]) == 1 {
                locked = true;
                break;
            }
            if attempt == 0 {
                self.write(usb, VCO_CURRENT.set(VCO_CURRENT_RETRY))?;
            }
        }
        if locked {
            // Autotune down to 8 kHz once settled.
            self.write(usb, PLL_AUTO_CLK.set(PLL_AUTO_CLK_8KHZ))?;
        }

        let actual_lo = 2.0 * ref_hz as f64 * (nint as f64 + sdm as f64 / 65536.0) / mix_div as f64;
        self.pll = Some(PllState {
            nint,
            sdm,
            mix_div,
            actual_lo_hz: actual_lo,
            locked,
        });
        Ok(actual_lo)
    }

    /// True hardware readback of the PLL lock bit.
    pub fn pll_locked(&self, usb: &dyn TunerBus) -> Result<bool> {
        let data = self.read_status(usb, R2 as u16 + 1)?;
        Ok(PLL_LOCKED.get(data[R2 as usize]) == 1)
    }

    // ---- bandwidth ---------------------------------------------------------

    /// Set the IF filter for `bw_hz` and return the low-IF the demod must
    /// be programmed with afterwards (`int_freq` shifts with the filter).
    pub fn set_bandwidth(&mut self, usb: &dyn TunerBus, bw_hz: u32) -> Result<u32> {
        const FILT_HP_BW1: i32 = 350_000;
        const FILT_HP_BW2: i32 = 380_000;
        const LP_TABLE: [i32; 10] = [
            1_700_000, 1_600_000, 1_550_000, 1_450_000, 1_200_000, 900_000, 700_000, 550_000,
            450_000, 350_000,
        ];
        /// `FILT_BW` bit for each high-pass stage the bandwidth drops.
        const HP_BW2_DROPPED: u8 = 0b01;
        const HP_BW1_DROPPED: u8 = 0b10;
        /// `HPF`, counted down from its lowest corner.
        const HPF_LOWEST: u8 = 15;

        // librtlsdr's (reg_0a, reg_0b) per band, as fields.
        struct Filter {
            filt_q: u8,
            narrow: u8,
            filt_bw: u8,
            hpf: u8,
        }
        let mut bw = bw_hz as i32;
        let filter = if bw > 7_000_000 {
            self.if_freq_hz = 4_570_000;
            Filter { filt_q: 1, narrow: 0, filt_bw: FILT_BW_WIDEST, hpf: 11 }
        } else if bw > 6_000_000 {
            self.if_freq_hz = 4_570_000;
            Filter { filt_q: 1, narrow: 0, filt_bw: FILT_BW_MIDDLE, hpf: 10 }
        } else if bw > LP_TABLE[0] + FILT_HP_BW1 + FILT_HP_BW2 {
            self.if_freq_hz = 3_570_000;
            Filter { filt_q: 1, narrow: 0, filt_bw: FILT_BW_NARROWEST, hpf: 11 }
        } else {
            let mut if_hz: i32 = 2_300_000;
            let mut filt_bw = 0;
            let mut real_bw: i32 = 0;

            if bw > LP_TABLE[0] + FILT_HP_BW1 {
                bw -= FILT_HP_BW2;
                if_hz += FILT_HP_BW2;
                real_bw += FILT_HP_BW2;
            } else {
                filt_bw |= HP_BW2_DROPPED;
            }
            if bw > LP_TABLE[0] {
                bw -= FILT_HP_BW1;
                if_hz += FILT_HP_BW1;
                real_bw += FILT_HP_BW1;
            } else {
                filt_bw |= HP_BW1_DROPPED;
            }

            // Narrowest low-pass that still passes `bw`.
            let mut lp_idx = 0;
            for (i, cutoff) in LP_TABLE.iter().enumerate() {
                if bw > *cutoff {
                    break;
                }
                lp_idx = i;
            }
            real_bw += LP_TABLE[lp_idx];
            if_hz -= real_bw / 2;
            self.if_freq_hz = if_hz as u32;
            Filter { filt_q: 0, narrow: 1, filt_bw, hpf: HPF_LOWEST - lp_idx as u8 }
        };

        self.write(usb, FILT_Q.set(filter.filt_q))?;
        self.write(
            usb,
            NARROW_BANDWIDTH_PATH.set(filter.narrow).with(FILT_BW.set(filter.filt_bw)).with(HPF.set(filter.hpf)),
        )?;
        Ok(self.if_freq_hz)
    }

    // ---- gain --------------------------------------------------------------

    /// Hardware AGC for LNA and mixer, fixed mid VGA.
    pub fn set_gain_auto(&mut self, usb: &dyn TunerBus) -> Result<()> {
        self.write(usb, LNA_GAIN_MANUAL.set(0))?;
        self.write(usb, MIXER_GAIN_AUTO.set(1))?;
        self.write(usb, vga_code(VGA_CODE_AUTO_GAIN))?;
        Ok(())
    }

    /// Manual overall gain in tenths of dB, walked across LNA and mixer
    /// steps the way librtlsdr does; VGA fixed at 16.3 dB. Returns the
    /// snapped total (tenths of dB, tuner stages only).
    pub fn set_gain_manual(&mut self, usb: &dyn TunerBus, tenth_db: i32) -> Result<i32> {
        self.write(usb, LNA_GAIN_MANUAL.set(1))?;
        self.write(usb, MIXER_GAIN_AUTO.set(0))?;
        self.write(usb, vga_code(VGA_CODE_MANUAL_GAIN))?;

        // Snap to the nearest LNA/mixer step (pure, unit-tested) and
        // return the tuner total actually applied — the scale
        // `Rtl::gain` reads back, so set→read round-trips.
        let (lna_idx, mix_idx, total) = snap_tuner_gain(tenth_db);
        self.write(usb, LNA_GAIN.set(lna_idx))?;
        self.write(usb, MIX_GAIN.set(mix_idx))?;
        Ok(total)
    }

    /// Set the three stages independently by index (LNA 0–15, mixer 0–15,
    /// VGA 0–15). Returns the resulting per-stage gains in tenths of dB.
    pub fn set_gain_stages(
        &mut self,
        usb: &dyn TunerBus,
        lna_idx: u8,
        mix_idx: u8,
        vga_idx: u8,
    ) -> Result<(i32, i32, i32)> {
        self.write(usb, LNA_GAIN_MANUAL.set(1))?;
        self.write(usb, MIXER_GAIN_AUTO.set(0))?;
        self.write(usb, LNA_GAIN.set(lna_idx))?;
        self.write(usb, MIX_GAIN.set(mix_idx))?;
        self.write(usb, vga_code(vga_idx))?;
        Ok((
            cumulative(&LNA_STEPS, lna_idx as usize),
            cumulative(&MIXER_STEPS, mix_idx as usize),
            VGA_BASE_TENTH_DB + cumulative(&VGA_STEPS, vga_idx as usize),
        ))
    }

    /// The per-stage INDICES in effect (LNA, mixer, VGA). A stage under
    /// manual control is at the index written to it (the register
    /// cache); only a stage the AGC drives is read from the status
    /// register's indicators — measured on a dongle: under
    /// manual gain R3 read LNA 15 / mixer 15 whatever was written
    /// (8/8, 3/12, 15/0), so it tracks the AGC, not the gain in effect.
    /// The VGA is write-only: always the cache.
    pub fn read_gain_indices(&self, usb: &dyn TunerBus) -> Result<(u8, u8, u8)> {
        let lna_manual = LNA_GAIN_MANUAL.get(self.cached(LNA_GAIN_MANUAL.reg)) == 1;
        let mixer_manual = MIXER_GAIN_AUTO.get(self.cached(MIXER_GAIN_AUTO.reg)) == 0;
        let status = if lna_manual && mixer_manual { None } else { Some(self.read_status(usb, R3 as u16 + 1)?[R3 as usize]) };
        let lna = match status {
            Some(r3) if !lna_manual => LNA_GAIN_INDICATOR.get(r3),
            _ => LNA_GAIN.get(self.cached(LNA_GAIN.reg)),
        };
        let mixer = match status {
            Some(r3) if !mixer_manual => MIXER_GAIN_INDICATOR.get(r3),
            _ => MIX_GAIN.get(self.cached(MIX_GAIN.reg)),
        };
        Ok((lna, mixer, VGA_CODE.get(self.cached(VGA_CODE.reg))))
    }

    /// Each stage's 16 levels in tenths of dB, index 0 first (the
    /// cumulative step tables; VGA from its base).
    pub fn stage_levels_tenth_db() -> [[i32; 16]; 3] {
        let ladder = |steps: &[i32; 16], base: i32| core::array::from_fn(|i| base + cumulative(steps, i));
        [ladder(&LNA_STEPS, 0), ladder(&MIXER_STEPS, 0), ladder(&VGA_STEPS, VGA_BASE_TENTH_DB)]
    }

    /// Live per-stage gains in tenths of dB, from the indices in effect
    /// ([`R82xx::read_gain_indices`]).
    pub fn read_gains(&self, usb: &dyn TunerBus) -> Result<(i32, i32, i32)> {
        let (lna, mixer, vga) = self.read_gain_indices(usb)?;
        Ok((
            cumulative(&LNA_STEPS, lna as usize),
            cumulative(&MIXER_STEPS, mixer as usize),
            VGA_BASE_TENTH_DB + cumulative(&VGA_STEPS, vga as usize),
        ))
    }

    /// Standby sequence (used when switching to direct sampling):
    /// librtlsdr's `r82xx_standby`, register by register.
    pub fn standby(&mut self, usb: &dyn TunerBus) -> Result<()> {
        if !self.init_done {
            return Ok(());
        }
        let standby: [Bits; 11] = [
            PDET1_OFF.set(1).with(PDET3_ON.set(0)).with(FILT_3DB.set(1)).with(V6MHZ.set(1)).with(CABLE2_INPUT.set(0)).with(PW_LNA.set(1)),
            LOOP_THROUGH_OFF.set(1).with(CABLE1_INPUT.set(0)).with(LNA1_OFF.set(1)).with(LNA_GAIN_MANUAL.set(0)).with(LNA_GAIN.set(0)),
            IMAGE_SIDE.set(0).with(MIXER_ON.set(0)).with(PW0_MIX.set(1)).with(MIXER_GAIN_AUTO.set(1)).with(MIX_GAIN.set(10)),
            MIXER_BUFFER_ON.set(0).with(PW0_AMP.set(1)).with(IMR_G.set(0)),
            IF_FILTER_OFF.set(1).with(PW1_IFFILT.set(1)).with(IMR_P.set(0)),
            CHANNEL_FILTER_ON.set(0).with(PW_FILT.set(1)).with(FILT_Q.set(1)).with(FILT_CODE.set(6)),
            R12_BIT7.set(0).with(VGA_ON.set(0)).with(R12_BIT5.set(1)).with(VGA_PIN_CONTROL.set(1)).with(VGA_CODE.set(5)),
            R15_STANDARD,
            PW_LDO_A.set(0).with(CHARGE_PUMP_CURRENT.set(0)).with(R17_LOW_BITS.set(3)),
            PW_LDO_D.set(3).with(DIVIDER_BUFFER_CURRENT.set(DIVIDER_BUFFER_150UA)).with(OPEN_D.set(0)).with(R23_LOW_BITS.set(4)),
            RF_FILTER_ON.set(0).with(RF_POLY_FILTER_CURRENT.set(0)).with(SW_AGC.set(0)).with(R25_LOW_BITS.set(12)),
        ];
        for bits in standby {
            self.write_regs(usb, bits.reg, &[bits.byte()])?;
        }
        Ok(())
    }
}

/// The IF VGA set to `code` under gain control (librtlsdr's mask 0x9f:
/// the pin-control bit and R12 bit 7 cleared with it).
fn vga_code(code: u8) -> Bits {
    R12_BIT7.set(0).with(VGA_PIN_CONTROL.set(0)).with(VGA_CODE.set(code))
}

fn cumulative(steps: &[i32; 16], idx: usize) -> i32 {
    steps[..=idx.min(15)].iter().sum()
}

/// Snap a requested tuner gain (LNA+mixer, tenths of dB) to the
/// NEAREST achievable point on the R820T2's gain ladder, returning
/// `(lna_idx, mixer_idx, snapped_tenths)`. The ladder is librtlsdr's:
/// walk the LNA and mixer index sequences alternately, each step
/// adding its stage's increment. Two things this gets right that the
/// old inline loop did not: it snaps to the NEAREST step, not the
/// first one at-or-past the target (a requested 31.0 dB used to become
/// 32.8 — always biased high by up to a full step), and it tracks the
/// best match across the whole walk, so the tables' non-monotonic tail
/// (mixer index 15 is −0.8 dB) can't strand the pick above the true
/// maximum. VGA is a separate fixed baseband stage and is NOT part of
/// this scale — see `Rtl::set_gain`.
fn snap_tuner_gain(target_tenth: i32) -> (u8, u8, i32) {
    let (mut lna, mut mix, mut total) = (0usize, 0usize, 0i32);
    let (mut best_lna, mut best_mix, mut best_total) = (0usize, 0usize, 0i32);
    // 30 steps covers both index ladders (LNA 1..=15, mixer 1..=15).
    for step in 0..30 {
        if (total - target_tenth).abs() < (best_total - target_tenth).abs() {
            (best_lna, best_mix, best_total) = (lna, mix, total);
        }
        if total >= target_tenth {
            break; // past the target and monotonic here — nearest is found
        }
        if step % 2 == 0 {
            if lna >= 15 {
                break;
            }
            lna += 1;
            total += LNA_STEPS[lna];
        } else {
            if mix >= 15 {
                break;
            }
            mix += 1;
            total += MIXER_STEPS[mix];
        }
    }
    (best_lna as u8, best_mix as u8, best_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fmt::Write as _;

    /// A tuner bus that records every transaction and answers status
    /// reads from a script (given as the tuner's registers 0..=4, which
    /// it sends bit-reversed, as the chip does).
    struct Recorder {
        status: [u8; 5],
        log: RefCell<String>,
    }

    impl TunerBus for Recorder {
        fn i2c_write(&self, i2c_addr: u16, buf: &[u8]) -> Result<()> {
            let _ = writeln!(self.log.borrow_mut(), "W {i2c_addr:02x} {buf:02x?}");
            Ok(())
        }
        fn i2c_read(&self, i2c_addr: u16, len: u16) -> Result<Vec<u8>> {
            let _ = writeln!(self.log.borrow_mut(), "R {i2c_addr:02x} {len}");
            Ok(self.status[..len as usize].iter().map(|&b| b.reverse_bits()).collect())
        }
        fn i2c_read_reg(&self, i2c_addr: u16, reg: u8) -> Result<u8> {
            let _ = writeln!(self.log.borrow_mut(), "RR {i2c_addr:02x} {reg:02x}");
            Ok(self.status[reg as usize])
        }
    }

    /// Every entry point, on every variant, under two status scripts —
    /// the PLL locks and the filter calibrates, or neither does — as one
    /// transcript of writes, reads and returned values.
    fn transcript() -> String {
        let mut out = String::new();
        for status in [[0x69, 0x00, 0x40, 0x5a, 0x25], [0x69, 0x00, 0x00, 0x31, 0x1f]] {
            for (variant, xtal) in [
                (R82xxVariant::R820T, 28_800_000),
                (R82xxVariant::R828D { blog_v4: false }, R828D_XTAL_FREQ),
                (R82xxVariant::R828D { blog_v4: true }, 28_800_000),
            ] {
                let bus = Recorder { status, log: RefCell::new(String::new()) };
                let mut t = R82xx::new(variant, xtal);
                let mut r = String::new();
                let _ = writeln!(r, "probe {:?}", R82xx::probe_bus(&bus));
                let _ = writeln!(r, "init {:?}", t.init(&bus).is_ok());
                for bw in [300_000, 1_000_000, 2_400_000, 2_500_000, 6_500_000, 8_000_000] {
                    let _ = writeln!(r, "bw {bw} {:?}", t.set_bandwidth(&bus, bw).ok());
                }
                for f in [
                    1_000_000u32, 25_000_000, 52_000_000, 100_000_000, 162_550_000, 300_000_000,
                    460_512_500, 600_000_000, 852_675_000, 1_700_000_000,
                ] {
                    let _ = writeln!(r, "f {f} {:?} {:?}", t.set_freq(&bus, f).ok(), t.pll.map(|p| (p.nint, p.sdm, p.mix_div, p.locked)));
                }
                let _ = writeln!(r, "auto {:?}", t.set_gain_auto(&bus).is_ok());
                for g in [0, 150, 310, 496] {
                    let _ = writeln!(r, "gain {g} {:?}", t.set_gain_manual(&bus, g).ok());
                }
                let _ = writeln!(r, "stages {:?}", t.set_gain_stages(&bus, 5, 7, 9).ok());
                let _ = writeln!(r, "read {:?}", t.read_gains(&bus).ok());
                let _ = writeln!(r, "lock {:?}", t.pll_locked(&bus).ok());
                let _ = writeln!(r, "standby {:?}", t.standby(&bus).is_ok());
                let _ = writeln!(out, "== {variant:?} {xtal}\n{}{r}", bus.log.borrow());
            }
        }
        out
    }

    fn fnv1a(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
    }

    /// The register traffic is pinned: renaming values, restructuring
    /// field writes or re-deriving a table must reproduce it byte for
    /// byte. (Re-pinned only for deliberate traffic changes: the redundant
    /// R16 bit-2 write leaving sysfreq_sel — six lines fewer; a manual-gain
    /// read-back no longer reading R3, whose indicators track the AGC —
    /// each of the six reads after `stages` now answers the written
    /// 113/88/195 with no status read, twelve lines fewer.)
    #[test]
    fn register_traffic_is_pinned() {
        let t = transcript();
        assert_eq!((t.lines().count(), fnv1a(&t)), (2095, 11698129531884488284), "{t}");
    }

    // Ladder totals (tenths of dB) from walking LNA/mixer alternately —
    // the reference points the nearest-snap is judged against.
    // (0,0)=0 (7,7)=254 (8,8)=297 (9,8)=328 …

    #[test]
    fn snap_hits_exact_ladder_points() {
        assert_eq!(snap_tuner_gain(0), (0, 0, 0));
        assert_eq!(snap_tuner_gain(254), (7, 7, 254)); // exact rung
        assert_eq!(snap_tuner_gain(144), (4, 4, 144)); // exact rung
    }

    #[test]
    fn snap_picks_nearest_not_first_past() {
        // 31.0 dB sits between rungs 297 (29.7) and 328 (32.8); 297 is
        // nearer (13 vs 18 tenths). The OLD walk returned 328 — the
        // systematic overshoot this fix removes.
        assert_eq!(snap_tuner_gain(310).2, 297);
        // 32.0 dB is nearer 328 (8 vs 23) — snaps up, correctly.
        assert_eq!(snap_tuner_gain(320).2, 328);
    }

    #[test]
    fn snap_is_monotonic_and_bounded() {
        // Rising requests never lower the applied gain, and nothing
        // exceeds the ladder's true max (mixer index 15 is −0.8 dB, so
        // a naive last-step pick could strand below it — best-tracking
        // must not).
        let mut prev = -1;
        let mut max_seen = 0;
        for t in (0..=520).step_by(5) {
            let (lna, mix, total) = snap_tuner_gain(t);
            assert!(lna <= 15 && mix <= 15, "index out of range at {t}");
            assert!(total >= prev, "non-monotonic at target {t}: {total} < {prev}");
            prev = total;
            max_seen = max_seen.max(total);
        }
        // The achievable max is LNA(15) + mixer(14) = 496 tenths.
        assert_eq!(max_seen, 496, "ladder max should be 49.6 dB");
    }
}
