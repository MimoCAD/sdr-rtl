//! Step 3, block scan: every byte value at every address of the register
//! spaces the single-byte scans have not covered yet, one byte at a time
//! on top of the documented-only baseline, plain method (no restart
//! between candidates, as data/step3-bytescan.jsonl). The byte scan over
//! demodulator pages 0-2 found no value that streams; these are the other
//! spaces the protocol declares and the documents do not map whole:
//!
//! - `sys`: request block 2 ([RTL2832U] Table 28), wValue = 0x3000 +
//!   offset (step 1A), offsets 0x00..=0xFF. Table 14 (p. 27) maps
//!   0x0000-0x000C and the I2C master at 0x0040-0x0053 (pp. 28, 33-36).
//! - `usb`: request block 1, wValue = 0x2000 + offset, offsets
//!   0x000..=0x1FF. Table 29 (p. 38) maps five 32-bit registers between
//!   0x0000 and 0x0160 and calls the rest reserved up to 0x0FFF.
//! - `p3`, `p4`: demodulator pages 3 and 4 as write targets, offsets
//!   0x00..=0xFF (page 3 reads live values; page 4 partly mirrors page 0:
//!   data/step3-scan.jsonl).
//!
//! Candidate = (offset, value), iterated offset then value, ascending; the
//! value the address holds when its candidate comes up is logged as a skip.
//! Per candidate: read the original; write; read back (a register that
//! does not read back what was written is still measured); a 50 ms bulk
//! window; a 200 ms follow-up on any byte; restore; verify; on any byte a
//! 50 ms window after the restore; the baseline re-checked (demodulator
//! and USB items), reapplied if disturbed, power cycle if it does not
//! hold or the chip stops answering. The windows and the streams
//! criterion are the plain byte scan's.
//!
//! USB and system registers are 32-bit little-endian words (step 1A/1D,
//! data/step1-sys-usb-encoding.json, data/step1-writes.json); byte reads
//! inside them work (step 1A). Each candidate here is one byte written
//! with wLength 1 at one address: whether the chip takes a one-byte write
//! into a 32-bit register is what the read-back (`in`) records.
//!
//! One record per block: data/step3-blockscan-{sys,usb,p3,p4}.jsonl,
//! compact lines (legend in the header), resumable by (offset, value).
//! `--limit N` stops after N new tested candidates; `--out PATH` writes a
//! scratch file; `--from OFF [VAL]` skips every candidate before it.

use std::collections::HashSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use crate::bytescan::{FOLLOW_UP, WINDOW, field};
use crate::json::J;
use crate::regs::*;
use crate::restart::bulk_events;
use crate::step3::{self, RATE_HZ};
use crate::usb::{self, BulkCounter, Rtl};

/// After a candidate that gave bytes, the window after the restore.
const AFTER_RESTORE: Duration = Duration::from_millis(50);

/// What the line's self-check fields mean, for the setup line.
const CHECK_FIELDS: &str = "Line fields q / qf / qa = [submitted, completed, errored, halts, fewest in flight, halted at any point 0/1]";

/// The four register spaces this scan covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Sys,
    Usb,
    P3,
    P4,
}

impl Block {
    pub const ALL: [Block; 4] = [Block::Sys, Block::Usb, Block::P3, Block::P4];

    pub fn parse(s: &str) -> Option<Block> {
        Block::ALL.into_iter().find(|b| b.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Block::Sys => "sys",
            Block::Usb => "usb",
            Block::P3 => "p3",
            Block::P4 => "p4",
        }
    }

    pub fn record(self) -> String {
        format!("step3-blockscan-{}.jsonl", self.name())
    }

    /// Offsets covered: 0..n.
    pub fn offsets(self) -> u16 {
        match self {
            Block::Usb => 0x200,
            _ => 0x100,
        }
    }

    fn read(self, rtl: &Rtl, off: u16) -> usb::R<u8> {
        match self {
            Block::Sys => rtl.sys_read8(off),
            Block::Usb => Ok(rtl.usb_read(off, 1)?[0]),
            Block::P3 => rtl.demod_read8(3, off as u8),
            Block::P4 => rtl.demod_read8(4, off as u8),
        }
    }

    fn write(self, rtl: &Rtl, off: u16, v: u8) -> usb::R<()> {
        match self {
            Block::Sys => rtl.sys_write8(off, v),
            Block::Usb => rtl.usb_write(off, &[v]),
            Block::P3 => rtl.demod_write8(3, off as u8, v),
            Block::P4 => rtl.demod_write8(4, off as u8, v),
        }
    }

