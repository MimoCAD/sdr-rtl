//! Step 5: read the chip's ROM through `GetROMCode` ([RTL2832U] Table 27,
//! p. 37: bmRequestType 0xC0, wIndex 0x0400 = block 4 "ROM Code" of
//! Table 28, wValue = the ROM address, wLength = the size to read).
//! Read-only: no Set request is sent.
//!
//! The ROM image is written OUTSIDE every repository, to
//! `~/mimosdr-captures/rtl2832u-rom/` (the project owner's rule for the
//! firmware analysis, 2026-10-09). The record under `derivation/data/`
//! carries only the image's size, its SHA-256, the read method and the
//! consistency check: no byte of the ROM.
//!
//! Method:
//! 1. Chunk size: reads of several lengths at three addresses compared
//!    with the same span read one byte per request (the smallest request,
//!    so the reference cannot depend on how the chip packs a data stage).
//!    The chunk is the largest tested length whose every read came back
//!    full and equal to the reference.
//! 2. The whole 16-bit wValue space, 0x0000-0xFFFF, read twice in that
//!    chunk (passes A and B), each chunk's outcome kept (full, short,
//!    stall); then a power cycle and a third pass (C).
//! 3. The size: the ROM is what reads the same in two power sessions,
//!    from 0x0000 up to the first address where pass C differs from pass
//!    A (or the first failed read). Recorded beside it: a statistics map
//!    of the space per 4 KiB, whether the space mirrors at a power-of-two
//!    period, and the documented system and USB blocks read through their
//!    own requests and through GetROMCode at the same wValue (verdicts
//!    only).
//! 4. wIndex page nibble 1-15 (Table 28: "true only as [2]=0") read at
//!    two addresses and compared with page 0: a banked ROM would differ.
//!
//! Why 64: `rom-dump --probe ADDR:LEN ...` (prints only, no record)
//! measured the request lengths first; see [`CHUNK_TRIALS`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlType, Recipient};

use crate::json::{J, hex16};
use crate::regs::*;
use crate::usb::{self, Rtl};

/// The addressable bound: wValue is 16 bits (Table 27).
const SPACE: usize = 0x1_0000;
/// The lengths tried for the chunk; 64 is the control endpoint's packet
/// size at high speed (read from the descriptor and recorded). Longer
/// lengths are not tried: `rom-dump --probe` (2026-10-09, from a fresh
/// power-on) read 1, 8 and 64 bytes in ~1.7 ms each; 65 bytes ended in a
/// Stall after ~84 ms with the chip still answering; 256 bytes ended in a
/// Stall after ~98 ms and then every request (GetROMCode and a system-
/// block read alike) timed out until a power cycle.
const CHUNK_TRIALS: [u16; 6] = [1, 8, 32, 63, 64, 65];
const PROBE_NOTE: &str = "rom-dump --probe, 2026-10-09, fresh power-on: wLength 1, 8, 64 answered in ~1.7 ms; 65 -> Stall after ~84 ms, the chip still answering; 256 -> Stall after ~98 ms, then every request (GetROMCode and the system block's DEMOD_CTL alike) Cancelled at the timeout until a power cycle; so no length above 65 is tried";
/// Where the chunk trials read: the start, the middle of the lower half,
/// the start of the upper half.
const TRIAL_ADDRESSES: [u16; 3] = [0x0000, 0x2000, 0x8000];
/// The span each trial address checks byte by byte.
const TRIAL_SPAN: usize = 1024;
const TIMEOUT: Duration = Duration::from_millis(500);

fn out_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let p = PathBuf::from(home).join("mimosdr-captures/rtl2832u-rom");
    std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
    Ok(p)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Full,
    Short(usize),
    Failed,
}

/// One GetROMCode; the bytes returned (possibly fewer than asked) or the
/// transfer error.
fn get_rom(rtl: &Rtl, addr: u16, len: u16, page: u16) -> Result<Vec<u8>, String> {
    rtl.dev
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: BREQUEST,
                value: addr,
                index: windex(BLOCK_ROM, ACCESS_GET, page),
                length: len,
            },
            TIMEOUT,
        )
        .wait()
        .map_err(|e| format!("{e:?}"))
}

