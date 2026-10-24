//! The starting states the step-3 bit scan can run from.
//!
//! The first bit scan (`data/step3-scan.jsonl`) started from the
//! documented zero-IF baseline ([RTL2832U] 9.1: AD_EN_reg = AD_EN_reg1 =
//! 1, en_bbin = 1, pset_iffreq = 0). The tuner is a low-IF part ([R820T]
//! 4.3 and Table 4-1: IF 3.57-4.57 MHz) mixing high-side (findings 4h),
//! and step 4 saw the demodulator respond to a tone only in the documented
//! IF mode with the tuner powered and tuned. Two more starting states:
//!
//! - `low-if`: step 4's configuration (the writes of `tuner::setup_demod`,
//!   the tuner image of `tuner::image` in the `vagc` configuration of
//!   findings 4b, the PLL at 300 MHz RF with /8 as in findings 4e), the
//!   resampler at 2.4 Msps instead of 8 MHz, and the bulk endpoint exactly
//!   as the zero-IF baseline sets it. Itemised ([`LowItem`]) so a hit can
//!   be minimised item by item.
//! - `power-on`: the chip as it powers up, with only the demodulator PLL
//!   enabled (without it every demodulator access stalls: findings 1C, and
//!   the scan's candidates are demodulator writes) and the bulk endpoint's
//!   registers checked against the descriptor (written only where they
//!   differ; at power-on they matched in findings 1A, so normally nothing).

use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use crate::content::{self, Content};
use crate::json::{J, hex8, hex32};
use crate::meter::{self, Meter, Reading};
use crate::regs::*;
use crate::source::{self, Level, Source};
use crate::step3::{self, Item};
use crate::tuner::{self, Config, DemodIf, Pll};
use crate::usb::{self, Rtl};

/// The RF the low-if baseline tunes to: 300 MHz, where findings 4e
/// received the tone at the predicted IF (`data/step4-tune-300.000MHz.json`).
pub const LOWIF_RF_HZ: f64 = 300e6;
/// SEL_DIV 010 = /8 ([R820T2] p. 9), the code `tuner::choose_div` picked
/// for the 304.57 MHz LO in findings 4e (Nint 84, SDM 39504).
pub const LOWIF_SEL_DIV: u8 = 0b010;
/// R2 VCO_INDICATOR[6:0] inside the VCO's range (findings 4c: 70-127
/// where the tone was received, 0 or 63 outside).
pub const VCO_INDICATOR_LOCKED: RangeInclusive<u8> = 70..=127;
/// The resampler's power-on ratio, 0xC99999 ([RTL2832U] Table 7), is
/// f_s = f_xtal * 2^22 / ratio = 64/7 MHz by the 9.4 formula: the rate a
/// stream from the power-on state would be judged against.
pub fn power_on_rate_hz() -> f64 {
    XTAL_HZ * TWO_POW_22 / RSAMP_RESET as f64
}

/// The low-if baseline's items, in the order applied: `tuner::setup_demod`'s
/// writes (its 8 MHz rsamp_ratio replaced by [`LowItem::Rsamp`] at the
/// scan's rate), the tuner image, then the zero-IF baseline's endpoint
/// items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LowItem {
    DemodCtl,
    TunerPower,
    IicRepeat,
    AdEnIf,
    EnBbinOff,
    IfFreq,
    SpecInv,
    AgcLoops,
    Dagc,
    AgcPolarity,
    Rsamp,
    TunerImage,
    EpaMaxPkt,
    EpaCfg,
    EpaFifoReset,
    Dma,
}

pub const LOW_ITEMS: [LowItem; 16] = [
    LowItem::DemodCtl,
    LowItem::TunerPower,
    LowItem::IicRepeat,
    LowItem::AdEnIf,
    LowItem::EnBbinOff,
    LowItem::IfFreq,
    LowItem::SpecInv,
    LowItem::AgcLoops,
    LowItem::Dagc,
    LowItem::AgcPolarity,
    LowItem::Rsamp,
    LowItem::TunerImage,
    LowItem::EpaMaxPkt,
    LowItem::EpaCfg,
    LowItem::EpaFifoReset,
    LowItem::Dma,
];

/// low-if-open's extra item: page 0 0x07 bit 7 (its power-on value 0x3C,
/// written 0xBC; data/step3-confirm-lowif-p0-07-bc.json).
pub const OPEN_PATH_ITEM: &str = "OpenPath";
pub const OPEN_PATH_PAGE: u8 = 0;
pub const OPEN_PATH_OFF: u8 = 0x07;
pub const OPEN_PATH_BIT: u8 = 1 << 7;

/// [RTL2832U] 9.3 (p. 19), the "DAB Mode" example: pset_iffreq =
/// -1066988 = 0x2FB814 (two's complement over 22 bits).
pub const DAB_PSET_IFFREQ: i32 = -1_066_988;
/// [RTL2832U] 9.4 (p. 20), the "DAB mode" example: rsamp_ratio =
/// 14745600 = 0xE10000.
pub const DAB_RSAMP_RATIO: u32 = 0xE1_0000;
/// f_symbol of the DAB example: 28.8 MHz x 2^22 / 0xE10000 = 8.192 MHz
/// exactly. Stated as the literal so that rsamp_ratio(rate)'s floor gives
/// 0xE10000 (the quotient computed in floating point floors to 0xE0FFFF;
/// the unit test checks).
pub const DAB_RATE_HZ: f64 = 8.192e6;
pub fn dab_rate_hz() -> f64 {
    DAB_RATE_HZ
}
/// f_IF_D of the DAB example: 1066988 / 2^22 x 28.8 MHz = 7.32643 MHz.
pub fn dab_if_hz() -> f64 {
    -(DAB_PSET_IFFREQ as f64) / TWO_POW_22 * XTAL_HZ
}
/// The 22-bit pattern over {0x19, 0x1A, 0x1B}, MSB first.
pub fn dab_pset_bytes() -> [u8; 3] {
    let u = (DAB_PSET_IFFREQ as u32) & 0x3F_FFFF;
    [(u >> 16) as u8, (u >> 8) as u8, u as u8]
}
/// dab-open's extra rate flags (bytes/s, each within
/// `content::TARGET_TOLERANCE`): 2 x 2.048 MHz and 2 x 8.192 MHz (8-bit
/// I/Q pairs), and 4 x each (16-bit samples).
pub const DAB_RATE_TARGETS: [f64; 4] = [4.096e6, 16.384e6, 8.192e6, 32.768e6];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dab_examples_match_the_datasheet() {
        assert_eq!(dab_pset_bytes(), [0x2F, 0xB8, 0x14]);
        assert_eq!(rsamp_ratio(dab_rate_hz()), DAB_RSAMP_RATIO);
        assert!((XTAL_HZ * TWO_POW_22 / DAB_RSAMP_RATIO as f64 - DAB_RATE_HZ).abs() < 1e-3);
        // The confirmation's other two rates: the ratio doubled and halved.
        assert_eq!(rsamp_ratio(DAB_RATE_HZ / 2.0), 2 * DAB_RSAMP_RATIO);
        assert_eq!(rsamp_ratio(DAB_RATE_HZ * 2.0), DAB_RSAMP_RATIO / 2);
        assert!((dab_if_hz() - 7_326_425.17).abs() < 1.0, "{}", dab_if_hz());
    }
}

