//! Step 3: raw IQ output. The documents describe no raw-IQ mode, so it is
//! searched for: first the documented-only baseline is measured, then
//! every single bit of demodulator pages 0-4 is flipped in turn on top of
//! it. "Streaming" = sustained bulk data within 20 % of 2 bytes x the
//! sample rate.

use std::collections::HashSet;
use std::io::Write as _;
use std::time::Duration;

use crate::baselines::{self, Baseline};
use crate::content;
use crate::json::{J, hex8, hex32};
use crate::regs::*;
use crate::usb::{self, BulkCounter, Rtl};

pub const RATE_HZ: f64 = 2.4e6;
pub const CONFIRM_RATES_HZ: [f64; 3] = [2.4e6, 1.2e6, 2.048e6];
pub const WINDOW: Duration = Duration::from_millis(100);
pub const BUF_LEN: usize = 16 * 1024;
pub const TOLERANCE: f64 = 0.20;
const PAGES: u8 = 5;

pub fn expected_bytes_per_s(rate: f64) -> f64 {
    2.0 * rate
}

pub fn streams(bytes_per_s: f64, rate: f64) -> bool {
    (bytes_per_s / expected_bytes_per_s(rate) - 1.0).abs() <= TOLERANCE
}

/// One documented baseline write, recorded with its read-back.
pub fn step(name: &str, r: Result<J, usb::Fail>) -> J {
    match r {
        Ok(j) => J::obj().with("write", name).with("result", j),
        Err(e) => J::obj().with("write", name).with("error", e.to_string()),
    }
}

pub fn rmw_sys8(rtl: &Rtl, off: u16, clear: u8, set: u8) -> Result<J, usb::Fail> {
    let old = rtl.sys_read8(off)?;
    rtl.sys_write8(off, (old & !clear) | set)?;
    let back = rtl.sys_read8(off)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)))
}

fn rmw_demod(rtl: &Rtl, page: u8, off: u8, mask: u8, value: u8) -> Result<J, usb::Fail> {
    let (old, back) = rtl.demod_rmw(page, off, mask, value)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)).with("verified", back & mask == value & mask))
}

pub fn rmw_usb32(rtl: &Rtl, off: u16, clear: u32, set: u32) -> Result<J, usb::Fail> {
    let old = rtl.usb_read32(off)?;
    rtl.usb_write32(off, (old & !clear) | set)?;
    let back = rtl.usb_read32(off)?;
    Ok(J::obj().with("before", hex32(old)).with("after", hex32(back)))
}

/// The baseline items, each removable for the minimisation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    DemodCtl,
    TunerPower,
    AdEn,
    EnBbin,
    IfFreqZero,
    Rsamp,
    EpaMaxPkt,
    EpaCfg,
    EpaFifoReset,
    Dma,
}

pub const ITEMS: [Item; 10] = [
    Item::DemodCtl,
    Item::TunerPower,
    Item::AdEn,
    Item::EnBbin,
    Item::IfFreqZero,
    Item::Rsamp,
    Item::EpaMaxPkt,
    Item::EpaCfg,
    Item::EpaFifoReset,
    Item::Dma,
];

