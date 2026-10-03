//! Step 3, byte scan: every byte value of demodulator pages 0, 1 and 2,
//! one register at a time, on top of the documented-only baseline. The
//! single-bit scan (data/step3-scan.jsonl) found no bit that streams;
//! this covers every value of every register instead.
//!
//! Candidate space: page 0, 1, 2 x offset 0x00..=0xFF x value 0x00..=0xFF,
//! minus the value the register holds when its candidate comes up (that
//! one is logged as a skip). Iterated page, offset, value, ascending.
//! Page 3 is excluded (its bytes are live readbacks: 246 of the bit
//! scan's 247 unverified restores were there), and page 4 too (its writes
//! at 0x08, 0x10, 0x20 and 0xA7 acted like page 0's in the bit scan).
//!
//! Detector: a 50 ms window right after the write. The bit scan's 100 ms
//! windows read exactly 0 bytes (and 0 stale bytes) for 10238 of 10240
//! candidates, so any byte is a rare event. A 4.8 MB/s stream (2 bytes x
//! 2.4 Msps) fills a 16 KiB transfer every 3.4 ms: 50 ms expects 240000
//! bytes, ~14.6 transfers, so a one-transfer quantisation error is 6.8 %,
//! inside the 20 % tolerance. Any byte (counted or stale) in that window
//! triggers a 200 ms follow-up window, and the candidate streams if either
//! window is within 20 % of 2 x rate; a stream that comes up as late as
//! ~50 ms after the write is still caught by the follow-up.
//!
//! Resumable: candidates already in the file are skipped. `--limit N`
//! stops after N new tested candidates. `--out PATH` writes (and resumes)
//! PATH instead of the record (a scratch run); `--from P OFF [VAL]` skips
//! every candidate before page P, offset OFF (value VAL).

use std::collections::HashSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use crate::json::{J, hex8};
use crate::step3::{self, RATE_HZ};
use crate::usb::{self, BulkCounter};

pub const PAGES: [u8; 3] = [0, 1, 2];
pub const WINDOW: Duration = Duration::from_millis(50);
pub const FOLLOW_UP: Duration = Duration::from_millis(200);

/// The command line of either byte scan.
pub struct Opts {
    pub restart: bool,
    pub limit: Option<usize>,
    /// A scratch file written instead of the record.
    pub out: Option<std::path::PathBuf>,
    /// The first (page, offset, value) tested; earlier candidates are not run.
    pub from: Option<(u8, u8, u8)>,
}

impl Opts {
    pub fn parse(args: &[String]) -> Result<Opts, String> {
        const USAGE: &str = "usage: harness bytescan [--restart] [--limit N] [--out PATH] [--from PAGE OFFSET [VALUE]]";
        let num = |s: Option<&String>| -> Result<u32, String> {
            let s = s.ok_or(USAGE)?;
            match s.strip_prefix("0x") {
                Some(h) => u32::from_str_radix(h, 16),
                None => s.parse(),
            }
            .map_err(|e| format!("{s}: {e}"))
        };
        let mut o = Opts { restart: false, limit: None, out: None, from: None };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--restart" => o.restart = true,
                "--limit" => {
                    o.limit = Some(num(args.get(i + 1))? as usize);
                    i += 1;
                }
                "--out" => {
                    o.out = Some(args.get(i + 1).ok_or(USAGE)?.into());
                    i += 1;
                }
                "--from" => {
                    let p = num(args.get(i + 1))?;
                    let off = num(args.get(i + 2))?;
                    i += 2;
                    let mut v = 0;
                    if let Some(a) = args.get(i + 1).filter(|a| !a.starts_with("--")) {
                        v = num(Some(a))?;
                        i += 1;
                    }
                    if p > 255 || !PAGES.contains(&(p as u8)) || off > 255 || v > 255 {
                        return Err(format!("--from {p} {off} {v}: page must be one of {PAGES:?}, offset and value 0..=255"));
                    }
                    o.from = Some((p as u8, off as u8, v as u8));
                }
                _ => return Err(USAGE.into()),
            }
            i += 1;
        }
        Ok(o)
    }

    /// The scratch file if given, else the record `name` under data/.
    pub fn path(&self, name: &str) -> std::path::PathBuf {
        self.out.clone().unwrap_or_else(|| crate::data_dir().join(name))
    }

    /// True for a candidate before `--from`.
    pub fn before_start(&self, page: u8, off: u8, value: u8) -> bool {
        self.from.is_some_and(|f| (page, off, value) < f)
    }
}

const RECORD: &str = "step3-bytescan.jsonl";

