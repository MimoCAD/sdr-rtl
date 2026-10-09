//! Step 6, read-only: the two spaces the firmware's code opens that no
//! scan covered (findings step 5): demodulator pages 5-15 (block 0 masks
//! the page to 4 bits and writes it to register 0x00) and the 8051's
//! external-data space outside 0x2000-0x21FF / 0x3000-0x30FF (blocks 1
//! and 2 are a raw window; page nibble 1 copies bytes straight).
//!
//! No writes anywhere. The state is the low-if-open held state (the scan's
//! starting point), with the bulk queue drained between reads so the
//! replay keeps running. Every space is read twice, the passes at least
//! [`LIVE_GAP`] apart: a byte that differs is live.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::baselines::Baseline;
use crate::json::{J, hex8, hex16, hex_bytes};
use crate::regs::*;
use crate::step3;
use crate::usb::{self, BulkCounter, Rtl};

/// The firmware's data-stage packet limit (findings 5a, 5c): never longer.
pub const XDATA_CHUNK: u16 = 64;
/// Block 2's page nibble 1: bytes copied straight, as the 8051 stores them
/// (findings 5c).
pub const XDATA_STRAIGHT: u16 = 1;
/// The external-data ranges surveyed (inclusive), the project owner's list.
pub const XDATA_RANGES: [(u16, u16); 4] = [(0x2200, 0x2FFF), (0x3100, 0x3FFF), (0x4000, 0xFBFF), (0xFC00, 0xFDFF)];
/// Reference spans read the same way, for the mirror check: the USB and
/// system blocks every earlier scan read (reads only).
pub const XDATA_REFERENCES: [(u16, u16); 2] = [(0x2000, 0x21FF), (0x3000, 0x30FF)];
/// Demodulator pages surveyed, and the reference pages for the mirror check.
pub const NEW_PAGES: std::ops::RangeInclusive<u8> = 5..=15;
pub const REF_PAGES: std::ops::RangeInclusive<u8> = 0..=4;
/// The 8051's data RAM (findings 5d: X:4000-X:40FF; the first survey found
/// X:4100-X:41FF non-zero and contiguous with it): its bytes are never
/// recorded, because it holds the USB string descriptors built from the
/// EEPROM, the serial number among them. Only its class goes in the record.
pub const DATA_RAM: (u16, u16) = (0x4000, 0x41FF);
/// The least time between the two passes over a space.
pub const LIVE_GAP: Duration = Duration::from_millis(100);

fn xdata_read(rtl: &Rtl, addr: u16) -> usb::R<Vec<u8>> {
    rtl.get(addr, windex(BLOCK_SYS, ACCESS_GET, XDATA_STRAIGHT), XDATA_CHUNK)
}

/// One pass over the external-data chunks: Ok(bytes) or the error.
type XPass = Vec<(u16, Result<Vec<u8>, String>)>;

struct Bench {
    rtl: Rtl,
    bulk: BulkCounter,
    recoveries: J,
}

impl Bench {
    fn fresh() -> Result<Bench, String> {
        let (rtl, bulk, setup) = Baseline::LowIfOpen.fresh()?;
        Ok(Bench { rtl, bulk, recoveries: J::Arr(vec![setup.with("kind", "setup")]) })
    }

    /// After a failed read: if the chip no longer answers, power-cycle and
    /// rebuild the held state; the record says which read did it.
    fn after_error(&mut self, what: String) -> Result<bool, String> {
        if step3::alive(&self.rtl) {
            return Ok(false);
        }
        let (rtl, bulk, setup) = Baseline::LowIfOpen.fresh()?;
        self.rtl = rtl;
        self.bulk = bulk;
        self.recoveries.push(setup.with("kind", "recovery").with("after", what));
        Ok(true)
    }
}

fn xpass(b: &mut Bench, chunks: &[u16]) -> Result<XPass, String> {
    let mut out = Vec::new();
    for (n, &a) in chunks.iter().enumerate() {
        if n % 16 == 0 {
            b.bulk.drain();
        }
        match xdata_read(&b.rtl, a) {
            Ok(v) => out.push((a, Ok(v))),
            Err(e) => {
                let wedged = b.after_error(format!("X:{a:04X} read: {e}"))?;
                out.push((a, Err(format!("{e}{}", if wedged { " (chip gone: power cycle, held state rebuilt, span skipped)" } else { "" }))));
            }
        }
    }
    Ok(out)
}

/// One pass over demodulator pages: per page, register 0x00 read first,
/// then 0x00-0xFF byte by byte (None = the read failed).
/// One page's pass: (page, register 0x00 read first, 0x00-0xFF).
type DPage = (u8, Option<u8>, Vec<Option<u8>>);

fn dpass(b: &mut Bench, pages: &[u8]) -> Result<Vec<DPage>, String> {
    let mut out = Vec::new();
    for &p in pages {
        b.bulk.drain();
        let r0 = b.rtl.demod_read8(p, 0x00).ok();
        let mut regs = Vec::with_capacity(256);
        for off in 0..=255u8 {
            if off % 32 == 0 {
                b.bulk.drain();
            }
            match b.rtl.demod_read8(p, off) {
                Ok(v) => regs.push(Some(v)),
                Err(e) => {
                    b.after_error(format!("page {p} 0x{off:02X} read: {e}"))?;
                    regs.push(None);
                }
            }
        }
        out.push((p, r0, regs));
    }
    Ok(out)
}