/// One back-end item: (name, page, [(offset, mask, value)]). Only bits the
/// document names, each set to its documented "off".
pub type DabItem = (&'static str, u8, &'static [(u8, u8, u8)]);
pub const DAB_ITEMS: [DabItem; 5] = [
    ("AciOff", 1, &[(0x15, 0x02, 0x00)]),
    ("CciOff", 1, &[(0x40, 0x02, 0x00)]),
    ("IncOff", 1, &[(0x5D, 0x01, 0x00)]),
    ("AutoScanOff", 1, &[(0x5F, 0x01, 0x01)]),
    (
        "PidPassAll",
        0,
        &[
            (0x61, 0x40, 0x40),
            (0x62, 0xFF, 0x00),
            (0x63, 0xFF, 0x00),
            (0x64, 0xFF, 0x00),
            (0x65, 0xFF, 0x00),
        ],
    ),
];
/// Each back-end item's documented meaning, for the record.
pub const DAB_ITEM_TEXT: [(&str, &str); 5] = [
    (
        "AciOff",
        "AciOff: En_aci page 1 0x15[1] = 0 (Table 6, p. 20: '0: Disable adjacent channel rejection'; 9.3 calls it 'an adjacent channel canceller'). AMBIGUOUS placement: the document puts it in the DDC (9.3, the front half), not the DVB-T back end; turned off here as an itemised choice",
    ),
    (
        "CciOff",
        "CciOff: En_cci page 1 0x40[1] = 0 (Table 8, p. 21: '0: Disable CCI cancellation'; 9.5 co-channel interference rejection)",
    ),
    (
        "IncOff",
        "IncOff: en_inc page 1 0x5D[0] = 0 (Table 9, p. 21: '0: Disable impulse noise cancellation'; 9.6)",
    ),
    (
        "AutoScanOff",
        "AutoScanOff: dis_auto_scan page 1 0x5F[0] = 1 (Table 11, p. 22: '1: Disable auto mode detection'; 9.8 FFT mode detection). The bit's polarity is inverted against the others (1 = off), as printed",
    ),
    (
        "PidPassAll",
        "PidPassAll: the PID filter at its documented pass-all setting (9.11, Table 13, p. 25; Fig. 7): mode_PID page 0 0x61[6] = 1 and en_PID[31:0] page 0 0x62-0x65 = 0 — no individual filter enabled, so the XOR with mode_PID passes every packet ('the default register setting will let all PID transport streams pass through'). The document has no 'filter off' bit other than this; enable_PID is the output gate (not changed, see below)",
    ),
];

/// zero-if-open's tone offsets from RF = LO - 4.57 MHz (the RF the tuner
/// puts at its IF): 0, then +-0.2, +-0.5, +-1.0 MHz.
pub const ZERO_OPEN_TONE_OFFSETS_HZ: [f64; 7] = [0.0, 200e3, -200e3, 500e3, -500e3, 1.0e6, -1.0e6];

/// Why zero-if-open's scan keeps the tone keyed (at the positive control's
/// RF, 300 MHz, CONTROL_LEVEL).
pub const SCAN_TONE_WHY: &str = "data/step8-fifo-watch.json and data/step8-fifo-watch-lowif-open.json: in the held-open states the only new bytes that ever enter the FIFO memory are 4 bytes at one fixed position of each 188-byte block, rewritten in bursts ~2.5-3 s apart, and only while the tone is on (0 changed windows in 3 of 4 tone-off states each; the 4th is the window right after unkeying). A bit that changed what this signal-driven writer writes (whole blocks, every period) shows only with a signal present, so the scan keeps the tone keyed at the positive control's RF; its thresholds come from the tone-on solo windows (the transmitter shares the host's USB: ~37 MB/s)";

/// The power-on baseline's two items.
pub const POWER_ON_ITEMS: [&str; 2] = ["DemodPll", "Endpoint"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    ZeroIf,
    LowIf,
    /// low-if with page 0 0x07 bit 7 set: the bulk endpoint's data path
    /// held open (data/step3-confirm-lowif-p0-07-bc.json).
    LowIfOpen,
    /// low-if-open with the front half at the datasheet's DAB example
    /// (9.3, 9.4) and every documented DVB-T back-end enable off.
    DabOpen,
    /// The documented zero-IF input (9.1, 9.3) with the tuner configured
    /// and tuned and the path held open (findings step 8).
    ZeroIfOpen,
    PowerOn,
}