/// Apply the documented-only baseline (minus `skip`) for `rate`.
pub fn apply_baseline(rtl: &Rtl, rate: f64, skip: &[Item]) -> J {
    let mut log = J::Arr(Vec::new());
    let (_, ep_addr, mps) = rtl.bulk_in().unwrap_or((0, 0, 0));
    for item in ITEMS.into_iter().filter(|i| !skip.contains(i)) {
        let rec = match item {
            // Table 15: PLL, ADC_I, ADC_Q enabled; reset released.
            Item::DemodCtl => step(
                "DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15)",
                rmw_sys8(rtl, DEMOD_CTL, 0, DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE),
            ),
            // Tables 16-19, p. 29: GPIO3 output, driven 0 = tuner power on.
            Item::TunerPower => step("GPIO3: GPD[3]=0 output, GPOE[3]=1, GPO[3]=0 power on (Tables 16-19)", (|| {
                let a = rmw_sys8(rtl, GPD, GPIO_TUNER_POWER, 0)?;
                let b = rmw_sys8(rtl, GPOE, 0, GPIO_TUNER_POWER)?;
                let c = rmw_sys8(rtl, GPO, GPIO_TUNER_POWER, 0)?;
                Ok(J::obj().with("GPD", a).with("GPOE", b).with("GPO", c))
            })()),
            // §9.1: zero-IF: AD_EN_reg = AD_EN_reg1 = 1.
            Item::AdEn => step(
                "page 0 0x08[7:6] = 11 AD_EN_reg, AD_EN_reg1 (Table 4, zero-IF §9.1)",
                rmw_demod(rtl, AD_EN_PAGE, AD_EN_OFF, AD_EN_REG | AD_EN_REG1, AD_EN_REG | AD_EN_REG1),
            ),
            Item::EnBbin => step("page 1 0xB1[0] = 1 en_bbin (Table 4, zero-IF §9.1)", rmw_demod(rtl, BBIN_PAGE, BBIN_OFF, EN_BBIN, EN_BBIN)),
            // §9.3: f_IF = 0 -> pset_iffreq = 0, {0x19..0x1B}[21:0].
            Item::IfFreqZero => step("pset_iffreq = 0 (Table 6, §9.3 zero IF)", (|| {
                let a = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB, 0x3F, 0)?;
                let b = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, 0)?;
                let c = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, 0)?;
                Ok(J::Arr(vec![a, b, c]))
            })()),
            // §9.4: rsamp_ratio = floor(28.8e6 / rate * 2^22) at {0x9F..0xA2}[27:2].
            Item::Rsamp => {
                let ratio = rsamp_ratio(rate);
                let word = (ratio & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
                let b = word.to_be_bytes();
                let masks = [0x0F, 0xFF, 0xFF, 0xFC];
                step(&format!("rsamp_ratio = 0x{ratio:07X} for {rate} Hz (Table 7, §9.4)"), (|| {
                    let mut v = Vec::new();
                    for k in 0..4 {
                        v.push(rmw_demod(rtl, RSAMP_PAGE, RSAMP_MSB + k as u8, masks[k], b[k])?);
                    }
                    Ok(J::Arr(v))
                })())
            }
            // Table 33: the descriptor's max packet size.
            Item::EpaMaxPkt => step(
                "USB_EPA_MAXPKT = descriptor wMaxPacketSize (Table 33)",
                rmw_usb32(rtl, USB_EPA_MAXPKT, EPA_MAXPKT_MASK, mps as u32 & EPA_MAXPKT_MASK),
            ),
            // Table 31: enable, bulk, endpoint number from the descriptor.
            Item::EpaCfg => step(
                "USB_EPA_CFG: enable, type bulk, number from the descriptor (Table 31)",
                rmw_usb32(
                    rtl,
                    USB_EPA_CFG,
                    EPA_CFG_ENABLE | EPA_CFG_TYPE_MASK | EPA_CFG_NUM_MASK | EPA_CFG_ISO_MASK,
                    EPA_CFG_ENABLE | EPA_CFG_TYPE_BULK << EPA_CFG_TYPE_SHIFT | (ep_addr as u32 & EPA_CFG_NUM_MASK),
                ),
            ),
            // Table 32: FIFO reset asserted then released, stall cleared.
            Item::EpaFifoReset => step("USB_EPA_CTL: FIFO reset 1 then 0, stall 0 (Table 32)", (|| {
                let a = rmw_usb32(rtl, USB_EPA_CTL, EPA_CTL_STALL, EPA_CTL_FIFO_RESET)?;
                let b = rmw_usb32(rtl, USB_EPA_CTL, EPA_CTL_FIFO_RESET | EPA_CTL_STALL, 0)?;
                Ok(J::Arr(vec![a, b]))
            })()),
            // Table 30: DMA enable.
            Item::Dma => step("USB_SYSCTL[0] = 1 DMA enable (Table 30)", rmw_usb32(rtl, USB_SYSCTL, 0, USB_SYSCTL_DMA_EN)),
        };
        log.push(rec.with("item", format!("{item:?}")));
    }
    log
}

pub fn window_record(w: &usb::Window, rate: f64) -> J {
    J::obj()
        .with("bytes", w.bytes)
        .with("elapsed_s", w.elapsed.as_secs_f64())
        .with("bytes_per_s", w.rate())
        .with("expected_bytes_per_s", expected_bytes_per_s(rate))
        .with("streams", streams(w.rate(), rate))
        .with("completions", w.completions)
        .with("completion_sizes_first", J::Arr(w.sizes.iter().map(|n| J::from(*n)).collect()))
        .with("stale_bytes_discarded", w.stale_bytes)
        .with("errors", J::Arr(w.errors.iter().map(|e| J::from(e.as_str())).collect()))
        .with("bulk_check", w.check.record())
}

pub fn fresh_with_baseline(rate: f64, skip: &[Item]) -> Result<(Rtl, BulkCounter, J), String> {
    let cyc = if crate::no_cycle() {
        J::obj().with("skipped", "--no-cycle: the dongle was opened as found, not power-cycled")
    } else {
        usb::power_cycle()?
    };
    let rtl = Rtl::open()?;
    let log = apply_baseline(&rtl, rate, skip);
    let bulk = BulkCounter::open(&rtl, BUF_LEN)?;
    Ok((rtl, bulk, J::obj().with("power_cycle", cyc).with("baseline_writes", log)))
}

