//! The starting states the step-3 bit scan can run from.
//!
//! The first bit scan (`data/step3-scan.jsonl`) started from the
//! documented zero-IF baseline ([RTL2832U] 9.1: AD_EN_reg = AD_EN_reg1 =
//! 1, en_bbin = 1, pset_iffreq = 0). The tuner is a low-IF part ([R820T]
//! 4.3 and Table 4-1: IF 3.57-4.57 MHz) mixing high-side (findings 4h),
//! and step 4 saw the demodulator respond to a tone only in the documented
//! IF mode with the tuner powered and tuned. Two more starting states:
//!
//! - `low-if`: step 4's configuration (the writes of `tuner::setup_demod`,
//!   the tuner image of `tuner::image` in the `vagc` configuration of
//!   findings 4b, the PLL at 300 MHz RF with /8 as in findings 4e), the
//!   resampler at 2.4 Msps instead of 8 MHz, and the bulk endpoint exactly
//!   as the zero-IF baseline sets it. Itemised ([`LowItem`]) so a hit can
//!   be minimised item by item.
//! - `power-on`: the chip as it powers up, with only the demodulator PLL
//!   enabled (without it every demodulator access stalls: findings 1C, and
//!   the scan's candidates are demodulator writes) and the bulk endpoint's
//!   registers checked against the descriptor (written only where they
//!   differ; at power-on they matched in findings 1A, so normally nothing).

use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use crate::json::{J, hex8, hex32};
use crate::meter::{self, Meter, Reading};
use crate::regs::*;
use crate::source::{self, Level, Source};
use crate::step3::{self, Item};
use crate::tuner::{self, Config, DemodIf, Pll};
use crate::usb::{self, Rtl};

/// The RF the low-if baseline tunes to: 300 MHz, where findings 4e
/// received the tone at the predicted IF (`data/step4-tune-300.000MHz.json`).
pub const LOWIF_RF_HZ: f64 = 300e6;
/// SEL_DIV 010 = /8 ([R820T2] p. 9), the code `tuner::choose_div` picked
/// for the 304.57 MHz LO in findings 4e (Nint 84, SDM 39504).
pub const LOWIF_SEL_DIV: u8 = 0b010;
/// R2 VCO_INDICATOR[6:0] inside the VCO's range (findings 4c: 70-127
/// where the tone was received, 0 or 63 outside).
pub const VCO_INDICATOR_LOCKED: RangeInclusive<u8> = 70..=127;
/// The resampler's power-on ratio, 0xC99999 ([RTL2832U] Table 7), is
/// f_s = f_xtal * 2^22 / ratio = 64/7 MHz by the 9.4 formula: the rate a
/// stream from the power-on state would be judged against.
pub fn power_on_rate_hz() -> f64 {
    XTAL_HZ * TWO_POW_22 / RSAMP_RESET as f64
}

/// The low-if baseline's items, in the order applied: `tuner::setup_demod`'s
/// writes (its 8 MHz rsamp_ratio replaced by [`LowItem::Rsamp`] at the
/// scan's rate), the tuner image, then the zero-IF baseline's endpoint
/// items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LowItem {
    DemodCtl,
    TunerPower,
    IicRepeat,
    AdEnIf,
    EnBbinOff,
    IfFreq,
    SpecInv,
    AgcLoops,
    Dagc,
    AgcPolarity,
    Rsamp,
    TunerImage,
    EpaMaxPkt,
    EpaCfg,
    EpaFifoReset,
    Dma,
}

pub const LOW_ITEMS: [LowItem; 16] = [
    LowItem::DemodCtl,
    LowItem::TunerPower,
    LowItem::IicRepeat,
    LowItem::AdEnIf,
    LowItem::EnBbinOff,
    LowItem::IfFreq,
    LowItem::SpecInv,
    LowItem::AgcLoops,
    LowItem::Dagc,
    LowItem::AgcPolarity,
    LowItem::Rsamp,
    LowItem::TunerImage,
    LowItem::EpaMaxPkt,
    LowItem::EpaCfg,
    LowItem::EpaFifoReset,
    LowItem::Dma,
];