impl Baseline {
    pub fn parse(s: &str) -> Result<Baseline, String> {
        match s {
            "zero-if" => Ok(Baseline::ZeroIf),
            "low-if" => Ok(Baseline::LowIf),
            "low-if-open" => Ok(Baseline::LowIfOpen),
            "dab-open" => Ok(Baseline::DabOpen),
            "zero-if-open" => Ok(Baseline::ZeroIfOpen),
            "power-on" => Ok(Baseline::PowerOn),
            _ => Err(format!(
                "--baseline {s}: zero-if | low-if | low-if-open | dab-open | zero-if-open | power-on"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Baseline::ZeroIf => "zero-if",
            Baseline::LowIf => "low-if",
            Baseline::LowIfOpen => "low-if-open",
            Baseline::DabOpen => "dab-open",
            Baseline::ZeroIfOpen => "zero-if-open",
            Baseline::PowerOn => "power-on",
        }
    }

    /// The scan record under derivation/data.
    pub fn record_name(self) -> &'static str {
        match self {
            Baseline::ZeroIf => "step3-scan.jsonl",
            Baseline::LowIf => "step3-scan-lowif.jsonl",
            Baseline::LowIfOpen => "step3-scan-lowif-open.jsonl",
            Baseline::DabOpen => "step7-scan-dab-open.jsonl",
            Baseline::ZeroIfOpen => "step8-scan-zero-if-open.jsonl",
            Baseline::PowerOn => "step3-scan-poweron.jsonl",
        }
    }

    /// The data path held open by page 0 0x07 bit 7 (the content
    /// detector's baselines).
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Baseline::LowIfOpen | Baseline::DabOpen | Baseline::ZeroIfOpen
        )
    }

    /// The sample rate a stream is judged against: the rate the baseline
    /// programs into rsamp_ratio, or the power-on ratio's.
    pub fn rate_hz(self) -> f64 {
        match self {
            Baseline::ZeroIf | Baseline::LowIf | Baseline::LowIfOpen | Baseline::ZeroIfOpen => {
                step3::RATE_HZ
            }
            Baseline::DabOpen => dab_rate_hz(),
            Baseline::PowerOn => power_on_rate_hz(),
        }
    }

    /// The rates a confirmation tracks: the baseline's and two others.
    pub fn confirm_rates_hz(self) -> Vec<f64> {
        match self {
            Baseline::PowerOn => vec![self.rate_hz()],
            // The DAB example, then its ratio x2 and x1/2 (rate /2, x2).
            Baseline::DabOpen => vec![dab_rate_hz(), dab_rate_hz() / 2.0, dab_rate_hz() * 2.0],
            _ => step3::CONFIRM_RATES_HZ.to_vec(),
        }
    }

    /// The RF at the DDC's centre: the tone the controls use sits here
    /// (plus an offset). low-if: the tuned RF (IF 4.57 MHz = the DDC's).
    /// dab-open: the DDC is at the DAB example's f_IF_D, so the RF whose
    /// IF lands there under the same high-side LO.
    pub fn ddc_centre_rf_hz(self) -> f64 {
        match self {
            Baseline::DabOpen => low_if_pll().lo_hz().unwrap_or(0.0) - dab_if_hz(),
            _ => LOWIF_RF_HZ,
        }
    }

    /// The tone test's states: (name, RF or None for the source off). The
    /// first and last are off; the off states are the reference the tone
    /// states are compared with.
    pub fn tone_plan(self) -> Vec<(String, Option<f64>)> {
        let mut plan: Vec<(String, Option<f64>)> = vec![("tone off".into(), None)];
        match self {
            Baseline::ZeroIfOpen => {
                let lo = low_if_pll().lo_hz().unwrap_or(0.0);
                let at_if = lo - tuner::IF_HZ;
                for o in ZERO_OPEN_TONE_OFFSETS_HZ {
                    plan.push((format!("tone at LO - 4.57 MHz {o:+} Hz"), Some(at_if + o)));
                }
                plan.push(("tone at the LO".into(), Some(lo)));
            }
            _ => {
                for o in crate::confirm_write::TONE_OFFSETS_HZ {
                    plan.push((
                        format!("tone at the DDC centre RF {o:+} Hz"),
                        Some(self.ddc_centre_rf_hz() + o),
                    ));
                }
            }
        }
        plan.push(("tone off again".into(), None));
        plan
    }

    /// Byte rates (bytes/s) flagged in addition to the stream band.
    pub fn rate_targets(self) -> Vec<f64> {
        match self {
            Baseline::DabOpen => DAB_RATE_TARGETS.to_vec(),
            _ => Vec::new(),
        }
    }

    /// The names of the removable items (for minimisation).
    pub fn item_names(self) -> Vec<String> {
        match self {
            Baseline::ZeroIf => step3::ITEMS.iter().map(|i| format!("{i:?}")).collect(),
            Baseline::LowIf => LOW_ITEMS.iter().map(|i| format!("{i:?}")).collect(),
            Baseline::LowIfOpen => LOW_ITEMS
                .iter()
                .map(|i| format!("{i:?}"))
                .chain([OPEN_PATH_ITEM.to_string()])
                .collect(),
            Baseline::DabOpen => LOW_ITEMS
                .iter()
                .map(|i| format!("{i:?}"))
                .chain(DAB_ITEMS.iter().map(|(n, ..)| n.to_string()))
                .chain([OPEN_PATH_ITEM.to_string()])
                .collect(),
            Baseline::ZeroIfOpen => ZERO_OPEN_ITEMS.iter().map(|s| s.to_string()).collect(),
            Baseline::PowerOn => POWER_ON_ITEMS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Every item of the baseline in words, with its citation and why it
    /// is there, for the record's header.
    pub fn definition(self) -> J {
        let items: &[&str] = match self {
            Baseline::ZeroIf => &[
                "the documented-only baseline of findings step 3 (step3::apply_baseline): DEMOD_CTL PLL|ADC_I|ADC_Q|reset-release (Table 15); GPIO3 tuner power (Tables 16-19); AD_EN_reg = AD_EN_reg1 = 1, en_bbin = 1 (Table 4, zero-IF 9.1); pset_iffreq = 0 (Table 6, 9.3); rsamp_ratio for 2.4 Msps (Table 7, 9.4); USB_EPA_MAXPKT, USB_EPA_CFG, USB_EPA_CTL FIFO reset pulse, USB_SYSCTL DMA (Tables 30-33)",
            ],
            Baseline::LowIf | Baseline::LowIfOpen | Baseline::DabOpen => &[
                "a power cycle, then in order, every write a read-modify-write read back (tuner::setup_demod's writes, rsamp_ratio at the scan's rate instead of its 8 MHz):",
                "DemodCtl: DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15; the PLL makes the demodulator answer, findings 1C)",
                "TunerPower: GPIO3 GPD[3] = 0, GPOE[3] = 1, GPO[3] = 0, then 50 ms (Tables 16-19, p. 29)",
                "IicRepeat: page 1 0x01[3] = 1 (Table 3; the tuner path, findings 2)",
                "AdEnIf: page 0 0x08[7:6] = 01, AD_EN_reg = 0, AD_EN_reg1 = 1 (Table 4, IF mode 9.1)",
                "EnBbinOff: page 1 0xB1[0] = 0 en_bbin (Table 4, IF mode 9.1)",
                "IfFreq: pset_iffreq = -floor(4.57e6 / 28.8e6 * 2^22) = -665554 over page 1 0x19-0x1B (Table 6, 9.3; the IF of [R820T] Table 4-1 row 5)",
                "SpecInv: page 1 0x15[0] = 1 (9.3: a high-side tuner needs the inverted spectrum; findings 4h measured high-side mixing)",
                "AgcLoops: page 1 0x04[7:5] = 110, en_if_agc = en_rf_agc = 1, aagc_hold = 0 (Table 2)",
                "Dagc: page 1 0x11[0] = 1 en_dagc (Table 10)",
                "AgcPolarity: page 0 0x0E[1:0] = 00 (Table 2; findings 4b: polarity 0 closes the IF loop through VAGC)",
                "Rsamp: rsamp_ratio = floor(28.8e6 / rate * 2^22) over page 1 0x9F-0xA2 [27:2] (9.4 formula, Table 7); 0x3000000 at 2.4 Msps",
                "TunerImage: tuner::image (every fixed cell of [R820T2] Table 1-2 as printed, every other field the stated choice of tuner::IMAGE_NOTES) in the vagc configuration (VGA_MODE = 1, the IF gain from the VAGC pin wired to the demodulator's IF AGC output, findings 4b), CLK_OUT_ENB = 0 (the demodulator is clocked from it, findings 4a), PLL for LO = 300 MHz + 4.57 MHz (high side), SEL_DIV 010 /8, REF_DIV2 1: Nint 84, SDM 39504 (findings 4c, 4e); R5-R15 checked by the 32-byte read-back (R16-R31 cannot be read back, findings 4a); after 10 ms R2 VCO_INDICATOR checked in 70..=127 (findings 4c)",
                "EpaMaxPkt, EpaCfg, EpaFifoReset, Dma: the bulk endpoint as the zero-IF baseline sets it: USB_EPA_MAXPKT = the descriptor's wMaxPacketSize (Table 33); USB_EPA_CFG enable, bulk, endpoint number (Table 31); USB_EPA_CTL FIFO reset 1 then 0, stall 0 (Table 32); USB_SYSCTL DMA (Table 30)",
            ],
            Baseline::ZeroIfOpen => &[
                "a power cycle, then in order (ZERO_OPEN_ITEMS), every demodulator write a read-modify-write read back, every USB/system write read back:",
                "DemodCtl: DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15) — the zero-IF baseline's item",
                "TunerPower: GPIO3 GPD[3] = 0, GPOE[3] = 1, GPO[3] = 0, then 50 ms (Tables 16-19, p. 29) — as low-if (the zero-IF baseline's item without the 50 ms)",
                "IicRepeat: page 1 0x01[3] = 1 (Table 3; the tuner path, findings 2) — as low-if",
                "AdEn: page 0 0x08[7:6] = 11, AD_EN_reg = AD_EN_reg1 = 1 (Table 4, zero-IF input 9.1) — the zero-IF baseline's item",
                "EnBbin: page 1 0xB1[0] = 1 en_bbin (Table 4, zero-IF input 9.1; 9.2: DC cancellation and IQ compensation default on with it) — the zero-IF baseline's item",
                "IfFreqZero: pset_iffreq = 0 over page 1 0x19-0x1B (Table 6, 9.3 zero-IF example) — the zero-IF baseline's item",
                "AgcLoops: page 1 0x04[7:5] = 110, en_if_agc = en_rf_agc = 1, aagc_hold = 0 (Table 2) — as low-if",
                "Dagc: page 1 0x11[0] = 1 en_dagc (Table 10) — as low-if",
                "AgcPolarity: page 0 0x0E[1:0] = 00 (Table 2; findings 4b: polarity 0 closes the IF loop through VAGC) — as low-if",
                "Rsamp: rsamp_ratio = floor(28.8e6 / rate * 2^22) over page 1 0x9F-0xA2 [27:2] (9.4 formula, Table 7); 0x3000000 at 2.4 Msps",
                "TunerImage: tuner::image in the vagc configuration, CLK_OUT_ENB = 0 (findings 4a), PLL for LO = 300 MHz + 4.57 MHz = 304.57 MHz (high side), SEL_DIV 010 /8, REF_DIV2 1: Nint 84, SDM 39504 (findings 4c, 4e); R5-R15 checked by the read-back; R2 VCO_INDICATOR checked in 70..=127 after 10 ms — exactly low-if's item. The tuner itself stays a low-IF part: its output is RF 300 MHz at IF 4.57 MHz ([R820T] Table 4-1), so with pset_iffreq = 0 a tone lands 4.57 MHz off the DDC's centre (or wherever the resampler folds it); the tone test steps it across offsets for that reason",
                "EpaMaxPkt, EpaCfg, EpaFifoReset, Dma: the bulk endpoint as the zero-IF baseline sets it (Tables 30-33)",
                "OpenPath (last): page 0 0x07 |= 0x80 (0x3C -> 0xBC), read back. data/step3-confirm-lowif-p0-07-bc.json: with the tuner programmed this bit makes the bulk endpoint deliver at the bus's full rate the FIFO memory's 3,760 bytes round and round; needs DemodCtl, IicRepeat, TunerImage",
                "NOT written, with the reason: Spec_inv / En_aci page 1 0x15 (the zero-IF baseline does not write them; their power-on values stand and are read into the setup record); the Table 5 DC/IQ enables (on by default with en_bbin, 9.2); opt_adc_iq page 0 0x06[5:4] (no reason to swap); the tuner's own IF stays 4.57 MHz (it is a low-IF part)",
            ],
            Baseline::PowerOn => &[
                "a power cycle (VBUS off/on through the analyzer), then only:",
                "DemodPll: DEMOD_CTL bit 7 = 1, demodulator PLL enable (Table 15), read back. Unavoidable: with it 0 every demodulator read and write stalls (findings 1B/1C: 3/59 oracle bytes as booted, 58/59 with the PLL on), and every candidate of the scan is a demodulator write. The ADC enables (bits 6, 3) are NOT set: findings 1C needed only the PLL.",
                "Endpoint: the bulk endpoint's registers read and compared with what the host's bulk IN transfers need: USB_SYSCTL[0] DMA = 1 (Table 30), USB_EPA_CFG enable, type bulk, iso 00, number = the descriptor's endpoint (Table 31), USB_EPA_MAXPKT = the descriptor's wMaxPacketSize (Table 33), USB_EPA_CTL stall (bit 4) = 0 (Table 32). A register is written (read-modify-write, read back) only if it differs; at power-on findings 1A read all four as needed (0x01, 0xD1, 0x200, 0), so normally none is written. No FIFO reset pulse (nothing shows it is needed; the FIFO is at its power-on state).",
                "Nothing else: the tuner at its own power-on state (it answers, and clocks the demodulator, findings 4a), the demodulator's registers at theirs (page 0 0x08 = 0x0D: AD_EN_reg = AD_EN_reg1 = 0; page 1 0xB1 = 0x1F: en_bbin 1; pset_iffreq 0x35D75D = 32/7 MHz; rsamp_ratio 0xC99999 = 64/7 MHz; findings step 3 baseline and restart probe), GPIO3 not driven, the I2C repeater closed.",
            ],
        };
        let mut items: Vec<&str> = items.to_vec();
        if self == Baseline::DabOpen {
            items.extend_from_slice(&[
                "dab-open replaces three low-if items' values: IfFreq = the DAB example of 9.3 (p. 19): pset_iffreq = -1066988 = 0x2FB814 (22-bit two's complement), i.e. f_IF_D = 1066988 / 2^22 x 28.8 MHz = 7.32643 MHz; the tuner stays at low-if's LO (304.57 MHz, high side), so the DDC's centre is RF 297.2436 MHz, where the controls put the tone; Rsamp = the DAB example of 9.4 (p. 20): rsamp_ratio = 14745600 = 0xE10000, i.e. f_symbol = 28.8 MHz x 2^22 / 0xE10000 = 8.192 MHz (= 4 x 2.048 MHz); Dagc = en_dagc 0, page 1 0x11[0] (Table 10, '0: Disable DAGC')",
                "then the documented DVB-T back-end enables off (DAB_ITEMS), each a read-modify-write read back:",
            ]);
            for (_, what) in DAB_ITEM_TEXT {
                items.push(what);
            }
            items.push("NOT changed, with the reason: enable_PID page 0 0x61[7] stays 1 — Table 13 documents it as 'Set 1 to Enable Output of PID Filter / Set 0 No Output', the output gate, which the held-open path needs (the solo measures enable_PID = 0 separately); err_pass page 0 0x61[5] stays 1 ('Pass All Error Packets', not an enable); pset_mode_gi page 1 0x5F[3:1] left as read (a preset used only with auto detection off; Table 11 prints default 3, findings 1C read 0); cfreq_off_ratio page 1 0x9D-0x9F stays 0xAEBAF (9.9 says it 'must be set according to the sampling frequency' but the DAB example gives no value); the AGC loops (Table 2) stay on (the tuner's VGA is closed through them, findings 4b); en_dc_est / en_iq_comp / en_iq_est (Table 5) are zero-IF blocks already bypassed by en_bbin = 0 (9.2)");
        }
        if self.is_open() && self != Baseline::ZeroIfOpen {
            items.push("OpenPath (last): page 0 0x07 |= 0x80 (0x3C -> 0xBC), read back. data/step3-confirm-lowif-p0-07-bc.json: with the tuner programmed this bit makes the bulk endpoint deliver at the bus's full rate (45 MB/s, FIFO dropped-block counter 255) the FIFO memory's 3,760 bytes (20 x 188) round and round; needs DemodCtl, IicRepeat, TunerImage");
        }
        let mut def = J::obj();
        if self == Baseline::DabOpen {
            def.set(
                "dab_example",
                J::obj()
                    .with("pset_iffreq", DAB_PSET_IFFREQ)
                    .with(
                        "pset_iffreq_bytes",
                        J::Arr(dab_pset_bytes().iter().map(|b| hex8(*b)).collect()),
                    )
                    .with("f_if_d_hz", dab_if_hz())
                    .with("rsamp_ratio", hex32(DAB_RSAMP_RATIO))
                    .with("rsamp_ratio_written", hex32(rsamp_ratio(DAB_RATE_HZ)))
                    .with("f_symbol_hz", DAB_RATE_HZ)
                    .with("ddc_centre_rf_hz", self.ddc_centre_rf_hz())
                    .with(
                        "rate_targets_bytes_per_s",
                        J::Arr(DAB_RATE_TARGETS.iter().map(|t| J::from(*t)).collect()),
                    ),
            );
        }
        if self == Baseline::ZeroIfOpen {
            def.set(
                "tone_plan",
                J::obj()
                    .with("lo_hz", low_if_pll().lo_hz())
                    .with("tuner_if_hz", tuner::IF_HZ)
                    .with("states", J::Arr(self.tone_plan().iter().map(|(n, hz)| J::obj().with("state", n.as_str()).with("rf_hz", *hz)).collect()))
                    .with("why", "in zero-IF the DDC sits at 0 Hz (pset_iffreq = 0) while the tuner delivers RF 300 MHz at its 4.57 MHz IF: the tone is stepped across RF = LO - 4.57 MHz + {0, +-0.2, +-0.5, +-1.0} MHz and RF = LO itself, and ANY spectral line that moves with it is looked for (tone_lines), not one expected bin"),
            );
        }
        def.with("baseline", self.name())
            .with("items", J::Arr(items.iter().map(|s| J::from(*s)).collect()))
            .with("rate_hz_judged_against", self.rate_hz())
            .with(
                "expected_bytes_per_s",
                step3::expected_bytes_per_s(self.rate_hz()),
            )
            .with(
                "streams_criterion",
                format!(
                    "bytes/s within {}% of 2 x {} Hz",
                    step3::TOLERANCE * 100.0,
                    self.rate_hz()
                ),
            )
    }

    /// Apply the baseline at `rate` without the items named in `skip`; the
    /// record of every write and read-back. The power-on baseline writes
    /// no rate.
    pub fn apply_at(self, rtl: &Rtl, rate: f64, skip: &[String]) -> J {
        match self {
            Baseline::ZeroIf => {
                let items: Vec<Item> = step3::ITEMS
                    .into_iter()
                    .filter(|i| skip.contains(&format!("{i:?}")))
                    .collect();
                step3::apply_baseline(rtl, rate, &items)
            }
            Baseline::LowIf => apply_low_if(rtl, rate, skip, false),
            Baseline::LowIfOpen | Baseline::DabOpen => {
                let dab = self == Baseline::DabOpen;
                let mut log = apply_low_if(rtl, rate, skip, dab);
                if dab {
                    for (name, page, regs) in DAB_ITEMS
                        .iter()
                        .filter(|(n, ..)| !skip.iter().any(|s| s == n))
                    {
                        let mut v = Vec::new();
                        let mut err = None;
                        for &(off, mask, val) in regs.iter() {
                            match rmw_demod(rtl, *page, off, mask, val) {
                                Ok(j) => v.push(
                                    j.with("page", *page)
                                        .with("offset", hex8(off))
                                        .with("mask", hex8(mask))
                                        .with("value", hex8(val)),
                                ),
                                Err(e) => err = Some(e.to_string()),
                            }
                        }
                        log.push(
                            J::obj()
                                .with("item", *name)
                                .with("record", J::Arr(v))
                                .with("error", err),
                        );
                    }
                }
                if !skip.iter().any(|s| s == OPEN_PATH_ITEM) {
                    let r = step3::step(
                        "page 0 0x07[7] = 1: the endpoint data path open (data/step3-confirm-lowif-p0-07-bc.json)",
                        rmw_demod(
                            rtl,
                            OPEN_PATH_PAGE,
                            OPEN_PATH_OFF,
                            OPEN_PATH_BIT,
                            OPEN_PATH_BIT,
                        ),
                    );
                    log.push(J::obj().with("item", OPEN_PATH_ITEM).with("record", r));
                }
                log
            }
            Baseline::ZeroIfOpen => apply_zero_if_open(rtl, rate, skip),
            Baseline::PowerOn => apply_power_on(rtl, skip),
        }
    }

    pub fn apply(self, rtl: &Rtl) -> J {
        self.apply_at(rtl, self.rate_hz(), &[])
    }

    /// The names of the baseline's settings a candidate disturbed (empty =
    /// intact).
    pub fn disturbed(self, rtl: &Rtl) -> Vec<String> {
        match self {
            Baseline::ZeroIf => step3::baseline_disturbed(rtl, step3::RATE_HZ),
            Baseline::LowIf => low_if_disturbed(rtl, step3::RATE_HZ, false),
            Baseline::LowIfOpen | Baseline::DabOpen => {
                let dab = self == Baseline::DabOpen;
                let mut bad = low_if_disturbed(rtl, self.rate_hz(), dab);
                if dab {
                    for (name, page, regs) in DAB_ITEMS {
                        for &(off, mask, val) in regs.iter() {
                            if rtl
                                .demod_read8(page, off)
                                .ok()
                                .map(|v| v & mask == val & mask)
                                != Some(true)
                            {
                                bad.push(format!("{name} p{page} 0x{off:02X}"));
                            }
                        }
                    }
                    if rtl.demod_read8(1, 0x11).ok().map(|v| v & 1 == 0) != Some(true) {
                        bad.push("en_dagc = 0".into());
                    }
                }
                if rtl
                    .demod_read8(OPEN_PATH_PAGE, OPEN_PATH_OFF)
                    .ok()
                    .map(|v| v & OPEN_PATH_BIT != 0)
                    != Some(true)
                {
                    bad.push("page 0 0x07[7] = 1".into());
                }
                bad
            }
            Baseline::ZeroIfOpen => zero_if_open_disturbed(rtl, step3::RATE_HZ),
            Baseline::PowerOn => power_on_disturbed(rtl),
        }
    }

    /// Power-cycle (unless `--no-cycle`), open, apply; the bulk counter
    /// (transfers of `buf_len` bytes) opened after the baseline as in
    /// `step3::fresh_with_baseline`.
    pub fn fresh_at(
        self,
        rate: f64,
        skip: &[String],
        buf_len: usize,
    ) -> Result<(Rtl, usb::BulkCounter, J), String> {
        let cyc = if crate::no_cycle() {
            J::obj().with(
                "skipped",
                "--no-cycle: the dongle was opened as found, not power-cycled",
            )
        } else {
            usb::power_cycle()?
        };
        let rtl = Rtl::open()?;
        let log = self.apply_at(&rtl, rate, skip);
        let bulk = usb::BulkCounter::open(&rtl, buf_len)?;
        Ok((
            rtl,
            bulk,
            J::obj()
                .with("power_cycle", cyc)
                .with("baseline", self.name())
                .with("rate_hz", rate)
                .with(
                    "skipped_items",
                    J::Arr(skip.iter().map(|s| J::from(s.as_str())).collect()),
                )
                .with("baseline_writes", log),
        ))
    }

    /// The scan's setup: zero-if exactly as the original scan did it.
    pub fn fresh(self) -> Result<(Rtl, usb::BulkCounter, J), String> {
        if self == Baseline::ZeroIf {
            return step3::fresh_with_baseline(step3::RATE_HZ, &[]);
        }
        self.fresh_at(self.rate_hz(), &[], step3::BUF_LEN)
    }
}

/// `n` 100 ms windows: their records, whether any streamed, total bytes
/// (window and stale).
/// With `content`, each window's bytes are kept long enough to compute
/// its [`Content`]; the bytes are then dropped.
fn windows_content(
    bulk: &mut usb::BulkCounter,
    rate: f64,
    n: usize,
    content: bool,
) -> (J, bool, usize, Vec<Content>) {
    let mut ws = J::Arr(Vec::new());
    let (mut any, mut total, mut cs) = (false, 0, Vec::new());
    let mut data = Vec::new();
    for _ in 0..n {
        data.clear();
        let w = bulk.measure(step3::WINDOW, if content { Some(&mut data) } else { None });
        any |= step3::streams(w.rate(), rate);
        total += w.bytes + w.stale_bytes;
        let mut rec = step3::window_record(&w, rate);
        if content {
            let c = Content::of(&data, w.elapsed.as_secs_f64());
            rec.set("c", c.compact());
            cs.push(c);
        }
        ws.push(rec);
    }
    (ws, any, total, cs)
}

/// The baseline measured alone: ten 100 ms windows. For low-if, when a
/// source is given (`--source-cmd`), also the positive control: the meter
/// of findings 4b (`if_agc_val`, MSB-first) with the tone off and on at
/// [`LOWIF_RF_HZ`], and ten more windows with the tone on; the source is
/// unkeyed and closed before the scan. Returns (record, any window
/// streamed).
pub fn solo(
    b: Baseline,
    rtl: &Rtl,
    bulk: &mut usb::BulkCounter,
) -> Result<(J, bool, Vec<Content>), String> {
    let rate = b.rate_hz();
    let open = b.is_open();
    let (ws, mut streams, total, contents) = windows_content(bulk, rate, 10, open);
    println!("{} alone: {total} B in 10 x 100 ms", b.name());
    for c in &contents {
        println!("  {}", c.compact().render());
    }
    let mut rec = J::obj()
        .with("windows", ws)
        .with("bytes_total", total)
        .with("any_window_streams", streams);
    // zero-if-open's scan runs with the tone keyed (SCAN_TONE_WHY): its
    // thresholds come from the tone-on windows.
    let mut threshold_basis: Option<Vec<Content>> = None;
    if open {
        rec.set("content_legend", content::LEGEND);
        // A held state that already delivers something other than the
        // replay is the finding itself: the scan does not start.
        let nr = nonreplay(&contents);
        rec.set("nonreplay_content", nr);
        streams |= nr;
    }
    if b == Baseline::DabOpen {
        // The one documented back-end bit dab-open leaves on, measured:
        // enable_PID page 0 0x61[7], 'Set 0 No Output' (Table 13).
        let w = rmw_demod(rtl, 0, 0x61, 0x80, 0x00);
        let (wp, _, np, cp) = windows_content(bulk, rate, 10, true);
        let r = rmw_demod(rtl, 0, 0x61, 0x80, 0x80);
        let (wr, _, nr, _) = windows_content(bulk, rate, 3, true);
        println!("enable_PID = 0: {np} B in 10 x 100 ms; restored: {nr} B in 3 windows");
        rec.set(
            "enable_pid_0",
            J::obj()
                .with("why", "enable_PID (page 0 0x61[7]) is the only documented back-end bit dab-open does not turn off (documented 'Set 0 No Output'); measured here, then restored")
                .with("write", w.map_err(|e| e.to_string()).unwrap_or_else(J::from))
                .with("windows", wp)
                .with("bytes_total", np)
                .with("content", J::Arr(cp.iter().map(Content::compact).collect()))
                .with("restore", r.map_err(|e| e.to_string()).unwrap_or_else(J::from))
                .with("windows_after_restore", wr)
                .with("bytes_after_restore", nr),
        );
    }
    if matches!(
        b,
        Baseline::LowIf | Baseline::LowIfOpen | Baseline::DabOpen | Baseline::ZeroIfOpen
    ) && source::command().is_some()
    {
        let start = Instant::now();
        let mut src = Source::open()?;
        let described = src.describe();
        let off_src = src.off()?;
        let off = Reading::take(rtl, start, false, None);
        let on_src = src.level(b.ddc_centre_rf_hz(), CONTROL_LEVEL)?;
        let on = Reading::take(rtl, start, false, None);
        let (w_on, s_on, n_on, c_on) = windows_content(bulk, rate, 10, open);
        for c in &c_on {
            println!("  tone on: {}", c.compact().render());
        }
        if open {
            let nr = nonreplay(&c_on);
            rec.set("nonreplay_content_tone_on", nr);
            streams |= nr;
        }
        let off_again = src.off()?;
        if b == Baseline::ZeroIfOpen {
            threshold_basis = Some(c_on.clone());
            rec.set("thresholds_from", "the ten tone-on windows (positive_control.windows_tone_on): the scan runs with the tone keyed (header scan_tone)");
            // The multi-offset tone test of the held state alone: every
            // state's windows judged for content, the bytes kept for the
            // spectra, then any line that moves with the tone.
            let mut states = J::Arr(Vec::new());
            let mut kept = Vec::new();
            for (name, hz) in b.tone_plan() {
                let s = match hz {
                    Some(h) => src.level(h, CONTROL_LEVEL)?,
                    None => src.off()?,
                };
                let mut all = Vec::new();
                let mut cs = Vec::new();
                let mut ws = J::Arr(Vec::new());
                for _ in 0..10 {
                    let mut data = Vec::new();
                    let w = bulk.measure(step3::WINDOW, Some(&mut data));
                    let c = Content::of(&data, w.elapsed.as_secs_f64());
                    ws.push(step3::window_record(&w, rate).with("c", c.compact()));
                    cs.push(c);
                    if all.len() < crate::confirm_write::TONE_KEEP_BYTES {
                        all.extend_from_slice(&data);
                    }
                }
                let nr = nonreplay(&cs);
                streams |= nr;
                let n: usize = cs.iter().map(|c| c.bytes).sum();
                println!(
                    "  {name}: {n} B; {}",
                    cs.first().map(|c| c.compact().render()).unwrap_or_default()
                );
                states.push(
                    J::obj()
                        .with("state", name.as_str())
                        .with("rf_hz", hz)
                        .with("source", s)
                        .with("windows", ws)
                        .with("nonreplay_content", nr),
                );
                all.truncate(crate::confirm_write::TONE_KEEP_BYTES);
                kept.push((name, hz, all));
            }
            src.off()?;
            let lines = crate::confirm_write::tone_lines(&kept);
            println!("tone lines: {}", lines.render());
            rec.set(
                "tone_sweep",
                J::obj()
                    .with(
                        "plan",
                        "Baseline::tone_plan (header baseline_definition.tone_plan)",
                    )
                    .with("states", states)
                    .with("tone_lines", lines),
            );
        }
        src.quit();
        let moved = meter::moved(&off, &on, Meter::IfAgcMsbFirst)
            .map(|(a, c, m)| J::obj().with("off", a).with("on", c).with("moved", m));
        println!(
            "{} with the tone on: {n_on} B in 10 x 100 ms; if_agc_val off -> on: {}",
            b.name(),
            moved.as_ref().map(J::render).unwrap_or_default()
        );
        streams |= s_on;
        rec.set(
            "positive_control",
            J::obj()
                .with("purpose", "the low-if baseline is the state in which step 4 measured the tone: if_agc_val (MSB-first, findings 4b, about -207 counts/dB, 8191 = no signal) must fall with the tone on")
                .with("source", described)
                .with("tone_off", off_src)
                .with("reading_off", J::obj().with("medians", off.medians()).with("snapshots", off.full()))
                .with("tone_on", on_src)
                .with("reading_on", J::obj().with("medians", on.medians()).with("snapshots", on.full()))
                .with("if_agc_val_msb_first", moved)
                .with("windows_tone_on", w_on)
                .with("bytes_total_tone_on", n_on)
                .with("any_window_streams_tone_on", s_on)
                .with("tone_off_after", off_again),
        );
    }
    Ok((rec, streams, threshold_basis.unwrap_or(contents)))
}

/// Any window that carried enough bytes to score and is not the replay
/// (period score below 1 - the detector's margin).
fn nonreplay(cs: &[Content]) -> bool {
    cs.iter().any(|c| {
        c.bytes >= content::MIN_BYTES_FOR_PERIOD
            && c.period_score
                .is_some_and(|p| p < 1.0 - content::PERIOD_MARGIN)
    })
}

/// The positive control's level: +10 dB relative, where findings 4c-4e
/// received the tone at full strength (if_agc_val about -800).
pub const CONTROL_LEVEL: Level = Level {
    txvga_db: 10,
    backoff_db: 0,
};

pub fn low_if_pll() -> Pll {
    Pll::program(
        tuner::lo_for(LOWIF_RF_HZ, tuner::IF_HZ, true),
        LOWIF_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )
    .expect("300 MHz + 4.57 MHz with /8 is inside Nint's range")
}

fn rmw_demod(rtl: &Rtl, page: u8, off: u8, mask: u8, value: u8) -> Result<J, usb::Fail> {
    let (old, back) = rtl.demod_rmw(page, off, mask, value)?;
    Ok(J::obj()
        .with("before", hex8(old))
        .with("after", hex8(back))
        .with("verified", back & mask == value & mask))
}

/// The low-if items; with `dab`, IfFreq is the DAB example's pset_iffreq
/// and Dagc writes en_dagc = 0 (dab-open's definition).
fn apply_low_if(rtl: &Rtl, rate: f64, skip: &[String], dab: bool) -> J {
    let mut log = J::Arr(Vec::new());
    for item in LOW_ITEMS
        .into_iter()
        .filter(|i| !skip.contains(&format!("{i:?}")))
    {
        log.push(
            J::obj()
                .with("item", format!("{item:?}"))
                .with("record", low_item(rtl, item, rate, dab)),
        );
    }
    log
}

/// One low-if item applied (every demodulator write a read-modify-write
/// read back); with `dab`, IfFreq and Dagc take dab-open's values.
fn low_item(rtl: &Rtl, item: LowItem, rate: f64, dab: bool) -> J {
    let d = DemodIf::default();
    {
        match item {
            LowItem::DemodCtl => step3::step(
                "DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15)",
                step3::rmw_sys8(
                    rtl,
                    DEMOD_CTL,
                    0,
                    DEMOD_CTL_PLL_EN
                        | DEMOD_CTL_ADC_I_EN
                        | DEMOD_CTL_ADC_Q_EN
                        | DEMOD_CTL_RESET_RELEASE,
                ),
            ),
            LowItem::TunerPower => step3::step(
                "GPIO3: GPD[3]=0 output, GPOE[3]=1, GPO[3]=0 tuner power on (Tables 16-19)",
                (|| {
                    let a = step3::rmw_sys8(rtl, GPD, GPIO_TUNER_POWER, 0)?;
                    let b = step3::rmw_sys8(rtl, GPOE, 0, GPIO_TUNER_POWER)?;
                    let c = step3::rmw_sys8(rtl, GPO, GPIO_TUNER_POWER, 0)?;
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(J::obj()
                        .with("GPD", a)
                        .with("GPOE", b)
                        .with("GPO", c)
                        .with("settle_ms", 50))
                })(),
            ),
            LowItem::IicRepeat => step3::step(
                "page 1 0x01[3] = 1 IIC_repeat (Table 3)",
                rmw_demod(
                    rtl,
                    IIC_REPEAT_PAGE,
                    IIC_REPEAT_OFF,
                    IIC_REPEAT_BIT_TABLE3,
                    IIC_REPEAT_BIT_TABLE3,
                ),
            ),
            LowItem::AdEnIf => step3::step(
                "page 0 0x08[7:6] = 01: AD_EN_reg = 0, AD_EN_reg1 = 1 (Table 4, IF mode 9.1)",
                rmw_demod(
                    rtl,
                    AD_EN_PAGE,
                    AD_EN_OFF,
                    AD_EN_REG | AD_EN_REG1,
                    AD_EN_REG1,
                ),
            ),
            LowItem::EnBbinOff => step3::step(
                "page 1 0xB1[0] = 0 en_bbin (IF mode 9.1)",
                rmw_demod(rtl, BBIN_PAGE, BBIN_OFF, EN_BBIN, 0),
            ),
            LowItem::IfFreq => {
                let (b, what) = if dab {
                    (
                        dab_pset_bytes(),
                        format!(
                            "pset_iffreq = {DAB_PSET_IFFREQ} = 0x2FB814, the DAB example (9.3, p. 19)"
                        ),
                    )
                } else {
                    (
                        tuner::pset_iffreq_bytes(d.if_hz),
                        format!(
                            "pset_iffreq = {} for {} Hz (Table 6, 9.3)",
                            tuner::pset_iffreq(d.if_hz),
                            d.if_hz
                        ),
                    )
                };
                step3::step(
                    &what,
                    (|| {
                        let x = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB, 0x3F, b[0])?;
                        let y = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])?;
                        let z = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])?;
                        Ok(J::Arr(vec![x, y, z]))
                    })(),
                )
            }
            LowItem::SpecInv => step3::step(
                &format!(
                    "page 1 0x15[0] = {} Spec_inv (Table 6, 9.3)",
                    d.spec_inv as u8
                ),
                rmw_demod(rtl, 1, 0x15, 0x01, d.spec_inv as u8),
            ),
            LowItem::AgcLoops => step3::step(
                "page 1 0x04[7:5] = 1 1 0: en_if_agc, en_rf_agc, aagc_hold (Table 2)",
                rmw_demod(rtl, 1, 0x04, 0xE0, 0xC0),
            ),
            LowItem::Dagc if dab => step3::step(
                "page 1 0x11[0] = 0 en_dagc: '0: Disable DAGC' (Table 10)",
                rmw_demod(rtl, 1, 0x11, 0x01, 0x00),
            ),
            LowItem::Dagc => step3::step(
                "page 1 0x11[0] = 1 en_dagc (Table 10)",
                rmw_demod(rtl, 1, 0x11, 0x01, 0x01),
            ),
            LowItem::AgcPolarity => step3::step(
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
            ),
            LowItem::Rsamp => {
                let all_but: Vec<Item> = step3::ITEMS
                    .into_iter()
                    .filter(|i| *i != Item::Rsamp)
                    .collect();
                step3::apply_baseline(rtl, rate, &all_but)
            }
            LowItem::TunerImage => {
                let front = Config::Vagc.front();
                let pll = low_if_pll();
                let img = tuner::image(&front, &pll);
                let applied = tuner::apply_image(rtl, &img);
                // [R820T] Table 1-2: PLL locking time 5 ms max.
                std::thread::sleep(Duration::from_millis(10));
                let lock = tuner::read_status(rtl).map(|s| {
                    let v = s[2] & 0x7F;
                    J::obj()
                        .with("status", tuner::status_record(&s))
                        .with("vco_indicator_in_70_127", VCO_INDICATOR_LOCKED.contains(&v))
                });
                J::obj()
                    .with(
                        "write",
                        "tuner image, vagc configuration, 300 MHz RF (findings 4a-4e)",
                    )
                    .with("config", Config::Vagc.name())
                    .with("rf_hz", LOWIF_RF_HZ)
                    .with("front", front.record())
                    .with("pll", pll.record())
                    .with("image", tuner::image_record(&img))
                    .with("applied", applied)
                    .with(
                        "lock_check_after_10_ms",
                        lock.unwrap_or_else(|e| J::from(format!("error: {e}"))),
                    )
            }
            LowItem::EpaMaxPkt | LowItem::EpaCfg | LowItem::EpaFifoReset | LowItem::Dma => {
                let name = format!("{item:?}");
                let all_but: Vec<Item> = step3::ITEMS
                    .into_iter()
                    .filter(|i| format!("{i:?}") != name)
                    .collect();
                step3::apply_baseline(rtl, rate, &all_but)
            }
        }
    }
}