pub fn baseline() -> Result<(), String> {
    usb::guard()?;
    let mut rec = crate::header("3-baseline", "bulk IN bytes per 100 ms window with the documented-only baseline applied");
    let (rtl, mut bulk, setup) = fresh_with_baseline(RATE_HZ, &[])?;
    rec.set("descriptor", rtl.descriptor_record());
    rec.set("rate_hz", RATE_HZ);
    rec.set("rsamp_ratio", hex32(rsamp_ratio(RATE_HZ)));
    rec.set("setup", setup);
    rec.set("detector", format!("{} transfers of {} bytes queued; streaming = within {}% of 2 x rate", BulkCounter::QUEUED, BUF_LEN, TOLERANCE * 100.0));
    let mut windows = J::Arr(Vec::new());
    for _ in 0..10 {
        windows.push(window_record(&bulk.measure(WINDOW, None), RATE_HZ));
    }
    rec.set("windows", windows);
    bulk.close();
    crate::write_record("step3-baseline.json", &rec);
    Ok(())
}

/// The bulk detector's own check: windows from the baseline with the
/// endpoint left alone, with the chip's stall bit set (USB_EPA_CTL bit 4,
/// Table 32), with it cleared again, and after a FIFO reset pulse (bit 9),
/// each with its self-check. A stalled endpoint must show as halts with
/// their ClearFeature(ENDPOINT_HALT) results, and the queue must be back
/// to full in flight once the bit is cleared. A last pair of phases does
/// the same with the halt left uncleared (completions only resubmitted),
/// then clears it.
pub fn detector_check() -> Result<(), String> {
    usb::guard()?;
    let mut rec = crate::header("3-detector-check", "the bulk IN detector's self-check across a deliberately stalled endpoint")
        .with("detector", BulkCounter::describe(BUF_LEN, "each window carries bulk_check (the span since the previous window's end)"));
    let (rtl, mut bulk, setup) = fresh_with_baseline(RATE_HZ, &[])?;
    rec.set("setup", setup);
    let epa = |rtl: &Rtl| rtl.usb_read32(USB_EPA_CTL).map(hex32).map_err(|e| e.to_string());
    let mut phases = J::Arr(Vec::new());
    let mut phase = |name: &str, action: J, bulk: &mut BulkCounter, n: usize| {
        let mut w = J::Arr(Vec::new());
        for _ in 0..n {
            w.push(window_record(&bulk.measure(WINDOW, None), RATE_HZ));
        }
        phases.push(J::obj().with("phase", name).with("action", action).with("USB_EPA_CTL_after", epa(&rtl).ok()).with("windows", w));
    };
    phase("idle", J::from("none"), &mut bulk, 3);
    let set = rmw_usb32(&rtl, USB_EPA_CTL, 0, EPA_CTL_STALL).unwrap_or_else(|e| J::from(format!("error: {e}")));
    phase("stall bit set", J::obj().with("USB_EPA_CTL bit 4 = 1", set), &mut bulk, 2);
    let clr = rmw_usb32(&rtl, USB_EPA_CTL, EPA_CTL_STALL, 0).unwrap_or_else(|e| J::from(format!("error: {e}")));
    phase("stall bit cleared", J::obj().with("USB_EPA_CTL bit 4 = 0", clr), &mut bulk, 3);
    let fifo = (|| {
        let a = rmw_usb32(&rtl, USB_EPA_CTL, 0, EPA_CTL_FIFO_RESET)?;
        let b = rmw_usb32(&rtl, USB_EPA_CTL, EPA_CTL_FIFO_RESET, 0)?;
        Ok::<J, usb::Fail>(J::Arr(vec![a, b]))
    })()
    .unwrap_or_else(|e| J::from(format!("error: {e}")));
    phase("after FIFO reset pulse", J::obj().with("USB_EPA_CTL bit 9 = 1 then 0", fifo), &mut bulk, 2);
    // What a detector that only resubmits does after a halt (the harness's
    // counter before the self-check): the halt is NOT cleared here.
    bulk.clear_halts = false;
    let set = rmw_usb32(&rtl, USB_EPA_CTL, 0, EPA_CTL_STALL).unwrap_or_else(|e| J::from(format!("error: {e}")));
    phase("stall bit set, halt not cleared (resubmit only)", J::obj().with("USB_EPA_CTL bit 4 = 1", set), &mut bulk, 2);
    let clr = rmw_usb32(&rtl, USB_EPA_CTL, EPA_CTL_STALL, 0).unwrap_or_else(|e| J::from(format!("error: {e}")));
    phase("stall bit cleared, halt still not cleared (resubmit only)", J::obj().with("USB_EPA_CTL bit 4 = 0", clr), &mut bulk, 2);
    bulk.clear_halts = true;
    bulk.force_clear_halt();
    phase("after ClearFeature(ENDPOINT_HALT)", J::from("cancel, clear, queue again"), &mut bulk, 2);
    rec.set("phases", phases);
    rec.set("chip_answers_at_end", alive(&rtl));
    bulk.close();
    crate::write_record("step3-detector-check.json", &rec);
    Ok(())
}