    /// The candidate space, as the record header states it.
    fn space(self) -> String {
        let (what, lo, hi) = match self {
            Block::Sys => ("system block (request block 2, Table 28), wValue = 0x3000 + offset (step 1A)", 0x3000u32, 0x30FFu32),
            Block::Usb => ("USB block (request block 1, Table 28), wValue = 0x2000 + offset (step 1A)", 0x2000, 0x21FF),
            Block::P3 => ("demodulator page 3 (request block 0, wIndex page 3), wValue = offset << 8 | 0x20 (step 1C)", 0, 0xFF),
            Block::P4 => ("demodulator page 4 (request block 0, wIndex page 4), wValue = offset << 8 | 0x20 (step 1C)", 0, 0xFF),
        };
        format!(
            "{what}: offset 0x00..=0x{:X} (wValue 0x{lo:04X}..=0x{hi:04X}) x value 0x00..=0xFF, except the value the address holds when its candidate comes up (logged as kind s); iterated offset, value in ascending order with no reordering; {} candidates",
            self.offsets() - 1,
            self.offsets() as usize * 256
        )
    }

    /// Why the offset bound, from the documented map, and what lies outside it.
    fn bound(self) -> &'static str {
        match self {
            Block::Sys => "[RTL2832U] Table 14 (p. 27-28) maps the system block at 0x0000-0x000C (DEMOD_CTL, GPO, GPI, GPOE, GPD, SYSINTE, SYSINTS, GP_CFG0, GP_CFG1, SYSINTE_1, SYSINTS_1, DEMOD_CTL_1, IR_SUSPEND) and the I2C master at 0x0040-0x0053 (I2CCR, I2CMCR, I2CMSTR, I2CMSR, I2CMFR; Tables 22-26); 0x00-0xFF covers both, the gap 0x0D-0x3F between them and 0x54-0xFF after them, the room an 8051 system's unmapped registers would occupy. Deferred: the IrDA registers of the same table, IR_RX_BUFF 0xFC00-0xFC7F (a 128-byte receive buffer) and IR_RX_IE..IR_RESUME_MASK_T_LEN 0xFD00-0xFD19 (26 control registers): 0x3000 + 0xFC00 does not fit a 16-bit wValue, Table 28 names a separate request block (5, IR) and gives no base for it, and they are an infrared receiver's buffer and control, a larger and separately addressed space to be declared on its own if this one is empty",
            Block::Usb => "[RTL2832U] Table 29 (p. 38) maps the USB block as USB_SYSCTL 0x0000, USB_EPA_CFG 0x0144, USB_EPA_CTL 0x0148, USB_EPA_MAXPKT 0x0158, USB_EPA_FIFO_CFG 0x0160 (each 32-bit, Tables 30-34) with reserved ranges 0x0004-0x0140, 0x014C-0x0154, 0x015C and 0x0164-0x0FFF. 0x000-0x1FF covers every named register, every reserved gap among them and 0x0164-0x01FF after the last one; outside the bound lies only the reserved 0x0200-0x0FFF (3584 bytes), a later declaration if this one is empty",
            Block::P3 => "Table 27 (p. 36) gives the demodulator pages 0-4; page 3 is where the documents place the readbacks (if_agc_val, rf_agc_val, aagc_lock, dagc_val, inc_det_cnt, sfreq_off, cfreq_off, mode_gi_idx: Tables 2, 9-12, all read-only). Offsets 0x00-0xFF, the whole page, as for pages 0-2",
            Block::P4 => "Table 27 (p. 36) gives the demodulator pages 0-4 and documents no register on page 4. Offsets 0x00-0xFF, the whole page, as for pages 0-2",
        }
    }

    /// What the earlier records already say about this space.
    fn known(self) -> &'static str {
        match self {
            Block::Sys => "step 1A (data/step1-sys-usb-encoding.json): power-on values DEMOD_CTL 0x20, GPO 0x18, GPOE 0x19, GPD 0x06, GP_CFG0 0x96, GP_CFG1 0x55, I2CCR 0x17, I2CMCR 0x02000028, I2CMSTR 0x00930910; the baseline writes DEMOD_CTL 0xE8 and GPO 0x10 (data/step3-baseline.json). Step 2: GPIO3 does not switch the tuner's supply on this board; step 4a: the demodulator is clocked from the tuner's clock output",
            Block::Usb => "step 1A/1D: power-on USB_SYSCTL 0x01, USB_EPA_CFG 0xD1, USB_EPA_CTL 0, USB_EPA_MAXPKT 0x200, USB_EPA_FIFO_CFG 0x14; USB_EPA_MAXPKT written as a 32-bit word reads back (data/step1-writes.json); the baseline sets EPA_MAXPKT 0x200, EPA_CFG 0xD1, pulses the FIFO reset, clears the stall bit, sets DMA (data/step3-baseline.json). Step 3 detector-check: USB_EPA_CTL bit 4 halts the endpoint, ClearFeature(ENDPOINT_HALT) clears both the halt and the chip's bit",
            Block::P3 => "bit scan (data/step3-scan.jsonl, 2048 candidates on page 3): 0 bytes, 0 wedges, 0 disturbed baselines, 0 write or read errors; 60 offsets did not read back their restore (live values): 0x05-0x11, 0x14-0x1A, 0x1D, 0x1E, 0x20, 0x21, 0x24-0x27, 0x2A-0x2D, 0x2F, 0x30, 0x32, 0x34, 0x35, 0x37, 0x3A, 0x3B, 0x40, 0x41, 0x50-0x52, 0x5D, 0x5E, 0x60, 0x61, 0x66-0x6D, 0x72, 0x77, 0x79; 58 offsets read non-zero at the baseline. A value that does not read back is still measured",
            Block::P4 => "bit scan (data/step3-scan.jsonl, 2048 candidates on page 4): 0 bytes; writes at 0x08 (bits 0-7 cleared AD_EN: baseline disturbed), 0x10 (bit 1: a demodulator soft reset, baseline disturbed, restore stalled), 0x20 (bit 4) and 0xA7 (bits 5, 6) wedged the chip - the same offsets and effects as page 0's, so page 4 writes reach page 0's registers there; at bit 0 page 4's reads equalled page 0's at 106 of 256 offsets, and page 4 read 0x00 everywhere but 0x00 (0x04), 0x01 (0x02), 0x1C (0x03), 0x46 (0x10), 0x56 (0x20), 0xAD (0x0A). Every offset is covered regardless",
        }
    }

    /// Documented effects a candidate here can have on the bench.
    fn hazards(self) -> &'static str {
        match self {
            Block::Sys => "DEMOD_CTL (0x00): values with bit 5 clear hold the demodulator in reset (its registers return to power-on values; the baseline check reapplies after the restore), bit 7 clear stops its PLL, bits 6/3 the ADCs (Table 15). GPO (0x01): bit 3 is the tuner power pin, bits 1 and 5 the clock-select latches (Tables 16-19). I2CMCR (0x44-0x47): bit 30 'Command Start' begins an I2C transaction with I2CMFR (0x50) as its FIFO byte (Tables 23, 26) on the bus that carries the tuner, the demodulator's own slave and the configuration EEPROM (Fig. 5, Fig. 9; step 2's address sweep); the documents do not say what a one-byte transaction started with arbitrary length and address bits does to those devices. A chip that stops answering is power-cycled",
            Block::Usb => "USB_SYSCTL (0x00-0x03): bit 10 (offset 0x01 bit 2) SIE reset - expected to drop the device off the bus until a power cycle; bit 3 full-packet mode; bit 0 DMA enable. USB_EPA_CFG (0x144-0x147): endpoint enable, type, number. USB_EPA_CTL (0x148-0x14B): bit 4 stalls the endpoint (the detector clears the halt and records it), bit 9 FIFO reset, bit 5 flush. USB_EPA_MAXPKT (0x158), USB_EPA_FIFO_CFG (0x160). The USB baseline items are re-checked after every candidate",
            Block::P3 => "none documented; page 3's documented registers are read-only",
            Block::P4 => "the bit scan's: 0x08 clears AD_EN, 0x10 bit 1 resets the demodulator, 0x20 bit 4 and 0xA7 bits 5-6 wedge the chip (power cycle)",
        }
    }
}