/// The span [addr, addr+n) one byte per request; None at a failure.
fn bytewise(rtl: &Rtl, addr: u16, n: usize) -> Result<Vec<u8>, String> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let a = addr as usize + i;
        let b = get_rom(rtl, a as u16, 1, 0).map_err(|e| format!("byte read at 0x{a:04X}: {e}"))?;
        if b.len() != 1 {
            return Err(format!("byte read at 0x{a:04X}: {} bytes", b.len()));
        }
        v.push(b[0]);
    }
    Ok(v)
}

/// Chunk trials: for each length, read the trial spans in that length and
/// compare with the byte-by-byte reference.
fn chunk_trials(rtl: &Rtl) -> Result<(u16, J), String> {
    let mut refs = Vec::new();
    for &a in &TRIAL_ADDRESSES {
        refs.push(bytewise(rtl, a, TRIAL_SPAN)?);
    }
    let mut rows = J::Arr(Vec::new());
    let mut best = 0u16;
    for &len in &CHUNK_TRIALS {
        let mut full = 0usize;
        let mut short = 0usize;
        let mut failed = 0usize;
        let mut mismatched_bytes = 0usize;
        for (k, &a) in TRIAL_ADDRESSES.iter().enumerate() {
            let mut off = 0usize;
            while off < TRIAL_SPAN {
                let n = (len as usize).min(TRIAL_SPAN - off);
                match get_rom(rtl, a + off as u16, n as u16, 0) {
                    Ok(b) => {
                        if b.len() == n {
                            full += 1;
                        } else {
                            short += 1;
                        }
                        mismatched_bytes += b.iter().zip(&refs[k][off..off + n]).filter(|(x, y)| x != y).count();
                    }
                    Err(_) => failed += 1,
                }
                off += n;
            }
        }
        let ok = short == 0 && failed == 0 && mismatched_bytes == 0;
        if ok {
            best = best.max(len);
        }
        rows.push(
            J::obj()
                .with("length", len)
                .with("reads_full", full)
                .with("reads_short", short)
                .with("reads_failed", failed)
                .with("bytes_differing_from_bytewise", mismatched_bytes)
                .with("agrees", ok),
        );
    }
    if best == 0 {
        return Err("no chunk length agreed with the byte-by-byte reference".into());
    }
    Ok((best, rows))
}

struct Pass {
    data: Vec<u8>,
    outcomes: Vec<(u16, Outcome)>,
    ms: u128,
}

fn read_space(rtl: &Rtl, chunk: u16) -> Pass {
    let start = Instant::now();
    let mut data = vec![0u8; SPACE];
    let mut outcomes = Vec::new();
    let mut a = 0usize;
    while a < SPACE {
        let n = (chunk as usize).min(SPACE - a);
        let o = match get_rom(rtl, a as u16, n as u16, 0) {
            Ok(b) => {
                data[a..a + b.len()].copy_from_slice(&b);
                if b.len() == n { Outcome::Full } else { Outcome::Short(b.len()) }
            }
            Err(_) => Outcome::Failed,
        };
        outcomes.push((a as u16, o));
        a += n;
    }
    Pass { data, outcomes, ms: start.elapsed().as_millis() }
}

fn pass_summary(p: &Pass) -> J {
    let full = p.outcomes.iter().filter(|(_, o)| *o == Outcome::Full).count();
    let bad: Vec<J> = p
        .outcomes
        .iter()
        .filter(|(_, o)| *o != Outcome::Full)
        .take(32)
        .map(|(a, o)| J::obj().with("address", hex16(*a)).with("outcome", format!("{o:?}")))
        .collect();
    J::obj()
        .with("chunks", p.outcomes.len())
        .with("chunks_full", full)
        .with("chunks_not_full_first_32", J::Arr(bad))
        .with("elapsed_ms", p.ms as u64)
}

/// The first address whose chunk was not read in full.
fn first_failure(p: &Pass) -> Option<usize> {
    p.outcomes.iter().find(|(_, o)| *o != Outcome::Full).map(|(a, o)| match o {
        Outcome::Short(n) => *a as usize + n,
        _ => *a as usize,
    })
}

