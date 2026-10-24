//! The R820T-family tuner from the documents alone.
//!
//! - The register image for R5-R31: every cell [R820T2] Table 1-2 (p. 5)
//!   fixes is written as printed; every other field is a choice from its
//!   Table 1-3 meaning (pp. 6-11), with the reason beside it in
//!   [`image`]. The document gives no defaults, so each choice is also
//!   listed in [`IMAGE_NOTES`] for the records.
//! - PLL programming by the documented formula ([R820T2] pp. 9-10):
//!   Nint = 4*Ni2c + Si2c + 13, Nfra = SDM_IN[16:1] / 2^16,
//!   Ndiv = (Nint + Nfra) * 2, f_ref = f_xtal or f_xtal / 2 (REF_DIV2),
//!   the mixer input = VCO / {2, 4, 8, 1} by SEL_DIV. The document never
//!   states f_VCO = f_ref * Ndiv; that relation is the hypothesis the PLL
//!   sweep tests, and the VCO range and the divider rule come from it
//!   (data/step4-pll-sweep.jsonl).
//! - The I2C paths step 2 established (data/step2-tuner-access.json):
//!   writes at wValue (reg << 8) | 0x34 through the tuner block, reads
//!   from register 0 at wValue 0x34, each byte bit-reversed ([R820T]
//!   p. 17, LSB first).
//! - The demodulator in IF mode ([RTL2832U] 9.1, Table 4): the tuner's
//!   output is a low IF ([R820T] 4.3, Figure D), so AD_EN_reg1 = 1,
//!   AD_EN_reg = 0, en_bbin = 0, pset_iffreq for the IF (9.3), Spec_inv
//!   for a high-side LO (9.3: "the tuner is high-side mixing and the
//!   spectrum is inversed").

use std::time::Duration;

use crate::json::{J, hex_bytes, hex8};
use crate::regs::*;
use crate::usb::{self, Rtl};

/// Nint = 4*Ni2c + Si2c + 13 with Ni2c six bits and Si2c two ([R820T2] p. 9).
pub const NINT_MIN: u16 = 13;
pub const NINT_MAX: u16 = 13 + 4 * 63 + 3;
/// SDM_IN[16:1]: sixteen fractional bits (R21 = [8:1], R22 = [16:9]).
pub const SDM_SCALE: f64 = 65536.0;

/// SEL_DIV (R16[7:5]) to the mixer divider ([R820T2] p. 9): 000 /2,
/// 001 /4, 010 /8, 011 /1. Codes 100-111 are undescribed.
pub fn divider(sel_div: u8) -> Option<u32> {
    match sel_div {
        0b000 => Some(2),
        0b001 => Some(4),
        0b010 => Some(8),
        0b011 => Some(1),
        _ => None,
    }
}

/// The undescribed codes' dividers, measured hypothesis-free by `harness
/// lo-find` (data/step4-lo-find-sel100.jsonl, -sel101.jsonl: with the
/// PLL at Nint 88, f_VCO 2.534 GHz, the receiver answered a tone at
/// 71-77 MHz for code 100 and 29-38 MHz for code 101, i.e. an IF below
/// LOs of 79.2 and 39.6 MHz = f_VCO / 32 and / 64). Codes 110 and 111
/// answered only at 623-632 MHz (f_VCO / 4) at that Nint and not along
/// the Nint range in the sweep: not dividers the driver can use. Code 011
/// divides by 16, not the document's /1 (findings 10.5,
/// data/step10-5-seldiv.json).
pub const MEASURED_DIVIDERS: [(u8, u32); 3] = [(0b011, 16), (0b100, 32), (0b101, 64)];

/// A code's divider: the measured one where the bench has measured it
/// (it overrides the document), else the documented one.
pub fn divider_any(sel_div: u8) -> Option<u32> {
    MEASURED_DIVIDERS
        .iter()
        .find(|(c, _)| *c == sel_div)
        .map(|(_, d)| *d)
        .or_else(|| divider(sel_div))
}

/// The divider values the PLL sweep tries for an undescribed SEL_DIV code:
/// the four documented ones and the documented doubling continued twice.
/// A declared hypothesis set, not a known answer.
pub const UNDESCRIBED_DIV_HYPOTHESES: [u32; 6] = [1, 2, 4, 8, 16, 32];

/// REF_DIV2 (R16[4]): 0 -> f_ref = f_xtal; 1 -> f_xtal / 2 "for Xtal >
/// 24 MHz" ([R820T2] p. 9). The crystal is 28.8 MHz, so the document's
/// rule gives 1; the sweep can test 0 as well.
pub const REF_DIV2_BY_DOCUMENT: bool = true;

pub fn f_ref(ref_div2: bool) -> f64 {
    if ref_div2 { XTAL_HZ / 2.0 } else { XTAL_HZ }
}

/// The VCO frequency range, (low, high) in Hz, measured by `harness
/// pll-sweep` (data/step4-pll-sweep.jsonl, Nint in steps of 5, Nfra 0,
/// REF_DIV2 = 1): the tone is received at the hypothesised LO for Nint
/// 58..=128 with SEL_DIV 010 (/8) and 63..=128 with 001 (/4), Nint 58
/// marginal with /4, nothing at 53 or 133; f_VCO = 28.8 MHz * Nint gives
/// 1.670-3.686 GHz received, 1.526 and 3.830 GHz not. The stated range
/// keeps 30 MHz inside the received bounds. R2 VCO_INDICATOR reads 70-127
/// across that span and 0 or 63 outside it.
pub const VCO_RANGE_HZ: Option<(f64, f64)> = Some((1.70e9, 3.65e9));

