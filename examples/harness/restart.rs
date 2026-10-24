//! Step 3, byte scan with a documented restart. The plain byte scan
//! (data/step3-bytescan.jsonl) wrote each value into a running chip and
//! found no stream; here every candidate is followed by the two restarts
//! the documents provide: the demodulator hardware reset (DEMOD_CTL bit 5,
//! 0 = held in reset, 1 = released; [RTL2832U] Table 15) and the endpoint
//! FIFO reset (USB_EPA_CTL bit 9; Table 32).
//!
//! `harness restart-probe` measures, once, what each restart does to the
//! demodulator's registers (data/step3-restart-probe.json); the order the
//! scan uses is fixed from that record ([`ORDER`]).

use std::collections::HashSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use crate::json::{J, hex8, hex32};
use crate::regs::*;
use crate::step3::{self, RATE_HZ};
use crate::usb::{self, BulkCounter, Rtl};

/// The registers the probe watches: the baseline's demodulator settings,
/// the repeater byte, and a test register.
const WATCH: &[(&str, u8, u8)] = &[
    ("AD_EN p0 0x08", 0, 0x08),
    ("opt_adc_iq p0 0x06", 0, 0x06),
    ("IIC_repeat p1 0x01", 1, 0x01),
    ("rf_agc_max p1 0x0B (test register)", 1, 0x0B),
    ("Spec_inv,En_aci p1 0x15", 1, 0x15),
    ("pset_iffreq p1 0x19", 1, 0x19),
    ("pset_iffreq p1 0x1A", 1, 0x1A),
    ("pset_iffreq p1 0x1B", 1, 0x1B),
    ("rsamp p1 0x9F", 1, 0x9F),
    ("rsamp p1 0xA0", 1, 0xA0),
    ("rsamp p1 0xA1", 1, 0xA1),
    ("rsamp p1 0xA2", 1, 0xA2),
    ("en_bbin p1 0xB1", 1, 0xB1),
];
/// rf_agc_max (page 1, 0x0B; Table 2, RW, reset 0x7F), shown writable in
/// step 1D (data/step1-writes.json). The probe writes TEST_VALUE.
const TEST_PAGE: u8 = 1;
const TEST_OFF: u8 = 0x0B;
const TEST_VALUE: u8 = 0x7E;
const TEST_VALUE_IN_RESET: u8 = 0x7D;

fn snapshot(rtl: &Rtl) -> Vec<Option<u8>> {
    WATCH
        .iter()
        .map(|&(_, p, o)| rtl.demod_read8(p, o).ok())
        .collect()
}

fn snap_json(rtl: &Rtl, v: &[Option<u8>]) -> J {
    let mut o = J::obj();
    for ((name, _, _), b) in WATCH.iter().zip(v) {
        o.set(name, b.map(hex8));
    }
    o.with("DEMOD_CTL", rtl.sys_read8(DEMOD_CTL).ok().map(hex8))
        .with("USB_EPA_CTL", rtl.usb_read32(USB_EPA_CTL).ok().map(hex32))
}

/// Names of the watched registers whose value changed between two snapshots.
fn changed(a: &[Option<u8>], b: &[Option<u8>]) -> J {
    J::Arr(
        WATCH
            .iter()
            .zip(a.iter().zip(b))
            .filter(|(_, (x, y))| x != y)
            .map(|((n, _, _), _)| J::from(*n))
            .collect(),
    )
}

fn res<T>(r: usb::R<T>) -> J {
    match r {
        Ok(_) => J::from("ok"),
        Err(e) => J::from(format!("error: {e}")),
    }
}

/// DEMOD_CTL bit 5 to 0 (held in reset) or 1 (released), read-modify-write.
fn demod_reset(rtl: &Rtl, hold: bool) -> usb::R<u8> {
    let c = rtl.sys_read8(DEMOD_CTL)?;
    let n = if hold {
        c & !DEMOD_CTL_RESET_RELEASE
    } else {
        c | DEMOD_CTL_RESET_RELEASE
    };
    rtl.sys_write8(DEMOD_CTL, n)?;
    rtl.sys_read8(DEMOD_CTL)
}