/// The smallest power-of-two period (>= 256) at which `d` repeats whole.
fn mirror_period(d: &[u8]) -> Option<usize> {
    let mut p = 256;
    while p < d.len() {
        if (p..d.len()).all(|i| d[i] == d[i % p]) {
            return Some(p);
        }
        p *= 2;
    }
    None
}

fn differing(a: &[u8], b: &[u8]) -> (usize, Option<usize>) {
    let mut n = 0;
    let mut first = None;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            n += 1;
            first.get_or_insert(i);
        }
    }
    (n, first)
}

/// `rom-dump --probe`: a handful of reads with their timing and outcome,
/// printed only (no record, no image), to size the method's requests.
fn probe(reads: &[(u16, u16)]) -> Result<(), String> {
    if !crate::no_cycle() {
        println!("power cycle: {}", usb::power_cycle()?.render());
    }
    let rtl = Rtl::open()?;
    let alive = |rtl: &Rtl, when: &str| match rtl.sys_read8(0) {
        Ok(v) => println!("{when}: DEMOD_CTL reads 0x{v:02X} (the chip answers)"),
        Err(e) => println!("{when}: DEMOD_CTL read {e}"),
    };
    alive(&rtl, "before");
    // The register blocks through their own requests and through
    // GetROMCode at the same wValue, same session (register values only).
    for &(name, base, off, len) in &[("sys", SYS_BASE, 0u16, 16u16), ("sys", SYS_BASE, 0x40, 16), ("usb", USB_BASE, 0, 16), ("usb", USB_BASE, 0x140, 32)] {
        let own = if name == "sys" { rtl.sys_read(off, len) } else { rtl.usb_read(off, len) };
        let rom = get_rom(&rtl, base + off, len, 0);
        println!("{name} 0x{:04X} own {:02X?}", base + off, own.map_err(|e| e.to_string()));
        println!("{name} 0x{:04X} rom {:02X?}", base + off, rom);
    }
    for &(addr, len) in reads {
        let t = Instant::now();
        let r = get_rom(&rtl, addr, len, 0);
        let us = t.elapsed().as_micros();
        match r {
            Ok(b) => println!("addr 0x{addr:04X} len {len}: {} bytes in {us} us", b.len()),
            Err(e) => println!("addr 0x{addr:04X} len {len}: {e} after {us} us"),
        }
        alive(&rtl, "  after");
    }
    Ok(())
}

/// The documented register blocks read through their own requests and
/// through GetROMCode at the same wValue, in the same session: equal as
/// read, equal with the bytes of each 32-bit word reversed, or neither.
/// Only the verdicts are recorded (these are register values, not ROM).
fn register_blocks(rtl: &Rtl) -> J {
    let mut out = J::Arr(Vec::new());
    for &(name, base, off, len) in &[("system", SYS_BASE, 0u16, 16u16), ("system", SYS_BASE, 0x40, 20), ("usb", USB_BASE, 0, 16), ("usb", USB_BASE, 0x144, 32)] {
        let own = if name == "system" { rtl.sys_read(off, len).map_err(|e| e.to_string()) } else { rtl.usb_read(off, len).map_err(|e| e.to_string()) };
        let rom = get_rom(rtl, base + off, len, 0);
        let verdict = match (&own, &rom) {
            (Ok(o), Ok(r)) if o == r => "equal as read".to_string(),
            (Ok(o), Ok(r)) if o.len() == r.len() && o.chunks(4).zip(r.chunks(4)).all(|(x, y)| x.iter().rev().eq(y.iter())) => {
                "equal with the 4 bytes of every 32-bit word reversed".to_string()
            }
            (Ok(_), Ok(r)) if r.iter().all(|&x| x == r[0]) => "different: GetROMCode reads one repeated value".to_string(),
            (Ok(o), Ok(r)) => format!("different in {} of {} bytes", o.iter().zip(r).filter(|(x, y)| x != y).count(), o.len()),
            (o, r) => format!("own request {:?}, GetROMCode {:?}", o.as_ref().map(|v| v.len()), r.as_ref().map(|v| v.len())),
        };
        out.push(J::obj().with("block", name).with("wValue", hex16(base + off)).with("length", len).with("verdict", verdict));
    }
    out
}