/// The documented IFs ([R820T] Table 4-1, p. 20) plus the demodulator's
/// own power-on pset_iffreq (0x35D75D = -665763, data/step1-writes.json:
/// 665763 / 2^22 * 28.8 MHz = 4.571428 MHz = 32/7 MHz), each with the
/// standard it is printed for.
pub const IF_CANDIDATES_HZ: [(&str, f64); 6] = [
    ("DVB-T 6 MHz (Table 4-1 row 2)", 3.57e6),
    ("ATSC / ISDB-T 6 MHz (rows 1, 7)", 4.063e6),
    ("DVB-T 7 MHz (row 3)", 4.07e6),
    ("DVB-T 7/8 MHz, DTMB (rows 4, 5, 8)", 4.57e6),
    (
        "the demodulator's power-on pset_iffreq, 32/7 MHz",
        32.0e6 / 7.0,
    ),
    ("zero IF (9.3 example)", 0.0),
];
/// The IF used unless a step says otherwise: 4.57 MHz, the IF printed
/// for the 8 MHz channel ([R820T] Table 4-1 row 5), paired with the
/// demodulator's 8 MHz resampler ratio (Table 7's reset, 64/7 MHz),
/// the widest documented passband.
pub const IF_HZ: f64 = 4.57e6;
/// rsamp_ratio for the 8 MHz bandwidth, f_symbol = 64/7 MHz: 0xC99999
/// ([RTL2832U] Table 7, also the reset value).
pub const RSAMP_8MHZ: u32 = 0xC9_9999;

/// The PLL registers' content for one LO.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pll {
    pub sel_div: u8,
    pub ref_div2: bool,
    pub nint: u16,
    /// SDM_IN[16:1] as a 16-bit integer (Nfra = sdm / 2^16).
    pub sdm: u16,
}

impl Pll {
    /// f_LO under the hypothesis f_VCO = f_ref * Ndiv, for a divider `div`.
    pub fn lo_hz_for(&self, div: u32) -> f64 {
        f_ref(self.ref_div2) * 2.0 * (self.nint as f64 + self.sdm as f64 / SDM_SCALE) / div as f64
    }

    /// f_LO for a documented or measured SEL_DIV code.
    pub fn lo_hz(&self) -> Option<f64> {
        divider_any(self.sel_div).map(|d| self.lo_hz_for(d))
    }

    /// f_VCO under the same hypothesis.
    pub fn vco_hz(&self) -> f64 {
        self.lo_hz_for(1)
    }

    /// The integer and fractional words for `lo_hz` with a documented or
    /// measured divider code: Nint + Nfra = f_LO * div / (2 f_ref).
    pub fn program(lo_hz: f64, sel_div: u8, ref_div2: bool) -> Result<Pll, String> {
        let div = divider_any(sel_div).ok_or(format!(
            "SEL_DIV {sel_div:03b} has no documented or measured divider"
        ))?;
        Pll::program_with(lo_hz, div, sel_div, ref_div2)
    }

    /// The same words under a divider hypothesis `div` written with the
    /// code `sel_div` (for the undescribed codes 100-111).
    pub fn program_with(lo_hz: f64, div: u32, sel_div: u8, ref_div2: bool) -> Result<Pll, String> {
        let n = lo_hz * div as f64 / (2.0 * f_ref(ref_div2));
        let mut nint = n.floor() as i64;
        let mut sdm = ((n - nint as f64) * SDM_SCALE).round() as i64;
        if sdm >= SDM_SCALE as i64 {
            nint += 1;
            sdm = 0;
        }
        if nint < NINT_MIN as i64 || nint > NINT_MAX as i64 {
            return Err(format!(
                "Nint {nint} outside {NINT_MIN}..={NINT_MAX} for an LO of {lo_hz} Hz with divider {div} (SEL_DIV {sel_div:03b}) and f_ref {} Hz",
                f_ref(ref_div2)
            ));
        }
        Ok(Pll {
            sel_div,
            ref_div2,
            nint: nint as u16,
            sdm: sdm as u16,
        })
    }

    /// R16: SEL_DIV[7:5], REF_DIV2[4], fixed 0 1 at [3:2], CAPX[1:0] = 00
    /// (no internal crystal capacitance: the crystal's 16 pF load is on
    /// the board, [R820T] Table 6-1).
    pub fn r16(&self) -> u8 {
        (self.sel_div & 7) << 5 | (self.ref_div2 as u8) << 4 | 0x04
    }

    /// R20: SI2C[7:6], NI2C[5:0] with Nint - 13 = 4*Ni2c + Si2c.
    pub fn r20(&self) -> u8 {
        let m = self.nint - NINT_MIN;
        ((m & 3) as u8) << 6 | ((m >> 2) as u8 & 0x3F)
    }

    /// R21: SDM_IN[8:1].
    pub fn r21(&self) -> u8 {
        self.sdm as u8
    }

    /// R22: SDM_IN[16:9].
    pub fn r22(&self) -> u8 {
        (self.sdm >> 8) as u8
    }

    pub fn record(&self) -> J {
        J::obj()
            .with("sel_div", self.sel_div)
            .with("divider", divider(self.sel_div))
            .with("divider_used", divider_any(self.sel_div))
            .with("ref_div2", self.ref_div2)
            .with("f_ref_hz", f_ref(self.ref_div2))
            .with("nint", self.nint)
            .with("sdm_in_16_1", self.sdm)
            .with("nfra", self.sdm as f64 / SDM_SCALE)
            .with("vco_hz_if_f_vco_is_f_ref_x_ndiv", self.vco_hz())
            .with("lo_hz_if_f_vco_is_f_ref_x_ndiv", self.lo_hz())
            .with("r16", hex8(self.r16()))
            .with("r20", hex8(self.r20()))
            .with("r21", hex8(self.r21()))
            .with("r22", hex8(self.r22()))
    }
}

