//! Step 1: the vendor-request encodings. Read-only first: each candidate
//! wValue encoding is scored by how many documented reset values it reads
//! back. Writes only after an encoding is established, each verified by
//! read-back and restored.

use std::time::{Duration, Instant};

use crate::json::{J, hex_bytes, hex8, hex16, hex32};
use crate::regs::*;
use crate::usb::{self, DemodForm, Rtl};

/// A system/USB wValue candidate: name and the placement of (base, offset).
struct SysForm {
    name: &'static str,
    why: &'static str,
    f: fn(u16, u16) -> Option<u16>,
}

const SYS_FORMS: &[SysForm] = &[
    SysForm {
        name: "base+offset",
        why: "Table 27 '(BaseAdd)<<8 + OffsetAdd' with BaseAdd the high byte of the printed base (0x20 / 0x30)",
        f: |b, o| Some(b + o),
    },
    SysForm {
        name: "offset",
        why: "the offset alone (base implied by wIndex's block)",
        f: |_, o| Some(o),
    },
    SysForm {
        name: "offset<<8",
        why: "the demodulator-style reading 'offset in the high byte'",
        f: |_, o| if o <= 0xFF { Some(o << 8) } else { None },
    },
    SysForm {
        name: "offset<<8|base>>8",
        why: "Table 27's '<<8' applied to the offset instead of the base",
        f: |b, o| {
            if o <= 0xFF {
                Some(o << 8 | b >> 8)
            } else {
                None
            }
        },
    },
    SysForm {
        name: "swap(base+offset)",
        why: "base+offset with its bytes exchanged",
        f: |b, o| Some((b + o).swap_bytes()),
    },
];

/// (name, offset, mask, reset) for 8-bit system registers.
const SYS8: &[(&str, u16, u8, u8)] = &[
    ("DEMOD_CTL", DEMOD_CTL, DEMOD_CTL_DOC_MASK, DEMOD_CTL_RESET),
    ("GPO", GPO, 0xFF, GPO_RESET),
    ("GPOE", GPOE, 0xFF, GPOE_RESET),
    ("GPD", GPD, 0xFF, GPD_RESET),
    ("GP_CFG0", GP_CFG0, 0xFF, GP_CFG0_RESET),
    ("GP_CFG1", GP_CFG1, 0xFF, GP_CFG1_RESET),
];
/// (name, offset, mask, reset) for 32-bit system registers.
const SYS32: &[(&str, u16, u32, u32)] = &[
    ("I2CCR", I2CCR, I2CCR_FD10_MASK, I2CCR_RESET),
    ("I2CMCR", I2CMCR, I2CMCR_DOC_MASK, I2CMCR_RESET),
    ("I2CMSTR", I2CMSTR, I2CMSTR_DOC_MASK, I2CMSTR_RESET),
];
/// (name, offset, mask, reset) for 32-bit USB registers.
const USB32: &[(&str, u16, u32, u32)] = &[
    (
        "USB_SYSCTL",
        USB_SYSCTL,
        USB_SYSCTL_DOC_MASK,
        USB_SYSCTL_RESET,
    ),
    (
        "USB_EPA_CFG",
        USB_EPA_CFG,
        EPA_CFG_RESET_MASK,
        EPA_CFG_RESET,
    ),
    ("USB_EPA_CTL", USB_EPA_CTL, EPA_CTL_RESET_MASK, 0),
    (
        "USB_EPA_MAXPKT",
        USB_EPA_MAXPKT,
        EPA_MAXPKT_MASK,
        EPA_MAXPKT_RESET,
    ),
    (
        "USB_EPA_FIFO_CFG",
        USB_EPA_FIFO_CFG,
        EPA_FIFO_SIZE_MASK,
        EPA_FIFO_SIZE_RESET,
    ),
];

fn read_res(r: usb::R<Vec<u8>>) -> (J, Option<Vec<u8>>) {
    match r {
        Ok(b) => (hex_bytes(&b), Some(b)),
        Err(e) => (J::Str(format!("error: {e}")), None),
    }
}

