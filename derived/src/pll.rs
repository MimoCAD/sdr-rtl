//! The R820T-family synthesizer: the documented words, the measured VCO
//! range and dividers, and the lock reading.
//!
//! Documented ([R820T2] pp. 9-10): Nint = 4·Ni2c + Si2c + 13 (R20),
//! Nfra = SDM_IN[16:1] / 2^16 (R21 = [8:1], R22 = [16:9]),
//! Ndiv = (Nint + Nfra) · 2, f_ref = f_xtal / 2 with REF_DIV2 = 1 ("for
//! Xtal > 24 MHz"; the crystal is 28.8 MHz), SEL_DIV (R16[7:5]) the "PLL
//! to Mixer divider" 000 /2, 001 /4, 010 /8, 011 /1. The document never
//! states f_VCO = f_ref · Ndiv; findings step 4c tested that hypothesis
//! as f_LO = f_ref · 2 · (Nint + Nfra) / divider and received the tone
//! exactly where it predicts for /2, /4 and /8 across 15 settings per
//! code, and step 4e tuned fractionally by it.
//!
//! Measured (`tables::pll`, from findings steps 4c-4d): the VCO is
//! received from 1.670 to 3.686 GHz and the driver keeps it inside
//! `VCO_RANGE_HZ` (1.71-3.65 GHz, a choice 30 MHz inside the received
//! bounds); the undescribed codes 100 and 101 divide by 32 and 64; 110
//! and 111 are unusable; /1 is documented but untestable and never
//! needed below the tuner's 1002 MHz. No code reaches an LO between
//! 114.06 and 213.75 MHz (`LO_UNREACHABLE_HZ`); that span is refused.
//!
//! R2 VCO_INDICATOR[6:0] is named and never described; step 4c read it
//! as one function of the VCO's position (0 and 63 outside the range,
//! 70-127 inside), and `R2_BY_LOCK_STATE` counts, per value, how often
//! the tone was received. The lock reading here is that table's.

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::image::{CAPX_NONE, NI2C_MASK, R16_FIXED, REF_DIV2_BIT, SEL_DIV_SHIFT, SI2C_SHIFT};
use crate::tables::pll::{F_REF_HZ, LO_REACH, LO_UNREACHABLE_HZ, NINT_RANGE, R2_BY_LOCK_STATE, REF_DIV2, SEL_DIV, VCO_RANGE_HZ};
use crate::tables::types::DividerStatus;

/// SDM_IN[16:1] is sixteen bits ([R820T2] p. 10): Nfra = sdm / 2^16.
pub const SDM_SCALE: f64 = 65536.0;
/// Nint = 4·Ni2c + Si2c + 13 ([R820T2] p. 9): R20 holds Nint − 13.
pub const NINT_OFFSET: u16 = 13;
/// R2 bit 7 reads 1 always (findings step 4c; the matrix prints it 0);
/// VCO_INDICATOR is [6:0].
pub const VCO_INDICATOR_MASK: u8 = 0x7F;

/// The LO step one SDM count makes under a divider: 2·f_ref / 2^16 / div
/// (439.45 Hz / div).
pub fn sdm_step_hz(divider: u32) -> f64 {
    2.0 * F_REF_HZ / SDM_SCALE / divider as f64
}

/// The synthesizer's programmed words. `lo_hz()` and `vco_hz()` are
/// derived from these words, never echoed from a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pll {
    /// SEL_DIV (R16[7:5]).
    pub sel_div: u8,
    /// The mixer divider the code gives (`tables::pll::SEL_DIV`).
    pub divider: u32,
    /// Nint, 13..=268.
    pub nint: u16,
    /// SDM_IN[16:1] as one 16-bit integer.
    pub sdm: u16,
}

/// The codes the driver may program, smallest divider first: the
/// documented codes the sweep confirmed and the undescribed ones lo-find
/// measured. Smallest divider first is the rule every band reading was
/// taken under (the harness's `choose_div`), so the band table's
/// conditions are reproduced.
pub fn usable_codes() -> Vec<(u8, u32)> {
    let mut v: Vec<(u8, u32)> = SEL_DIV
        .iter()
        .filter_map(|e| match e.status {
            DividerStatus::DocumentedConfirmed { .. } | DividerStatus::Measured { .. } => e.divider.map(|d| (e.code, d)),
            DividerStatus::DocumentedUntested { .. } | DividerStatus::Unusable { .. } => None,
        })
        .collect();
    v.sort_by_key(|&(_, d)| d);
    v
}