/// The power-on baseline's two items.
pub const POWER_ON_ITEMS: [&str; 2] = ["DemodPll", "Endpoint"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    ZeroIf,
    LowIf,
    PowerOn,
}

impl Baseline {
    pub fn parse(s: &str) -> Result<Baseline, String> {
        match s {
            "zero-if" => Ok(Baseline::ZeroIf),
            "low-if" => Ok(Baseline::LowIf),
            "power-on" => Ok(Baseline::PowerOn),
            _ => Err(format!("--baseline {s}: zero-if | low-if | power-on")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Baseline::ZeroIf => "zero-if",
            Baseline::LowIf => "low-if",
            Baseline::PowerOn => "power-on",
        }
    }

    /// The scan record under derivation/data.
    pub fn record_name(self) -> &'static str {
        match self {
            Baseline::ZeroIf => "step3-scan.jsonl",
            Baseline::LowIf => "step3-scan-lowif.jsonl",
            Baseline::PowerOn => "step3-scan-poweron.jsonl",
        }
    }

    /// The sample rate a stream is judged against: the rate the baseline
    /// programs into rsamp_ratio, or the power-on ratio's.
    pub fn rate_hz(self) -> f64 {
        match self {
            Baseline::ZeroIf | Baseline::LowIf => step3::RATE_HZ,
            Baseline::PowerOn => power_on_rate_hz(),
        }
    }