/// One zero-IF documented item (step3::ITEMS) alone, as the zero-IF
/// baseline writes it.
fn zero_item(rtl: &Rtl, item: Item, rate: f64) -> J {
    let all_but: Vec<Item> = step3::ITEMS.into_iter().filter(|i| *i != item).collect();
    step3::apply_baseline(rtl, rate, &all_but)
}

/// zero-if-open's items in order (names for the record and minimisation).
pub const ZERO_OPEN_ITEMS: [&str; 16] = [
    "DemodCtl",
    "TunerPower",
    "IicRepeat",
    "AdEn",
    "EnBbin",
    "IfFreqZero",
    "AgcLoops",
    "Dagc",
    "AgcPolarity",
    "Rsamp",
    "TunerImage",
    "EpaMaxPkt",
    "EpaCfg",
    "EpaFifoReset",
    "Dma",
    OPEN_PATH_ITEM,
];

/// zero-if-open: the documented zero-IF input (9.1: AD_EN_reg = AD_EN_reg1
/// = 1, en_bbin = 1; 9.3: pset_iffreq = 0) with the tuner powered,
/// configured and tuned as low-if, the AGC items as low-if, the endpoint
/// as zero-if, and the path held open.
fn apply_zero_if_open(rtl: &Rtl, rate: f64, skip: &[String]) -> J {
    let mut log = J::Arr(Vec::new());
    for name in ZERO_OPEN_ITEMS
        .into_iter()
        .filter(|n| !skip.iter().any(|s| s == n))
    {
        let rec = match name {
            "DemodCtl" => low_item(rtl, LowItem::DemodCtl, rate, false),
            "TunerPower" => low_item(rtl, LowItem::TunerPower, rate, false),
            "IicRepeat" => low_item(rtl, LowItem::IicRepeat, rate, false),
            "AdEn" => zero_item(rtl, Item::AdEn, rate),
            "EnBbin" => zero_item(rtl, Item::EnBbin, rate),
            "IfFreqZero" => zero_item(rtl, Item::IfFreqZero, rate),
            "AgcLoops" => low_item(rtl, LowItem::AgcLoops, rate, false),
            "Dagc" => low_item(rtl, LowItem::Dagc, rate, false),
            "AgcPolarity" => low_item(rtl, LowItem::AgcPolarity, rate, false),
            "Rsamp" => low_item(rtl, LowItem::Rsamp, rate, false),
            "TunerImage" => low_item(rtl, LowItem::TunerImage, rate, false),
            "EpaMaxPkt" => low_item(rtl, LowItem::EpaMaxPkt, rate, false),
            "EpaCfg" => low_item(rtl, LowItem::EpaCfg, rate, false),
            "EpaFifoReset" => low_item(rtl, LowItem::EpaFifoReset, rate, false),
            "Dma" => low_item(rtl, LowItem::Dma, rate, false),
            _ => step3::step(
                "page 0 0x07[7] = 1: the endpoint data path open (data/step3-confirm-lowif-p0-07-bc.json)",
                rmw_demod(
                    rtl,
                    OPEN_PATH_PAGE,
                    OPEN_PATH_OFF,
                    OPEN_PATH_BIT,
                    OPEN_PATH_BIT,
                ),
            ),
        };
        log.push(J::obj().with("item", name).with("record", rec));
    }
    // Not an item: Spec_inv / En_aci (page 1 0x15) are left as found (the
    // zero-IF baseline does not write them); recorded for the header.
    log.push(
        J::obj()
            .with(
                "read_only",
                "page 1 0x15 (Spec_inv [0], En_aci [1]) as found, not written",
            )
            .with("value", rtl.demod_read8(1, 0x15).ok().map(hex8)),
    );
    log
}