/// The divider of a SEL_DIV code, documented or measured; None for the
/// unusable codes.
pub fn divider_of(sel_div: u8) -> Option<u32> {
    SEL_DIV.iter().find(|e| e.code == sel_div).and_then(|e| e.divider)
}

fn unsupported(what: String) -> Error {
    Error::Unsupported { backend: BACKEND, what }
}

/// Why an LO cannot be programmed: inside the measured gap, or outside
/// every code's reach.
fn unreachable(lo_hz: f64) -> Error {
    for &(gap_lo, gap_hi) in &LO_UNREACHABLE_HZ {
        if lo_hz > gap_lo && lo_hz < gap_hi {
            return unsupported(format!(
                "LO {:.4} MHz lies in {:.4}-{:.4} MHz, which no SEL_DIV code reaches with the VCO inside {:.2}-{:.2} GHz \
                 (findings step 4d: /32 tops out at {:.4} MHz, /8 starts at {:.4} MHz, and no code divides by 16); \
                 whether the VCO runs lower under settings the sweep did not vary is an open search",
                lo_hz / 1e6,
                gap_lo / 1e6,
                gap_hi / 1e6,
                VCO_RANGE_HZ.0 / 1e9,
                VCO_RANGE_HZ.1 / 1e9,
                gap_lo / 1e6,
                gap_hi / 1e6,
            ));
        }
    }
    let usable = usable_codes();
    let reach = LO_REACH.iter().filter(|r| usable.iter().any(|&(c, _)| c == r.sel_div));
    let lowest = reach.clone().map(|r| r.lo_min_hz).fold(f64::INFINITY, f64::min);
    let highest = reach.map(|r| r.lo_max_hz).fold(0.0, f64::max);
    unsupported(format!(
        "LO {:.4} MHz is outside the span the usable dividers reach, {:.4} MHz to {:.4} MHz (tables::pll::LO_REACH)",
        lo_hz / 1e6,
        lowest / 1e6,
        highest / 1e6
    ))
}

impl Pll {
    /// The words for an LO: the smallest usable divider whose VCO lands
    /// inside `VCO_RANGE_HZ`, then Nint + Nfra = f_LO · div / (2 f_ref).
    pub fn program(lo_hz: f64) -> Result<Pll> {
        if !lo_hz.is_finite() || lo_hz <= 0.0 {
            return Err(unsupported(format!("LO {lo_hz} Hz is not a frequency")));
        }
        for (code, div) in usable_codes() {
            let vco = lo_hz * div as f64;
            if vco >= VCO_RANGE_HZ.0 && vco <= VCO_RANGE_HZ.1 {
                return Pll::with_divider(lo_hz, code);
            }
        }
        Err(unreachable(lo_hz))
    }

    /// The words for an LO under a given code (no VCO-range check: the
    /// caller's choice). Nint is floor(n), the SDM word the rounded
    /// fraction, carried into Nint when it rounds to 2^16.
    pub fn with_divider(lo_hz: f64, sel_div: u8) -> Result<Pll> {
        let divider = divider_of(sel_div)
            .ok_or_else(|| unsupported(format!("SEL_DIV {sel_div:03b} is not a usable divider (tables::pll::SEL_DIV)")))?;
        let n = lo_hz * divider as f64 / (2.0 * F_REF_HZ);
        let mut nint = n.floor() as i64;
        let mut sdm = ((n - nint as f64) * SDM_SCALE).round() as i64;
        if sdm >= SDM_SCALE as i64 {
            nint += 1;
            sdm = 0;
        }
        if nint < NINT_RANGE.0 as i64 || nint > NINT_RANGE.1 as i64 {
            return Err(unsupported(format!(
                "Nint {nint} outside {}..={} for an LO of {lo_hz} Hz with divider {divider} (SEL_DIV {sel_div:03b})",
                NINT_RANGE.0, NINT_RANGE.1
            )));
        }
        Ok(Pll { sel_div, divider, nint: nint as u16, sdm: sdm as u16 })
    }

