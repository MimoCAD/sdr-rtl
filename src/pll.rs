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
//! and 111 are unusable. Code 011, which [R820T2] prints as /1 and the
//! generated table (taken before step 10) carries as documented and
//! untested, **divides by 16** (findings 10.5: Nint 100, SDM 7646 put
//! the DDC centre at 176.64 MHz, where /16 predicts, and the
//! hypothesis-free sweep answered only there); the driver overrides the
//! table's entry here rather than edit the generated copy. With /16 the
//! usable codes reach every LO from 26.72 MHz to 1825 MHz with no gap
//! ([`lo_reach`], [`lo_gaps`]): findings 4d's 114.06-213.75 MHz gap is
//! closed by /16's 106.875-228.125 MHz.
//!
//! R2 VCO_INDICATOR[6:0] is named and never described; step 4c read it
//! as one function of the VCO's position (0 and 63 outside the range,
//! 70-127 inside) and step 14 filled the bottom in (64-69 inside), and
//! `R2_BY_LOCK_STATE` counts, per value, how often the tone was
//! received. The lock reading here is that table's.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::image::{CAPX_NONE, NI2C_MASK, R16_FIXED, REF_DIV2_BIT, SEL_DIV_SHIFT, SI2C_SHIFT};
use crate::math::{floor, round};
use crate::tables::pll::{F_REF_HZ, NINT_RANGE, R2_BY_LOCK_STATE, REF_DIV2, SEL_DIV, VCO_RANGE_HZ};
use crate::tables::types::DividerStatus;

/// SEL_DIV 011 and its measured divider, 16 (findings 10.5), in place of
/// the generated table's documented-untested /1.
pub const SEL_DIV_BY_16: u8 = 0b011;
pub const BY_16: u32 = 16;

/// SDM_IN[16:1] is sixteen bits ([R820T2] p. 10): Nfra = sdm / 2^16.
pub const SDM_SCALE: f64 = 65536.0;
/// Nint = 4·Ni2c + Si2c + 13 ([R820T2] p. 9): R20 holds Nint − 13.
pub const NINT_OFFSET: u16 = 13;
/// R2 bit 7 reads 1 always (findings step 4c; the matrix prints it 0);
/// VCO_INDICATOR is [6:0].
pub const VCO_INDICATOR_MASK: u8 = 0x7F;

/// What the synthesizer runs at against its words, at the VCO: a quarter
/// of an SDM count above them, 2·f_ref / 2^16 / 4 = 109.86 Hz (findings
/// 26). Measured by tuning one RF under two dividers back to back (the
/// crystal cannot move between them), where the two errors differ by the
/// offset × (1/d₁ − 1/d₂): 110.03 ± 0.69 Hz on dongle A and 109.66 ±
/// 0.49 Hz on dongle B, 32 pairs each, at 105-890 MHz across every pair of
/// neighbouring dividers /2-/32. (A sweep fit of crystal and offset
/// together had read 123 Hz: the crystal drifts during a sweep and the
/// fit trades one for the other.)
pub const VCO_OFFSET_HZ: f64 = 2.0 * F_REF_HZ / SDM_SCALE / 4.0;

/// The LO step one SDM count makes under a divider: 2·f_ref / 2^16 / div
/// (439.45 Hz / div). Tests only: the tolerance they hold tunings to.
#[cfg(test)]
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
/// documented codes the sweep confirmed, the undescribed ones lo-find
/// measured, and 011 = /16 (findings 10.5). Smallest divider first is
/// the rule every band reading was taken under (the harness's
/// `choose_div`), so the band table's conditions are reproduced wherever
/// the band table has readings; /16 wins only where no smaller divider
/// keeps the VCO in range (LO 106.875-213.75 MHz).
pub fn usable_codes() -> Vec<(u8, u32)> {
    let mut v: Vec<(u8, u32)> = SEL_DIV
        .iter()
        .filter(|e| e.code != SEL_DIV_BY_16)
        .filter_map(|e| match e.status {
            DividerStatus::DocumentedConfirmed { .. } | DividerStatus::Measured { .. } => {
                e.divider.map(|d| (e.code, d))
            }
            DividerStatus::DocumentedUntested { .. } | DividerStatus::Unusable { .. } => None,
        })
        .collect();
    v.push((SEL_DIV_BY_16, BY_16));
    v.sort_by_key(|&(_, d)| d);
    v
}