fn zero_if_open_disturbed(rtl: &Rtl, rate: f64) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |name: &str, ok: Option<bool>| {
        if ok != Some(true) {
            bad.push(name.to_string());
        }
    };
    let want = DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE;
    check(
        "DEMOD_CTL",
        rtl.sys_read8(DEMOD_CTL).ok().map(|v| v & want == want),
    );
    check(
        "AD_EN = 11",
        rtl.demod_read8(AD_EN_PAGE, AD_EN_OFF)
            .ok()
            .map(|v| v & (AD_EN_REG | AD_EN_REG1) == AD_EN_REG | AD_EN_REG1),
    );
    check(
        "en_bbin = 1",
        rtl.demod_read8(BBIN_PAGE, BBIN_OFF)
            .ok()
            .map(|v| v & EN_BBIN == EN_BBIN),
    );
    check(
        "IIC_repeat",
        rtl.demod_read8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF)
            .ok()
            .map(|v| v & IIC_REPEAT_BIT_TABLE3 != 0),
    );
    check(
        "pset_iffreq = 0",
        rtl.demod_read(IFFREQ_PAGE, IFFREQ_MSB, 3)
            .ok()
            .map(|v| v[0] & 0x3F == 0 && v[1] == 0 && v[2] == 0),
    );
    let word = (rsamp_ratio(rate) & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
    let b = word.to_be_bytes();
    let masks = [0x0F, 0xFF, 0xFF, 0xFC];
    check(
        "rsamp_ratio",
        rtl.demod_read(RSAMP_PAGE, RSAMP_MSB, 4)
            .ok()
            .map(|v| (0..4).all(|k| v[k] & masks[k] == b[k] & masks[k])),
    );
    check(
        "tuner R0 = 0x96, R2 in 70..=127",
        tuner::read_status(rtl)
            .ok()
            .map(|s| s[0] == TUNER_R0_CHECK && VCO_INDICATOR_LOCKED.contains(&(s[2] & 0x7F))),
    );
    check(
        "page 0 0x07[7] = 1",
        rtl.demod_read8(OPEN_PATH_PAGE, OPEN_PATH_OFF)
            .ok()
            .map(|v| v & OPEN_PATH_BIT != 0),
    );
    bad
}