/// USB_EPA_CTL bit 9 set then cleared; returns the two read-backs.
fn fifo_reset(rtl: &Rtl) -> usb::R<(u32, u32)> {
    let c = rtl.usb_read32(USB_EPA_CTL)?;
    rtl.usb_write32(USB_EPA_CTL, c | EPA_CTL_FIFO_RESET)?;
    let a = rtl.usb_read32(USB_EPA_CTL)?;
    rtl.usb_write32(USB_EPA_CTL, a & !EPA_CTL_FIFO_RESET)?;
    let b = rtl.usb_read32(USB_EPA_CTL)?;
    Ok((a, b))
}

pub fn probe() -> Result<(), String> {
    usb::guard()?;
    let mut rec = crate::header("3-restart-probe", "what the documented restarts do to the demodulator's registers")
        .with("restarts", "demodulator reset: DEMOD_CTL bit 5 to 0 then 1 (Table 15); FIFO reset: USB_EPA_CTL bit 9 set then cleared (Table 32)")
        .with("test_register", format!("page {TEST_PAGE} 0x{TEST_OFF:02X} rf_agc_max (Table 2, RW, reset 0x7F) written 0x{TEST_VALUE:02X}"));
    let (rtl, bulk, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
    bulk.close();
    rec.set("setup", setup);

    // A: candidate written into the running chip, then the reset pulse.
    let w = res(rtl.demod_write8(TEST_PAGE, TEST_OFF, TEST_VALUE));
    let before = snapshot(&rtl);
    let held = demod_reset(&rtl, true);
    let during = snapshot(&rtl);
    let released = demod_reset(&rtl, false);
    std::thread::sleep(Duration::from_millis(10));
    let after = snapshot(&rtl);
    rec.set(
        "a_write_then_reset_pulse",
        J::obj()
            .with("test_write", w)
            .with("before", snap_json(&rtl, &before))
            .with("demod_ctl_held", held.as_ref().ok().map(|v| hex8(*v)))
            .with(
                "reads_while_held",
                J::Arr(during.iter().map(|b| J::from(b.map(hex8))).collect()),
            )
            .with(
                "demod_ctl_released",
                released.as_ref().ok().map(|v| hex8(*v)),
            )
            .with("after_release", snap_json(&rtl, &after))
            .with("changed_by_pulse", changed(&before, &after)),
    );
    println!("A: pulse changed {}", changed(&before, &after).render());

    // B: writes while held in reset, then release.
    let _ = step3::apply_baseline(&rtl, RATE_HZ, &[]);
    let _ = rtl.demod_write8(TEST_PAGE, TEST_OFF, TEST_VALUE);
    let before = snapshot(&rtl);
    let held = demod_reset(&rtl, true);
    let w_held = res(rtl.demod_write8(TEST_PAGE, TEST_OFF, TEST_VALUE_IN_RESET));
    let r_held = rtl.demod_read8(TEST_PAGE, TEST_OFF);
    let released = demod_reset(&rtl, false);
    std::thread::sleep(Duration::from_millis(10));
    let after = snapshot(&rtl);
    rec.set(
        "b_write_while_held",
        J::obj()
            .with("before", snap_json(&rtl, &before))
            .with("demod_ctl_held", held.as_ref().ok().map(|v| hex8(*v)))
            .with("write_0x7D_while_held", w_held)
            .with(
                "read_while_held",
                match &r_held {
                    Ok(v) => hex8(*v),
                    Err(e) => J::from(format!("error: {e}")),
                },
            )
            .with(
                "demod_ctl_released",
                released.as_ref().ok().map(|v| hex8(*v)),
            )
            .with("after_release", snap_json(&rtl, &after))
            .with("changed", changed(&before, &after)),
    );
    println!(
        "B: write while held {:?}, after release test reg {:?}",
        r_held.as_ref().ok(),
        after[3]
    );

    // C: FIFO reset pulse alone.
    let _ = step3::apply_baseline(&rtl, RATE_HZ, &[]);
    let _ = rtl.demod_write8(TEST_PAGE, TEST_OFF, TEST_VALUE);
    let before = snapshot(&rtl);
    let f = fifo_reset(&rtl);
    let after = snapshot(&rtl);
    rec.set(
        "c_fifo_reset_pulse",
        J::obj()
            .with("before", snap_json(&rtl, &before))
            .with(
                "epa_ctl_set_then_cleared",
                f.as_ref()
                    .ok()
                    .map(|(a, b)| J::Arr(vec![hex32(*a), hex32(*b)])),
            )
            .with("error", f.err().map(|e| e.to_string()))
            .with("after", snap_json(&rtl, &after))
            .with("changed", changed(&before, &after)),
    );
    println!(
        "C: FIFO reset changed {}",
        changed(&before, &after).render()
    );
    rec.set("chip_answers_at_end", step3::alive(&rtl));
    crate::write_record("step3-restart-probe.json", &rec);
    Ok(())
}

// ---- The byte scan with restart ----

/// The restart order, fixed from data/step3-restart-probe.json:
/// - the demodulator reset pulse returns every watched demodulator
///   register to its power-on value (the test write 0x7E -> 0x7F, AD_EN
///   0xCD -> 0x0D, pset_iffreq 0 -> 0x35D75D, rsamp 0x3000000 ->
///   0xC99999), so a candidate written before it is lost;
/// - while held in reset, demodulator reads and writes stall, so nothing
///   can be written during the reset;
/// - the FIFO reset changes no demodulator register.
///
/// Hence, per candidate: demodulator reset pulse, then the baseline's
/// demodulator bytes written again, then the candidate, then the FIFO
/// reset pulse, then the candidate verified by read-back, then measure.
/// Every candidate therefore runs from the same state: power-on values,
/// the baseline, and the candidate alone.
pub const ORDER: &str = "DEMOD_CTL bit 5 0 then 1; baseline demodulator bytes rewritten; candidate written; USB_EPA_CTL bit 9 1 then 0; candidate read back; measure";

/// The baseline's demodulator bytes (Table 4 AD_EN, en_bbin; Table 6
/// pset_iffreq; Table 7 rsamp_ratio), captured after the baseline is
/// applied and verified, and written again after each demodulator reset.
const BASE_REGS: [(u8, u8); 9] = [
    (AD_EN_PAGE, AD_EN_OFF),
    (BBIN_PAGE, BBIN_OFF),
    (IFFREQ_PAGE, IFFREQ_MSB),
    (IFFREQ_PAGE, IFFREQ_MSB + 1),
    (IFFREQ_PAGE, IFFREQ_MSB + 2),
    (RSAMP_PAGE, RSAMP_MSB),
    (RSAMP_PAGE, RSAMP_MSB + 1),
    (RSAMP_PAGE, RSAMP_MSB + 2),
    (RSAMP_PAGE, RSAMP_MSB + 3),
];

/// First-byte deadline after the restart, and the judging window.
pub const FIRST_BYTE: Duration = Duration::from_millis(200);
pub const JUDGE: Duration = Duration::from_millis(200);
const AFTER_RESTORE: Duration = Duration::from_millis(50);

/// What the line's self-check fields mean, for the setup line.
const CHECK_FIELDS: &str = "Line fields q / qj = [submitted, completed, errored, halts, fewest in flight, halted at any point 0/1]";

const RECORD: &str = "step3-bytescan-restart.jsonl";

/// (page, offset, value) of every candidate or skip line ("k":"c" / "s").
fn done(path: &std::path::Path) -> HashSet<(u8, u8, u8)> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if !line.starts_with("{\"k\":\"c\"") && !line.starts_with("{\"k\":\"s\"") {
                continue;
            }
            let f = |k| crate::bytescan::field(line, k);
            if let (Some(p), Some(o), Some(v)) = (f("p"), f("o"), f("v")) {
                set.insert((p as u8, o as u8, v as u8));
            }
        }
    }
    set
}