/// The candidates a scan record already holds, and whether it has its
/// header line.
fn done_keys(path: &std::path::Path) -> (HashSet<(u8, u8, u8)>, bool) {
    let mut set = HashSet::new();
    let mut has_header = false;
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            has_header |= line.contains("\"kind\":\"header\"");
            // Lines carry "page":P,"offset":O,"bit":B as plain integers.
            let get = |k: &str| -> Option<u8> {
                let i = line.find(&format!("\"{k}\":"))? + k.len() + 3;
                line[i..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
            };
            if let (Some(p), Some(o), Some(b)) = (get("page"), get("offset_dec"), get("bit")) {
                set.insert((p, o, b));
            }
        }
    }
    (set, has_header)
}

/// The baseline's demodulator and system settings still in place: the
/// names of any that a candidate disturbed (empty = intact).
pub fn baseline_disturbed(rtl: &Rtl, rate: f64) -> Vec<String> {
    let word = (rsamp_ratio(rate) & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
    let b = word.to_be_bytes();
    let mut checks: Vec<(String, Option<bool>)> = vec![
        (
            "DEMOD_CTL".into(),
            rtl.sys_read8(DEMOD_CTL).ok().map(|v| {
                let want = DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE;
                v & want == want
            }),
        ),
        ("AD_EN".into(), rtl.demod_read8(AD_EN_PAGE, AD_EN_OFF).ok().map(|v| v & (AD_EN_REG | AD_EN_REG1) == AD_EN_REG | AD_EN_REG1)),
        ("en_bbin".into(), rtl.demod_read8(BBIN_PAGE, BBIN_OFF).ok().map(|v| v & EN_BBIN == EN_BBIN)),
    ];
    let masks = [0x0F, 0xFF, 0xFF, 0xFC];
    for k in 0..4 {
        let ok = rtl.demod_read8(RSAMP_PAGE, RSAMP_MSB + k as u8).ok().map(|v| v & masks[k] == b[k] & masks[k]);
        checks.push((format!("rsamp_ratio byte {k}"), ok));
    }
    checks.into_iter().filter(|(_, ok)| *ok != Some(true)).map(|(n, _)| n).collect()
}

/// The chip answers if a system read succeeds.
pub fn alive(rtl: &Rtl) -> bool {
    rtl.sys_read8(DEMOD_CTL).is_ok()
}

pub const SCAN_USAGE: &str = "scan [--baseline zero-if|low-if|low-if-open|power-on] [--limit N] [--redo-flowless | --redo-list P:OFF:BIT,...]";

/// low-if-open: the window the held flow is checked with before each
/// candidate's write.
pub const FLOW_CHECK: Duration = Duration::from_millis(50);

/// low-if-open: the candidates whose window and stale count were both 0
/// (the path carried nothing before or after the write: the held flow had
/// been lost before them, which the register checks cannot see) and that
/// have no "redo" line yet. Lines carry "bytes":N and
/// "stale_bytes_discarded":N as plain integers.
fn flowless_keys(path: &std::path::Path) -> HashSet<(u8, u8, u8)> {
    let mut set = HashSet::new();
    let mut redone = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines().filter(|l| l.contains("\"kind\":\"candidate\"")) {
            let get = |k: &str| -> Option<usize> {
                let i = line.find(&format!("\"{k}\":"))? + k.len() + 3;
                line[i..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
            };
            let (Some(p), Some(o), Some(b)) = (get("page"), get("offset_dec"), get("bit")) else { continue };
            let key = (p as u8, o as u8, b as u8);
            if line.contains("\"redo\":true") {
                redone.insert(key);
            } else if get("bytes") == Some(0) && get("stale_bytes_discarded") == Some(0) {
                set.insert(key);
            }
        }
    }
    set.retain(|k| !redone.contains(k));
    set
}

/// The single-bit scan from one of the baselines ([`Baseline`]; zero-if,
/// the original, by default), resumable from its record, at most `--limit`
/// new candidates per run. A new record (low-if, power-on) starts with the
/// baseline measured alone; if it streams by itself the scan stops there.
pub fn scan(args: &[String]) -> Result<(), String> {
    let mut baseline = Baseline::ZeroIf;
    let mut limit: Option<usize> = None;
    let mut redo_mode = false;
    let mut redo_list: HashSet<(u8, u8, u8)> = HashSet::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--redo-flowless" {
            redo_mode = true;
            i += 1;
            continue;
        }
        if args[i] == "--redo-list" {
            // P:OFF:BIT,... (OFF hex with 0x or decimal): re-test exactly these.
            let v = args.get(i + 1).ok_or(SCAN_USAGE)?;
            for item in v.split(',') {
                let f: Vec<&str> = item.split(':').collect();
                let n = |s: &str| -> Result<u8, String> {
                    match s.strip_prefix("0x") {
                        Some(h) => u8::from_str_radix(h, 16).map_err(|e| format!("{item}: {e}")),
                        None => s.parse().map_err(|e| format!("{item}: {e}")),
                    }
                };
                if f.len() != 3 {
                    return Err(format!("--redo-list {item}: P:OFF:BIT"));
                }
                redo_list.insert((n(f[0])?, n(f[1])?, n(f[2])?));
            }
            redo_mode = true;
            i += 2;
            continue;
        }
        let v = args.get(i + 1).ok_or(SCAN_USAGE)?;
        match args[i].as_str() {
            "--baseline" => baseline = Baseline::parse(v)?,
            "--limit" => limit = Some(v.parse().map_err(|e| format!("--limit {v}: {e}"))?),
            a => return Err(format!("{a}: {SCAN_USAGE}")),
        }
        i += 2;
    }
    usb::guard()?;
    let rate = baseline.rate_hz();
    let path = crate::data_dir().join(baseline.record_name());
    let (done, has_header) = done_keys(&path);
    if redo_mode && baseline != Baseline::LowIfOpen {
        return Err("--redo-flowless is for --baseline low-if-open".into());
    }
    let redo = if !redo_list.is_empty() {
        redo_list
    } else if redo_mode {
        flowless_keys(&path)
    } else {
        HashSet::new()
    };
    if redo_mode {
        println!("{} candidates to re-test from the held state with its flow verified", redo.len());
    }
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    let (mut rtl, mut bulk, setup) = baseline.fresh()?;
    if !has_header && baseline == Baseline::ZeroIf {
        let hdr = crate::header("3-scan", "bulk IN bytes per 100 ms with one demodulator bit flipped on top of the baseline")
            .with("candidate_space", "page 0..=4 x offset 0x00..=0xFF x bit 0..=7 (10240 candidates)")
            .with("settings_held_fixed", format!("documented-only baseline at {RATE_HZ} Hz; window 100 ms; {} x {} byte transfers queued", BulkCounter::QUEUED, BUF_LEN))
            .with("procedure", "read byte; write it with the bit flipped; measure; restore; verify the restore by read-back");
        writeln!(out, "{}", hdr.with("kind", "header").render()).map_err(|e| e.to_string())?;
    }
    let open = baseline == Baseline::LowIfOpen;
    let mut held: Option<content::Thresholds> = None;
    if has_header && open {
        let first = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        held = Some(content::Thresholds::from_header(first.lines().next().unwrap_or("")).ok_or("the record's header has no detector_thresholds")?);
    }
    if !has_header && baseline != Baseline::ZeroIf {
        let (solo, solo_streams, solo_content) = baselines::solo(baseline, &rtl, &mut bulk)?;
        let mut hdr = crate::header_with_source("3-scan", &format!("bulk IN bytes per 100 ms with one demodulator bit flipped on top of the {} baseline", baseline.name()))
            .with("candidate_space", "page 0..=4 x offset 0x00..=0xFF x bit 0..=7 (10240 candidates)")
            .with("settings_held_fixed", format!("the {} baseline (baseline_definition); streams judged against {rate} Hz; window 100 ms; {} x {} byte transfers queued; the source unkeyed during the scan", baseline.name(), BulkCounter::QUEUED, BUF_LEN))
            .with("procedure", "read byte; write it with the bit flipped; measure 100 ms (bytes already queued before the window counted as stale); restore; verify the restore by read-back; chip answering; the baseline's settings re-read (baseline_definition), reapplied if disturbed, power cycle and the baseline from scratch if they do not hold or the chip is gone")
            .with("baseline_definition", baseline.definition())
            .with("descriptor", rtl.descriptor_record())
            .with("baseline_setup", setup)
            .with("solo", solo)
            .with("solo_streams", solo_streams);
        if open {
            let t = content::Thresholds::from_solo(&solo_content, rate);
            println!("held state thresholds: {}", t.record().render());
            hdr.set(
                "hypothesis",
                "page 0 0x07 bit 7 opens the bulk endpoint's data path (data/step3-confirm-lowif-p0-07-bc.json: 45 MB/s replay of the FIFO memory, period 3,760 B = the 20-block FIFO). A second register may select what feeds that FIFO; with the path held open, a bit that routes the sampled signal into it would change the CONTENT, not just the flow: the 3,760-byte period would break",
            );
            hdr.set("detector", content::LEGEND);
            hdr.set("detector_thresholds", t.record());
            hdr.set("on_interesting", "the line is written, the chunk ends, and confirm-write runs on the candidate's value from this baseline (rate tracking at 2.4/1.2/2.048 Msps, tone off and at two offsets with --source-cmd, I/Q statistics and spectrum, minimisation); a 'confirmation' line names its record");
            held = Some(t);
        }
        writeln!(out, "{}", hdr.with("kind", "header").render()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        if solo_streams {
            bulk.close();
            println!("the {} baseline streams by itself: scan not started (see the header of {})", baseline.name(), path.display());
            return Ok(());
        }
        println!("{} baseline alone: no stream; scanning", baseline.name());
    } else {
        writeln!(out, "{}", setup.with("kind", "setup").render()).map_err(|e| e.to_string())?;
    }
    let (mut tested, mut hits, mut any_bytes, mut wedges, mut reapplied) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut changed, mut data, mut flow_lost) = (0usize, Vec::new(), 0usize);
    let mut interesting: Option<(u8, u8, u8)> = None;
    'scan: for page in 0..PAGES {
        for off in 0..=255u8 {
            for bit in 0..8u8 {
                if redo_mode != redo.contains(&(page, off, bit)) || (!redo_mode && done.contains(&(page, off, bit))) {
                    continue;
                }
                if limit.is_some_and(|n| tested >= n) || interesting.is_some() {
                    break 'scan;
                }
                tested += 1;
                let mut line = J::obj().with("kind", "candidate").with("page", page).with("offset_dec", off).with("offset", hex8(off)).with("bit", bit);
                if redo_mode {
                    line.set("redo", true);
                }
                if let Some(t) = &held {
                    // The held state includes the flow itself: a candidate
                    // can stop the replay for good while every register
                    // reads back as held (p0 0xB5 b3, p4 0x02 b0).
                    let f = bulk.measure(FLOW_CHECK, None);
                    line.set("held_flow_before", f.rate().round());
                    if f.rate() < t.rate_lo {
                        flow_lost += 1;
                        line.set("held_reapplied", baseline.apply(&rtl));
                        let g = bulk.measure(FLOW_CHECK, None);
                        line.set("held_flow_after_reapply", g.rate().round());
                        if g.rate() < t.rate_lo {
                            drop(bulk);
                            drop(rtl);
                            let (r, b, setup) = baseline.fresh()?;
                            writeln!(out, "{}", setup.with("kind", "recovery").with("reason", "held flow lost").with("before", J::obj().with("page", page).with("offset", hex8(off)).with("bit", bit)).render())
                                .map_err(|e| e.to_string())?;
                            rtl = r;
                            bulk = b;
                            let h = bulk.measure(FLOW_CHECK, None);
                            line.set("held_flow_after_power_cycle", h.rate().round());
                        }
                    }
                }
                let orig = rtl.demod_read8(page, off);
                match orig {
                    Err(e) => {
                        line.set("read_error", e.to_string());
                    }
                    Ok(o) => {
                        let flipped = o ^ (1 << bit);
                        line.set("original", hex8(o));
                        line.set("flipped", hex8(flipped));
                        let wr = rtl.demod_write8(page, off, flipped);
                        line.set("write_error", wr.err().map(|e| e.to_string()));
                        data.clear();
                        let w = bulk.measure(WINDOW, if open { Some(&mut data) } else { None });
                        line.set("bytes", w.bytes);
                        line.set("bytes_per_s", w.rate());
                        line.set("streams", streams(w.rate(), rate));
                        line.set("stale_bytes_discarded", w.stale_bytes);
                        hits += streams(w.rate(), rate) as usize;
                        any_bytes += (w.bytes + w.stale_bytes > 0) as usize;
                        if let Some(t) = &held {
                            // The content detector; the bytes are dropped.
                            let c = content::Content::of(&data, w.elapsed.as_secs_f64());
                            let (why, note) = t.judge(&c);
                            line.set("c", c.compact());
                            if !note.is_empty() {
                                changed += 1;
                                line.set("note", J::Arr(note.iter().map(|s| J::from(*s)).collect()));
                                println!("p{page} 0x{off:02X} b{bit}: {} {}", note.join(", "), c.compact().render());
                            }
                            if !why.is_empty() {
                                line.set("interesting", J::Arr(why.iter().map(|s| J::from(*s)).collect()));
                                println!("p{page} 0x{off:02X} b{bit}: INTERESTING ({}) {}", why.join(", "), c.compact().render());
                                interesting = Some((page, off, flipped));
                            }
                        } else if w.bytes + w.stale_bytes > 0 {
                            println!("p{page} 0x{off:02X} b{bit}: {} B in the window, {} B stale", w.bytes, w.stale_bytes);
                        }
                        if !w.errors.is_empty() {
                            line.set("transfer_errors", J::Arr(w.errors.iter().map(|e| J::from(e.as_str())).collect()));
                        }
                        line.set("bulk_check", w.check.compact());
                        let rs = rtl.demod_write8(page, off, o);
                        let back = rtl.demod_read8(page, off);
                        line.set("restore_error", rs.err().map(|e| e.to_string()));
                        line.set("restore_readback", back.as_ref().ok().map(|b| hex8(*b)));
                        line.set("restore_verified", back.as_ref().ok() == Some(&o));
                    }
                }
                let mut wedged = !alive(&rtl);
                if !wedged {
                    let disturbed = baseline.disturbed(&rtl);
                    if !disturbed.is_empty() {
                        // The candidate (or its restore) changed a baseline
                        // setting: log which, and apply the baseline again;
                        // if that does not hold, power-cycle as for a wedge.
                        reapplied += 1;
                        line.set("baseline_disturbed", J::Arr(disturbed.into_iter().map(J::from).collect()));
                        line.set("baseline_reapplied", baseline.apply(&rtl));
                        let still = baseline.disturbed(&rtl);
                        wedged = !still.is_empty();
                        line.set("baseline_still_disturbed", J::Arr(still.into_iter().map(J::from).collect()));
                    }
                }
                if wedged {
                    wedges += 1;
                    println!("p{page} 0x{off:02X} b{bit}: wedged or baseline lost; power cycle");
                    line.set("wedged", true);
                    writeln!(out, "{}", line.render()).map_err(|e| e.to_string())?;
                    drop(bulk);
                    drop(rtl);
                    let (r, b, setup) = baseline.fresh()?;
                    writeln!(out, "{}", setup.with("kind", "recovery").with("after", J::obj().with("page", page).with("offset", hex8(off)).with("bit", bit)).render())
                        .map_err(|e| e.to_string())?;
                    rtl = r;
                    bulk = b;
                    continue;
                }
                writeln!(out, "{}", line.render()).map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())?;
        }
        println!("page {page} done");
    }
    out.flush().map_err(|e| e.to_string())?;
    bulk.close();
    println!(
        "{} baseline: {tested} tested this run ({} before), {hits} streams, {any_bytes} with any bytes, {wedges} wedges, {reapplied} baseline reapplies{}",
        baseline.name(),
        done.len(),
        if open { format!(", {changed} with rate or signature changed, held flow lost before {flow_lost}, interesting: {interesting:?}") } else { String::new() }
    );
    if let Some((p, o, v)) = interesting {
        drop(rtl);
        println!("confirming p{p} 0x{o:02X} = 0x{v:02X} from {}", baseline.name());
        let r = crate::confirm_write::confirm(baseline, p, o, v, &crate::confirm_write::Opts::default());
        let mut l = J::obj().with("kind", "confirmation").with("page", p).with("offset", hex8(o)).with("value", hex8(v));
        match &r {
            Ok(name) => l.set("record", name.as_str()),
            Err(e) => l.set("error", e.as_str()),
        };
        writeln!(out, "{}", l.render()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        r?;
    }
    Ok(())
}

