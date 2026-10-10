//! Gain: the three manual stages of the tuner (LNA_GAIN R5[3:0],
//! MIX_GAIN R7[3:0], VGA_CODE R12[3:0], sixteen codes each) with the dB
//! per code the bench measured (`tables::gain`, findings step 4j: the
//! comparator method, 1 dB resolution, at 300 MHz with the other two
//! stages at code 8), and one rule that spreads a single dB number over
//! the three.
//!
//! Every dB here is RELATIVE to code 0 of its stage: the bench measured
//! differences between codes, not absolute gain, so the "total" is the
//! dB above the all-zero setting (0 to about 109 dB, which is the
//! document's 104 dB AGC range to within the tables' resolution,
//! [R820T] Table 1-2).
//!
//! The rule for one number (`codes_for_db`): a ladder filled FRONT
//! FIRST — the LNA from code 0 to its highest-gain code, then the mixer
//! to its first code of maximum gain, then the VGA to code 15 — and the
//! request snaps to the ladder's nearest rung. Why front first: [R820T]
//! Table 1-2 specifies the 3.5 dB noise figure at maximum gain and the
//! IIP3 as −7.5 dBm at LNA max gain against +35 dBm at LNA min, so the
//! LNA's gain is what sets the noise figure and what costs linearity; a
//! request for more gain takes it at the front, and a request for less
//! gives it back at the VGA first, the stage after the IF filter
//! (Figure D), where a step changes neither the noise figure nor the
//! front end's linearity. The ladder is non-decreasing by construction,
//! so the mapping is monotonic and a rung's dB round-trips exactly.

use alloc::vec::Vec;

use sdr_core::gain::{Stage, StageReading, nearest_level};

use crate::tables::gain::{LNA_GAIN_DB, MIX_GAIN_DB, VGA_GAIN_DB};
use crate::tables::types::GainCode;

/// Codes per stage: a 4-bit field.
pub const CODES: usize = 16;

/// One code's dB relative to code 0: the comparator's value, or where
/// the flip lay below the source's floor (VGA code 15) the lower bound
/// the record states (≥ 55 dB), used as the level's value and marked so
/// in the module's documentation.
fn db_of(g: &GainCode) -> f64 {
    g.comparator_db.or(g.comparator_at_least_db).map(f64::from).unwrap_or(0.0)
}

fn table(stage: Stage) -> Option<&'static [GainCode; 16]> {
    match stage {
        Stage::Lna => Some(&LNA_GAIN_DB),
        Stage::Mixer => Some(&MIX_GAIN_DB),
        Stage::Vga => Some(&VGA_GAIN_DB),
        Stage::Amp | Stage::Pga => None,
    }
}

/// The dB of every code of a stage (index = code), or None for a stage
/// this tuner does not have.
pub fn levels_db(stage: Stage) -> Option<[f64; CODES]> {
    let t = table(stage)?;
    let mut v = [0.0; CODES];
    for (i, g) in t.iter().enumerate() {
        debug_assert_eq!(g.code as usize, i);
        v[i] = db_of(g);
    }
    Some(v)
}

/// The dB of one code of a stage (0 for a stage this tuner lacks).
pub fn code_db(stage: Stage, code: u8) -> f64 {
    levels_db(stage).map_or(0.0, |l| l[(code as usize).min(CODES - 1)])
}

/// The three stages' codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Codes {
    pub lna: u8,
    pub mix: u8,
    pub vga: u8,
}

impl Codes {
    /// The sum of the three stages' relative dB.
    pub fn total_db(&self) -> f64 {
        code_db(Stage::Lna, self.lna) + code_db(Stage::Mixer, self.mix) + code_db(Stage::Vga, self.vga)
    }

    pub fn readings(&self) -> Vec<StageReading> {
        [(Stage::Lna, self.lna), (Stage::Mixer, self.mix), (Stage::Vga, self.vga)]
            .into_iter()
            .map(|(stage, code)| StageReading { stage, level: code as usize, levels: CODES, db: code_db(stage, code) })
            .collect()
    }

    pub fn with_level(&self, stage: Stage, code: u8) -> Codes {
        match stage {
            Stage::Lna => Codes { lna: code, ..*self },
            Stage::Mixer => Codes { mix: code, ..*self },
            Stage::Vga => Codes { vga: code, ..*self },
            Stage::Amp | Stage::Pga => *self,
        }
    }
}

/// The first code at which a stage reaches its maximum dB (the mixer
/// peaks at code 13 and falls back 1 dB at code 15, findings step 4j).
pub fn top_code(stage: Stage) -> u8 {
    let l = levels_db(stage).unwrap_or([0.0; CODES]);
    let max = l.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    l.iter().position(|&d| d == max).unwrap_or(0) as u8
}

/// The ladder: every (total dB, codes) rung, front first.
pub fn ladder() -> Vec<(f64, Codes)> {
    let lna_top = top_code(Stage::Lna);
    let mix_top = top_code(Stage::Mixer);
    let vga_top = top_code(Stage::Vga);
    let mut rungs = Vec::with_capacity(CODES * 3);
    for lna in 0..=lna_top {
        rungs.push(Codes { lna, mix: 0, vga: 0 });
    }
    for mix in 1..=mix_top {
        rungs.push(Codes { lna: lna_top, mix, vga: 0 });
    }
    for vga in 1..=vga_top {
        rungs.push(Codes { lna: lna_top, mix: mix_top, vga });
    }
    rungs.into_iter().map(|c| (c.total_db(), c)).collect()
}