fn capture_base(rtl: &Rtl) -> Result<Vec<(u8, u8, u8)>, String> {
    BASE_REGS
        .iter()
        .map(|&(p, o)| {
            rtl.demod_read8(p, o)
                .map(|v| (p, o, v))
                .map_err(|e| format!("baseline read p{p} 0x{o:02X}: {e}"))
        })
        .collect()
}

/// A window's bulk errors, halts and halt clears as events of the line;
/// on a halt also USB_EPA_CTL (bit 4 is the chip's stall bit, Table 32).
pub fn bulk_events(c: &usb::Check, which: &str, rtl: &Rtl, e: &mut Vec<String>) {
    for k in c.error_list() {
        e.push(format!("{which}bulk: {k}"));
    }
    for r in &c.clears {
        e.push(format!("{which}bulk halt cleared: {r}"));
    }
    if c.halted && c.clears.iter().all(|r| !r.starts_with("ok")) {
        e.push(format!("{which}bulk: endpoint halted and not cleared"));
    }
    if c.halts > 0 {
        e.push(match rtl.usb_read32(USB_EPA_CTL) {
            Ok(v) => format!("{which}USB_EPA_CTL after the clear 0x{v:08X}"),
            Err(x) => format!("{which}USB_EPA_CTL after the clear: {x}"),
        });
    }
    if c.in_flight_min == 0 && !c.disconnected {
        e.push(format!("{which}bulk: no transfer in flight at some point"));
    }
}