fn low_if_disturbed(rtl: &Rtl, rate: f64, dab: bool) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |name: &str, ok: Option<bool>| {
        if ok != Some(true) {
            bad.push(name.to_string());
        }
    };
    let want = DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE;
    check(
        "DEMOD_CTL",
        rtl.sys_read8(DEMOD_CTL).ok().map(|v| v & want == want),
    );
    check(
        "AD_EN = 01",
        rtl.demod_read8(AD_EN_PAGE, AD_EN_OFF)
            .ok()
            .map(|v| v & (AD_EN_REG | AD_EN_REG1) == AD_EN_REG1),
    );
    check(
        "en_bbin = 0",
        rtl.demod_read8(BBIN_PAGE, BBIN_OFF)
            .ok()
            .map(|v| v & EN_BBIN == 0),
    );
    check(
        "IIC_repeat",
        rtl.demod_read8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF)
            .ok()
            .map(|v| v & IIC_REPEAT_BIT_TABLE3 != 0),
    );
    check(
        "Spec_inv = 1",
        rtl.demod_read8(1, 0x15).ok().map(|v| v & 1 == 1),
    );
    let p = if dab {
        dab_pset_bytes()
    } else {
        tuner::pset_iffreq_bytes(tuner::IF_HZ)
    };
    check(
        "pset_iffreq",
        rtl.demod_read(IFFREQ_PAGE, IFFREQ_MSB, 3)
            .ok()
            .map(|v| v[0] & 0x3F == p[0] & 0x3F && v[1] == p[1] && v[2] == p[2]),
    );
    let word = (rsamp_ratio(rate) & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
    let b = word.to_be_bytes();
    let masks = [0x0F, 0xFF, 0xFF, 0xFC];
    check(
        "rsamp_ratio",
        rtl.demod_read(RSAMP_PAGE, RSAMP_MSB, 4)
            .ok()
            .map(|v| (0..4).all(|k| v[k] & masks[k] == b[k] & masks[k])),
    );
    check(
        "tuner R0 = 0x96, R2 in 70..=127",
        tuner::read_status(rtl)
            .ok()
            .map(|s| s[0] == TUNER_R0_CHECK && VCO_INDICATOR_LOCKED.contains(&(s[2] & 0x7F))),
    );
    bad
}