    /// f_LO = f_ref · 2 · (Nint + Nfra) / divider, from the words.
    pub fn lo_hz(&self) -> f64 {
        self.vco_hz() / self.divider as f64
    }

    /// f_VCO = f_ref · 2 · (Nint + Nfra), the relation findings step 4c
    /// confirmed.
    pub fn vco_hz(&self) -> f64 {
        F_REF_HZ * 2.0 * (self.nint as f64 + self.sdm as f64 / SDM_SCALE)
    }

    /// R16: SEL_DIV [7:5], REF_DIV2 [4] (1: the sweep's setting, which
    /// every table holds for), the fixed 0 1 at [3:2], CAPX [1:0] = 00.
    pub fn r16(&self) -> u8 {
        (self.sel_div & 0b111) << SEL_DIV_SHIFT | if REF_DIV2 { REF_DIV2_BIT } else { 0 } | R16_FIXED | CAPX_NONE
    }

    /// R20: SI2C [7:6], NI2C [5:0] with Nint − 13 = 4·Ni2c + Si2c.
    pub fn r20(&self) -> u8 {
        let m = self.nint - NINT_OFFSET;
        ((m & 0b11) as u8) << SI2C_SHIFT | ((m >> 2) as u8 & NI2C_MASK)
    }

    /// R21: SDM_IN[8:1].
    pub fn r21(&self) -> u8 {
        self.sdm as u8
    }

    /// R22: SDM_IN[16:9].
    pub fn r22(&self) -> u8 {
        (self.sdm >> 8) as u8
    }

    /// The words back from the four register bytes (R16, R20, R21, R22):
    /// None when REF_DIV2 is not the sweep's 1 or the code is unusable.
    pub fn from_registers(r16: u8, r20: u8, r21: u8, r22: u8) -> Option<Pll> {
        if (r16 & REF_DIV2_BIT != 0) != REF_DIV2 {
            return None;
        }
        let sel_div = r16 >> SEL_DIV_SHIFT;
        let divider = divider_of(sel_div)?;
        let si2c = (r20 >> SI2C_SHIFT) as u16;
        let ni2c = (r20 & NI2C_MASK) as u16;
        let nint = 4 * ni2c + si2c + NINT_OFFSET;
        let sdm = (r22 as u16) << 8 | r21 as u16;
        Some(Pll { sel_div, divider, nint, sdm })
    }
}

/// The span of R2 values the sweep read with the tone received at full
/// strength more often than not received at all (`R2_BY_LOCK_STATE`):
/// 70..=127. 0 and 63 lie outside it (out of range on either side,
/// findings step 4c), and the values between the sweep's steps (its
/// Nint step is 5; step 4e read 93 at Nint 84 with the tone received)
/// lie inside it.
pub fn r2_locked_span() -> (u8, u8) {
    let inside = R2_BY_LOCK_STATE.iter().filter(|c| c.full_strength > c.not_received).map(|c| c.r2);
    let lo = inside.clone().min().unwrap_or(u8::MAX);
    let hi = inside.max().unwrap_or(0);
    (lo, hi)
}

/// Whether R2 says the VCO is inside its range: VCO_INDICATOR[6:0] is
/// within [`r2_locked_span`]. A reading, not a proof.
pub fn r2_says_locked(r2: u8) -> bool {
    let v = r2 & VCO_INDICATOR_MASK;
    let (lo, hi) = r2_locked_span();
    v >= lo && v <= hi
}

/// The synthesizer as a reader sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PllState {
    pub sel_div: u8,
    pub divider: u32,
    pub nint: u16,
    pub sdm: u16,
    pub lo_hz: f64,
    pub vco_hz: f64,
    /// R2 VCO_INDICATOR[6:0] as last read.
    pub vco_indicator: u8,
    /// [`r2_says_locked`] of that reading.
    pub locked: bool,
}