/// The restart sequence of [`ORDER`] up to and including the FIFO reset.
/// Ok(true) when every step was accepted and read back as intended.
fn restart(
    rtl: &Rtl,
    base: &[(u8, u8, u8)],
    page: u8,
    off: u8,
    value: u8,
    e: &mut Vec<String>,
) -> bool {
    let mut ok = true;
    match demod_reset(rtl, true) {
        Ok(c) if c & DEMOD_CTL_RESET_RELEASE == 0 => {}
        Ok(c) => {
            ok = false;
            e.push(format!("hold reset read back 0x{c:02X}"));
        }
        Err(x) => {
            ok = false;
            e.push(format!("hold reset: {x}"));
        }
    }
    match demod_reset(rtl, false) {
        Ok(c) if c & DEMOD_CTL_RESET_RELEASE != 0 => {}
        Ok(c) => {
            ok = false;
            e.push(format!("release reset read back 0x{c:02X}"));
        }
        Err(x) => {
            ok = false;
            e.push(format!("release reset: {x}"));
        }
    }
    for &(p, o, v) in base {
        if let Err(x) = rtl.demod_write8(p, o, v) {
            ok = false;
            e.push(format!("baseline write p{p} 0x{o:02X}: {x}"));
        }
    }
    if let Err(x) = rtl.demod_write8(page, off, value) {
        e.push(format!("candidate write: {x}"));
    }
    match fifo_reset(rtl) {
        Ok((a, b)) if a & EPA_CTL_FIFO_RESET != 0 && b & EPA_CTL_FIFO_RESET == 0 => {}
        Ok((a, b)) => {
            ok = false;
            e.push(format!("fifo reset read back 0x{a:08X} 0x{b:08X}"));
        }
        Err(x) => {
            ok = false;
            e.push(format!("fifo reset: {x}"));
        }
    }
    ok
}