/// The rung nearest `db` (the first of equals).
pub fn codes_for_db(db: f64) -> Codes {
    let l = ladder();
    let dbs: Vec<f64> = l.iter().map(|r| r.0).collect();
    l[nearest_level(&dbs, db)].1
}

/// The highest total the ladder reaches.
pub fn max_db() -> f64 {
    ladder().last().map_or(0.0, |r| r.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables of findings step 4j, as the stage levels.
    #[test]
    fn stage_levels_are_the_comparator_tables() {
        let lna = levels_db(Stage::Lna).unwrap();
        assert_eq!(lna, [0.0, 3.0, 7.0, 10.0, 13.0, 15.0, 18.0, 21.0, 23.0, 27.0, 28.0, 30.0, 31.0, 34.0, 35.0, 36.0]);
        let mix = levels_db(Stage::Mixer).unwrap();
        assert_eq!(mix, [0.0, 2.0, 4.0, 5.0, 7.0, 8.0, 10.0, 11.0, 12.0, 14.0, 15.0, 17.0, 17.0, 18.0, 18.0, 17.0]);
        let vga = levels_db(Stage::Vga).unwrap();
        assert_eq!(vga, [0.0, 3.0, 6.0, 9.0, 12.0, 16.0, 20.0, 23.0, 26.0, 30.0, 33.0, 37.0, 40.0, 44.0, 49.0, 55.0]);
        assert!(levels_db(Stage::Amp).is_none());
        assert!(levels_db(Stage::Pga).is_none());
        assert_eq!(top_code(Stage::Lna), 15);
        assert_eq!(top_code(Stage::Mixer), 13);
        assert_eq!(top_code(Stage::Vga), 15);
    }

    #[test]
    fn the_ladder_is_non_decreasing_and_front_first() {
        let l = ladder();
        assert_eq!(l.len(), 16 + 13 + 15);
        for w in l.windows(2) {
            assert!(w[1].0 >= w[0].0, "{:?} then {:?}", w[0], w[1]);
        }
        assert_eq!(l[0], (0.0, Codes { lna: 0, mix: 0, vga: 0 }));
        assert_eq!(l[15], (36.0, Codes { lna: 15, mix: 0, vga: 0 }));
        assert_eq!(l[16], (38.0, Codes { lna: 15, mix: 1, vga: 0 }));
        assert_eq!(l[28], (54.0, Codes { lna: 15, mix: 13, vga: 0 }));
        assert_eq!(l[29], (57.0, Codes { lna: 15, mix: 13, vga: 1 }));
        assert_eq!(*l.last().unwrap(), (109.0, Codes { lna: 15, mix: 13, vga: 15 }));
        assert_eq!(max_db(), 109.0);
    }

    /// Monotonic in the request, and every rung's dB comes back as itself.
    #[test]
    fn the_mapping_is_monotonic_and_round_trips() {
        let mut last = -1.0;
        let mut db = -5.0;
        while db <= 115.0 {
            let t = codes_for_db(db).total_db();
            assert!(t >= last, "{db}: {t} after {last}");
            last = t;
            db += 0.25;
        }
        for (db, codes) in ladder() {
            let got = codes_for_db(db);
            assert_eq!(got.total_db(), db);
            assert_eq!(got, codes_for_db(codes.total_db()));
        }
        assert_eq!(codes_for_db(-10.0), Codes { lna: 0, mix: 0, vga: 0 });
        assert_eq!(codes_for_db(1000.0), Codes { lna: 15, mix: 13, vga: 15 });
        // 40 dB: the LNA full (36) and 4 dB from the mixer (code 2).
        assert_eq!(codes_for_db(40.0), Codes { lna: 15, mix: 2, vga: 0 });
        // Between rungs the nearer wins: 1 dB is nearer 0 than 3.
        assert_eq!(codes_for_db(1.0), Codes { lna: 0, mix: 0, vga: 0 });
        assert_eq!(codes_for_db(2.0), Codes { lna: 1, mix: 0, vga: 0 });
    }

    #[test]
    fn readings_carry_level_count_and_db() {
        let r = Codes { lna: 8, mix: 8, vga: 8 }.readings();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], StageReading { stage: Stage::Lna, level: 8, levels: 16, db: 23.0 });
        assert_eq!(r[1], StageReading { stage: Stage::Mixer, level: 8, levels: 16, db: 12.0 });
        assert_eq!(r[2], StageReading { stage: Stage::Vga, level: 8, levels: 16, db: 26.0 });
        assert_eq!(Codes { lna: 8, mix: 8, vga: 8 }.total_db(), 61.0);
        assert_eq!(Codes { lna: 0, mix: 0, vga: 0 }.with_level(Stage::Vga, 15), Codes { lna: 0, mix: 0, vga: 15 });
    }
}