    /// The names of the removable items (for minimisation).
    pub fn item_names(self) -> Vec<String> {
        match self {
            Baseline::ZeroIf => step3::ITEMS.iter().map(|i| format!("{i:?}")).collect(),
            Baseline::LowIf => LOW_ITEMS.iter().map(|i| format!("{i:?}")).collect(),
            Baseline::PowerOn => POWER_ON_ITEMS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Every item of the baseline in words, with its citation and why it
    /// is there, for the record's header.
    pub fn definition(self) -> J {
        let items: &[&str] = match self {
            Baseline::ZeroIf => &[
                "the documented-only baseline of findings step 3 (step3::apply_baseline): DEMOD_CTL PLL|ADC_I|ADC_Q|reset-release (Table 15); GPIO3 tuner power (Tables 16-19); AD_EN_reg = AD_EN_reg1 = 1, en_bbin = 1 (Table 4, zero-IF 9.1); pset_iffreq = 0 (Table 6, 9.3); rsamp_ratio for 2.4 Msps (Table 7, 9.4); USB_EPA_MAXPKT, USB_EPA_CFG, USB_EPA_CTL FIFO reset pulse, USB_SYSCTL DMA (Tables 30-33)",
            ],
            Baseline::LowIf => &[
                "a power cycle, then in order, every write a read-modify-write read back (tuner::setup_demod's writes, rsamp_ratio at the scan's rate instead of its 8 MHz):",
                "DemodCtl: DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15; the PLL makes the demodulator answer, findings 1C)",
                "TunerPower: GPIO3 GPD[3] = 0, GPOE[3] = 1, GPO[3] = 0, then 50 ms (Tables 16-19, p. 29)",
                "IicRepeat: page 1 0x01[3] = 1 (Table 3; the tuner path, findings 2)",
                "AdEnIf: page 0 0x08[7:6] = 01, AD_EN_reg = 0, AD_EN_reg1 = 1 (Table 4, IF mode 9.1)",
                "EnBbinOff: page 1 0xB1[0] = 0 en_bbin (Table 4, IF mode 9.1)",
                "IfFreq: pset_iffreq = -floor(4.57e6 / 28.8e6 * 2^22) = -665554 over page 1 0x19-0x1B (Table 6, 9.3; the IF of [R820T] Table 4-1 row 5)",
                "SpecInv: page 1 0x15[0] = 1 (9.3: a high-side tuner needs the inverted spectrum; findings 4h measured high-side mixing)",
                "AgcLoops: page 1 0x04[7:5] = 110, en_if_agc = en_rf_agc = 1, aagc_hold = 0 (Table 2)",
                "Dagc: page 1 0x11[0] = 1 en_dagc (Table 10)",
                "AgcPolarity: page 0 0x0E[1:0] = 00 (Table 2; findings 4b: polarity 0 closes the IF loop through VAGC)",
                "Rsamp: rsamp_ratio = floor(28.8e6 / rate * 2^22) over page 1 0x9F-0xA2 [27:2] (9.4 formula, Table 7); 0x3000000 at 2.4 Msps",
                "TunerImage: tuner::image (every fixed cell of [R820T2] Table 1-2 as printed, every other field the stated choice of tuner::IMAGE_NOTES) in the vagc configuration (VGA_MODE = 1, the IF gain from the VAGC pin wired to the demodulator's IF AGC output, findings 4b), CLK_OUT_ENB = 0 (the demodulator is clocked from it, findings 4a), PLL for LO = 300 MHz + 4.57 MHz (high side), SEL_DIV 010 /8, REF_DIV2 1: Nint 84, SDM 39504 (findings 4c, 4e); R5-R15 checked by the 32-byte read-back (R16-R31 cannot be read back, findings 4a); after 10 ms R2 VCO_INDICATOR checked in 70..=127 (findings 4c)",
                "EpaMaxPkt, EpaCfg, EpaFifoReset, Dma: the bulk endpoint as the zero-IF baseline sets it: USB_EPA_MAXPKT = the descriptor's wMaxPacketSize (Table 33); USB_EPA_CFG enable, bulk, endpoint number (Table 31); USB_EPA_CTL FIFO reset 1 then 0, stall 0 (Table 32); USB_SYSCTL DMA (Table 30)",
            ],
            Baseline::PowerOn => &[
                "a power cycle (VBUS off/on through the analyzer), then only:",
                "DemodPll: DEMOD_CTL bit 7 = 1, demodulator PLL enable (Table 15), read back. Unavoidable: with it 0 every demodulator read and write stalls (findings 1B/1C: 3/59 oracle bytes as booted, 58/59 with the PLL on), and every candidate of the scan is a demodulator write. The ADC enables (bits 6, 3) are NOT set: findings 1C needed only the PLL.",
                "Endpoint: the bulk endpoint's registers read and compared with what the host's bulk IN transfers need: USB_SYSCTL[0] DMA = 1 (Table 30), USB_EPA_CFG enable, type bulk, iso 00, number = the descriptor's endpoint (Table 31), USB_EPA_MAXPKT = the descriptor's wMaxPacketSize (Table 33), USB_EPA_CTL stall (bit 4) = 0 (Table 32). A register is written (read-modify-write, read back) only if it differs; at power-on findings 1A read all four as needed (0x01, 0xD1, 0x200, 0), so normally none is written. No FIFO reset pulse (nothing shows it is needed; the FIFO is at its power-on state).",
                "Nothing else: the tuner at its own power-on state (it answers, and clocks the demodulator, findings 4a), the demodulator's registers at theirs (page 0 0x08 = 0x0D: AD_EN_reg = AD_EN_reg1 = 0; page 1 0xB1 = 0x1F: en_bbin 1; pset_iffreq 0x35D75D = 32/7 MHz; rsamp_ratio 0xC99999 = 64/7 MHz; findings step 3 baseline and restart probe), GPIO3 not driven, the I2C repeater closed.",
            ],
        };
        J::obj()
            .with("baseline", self.name())
            .with("items", J::Arr(items.iter().map(|s| J::from(*s)).collect()))
            .with("rate_hz_judged_against", self.rate_hz())
            .with("expected_bytes_per_s", step3::expected_bytes_per_s(self.rate_hz()))
            .with("streams_criterion", format!("bytes/s within {}% of 2 x {} Hz", step3::TOLERANCE * 100.0, self.rate_hz()))
    }

    /// Apply the baseline at `rate` without the items named in `skip`; the
    /// record of every write and read-back. The power-on baseline writes
    /// no rate.
    pub fn apply_at(self, rtl: &Rtl, rate: f64, skip: &[String]) -> J {
        match self {
            Baseline::ZeroIf => {
                let items: Vec<Item> = step3::ITEMS.into_iter().filter(|i| skip.contains(&format!("{i:?}"))).collect();
                step3::apply_baseline(rtl, rate, &items)
            }
            Baseline::LowIf => apply_low_if(rtl, rate, skip),
            Baseline::PowerOn => apply_power_on(rtl, skip),
        }
    }

    pub fn apply(self, rtl: &Rtl) -> J {
        self.apply_at(rtl, self.rate_hz(), &[])
    }

    /// The names of the baseline's settings a candidate disturbed (empty =
    /// intact).
    pub fn disturbed(self, rtl: &Rtl) -> Vec<String> {
        match self {
            Baseline::ZeroIf => step3::baseline_disturbed(rtl, step3::RATE_HZ),
            Baseline::LowIf => low_if_disturbed(rtl, step3::RATE_HZ),
            Baseline::PowerOn => power_on_disturbed(rtl),
        }
    }

    /// Power-cycle (unless `--no-cycle`), open, apply; the bulk counter
    /// (transfers of `buf_len` bytes) opened after the baseline as in
    /// `step3::fresh_with_baseline`.
    pub fn fresh_at(self, rate: f64, skip: &[String], buf_len: usize) -> Result<(Rtl, usb::BulkCounter, J), String> {
        let cyc = if crate::no_cycle() {
            J::obj().with("skipped", "--no-cycle: the dongle was opened as found, not power-cycled")
        } else {
            usb::power_cycle()?
        };
        let rtl = Rtl::open()?;
        let log = self.apply_at(&rtl, rate, skip);
        let bulk = usb::BulkCounter::open(&rtl, buf_len)?;
        Ok((
            rtl,
            bulk,
            J::obj()
                .with("power_cycle", cyc)
                .with("baseline", self.name())
                .with("rate_hz", rate)
                .with("skipped_items", J::Arr(skip.iter().map(|s| J::from(s.as_str())).collect()))
                .with("baseline_writes", log),
        ))
    }

    /// The scan's setup: zero-if exactly as the original scan did it.
    pub fn fresh(self) -> Result<(Rtl, usb::BulkCounter, J), String> {
        if self == Baseline::ZeroIf {
            return step3::fresh_with_baseline(step3::RATE_HZ, &[]);
        }
        self.fresh_at(self.rate_hz(), &[], step3::BUF_LEN)
    }
}

/// `n` 100 ms windows: their records, whether any streamed, total bytes
/// (window and stale).
fn windows(bulk: &mut usb::BulkCounter, rate: f64, n: usize) -> (J, bool, usize) {
    let mut ws = J::Arr(Vec::new());
    let (mut any, mut total) = (false, 0);
    for _ in 0..n {
        let w = bulk.measure(step3::WINDOW, None);
        any |= step3::streams(w.rate(), rate);
        total += w.bytes + w.stale_bytes;
        ws.push(step3::window_record(&w, rate));
    }
    (ws, any, total)
}

/// The baseline measured alone: ten 100 ms windows. For low-if, when a
/// source is given (`--source-cmd`), also the positive control: the meter
/// of findings 4b (`if_agc_val`, MSB-first) with the tone off and on at
/// [`LOWIF_RF_HZ`], and ten more windows with the tone on; the source is
/// unkeyed and closed before the scan. Returns (record, any window
/// streamed).
pub fn solo(b: Baseline, rtl: &Rtl, bulk: &mut usb::BulkCounter) -> Result<(J, bool), String> {
    let rate = b.rate_hz();
    let (ws, mut streams, total) = windows(bulk, rate, 10);
    println!("{} alone: {total} B in 10 x 100 ms", b.name());
    let mut rec = J::obj().with("windows", ws).with("bytes_total", total).with("any_window_streams", streams);
    if b == Baseline::LowIf && source::command().is_some() {
        let start = Instant::now();
        let mut src = Source::open()?;
        let described = src.describe();
        let off_src = src.off()?;
        let off = Reading::take(rtl, start, false, None);
        let on_src = src.level(LOWIF_RF_HZ, CONTROL_LEVEL)?;
        let on = Reading::take(rtl, start, false, None);
        let (w_on, s_on, n_on) = windows(bulk, rate, 10);
        let off_again = src.off()?;
        src.quit();
        let moved = meter::moved(&off, &on, Meter::IfAgcMsbFirst).map(|(a, c, m)| J::obj().with("off", a).with("on", c).with("moved", m));
        println!("{} with the tone on: {n_on} B in 10 x 100 ms; if_agc_val off -> on: {}", b.name(), moved.as_ref().map(J::render).unwrap_or_default());
        streams |= s_on;
        rec.set(
            "positive_control",
            J::obj()
                .with("purpose", "the low-if baseline is the state in which step 4 measured the tone: if_agc_val (MSB-first, findings 4b, about -207 counts/dB, 8191 = no signal) must fall with the tone on")
                .with("source", described)
                .with("tone_off", off_src)
                .with("reading_off", J::obj().with("medians", off.medians()).with("snapshots", off.full()))
                .with("tone_on", on_src)
                .with("reading_on", J::obj().with("medians", on.medians()).with("snapshots", on.full()))
                .with("if_agc_val_msb_first", moved)
                .with("windows_tone_on", w_on)
                .with("bytes_total_tone_on", n_on)
                .with("any_window_streams_tone_on", s_on)
                .with("tone_off_after", off_again),
        );
    }
    Ok((rec, streams))
}

/// The positive control's level: +10 dB relative, where findings 4c-4e
/// received the tone at full strength (if_agc_val about -800).
pub const CONTROL_LEVEL: Level = Level { txvga_db: 10, backoff_db: 0 };

pub fn low_if_pll() -> Pll {
    Pll::program(tuner::lo_for(LOWIF_RF_HZ, tuner::IF_HZ, true), LOWIF_SEL_DIV, tuner::REF_DIV2_BY_DOCUMENT).expect("300 MHz + 4.57 MHz with /8 is inside Nint's range")
}

fn rmw_demod(rtl: &Rtl, page: u8, off: u8, mask: u8, value: u8) -> Result<J, usb::Fail> {
    let (old, back) = rtl.demod_rmw(page, off, mask, value)?;
    Ok(J::obj().with("before", hex8(old)).with("after", hex8(back)).with("verified", back & mask == value & mask))
}

fn apply_low_if(rtl: &Rtl, rate: f64, skip: &[String]) -> J {
    let d = DemodIf::default();
    let mut log = J::Arr(Vec::new());
    for item in LOW_ITEMS.into_iter().filter(|i| !skip.contains(&format!("{i:?}"))) {
        let rec = match item {
            LowItem::DemodCtl => step3::step(
                "DEMOD_CTL |= PLL|ADC_I|ADC_Q|reset-release (Table 15)",
                step3::rmw_sys8(rtl, DEMOD_CTL, 0, DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE),
            ),
            LowItem::TunerPower => step3::step("GPIO3: GPD[3]=0 output, GPOE[3]=1, GPO[3]=0 tuner power on (Tables 16-19)", (|| {
                let a = step3::rmw_sys8(rtl, GPD, GPIO_TUNER_POWER, 0)?;
                let b = step3::rmw_sys8(rtl, GPOE, 0, GPIO_TUNER_POWER)?;
                let c = step3::rmw_sys8(rtl, GPO, GPIO_TUNER_POWER, 0)?;
                std::thread::sleep(Duration::from_millis(50));
                Ok(J::obj().with("GPD", a).with("GPOE", b).with("GPO", c).with("settle_ms", 50))
            })()),
            LowItem::IicRepeat => step3::step("page 1 0x01[3] = 1 IIC_repeat (Table 3)", rmw_demod(rtl, IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT_TABLE3, IIC_REPEAT_BIT_TABLE3)),
            LowItem::AdEnIf => step3::step(
                "page 0 0x08[7:6] = 01: AD_EN_reg = 0, AD_EN_reg1 = 1 (Table 4, IF mode 9.1)",
                rmw_demod(rtl, AD_EN_PAGE, AD_EN_OFF, AD_EN_REG | AD_EN_REG1, AD_EN_REG1),
            ),
            LowItem::EnBbinOff => step3::step("page 1 0xB1[0] = 0 en_bbin (IF mode 9.1)", rmw_demod(rtl, BBIN_PAGE, BBIN_OFF, EN_BBIN, 0)),
            LowItem::IfFreq => {
                let b = tuner::pset_iffreq_bytes(d.if_hz);
                step3::step(&format!("pset_iffreq = {} for {} Hz (Table 6, 9.3)", tuner::pset_iffreq(d.if_hz), d.if_hz), (|| {
                    let x = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB, 0x3F, b[0])?;
                    let y = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 1, 0xFF, b[1])?;
                    let z = rmw_demod(rtl, IFFREQ_PAGE, IFFREQ_MSB + 2, 0xFF, b[2])?;
                    Ok(J::Arr(vec![x, y, z]))
                })())
            }
            LowItem::SpecInv => step3::step(&format!("page 1 0x15[0] = {} Spec_inv (Table 6, 9.3)", d.spec_inv as u8), rmw_demod(rtl, 1, 0x15, 0x01, d.spec_inv as u8)),
            LowItem::AgcLoops => step3::step("page 1 0x04[7:5] = 1 1 0: en_if_agc, en_rf_agc, aagc_hold (Table 2)", rmw_demod(rtl, 1, 0x04, 0xE0, 0xC0)),
            LowItem::Dagc => step3::step("page 1 0x11[0] = 1 en_dagc (Table 10)", rmw_demod(rtl, 1, 0x11, 0x01, 0x01)),
            LowItem::AgcPolarity => step3::step(
                &format!("page 0 0x0E[1:0] = polar_rf_agc {}, polar_if_agc {} (Table 2)", d.polar_rf_agc as u8, d.polar_if_agc as u8),
                rmw_demod(rtl, 0, 0x0E, 0x03, (d.polar_rf_agc as u8) << 1 | d.polar_if_agc as u8),
            ),
            LowItem::Rsamp => {
                let all_but: Vec<Item> = step3::ITEMS.into_iter().filter(|i| *i != Item::Rsamp).collect();
                step3::apply_baseline(rtl, rate, &all_but)
            }
            LowItem::TunerImage => {
                let front = Config::Vagc.front();
                let pll = low_if_pll();
                let img = tuner::image(&front, &pll);
                let applied = tuner::apply_image(rtl, &img);
                // [R820T] Table 1-2: PLL locking time 5 ms max.
                std::thread::sleep(Duration::from_millis(10));
                let lock = tuner::read_status(rtl).map(|s| {
                    let v = s[2] & 0x7F;
                    J::obj().with("status", tuner::status_record(&s)).with("vco_indicator_in_70_127", VCO_INDICATOR_LOCKED.contains(&v))
                });
                J::obj()
                    .with("write", "tuner image, vagc configuration, 300 MHz RF (findings 4a-4e)")
                    .with("config", Config::Vagc.name())
                    .with("rf_hz", LOWIF_RF_HZ)
                    .with("front", front.record())
                    .with("pll", pll.record())
                    .with("image", tuner::image_record(&img))
                    .with("applied", applied)
                    .with("lock_check_after_10_ms", lock.unwrap_or_else(|e| J::from(format!("error: {e}"))))
            }
            LowItem::EpaMaxPkt | LowItem::EpaCfg | LowItem::EpaFifoReset | LowItem::Dma => {
                let name = format!("{item:?}");
                let all_but: Vec<Item> = step3::ITEMS.into_iter().filter(|i| format!("{i:?}") != name).collect();
                step3::apply_baseline(rtl, rate, &all_but)
            }
        };
        log.push(J::obj().with("item", format!("{item:?}")).with("record", rec));
    }
    log
}