/// The front-end fields the measurements vary. Everything not here is a
/// fixed choice in [`image`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Front {
    /// R5 LNA_GAIN_MODE: 1 manual (LNA_GAIN used), 0 auto.
    pub lna_manual: bool,
    /// R5 LNA_GAIN[3:0]: 15 max ... 0 min.
    pub lna_gain: u8,
    /// R7 MIXGAIN_MODE: 0 manual (MIX_GAIN used), 1 auto.
    pub mix_manual: bool,
    /// R7 MIX_GAIN[3:0]: 0000 min ... 1111 max.
    pub mix_gain: u8,
    /// R12 VGA_MODE: 1 = IF gain from the VAGC pin, 0 = from VGA_CODE.
    pub vga_from_pin: bool,
    /// R12 VGA_CODE[3:0]: 0000 = -12.0 dB ... 1111 = +40.5 dB.
    pub vga_code: u8,
    /// R6 PWD_PDET1 (0 on / 1 off) and PWD_PDET3 (0 off / 1 on).
    pub pdet1_on: bool,
    pub pdet3_on: bool,
    /// R10 FILT_CODE[3:0]: 0000 widest ... 1111 narrowest.
    pub filt_code: u8,
    /// R11 FILT_BW[1:0]: 00 widest, 01/10 middle, 11 narrowest.
    pub filt_bw: u8,
    /// R11 HPF[3:0]: 0000 highest corner ... 1111 lowest.
    pub hpf: u8,
    /// R30 bit 6: Table 1-2 fixes it at 1; Table 1-3 calls it FILTER_EXT
    /// (0 off / 1 on, "filter extension under weak signal"). The matrix's
    /// value by default; the filter step can run both.
    pub r30_b6: bool,
    /// R26 RFMUX[1:0]: 00 tracking filter on, 01 bypass, 10/11 undescribed.
    pub rfmux: u8,
    /// R26 RFFILT[1:0]: 00 highest band, 01 middle, 10 low, 11 undescribed.
    pub rffilt: u8,
    /// R27 TF_NCH[3:0] (notch corner, 0000 highest) and TF_LP[3:0]
    /// (low-pass corner, 0000 highest).
    pub tf_nch: u8,
    pub tf_lp: u8,
    /// R18 PW_SDM, undescribed; 0 unless a sweep says otherwise.
    pub pw_sdm: bool,
    /// R15 CLK_OUT_ENB: 1 switches the tuner's clock output off. On this
    /// board that stops the demodulator (it is clocked from the tuner),
    /// so false; `clkout-check` is the only step that sets it.
    pub clk_out_off: bool,
}

impl std::default::Default for Front {
    /// The bring-up front end: every gain stage manual at the middle of
    /// its code range (8 of 0..=15, so a sweep can go both ways), the
    /// tuner's own power detectors off (the gains are manual), the
    /// channel filter at its widest codes and the high-pass at its lowest
    /// corner (so a tone anywhere in the IF band gets through), the
    /// tracking filter bypassed and the band code 00 (RFMUX = 01 makes
    /// the front end as band-agnostic as the document allows, so the PLL
    /// sweep and the meter are not confounded by the unmeasured band
    /// table).
    fn default() -> Front {
        Front {
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
            hpf: 15,
            r30_b6: true,
            rfmux: 0b01,
            rffilt: 0b00,
            tf_nch: 0,
            tf_lp: 0,
            pw_sdm: false,
            clk_out_off: false,
        }
    }
}

/// The tuner configurations the meter step compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    /// Every gain manual ([`Front::default`]).
    Manual,
    /// IF gain from the VAGC pin (VGA_MODE = 1): if the board wires the
    /// demodulator's AGC_IF output to the pin, the demodulator's IF AGC
    /// loop closes through the tuner and if_agc_val becomes a level meter.
    Vagc,
    /// LNA and mixer gain auto (the tuner's own power detectors 1 and 3
    /// on, LNA_GAIN_MODE = 0, MIXGAIN_MODE = 1), VGA manual.
    Auto,
}