pub fn run(opts: &crate::bytescan::Opts) -> Result<(), String> {
    use crate::bytescan::PAGES;
    let limit = opts.limit;
    let path = opts.path(RECORD);
    usb::guard()?;
    let done = done(&path);
    let total = PAGES.len() * 256 * 256;
    let fresh_file = std::fs::metadata(&path)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let w =
        |out: &mut std::fs::File, j: J| writeln!(out, "{}", j.render()).map_err(|e| e.to_string());
    if fresh_file {
        let hdr = crate::header("3-bytescan-restart", "bulk IN bytes after writing one demodulator byte value, then the documented demodulator and FIFO restarts")
            .with("kind", "header")
            .with(
                "candidate_space",
                "page 0, 1, 2 x offset 0x00..=0xFF x value 0x00..=0xFF, except the value the register holds (in the baseline state) when its candidate comes up, logged as a skip; iterated page, offset, value in ascending order with no reordering",
            )
            .with(
                "excluded",
                "page 3: live readback values (246 of the bit scan's 247 unverified restores, data/step3-scan.jsonl); page 4: its writes at 0x08, 0x10, 0x20, 0xA7 acted like page 0's in the bit scan",
            )
            .with("restart_order", ORDER)
            .with(
                "restart_order_evidence",
                "data/step3-restart-probe.json: the DEMOD_CTL bit 5 pulse returns every watched demodulator register to its power-on value; demodulator reads and writes stall while held in reset; the FIFO reset changes no demodulator register",
            )
            .with(
                "settings_held_fixed",
                format!(
                    "documented-only baseline at {RATE_HZ} Hz; {} x {} byte bulk transfers queued; first-byte deadline {} ms after the FIFO reset; on any byte a further {} ms window",
                    BulkCounter::QUEUED,
                    step3::BUF_LEN,
                    FIRST_BYTE.as_millis(),
                    JUDGE.as_millis()
                ),
            )
            .with("streams_criterion", format!("the further {} ms window within {}% of 2 x {RATE_HZ} bytes/s", JUDGE.as_millis(), step3::TOLERANCE * 100.0))
            .with(
                "line_legend",
                "k: c candidate / s skip (the register's current value) / setup / recovery; p page; o offset; v value; orig the register's value before the candidate (all decimal); rst the restart sequence accepted and read back as intended; in the candidate read back in place after the restart; b bytes (first completion + the further window); lat ms from the end of the FIFO reset to the first byte (null = none within the deadline); bps the further window's bytes/s; s streams; d bytes completed between candidates, discarded before the restart (absent when 0); q the bulk detector's self-check from the previous candidate's last window to the end of the first-byte wait, qj the same for the further window: [transfers submitted, completed, completed with an error, halts (Stall completions, each cleared by ClearFeature ENDPOINT_HALT), fewest transfers in flight, endpoint halted at any point 0/1] (lines written before this field existed lack it); e errors and events (absent when none); w wedged -> power cycle and re-baseline (next line k=recovery)",
            )
            .with(
                "procedure",
                "read the byte (skip if equal); discard completed transfers; restart sequence with the candidate; read the candidate back; first-byte wait; on bytes the further window; restore the original and verify; on bytes a 50 ms check after the restore; re-check the baseline (reapply, else power-cycle); a chip that stops answering or keeps streaming after the restore is power-cycled",
            );
        w(&mut out, hdr)?;
    }
    println!(
        "bytescan --restart: {} of {} candidates already logged in {}; limit {:?}",
        done.len(),
        total,
        path.display(),
        limit
    );

    let (mut rtl, mut bulk, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
    let mut base = capture_base(&rtl)?;
    w(
        &mut out,
        setup.with("k", "setup").with(
            "detector",
            usb::BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS),
        ),
    )?;
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
                let line = J::obj()
                    .with("k", "c")
                    .with("p", page)
                    .with("o", off)
                    .with("v", value);
                let mut e = Vec::new();
                let mut recover = false;
                let orig = match rtl.demod_read8(page, off) {
                    Ok(o) if o == value => {
                        w(
                            &mut out,
                            J::obj()
                                .with("k", "s")
                                .with("p", page)
                                .with("o", off)
                                .with("v", value),
                        )?;
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
                let mut line = line.with("orig", orig);
                if let Some(o) = orig {
                    let stale = bulk.drain();
                    let rst = restart(&rtl, &base, page, off, value, &mut e);
                    let back = rtl.demod_read8(page, off);
                    let inplace = back.as_ref().ok() == Some(&value);
                    let fb = bulk.first_byte(FIRST_BYTE);
                    let (lat, b0) = (fb.latency, fb.bytes);
                    bulk_events(&fb.check, "", &rtl, &mut e);
                    let mut bytes = b0;
                    let mut streams = false;
                    line.set("rst", rst);
                    line.set("in", inplace);
                    if stale > 0 {
                        line.set("d", stale);
                    }
                    line.set("q", fb.check.compact());
                    if !inplace {
                        e.push(match &back {
                            Ok(b) => format!("candidate read back {b}"),
                            Err(x) => format!("candidate read back: {x}"),
                        });
                    }
                    if b0 > 0 {
                        let f = bulk.measure(JUDGE, None);
                        bytes += f.bytes + f.stale_bytes;
                        streams = step3::streams(f.rate(), RATE_HZ);
                        line.set("bps", f.rate().round());
                        line.set("qj", f.check.compact());
                        bulk_events(&f.check, "judge window ", &rtl, &mut e);
                    }
                    line.set("b", bytes);
                    line.set("lat", lat.map(|d| (d.as_secs_f64() * 1e4).round() / 10.0));
                    line.set("s", streams);
                    match rtl
                        .demod_write8(page, off, o)
                        .and_then(|_| rtl.demod_read8(page, off))
                    {
                        Ok(b) if b == o => {}
                        Ok(b) => e.push(format!("restore read back {b}")),
                        Err(x) => e.push(format!("restore: {x}")),
                    }
                    if b0 > 0 {
                        let a = bulk.measure(AFTER_RESTORE, None);
                        bulk_events(&a.check, "after-restore window ", &rtl, &mut e);
                        if step3::streams(a.rate(), RATE_HZ) {
                            e.push("stream persisted after restore".into());
                            recover = true;
                        }
                    }
                    if streams {
                        hits += 1;
                        println!(
                            "STREAMS: page {page} offset 0x{off:02X} value 0x{value:02X} ({bytes} bytes)"
                        );
                    }
                }
                if !recover && !step3::alive(&rtl) {
                    recover = true;
                }
                if !recover {
                    let d = step3::baseline_disturbed(&rtl, RATE_HZ);
                    if !d.is_empty() {
                        e.push(format!("baseline disturbed: {}", d.join(", ")));
                        let _ = step3::apply_baseline(&rtl, RATE_HZ, &[]);
                        let still = step3::baseline_disturbed(&rtl, RATE_HZ);
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
                    line.set("w", true);
                    w(&mut out, line)?;
                    drop(bulk);
                    drop(rtl);
                    let (r, b, setup) = step3::fresh_with_baseline(RATE_HZ, &[])?;
                    rtl = r;
                    bulk = b;
                    base = capture_base(&rtl)?;
                    w(
                        &mut out,
                        setup
                            .with("k", "recovery")
                            .with(
                                "detector",
                                usb::BulkCounter::describe(step3::BUF_LEN, CHECK_FIELDS),
                            )
                            .with(
                                "after",
                                J::obj().with("p", page).with("o", off).with("v", value),
                            ),
                    )?;
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
        "bytescan --restart: tested {tested}, skipped {skipped}, streaming hits {hits}, in {secs:.1} s ({per_s:.2} tested/s); {remaining} candidates left (~{:.2} h at this rate)",
        remaining as f64 / per_s.max(1e-9) / 3600.0
    );
    Ok(())
}