/// Per 4 KiB of the space: how many bytes are zero, how many distinct
/// values, how many differ between passes A and C. Statistics only.
fn region_map(a: &[u8], c: &[u8]) -> J {
    let mut out = J::Arr(Vec::new());
    for base in (0..a.len()).step_by(0x1000) {
        let s = &a[base..base + 0x1000];
        let mut seen = [false; 256];
        s.iter().for_each(|&x| seen[x as usize] = true);
        out.push(
            J::obj()
                .with("start", hex16(base as u16))
                .with("zero_bytes", s.iter().filter(|&&x| x == 0).count())
                .with("distinct_values", seen.iter().filter(|&&x| x).count())
                .with("differing_a_c", s.iter().zip(&c[base..base + 0x1000]).filter(|(x, y)| x != y).count()),
        );
    }
    out
}

/// The runs of 256-byte blocks in which `a` and `b` differ, as
/// "0xSTART-0xEND(bytes differing)".
fn differing_ranges(a: &[u8], b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Option<(usize, usize, usize)> = None;
    for blk in 0..a.len() / 256 {
        let n = (blk * 256..blk * 256 + 256).filter(|&i| a[i] != b[i]).count();
        match (&mut run, n > 0) {
            (Some((_, end, c)), true) => {
                *end = blk * 256 + 255;
                *c += n;
            }
            (None, true) => run = Some((blk * 256, blk * 256 + 255, n)),
            (Some((s, e, c)), false) => {
                out.push(format!("0x{s:04X}-0x{e:04X}({c})"));
                run = None;
            }
            (None, false) => {}
        }
    }
    if let Some((s, e, c)) = run {
        out.push(format!("0x{s:04X}-0x{e:04X}({c})"));
    }
    out
}