fn low_if_disturbed(rtl: &Rtl, rate: f64) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |name: &str, ok: Option<bool>| {
        if ok != Some(true) {
            bad.push(name.to_string());
        }
    };
    let want = DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_ADC_Q_EN | DEMOD_CTL_RESET_RELEASE;
    check("DEMOD_CTL", rtl.sys_read8(DEMOD_CTL).ok().map(|v| v & want == want));
    check("AD_EN = 01", rtl.demod_read8(AD_EN_PAGE, AD_EN_OFF).ok().map(|v| v & (AD_EN_REG | AD_EN_REG1) == AD_EN_REG1));
    check("en_bbin = 0", rtl.demod_read8(BBIN_PAGE, BBIN_OFF).ok().map(|v| v & EN_BBIN == 0));
    check("IIC_repeat", rtl.demod_read8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF).ok().map(|v| v & IIC_REPEAT_BIT_TABLE3 != 0));
    check("Spec_inv = 1", rtl.demod_read8(1, 0x15).ok().map(|v| v & 1 == 1));
    let p = tuner::pset_iffreq_bytes(tuner::IF_HZ);
    check(
        "pset_iffreq",
        rtl.demod_read(IFFREQ_PAGE, IFFREQ_MSB, 3).ok().map(|v| v[0] & 0x3F == p[0] & 0x3F && v[1] == p[1] && v[2] == p[2]),
    );
    let word = (rsamp_ratio(rate) & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT;
    let b = word.to_be_bytes();
    let masks = [0x0F, 0xFF, 0xFF, 0xFC];
    check("rsamp_ratio", rtl.demod_read(RSAMP_PAGE, RSAMP_MSB, 4).ok().map(|v| (0..4).all(|k| v[k] & masks[k] == b[k] & masks[k])));
    check(
        "tuner R0 = 0x96, R2 in 70..=127",
        tuner::read_status(rtl).ok().map(|s| s[0] == TUNER_R0_CHECK && VCO_INDICATOR_LOCKED.contains(&(s[2] & 0x7F))),
    );
    bad
}