/// The 188-byte block size of [RTL2832U] Table 34 (USB_EPA_FIFO_CFG
/// counts dropped data in 188-byte blocks).
pub const BLOCK: usize = 188;

/// Block structure of a capture: its length modulo 188, and, laid out as
/// rows of 188 bytes, for every column the most common byte and the
/// fraction of rows holding it. A fixed marker at a fixed column shows
/// as a column near 1.0; noise-like samples give no such column.
pub fn block_structure(data: &[u8]) -> J {
    let rows = data.len() / BLOCK;
    let mut cols = J::Arr(Vec::new());
    let mut constant = J::Arr(Vec::new());
    if rows > 0 {
        for c in 0..BLOCK {
            let mut hist = [0usize; 256];
            for r in 0..rows {
                hist[data[r * BLOCK + c] as usize] += 1;
            }
            let (v, n) = hist.iter().enumerate().max_by_key(|(_, n)| **n).map(|(v, n)| (v, *n)).unwrap_or((0, 0));
            let frac = n as f64 / rows as f64;
            if frac >= 0.9 {
                constant.push(J::obj().with("column", c).with("byte", hex8(v as u8)).with("fraction", frac));
            }
            cols.push(J::obj().with("column", c).with("mode", hex8(v as u8)).with("fraction", frac));
        }
    }
    J::obj()
        .with("bytes", data.len())
        .with("bytes_mod_188", data.len() % BLOCK)
        .with("rows_of_188", rows)
        .with("columns_with_one_byte_in_90pct_of_rows", constant)
        .with("columns", cols)
}