/// The command line.
pub struct Opts {
    pub block: Block,
    pub limit: Option<usize>,
    pub out: Option<std::path::PathBuf>,
    pub from: Option<(u16, u8)>,
}

const USAGE: &str = "usage: harness blockscan --block sys|usb|p3|p4 [--limit N] [--out PATH] [--from OFFSET [VALUE]]";

impl Opts {
    pub fn parse(args: &[String]) -> Result<Opts, String> {
        let num = |s: Option<&String>| -> Result<u32, String> {
            let s = s.ok_or(USAGE)?;
            match s.strip_prefix("0x") {
                Some(h) => u32::from_str_radix(h, 16),
                None => s.parse(),
            }
            .map_err(|e| format!("{s}: {e}"))
        };
        let mut block = None;
        let mut o = Opts { block: Block::Sys, limit: None, out: None, from: None };
        let mut from = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--block" => {
                    let s = args.get(i + 1).ok_or(USAGE)?;
                    block = Some(Block::parse(s).ok_or_else(|| format!("--block {s}: one of sys, usb, p3, p4"))?);
                    i += 1;
                }
                "--limit" => {
                    o.limit = Some(num(args.get(i + 1))? as usize);
                    i += 1;
                }
                "--out" => {
                    o.out = Some(args.get(i + 1).ok_or(USAGE)?.into());
                    i += 1;
                }
                "--from" => {
                    let off = num(args.get(i + 1))?;
                    i += 1;
                    let mut v = 0;
                    if let Some(a) = args.get(i + 1).filter(|a| !a.starts_with("--")) {
                        v = num(Some(a))?;
                        i += 1;
                    }
                    from = Some((off, v));
                }
                _ => return Err(USAGE.into()),
            }
            i += 1;
        }
        o.block = block.ok_or(USAGE)?;
        if let Some((off, v)) = from {
            if off >= o.block.offsets() as u32 || v > 255 {
                return Err(format!("--from {off} {v}: offset 0..=0x{:X}, value 0..=255 for block {}", o.block.offsets() - 1, o.block.name()));
            }
            o.from = Some((off as u16, v as u8));
        }
        Ok(o)
    }

    fn path(&self) -> std::path::PathBuf {
        self.out.clone().unwrap_or_else(|| crate::data_dir().join(self.block.record()))
    }

    fn before_start(&self, off: u16, value: u8) -> bool {
        self.from.is_some_and(|f| (off, value) < f)
    }
}