impl PllState {
    pub fn new(pll: &Pll, r2: u8) -> PllState {
        PllState {
            sel_div: pll.sel_div,
            divider: pll.divider,
            nint: pll.nint,
            sdm: pll.sdm,
            lo_hz: pll.lo_hz(),
            vco_hz: pll.vco_hz(),
            vco_indicator: r2 & VCO_INDICATOR_MASK,
            locked: r2_says_locked(r2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::band::IF_HZ;

    #[test]
    fn usable_codes_are_the_confirmed_and_measured_dividers_ascending() {
        assert_eq!(usable_codes(), vec![(0b000, 2), (0b001, 4), (0b010, 8), (0b100, 32), (0b101, 64)]);
        assert_eq!(divider_of(0b011), Some(1), "documented /1, untested");
        assert_eq!(divider_of(0b110), None);
        assert_eq!(divider_of(0b111), None);
    }

    /// Findings step 4e: 300 MHz → /8, Nint 84, SDM 39504, LO 304.570 MHz
    /// exactly; the recorded words R16 0x54, R20 0xD1, R21 0x50, R22 0x9A
    /// (tables::tuner_image's header).
    #[test]
    fn the_300_mhz_tune_matches_the_record() {
        let p = Pll::program(300e6 + IF_HZ).unwrap();
        assert_eq!(p, Pll { sel_div: 0b010, divider: 8, nint: 84, sdm: 39504 });
        assert_eq!((p.r16(), p.r20(), p.r21(), p.r22()), (0x54, 0xD1, 0x50, 0x9A));
        assert!((p.lo_hz() - 304570019.53125).abs() < 1e-3, "{}", p.lo_hz());
        assert!((p.vco_hz() - 2436560156.25).abs() < 1e-3);
        assert_eq!(Pll::from_registers(0x54, 0xD1, 0x50, 0x9A), Some(p));
    }

    /// Findings step 4b: SEL_DIV 010 with Nint 84 for a 304.57 MHz LO;
    /// step 4d: Nint 88 is f_VCO 2.534 GHz.
    #[test]
    fn nint_to_vco_follows_the_sweep() {
        let p = Pll { sel_div: 0b010, divider: 8, nint: 88, sdm: 0 };
        assert!((p.vco_hz() - 2534.4e6).abs() < 1.0);
        assert!((p.lo_hz() - 316.8e6).abs() < 1.0);
        let r = Pll { sel_div: 0b100, divider: 32, nint: 88, sdm: 0 };
        assert!((r.lo_hz() - 79.2e6).abs() < 1.0, "step 4d: code 100 at Nint 88 is an LO near 79 MHz");
        let s = Pll { sel_div: 0b101, divider: 64, nint: 88, sdm: 0 };
        assert!((s.lo_hz() - 39.6e6).abs() < 1.0);
    }

    /// Every RF of the tuner's range outside the gap programs: the
    /// divider is the smallest whose VCO is in range, the LO derived
    /// from the words is within half an SDM step of the request, and
    /// the words decode back to the same PLL.
    #[test]
    fn the_grid_round_trips_within_the_sdm_resolution() {
        let mut programmed = 0;
        let mut refused = 0;
        let mut rf = 42.0e6;
        while rf <= 1002.0e6 {
            let lo = rf + IF_HZ;
            match Pll::program(lo) {
                Ok(p) => {
                    programmed += 1;
                    let vco = p.vco_hz();
                    assert!(vco >= VCO_RANGE_HZ.0 - 1.0 && vco <= VCO_RANGE_HZ.1 + 1.0, "{rf}: VCO {vco}");
                    assert!((p.lo_hz() - lo).abs() <= sdm_step_hz(p.divider) / 2.0 + 1e-6, "{rf}: LO {} vs {lo}", p.lo_hz());
                    for (code, div) in usable_codes() {
                        if div < p.divider {
                            let v = lo * div as f64;
                            assert!(v < VCO_RANGE_HZ.0 || v > VCO_RANGE_HZ.1, "{rf}: code {code:03b} (/{div}) was in range but not chosen");
                        }
                    }
                    assert_eq!(Pll::from_registers(p.r16(), p.r20(), p.r21(), p.r22()), Some(p));
                    assert!(p.nint >= NINT_RANGE.0 && p.nint <= NINT_RANGE.1);
                }
                Err(Error::Unsupported { what, .. }) => {
                    refused += 1;
                    assert!(lo > LO_UNREACHABLE_HZ[0].0 && lo < LO_UNREACHABLE_HZ[0].1, "{rf}: refused outside the gap: {what}");
                    assert!(what.contains("no SEL_DIV code reaches"), "{what}");
                }
                Err(e) => panic!("{rf}: {e}"),
            }
            rf += 0.25e6;
        }
        assert!(programmed > 3000, "{programmed}");
        assert!(refused > 350, "{refused}");
    }

    /// The gap's edges in RF: 109 MHz still reaches (/32), 110 does not;
    /// 209 does not, 209.25 does (/8).
    #[test]
    fn the_gap_edges() {
        assert_eq!(Pll::program(109.0e6 + IF_HZ).unwrap().divider, 32);
        assert!(Pll::program(110.0e6 + IF_HZ).is_err());
        assert!(Pll::program(150.0e6 + IF_HZ).is_err());
        assert!(Pll::program(209.0e6 + IF_HZ).is_err());
        assert_eq!(Pll::program(209.25e6 + IF_HZ).unwrap().divider, 8);
        // Below /64's reach and above /2's.
        assert!(Pll::program(20.0e6).is_err());
        assert!(Pll::program(1.9e9).is_err());
        assert!(Pll::program(0.0).is_err());
        assert!(Pll::program(f64::NAN).is_err());
    }

    /// Divider choice across the LO spans of `LO_REACH`.
    #[test]
    fn divider_choice_follows_the_reach_table() {
        assert_eq!(Pll::program(40.0e6).unwrap().divider, 64);
        assert_eq!(Pll::program(80.0e6).unwrap().divider, 32);
        assert_eq!(Pll::program(55.0e6).unwrap().divider, 32, "the smaller divider wins an overlap");
        assert_eq!(Pll::program(300.0e6).unwrap().divider, 8);
        assert_eq!(Pll::program(440.0e6).unwrap().divider, 4, "/4 at 1.76 GHz beats /8 at 3.52 GHz");
        assert_eq!(Pll::program(900.0e6).unwrap().divider, 2);
        assert_eq!(Pll::program(1006.57e6).unwrap().divider, 2);
    }

    #[test]
    fn r20_packs_nint_as_the_document_says() {
        // Nint 21: m = 8 = 4·2 + 0 → SI2C 0, NI2C 2.
        let p = Pll { sel_div: 0, divider: 2, nint: 21, sdm: 0 };
        assert_eq!(p.r20(), 0x02);
        // Nint 268 = 13 + 4·63 + 3 → SI2C 3, NI2C 63.
        let q = Pll { sel_div: 0, divider: 2, nint: 268, sdm: 0xFFFF };
        assert_eq!(q.r20(), 0xFF);
        assert_eq!((q.r21(), q.r22()), (0xFF, 0xFF));
        assert_eq!(Pll::from_registers(q.r16(), q.r20(), q.r21(), q.r22()), Some(q));
        // REF_DIV2 = 0 is not a setting the tables hold for.
        assert_eq!(Pll::from_registers(0x44, 0xD1, 0x50, 0x9A), None);
        // An unusable code decodes to nothing.
        assert_eq!(Pll::from_registers(0b110 << 5 | 0x14, 0xD1, 0x50, 0x9A), None);
    }

    /// Findings step 4c: 0 and 63 are out of range, 70-127 inside; bit 7
    /// reads 1 always and is masked; step 4e's 93 (between the sweep's
    /// steps) is inside.
    #[test]
    fn r2_lock_reading() {
        assert_eq!(r2_locked_span(), (70, 127));
        assert!(!r2_says_locked(0));
        assert!(!r2_says_locked(63));
        assert!(!r2_says_locked(0x80 | 63));
        assert!(!r2_says_locked(1));
        assert!(!r2_says_locked(69));
        for v in [70u8, 77, 83, 87, 91, 93, 95, 103, 108, 112, 116, 120, 122, 125, 127] {
            assert!(r2_says_locked(v), "{v}");
            assert!(r2_says_locked(0x80 | v), "{v} with bit 7");
        }
    }
}
