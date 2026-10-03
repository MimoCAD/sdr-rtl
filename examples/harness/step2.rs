//! Step 2: tuner access through the I2C repeater. The oracle is the
//! R820T's register 0, which reads 0x96 after bit reversal (0x69 as it
//! arrives, LSB first; [R820T] p. 17).
//!
//! Candidate preparation steps, all documented: the demodulator PLL, the
//! reset release and the two ADCs (DEMOD_CTL, Table 15), the I2C clock
//! divisor FD10 written at its reset value ("When powered on, software must
//! write FD10", Table 22), tuner power on GPIO3 (Tables 16-19, p. 29) and
//! IIC_repeat (Table 3, or the "7th bit" of Note2 under Table 27). They are
//! applied cumulatively from a fresh power-on, then removed one at a time.

use crate::json::{J, hex8, hex16, hex_bytes};
use crate::regs::*;
use crate::usb::{self, Rtl};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prep {
    DemodPll,
    DemodResetRelease,
    DemodAdcs,
    I2cFd10,
    TunerPower,
    /// IIC_repeat at page 1, 0x01 bit 3 (Table 3).
    RepeatBit3,
    /// IIC_repeat read as "the 7th bit of the first byte" (Note2, Table 27).
    RepeatBit7,
}

const ORDER: [Prep; 6] =
    [Prep::DemodPll, Prep::DemodResetRelease, Prep::DemodAdcs, Prep::I2cFd10, Prep::TunerPower, Prep::RepeatBit3];

fn apply(rtl: &Rtl, p: Prep) -> J {
    let rec = J::obj().with("prep", format!("{p:?}"));
    let res: Result<J, String> = (|| {
        let e = |x: usb::Fail| x.to_string();
        match p {
            Prep::DemodPll | Prep::DemodResetRelease | Prep::DemodAdcs => {
                let bits = match p {
                    Prep::DemodPll => DEMOD_CTL_PLL_EN,
                    Prep::DemodResetRelease => DEMOD_CTL_RESET_RELEASE,
                    _ => DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN,
                };
                let old = rtl.sys_read8(DEMOD_CTL).map_err(e)?;
                rtl.sys_write8(DEMOD_CTL, old | bits).map_err(e)?;
                let back = rtl.sys_read8(DEMOD_CTL).map_err(e)?;
                Ok(J::obj().with("register", "DEMOD_CTL").with("before", hex8(old)).with("after", hex8(back)))
            }
            Prep::I2cFd10 => {
                let old = rtl.sys_read(I2CCR, 1).map_err(e)?[0];
                let new = (old & !(I2CCR_FD10_MASK as u8)) | I2CCR_RESET as u8;
                rtl.sys_write8(I2CCR, new).map_err(e)?;
                let back = rtl.sys_read8(I2CCR).map_err(e)?;
                Ok(J::obj().with("register", "I2CCR").with("before", hex8(old)).with("after", hex8(back)))
            }
            Prep::TunerPower => {
                // GPD bit 3 = 0 (output, Table 19), GPOE bit 3 = 1 (Table 18),
                // GPO bit 3 = 0 (power on: "1 to turn off power", p. 29).
                let gpd = rtl.sys_read8(GPD).map_err(e)?;
                rtl.sys_write8(GPD, gpd & !GPIO_TUNER_POWER).map_err(e)?;
                let gpoe = rtl.sys_read8(GPOE).map_err(e)?;
                rtl.sys_write8(GPOE, gpoe | GPIO_TUNER_POWER).map_err(e)?;
                let gpo = rtl.sys_read8(GPO).map_err(e)?;
                rtl.sys_write8(GPO, gpo & !GPIO_TUNER_POWER).map_err(e)?;
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(J::obj()
                    .with("GPD", J::obj().with("before", hex8(gpd)).with("after", rtl.sys_read8(GPD).ok().map(hex8)))
                    .with("GPOE", J::obj().with("before", hex8(gpoe)).with("after", rtl.sys_read8(GPOE).ok().map(hex8)))
                    .with("GPO", J::obj().with("before", hex8(gpo)).with("after", rtl.sys_read8(GPO).ok().map(hex8))))
            }
            Prep::RepeatBit3 | Prep::RepeatBit7 => {
                let bit = if p == Prep::RepeatBit3 { IIC_REPEAT_BIT_TABLE3 } else { IIC_REPEAT_BIT_NOTE2 };
                let (old, back) = rtl.demod_rmw(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, bit, bit).map_err(e)?;
                Ok(J::obj().with("register", "page 1 0x01").with("before", hex8(old)).with("after", hex8(back)))
            }
        }
    })();
    match res {
        Ok(r) => rec.with("result", r),
        Err(err) => rec.with("error", err),
    }
}

/// The tuner-read candidates: Table 27's (offset<<8)+IICAdd with the
/// write and the read address, the reversed placement, and wValue 0
/// (Table 28's note: "ignoring the 'wValue' field").
const READ_WVALUES: [u16; 5] = [
    TUNER_I2C_WRITE as u16,
    TUNER_I2C_READ as u16,
    (TUNER_I2C_WRITE as u16) << 8,
    (TUNER_I2C_READ as u16) << 8,
    0x0000,
];