fn le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Phase A: score every system/USB form. Returns (record, best form index, little-endian?).
fn sys_usb(rtl: &Rtl) -> (J, Option<usize>, Option<bool>) {
    let mut forms = J::Arr(Vec::new());
    let mut best: Option<(usize, usize)> = None;
    let mut best_endian = None;
    for (fi, form) in SYS_FORMS.iter().enumerate() {
        let mut regs = J::Arr(Vec::new());
        let mut score8 = 0;
        let (mut score_le, mut score_be) = (0, 0);
        let mut total = 0;
        for &(name, off, mask, reset) in SYS8 {
            total += 1;
            let Some(v) = (form.f)(SYS_BASE, off) else {
                continue;
            };
            let (raw, b) = read_res(rtl.get(v, windex(BLOCK_SYS, ACCESS_GET, 0), 1));
            let ok = b.as_ref().is_some_and(|b| b[0] & mask == reset);
            score8 += ok as usize;
            regs.push(
                J::obj()
                    .with("block", "system")
                    .with("name", name)
                    .with("wValue", hex16(v))
                    .with("len", 1)
                    .with("read", raw)
                    .with("mask", hex8(mask))
                    .with("documented_reset", hex8(reset))
                    .with("matches", ok),
            );
        }
        let wide: Vec<(&str, &str, u16, u16, u32, u32)> = SYS32
            .iter()
            .map(|&(n, o, m, r)| ("system", n, SYS_BASE, o, m, r))
            .chain(
                USB32
                    .iter()
                    .map(|&(n, o, m, r)| ("usb", n, USB_BASE, o, m, r)),
            )
            .collect();
        for (blk, name, base, off, mask, reset) in wide {
            total += 1;
            let Some(v) = (form.f)(base, off) else {
                continue;
            };
            let block = if base == SYS_BASE {
                BLOCK_SYS
            } else {
                BLOCK_USB
            };
            let (raw4, b4) = read_res(rtl.get(v, windex(block, ACCESS_GET, 0), 4));
            let (raw1, _) = read_res(rtl.get(v, windex(block, ACCESS_GET, 0), 1));
            let ok_le = b4.as_ref().is_some_and(|b| le(b) & mask == reset);
            let ok_be = b4.as_ref().is_some_and(|b| be(b) & mask == reset);
            score_le += ok_le as usize;
            score_be += ok_be as usize;
            regs.push(
                J::obj()
                    .with("block", blk)
                    .with("name", name)
                    .with("wValue", hex16(v))
                    .with("read_len4", raw4)
                    .with("read_len1", raw1)
                    .with("mask", hex32(mask))
                    .with("documented_reset", hex32(reset))
                    .with("matches_little_endian", ok_le)
                    .with("matches_big_endian", ok_be),
            );
        }
        let score = score8 + score_le.max(score_be);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((fi, score));
            best_endian = if score_le == score_be {
                None
            } else {
                Some(score_le > score_be)
            };
        }
        forms.push(
            J::obj()
                .with("form", form.name)
                .with("justification", form.why)
                .with("oracles", total)
                .with("score_8bit", score8)
                .with("score_32bit_little_endian", score_le)
                .with("score_32bit_big_endian", score_be)
                .with("score", score)
                .with("reads", regs),
        );
    }
    // Byte addressing inside a 32-bit register: read 1 byte at each of the
    // four byte addresses of USB_EPA_MAXPKT and I2CMSTR under the best form.
    let mut bytewise = J::Arr(Vec::new());
    if let Some((fi, _)) = best {
        for (block, base, off) in [
            (BLOCK_USB, USB_BASE, USB_EPA_MAXPKT),
            (BLOCK_SYS, SYS_BASE, I2CMSTR),
        ] {
            for k in 0..4u16 {
                if let Some(v) = (SYS_FORMS[fi].f)(base, off + k) {
                    let (raw, _) = read_res(rtl.get(v, windex(block, ACCESS_GET, 0), 1));
                    bytewise.push(J::obj().with("wValue", hex16(v)).with("read_len1", raw));
                }
            }
        }
    }
    let rec = J::obj()
        .with(
            "candidate_space",
            J::Arr(SYS_FORMS.iter().map(|f| J::from(f.name)).collect()),
        )
        .with("forms", forms)
        .with("best_form", best.map(|(i, _)| SYS_FORMS[i].name))
        .with("best_score", best.map(|(_, s)| s))
        .with("best_byte_order_little_endian", best_endian)
        .with("byte_reads_within_32bit_registers", bytewise);
    (rec, best.map(|(i, _)| i), best_endian)
}