/// The divider of a SEL_DIV code: measured (011 = /16, findings 10.5),
/// or the table's documented or measured one; None for the unusable
/// codes.
pub fn divider_of(sel_div: u8) -> Option<u32> {
    if sel_div == SEL_DIV_BY_16 {
        return Some(BY_16);
    }
    SEL_DIV
        .iter()
        .find(|e| e.code == sel_div)
        .and_then(|e| e.divider)
}

/// The LO span one usable code reaches with the VCO inside
/// `VCO_RANGE_HZ`: (code, divider, lowest LO, highest LO), ascending.
/// Computed here rather than read from `tables::pll::LO_REACH`, which
/// predates /16.
pub fn lo_reach() -> Vec<(u8, u32, f64, f64)> {
    let mut v: Vec<_> = usable_codes()
        .into_iter()
        .map(|(c, d)| (c, d, VCO_RANGE_HZ.0 / d as f64, VCO_RANGE_HZ.1 / d as f64))
        .collect();
    v.sort_by(|a, b| a.2.total_cmp(&b.2));
    v
}

/// LO spans between the lowest and the highest reachable LO that no
/// usable code covers. Empty with /16 (tested).
pub fn lo_gaps() -> Vec<(f64, f64)> {
    let mut gaps = Vec::new();
    let mut covered_to = f64::NEG_INFINITY;
    for (_, _, lo, hi) in lo_reach() {
        if covered_to.is_finite() && lo > covered_to {
            gaps.push((covered_to, lo));
        }
        covered_to = covered_to.max(hi);
    }
    gaps
}

fn unsupported(what: String) -> Error {
    Error::Unsupported {
        backend: BACKEND,
        what,
    }
}

/// Why an LO cannot be programmed: inside a gap between the codes'
/// reaches, or outside them all.
fn unreachable(lo_hz: f64) -> Error {
    for (gap_lo, gap_hi) in lo_gaps() {
        if lo_hz > gap_lo && lo_hz < gap_hi {
            return unsupported(format!(
                "LO {:.4} MHz lies in {:.4}-{:.4} MHz, which no usable SEL_DIV code reaches with the VCO inside {:.2}-{:.2} GHz",
                lo_hz / 1e6,
                gap_lo / 1e6,
                gap_hi / 1e6,
                VCO_RANGE_HZ.0 / 1e9,
                VCO_RANGE_HZ.1 / 1e9,
            ));
        }
    }
    let reach = lo_reach();
    let lowest = reach.iter().map(|r| r.2).fold(f64::INFINITY, f64::min);
    let highest = reach.iter().map(|r| r.3).fold(0.0, f64::max);
    unsupported(format!(
        "LO {:.4} MHz is outside the span the usable dividers reach with the VCO inside {:.2}-{:.2} GHz, {:.4} MHz to {:.4} MHz",
        lo_hz / 1e6,
        VCO_RANGE_HZ.0 / 1e9,
        VCO_RANGE_HZ.1 / 1e9,
        lowest / 1e6,
        highest / 1e6
    ))
}

impl Pll {
    /// The words for an LO: the smallest usable divider whose VCO lands
    /// inside `VCO_RANGE_HZ`, then Nint + Nfra = f_LO · div / (2 f_ref).
    pub fn program(lo_hz: f64) -> Result<Pll> {
        Pll::candidates(lo_hz)?
            .into_iter()
            .next()
            .ok_or_else(|| unreachable(lo_hz))
    }

    /// The words for an LO under every usable divider whose VCO lands inside
    /// `VCO_RANGE_HZ`, smallest divider first: the order a tuning tries them
    /// until one locks. The range is the documented one; where a unit's
    /// VCO actually starts varies (findings 21: two dongles did not lock
    /// below 1.72 GHz at /2, one locked down to 1.709), so the first may
    /// not lock and the next is the fallback.
    pub fn candidates(lo_hz: f64) -> Result<Vec<Pll>> {
        if !lo_hz.is_finite() || lo_hz <= 0.0 {
            return Err(unsupported(format!("LO {lo_hz} Hz is not a frequency")));
        }
        let v = usable_codes()
            .into_iter()
            .filter(|&(_, div)| (VCO_RANGE_HZ.0..=VCO_RANGE_HZ.1).contains(&(lo_hz * div as f64)))
            .map(|(code, _)| Pll::with_divider(lo_hz, code))
            .collect::<Result<Vec<Pll>>>()?;
        if v.is_empty() {
            return Err(unreachable(lo_hz));
        }
        Ok(v)
    }