/// Class of one 64-byte chunk.
fn class(a: &[u8], b: &[u8]) -> &'static str {
    if a != b {
        "live"
    } else if a.iter().all(|&x| x == 0) {
        "all-zero"
    } else if a.iter().all(|&x| x == 0xFF) {
        "all-one"
    } else if a.iter().all(|&x| x == a[0]) {
        "constant"
    } else {
        "other"
    }
}

pub fn run() -> Result<(), String> {
    usb::guard()?;
    let t0 = Instant::now();
    let mut rec = crate::header_with_source("6-survey", "read-only: demodulator pages 5-15 and the 8051's external data outside the scanned ranges, two passes each, from the low-if-open held state")
        .with("writes", "none: every request in this step is a Get")
        .with("demod", "block 0 Get, wValue = offset<<8 | 0x20, wIndex page = 0..=15 (the firmware writes the page to register 0x00 when it changes, findings 5c); one byte per request; register 0x00 read first on each page; pages 0-4 read too as the mirror reference")
        .with("xdata", format!("block 2 Get, page nibble {XDATA_STRAIGHT} (bytes straight, findings 5c), wValue = address, wLength {XDATA_CHUNK} (the firmware's packet limit); ranges {:?}; references (mirror check) {:?}", XDATA_RANGES.map(|(a, b)| format!("0x{a:04X}-0x{b:04X}")), XDATA_REFERENCES.map(|(a, b)| format!("0x{a:04X}-0x{b:04X}"))))
        .with("live", format!("each space read twice, the second pass at least {} ms after the first; a byte that differs is live", LIVE_GAP.as_millis()));
    let mut b = Bench::fresh()?;
    // Demodulator pages.
    let pages: Vec<u8> = REF_PAGES.chain(NEW_PAGES).collect();
    let d1 = dpass(&mut b, &pages)?;
    std::thread::sleep(LIVE_GAP);
    let d2 = dpass(&mut b, &pages)?;
    let page0 = d1.iter().find(|(p, _, _)| *p == 0).map(|(_, _, r)| r.clone()).unwrap_or_default();
    let refs: Vec<(u8, Vec<Option<u8>>)> = d1.iter().filter(|(p, _, _)| REF_PAGES.contains(p)).map(|(p, _, r)| (*p, r.clone())).collect();
    let mut dsum = J::Arr(Vec::new());
    for ((p, r0, a), (_, r0b, bb)) in d1.iter().zip(d2.iter()) {
        if !NEW_PAGES.contains(p) {
            continue;
        }
        let nontrivial: Vec<u8> = (0..=255u8).filter(|&o| a[o as usize].is_some_and(|v| v != 0 && v != 0xFF)).collect();
        let differ0: Vec<u8> = (0..=255u8).filter(|&o| a[o as usize] != page0[o as usize]).collect();
        let live: Vec<u8> = (0..=255u8).filter(|&o| a[o as usize] != bb[o as usize]).collect();
        let failed: Vec<u8> = (0..=255u8).filter(|&o| a[o as usize].is_none()).collect();
        let equal_ref: Vec<J> = refs
            .iter()
            .map(|(rp, rr)| J::obj().with("page", *rp).with("equal_offsets", (0..256).filter(|&o| rr[o] == a[o]).count()))
            .collect();
        let hexes = |v: &[u8]| J::Arr(v.iter().map(|o| hex8(*o)).collect());
        println!(
            "page {p}: reg 0x00 = {:?}/{:?}; non-trivial {}, differ from page 0 at {}, live {}, failed reads {}",
            r0, r0b, nontrivial.len(), differ0.len(), live.len(), failed.len()
        );
        dsum.push(
            J::obj()
                .with("page", *p)
                .with("reg0_first_read", r0.map(hex8))
                .with("reg0_is_page", *r0 == Some(*p))
                .with("reg0_second_pass", r0b.map(hex8))
                .with("bytes_pass1", J::Arr(a.iter().map(|v| v.map(hex8).unwrap_or(J::Null)).collect()))
                .with("nontrivial_offsets", hexes(&nontrivial))
                .with("differ_from_page0_offsets", hexes(&differ0))
                .with("equal_to_reference_page", J::Arr(equal_ref))
                .with("live_offsets", hexes(&live))
                .with("live_values", J::Arr(live.iter().map(|&o| J::obj().with("offset", hex8(o)).with("pass1", a[o as usize].map(hex8)).with("pass2", bb[o as usize].map(hex8))).collect()))
                .with("failed_offsets", hexes(&failed)),
        );
    }
    rec.set("demod_pages", dsum);
    rec.set("demod_reference_pages_pass1", J::Arr(d1.iter().filter(|(p, _, _)| REF_PAGES.contains(p)).map(|(p, _, r)| J::obj().with("page", *p).with("bytes", J::Arr(r.iter().map(|v| v.map(hex8).unwrap_or(J::Null)).collect()))).collect()));
    // External data.
    let mut chunks: Vec<u16> = Vec::new();
    for (lo, hi) in XDATA_REFERENCES.iter().chain(XDATA_RANGES.iter()) {
        let mut a = *lo as u32;
        while a <= *hi as u32 {
            chunks.push(a as u16);
            a += XDATA_CHUNK as u32;
        }
    }
    let x1 = xpass(&mut b, &chunks)?;
    std::thread::sleep(LIVE_GAP);
    let x2 = xpass(&mut b, &chunks)?;
    let in_ref = |a: u16| XDATA_REFERENCES.iter().any(|(lo, hi)| a >= *lo && a <= *hi);
    // Mirror: a non-trivial, non-live chunk equal to an earlier chunk (the
    // references first, then lower addresses).
    let mut seen: HashMap<Vec<u8>, u16> = HashMap::new();
    let mut rows: Vec<(u16, String, Option<u8>)> = Vec::new();
    let mut raw = J::Arr(Vec::new());
    for ((a, r1), (_, r2)) in x1.iter().zip(x2.iter()) {
        let c = match (r1, r2) {
            (Ok(p), Ok(q)) => {
                let k = class(p, q);
                if k == "other" {
                    if let Some(src) = seen.get(p).filter(|&&s| s != *a) {
                        format!("mirror of 0x{src:04X} (delta 0x{:04X})", a.wrapping_sub(*src))
                    } else {
                        seen.insert(p.clone(), *a);
                        k.to_string()
                    }
                } else {
                    k.to_string()
                }
            }
            (Err(e), _) | (_, Err(e)) => format!("read failed: {e}"),
        };
        if !in_ref(*a) {
            let constant = match r1 {
                Ok(p) if c == "constant" => Some(p[0]),
                _ => None,
            };
            rows.push((*a, c.clone(), constant));
            if c == "live" || c == "other" || c.starts_with("read failed") {
                let mut j = J::obj().with("addr", hex16(*a)).with("class", c.as_str());
                if (DATA_RAM.0..=DATA_RAM.1).contains(a) {
                    j.set("bytes", "not recorded: the 8051's data RAM holds the USB descriptors built from the EEPROM, serial number included");
                } else {
                    j.set("pass1", r1.as_ref().ok().map(|v| hex_bytes(v)));
                    j.set("pass2", r2.as_ref().ok().map(|v| hex_bytes(v)));
                }
                raw.push(j);
            }
        }
    }
    // Merge consecutive chunks of one class (mirrors with one delta).
    let mut ranges = J::Arr(Vec::new());
    let mut i = 0;
    while i < rows.len() {
        let (start, ref c, k) = rows[i];
        let mut j = i;
        while j + 1 < rows.len() && rows[j + 1].1 == *c && rows[j + 1].2 == k && rows[j + 1].0 as u32 == rows[j].0 as u32 + XDATA_CHUNK as u32 {
            j += 1;
        }
        let end = rows[j].0 as u32 + XDATA_CHUNK as u32 - 1;
        // A mirror class names its source chunk; merge on the delta instead.
        ranges.push(J::obj().with("from", hex16(start)).with("to", format!("0x{end:04X}")).with("class", c.as_str()).with("constant_byte", k.map(hex8)));
        i = j + 1;
    }
    let mirror_merged = merge_mirrors(&rows);
    for r in &mirror_merged {
        println!("{r}");
    }
    rec.set("xdata_ranges", ranges);
    rec.set("xdata_ranges_mirrors_merged", J::Arr(mirror_merged.into_iter().map(J::from).collect()));
    rec.set("xdata_live_other_failed_chunks", raw);
    rec.set("power_sessions", b.recoveries.clone());
    rec.set("elapsed_s", t0.elapsed().as_secs_f64());
    b.bulk.close();
    crate::write_record("step6-survey.json", &rec);
    Ok(())
}

/// The ranges in words, with consecutive mirror chunks of one delta merged.
fn merge_mirrors(rows: &[(u16, String, Option<u8>)]) -> Vec<String> {
    let key = |c: &str| -> String {
        match c.find("(delta ") {
            Some(i) => format!("mirror {}", &c[i..]),
            None => c.to_string(),
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let k = key(&rows[i].1);
        let mut j = i;
        while j + 1 < rows.len() && key(&rows[j + 1].1) == k && rows[j + 1].2 == rows[i].2 && rows[j + 1].0 as u32 == rows[j].0 as u32 + XDATA_CHUNK as u32 {
            j += 1;
        }
        let end = rows[j].0 as u32 + XDATA_CHUNK as u32 - 1;
        let cb = rows[i].2.map(|b| format!(" 0x{b:02X}")).unwrap_or_default();
        out.push(format!("X:{:04X}-X:{end:04X} {}{cb} ({} chunks)", rows[i].0, k, j - i + 1));
        i = j + 1;
    }
    out
}