fn apply_power_on(rtl: &Rtl, skip: &[String]) -> J {
    let mut log = J::Arr(Vec::new());
    if !skip.iter().any(|s| s == POWER_ON_ITEMS[0]) {
        log.push(step3::step("DEMOD_CTL |= PLL enable only (Table 15; findings 1C)", step3::rmw_sys8(rtl, DEMOD_CTL, 0, DEMOD_CTL_PLL_EN)).with("item", POWER_ON_ITEMS[0]));
    }
    if skip.iter().any(|s| s == POWER_ON_ITEMS[1]) {
        return log;
    }
    let (_, ep_addr, mps) = rtl.bulk_in().unwrap_or((0, 0, 0));
    let cfg_mask = EPA_CFG_ENABLE | EPA_CFG_TYPE_MASK | EPA_CFG_NUM_MASK | EPA_CFG_ISO_MASK;
    let cfg_want = EPA_CFG_ENABLE | EPA_CFG_TYPE_BULK << EPA_CFG_TYPE_SHIFT | (ep_addr as u32 & EPA_CFG_NUM_MASK);
    let regs: [(&str, u16, u32, u32); 4] = [
        ("USB_SYSCTL[0] DMA = 1 (Table 30)", USB_SYSCTL, USB_SYSCTL_DMA_EN, USB_SYSCTL_DMA_EN),
        ("USB_EPA_CFG enable, bulk, iso 00, endpoint number (Table 31)", USB_EPA_CFG, cfg_mask, cfg_want),
        ("USB_EPA_MAXPKT = descriptor wMaxPacketSize (Table 33)", USB_EPA_MAXPKT, EPA_MAXPKT_MASK, mps as u32 & EPA_MAXPKT_MASK),
        ("USB_EPA_CTL stall = 0 (Table 32)", USB_EPA_CTL, EPA_CTL_STALL, 0),
    ];
    for (name, off, mask, want) in regs {
        let rec = match rtl.usb_read32(off) {
            Err(e) => J::obj().with("check", name).with("error", e.to_string()),
            Ok(v) if v & mask == want => J::obj().with("check", name).with("read", hex32(v)).with("as_needed", true).with("written", false),
            Ok(v) => J::obj().with("check", name).with("read", hex32(v)).with("as_needed", false).with("written", step3::step(name, step3::rmw_usb32(rtl, off, mask, want))),
        };
        log.push(rec.with("item", POWER_ON_ITEMS[1]));
    }
    log
}

fn power_on_disturbed(rtl: &Rtl) -> Vec<String> {
    match rtl.sys_read8(DEMOD_CTL) {
        Ok(v) if v & DEMOD_CTL_PLL_EN != 0 => Vec::new(),
        Ok(v) => vec![format!("DEMOD_CTL PLL (reads {})", hex8(v).render())],
        Err(_) => vec!["DEMOD_CTL unreadable".into()],
    }
}