/// (offset, value) of every candidate or skip line ("k":"c" / "s").
fn done(path: &std::path::Path) -> HashSet<(u16, u8)> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if !line.starts_with("{\"k\":\"c\"") && !line.starts_with("{\"k\":\"s\"") {
                continue;
            }
            if let (Some(o), Some(v)) = (field(line, "o"), field(line, "v")) {
                set.insert((o as u16, v as u8));
            }
        }
    }
    set
}

/// The baseline's USB-block settings still in place (Tables 30-33; the
/// items `step3::apply_baseline` writes): the names of any disturbed.
fn usb_baseline_disturbed(rtl: &Rtl) -> Vec<String> {
    let (_, ep_addr, mps) = rtl.bulk_in().unwrap_or((0, 0, 0));
    let cfg_mask = EPA_CFG_ENABLE | EPA_CFG_TYPE_MASK | EPA_CFG_NUM_MASK | EPA_CFG_ISO_MASK;
    let cfg_want = EPA_CFG_ENABLE | EPA_CFG_TYPE_BULK << EPA_CFG_TYPE_SHIFT | (ep_addr as u32 & EPA_CFG_NUM_MASK);
    let checks: [(&str, Option<bool>); 4] = [
        ("USB_SYSCTL DMA", rtl.usb_read32(USB_SYSCTL).ok().map(|v| v & USB_SYSCTL_DMA_EN != 0)),
        ("USB_EPA_CFG", rtl.usb_read32(USB_EPA_CFG).ok().map(|v| v & cfg_mask == cfg_want)),
        ("USB_EPA_CTL", rtl.usb_read32(USB_EPA_CTL).ok().map(|v| v & (EPA_CTL_STALL | EPA_CTL_FIFO_RESET) == 0)),
        ("USB_EPA_MAXPKT", rtl.usb_read32(USB_EPA_MAXPKT).ok().map(|v| v & EPA_MAXPKT_MASK == mps as u32 & EPA_MAXPKT_MASK)),
    ];
    checks.into_iter().filter(|(_, ok)| *ok != Some(true)).map(|(n, _)| n.to_string()).collect()
}