impl Config {
    pub fn parse(s: &str) -> Result<Config, String> {
        match s {
            "manual" => Ok(Config::Manual),
            "vagc" => Ok(Config::Vagc),
            "auto" => Ok(Config::Auto),
            _ => Err(format!("config {s}: manual | vagc | auto")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Config::Manual => "manual",
            Config::Vagc => "vagc",
            Config::Auto => "auto",
        }
    }

    pub fn front(self) -> Front {
        let mut f = Front::default();
        match self {
            Config::Manual => {}
            Config::Vagc => f.vga_from_pin = true,
            Config::Auto => {
                f.lna_manual = false;
                f.mix_manual = false;
                f.pdet1_on = true;
                f.pdet3_on = true;
            }
        }
        f
    }
}

impl Front {
    pub fn record(&self) -> J {
        J::obj()
            .with("lna_manual", self.lna_manual)
            .with("lna_gain", self.lna_gain)
            .with("mix_manual", self.mix_manual)
            .with("mix_gain", self.mix_gain)
            .with("vga_from_pin", self.vga_from_pin)
            .with("vga_code", self.vga_code)
            .with("pdet1_on", self.pdet1_on)
            .with("pdet3_on", self.pdet3_on)
            .with("filt_code", self.filt_code)
            .with("filt_bw", self.filt_bw)
            .with("hpf", self.hpf)
            .with("r30_b6", self.r30_b6)
            .with("rfmux", self.rfmux)
            .with("rffilt", self.rffilt)
            .with("tf_nch", self.tf_nch)
            .with("tf_lp", self.tf_lp)
            .with("pw_sdm", self.pw_sdm)
            .with("clk_out_off", self.clk_out_off)
    }
}

/// The fixed cells of [R820T2] Table 1-2 for R5-R31 as (register, mask,
/// value); R30's B7/B6 as the matrix prints them.
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

/// Why each register holds what it holds, for the records. Fixed cells
/// are Table 1-2's; the rest are choices from Table 1-3's meanings.
pub const IMAGE_NOTES: [(u8, &str); 27] = [
    (
        5,
        "PWD_LT = 1 loop-through off (the board has no loop-through output; [R820T] Table 1-1 lists it as a standby current); bit 6 fixed 0; PWD_LNA1 = 0 on; LNA_GAIN_MODE, LNA_GAIN per Front",
    ),
    (
        6,
        "PWD_PDET1 (0 on / 1 off) and PWD_PDET3 (0 off / 1 on) per Front; FILT_3DB = 0 (0 dB, no extra filter gain); bits 4:3 fixed 1 0; PW_LNA = 000 max current (the 3.5 dB noise figure is specified at max gain, [R820T] Table 1-2)",
    ),
    (
        7,
        "bit 7 fixed 0; PWD_MIX = 1 on; PW0_MIX = 1 the document's 'normal current'; MIXGAIN_MODE, MIX_GAIN per Front",
    ),
    (
        8,
        "PWD_AMP = 1 mixer buffer on; PW0_AMP = 0 high current (the buffer drives the IF filter); IMR_G = 32, the middle of 0..=63 (no documented value; an image-rejection calibration is a later search)",
    ),
    (
        9,
        "PWD_IFFILT = 0 IF filter on; PW1_IFFILT = 0 high current; IMR_P = 32, the middle of 0..=63",
    ),
    (
        10,
        "PWD_FILT = 1 channel filter on; PW_FILT = 00 highest power; bit 4 fixed 1; FILT_CODE per Front",
    ),
    (
        11,
        "bit 7 fixed 0; FILT_BW per Front; bit 4 fixed 0; HPF per Front",
    ),
    (
        12,
        "bit 7 fixed 1; PWD_VGA = 1 on; bit 5 fixed 1; VGA_MODE, VGA_CODE per Front",
    ),
    (
        13,
        "LNA_VTH_H = 1010 (about 1.34 V), LNA_VTH_L = 0101 (about 0.84 V): a window symmetric about the middle of the documented 0.34-1.94 V range, high above low; used only when LNA gain is auto",
    ),
    (14, "MIX_VTH_H = 1010, MIX_VTH_L = 0101, as R13"),
    (
        15,
        "bits 7:5 fixed 0 0 1; CLK_OUT_ENB = 0 clock output on: writing 1 (clock output off) made the dongle stop answering every USB request until a power cycle (data/step4-meter-run1-aborted-at-R15.json: R5-R14 accepted, R15 and everything after kIOReturnNotResponding), so on this board the demodulator is clocked from the tuner's clock output ([R820T] 6.3 'Share Crystal'; [RTL2832U] 9.10 'oscillator output should be connected to the XI pin'); confirmed by harness clkout-check; bits 3:2 fixed 1 0; CLK_AGC_ENB = 0 internal AGC clock on (the detectors need it in auto mode); bit 0 fixed 0",
    ),
    (
        16,
        "SEL_DIV, REF_DIV2 per Pll; bits 3:2 fixed 0 1; CAPX = 00 no internal crystal capacitance (the crystal's load is on the board, [R820T] Table 6-1)",
    ),
    (
        17,
        "PW_LDO_A = 10 (2.0 V, the middle of the three documented voltages); bits 5:0 fixed 0 0 0 0 1 1",
    ),
    (
        18,
        "bit 7 fixed 1; bits 6:4 fixed 0; PW_SDM per Front (undescribed; 0 unless a sweep says otherwise); bits 2:0 fixed 0",
    ),
    (19, "all cells fixed 0"),
    (20, "SI2C[7:6], NI2C[5:0] from Nint (Pll)"),
    (21, "SDM_IN[8:1] (Pll)"),
    (22, "SDM_IN[16:9] (Pll)"),
    (
        23,
        "PW_LDO_D = 00 (1.8 V, 8 mA: the lower documented voltage at the higher current; 11 is off); bits 5:4 fixed 1 1; OPEN_D = 0 High-Z (not driving an undescribed open-drain pin); bits 2:0 fixed 1 0 0",
    ),
    (24, "bits 7:6 fixed 0 1; the unspecified cells 0"),
    (
        25,
        "PWD_RFFILT = 1 RF filter on; bits 6:5 fixed 0 0; SW_AGC = 0 agc_in; bits 3:2 fixed 1 1; the unspecified cells 0",
    ),
    (
        26,
        "RFMUX per Front; bits 5:4 fixed 1 0; PLL_AUTO_CLK = 00 128 kHz (the fastest documented auto-tune clock); RFFILT per Front",
    ),
    (27, "TF_NCH, TF_LP per Front"),
    (
        28,
        "PDET3_GAIN = 0111, the middle of 0 highest ..= 15 lowest; bits 3:2 fixed 0 1; bit 1 unspecified 0; bit 0 fixed 0",
    ),
    (
        29,
        "bits 7:6 fixed 1 1; PDET1_GAIN = 011 (the middle of the 3-bit field printed 0 highest .. 15 lowest); PDET2_GAIN = 100 (the middle of 0 lowest ..= 7 highest)",
    ),
    (
        30,
        "bit 7 fixed 0 (Table 1-2); bit 6 per Front (Table 1-2 fixes 1, Table 1-3 reads FILTER_EXT); PDET_CLK = 000000, the minimum in both the 5-bit and the 6-bit reading of the contradiction",
    ),
    (31, "bits 7:2 fixed 1 1 0 0 0 0; the unspecified cells 0"),
];

/// The 32-byte register image (R0-R4 are read-only and left 0; only
/// R5-R31 are written).
pub fn image(f: &Front, p: &Pll) -> [u8; 32] {
    let mut r = [0u8; 32];
    r[5] = 0x80 | (f.lna_manual as u8) << 4 | (f.lna_gain & 0x0F);
    r[6] = (!f.pdet1_on as u8) << 7 | (f.pdet3_on as u8) << 6 | 0x10;
    r[7] = 0x40 | 0x20 | (!f.mix_manual as u8) << 4 | (f.mix_gain & 0x0F);
    r[8] = 0x80 | 32;
    r[9] = 32;
    r[10] = 0x80 | 0x10 | (f.filt_code & 0x0F);
    r[11] = (f.filt_bw & 3) << 5 | (f.hpf & 0x0F);
    r[12] = 0x80 | 0x40 | 0x20 | (f.vga_from_pin as u8) << 4 | (f.vga_code & 0x0F);
    r[13] = 0xA5;
    r[14] = 0xA5;
    r[15] = 0x20 | (f.clk_out_off as u8) << 4 | 0x08;
    r[16] = p.r16();
    r[17] = 0x80 | 0x03;
    r[18] = 0x80 | (f.pw_sdm as u8) << 3;
    r[19] = 0x00;
    r[20] = p.r20();
    r[21] = p.r21();
    r[22] = p.r22();
    r[23] = 0x20 | 0x10 | 0x04;
    r[24] = 0x40;
    r[25] = 0x80 | 0x08 | 0x04;
    r[26] = (f.rfmux & 3) << 6 | 0x20 | (f.rffilt & 3);
    r[27] = (f.tf_nch & 0x0F) << 4 | (f.tf_lp & 0x0F);
    r[28] = 0x70 | 0x04;
    r[29] = 0xC0 | 0b011 << 3 | 0b100;
    r[30] = (f.r30_b6 as u8) << 6;
    r[31] = 0xC0;
    r
}

/// Fixed cells an image violates (empty for any image built here with
/// R30 bit 6 at the matrix's value).
pub fn fixed_violations(img: &[u8; 32]) -> Vec<String> {
    FIXED
        .iter()
        .filter(|&&(reg, mask, value)| img[reg as usize] & mask != value)
        .map(|&(reg, mask, value)| {
            format!(
                "R{reg}: 0x{:02X} & 0x{mask:02X} != 0x{value:02X}",
                img[reg as usize]
            )
        })
        .collect()
}

pub fn image_record(img: &[u8; 32]) -> J {
    let mut regs = J::Arr(Vec::new());
    for (reg, note) in IMAGE_NOTES {
        regs.push(
            J::obj()
                .with("reg", reg)
                .with("value", hex8(img[reg as usize]))
                .with("why", note),
        );
    }
    J::obj()
        .with("source", "[R820T2] Table 1-2 fixed cells as printed; every other field a stated choice from Table 1-3")
        .with("fixed_cell_violations", J::Arr(fixed_violations(img).into_iter().map(J::from).collect()))
        .with("registers", regs)
}

// ---- I2C paths (step 2) ----

/// One register through the tuner block: wValue (reg << 8) | 0x34.
pub fn write_reg(rtl: &Rtl, reg: u8, v: u8) -> usb::R<()> {
    rtl.tuner_set(((reg as u16) << 8) | TUNER_I2C_WRITE as u16, &[v])
}

/// Several registers, one write each; the record lists any error.
pub fn write_regs(rtl: &Rtl, regs: &[(u8, u8)]) -> Vec<String> {
    let mut errors = Vec::new();
    for &(reg, v) in regs {
        if let Err(e) = write_reg(rtl, reg, v) {
            errors.push(format!("R{reg} <- 0x{v:02X}: {e}"));
        }
    }
    errors
}

/// R5-R31 of an image, in ascending order.
pub fn write_image(rtl: &Rtl, img: &[u8; 32]) -> Vec<String> {
    let regs: Vec<(u8, u8)> = (5u8..32).map(|r| (r, img[r as usize])).collect();
    write_regs(rtl, &regs)
}

/// `len` bytes from register 0, each bit-reversed ([R820T] p. 17).
pub fn read_regs(rtl: &Rtl, len: u16) -> usb::R<Vec<u8>> {
    Ok(rtl
        .tuner_get(TUNER_I2C_WRITE as u16, len)?
        .into_iter()
        .map(u8::reverse_bits)
        .collect())
}

/// R0-R4: the check byte, R1 and R4 (unspecified), R2 VCO_INDICATOR,
/// R3 RF_INDICATOR.
pub fn read_status(rtl: &Rtl) -> usb::R<[u8; 5]> {
    let v = read_regs(rtl, 5)?;
    Ok([v[0], v[1], v[2], v[3], v[4]])
}

pub fn status_record(s: &[u8; 5]) -> J {
    J::obj()
        .with("r0", hex8(s[0]))
        .with("r0_is_0x96", s[0] == TUNER_R0_CHECK)
        .with("r1", hex8(s[1]))
        .with("r2_vco_indicator", s[2] & 0x7F)
        .with("r2_bit7", s[2] >> 7)
        .with("r3_rf_indicator", s[3])
        .with("r4", hex8(s[4]))
}

/// Registers a read returns before it wraps: a 32-byte read gives R0-R15
/// and then R0-R15 again (data/step4-clkout-check.json), so only R5-R15
/// can be verified by read-back.
pub const READBACK_REGS: u8 = 16;

/// Write an image and read the register file back (32 bytes, continuing
/// from register 0 as the read protocol does; it wraps after R15); if
/// the long read is refused, the 5-byte status. Returns the record.
pub fn apply_image(rtl: &Rtl, img: &[u8; 32]) -> J {
    let errors = write_image(rtl, img);
    std::thread::sleep(Duration::from_millis(10));
    let back32 = read_regs(rtl, 32);
    let (readback, matches) = match &back32 {
        Ok(b) => {
            let m: Vec<u8> = (5u8..READBACK_REGS)
                .filter(|&r| b[r as usize] != img[r as usize])
                .collect();
            let wraps = b[..16] == b[16..];
            (
                hex_bytes(b),
                Some(
                    J::obj()
                        .with(
                            "r5_to_r15_differ_at",
                            J::Arr(m.into_iter().map(J::from).collect()),
                        )
                        .with("bytes_16_to_31_repeat_0_to_15", wraps),
                ),
            )
        }
        Err(e) => (J::from(format!("32-byte read: {e}")), None),
    };
    let status = read_status(rtl)
        .map(|s| status_record(&s))
        .unwrap_or_else(|e| J::from(format!("error: {e}")));
    J::obj()
        .with("image", hex_bytes(&img[5..]))
        .with(
            "write_errors",
            J::Arr(errors.into_iter().map(J::from).collect()),
        )
        .with("readback_32", readback)
        .with("readback_check", matches)
        .with("status", status)
}

// ---- The demodulator in IF mode ----

/// What the demodulator is set to for a tuner measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DemodIf {
    /// The IF the DDC is centred on (pset_iffreq, [RTL2832U] 9.3).
    pub if_hz: f64,
    /// Spec_inv (page 1, 0x15[0]): 1 for a high-side LO (9.3).
    pub spec_inv: bool,
    /// polar_if_agc / polar_rf_agc (page 0, 0x0E[0] / [1], Table 2):
    /// 1 inverts the AGC sigma-delta pin.
    pub polar_if_agc: bool,
    pub polar_rf_agc: bool,
}