/// `rom-dump --xdata-check`: read-only checks of what the firmware listing
/// predicts about the USB (1) and system (2) request blocks — one handler,
/// wValue a plain 16-bit external-data address, the bytes of each 32-bit
/// word reversed only when the wIndex page nibble is 0. Prints only.
fn xdata_check() -> Result<(), String> {
    if !crate::no_cycle() {
        println!("power cycle: {}", usb::power_cycle()?.render());
    }
    let rtl = Rtl::open()?;
    let show = |what: &str, r: Result<Vec<u8>, String>| println!("{what:<58} {:02X?}", r);
    let get = |value: u16, block: u16, page: u16, len: u16| {
        rtl.get(value, windex(block, ACCESS_GET, page), len).map_err(|e| e.to_string())
    };
    for &(value, len) in &[(0x3000u16, 8u16), (0x2144, 4), (0x2158, 4), (0x2000, 4)] {
        show(&format!("block 2 (system) wValue 0x{value:04X}, page nibble 0"), get(value, BLOCK_SYS, 0, len));
        show(&format!("block 1 (USB)    wValue 0x{value:04X}, page nibble 0"), get(value, BLOCK_USB, 0, len));
        show(&format!("block 2 (system) wValue 0x{value:04X}, page nibble 1"), get(value, BLOCK_SYS, 1, len));
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), String> {
    usb::guard()?;
    if args.iter().any(|a| a == "--xdata-check") {
        return xdata_check();
    }
    if args.iter().any(|a| a == "--probe") {
        // ADDR:LEN pairs (hex with 0x, else decimal) after --probe.
        let mut reads = Vec::new();
        for a in args.iter().filter(|a| a.contains(':')) {
            let (x, y) = a.split_once(':').unwrap_or_default();
            let num = |s: &str| match s.strip_prefix("0x") {
                Some(h) => u16::from_str_radix(h, 16).ok(),
                None => s.parse().ok(),
            };
            match (num(x), num(y)) {
                (Some(addr), Some(len)) => reads.push((addr, len)),
                _ => return Err(format!("--probe: bad ADDR:LEN {a}")),
            }
        }
        return probe(&reads);
    }
    let dir = out_dir()?;
    let mut rec = crate::header("5-rom-dump", "the ROM read through GetROMCode (Table 27): its size, SHA-256, the read method and the consistency of repeated reads; no ROM byte is recorded");
    let cyc = if crate::no_cycle() {
        J::obj().with("skipped", "--no-cycle: the dongle was opened as found, not power-cycled")
    } else {
        usb::power_cycle()?
    };
    rec.set("power_cycle_before", cyc);
    let rtl = Rtl::open()?;
    let mps0 = rtl.dev.device_descriptor().max_packet_size_0();
    rec.set(
        "request",
        J::obj()
            .with("bmRequestType", "0xC0")
            .with("bRequest", "0x00 (Table 27: don't care)")
            .with("wIndex", hex16(windex(BLOCK_ROM, ACCESS_GET, 0)))
            .with("wValue", "the ROM address (Table 27: \"Rom Code Address\")")
            .with("wLength", "the chunk length")
            .with("timeout_ms", TIMEOUT.as_millis() as u64)
            .with("control_endpoint_max_packet", mps0),
    );
    println!("step 5: chunk trials (lengths {CHUNK_TRIALS:?} at {TRIAL_ADDRESSES:04X?}, {TRIAL_SPAN} B each, against one-byte reads)");
    let (chunk, trials) = chunk_trials(&rtl)?;
    println!("  chunk = {chunk}");
    rec.set(
        "chunk",
        J::obj()
            .with("rule", "the largest tested length whose every read at every trial address came back full and equal, byte for byte, to the same span read one byte per request")
            .with("trial_addresses", J::Arr(TRIAL_ADDRESSES.iter().map(|a| hex16(*a)).collect()))
            .with("trial_span_bytes", TRIAL_SPAN)
            .with("trials", trials)
            .with("longer_lengths", PROBE_NOTE)
            .with("chosen", chunk),
    );

    println!("  pass A");
    let a = read_space(&rtl, chunk);
    println!("  pass B");
    let b = read_space(&rtl, chunk);

    // wIndex page nibble: does the ROM block bank by page?
    let mut pages = J::Arr(Vec::new());
    for &addr in &[0x0000u16, 0x8000] {
        let base = get_rom(&rtl, addr, 64, 0).ok();
        for page in 1..16u16 {
            let r = get_rom(&rtl, addr, 64, page);
            let same = match (&base, &r) {
                (Some(x), Ok(y)) => J::from(x == y),
                _ => J::Null,
            };
            pages.push(
                J::obj()
                    .with("address", hex16(addr))
                    .with("wIndex", hex16(windex(BLOCK_ROM, ACCESS_GET, page)))
                    .with("outcome", match &r {
                        Ok(y) => format!("{} bytes", y.len()),
                        Err(e) => e.clone(),
                    })
                    .with("equal_to_page_0", same),
            );
        }
    }
    let blocks = register_blocks(&rtl);
    drop(rtl);

    let cyc2 = if crate::no_cycle() {
        J::obj().with("skipped", "--no-cycle")
    } else {
        usb::power_cycle()?
    };
    let rtl = Rtl::open()?;
    println!("  pass C (after a power cycle)");
    let c = read_space(&rtl, chunk);
    drop(rtl);

    let (ab_n, ab_first) = differing(&a.data, &b.data);
    let (ac_n, ac_first) = differing(&a.data, &c.data);
    let outcomes_equal = a.outcomes == b.outcomes && a.outcomes == c.outcomes;
    // The raw passes, outside every repository, for the analysis.
    for (name, p) in [("pass-a", &a), ("pass-b", &b), ("pass-c", &c)] {
        let f = dir.join(format!("rtl2832u-rom-space-{name}.bin"));
        std::fs::write(&f, &p.data).map_err(|e| format!("write {}: {e}", f.display()))?;
    }
    let ac_ranges = differing_ranges(&a.data, &c.data);
    println!("  A/C differing 256-byte blocks: {}", ac_ranges.join(" "));

    let fail = first_failure(&a);
    // A pass with failed chunks holds zeros there: no image.
    if fail == Some(0) || a.data.iter().all(|&x| x == 0x00 || x == 0xFF) {
        return Err(format!(
            "pass A has no content (first failed read at {fail:?}; every byte 0x00 or 0xFF: {}); nothing written",
            a.data.iter().all(|&x| x == 0x00 || x == 0xFF)
        ));
    }
    if ab_n != 0 || !outcomes_equal {
        return Err(format!("passes A and B differ ({ab_n} bytes, outcomes equal {outcomes_equal}); nothing written"));
    }
    // The size: the ROM is the part of the space that reads the same in
    // two power sessions, from 0x0000 up to the first address that does
    // not (or the first failed read). Measured 2026-10-09: 0x2000-0x2FFF
    // is one repeated byte value that changes per power session, and it is
    // not the USB block (GetUSBReg at the same wValue reads the documented
    // registers in the same session); 0x3000 up holds the system block's
    // registers (see `register_blocks`).
    let volatile = ac_first;
    let size = [fail, volatile].into_iter().flatten().min().unwrap_or(SPACE);
    let size_basis = match (fail, volatile) {
        (Some(f), _) if Some(f) == Some(size) => format!("the first failed read at 0x{f:04X}"),
        (_, Some(v)) if v == size => format!(
            "0x0000-0x{:04X} reads identically in passes A, B and C (C after a power cycle); from 0x{v:04X} pass C differs from A: the first address that is not the same memory in every power session",
            v - 1
        ),
        _ => "no read failed and every pass agrees over the whole 16-bit space: the 16-bit wValue is the bound".to_string(),
    };
    let image = &a.data[..size];
    if image.iter().all(|&x| x == 0x00 || x == 0xFF) {
        return Err("the stable part of the space is all 0x00/0xFF; nothing written".into());
    }
    let sha = sha256_hex(image);
    let bin = dir.join("rtl2832u-rom.bin");
    std::fs::write(&bin, image).map_err(|e| format!("write {}: {e}", bin.display()))?;

    rec.set("power_cycle_before_pass_c", cyc2);
    rec.set(
        "passes",
        J::obj()
            .with("space", "wValue 0x0000-0xFFFF")
            .with("a", pass_summary(&a))
            .with("b", pass_summary(&b))
            .with("c_after_power_cycle", pass_summary(&c)),
    );
    let (img_ac, _) = differing(image, &c.data[..size]);
    let (img_ab, _) = differing(image, &b.data[..size]);
    rec.set(
        "consistency",
        J::obj()
            .with("image_bytes_differing_a_b", img_ab)
            .with("image_bytes_differing_a_c", img_ac)
            .with("image_identical_in_a_b_c", img_ab == 0 && img_ac == 0)
            .with("space_bytes_differing_a_b", ab_n)
            .with("first_differing_a_b", ab_first.map(|i| hex16(i as u16)))
            .with("space_bytes_differing_a_c", ac_n)
            .with("first_differing_a_c", ac_first.map(|i| hex16(i as u16)))
            .with("space_a_c_differing_256_byte_blocks", J::Arr(ac_ranges.iter().map(|s| J::from(s.as_str())).collect()))
            .with("chunk_outcomes_equal_in_all_passes", outcomes_equal),
    );
    rec.set("space_map_4KiB", region_map(&a.data, &c.data));
    rec.set("register_blocks_through_GetROMCode", blocks);
    rec.set("wIndex_page_nibble", pages);
    rec.set(
        "size",
        J::obj()
            .with("bytes", size)
            .with("hex", format!("0x{size:X}"))
            .with("basis", size_basis)
            .with("first_failed_address", fail.map(|f| format!("0x{f:04X}")))
            .with("space_mirror_period", mirror_period(&a.data).map(|m| format!("0x{m:X}"))),
    );
    rec.set("sha256", sha.clone());
    rec.set(
        "image_location",
        "outside every repository: ~/mimosdr-captures/rtl2832u-rom/rtl2832u-rom.bin (the image), rtl2832u-rom-space-pass-{a,b,c}.bin (the whole 16-bit space per pass)",
    );
    crate::write_record("step5-rom-dump.json", &rec);
    println!("  size {size} B (0x{size:X}); SHA-256 {sha}");
    println!("  image A/B {img_ab} differing, A/C {img_ac} differing; space A/B {ab_n}, A/C {ac_n}; outcomes equal: {outcomes_equal}");
    println!("  wrote {}", bin.display());
    Ok(())
}

// ---- SHA-256 (FIPS 180-4), so the record needs no dependency ----

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub fn sha256_hex(msg: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut m = msg.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((msg.len() as u64) * 8).to_be_bytes());
    for block in m.chunks(64) {
        let mut w = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes([block[4 * t], block[4 * t + 1], block[4 * t + 2], block[4 * t + 3]]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for t in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[t]).wrapping_add(w[t]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}
