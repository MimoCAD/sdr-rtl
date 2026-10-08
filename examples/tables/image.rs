//! The tuner register image R5-R31 as the harness wrote it for the
//! `tune 300e6` record (data/step4-tune-300.000MHz.json), with the
//! reason beside every register (the harness's `tuner::IMAGE_NOTES`,
//! copied into every record). These are documented-choice values from
//! [R820T2] Tables 1-2 and 1-3, not measurements.

use crate::emit::{self, Src};
use crate::records::TuneRecord;

pub fn emit(rec: &TuneRecord) -> Result<String, String> {
    if rec.fixed_cell_violations != 0 {
        return Err(format!("{}: the image violates {} fixed cells", rec.provenance.file, rec.fixed_cell_violations));
    }
    if rec.write_errors != 0 {
        return Err(format!("{}: {} write errors", rec.provenance.file, rec.write_errors));
    }
    if !rec.readback_differs_at.is_empty() {
        return Err(format!("{}: R5-R15 read back differently at {:?}", rec.provenance.file, rec.readback_differs_at));
    }
    let mut s = Src::new(&[&rec.provenance]);
    let front: Vec<String> = rec.front.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    let pll: Vec<String> = rec.pll.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    s.module_doc(&format!(
        "The tuner's bring-up register image, R5-R31, as written for the
`tune {:.0} MHz` record in the {} configuration, with the reason for
every register.

These are NOT measurements. Source, as the record states it: \"{}\".
Every fixed cell of [R820T2] Table 1-2 is as printed (the record counts
0 violations); every other field is a stated choice from Table 1-3's
meaning, or a value of the record's front end / PLL words below. The
one choice the bench forced is R15's CLK_OUT_ENB = 0 (findings.md step
4a: the demodulator is clocked from the tuner's clock output on this
board). R5-R15 read back exactly as written; R16-R31 cannot be read
back (the read wraps after 16 registers).

The front end the image was built for (the fields marked \"per Front\"
in the reasons): {}.

The PLL words (the fields marked \"per Pll\"): {}.",
        rec.freq_hz / 1e6,
        rec.config,
        rec.image_source,
        front.join(", "),
        pll.join(", ")
    ));
    s.blank();
    s.line("use super::types::*;");
    s.blank();
    s.doc("The first register of the image (R0-R4 are read-only).");
    s.line(&format!("pub const IMAGE_FIRST_REG: u8 = {};", rec.registers[0].reg));
    s.doc("R5..=R31: value and reason.");
    s.line(&format!("pub const TUNER_IMAGE: [RegisterChoice; {}] = [", rec.registers.len()));
    for r in &rec.registers {
        s.line(&format!("    RegisterChoice {{ reg: {}, value: 0x{:02X}, why: {} }},", r.reg, r.value, emit::s(&r.why)));
    }
    s.line("];");
    Ok(s.finish())
}