/// What the line's self-check fields mean, for the setup line.
const CHECK_FIELDS: &str = "bulk_check = [submitted, completed, errored, halts, fewest in flight, halted at any point 0/1] from the previous window's end to this window's end; the follow-up and after-restore windows carry the full check object";

/// An integer field `"k":N` of a JSONL line.
pub fn field(line: &str, k: &str) -> Option<u32> {
    let key = format!("\"{k}\":");
    let i = line.find(&key)? + key.len();
    line[i..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// (page, offset, value) of every candidate line (tested or skipped).
fn done(path: &std::path::Path) -> HashSet<(u8, u8, u8)> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if !line.contains("\"kind\":\"candidate\"") && !line.contains("\"kind\":\"skip\"") {
                continue;
            }
            if let (Some(p), Some(o), Some(v)) = (field(line, "page"), field(line, "offset_dec"), field(line, "value_dec")) {
                set.insert((p as u8, o as u8, v as u8));
            }
        }
    }
    set
}

pub fn run(args: &[String]) -> Result<(), String> {
    let opts = Opts::parse(args)?;
    if opts.restart {
        return crate::restart::run(&opts);
    }
    let limit = opts.limit;
    let path = opts.path(RECORD);
    crate::usb::guard()?;
    let done = done(&path);
    let total = PAGES.len() * 256 * 256;
    let fresh_file = std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    let w = |out: &mut std::fs::File, j: J| writeln!(out, "{}", j.render()).map_err(|e| e.to_string());
    if fresh_file {
        let hdr = crate::header("3-bytescan", "bulk IN bytes after writing one demodulator byte value on top of the baseline")
            .with("kind", "header")
            .with(
                "candidate_space",
                "page 0, 1, 2 x offset 0x00..=0xFF x value 0x00..=0xFF, except the value the register holds when its candidate comes up (logged as kind=skip); iterated page, offset, value in ascending order with no reordering",
            )
            .with(
                "excluded",
                "page 3: live readback values (246 of the bit scan's 247 unverified restores); page 4: its writes at 0x08, 0x10, 0x20, 0xA7 acted like page 0's in the bit scan (data/step3-scan.jsonl)",
            )
            .with(
                "settings_held_fixed",
                format!(
                    "documented-only baseline at {RATE_HZ} Hz; {} x {} byte bulk transfers queued; window {} ms right after the write, then a {} ms follow-up window if any byte arrived",
                    BulkCounter::QUEUED,
                    step3::BUF_LEN,
                    WINDOW.as_millis(),
                    FOLLOW_UP.as_millis()
                ),
            )
            .with(
                "window_justification",
                "bit scan: 10238 of 10240 100 ms windows read 0 bytes and 0 stale bytes; 4.8 MB/s fills a 16 KiB transfer every 3.4 ms, so 50 ms expects ~14.6 transfers (one-transfer quantisation 6.8 % < 20 % tolerance); any byte triggers the 200 ms follow-up",
            )
            .with("streams_criterion", format!("within {}% of 2 x {RATE_HZ} bytes/s in either window", step3::TOLERANCE * 100.0))
            .with(
                "procedure",
                "read the byte; write the value; measure; follow up if any byte; restore the original; verify by read-back; if any byte arrived, measure 50 ms after the restore; re-check the baseline (reapply, else power-cycle); a chip that stops answering or keeps streaming after the restore is power-cycled and re-baselined",
            );
        w(&mut out, hdr)?;
    }
    println!("bytescan: {} of {} candidates already logged in {}; limit {:?}", done.len(), total, path.display(), limit);

    let (mut rtl, mut bulk, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
    w(&mut out, setup.with("kind", "setup").with("detector", usb::BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS)))?;
    let start = Instant::now();
    let (mut tested, mut skipped, mut hits) = (0usize, 0usize, 0usize);
    'scan: for page in PAGES {
        for off in 0..=255u8 {
            for value in 0..=255u8 {
                if done.contains(&(page, off, value)) || opts.before_start(page, off, value) {
                    continue;
                }
                if limit.is_some_and(|n| tested >= n) {
                    break 'scan;
                }
                let mut line = J::obj();
                let orig = rtl.demod_read8(page, off);
                let mut recover = false;
                match orig {
                    Err(e) => {
                        line = line.with("kind", "candidate");
                        line.set("page", page);
                        line.set("offset_dec", off);
                        line.set("offset", hex8(off));
                        line.set("value_dec", value);
                        line.set("value", hex8(value));
                        line.set("read_error", e.to_string());
                        tested += 1;
                        recover = !step3::alive(&rtl);
                    }
                    Ok(o) if o == value => {
                        let l = J::obj()
                            .with("kind", "skip")
                            .with("page", page)
                            .with("offset_dec", off)
                            .with("offset", hex8(off))
                            .with("value_dec", value)
                            .with("value", hex8(value))
                            .with("reason", "the register's current value");
                        w(&mut out, l)?;
                        skipped += 1;
                        continue;
                    }
                    Ok(o) => {
                        tested += 1;
                        line = line.with("kind", "candidate");
                        line.set("page", page);
                        line.set("offset_dec", off);
                        line.set("offset", hex8(off));
                        line.set("value_dec", value);
                        line.set("value", hex8(value));
                        line.set("original", hex8(o));
                        let wr = rtl.demod_write8(page, off, value);
                        line.set("write_error", wr.err().map(|e| e.to_string()));
                        let win = bulk.measure(WINDOW, None);
                        line.set("bytes", win.bytes);
                        line.set("bytes_per_s", win.rate());
                        line.set("stale_bytes_discarded", win.stale_bytes);
                        if !win.errors.is_empty() {
                            line.set("transfer_errors", J::Arr(win.errors.iter().map(|e| J::from(e.as_str())).collect()));
                        }
                        line.set("bulk_check", win.check.compact());
                        if !win.check.clears.is_empty() {
                            line.set("bulk_halt_clears", J::Arr(win.check.clears.iter().map(|c| J::from(c.as_str())).collect()));
                        }
                        let mut streams = step3::streams(win.rate(), RATE_HZ);
                        let any = win.bytes > 0 || win.stale_bytes > 0;
                        if any {
                            let f = bulk.measure(FOLLOW_UP, None);
                            streams |= step3::streams(f.rate(), RATE_HZ);
                            line.set("follow_up", step3::window_record(&f, RATE_HZ));
                        }
                        line.set("streams", streams);
                        let rs = rtl.demod_write8(page, off, o);
                        let back = rtl.demod_read8(page, off);
                        line.set("restore_error", rs.err().map(|e| e.to_string()));
                        line.set("restore_readback", back.as_ref().ok().map(|b| hex8(*b)));
                        line.set("restore_verified", back.as_ref().ok() == Some(&o));
                        if any {
                            let after = bulk.measure(WINDOW, None);
                            line.set("after_restore_bytes", after.bytes + after.stale_bytes);
                            line.set("after_restore_bulk_check", after.check.record());
                            if step3::streams(after.rate(), RATE_HZ) {
                                line.set("stream_persisted_after_restore", true);
                                recover = true;
                            }
                        }
                        if streams {
                            hits += 1;
                            println!("STREAMS: page {page} offset 0x{off:02X} value 0x{value:02X} ({} bytes in {} ms)", win.bytes, WINDOW.as_millis());
                        }
                        if !recover && !step3::alive(&rtl) {
                            recover = true;
                        }
                        if !recover {
                            let disturbed = step3::baseline_disturbed(&rtl, RATE_HZ);
                            if !disturbed.is_empty() {
                                line.set("baseline_disturbed", J::Arr(disturbed.into_iter().map(J::from).collect()));
                                line.set("baseline_reapplied", step3::apply_baseline(&rtl, RATE_HZ, &[]));
                                let still = step3::baseline_disturbed(&rtl, RATE_HZ);
                                recover = !still.is_empty();
                                line.set("baseline_still_disturbed", J::Arr(still.into_iter().map(J::from).collect()));
                            }
                        }
                    }
                }
                if recover {
                    line.set("wedged", true);
                    w(&mut out, line)?;
                    drop(bulk);
                    drop(rtl);
                    let (r, b, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
                    let after = J::obj().with("page", page).with("offset", hex8(off)).with("value", hex8(value));
                    w(&mut out, setup.with("kind", "recovery").with("detector", usb::BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS)).with("after_candidate", after))?;
                    rtl = r;
                    bulk = b;
                    continue;
                }
                w(&mut out, line)?;
            }
        }
    }
    bulk.close();
    let secs = start.elapsed().as_secs_f64();
    let remaining = total.saturating_sub(done.len() + tested + skipped);
    let per_s = tested as f64 / secs;
    println!(
        "bytescan: tested {tested}, skipped {skipped}, streaming hits {hits}, in {secs:.1} s ({per_s:.2} tested/s); {remaining} candidates left (~{:.2} h at this rate)",
        remaining as f64 / per_s.max(1e-9) / 3600.0
    );
    Ok(())
}