/// Wait after each DEMOD_CTL enable before the search repeats.
const SETTLE: Duration = Duration::from_millis(50);

fn all_demod_forms() -> Vec<DemodForm> {
    let mut v: Vec<DemodForm> = (0..=255u8).map(DemodForm::Low).collect();
    v.extend((0..=255u8).map(DemodForm::High));
    v
}

/// Phase B: read every oracle byte under every demodulator form.
fn demod_search(rtl: &Rtl, condition: &str) -> (J, Vec<(DemodForm, usize, usize)>) {
    let oracle = demod_oracle();
    let nonzero_total = oracle.iter().filter(|o| o.4 & o.3 != 0).count();
    let start = Instant::now();
    let mut forms = J::Arr(Vec::new());
    let mut scores = Vec::new();
    for form in all_demod_forms() {
        let mut raw = String::new();
        let (mut score, mut score_nz, mut errors) = (0, 0, 0);
        let mut first_error = None;
        let mut redacted = 0;
        for (_, page, off, mask, value, _) in &oracle {
            let wv = form.wvalue(*off);
            let r = rtl.get(wv, windex(BLOCK_DEMOD, ACCESS_GET, *page as u16), 1);
            match r {
                Ok(b) => {
                    if eeprom_addressed(wv) {
                        // Answered, but the byte is EEPROM content: never recorded.
                        raw.push_str("RR ");
                        redacted += 1;
                    } else {
                        raw.push_str(&format!("{:02X} ", b[0]));
                    }
                    if b[0] & mask == *value {
                        score += 1;
                        if value & mask != 0 {
                            score_nz += 1;
                        }
                    }
                }
                Err(e) => {
                    raw.push_str("-- ");
                    errors += 1;
                    first_error.get_or_insert_with(|| e.to_string());
                }
            }
        }
        scores.push((form, score, score_nz));
        forms.push(
            J::obj()
                .with("form", form.describe())
                .with("score", score)
                .with("score_nonzero_oracles", score_nz)
                .with("errors", errors)
                .with("answered_but_redacted_eeprom_address", redacted)
                .with("first_error", first_error)
                .with("bytes_in_oracle_order", raw.trim_end().to_string()),
        );
    }
    let elapsed = start.elapsed();
    let mut ranked = scores.clone();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
    let top = ranked.first().map(|t| t.2).unwrap_or(0);
    let tied: Vec<J> = ranked
        .iter()
        .filter(|t| t.2 == top)
        .map(|t| J::from(t.0.describe()))
        .collect();
    let rec = J::obj()
        .with("condition", condition)
        .with(
            "candidate_space",
            "wValue = offset | k<<8 for k in 0..=255, and wValue = offset<<8 | k for k in 0..=255 (512 forms; k = 0 gives 'offset alone' and 'offset<<8')",
        )
        .with(
            "oracle",
            J::Arr(
                oracle
                    .iter()
                    .map(|(n, p, o, m, v, c)| {
                        J::obj()
                            .with("name", n.clone())
                            .with("page", *p)
                            .with("offset", hex8(*o))
                            .with("mask", hex8(*m))
                            .with("documented_reset", hex8(*v))
                            .with("cite", *c)
                    })
                    .collect(),
            ),
        )
        .with(
            "byte_legend",
            "two hex digits = the byte read; -- = the request stalled or failed; RR = answered, but the wValue low byte addressed the configuration EEPROM (0xA0/0xA1), whose contents (USB strings incl. serial) are never recorded",
        )
        .with("oracle_bytes", oracle.len())
        .with("oracle_bytes_with_nonzero_reset", nonzero_total)
        .with("elapsed_s", elapsed.as_secs_f64())
        .with("best_nonzero_score", top)
        .with("forms_at_best", J::Arr(tied))
        .with(
            "top10",
            J::Arr(
                ranked
                    .iter()
                    .take(10)
                    .map(|t| J::obj().with("form", t.0.describe()).with("score", t.1).with("score_nonzero", t.2))
                    .collect(),
            ),
        )
        .with("forms", forms);
    (rec, ranked)
}