    /// The words for an LO under a given code (no VCO-range check: the
    /// caller's choice). Nint is floor(n), the SDM word the rounded
    /// fraction, carried into Nint when it rounds to 2^16.
    pub fn with_divider(lo_hz: f64, sel_div: u8) -> Result<Pll> {
        let divider = divider_of(sel_div).ok_or_else(|| {
            unsupported(format!(
                "SEL_DIV {sel_div:03b} is not a usable divider (tables::pll::SEL_DIV)"
            ))
        })?;
        let n = lo_hz * divider as f64 / (2.0 * F_REF_HZ);
        let mut nint = floor(n) as i64;
        let mut sdm = round((n - nint as f64) * SDM_SCALE) as i64;
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
        Ok(Pll {
            sel_div,
            divider,
            nint: nint as u16,
            sdm: sdm as u16,
        })
    }

    /// The words under every candidate divider ([`Pll::candidates`]) for
    /// the synthesizer to deliver `lo_hz`: each divider's words for lo −
    /// [`VCO_OFFSET_HZ`] / divider, the offset the synthesizer runs at
    /// from its words (findings 26).
    pub fn candidates_delivering(lo_hz: f64) -> Result<Vec<Pll>> {
        Pll::candidates(lo_hz)?
            .into_iter()
            .map(|p| Pll::with_divider(lo_hz - VCO_OFFSET_HZ / p.divider as f64, p.sel_div))
            .collect()
    }

    /// The LO the words deliver: (f_VCO + [`VCO_OFFSET_HZ`]) / divider
    /// (findings 26).
    pub fn lo_delivered_hz(&self) -> f64 {
        (self.vco_hz() + VCO_OFFSET_HZ) / self.divider as f64
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
        (self.sel_div & 0b111) << SEL_DIV_SHIFT
            | if REF_DIV2 { REF_DIV2_BIT } else { 0 }
            | R16_FIXED
            | CAPX_NONE
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
    /// Tests only: the round trip of the words.
    #[cfg(test)]
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
        Some(Pll {
            sel_div,
            divider,
            nint,
            sdm,
        })
    }
}