fn apply_power_on(rtl: &Rtl, skip: &[String]) -> J {
    let mut log = J::Arr(Vec::new());
    if !skip.iter().any(|s| s == POWER_ON_ITEMS[0]) {
        log.push(
            step3::step(
                "DEMOD_CTL |= PLL enable only (Table 15; findings 1C)",
                step3::rmw_sys8(rtl, DEMOD_CTL, 0, DEMOD_CTL_PLL_EN),
            )
            .with("item", POWER_ON_ITEMS[0]),
        );
    }
    if skip.iter().any(|s| s == POWER_ON_ITEMS[1]) {
        return log;
    }
    let (_, ep_addr, mps) = rtl.bulk_in().unwrap_or((0, 0, 0));
    let cfg_mask = EPA_CFG_ENABLE | EPA_CFG_TYPE_MASK | EPA_CFG_NUM_MASK | EPA_CFG_ISO_MASK;
    let cfg_want = EPA_CFG_ENABLE
        | EPA_CFG_TYPE_BULK << EPA_CFG_TYPE_SHIFT
        | (ep_addr as u32 & EPA_CFG_NUM_MASK);
    let regs: [(&str, u16, u32, u32); 4] = [
        (
            "USB_SYSCTL[0] DMA = 1 (Table 30)",
            USB_SYSCTL,
            USB_SYSCTL_DMA_EN,
            USB_SYSCTL_DMA_EN,
        ),
        (
            "USB_EPA_CFG enable, bulk, iso 00, endpoint number (Table 31)",
            USB_EPA_CFG,
            cfg_mask,
            cfg_want,
        ),
        (
            "USB_EPA_MAXPKT = descriptor wMaxPacketSize (Table 33)",
            USB_EPA_MAXPKT,
            EPA_MAXPKT_MASK,
            mps as u32 & EPA_MAXPKT_MASK,
        ),
        (
            "USB_EPA_CTL stall = 0 (Table 32)",
            USB_EPA_CTL,
            EPA_CTL_STALL,
            0,
        ),
    ];
    for (name, off, mask, want) in regs {
        let rec = match rtl.usb_read32(off) {
            Err(e) => J::obj().with("check", name).with("error", e.to_string()),
            Ok(v) if v & mask == want => J::obj()
                .with("check", name)
                .with("read", hex32(v))
                .with("as_needed", true)
                .with("written", false),
            Ok(v) => J::obj()
                .with("check", name)
                .with("read", hex32(v))
                .with("as_needed", false)
                .with(
                    "written",
                    step3::step(name, step3::rmw_usb32(rtl, off, mask, want)),
                ),
        };
        log.push(rec.with("item", POWER_ON_ITEMS[1]));
    }
    log
}

fn power_on_disturbed(rtl: &Rtl) -> Vec<String> {
    match rtl.sys_read8(DEMOD_CTL) {
        Ok(v) if v & DEMOD_CTL_PLL_EN != 0 => Vec::new(),
        Ok(v) => vec![format!("DEMOD_CTL PLL (reads {})", hex8(v).render())],
        Err(_) => vec!["DEMOD_CTL unreadable".into()],
    }
}