/// Phases A and B only: no power cycle, no writes. For a bench whose
/// recovery path is not available; the state since attach is unknown.
pub fn run_readonly() -> Result<(), String> {
    usb::guard()?;
    let rtl = Rtl::open()?;
    let condition = "read-only, no power cycle (recovery unavailable): the chip's state since it was attached is unknown";
    let (mut a, sys_best, endian) = sys_usb(&rtl);
    let mut hdr = crate::header(
        "1A-readonly",
        "system/USB wValue encoding and register byte order, scored against documented reset values",
    );
    hdr.set("condition", condition);
    hdr.set("descriptor", rtl.descriptor_record());
    hdr.set(
        "settings_held_fixed",
        "read-only; bmRequestType 0xC0, bRequest 0, wIndex per Table 28",
    );
    if let J::Obj(f) = &mut a {
        for (k, v) in f.drain(..) {
            hdr.set(&k, v);
        }
    }
    crate::write_record("step1-readonly-sys-usb-encoding.json", &hdr);
    if let Some(i) = sys_best {
        println!(
            "system/USB: best form {} byte order little-endian={:?}",
            SYS_FORMS[i].name, endian
        );
    }
    let (b_now, ranked) = demod_search(&rtl, condition);
    let nz_total = demod_oracle().iter().filter(|o| o.4 & o.3 != 0).count();
    let mut b = crate::header(
        "1B-readonly",
        "demodulator wValue encoding, scored against documented reset values",
    );
    b.set(
        "settings_held_fixed",
        "read-only reads of 1 byte; bmRequestType 0xC0, bRequest 0, wIndex = page (block 0, get)",
    );
    b.set("result", b_now);
    crate::write_record("step1-readonly-demod-encoding.json", &b);
    println!(
        "demod: best {} nonzero score {}/{}",
        ranked[0].0.describe(),
        ranked[0].2,
        nz_total
    );
    Ok(())
}

