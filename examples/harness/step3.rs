//! Step 3: raw IQ output. The documents describe no raw-IQ mode, so it is
//! searched for: first the documented-only baseline is measured, then
//! every single bit of demodulator pages 0-4 is flipped in turn on top of
//! it. "Streaming" = sustained bulk data within 20 % of 2 bytes x the
//! sample rate.

use std::collections::HashSet;
use std::io::Write as _;
use std::time::Duration;

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
fn step(name: &str, r: Result<J, usb::Fail>) -> J {
    match r {
        Ok(j) => J::obj().with("write", name).with("result", j),
        Err(e) => J::obj().with("write", name).with("error", e.to_string()),
    }
}

fn rmw_sys8(rtl: &Rtl, off: u16, clear: u8, set: u8) -> Result<J, usb::Fail> {
    let old = rtl.sys_read8(off)?;
    rtl.sys_write8(off, (old & !clear) | set)?;
    let back = rtl.sys_read8(off)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)))
}

fn rmw_demod(rtl: &Rtl, page: u8, off: u8, mask: u8, value: u8) -> Result<J, usb::Fail> {
    let (old, back) = rtl.demod_rmw(page, off, mask, value)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)).with("verified", back & mask == value & mask))
}

fn rmw_usb32(rtl: &Rtl, off: u16, clear: u32, set: u32) -> Result<J, usb::Fail> {
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
}

pub fn fresh_with_baseline(rate: f64, skip: &[Item]) -> Result<(Rtl, BulkCounter, J), String> {
    let cyc = usb::power_cycle()?;
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

fn scan_path() -> std::path::PathBuf {
    crate::data_dir().join("step3-scan.jsonl")
}

fn done_keys() -> HashSet<(u8, u8, u8)> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(scan_path()) {
        for line in text.lines() {
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
    set
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

pub fn scan() -> Result<(), String> {
    usb::guard()?;
    let done = done_keys();
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(scan_path()).map_err(|e| e.to_string())?;
    if done.is_empty() {
        let hdr = crate::header("3-scan", "bulk IN bytes per 100 ms with one demodulator bit flipped on top of the baseline")
            .with("candidate_space", "page 0..=4 x offset 0x00..=0xFF x bit 0..=7 (10240 candidates)")
            .with("settings_held_fixed", format!("documented-only baseline at {RATE_HZ} Hz; window 100 ms; {} x {} byte transfers queued", BulkCounter::QUEUED, BUF_LEN))
            .with("procedure", "read byte; write it with the bit flipped; measure; restore; verify the restore by read-back");
        writeln!(out, "{}", hdr.with("kind", "header").render()).map_err(|e| e.to_string())?;
    }
    let (mut rtl, mut bulk, setup) = fresh_with_baseline(RATE_HZ, &[])?;
    writeln!(out, "{}", setup.with("kind", "setup").render()).map_err(|e| e.to_string())?;
    for page in 0..PAGES {
        for off in 0..=255u8 {
            for bit in 0..8u8 {
                if done.contains(&(page, off, bit)) {
                    continue;
                }
                let mut line = J::obj().with("kind", "candidate").with("page", page).with("offset_dec", off).with("offset", hex8(off)).with("bit", bit);
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
                        let w = bulk.measure(WINDOW, None);
                        line.set("bytes", w.bytes);
                        line.set("bytes_per_s", w.rate());
                        line.set("streams", streams(w.rate(), RATE_HZ));
                        line.set("stale_bytes_discarded", w.stale_bytes);
                        if !w.errors.is_empty() {
                            line.set("transfer_errors", J::Arr(w.errors.iter().map(|e| J::from(e.as_str())).collect()));
                        }
                        let rs = rtl.demod_write8(page, off, o);
                        let back = rtl.demod_read8(page, off);
                        line.set("restore_error", rs.err().map(|e| e.to_string()));
                        line.set("restore_readback", back.as_ref().ok().map(|b| hex8(*b)));
                        line.set("restore_verified", back.as_ref().ok() == Some(&o));
                    }
                }
                let mut wedged = !alive(&rtl);
                if !wedged {
                    let disturbed = baseline_disturbed(&rtl, RATE_HZ);
                    if !disturbed.is_empty() {
                        // The candidate (or its restore) changed a baseline
                        // setting: log which, and apply the baseline again;
                        // if that does not hold, power-cycle as for a wedge.
                        line.set("baseline_disturbed", J::Arr(disturbed.into_iter().map(J::from).collect()));
                        line.set("baseline_reapplied", apply_baseline(&rtl, RATE_HZ, &[]));
                        let still = baseline_disturbed(&rtl, RATE_HZ);
                        wedged = !still.is_empty();
                        line.set("baseline_still_disturbed", J::Arr(still.into_iter().map(J::from).collect()));
                    }
                }
                if wedged {
                    line.set("wedged", true);
                    writeln!(out, "{}", line.render()).map_err(|e| e.to_string())?;
                    drop(bulk);
                    drop(rtl);
                    let (r, b, setup) = fresh_with_baseline(RATE_HZ, &[])?;
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
    bulk.close();
    Ok(())
}

/// The 188-byte block size of [RTL2832U] Table 34 (USB_EPA_FIFO_CFG
/// counts dropped data in 188-byte blocks).
pub const BLOCK: usize = 188;

/// Block structure of a capture: its length modulo 188, and, laid out as
/// rows of 188 bytes, for every column the most common byte and the
/// fraction of rows holding it. A fixed marker at a fixed column shows
/// as a column near 1.0; noise-like samples give no such column.
fn block_structure(data: &[u8]) -> J {
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
fn iq_stats(data: &[u8]) -> J {
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