/// Try every tuner-read candidate; Some(wValue) of the first that returns
/// the check byte.
fn probe(rtl: &Rtl) -> (J, Option<u16>) {
    let mut out = J::Arr(Vec::new());
    let mut hit = None;
    for &v in &READ_WVALUES {
        for len in [1u16, 5] {
            let r = rtl.tuner_get(v, len);
            let (raw, first) = match &r {
                Ok(b) => (hex_bytes(b), Some(b[0])),
                Err(e) => (J::Str(format!("error: {e}")), None),
            };
            let ok = first.map(|b| b.reverse_bits() == TUNER_R0_CHECK);
            if ok == Some(true) && hit.is_none() {
                hit = Some(v);
            }
            out.push(
                J::obj()
                    .with("wValue", hex16(v))
                    .with("len", len)
                    .with("read_raw", raw)
                    .with("first_byte_bit_reversed", first.map(|b| hex8(b.reverse_bits())))
                    .with("r0_check_0x96", ok),
            );
        }
    }
    (out, hit)
}

fn fresh() -> Result<(Rtl, J), String> {
    let cyc = usb::power_cycle()?;
    Ok((Rtl::open()?, cyc))
}

pub fn run() -> Result<(), String> {
    usb::guard()?;
    let mut rec = crate::header("2", "tuner access: register 0 read through the RTL2832U's I2C repeater");
    rec.set(
        "candidate_space",
        J::obj()
            .with("preparation_steps", J::Arr(ORDER.iter().map(|p| J::from(format!("{p:?}"))).collect()))
            .with("repeater_bit_variants", "page 1 0x01 bit 3 (Table 3) | bit 7 (Note2 under Table 27)")
            .with("read_wvalues", J::Arr(READ_WVALUES.iter().map(|v| hex16(*v)).collect()))
            .with("read_lengths", "1, 5"),
    );
    rec.set("oracle", "first byte read, bit-reversed, equals 0x96 ([R820T] p. 17)");

    // Phase 1: cumulative from power-on.
    let (rtl, cyc) = fresh()?;
    let mut cumulative = J::Arr(Vec::new());
    let (p0, h0) = probe(&rtl);
    cumulative.push(J::obj().with("applied", J::Arr(vec![])).with("probe", p0).with("hit", h0.map(hex16)));
    let mut applied = Vec::new();
    for p in ORDER {
        let a = apply(&rtl, p);
        applied.push(J::from(format!("{p:?}")));
        let (pr, h) = probe(&rtl);
        cumulative.push(
            J::obj().with("applied", J::Arr(applied.clone())).with("this_step", a).with("probe", pr).with("hit", h.map(hex16)),
        );
    }
    rec.set("power_cycle", cyc);
    rec.set("cumulative", cumulative);
    let (_, full_hit) = probe(&rtl);

    // Phase 4 (on the fully prepared chip): the write path. R19 is all
    // fixed-0 cells ([R820T2] Table 1-2), so 0x00 is the documented value.
    let mut writes = J::Arr(Vec::new());
    let tv = ((TUNER_R19 as u16) << 8) | TUNER_I2C_WRITE as u16;
    let r = rtl.tuner_set(tv, &[TUNER_R19_FIXED]);
    writes.push(J::obj().with("form", "Table 27: wValue (19<<8)|0x34, data [0x00]").with("wValue", hex16(tv)).with("result", r.err().map(|e| e.to_string())));
    let r = rtl.tuner_set(0, &[TUNER_R19, TUNER_R19_FIXED]);
    writes.push(J::obj().with("form", "Table 28 note: wValue 0, data [19, 0x00]").with("wValue", hex16(0)).with("result", r.err().map(|e| e.to_string())));
    // The same writes with the repeater closed: a NAK would show as an error.
    let closed = rtl.demod_rmw(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT_TABLE3, 0).map(|(a, b)| (hex8(a), hex8(b)));
    let r = rtl.tuner_set(tv, &[TUNER_R19_FIXED]);
    writes.push(J::obj().with("form", "Table 27 form, repeater bit 3 cleared").with("repeater_clear", format!("{closed:?}")).with("result", r.err().map(|e| e.to_string())));
    let (pc, hc) = probe(&rtl);
    writes.push(J::obj().with("read_probe_with_repeater_cleared", pc).with("hit", hc.map(hex16)));
    rec.set("write_path", writes);
    drop(rtl);

    // Phase 2: ablation, each step removed from the full set.
    let mut ablation = J::Arr(Vec::new());
    for skip in ORDER {
        let (rtl, _) = fresh()?;
        let mut steps = J::Arr(Vec::new());
        for p in ORDER.into_iter().filter(|p| *p != skip) {
            steps.push(apply(&rtl, p));
        }
        let (pr, h) = probe(&rtl);
        ablation.push(
            J::obj()
                .with("removed", format!("{skip:?}"))
                .with("steps", steps)
                .with("probe", pr)
                .with("hit", h.map(hex16))
                .with("necessary", full_hit.is_some() && h.is_none()),
        );
    }
    rec.set("full_set_hit", full_hit.map(hex16));
    rec.set("ablation", ablation);

    // Phase 3: the repeater bit variants.
    let mut variants = J::Arr(Vec::new());
    for (name, reps) in [("bit 7 only", vec![Prep::RepeatBit7]), ("bit 3 and bit 7", vec![Prep::RepeatBit3, Prep::RepeatBit7])] {
        let (rtl, _) = fresh()?;
        let mut steps = J::Arr(Vec::new());
        for p in ORDER.into_iter().filter(|p| *p != Prep::RepeatBit3).chain(reps) {
            steps.push(apply(&rtl, p));
        }
        let (pr, h) = probe(&rtl);
        variants.push(J::obj().with("repeater", name).with("steps", steps).with("probe", pr).with("hit", h.map(hex16)));
    }
    rec.set("repeater_variants", variants);
    crate::write_record("step2-tuner-access.json", &rec);
    Ok(())
}