impl std::default::Default for DemodIf {
    fn default() -> DemodIf {
        DemodIf {
            if_hz: IF_HZ,
            spec_inv: true,
            polar_if_agc: false,
            polar_rf_agc: false,
        }
    }
}

impl DemodIf {
    pub fn record(&self) -> J {
        J::obj()
            .with("if_hz", self.if_hz)
            .with("pset_iffreq", pset_iffreq(self.if_hz))
            .with(
                "pset_iffreq_bytes",
                hex_bytes(&pset_iffreq_bytes(self.if_hz)),
            )
            .with("spec_inv", self.spec_inv)
            .with("polar_if_agc", self.polar_if_agc)
            .with("polar_rf_agc", self.polar_rf_agc)
    }
}

/// pset_iffreq = -floor(f_IF / f_xtal * 2^22) ([RTL2832U] 9.3, p. 19;
/// 4.57 MHz -> -665554).
pub fn pset_iffreq(if_hz: f64) -> i32 {
    -((if_hz / XTAL_HZ * TWO_POW_22).floor() as i32)
}

/// The 22-bit two's complement over {0x19, 0x1A, 0x1B}, MSB first
/// (0x19 holds bits 21:16).
pub fn pset_iffreq_bytes(if_hz: f64) -> [u8; 3] {
    let u = (pset_iffreq(if_hz) as u32) & 0x3F_FFFF;
    [(u >> 16) as u8, (u >> 8) as u8, u as u8]
}