/// The span of R2 values read with the tone received at full strength
/// more often than not received at all (`R2_BY_LOCK_STATE`, steps 4c and
/// 14): 64..=127. 0 and 63 lie outside it (the range's ends on either
/// side); 64-69, which step 4c's Nint steps of 5 never read, are step
/// 14's bottom 90 MHz of the VCO; step 4e's 93 (between step 4c's steps,
/// the tone received) lies inside.
pub fn r2_locked_span() -> (u8, u8) {
    let inside = R2_BY_LOCK_STATE
        .iter()
        .filter(|c| c.full_strength > c.not_received)
        .map(|c| c.r2);
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
    use crate::tables::band_paths::IF_HZ;
    use alloc::vec;

    #[test]
    fn usable_codes_are_the_confirmed_and_measured_dividers_ascending() {
        assert_eq!(
            usable_codes(),
            vec![
                (0b000, 2),
                (0b001, 4),
                (0b010, 8),
                (0b011, 16),
                (0b100, 32),
                (0b101, 64)
            ]
        );
        assert_eq!(
            divider_of(0b011),
            Some(16),
            "findings 10.5, not the printed /1"
        );
        assert_eq!(divider_of(0b110), None);
        assert_eq!(divider_of(0b111), None);
    }

    /// The reach per code and no gap: /16 covers 106.875-228.125 MHz,
    /// overlapping /32's top (114.0625) and /8's bottom (213.75).
    #[test]
    fn the_reach_has_no_gap_with_16() {
        assert!(lo_gaps().is_empty(), "{:?}", lo_gaps());
        let reach = lo_reach();
        assert_eq!(reach.len(), 6);
        let by16 = reach.iter().find(|r| r.1 == 16).unwrap();
        assert_eq!((by16.0, by16.2, by16.3), (0b011, 106.875e6, 228.125e6));
        let by32 = reach.iter().find(|r| r.1 == 32).unwrap();
        let by8 = reach.iter().find(|r| r.1 == 8).unwrap();
        assert!(by16.2 < by32.3 && by16.3 > by8.2);
        assert_eq!(reach.first().unwrap().2, 26.71875e6);
        assert_eq!(reach.last().unwrap().3, 1825.0e6);
    }

    /// Findings 10.5 (and 9h item 9): SEL_DIV 011, REF_DIV2 1, Nint 100,
    /// SDM 7646 is LO 180.21 MHz under /16, the DDC centre 176.64 MHz
    /// (LO − the stream's IF). The words decode to that LO, and
    /// programming the centre's LO gives those words back.
    #[test]
    fn sel_div_011_divides_by_16_as_benched() {
        use crate::demod::ddc_if_hz;
        let bench = Pll {
            sel_div: 0b011,
            divider: 16,
            nint: 100,
            sdm: 7646,
        };
        let regs = (bench.r16(), bench.r20(), bench.r21(), bench.r22());
        assert_eq!(regs.0 >> SEL_DIV_SHIFT, 0b011);
        assert_ne!(regs.0 & REF_DIV2_BIT, 0);
        let back = Pll::from_registers(regs.0, regs.1, regs.2, regs.3).unwrap();
        assert_eq!(back, bench);
        // f_LO = 14.4 MHz · 2 · (100 + 7646/65536) / 16.
        let lo = back.lo_hz();
        assert!((lo - 14.4e6 * 2.0 * (100.0 + 7646.0 / 65536.0) / 16.0).abs() < 1e-3);
        assert!((lo - 180.21e6).abs() < 1e3, "{lo}");
        assert!(
            (lo - ddc_if_hz() - 176.64e6).abs() < 2e3,
            "{}",
            lo - ddc_if_hz()
        );
        assert!(
            (back.vco_hz() - 2883.36e6).abs() < 0.1e6,
            "inside the VCO range: {}",
            back.vco_hz()
        );
        let p = Pll::program(176.64e6 + ddc_if_hz()).unwrap();
        assert_eq!(p, bench);
        // The control of findings 10.5: SEL_DIV 010 at the same words is
        // /8, twice the LO.
        let control = Pll::from_registers(
            (0b010 << SEL_DIV_SHIFT) | (regs.0 & !(0b111 << SEL_DIV_SHIFT)),
            regs.1,
            regs.2,
            regs.3,
        )
        .unwrap();
        assert_eq!(control.divider, 8);
        assert!((control.lo_hz() - 2.0 * lo).abs() < 1e-3);
    }

    /// Findings step 4e: 300 MHz → /8, Nint 84, SDM 39504, LO 304.570 MHz
    /// exactly; the recorded words R16 0x54, R20 0xD1, R21 0x50, R22 0x9A
    /// (tables::tuner_image's header).
    #[test]
    fn the_300_mhz_tune_matches_the_record() {
        let p = Pll::program(300e6 + IF_HZ).unwrap();
        assert_eq!(
            p,
            Pll {
                sel_div: 0b010,
                divider: 8,
                nint: 84,
                sdm: 39504
            }
        );
        assert_eq!(
            (p.r16(), p.r20(), p.r21(), p.r22()),
            (0x54, 0xD1, 0x50, 0x9A)
        );
        assert!((p.lo_hz() - 304570019.53125).abs() < 1e-3, "{}", p.lo_hz());
        assert!((p.vco_hz() - 2436560156.25).abs() < 1e-3);
        assert_eq!(Pll::from_registers(0x54, 0xD1, 0x50, 0x9A), Some(p));
    }

    /// Findings step 4b: SEL_DIV 010 with Nint 84 for a 304.57 MHz LO;
    /// step 4d: Nint 88 is f_VCO 2.534 GHz.
    #[test]
    fn nint_to_vco_follows_the_sweep() {
        let p = Pll {
            sel_div: 0b010,
            divider: 8,
            nint: 88,
            sdm: 0,
        };
        assert!((p.vco_hz() - 2534.4e6).abs() < 1.0);
        assert!((p.lo_hz() - 316.8e6).abs() < 1.0);
        let r = Pll {
            sel_div: 0b100,
            divider: 32,
            nint: 88,
            sdm: 0,
        };
        assert!(
            (r.lo_hz() - 79.2e6).abs() < 1.0,
            "step 4d: code 100 at Nint 88 is an LO near 79 MHz"
        );
        let s = Pll {
            sel_div: 0b101,
            divider: 64,
            nint: 88,
            sdm: 0,
        };
        assert!((s.lo_hz() - 39.6e6).abs() < 1.0);
    }

    /// Every RF of the tuner's range programs, at the stream's IF and at
    /// the step-4 tables' IF: the divider is the smallest whose VCO is
    /// in range, the LO derived from the words is within half an SDM
    /// step of the request, and the words decode back to the same PLL.
    #[test]
    fn the_grid_round_trips_within_the_sdm_resolution() {
        for if_hz in [crate::demod::ddc_if_hz(), IF_HZ] {
            let mut programmed = 0;
            let mut by16 = 0;
            let mut rf = 42.0e6;
            while rf <= 1002.0e6 {
                let lo = rf + if_hz;
                let p = Pll::program(lo).unwrap_or_else(|e| panic!("{rf}: {e}"));
                programmed += 1;
                if p.divider == BY_16 {
                    by16 += 1;
                }
                let vco = p.vco_hz();
                assert!(
                    vco >= VCO_RANGE_HZ.0 - 1.0 && vco <= VCO_RANGE_HZ.1 + 1.0,
                    "{rf}: VCO {vco}"
                );
                assert!(
                    (p.lo_hz() - lo).abs() <= sdm_step_hz(p.divider) / 2.0 + 1e-6,
                    "{rf}: LO {} vs {lo}",
                    p.lo_hz()
                );
                for (code, div) in usable_codes() {
                    if div < p.divider {
                        let v = lo * div as f64;
                        assert!(
                            v < VCO_RANGE_HZ.0 || v > VCO_RANGE_HZ.1,
                            "{rf}: code {code:03b} (/{div}) was in range but not chosen"
                        );
                    }
                }
                assert_eq!(
                    Pll::from_registers(p.r16(), p.r20(), p.r21(), p.r22()),
                    Some(p)
                );
                assert!(p.nint >= NINT_RANGE.0 && p.nint <= NINT_RANGE.1);
                rf += 0.25e6;
            }
            assert_eq!(programmed, 3841);
            // LO 106.875-213.75 MHz: about 107 MHz of RF in 0.25 MHz steps.
            assert!((420..=430).contains(&by16), "{by16}");
        }
    }

    /// Words for a delivered LO: the same dividers as for the words' LO,
    /// each moved by the offset over its divider, and the LO they deliver
    /// within half an SDM step of the request.
    #[test]
    fn the_delivered_lo_takes_the_synthesizers_offset() {
        for lo in [30e6, 110e6, 304.57e6, 500e6, 1093.57e6, 1723.57e6] {
            let plain = Pll::candidates(lo).unwrap();
            let delivering = Pll::candidates_delivering(lo).unwrap();
            assert_eq!(
                plain.iter().map(|p| p.divider).collect::<Vec<_>>(),
                delivering.iter().map(|p| p.divider).collect::<Vec<_>>()
            );
            for p in delivering {
                assert!(
                    (p.lo_delivered_hz() - lo).abs() <= sdm_step_hz(p.divider) / 2.0 + 1e-6,
                    "{lo}: /{} {}",
                    p.divider,
                    p.lo_delivered_hz() - lo
                );
                assert!(
                    (p.lo_delivered_hz() - p.lo_hz() - VCO_OFFSET_HZ / p.divider as f64).abs()
                        < 1e-6
                );
            }
        }
    }

    /// The old gap's edges in LO: /32 up to 106.875 MHz, /16 from there
    /// (the smaller divider wins the overlap with /32's top) to 213.75,
    /// /8 from 213.75. Outside every reach: refused, with the span named.
    #[test]
    fn the_former_gap_programs_with_16() {
        assert_eq!(Pll::program(106.8e6).unwrap().divider, 32);
        assert_eq!(Pll::program(106.875e6).unwrap().divider, 16);
        assert_eq!(Pll::program(110.0e6).unwrap().divider, 16);
        assert_eq!(Pll::program(150.0e6).unwrap().divider, 16);
        assert_eq!(Pll::program(213.7e6).unwrap().divider, 16);
        assert_eq!(Pll::program(213.75e6).unwrap().divider, 8);
        assert_eq!(Pll::program(150.0e6).unwrap().sel_div, 0b011);
        // Below /64's reach and above /2's.
        for lo in [20.0e6, 1.9e9] {
            match Pll::program(lo) {
                Err(Error::Unsupported { what, .. }) => {
                    assert!(what.contains("26.7188 MHz to 1825.0000 MHz"), "{what}")
                }
                other => panic!("{lo}: {other:?}"),
            }
        }
        assert!(Pll::program(0.0).is_err());
        assert!(Pll::program(f64::NAN).is_err());
    }

    /// Divider choice across the LO spans of `LO_REACH`.
    #[test]
    fn divider_choice_follows_the_reach_table() {
        assert_eq!(Pll::program(40.0e6).unwrap().divider, 64);
        assert_eq!(Pll::program(80.0e6).unwrap().divider, 32);
        assert_eq!(
            Pll::program(55.0e6).unwrap().divider,
            32,
            "the smaller divider wins an overlap"
        );
        assert_eq!(Pll::program(300.0e6).unwrap().divider, 8);
        assert_eq!(
            Pll::program(440.0e6).unwrap().divider,
            4,
            "/4 at 1.76 GHz beats /8 at 3.52 GHz"
        );
        assert_eq!(Pll::program(900.0e6).unwrap().divider, 2);
        assert_eq!(Pll::program(1006.57e6).unwrap().divider, 2);
    }

    #[test]
    fn r20_packs_nint_as_the_document_says() {
        // Nint 21: m = 8 = 4·2 + 0 → SI2C 0, NI2C 2.
        let p = Pll {
            sel_div: 0,
            divider: 2,
            nint: 21,
            sdm: 0,
        };
        assert_eq!(p.r20(), 0x02);
        // Nint 268 = 13 + 4·63 + 3 → SI2C 3, NI2C 63.
        let q = Pll {
            sel_div: 0,
            divider: 2,
            nint: 268,
            sdm: 0xFFFF,
        };
        assert_eq!(q.r20(), 0xFF);
        assert_eq!((q.r21(), q.r22()), (0xFF, 0xFF));
        assert_eq!(
            Pll::from_registers(q.r16(), q.r20(), q.r21(), q.r22()),
            Some(q)
        );
        // REF_DIV2 = 0 is not a setting the tables hold for.
        assert_eq!(Pll::from_registers(0x44, 0xD1, 0x50, 0x9A), None);
        // An unusable code decodes to nothing.
        assert_eq!(
            Pll::from_registers(0b110 << 5 | 0x14, 0xD1, 0x50, 0x9A),
            None
        );
    }

    /// Findings steps 4c and 14: 0 and 63 are the range's ends, 64-127
    /// inside (step 14 read 64-69 received at VCO 1.709-1.797 GHz, among
    /// them findings 11's 220 MHz at /8, R2 69); bit 7 reads 1 always and
    /// is masked; step 4e's 93 (between step 4c's steps) is inside.
    #[test]
    fn r2_lock_reading() {
        assert_eq!(r2_locked_span(), (64, 127));
        assert!(!r2_says_locked(0));
        assert!(!r2_says_locked(63));
        assert!(!r2_says_locked(0x80 | 63));
        assert!(!r2_says_locked(1));
        for v in [
            64u8, 65, 66, 67, 68, 69, 70, 77, 83, 87, 91, 93, 95, 103, 108, 112, 116, 120, 122,
            125, 127,
        ] {
            assert!(r2_says_locked(v), "{v}");
            assert!(r2_says_locked(0x80 | v), "{v} with bit 7");
        }
    }
}