/// I/Q statistics over interleaved unsigned 8-bit samples.
pub fn iq_stats(data: &[u8]) -> J {
    let mut ch = J::Arr(Vec::new());
    for (name, phase) in [("I", 0), ("Q", 1)] {
        let s: Vec<f64> = data.iter().skip(phase).step_by(2).map(|b| *b as f64).collect();
        let n = s.len().max(1) as f64;
        let mean = s.iter().sum::<f64>() / n;
        let var = s.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n;
        let mut hist = [0u64; 256];
        for x in &s {
            hist[*x as usize] += 1;
        }
        let distinct = hist.iter().filter(|c| **c > 0).count();
        let min = hist.iter().position(|c| *c > 0);
        let max = hist.iter().rposition(|c| *c > 0);
        ch.push(
            J::obj()
                .with("channel", name)
                .with("samples", s.len())
                .with("mean", mean)
                .with("variance", var)
                .with("distinct_values", distinct)
                .with("min", min)
                .with("max", max)
                .with("histogram", J::Arr(hist.iter().map(|c| J::from(*c)).collect())),
        );
    }
    J::obj().with("channels", ch)
}

pub fn confirm(args: &[String]) -> Result<(), String> {
    let parse = |s: Option<&String>| -> Result<u8, String> {
        let s = s.ok_or("confirm PAGE OFFSET BIT")?;
        let t = s.trim_start_matches("0x");
        u8::from_str_radix(t, if s.starts_with("0x") { 16 } else { 10 }).map_err(|e| e.to_string())
    };
    let (page, off, bit) = (parse(args.first())?, parse(args.get(1))?, parse(args.get(2))?);
    usb::guard()?;
    let mut rec = crate::header("3-confirm", "a scan hit: rate tracking across documented resampler rates, sample statistics, and minimisation of the baseline")
        .with("candidate", J::obj().with("page", page).with("offset", hex8(off)).with("bit", bit));
    let flip = |rtl: &Rtl| -> J {
        match rtl.demod_read8(page, off) {
            Ok(o) => {
                let f = o ^ (1 << bit);
                let w = rtl.demod_write8_verify(page, off, f);
                J::obj().with("original", hex8(o)).with("written", hex8(f)).with("readback", w.ok().map(hex8))
            }
            Err(e) => J::obj().with("error", e.to_string()),
        }
    };
    // (a) rate tracking, (b) sample statistics. A 100 ms control window
    // before the flip, then ten consecutive 100 ms windows starting right
    // after it, all bytes kept: a one-shot burst at the flip shows in the
    // first window, a sustained stream in all ten.
    let mut rates = J::Arr(Vec::new());
    for rate in CONFIRM_RATES_HZ {
        let (rtl, mut bulk, setup) = fresh_with_baseline(rate, &[])?;
        let control = bulk.measure(WINDOW, None);
        let f = flip(&rtl);
        let mut data = Vec::new();
        let mut windows = J::Arr(Vec::new());
        let (mut total, mut secs) = (0usize, 0f64);
        for _ in 0..10 {
            let w = bulk.measure(WINDOW, Some(&mut data));
            total += w.bytes + w.stale_bytes;
            secs += w.elapsed.as_secs_f64();
            windows.push(window_record(&w, rate));
        }
        let sustained = total as f64 / secs;
        rates.push(
            J::obj()
                .with("rate_hz", rate)
                .with("rsamp_ratio", hex32(rsamp_ratio(rate)))
                .with("setup", setup)
                .with("control_window_before_flip", window_record(&control, rate))
                .with("flip", f)
                .with("windows_after_flip", windows)
                .with("bytes_after_flip", total)
                .with("bytes_per_s_after_flip", sustained)
                .with("ratio_to_expected", sustained / expected_bytes_per_s(rate))
                .with("block_structure", block_structure(&data))
                .with("stats", iq_stats(&data)),
        );
        bulk.close();
    }
    rec.set("rate_tracking", rates);
    // Minimisation: remove each baseline item in turn, keep the flip.
    let mut mini = J::Arr(Vec::new());
    for item in ITEMS {
        let (rtl, mut bulk, setup) = fresh_with_baseline(RATE_HZ, &[item])?;
        let f = flip(&rtl);
        let w = bulk.measure(Duration::from_millis(500), None);
        mini.push(
            J::obj()
                .with("removed", format!("{item:?}"))
                .with("setup", setup)
                .with("flip", f)
                .with("window", window_record(&w, RATE_HZ))
                .with("necessary", !streams(w.rate(), RATE_HZ))
                .with("necessary_for_any_bytes", w.bytes == 0),
        );
        bulk.close();
    }
    rec.set("minimisation", mini);
    crate::write_record(&format!("step3-confirm-p{page}-{off:02x}-b{bit}.json"), &rec);
    Ok(())
}