fn rmw_sys8(rtl: &Rtl, off: u16, clear: u8, set: u8) -> Result<J, usb::Fail> {
    let old = rtl.sys_read8(off)?;
    rtl.sys_write8(off, (old & !clear) | set)?;
    let back = rtl.sys_read8(off)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)))
}

fn rmw_demod(rtl: &Rtl, page: u8, off: u8, mask: u8, value: u8) -> Result<J, usb::Fail> {
    let (old, back) = rtl.demod_rmw(page, off, mask, value)?;
    Ok(J::obj()
        .with("before", hex8(old))
        .with("after", hex8(back))
        .with("verified", back & mask == value & mask))
}

fn step(name: &str, r: Result<J, usb::Fail>) -> J {
    match r {
        Ok(j) => J::obj().with("write", name).with("result", j),
        Err(e) => J::obj().with("write", name).with("error", e.to_string()),
    }
}

/// The documented demodulator settings for a tuner measurement, each
/// read-modify-write verified by read-back. Order: demodulator clocks,
/// tuner power, the repeater, IF mode, the DDC, the resampler, the AGCs.
pub fn setup_demod(rtl: &Rtl, d: &DemodIf) -> J {
    let mut log = J::Arr(Vec::new());
    // Table 15: PLL, both ADCs, reset released (step 2: the PLL is what
    // the tuner path needs; the ADCs feed the demodulator).
    log.push(step(
        "DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15)",
        rmw_sys8(
            rtl,
            DEMOD_CTL,
            0,
            DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE,
        ),
    ));
    // Tables 16-19, p. 29: GPIO3 output, driven 0 = tuner power on.
    log.push(step(
        "GPIO3: GPD[3]=0 output, GPOE[3]=1, GPO[3]=0 tuner power on (Tables 16-19)",
        (|| {
            let a = rmw_sys8(rtl, GPD, GPIO_TUNER_POWER, 0)?;
            let b = rmw_sys8(rtl, GPOE, 0, GPIO_TUNER_POWER)?;
            let c = rmw_sys8(rtl, GPO, GPIO_TUNER_POWER, 0)?;
            std::thread::sleep(Duration::from_millis(50));
            Ok(J::obj()
                .with("GPD", a)
                .with("GPOE", b)
                .with("GPO", c)
                .with("settle_ms", 50))
        })(),
    ));
    // Table 3: IIC_repeat, the path to the tuner (step 2).
    log.push(step(
        "page 1 0x01[3] = 1 IIC_repeat (Table 3)",
        rmw_demod(
            rtl,
            IIC_REPEAT_PAGE,
            IIC_REPEAT_OFF,
            IIC_REPEAT_BIT_TABLE3,
            IIC_REPEAT_BIT_TABLE3,
        ),
    ));
    // 9.1 IF mode: AD_EN_reg1 = 1, AD_EN_reg0 = 0, en_bbin = 0.
    log.push(step(
        "page 0 0x08[7:6] = 01: AD_EN_reg = 0, AD_EN_reg1 = 1 (Table 4, IF mode 9.1)",
        rmw_demod(
            rtl,
            AD_EN_PAGE,
            AD_EN_OFF,
            AD_EN_REG | AD_EN_REG1,
            AD_EN_REG1,
        ),
    ));
    log.push(step(
        "page 1 0xB1[0] = 0 en_bbin (IF mode 9.1)",
        rmw_demod(rtl, BBIN_PAGE, BBIN_OFF, EN_BBIN, 0),
    ));
    // 9.3: the DDC at the IF, the spectrum inverted for a high-side LO.
    let b = pset_iffreq_bytes(d.if_hz);
    log.push(step(
        &format!(
            "pset_iffreq = {} for {} Hz (Table 6, 9.3)",
            pset_iffreq(d.if_hz),
            d.if_hz
        ),
        (|| {
            let a = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB, 0x3F, b[0])?;
            let c = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])?;
            let e = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])?;
            Ok(J::Arr(vec![a, c, e]))
        })(),
    ));
    log.push(step(
        &format!(
            "page 1 0x15[0] = {} Spec_inv (Table 6, 9.3)",
            d.spec_inv as u8
        ),
        rmw_demod(rtl, 1, 0x15, 0x01, d.spec_inv as u8),
    ));
    // Table 7: the 8 MHz bandwidth's ratio (the reset value, written so
    // the state is known).
    let word = (RSAMP_8MHZ & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
    let rb = word.to_be_bytes();
    let masks = [0x0F, 0xFF, 0xFF, 0xFC];
    log.push(step(
        "rsamp_ratio = 0xC99999, 8 MHz bandwidth (Table 7)",
        (|| {
            let mut v = Vec::new();
            for k in 0..4 {
                v.push(rmw_demod(
                    rtl,
                    RSAMP_PAGE,
                    RSAMP_MSB + k as u8,
                    masks[k],
                    rb[k],
                )?);
            }
            Ok(J::Arr(v))
        })(),
    ));
    // Table 2: both analog AGC loops enabled, not held; Table 10: DAGC
    // enabled (its gain is dagc_val); Table 2: the pins' polarity.
    log.push(step(
        "page 1 0x04[7:5] = 1 1 0: en_if_agc, en_rf_agc, aagc_hold (Table 2)",
        rmw_demod(rtl, 1, 0x04, 0xE0, 0xC0),
    ));
    log.push(step(
        "page 1 0x11[0] = 1 en_dagc (Table 10)",
        rmw_demod(rtl, 1, 0x11, 0x01, 0x01),
    ));
    log.push(step(
        &format!(
            "page 0 0x0E[1:0] = polar_rf_agc {}, polar_if_agc {} (Table 2)",
            d.polar_rf_agc as u8, d.polar_if_agc as u8
        ),
        rmw_demod(
            rtl,
            0,
            0x0E,
            0x03,
            (d.polar_rf_agc as u8) << 1 | d.polar_if_agc as u8,
        ),
    ));
    log
}