pub fn run() -> Result<(), String> {
    usb::guard()?;
    // Start from a fresh power-on state so the reset values are the chip's own.
    let cycle = usb::power_cycle()?;
    let mut rtl = Rtl::open()?;

    // Phase A.
    let (mut a, sys_best, endian) = sys_usb(&rtl);
    let mut hdr = crate::header(
        "1A",
        "system/USB wValue encoding and register byte order, scored against documented reset values",
    );
    hdr.set("power_cycle_before", cycle.clone());
    hdr.set("descriptor", rtl.descriptor_record());
    hdr.set(
        "settings_held_fixed",
        "fresh power-on; read-only; bmRequestType 0xC0, bRequest 0, wIndex per Table 28",
    );
    if let J::Obj(f) = &mut a {
        for (k, v) in f.drain(..) {
            hdr.set(&k, v);
        }
    }
    crate::write_record("step1-sys-usb-encoding.json", &hdr);
    let sys_form = sys_best.ok_or("no system/USB form scored")?;
    println!(
        "system/USB: best form {} byte order little-endian={:?}",
        SYS_FORMS[sys_form].name, endian
    );

    // Phase B, as booted.
    let (b_booted, ranked) = demod_search(&rtl, "as booted (no writes since power-on)");
    let nz_total = demod_oracle().iter().filter(|o| o.4 & o.3 != 0).count();
    let mut b = crate::header(
        "1B",
        "demodulator wValue encoding, scored against documented reset values",
    );
    b.set(
        "settings_held_fixed",
        "read-only reads of 1 byte; bmRequestType 0xC0, bRequest 0, wIndex = page (block 0, get)",
    );
    b.set("as_booted", b_booted);
    let top = ranked[0];
    println!(
        "demod as booted: best {} nonzero score {}/{}",
        top.0.describe(),
        top.2,
        nz_total
    );
    // A form fits when it reads back at least 90 % of the non-zero
    // documented reset bytes.
    let fits = |nz: usize| nz * 10 >= nz_total * 9;
    b.set(
        "fit_criterion",
        format!("score_nonzero_oracles >= 90% of {nz_total}"),
    );
    let mut chosen = fits(top.2).then_some(top.0);
    if chosen.is_none() {
        // Phase C: the DEMOD_CTL enables of Table 15, cumulatively, with
        // the system encoding just established: first the demodulator PLL
        // (bit 7), then, only if no form fits yet, ADC_I and ADC_Q (bits 6
        // and 3). Each write is verified by read-back, given a settle time
        // well above the tuner's 5 ms PLL lock time ([R820T] Table 1-2; the
        // RTL2832U prints none), and the full search is repeated.
        let v = (SYS_FORMS[sys_form].f)(SYS_BASE, DEMOD_CTL).unwrap();
        let mut stages = J::Arr(Vec::new());
        for (name, bits) in [
            ("DEMOD_CTL |= PLL enable (bit 7)", DEMOD_CTL_PLL_EN),
            (
                "DEMOD_CTL |= ADC_I enable (bit 6) | ADC_Q enable (bit 3)",
                DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN,
            ),
        ] {
            let old = rtl
                .get(v, windex(BLOCK_SYS, ACCESS_GET, 0), 1)
                .map_err(|e| e.to_string())?[0];
            let new = old | bits;
            rtl.set(v, windex(BLOCK_SYS, ACCESS_SET, 0), &[new])
                .map_err(|e| e.to_string())?;
            std::thread::sleep(SETTLE);
            let back = rtl
                .get(v, windex(BLOCK_SYS, ACCESS_GET, 0), 1)
                .map_err(|e| e.to_string())?[0];
            println!("{name}: DEMOD_CTL 0x{old:02X} -> wrote 0x{new:02X}, reads 0x{back:02X}");
            let (res, ranked2) = demod_search(&rtl, &format!("after {name}"));
            let best = ranked2[0];
            println!(
                "demod after {name}: best {} nonzero score {}/{}",
                best.0.describe(),
                best.2,
                nz_total
            );
            stages.push(
                J::obj()
                    .with("stage", name)
                    .with(
                        "write",
                        J::obj()
                            .with("before", hex8(old))
                            .with("written", hex8(new))
                            .with("readback", hex8(back))
                            .with("verified", back == new)
                            .with("settle_ms", SETTLE.as_millis() as u64),
                    )
                    .with("search", res),
            );
            if fits(best.2) {
                chosen = Some(best.0);
                break;
            }
        }
        b.set("enable_stages", stages);
    }
    b.set("chosen_form", chosen.map(|c| c.describe()));
    crate::write_record("step1-demod-encoding.json", &b);
    let Some(chosen) = chosen else {
        return Err("no demodulator form fits the documented reset values after the Table 15 enables; Phase D not run".into());
    };

    // Phase D: writes, each verified by read-back and restored.
    rtl.demod_form = Some(chosen);
    let mut d = crate::header(
        "1D",
        "write paths: demodulator, system and USB writes verified by read-back, then restored",
    );
    d.set("demod_form", chosen.describe());
    d.set("sys_usb_form", SYS_FORMS[sys_form].name);
    let mut writes = J::Arr(Vec::new());

    // Demodulator: rf_agc_max (page 1, 0x0B, RW, Table 2).
    let w = demod_write_check(&rtl, 1, 0x0B, 0x01);
    writes.push(w.with("register", "rf_agc_max page 1 0x0B (Table 2, RW)"));
    // Demodulator: if_agc_max (page 1, 0x09, RW, Table 2).
    let w = demod_write_check(&rtl, 1, 0x09, 0x01);
    writes.push(w.with("register", "if_agc_max page 1 0x09 (Table 2, RW)"));
    // Demodulator page 0: PID31 low byte (page 0, 0xA5, RW, Table 13).
    let w = demod_write_check(&rtl, 0, 0xA5, 0x01);
    writes.push(w.with("register", "PID31[7:0] page 0 0xA5 (Table 13, RW)"));

    // Multi-byte demodulator read: auto-increment?
    let singles: Vec<J> = (CFREQ_MSB..=0xA2)
        .map(|o| read_res(rtl.demod_read(1, o, 1)).0)
        .collect();
    let (multi, _) = read_res(rtl.demod_read(1, CFREQ_MSB, 6));
    d.set(
        "demod_multibyte_read",
        J::obj()
            .with("page", 1)
            .with("offset", hex8(CFREQ_MSB))
            .with("singles", J::Arr(singles))
            .with("len6", multi),
    );

    // USB: USB_EPA_MAXPKT (RW, Table 33), 32-bit, in the established order.
    let little = endian.unwrap_or(true);
    let to = |x: u32| {
        if little {
            x.to_le_bytes()
        } else {
            x.to_be_bytes()
        }
    };
    let from = |b: &[u8]| if little { le(b) } else { be(b) };
    let mv = (SYS_FORMS[sys_form].f)(USB_BASE, USB_EPA_MAXPKT).unwrap();
    let rd = |r: &Rtl| {
        r.get(mv, windex(BLOCK_USB, ACCESS_GET, 0), 4)
            .map(|b| from(&b))
    };
    let orig = rd(&rtl).map_err(|e| e.to_string())?;
    let test = (orig ^ 0x200) & EPA_MAXPKT_MASK | (orig & !EPA_MAXPKT_MASK);
    let wr = rtl.set(mv, windex(BLOCK_USB, ACCESS_SET, 0), &to(test));
    let back = rd(&rtl);
    let rs = rtl.set(mv, windex(BLOCK_USB, ACCESS_SET, 0), &to(orig));
    let back2 = rd(&rtl);
    writes.push(
        J::obj()
            .with("register", "USB_EPA_MAXPKT 0x2158 (Table 33, RW)")
            .with("original", hex32(orig))
            .with("written", hex32(test))
            .with("write_result", wr.err().map(|e| e.to_string()))
            .with("readback", back.ok().map(hex32))
            .with("restore_result", rs.err().map(|e| e.to_string()))
            .with("readback_after_restore", back2.ok().map(hex32)),
    );

    // System: DEMOD_CTL PLL enable (Table 15, RW), left enabled (step 3 needs it).
    let sv = (SYS_FORMS[sys_form].f)(SYS_BASE, DEMOD_CTL).unwrap();
    let old = rtl
        .get(sv, windex(BLOCK_SYS, ACCESS_GET, 0), 1)
        .map_err(|e| e.to_string())?[0];
    let flip = old ^ DEMOD_CTL_PLL_EN;
    let wr = rtl.set(sv, windex(BLOCK_SYS, ACCESS_SET, 0), &[flip]);
    let back = rtl
        .get(sv, windex(BLOCK_SYS, ACCESS_GET, 0), 1)
        .ok()
        .map(|b| b[0]);
    let rs = rtl.set(sv, windex(BLOCK_SYS, ACCESS_SET, 0), &[old]);
    let back2 = rtl
        .get(sv, windex(BLOCK_SYS, ACCESS_GET, 0), 1)
        .ok()
        .map(|b| b[0]);
    writes.push(
        J::obj()
            .with(
                "register",
                "DEMOD_CTL 0x3000 bit 7 PLL enable (Table 15, RW)",
            )
            .with("original", hex8(old))
            .with("written", hex8(flip))
            .with("write_result", wr.err().map(|e| e.to_string()))
            .with("readback", back.map(hex8))
            .with("restore_result", rs.err().map(|e| e.to_string()))
            .with("readback_after_restore", back2.map(hex8)),
    );
    d.set("writes", writes);
    crate::write_record("step1-writes.json", &d);
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

/// Flip `bits` of a demodulator byte, read back, restore, read back.
fn demod_write_check(rtl: &Rtl, page: u8, off: u8, bits: u8) -> J {
    let orig = match rtl.demod_read8(page, off) {
        Ok(v) => v,
        Err(e) => return J::obj().with("error", format!("read: {e}")),
    };
    let test = orig ^ bits;
    let wr = rtl.demod_write8(page, off, test);
    let back = rtl.demod_read8(page, off);
    let rs = rtl.demod_write8(page, off, orig);
    let back2 = rtl.demod_read8(page, off);
    let verified = back.as_ref().ok() == Some(&test) && back2.as_ref().ok() == Some(&orig);
    J::obj()
        .with("page", page)
        .with("offset", hex8(off))
        .with("original", hex8(orig))
        .with("written", hex8(test))
        .with("write_result", wr.err().map(|e| e.to_string()))
        .with("readback", back.ok().map(hex8))
        .with("restore_result", rs.err().map(|e| e.to_string()))
        .with("readback_after_restore", back2.ok().map(hex8))
        .with("write_verified", verified)
}