pub fn run(args: &[String]) -> Result<(), String> {
    let opts = Opts::parse(args)?;
    let block = opts.block;
    let limit = opts.limit;
    let path = opts.path();
    usb::guard()?;
    let done = done(&path);
    let total = block.offsets() as usize * 256;
    let fresh_file = std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    let w = |out: &mut std::fs::File, j: J| writeln!(out, "{}", j.render()).map_err(|e| e.to_string());
    if fresh_file {
        let hdr = crate::header(&format!("3-blockscan-{}", block.name()), "bulk IN bytes after writing one byte value at one address of a register space the byte scans had not covered, on top of the baseline")
            .with("k", "header")
            .with("block", block.name())
            .with("candidate_space", block.space())
            .with("bound", block.bound())
            .with(
                "byte_addressing",
                "USB and system registers are 32-bit little-endian words (step 1A, data/step1-sys-usb-encoding.json; step 1D, data/step1-writes.json); byte reads inside them work (step 1A); each candidate is one byte written with wLength 1 at one address, and whether the chip takes a one-byte write into a 32-bit register is what the read-back (in / rb) records; demodulator pages are byte registers",
            )
            .with("already_known", block.known())
            .with("hazards", block.hazards())
            .with(
                "method",
                "plain, as data/step3-bytescan.jsonl: the value is written into the running chip with no restart between candidates (METHODS.md section 5: every declared space covered once before any is repeated with a variant)",
            )
            .with(
                "settings_held_fixed",
                format!(
                    "documented-only baseline at {RATE_HZ} Hz; {} x {} byte bulk transfers queued; window {} ms right after the read-back, then a {} ms follow-up window if any byte arrived, then a {} ms window after the restore",
                    BulkCounter::QUEUED,
                    step3::BUF_LEN,
                    WINDOW.as_millis(),
                    FOLLOW_UP.as_millis(),
                    AFTER_RESTORE.as_millis()
                ),
            )
            .with(
                "window_justification",
                "the plain byte scan's (data/step3-bytescan.jsonl header): 4.8 MB/s fills a 16 KiB transfer every 3.4 ms, so 50 ms expects ~14.6 transfers (one-transfer quantisation 6.8 % < 20 % tolerance); any byte triggers the 200 ms follow-up, so a stream that starts up to ~50 ms after the write is still judged",
            )
            .with("streams_criterion", format!("within {}% of 2 x {RATE_HZ} bytes/s in the window or the follow-up", step3::TOLERANCE * 100.0))
            .with(
                "line_legend",
                "k: c candidate / s skip (the address's current value) / setup / recovery; o offset in the block (decimal); v value; orig the byte read before the write (null = the read failed); in the read-back after the write equals v; rb the read-back when it differs (absent when in); d bytes completed between candidates, discarded before the write (absent when 0); b bytes from the write to the end of the last window (window + follow-up); bps the follow-up window's bytes/s (absent without bytes); s streams; q the bulk detector's self-check from the previous candidate's last window to the end of the window, qf the same for the follow-up, qa for the after-restore window: [transfers submitted, completed, completed with an error, halts (Stall completions, each cleared by ClearFeature ENDPOINT_HALT), fewest transfers in flight, endpoint halted at any point 0/1]; e errors and events (absent when none); w wedged -> power cycle and re-baseline (next line k=recovery)",
            )
            .with(
                "procedure",
                "read the byte (skip if equal); discard completed transfers; write the value; read it back; window; on bytes the follow-up; restore the original and verify by read-back; on bytes the after-restore window; chip answering?; baseline re-checked (DEMOD_CTL, AD_EN, en_bbin, rsamp_ratio, USB_SYSCTL DMA, USB_EPA_CFG, USB_EPA_CTL, USB_EPA_MAXPKT): reapplied if disturbed, power cycle if it does not hold; a chip that stops answering or keeps streaming after the restore is power-cycled and re-baselined; the scan does not stop on a hit",
            );
        w(&mut out, hdr)?;
    }
    println!("blockscan {}: {} of {} candidates already logged in {}; limit {:?}", block.name(), done.len(), total, path.display(), limit);

    let (mut rtl, mut bulk, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
    w(&mut out, setup.with("k", "setup").with("detector", BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS)))?;
    let start = Instant::now();
    let (mut tested, mut skipped, mut hits, mut wedges) = (0usize, 0usize, 0usize, 0usize);
    'scan: for off in 0..block.offsets() {
        for value in 0..=255u8 {
            if done.contains(&(off, value)) || opts.before_start(off, value) {
                continue;
            }
            if limit.is_some_and(|n| tested >= n) {
                break 'scan;
            }
            let mut line = J::obj().with("k", "c").with("o", off).with("v", value);
            let mut e = Vec::new();
            let mut recover = false;
            let orig = match block.read(&rtl, off) {
                Ok(o) if o == value => {
                    w(&mut out, J::obj().with("k", "s").with("o", off).with("v", value))?;
                    skipped += 1;
                    continue;
                }
                Ok(o) => Some(o),
                Err(x) => {
                    e.push(format!("read: {x}"));
                    None
                }
            };
            tested += 1;
            line.set("orig", orig);
            if let Some(o) = orig {
                let stale = bulk.drain();
                if stale > 0 {
                    line.set("d", stale);
                }
                if let Err(x) = block.write(&rtl, off, value) {
                    e.push(format!("write: {x}"));
                }
                let back = block.read(&rtl, off);
                let inplace = back.as_ref().ok() == Some(&value);
                line.set("in", inplace);
                match &back {
                    Ok(b) if !inplace => {
                        line.set("rb", *b);
                    }
                    Err(x) => e.push(format!("read back: {x}")),
                    _ => {}
                }
                let win = bulk.measure(WINDOW, None);
                bulk_events(&win.check, "", &rtl, &mut e);
                let mut bytes = win.bytes + win.stale_bytes;
                let mut streams = step3::streams(win.rate(), RATE_HZ);
                line.set("q", win.check.compact());
                let any = bytes > 0;
                if any {
                    let f = bulk.measure(FOLLOW_UP, None);
                    bytes += f.bytes + f.stale_bytes;
                    streams |= step3::streams(f.rate(), RATE_HZ);
                    line.set("bps", f.rate().round());
                    line.set("qf", f.check.compact());
                    bulk_events(&f.check, "follow-up window ", &rtl, &mut e);
                }
                line.set("b", bytes);
                line.set("s", streams);
                match block.write(&rtl, off, o).and_then(|_| block.read(&rtl, off)) {
                    Ok(b) if b == o => {}
                    Ok(b) => e.push(format!("restore read back {b}")),
                    Err(x) => e.push(format!("restore: {x}")),
                }
                if any {
                    let a = bulk.measure(AFTER_RESTORE, None);
                    line.set("qa", a.check.compact());
                    bulk_events(&a.check, "after-restore window ", &rtl, &mut e);
                    if step3::streams(a.rate(), RATE_HZ) {
                        e.push("stream persisted after restore".into());
                        recover = true;
                    }
                }
                if streams {
                    hits += 1;
                    println!("STREAMS: block {} offset 0x{off:02X} value 0x{value:02X} ({bytes} bytes)", block.name());
                }
            }
            if !recover && !step3::alive(&rtl) {
                recover = true;
            }
            if !recover {
                let mut d = step3::baseline_disturbed(&rtl, RATE_HZ);
                d.extend(usb_baseline_disturbed(&rtl));
                if !d.is_empty() {
                    e.push(format!("baseline disturbed: {}", d.join(", ")));
                    let _ = step3::apply_baseline(&rtl, RATE_HZ, &[]);
                    let mut still = step3::baseline_disturbed(&rtl, RATE_HZ);
                    still.extend(usb_baseline_disturbed(&rtl));
                    if still.is_empty() {
                        e.push("baseline reapplied".into());
                    } else {
                        e.push(format!("baseline still disturbed: {}", still.join(", ")));
                        recover = true;
                    }
                }
            }
            if !e.is_empty() {
                line.set("e", J::Arr(e.into_iter().map(J::from).collect()));
            }
            if recover {
                wedges += 1;
                line.set("w", true);
                w(&mut out, line)?;
                println!("wedged at offset 0x{off:02X} value 0x{value:02X}: power cycle");
                drop(bulk);
                drop(rtl);
                let (r, b, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
                rtl = r;
                bulk = b;
                w(&mut out, setup.with("k", "recovery").with("detector", BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS)).with("after", J::obj().with("o", off).with("v", value)))?;
                continue;
            }
            w(&mut out, line)?;
        }
    }
    bulk.close();
    let secs = start.elapsed().as_secs_f64();
    let remaining = total.saturating_sub(done.len() + tested + skipped);
    let per_s = tested as f64 / secs;
    println!(
        "blockscan {}: tested {tested}, skipped {skipped}, streaming hits {hits}, wedges {wedges}, in {secs:.1} s ({per_s:.2} tested/s); {remaining} candidates left (~{:.2} h at this rate)",
        block.name(),
        remaining as f64 / per_s.max(1e-9) / 3600.0
    );
    Ok(())
}