/// Only the DDC part of [`setup_demod`] (for sweeps that move the IF).
pub fn set_ddc(rtl: &Rtl, if_hz: f64, spec_inv: bool) -> Vec<String> {
    let b = pset_iffreq_bytes(if_hz);
    let mut errors = Vec::new();
    for (k, (mask, v)) in [(0x3F, b[0]), (0xFF, b[1]), (0xFF, b[2])]
        .into_iter()
        .enumerate()
    {
        match rtl.demod_rmw(IFFREQ_PAGE, IFFREQ_MSB + k as u8, mask, v) {
            Ok((_, back)) if back & mask == v & mask => {}
            Ok((_, back)) => errors.push(format!("pset_iffreq byte {k} read back 0x{back:02X}")),
            Err(e) => errors.push(format!("pset_iffreq byte {k}: {e}")),
        }
    }
    if let Err(e) = rtl.demod_rmw(1, 0x15, 0x01, spec_inv as u8) {
        errors.push(format!("Spec_inv: {e}"));
    }
    errors
}

/// A power-cycled (unless `--no-cycle`) dongle with the demodulator in IF
/// mode and the tuner image written; the record of all of it.
pub fn fresh(d: &DemodIf, front: &Front, pll: &Pll) -> Result<(Rtl, J), String> {
    let cyc = if crate::no_cycle() {
        J::obj().with(
            "skipped",
            "--no-cycle: the dongle was opened as found, not power-cycled",
        )
    } else {
        usb::power_cycle()?
    };
    let rtl = Rtl::open()?;
    let demod = setup_demod(&rtl, d);
    let img = image(front, pll);
    let applied = apply_image(&rtl, &img);
    // Table 1-2: PLL locking time 5 ms max.
    std::thread::sleep(Duration::from_millis(10));
    if rtl.sys_read8(DEMOD_CTL).is_err() {
        return Err(format!(
            "the dongle stopped answering after the tuner image was written: {}",
            applied.render()
        ));
    }
    let rec = J::obj()
        .with("power_cycle", cyc)
        .with("demod", d.record())
        .with("demod_writes", demod)
        .with("front", front.record())
        .with("pll", pll.record())
        .with("tuner_image", image_record(&img))
        .with("tuner_applied", applied);
    Ok((rtl, rec))
}

/// Retune the PLL only (R16, R20-R22), wait the documented 5 ms lock
/// time twice over, read the status; errors listed.
pub fn retune(rtl: &Rtl, pll: &Pll) -> (Vec<String>, Option<[u8; 5]>) {
    let errors = write_regs(
        rtl,
        &[
            (16, pll.r16()),
            (20, pll.r20()),
            (21, pll.r21()),
            (22, pll.r22()),
        ],
    );
    std::thread::sleep(Duration::from_millis(10));
    (errors, read_status(rtl).ok())
}

/// The LO for an RF with the IF on the chosen side.
pub fn lo_for(rf_hz: f64, if_hz: f64, high_side: bool) -> f64 {
    if high_side {
        rf_hz + if_hz
    } else {
        rf_hz - if_hz
    }
}

/// Choose a SEL_DIV for an LO from [`VCO_RANGE_HZ`]: the smallest
/// divider whose VCO lands inside it (the driver's rule, src/pll.rs
/// `usable_codes`). With the VCO at 1.70-3.65 GHz the LOs reachable are
/// 26.6-57 (/64), 53-114 (/32), 106-228 (/16), 212-456 (/8), 425-912
/// (/4), 850-1825 MHz (/2). Until findings 10.5 code 011 was taken as the
/// document's /1, and 114-212 MHz was unreachable (step 4d); the step 4f
/// band table's first run skipped it for that reason.
pub fn choose_div(lo_hz: f64, ref_div2: bool) -> Result<u8, String> {
    let (lo, hi) =
        VCO_RANGE_HZ.ok_or("the VCO range is not measured yet (run pll-sweep); pass --div CODE")?;
    for code in [0b000u8, 0b001, 0b010, 0b011, 0b100, 0b101] {
        let div = divider_any(code).unwrap_or(1);
        let vco = lo_hz * div as f64;
        if vco >= lo && vco <= hi && Pll::program(lo_hz, code, ref_div2).is_ok() {
            return Ok(code);
        }
    }
    Err(format!(
        "no documented or measured divider puts {lo_hz} Hz x div inside the VCO range {lo}..{hi} Hz"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_image_keeps_every_fixed_cell() {
        let p = Pll::program(300e6 + IF_HZ, 0b000, true).unwrap();
        let img = image(&Front::default(), &p);
        assert!(
            fixed_violations(&img).is_empty(),
            "{:?}",
            fixed_violations(&img)
        );
        for c in [Config::Manual, Config::Vagc, Config::Auto] {
            assert!(fixed_violations(&image(&c.front(), &p)).is_empty());
        }
    }

    #[test]
    fn pll_formula_round_trips() {
        // f_ref 14.4 MHz, divider 2: Nint + Nfra = 304.57e6 * 2 / 28.8e6.
        let p = Pll::program(304.57e6, 0b000, true).unwrap();
        assert_eq!(p.nint, 21);
        let lo = p.lo_hz().unwrap();
        assert!((lo - 304.57e6).abs() < 28.8e6 / 65536.0, "{lo}");
        // Nint = 4*Ni2c + Si2c + 13 packs into R20.
        assert_eq!(p.r20(), ((21 - 13) % 4) << 6 | (21 - 13) / 4);
        // 011 is /16 (findings 10.5): 154.57 MHz is Nint 85 and back.
        let p = Pll::program(154.57e6, 0b011, true).unwrap();
        assert_eq!(p.nint, 85);
        assert!((p.lo_hz().unwrap() - 154.57e6).abs() < 28.8e6 / 65536.0);
        assert_eq!(choose_div(154.57e6, true), Ok(0b011));
        assert!(Pll::program(10e6, 0b011, true).is_err());
        assert!(Pll::program(1e6, 0b100, true).is_err());
    }

    #[test]
    fn pset_iffreq_matches_the_documents_example() {
        assert_eq!(pset_iffreq(4.57e6), -665554);
        assert_eq!(pset_iffreq_bytes(4.57e6), [0x35, 0xD8, 0x2E]);
        assert_eq!(pset_iffreq_bytes(0.0), [0, 0, 0]);
    }
}
