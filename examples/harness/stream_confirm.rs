//! Step 10: the raw-sample stream sequence that findings step 9 observed
//! on the USB wire from the vendor's own driver (findings 9b, 9h), run on
//! our bench, proven to be samples, and minimised.
//!
//! Every test starts from a power cycle, opens the bulk endpoint (polling
//! from before the first write: transfers kept in flight throughout), then
//! applies the sequence's items ([`ITEMS`], in findings 9b's order, each
//! write read back, a short window after each item), and last the start
//! write (page 0 0x06 = 0x80, findings 9b item 22). Tests:
//!
//! - `replay`: the sequence as observed (test 1);
//! - `rate`: rsamp_ratio x1, x2, x1/2 and the 2.4 Msps value (test 2a);
//! - `tone`: the tuner programmed, the tone keyed at offsets (test 2b);
//! - `pause`: page 1 0x01 bit 2 set and cleared (test 2c);
//! - `coeff-probe`: the 20 registers page 1 0x1C-0x2F written live, one
//!   byte at a time, against the tuner's noise and a tone (test 3, the
//!   layout);
//! - `coeffs`: the stream from a power cycle with the observed values, with
//!   our own low-pass ([`own_lowpass`]) and with all zeros (test 3);
//! - `minimise`: each item removed in turn from a power cycle (test 4);
//! - `seldiv`: the tuner's SEL_DIV 011 against the tone (test 5).
//!
//! USB-block offset 0x1C is never written ([`usb_write_guarded`]; see
//! derivation/HAZARD-usb-0x1C.md).

use std::time::{Duration, Instant};

use crate::confirm_write::{self, FFT_N};
use crate::content::{self, Content};
use crate::json::{J, hex_bytes, hex8, hex32};
use crate::regs::*;
use crate::source::{self, Level, Source};
use crate::step3;
use crate::tuner::{self, Config, Front, Pll};
use crate::usb::{self, BulkCounter, Rtl};

pub const USAGE: &str = "stream-confirm replay|rate|tone|pause|coeff-probe|coeffs|minimise|seldiv [--level TXVGA BACKOFF] [--gain LNA MIX VGA] [--windows N]";

/// The values the vendor driver wrote to page 1 0x1C-0x2F (findings 9b
/// item 13). A TEST FIXTURE of the observed state for the confirmation
/// only: the driver never ships it (test 3 checks what else streams).
pub const OBSERVED_COEFFS_FIXTURE: [u8; COEFF_COUNT] = [
    0xCA, 0xDC, 0xD7, 0xD8, 0xE0, 0xF2, 0x0E, 0x35, 0x06, 0x50, 0x9C, 0x0D, 0x71, 0x11, 0x14, 0x71,
    0x74, 0x19, 0x41, 0xA5,
];
/// The 20 registers no document names: page 1 0x1C-0x2F.
pub const COEFF_PAGE: u8 = 1;
pub const COEFF_FIRST: u8 = 0x1C;
pub const COEFF_COUNT: usize = 20;

/// rsamp_ratio observed (findings 9b item 12: 03 84 00 00 over page 1
/// 0x9F-0xA2, the field at [27:2]) = 0xE10000, the DAB example of 9.4.
pub const OBSERVED_RSAMP_RATIO: u32 = 0xE1_0000;
/// pset_iffreq observed (findings 9b item 10): 0x381112 over 0x19-0x1B.
pub const OBSERVED_IFFREQ: [u8; 3] = [0x38, 0x11, 0x12];
/// The output rate observed against the resampler's f_symbol: 2.048 Msps
/// from 8.192 MHz (findings 9f), a factor of 4. Test 2a checks it holds
/// at other ratios.
pub const OBSERVED_DECIMATION: f64 = 4.0;

/// The start write (findings 9b item 22).
pub const START_PAGE: u8 = 0;
pub const START_OFF: u8 = 0x06;
pub const START_VALUE: u8 = 0x80;
/// The pause bit (findings 9d): page 1 0x01 bit 2.
pub const PAUSE_PAGE: u8 = 1;
pub const PAUSE_OFF: u8 = 0x01;
pub const PAUSE_BIT: u8 = 1 << 2;

/// The window after each item (bytes counted, none expected).
const ITEM_WINDOW: Duration = Duration::from_millis(20);
/// The wait the vendor left after the demodulator reset (findings 9b).
const RESET_WAIT: Duration = Duration::from_millis(65);
/// Transfers of one packet for the first-byte latency (a 16 KiB transfer
/// completes only when full: ~4 ms at the observed rate).
const FIRST_BYTE_BUF: usize = 512;
const FIRST_BYTE_DEADLINE: Duration = Duration::from_millis(500);

/// f_symbol for a ratio by the 9.4 formula: f_xtal * 2^22 / ratio.
pub fn f_symbol(ratio: u32) -> f64 {
    XTAL_HZ * TWO_POW_22 / ratio as f64
}

/// The output sample rate the observation predicts for a ratio.
pub fn output_rate(ratio: u32) -> f64 {
    f_symbol(ratio) / OBSERVED_DECIMATION
}

/// rsamp_ratio [25:0] placed at [27:2] of page 1 0x9F-0xA2, MSB first
/// (Table 7); the top nibble (cfreq_off_ratio's [3:0], Table 12) 0 as
/// observed.
pub fn rsamp_bytes(ratio: u32) -> [u8; 4] {
    ((ratio & ((1 << RSAMP_BITS) - 1)) << RSAMP_SHIFT).to_be_bytes()
}

/// pset_iffreq's 22-bit two's complement to the IF in Hz (9.3).
pub fn iffreq_hz(b: [u8; 3]) -> f64 {
    let u = (b[0] as u32 & 0x3F) << 16 | (b[1] as u32) << 8 | b[2] as u32;
    let s = if u & 0x20_0000 != 0 {
        u as i32 - 0x40_0000
    } else {
        u as i32
    };
    -(s as f64) / TWO_POW_22 * XTAL_HZ
}

/// One item of the sequence (findings 9b / 9h), in the observed order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// USB_SYSCTL <- 0x09: DMA enable, full-packet mode (Table 30).
    UsbSysctl,
    /// USB_EPA_MAXPKT <- 0x0200 and the next two bytes 0 (Table 33).
    EpaMaxPkt,
    /// USB_EPA_CTL <- 0x0210 (FIFO reset, stall) then 0 (Table 32).
    EpaFifoReset,
    /// ClearFeature(ENDPOINT_HALT) on the bulk IN endpoint.
    ClearHalt,
    /// DEMOD_CTL_1 <- 0x02 (the value it reads at power-on).
    DemodCtl1,
    /// DEMOD_CTL 0xA0, 0x80, 0xA0: PLL on, reset held, released (Table 15).
    DemodReset,
    /// 65 ms without a request.
    Wait65,
    /// DEMOD_CTL 0xA8, 0xE8: ADC_Q, then ADC_I (Table 15).
    AdcEnable,
    /// Page 1 0x3E-0x3F <- 00 00 (one 2-byte write).
    P1x3E,
    /// Page 1 0x15 <- 0x00.
    P1x15Zero,
    /// Page 1 0x16-0x17 <- 00 00.
    P1x16,
    /// Page 1 0x18 <- 0x00.
    P1x18,
    /// pset_iffreq 0x19-0x1A <- 38 11, 0x1B <- 12 (IF 3.570 MHz, 9.3).
    IfFreq,
    /// Page 1 0x15 <- 0x01 (Spec_inv 1, En_aci 0, Table 6).
    SpecInv,
    /// rsamp_ratio 0x9F-0xA0, 0xA1-0xA2 (Table 7).
    Rsamp,
    /// Page 1 0x1C-0x2F, one byte each, ascending.
    Coeffs,
    /// Page 0 0x17 <- 0x11.
    P0x17,
    /// Page 0 0x18 <- 0x10.
    P0x18,
    /// Page 0 0x19 <- 0x25.
    P0x19,
    /// Page 1 0x92 <- 0x00.
    P1x92,
    /// Page 1 0x93 <- 0xF0.
    P1x93,
    /// Page 1 0x94 <- 0x0F.
    P1x94,
    /// Page 0 0x61 <- 0x60 (enable_PID 0, mode_PID 1, Table 13).
    P0x61,
    /// Page 0 0x20 <- 0x40.
    P0x20,
}

pub const ITEMS: [Item; 24] = [
    Item::UsbSysctl,
    Item::EpaMaxPkt,
    Item::EpaFifoReset,
    Item::ClearHalt,
    Item::DemodCtl1,
    Item::DemodReset,
    Item::Wait65,
    Item::AdcEnable,
    Item::P1x3E,
    Item::P1x15Zero,
    Item::P1x16,
    Item::P1x18,
    Item::IfFreq,
    Item::SpecInv,
    Item::Rsamp,
    Item::Coeffs,
    Item::P0x17,
    Item::P0x18,
    Item::P0x19,
    Item::P1x92,
    Item::P1x93,
    Item::P1x94,
    Item::P0x61,
    Item::P0x20,
];

/// findings 9b items 6-21: the demodulator writes between the ADC enable
/// and the start write.
pub const DEMOD_ITEMS: [Item; 16] = [
    Item::P1x3E,
    Item::P1x15Zero,
    Item::P1x16,
    Item::P1x18,
    Item::IfFreq,
    Item::SpecInv,
    Item::Rsamp,
    Item::Coeffs,
    Item::P0x17,
    Item::P0x18,
    Item::P0x19,
    Item::P1x92,
    Item::P1x93,
    Item::P1x94,
    Item::P0x61,
    Item::P0x20,
];

/// One request of an item.
enum Op {
    Usb(u16, Vec<u8>),
    Sys(u16, u8),
    Demod(u8, u8, Vec<u8>),
    ClearHalt,
    Wait(Duration),
}

/// What a test varies in the sequence.
#[derive(Clone, Debug)]
pub struct Plan {
    pub coeffs: [u8; COEFF_COUNT],
    pub rsamp_ratio: u32,
    pub skip: Vec<Item>,
    /// The value of page 0 0x19 (observed 0x25; power-on 0x20).
    pub p0x19: u8,
    /// AdcEnable as the one write DEMOD_CTL <- 0xE8 instead of 0xA8, 0xE8.
    pub adc_single_write: bool,
}

/// page 0 0x19 as observed (findings 9b item 16).
pub const OBSERVED_P0X19: u8 = 0x25;

impl Plan {
    pub fn observed() -> Plan {
        Plan {
            coeffs: OBSERVED_COEFFS_FIXTURE,
            rsamp_ratio: OBSERVED_RSAMP_RATIO,
            skip: Vec::new(),
            p0x19: OBSERVED_P0X19,
            adc_single_write: false,
        }
    }

    /// Only the items test 4 found necessary: the ADC enable, page 0
    /// 0x19, then the start (data/step10-4-minimise.json).
    pub fn minimal() -> Plan {
        Plan {
            skip: ITEMS
                .into_iter()
                .filter(|i| !MINIMAL_ITEMS.contains(i))
                .collect(),
            ..Plan::observed()
        }
    }

    /// The minimal items plus every item test 4 found to set the content:
    /// Spec_inv (and the zeroing of 0x15 before it), page 1 0x16-0x18,
    /// pset_iffreq, rsamp_ratio, the 20 tap registers (our own design),
    /// page 0 0x17.
    pub fn content() -> Plan {
        Plan {
            skip: ITEMS
                .into_iter()
                .filter(|i| !MINIMAL_ITEMS.contains(i) && !CONTENT_ITEMS.contains(i))
                .collect(),
            coeffs: own_lowpass(),
            ..Plan::observed()
        }
    }

    /// The sequence the bench reduced the observation to (findings step
    /// 10): DEMOD_CTL <- 0xE8 in one write, page 1 0x16-0x18 zeroed,
    /// pset_iffreq, Spec_inv (0x15 <- 0x01 without the zeroing write
    /// before it), rsamp_ratio, our own taps, page 0 0x19 <- 0x25, page 1
    /// 0x93 <- 0xF0 (test 4c: it or 0x94 steadies the rate), then the start.
    pub fn driver() -> Plan {
        let keep = [
            Item::AdcEnable,
            Item::P1x16,
            Item::P1x18,
            Item::IfFreq,
            Item::SpecInv,
            Item::Rsamp,
            Item::Coeffs,
            Item::P0x19,
            Item::P1x93,
        ];
        Plan {
            skip: ITEMS.into_iter().filter(|i| !keep.contains(i)).collect(),
            coeffs: own_lowpass(),
            adc_single_write: true,
            ..Plan::observed()
        }
    }

    pub fn record(&self) -> J {
        J::obj()
            .with("coeffs_page1_0x1C_0x2F", hex_bytes(&self.coeffs))
            .with("rsamp_ratio", hex32(self.rsamp_ratio))
            .with(
                "rsamp_bytes_0x9F_0xA2",
                hex_bytes(&rsamp_bytes(self.rsamp_ratio)),
            )
            .with("f_symbol_hz", f_symbol(self.rsamp_ratio))
            .with("predicted_output_rate_hz", output_rate(self.rsamp_ratio))
            .with("p0x19", hex8(self.p0x19))
            .with("adc_single_write", self.adc_single_write)
            .with(
                "skipped_items",
                J::Arr(
                    self.skip
                        .iter()
                        .map(|i| J::from(format!("{i:?}")))
                        .collect(),
                ),
            )
    }
}

/// Test 4's necessary items (data/step10-4-minimise.json): without
/// AdcEnable every byte is 0x80; without P0x19 no byte comes.
pub const MINIMAL_ITEMS: [Item; 2] = [Item::AdcEnable, Item::P0x19];
/// Test 4's content items: each one's removal kept the stream and changed
/// what it carries (the tone's bin, sign, rate or level).
pub const CONTENT_ITEMS: [Item; 8] = [
    Item::P1x15Zero,
    Item::P1x16,
    Item::P1x18,
    Item::IfFreq,
    Item::SpecInv,
    Item::Rsamp,
    Item::Coeffs,
    Item::P0x17,
];

impl Item {
    fn ops(self, plan: &Plan) -> Vec<Op> {
        let d = |page: u8, off: u8, v: &[u8]| Op::Demod(page, off, v.to_vec());
        match self {
            Item::UsbSysctl => vec![Op::Usb(USB_SYSCTL, vec![0x09])],
            Item::EpaMaxPkt => vec![Op::Usb(USB_EPA_MAXPKT, vec![0x00, 0x02, 0x00, 0x00])],
            Item::EpaFifoReset => vec![
                Op::Usb(USB_EPA_CTL, vec![0x10, 0x02]),
                Op::Usb(USB_EPA_CTL, vec![0x00, 0x00]),
            ],
            Item::ClearHalt => vec![Op::ClearHalt],
            Item::DemodCtl1 => vec![Op::Sys(DEMOD_CTL_1, 0x02)],
            Item::DemodReset => vec![
                Op::Sys(DEMOD_CTL, 0xA0),
                Op::Sys(DEMOD_CTL, 0x80),
                Op::Sys(DEMOD_CTL, 0xA0),
            ],
            Item::Wait65 => vec![Op::Wait(RESET_WAIT)],
            Item::AdcEnable if plan.adc_single_write => vec![Op::Sys(DEMOD_CTL, 0xE8)],
            Item::AdcEnable => vec![Op::Sys(DEMOD_CTL, 0xA8), Op::Sys(DEMOD_CTL, 0xE8)],
            Item::P1x3E => vec![d(1, 0x3E, &[0x00, 0x00])],
            Item::P1x15Zero => vec![d(1, 0x15, &[0x00])],
            Item::P1x16 => vec![d(1, 0x16, &[0x00, 0x00])],
            Item::P1x18 => vec![d(1, 0x18, &[0x00])],
            Item::IfFreq => vec![
                d(1, 0x19, &OBSERVED_IFFREQ[..2]),
                d(1, 0x1B, &OBSERVED_IFFREQ[2..]),
            ],
            Item::SpecInv => vec![d(1, 0x15, &[0x01])],
            Item::Rsamp => {
                let b = rsamp_bytes(plan.rsamp_ratio);
                vec![
                    d(RSAMP_PAGE, RSAMP_MSB, &b[..2]),
                    d(RSAMP_PAGE, RSAMP_MSB + 2, &b[2..]),
                ]
            }
            Item::Coeffs => (0..COEFF_COUNT)
                .map(|k| d(COEFF_PAGE, COEFF_FIRST + k as u8, &[plan.coeffs[k]]))
                .collect(),
            Item::P0x17 => vec![d(0, 0x17, &[0x11])],
            Item::P0x18 => vec![d(0, 0x18, &[0x10])],
            Item::P0x19 => vec![d(0, 0x19, &[plan.p0x19])],
            Item::P1x92 => vec![d(1, 0x92, &[0x00])],
            Item::P1x93 => vec![d(1, 0x93, &[0xF0])],
            Item::P1x94 => vec![d(1, 0x94, &[0x0F])],
            Item::P0x61 => vec![d(0, 0x61, &[0x60])],
            Item::P0x20 => vec![d(0, 0x20, &[0x40])],
        }
    }
}

/// The USB-block offset never written (derivation/HAZARD-usb-0x1C.md).
pub const USB_HAZARD_OFF: u16 = 0x1C;

/// True when `len` bytes from `off` cover [`USB_HAZARD_OFF`].
pub fn covers_hazard(off: u16, len: u16) -> bool {
    off <= USB_HAZARD_OFF && USB_HAZARD_OFF < off.saturating_add(len)
}

/// A USB-block write that refuses any byte range covering offset 0x1C.
pub fn usb_write_guarded(rtl: &Rtl, off: u16, data: &[u8]) -> Result<(), String> {
    if covers_hazard(off, data.len() as u16) {
        return Err(format!(
            "refused: a USB-block write at 0x{off:04X} of {} bytes covers offset 0x1C",
            data.len()
        ));
    }
    rtl.usb_write(off, data).map_err(|e| e.to_string())
}

fn exec(rtl: &Rtl, bulk: &mut BulkCounter, op: &Op) -> J {
    match op {
        Op::Usb(off, data) => {
            let w = usb_write_guarded(rtl, *off, data);
            let back = rtl.usb_read(*off, data.len().max(4) as u16);
            J::obj()
                .with("usb_offset", format!("0x{off:04X}"))
                .with("written", hex_bytes(data))
                .with("error", w.err())
                .with("readback", back.as_ref().ok().map(|b| hex_bytes(b)))
                .with(
                    "matches",
                    back.as_ref().ok().map(|b| b[..data.len()] == data[..]),
                )
        }
        Op::Sys(off, v) => {
            let w = rtl.sys_write8(*off, *v);
            let back = rtl.sys_read8(*off);
            J::obj()
                .with("sys_offset", format!("0x{off:04X}"))
                .with("written", hex8(*v))
                .with("error", w.err().map(|e| e.to_string()))
                .with("readback", back.as_ref().ok().map(|b| hex8(*b)))
                .with("matches", back.ok().map(|b| b == *v))
        }
        Op::Demod(page, off, data) => {
            let w = rtl.demod_write(*page, *off, data);
            let back = rtl.demod_read(*page, *off, data.len() as u16);
            J::obj()
                .with("page", *page)
                .with("offset", hex8(*off))
                .with("written", hex_bytes(data))
                .with("error", w.err().map(|e| e.to_string()))
                .with("readback", back.as_ref().ok().map(|b| hex_bytes(b)))
                .with("matches", back.ok().map(|b| b == *data))
        }
        Op::ClearHalt => {
            let n = bulk.force_clear_halt();
            J::obj()
                .with("clear_feature_endpoint_halt", true)
                .with("bytes_held_by_cancelled_transfers", n)
        }
        Op::Wait(d) => {
            let w = bulk.measure(*d, None);
            J::obj()
                .with("wait_ms", d.as_millis() as u64)
                .with("bytes_in_wait", w.bytes + w.stale_bytes)
                .with("bulk_check", w.check.compact())
        }
    }
}

/// The plan's items in order, a short window after each; the record and
/// the bytes seen in those windows.
fn apply_items(rtl: &Rtl, bulk: &mut BulkCounter, plan: &Plan) -> (J, usize) {
    let mut before = 0;
    let mut items = J::Arr(Vec::new());
    for item in ITEMS.into_iter().filter(|i| !plan.skip.contains(i)) {
        let ops: Vec<J> = item
            .ops(plan)
            .iter()
            .map(|op| exec(rtl, bulk, op))
            .collect();
        let w = bulk.measure(ITEM_WINDOW, None);
        before += w.bytes + w.stale_bytes;
        items.push(
            J::obj()
                .with("item", format!("{item:?}"))
                .with("ops", J::Arr(ops))
                .with("bytes_in_window_after", w.bytes + w.stale_bytes)
                .with("bulk_check", w.check.compact()),
        );
    }
    (items, before)
}

/// A dongle in the stream state (or not, when the plan removes too much).
pub struct Session {
    pub rtl: Rtl,
    pub bulk: BulkCounter,
    pub rec: J,
    pub bytes_before_start: usize,
    pub first_byte_ms: Option<f64>,
}

/// Power-cycle, poll the endpoint, apply the plan's items, write the
/// start; the bulk counter is then re-opened with 16 KiB transfers.
pub fn fresh(plan: &Plan) -> Result<Session, String> {
    let cyc = if crate::no_cycle() {
        J::obj().with(
            "skipped",
            "--no-cycle: the dongle was opened as found, not power-cycled",
        )
    } else {
        usb::power_cycle()?
    };
    let rtl = Rtl::open()?;
    let mut bulk = BulkCounter::open(&rtl, FIRST_BYTE_BUF)?;
    let w0 = bulk.measure(ITEM_WINDOW, None);
    let (items, b) = apply_items(&rtl, &mut bulk, plan);
    let before = w0.bytes + w0.stale_bytes + b;
    let t = Instant::now();
    let start = rtl.demod_write8(START_PAGE, START_OFF, START_VALUE);
    let write_ms = t.elapsed().as_secs_f64() * 1e3;
    let fb = bulk.first_byte(FIRST_BYTE_DEADLINE);
    let back = rtl.demod_read8(START_PAGE, START_OFF);
    let first_byte_ms = fb.latency.map(|d| d.as_secs_f64() * 1e3);
    bulk.close();
    let bulk = BulkCounter::open(&rtl, step3::BUF_LEN)?;
    println!(
        "  plan {}: {before} B before the start; first byte {:?} ms after the start write returned",
        if plan.skip.is_empty() {
            "full".to_string()
        } else {
            format!("without {:?}", plan.skip)
        },
        first_byte_ms.map(|m| (m * 1000.0).round() / 1000.0)
    );
    let rec = J::obj()
        .with("power_cycle", cyc)
        .with("plan", plan.record())
        .with("bulk_polled_from", format!("before the first write: {} transfers of {FIRST_BYTE_BUF} bytes in flight; after the first byte, re-opened with {} transfers of {} bytes", BulkCounter::QUEUED, BulkCounter::QUEUED, step3::BUF_LEN))
        .with("window_before_items", J::obj().with("bytes", w0.bytes + w0.stale_bytes).with("bulk_check", w0.check.compact()))
        .with("items", items)
        .with("bytes_before_start", before)
        .with(
            "start",
            J::obj()
                .with("write", "page 0 0x06 <- 0x80")
                .with("error", start.err().map(|e| e.to_string()))
                .with("control_transfer_ms", write_ms)
                .with("readback", back.ok().map(hex8))
                .with("first_byte_ms_after_write_returned", first_byte_ms)
                .with("bytes_until_first", fb.bytes)
                .with("bulk_check", fb.check.compact()),
        );
    Ok(Session {
        rtl,
        bulk,
        rec,
        bytes_before_start: before,
        first_byte_ms,
    })
}

/// `n` windows of `window` with every byte kept.
pub fn capture(bulk: &mut BulkCounter, n: usize, window: Duration) -> (J, Vec<u8>, f64) {
    let mut ws = J::Arr(Vec::new());
    let mut data = Vec::new();
    let mut secs = 0.0;
    for _ in 0..n {
        let w = bulk.measure(window, Some(&mut data));
        secs += w.elapsed.as_secs_f64();
        ws.push(
            J::obj()
                .with("bytes", w.bytes)
                .with("stale", w.stale_bytes)
                .with("elapsed_s", w.elapsed.as_secs_f64())
                .with("bytes_per_s", w.rate())
                .with("completions", w.completions)
                .with(
                    "sizes_first",
                    J::Arr(w.sizes.iter().take(8).map(|s| J::from(*s)).collect()),
                )
                .with("bulk_check", w.check.compact()),
        );
    }
    (ws, data, secs)
}

/// The stream's content: rate, period score, entropy, I/Q moments,
/// range, clipping.
pub fn summary(data: &[u8], secs: f64, predicted_rate: f64) -> J {
    let c = Content::of(data, secs);
    let min = data.iter().copied().min();
    let max = data.iter().copied().max();
    let clipped = data.iter().filter(|b| **b == 0 || **b == 255).count();
    let rate = c.rate();
    J::obj()
        .with("bytes", data.len())
        .with("secs", secs)
        .with("bytes_per_s", rate)
        .with("predicted_bytes_per_s", 2.0 * predicted_rate)
        .with(
            "ratio_to_predicted",
            if predicted_rate > 0.0 {
                rate / (2.0 * predicted_rate)
            } else {
                0.0
            },
        )
        .with("period_score_3760", c.period_score)
        .with("entropy_bits", c.entropy_bits)
        .with(
            "mean_even_odd",
            J::Arr(vec![J::from(c.mean_u[0]), J::from(c.mean_u[1])]),
        )
        .with(
            "var_even_odd",
            J::Arr(vec![J::from(c.var_u[0]), J::from(c.var_u[1])]),
        )
        .with("even_odd_correlation", correlation(data))
        .with("min", min)
        .with("max", max)
        .with(
            "clipped_fraction",
            clipped as f64 / data.len().max(1) as f64,
        )
}

fn short(data: &[u8], secs: f64) -> String {
    let c = Content::of(data, secs);
    format!(
        "{:.0} B/s, period {:?}, mean {:.2}/{:.2}, var {:.2}/{:.2}, range {:?}-{:?}",
        c.rate(),
        c.period_score.map(|p| (p * 1e4).round() / 1e4),
        c.mean_u[0],
        c.mean_u[1],
        c.var_u[0],
        c.var_u[1],
        data.iter().min(),
        data.iter().max()
    )
}

/// Pearson correlation of even against odd bytes.
fn correlation(data: &[u8]) -> Option<f64> {
    let n = data.len() / 2;
    if n < 2 {
        return None;
    }
    let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0f64, 0f64, 0f64, 0f64, 0f64);
    for k in 0..n {
        let (x, y) = (data[2 * k] as f64, data[2 * k + 1] as f64);
        sx += x;
        sy += y;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
    }
    let nf = n as f64;
    let cov = sxy / nf - sx * sy / (nf * nf);
    let vx = sxx / nf - sx * sx / (nf * nf);
    let vy = syy / nf - sy * sy / (nf * nf);
    (vx > 0.0 && vy > 0.0).then(|| cov / (vx * vy).sqrt())
}

/// Bins excluded around DC when a peak is searched (mean removal leaves
/// the DC bin unreliable).
const DC_GUARD: usize = 2;

fn is_dc(k: usize) -> bool {
    k <= DC_GUARD || k >= FFT_N - DC_GUARD
}

/// Bins a tone's peak may sit from its predicted bin (each 500 Hz at
/// 2.048 Msps): the source and the dongle disagree by a constant +3 bins
/// (+1.5 kHz at 300 MHz, ~5 ppm; data/step10-2b-tone.json). The record
/// keys keep the name "pm2" from the first runs; the tolerance is this.
const PEAK_TOL_BINS: usize = 4;

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2].max(f64::MIN_POSITIVE)
}

/// A tone state's spectrum read as complex I/Q (even bytes I, odd bytes
/// Q): the strongest bin outside DC, its power over the median bin (the
/// SNR), the bin the tone is predicted at and its SNR, and, when an off
/// reference is given, the bin that rose most over it.
pub fn tone_state(data: &[u8], predicted_cycles: Option<f64>, off: Option<&[f64]>, rate: f64) -> J {
    let Some((p, segs, ..)) = confirm_write::spectrum_power(data, true) else {
        return J::from("too few bytes");
    };
    let med = median(&p);
    let peak = (0..FFT_N)
        .filter(|k| !is_dc(*k))
        .max_by(|a, b| {
            p[*a]
                .partial_cmp(&p[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let db = |x: f64| 10.0 * (x / med).log10();
    let mut j = J::obj()
        .with("fft_n", FFT_N)
        .with("segments", segs)
        .with("peak_bin", peak)
        .with("peak_cycles_per_sample", confirm_write::cycles(peak))
        .with("peak_hz_at_rate", confirm_write::cycles(peak) * rate)
        .with("peak_snr_db_over_median", db(p[peak]));
    if let Some(c) = predicted_cycles {
        let k = ((c * FFT_N as f64).round() as i64).rem_euclid(FFT_N as i64) as usize;
        let best = (k.saturating_sub(PEAK_TOL_BINS)..=(k + PEAK_TOL_BINS).min(FFT_N - 1))
            .max_by(|a, b| {
                p[*a]
                    .partial_cmp(&p[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(k);
        let mirror = (FFT_N - k) % FFT_N;
        j.set("predicted_cycles_per_sample", c);
        j.set("predicted_bin", k);
        j.set("snr_db_at_predicted_bin_pm2", db(p[best]));
        j.set("snr_db_at_mirror_bin", db(p[mirror]));
        j.set(
            "peak_is_predicted_bin_pm2",
            (peak as i64 - k as i64).abs() <= PEAK_TOL_BINS as i64,
        );
    }
    if let Some(r) = off {
        let rise =
            |k: usize| 10.0 * (p[k].max(f64::MIN_POSITIVE) / r[k].max(f64::MIN_POSITIVE)).log10();
        let top = (0..FFT_N)
            .filter(|k| !is_dc(*k))
            .max_by(|a, b| {
                rise(*a)
                    .partial_cmp(&rise(*b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(0);
        j.set("max_rise_bin", top);
        j.set("max_rise_cycles_per_sample", confirm_write::cycles(top));
        j.set("max_rise_db_over_off", rise(top));
    }
    j
}

/// The band shape of a state: 32 bands across -0.5..0.5 cycles/sample
/// (complex), each the mean power per bin in dB (absolute, same scale
/// across states).
pub fn bands(data: &[u8]) -> Option<Vec<f64>> {
    const BANDS: usize = 32;
    let (p, ..) = confirm_write::spectrum_power(data, true)?;
    let per = FFT_N / BANDS;
    // Bands in ascending frequency: bins N/2..N (negative), then 0..N/2.
    let order: Vec<usize> = (FFT_N / 2..FFT_N).chain(0..FFT_N / 2).collect();
    Some(
        order
            .chunks(per)
            .map(|c| {
                let m =
                    c.iter().filter(|k| !is_dc(**k)).map(|k| p[*k]).sum::<f64>() / c.len() as f64;
                (10.0 * m.max(f64::MIN_POSITIVE).log10() * 10.0).round() / 10.0
            })
            .collect(),
    )
}

fn bands_j(b: &Option<Vec<f64>>) -> J {
    match b {
        Some(v) => J::Arr(v.iter().map(|x| J::from(*x)).collect()),
        None => J::Null,
    }
}

// ---- The tuner, from tuner.rs (findings 4) ----

/// The RF the tone tests centre on: 300 MHz, where findings 4e received
/// the tone (SEL_DIV 010 /8).
pub const TONE_RF_HZ: f64 = 300e6;
pub const TONE_SEL_DIV: u8 = 0b010;
/// Tone offsets from the DDC's centre RF (test 2b): both signs, inside
/// the observed filter's band and near its edge.
pub const TONE_OFFSETS_HZ: [f64; 6] = [100e3, 300e3, -200e3, -500e3, 700e3, -900e3];
/// The default tone level (relative): low enough not to clip the 8-bit
/// stream with the default manual gains; `--level` overrides.
pub const DEFAULT_LEVEL: Level = Level {
    txvga_db: 0,
    backoff_db: -20,
};
/// Tone offsets in the output's first and second alias zones (beyond
/// +-1.024 MHz at 2.048 Msps), for the coefficient comparison.
pub const ALIAS_OFFSETS_HZ: [f64; 3] = [1.5e6, -2.0e6, 2.5e6];

/// The tuner's PLL for the observed DDC IF, high side (findings 4h).
pub fn lo_for_rf(rf: f64) -> f64 {
    rf + iffreq_hz(OBSERVED_IFFREQ)
}

/// Open the I2C repeater (page 1 0x01 bit 3, Table 3), write the image
/// (CLK_OUT_ENB 0, findings 4a), read the status, close the repeater.
pub fn program_tuner(rtl: &Rtl, front: &Front, pll: &Pll) -> J {
    let open = rtl.demod_rmw(
        IIC_REPEAT_PAGE,
        IIC_REPEAT_OFF,
        IIC_REPEAT_BIT_TABLE3,
        IIC_REPEAT_BIT_TABLE3,
    );
    let before = tuner::read_status(rtl);
    let img = tuner::image(front, pll);
    let applied = tuner::apply_image(rtl, &img);
    std::thread::sleep(Duration::from_millis(10));
    let status = tuner::read_status(rtl);
    let close = rtl.demod_rmw(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT_TABLE3, 0);
    J::obj()
        .with(
            "repeater_open",
            open.map(|(a, b)| J::obj().with("before", hex8(a)).with("after", hex8(b)))
                .unwrap_or_else(|e| J::from(e.to_string())),
        )
        .with(
            "status_before",
            before
                .map(|s| tuner::status_record(&s))
                .unwrap_or_else(|e| J::from(e.to_string())),
        )
        .with("front", front.record())
        .with("pll", pll.record())
        .with("applied", applied)
        .with(
            "status_after_10_ms",
            status
                .map(|s| tuner::status_record(&s))
                .unwrap_or_else(|e| J::from(e.to_string())),
        )
        .with(
            "repeater_close",
            close
                .map(|(a, b)| J::obj().with("before", hex8(a)).with("after", hex8(b)))
                .unwrap_or_else(|e| J::from(e.to_string())),
        )
}

/// Options shared by the tests.
#[derive(Clone, Debug)]
pub struct Opts {
    pub level: Level,
    pub front: Front,
    pub windows: usize,
}

fn parse(args: &[String]) -> Result<(String, Opts), String> {
    let mut front = Config::Manual.front();
    let mut level = DEFAULT_LEVEL;
    let mut windows = 10;
    let mut test = None;
    let mut i = 0;
    let int = |s: Option<&String>| -> Result<i32, String> {
        s.ok_or(USAGE)?.parse().map_err(|e| format!("{e}: {USAGE}"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--level" => {
                level = Level {
                    txvga_db: int(args.get(i + 1))?,
                    backoff_db: int(args.get(i + 2))?,
                };
                i += 2;
            }
            "--gain" => {
                front.lna_gain = int(args.get(i + 1))? as u8;
                front.mix_gain = int(args.get(i + 2))? as u8;
                front.vga_code = int(args.get(i + 3))? as u8;
                i += 3;
            }
            "--windows" => {
                windows = int(args.get(i + 1))? as usize;
                i += 1;
            }
            a if test.is_none() => test = Some(a.to_string()),
            _ => return Err(USAGE.into()),
        }
        i += 1;
    }
    Ok((
        test.ok_or(USAGE)?,
        Opts {
            level,
            front,
            windows,
        },
    ))
}

pub fn run(args: &[String]) -> Result<(), String> {
    usb::guard()?;
    let (test, o) = parse(args)?;
    match test.as_str() {
        "replay" => replay(&o),
        "rate" => rate(&o),
        "pause" => pause(&o),
        "tone" => tone(&o),
        "coeff-probe" => coeff_probe(&o),
        "coeff-response" => coeff_response(&o),
        "coeffs" => coeffs(&o),
        "minimise" => minimise(&o),
        "minimal" => minimal(&o),
        "steady" => steady(&o),
        "seldiv" => seldiv(&o),
        _ => Err(USAGE.into()),
    }
}

fn header(test: &str, what: &str, o: &Opts) -> J {
    crate::header_with_source(&format!("10-stream-confirm-{test}"), what)
        .with("source_of_the_sequence", "findings.md step 9 (9b, 9h): the vendor driver's writes as observed on the USB wire; the coefficient values page 1 0x1C-0x2F are written as a test fixture of the observed state only")
        .with("tuner_front", o.front.record())
        .with("tone_level", J::obj().with("txvga_db", o.level.txvga_db).with("backoff_db", o.level.backoff_db))
        .with("content_legend", "even bytes = I, odd bytes = Q (a reading, tested by the tone's sign); spectra: Hann, FFT 4096, mean removed, up to 64 segments")
}

/// Test 1: the observed sequence from a power cycle.
fn replay(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let mut s = fresh(&plan)?;
    let (ws, data, secs) = capture(&mut s.bulk, o.windows, step3::WINDOW);
    println!("  stream: {}", short(&data, secs));
    let sum = summary(&data, secs, output_rate(plan.rsamp_ratio));
    let analysis = confirm_write::analyse(&data);
    s.bulk.close();
    let rec = header("replay", "test 1: the vendor's stream-start sequence (findings 9h steps 1-4) replayed from a power cycle; bytes before the start write; first-byte latency; rate and content after it (tuner not programmed)", o)
        .with("session", s.rec)
        .with("windows_after_start", ws)
        .with("summary", sum)
        .with("analysis", analysis)
        .with("expected_by_the_capture", "0 bytes before the start; first data ~0.55 ms after it; ~4.1 MB/s; bytes 124-128 around 126.5 with the tuner idle; period score far below 1");
    crate::write_record("step10-1-replay.json", &rec);
    Ok(())
}

/// Test 2a: rsamp_ratio x1, x2, x1/2 and the 2.4 Msps value, each from a
/// power cycle; then, in the x1 session, the ratio changed live.
fn rate(o: &Opts) -> Result<(), String> {
    // x1, x2, x1/2, the 2.4 Msps value, then outputs between 2.4 and the
    // x1/2 ratio's 4.096 Msps to bracket where tracking ends; 2.88 and
    // 3.2 Msps three times each (3.2 Msps gave 6.40 MB/s in one run and
    // 3.20 MB/s in the next).
    let ratios = [
        OBSERVED_RSAMP_RATIO,
        OBSERVED_RSAMP_RATIO * 2,
        OBSERVED_RSAMP_RATIO / 2,
        rsamp_ratio(2.4e6 * OBSERVED_DECIMATION),
        rsamp_ratio(2.88e6 * OBSERVED_DECIMATION),
        rsamp_ratio(3.2e6 * OBSERVED_DECIMATION),
        rsamp_ratio(3.6e6 * OBSERVED_DECIMATION),
        rsamp_ratio(2.88e6 * OBSERVED_DECIMATION),
        rsamp_ratio(3.2e6 * OBSERVED_DECIMATION),
        rsamp_ratio(2.88e6 * OBSERVED_DECIMATION),
        rsamp_ratio(3.2e6 * OBSERVED_DECIMATION),
        OBSERVED_RSAMP_RATIO,
    ];
    let mut runs = J::Arr(Vec::new());
    for (n, r) in ratios.into_iter().enumerate() {
        let plan = Plan {
            rsamp_ratio: r,
            ..Plan::observed()
        };
        let mut s = fresh(&plan)?;
        let (ws, data, secs) = capture(&mut s.bulk, o.windows, step3::WINDOW);
        let sum = summary(&data, secs, output_rate(r));
        println!(
            "  ratio 0x{r:07X} (f_symbol {:.0} Hz, predicted {:.0} B/s): {}",
            f_symbol(r),
            2.0 * output_rate(r),
            short(&data, secs)
        );
        let mut run = J::obj()
            .with("rsamp_ratio", hex32(r))
            .with("session", s.rec)
            .with("windows", ws)
            .with("summary", sum);
        if n == 0 {
            // The ratio changed while streaming, then restored.
            // Three ways, each twice, the ratio alternating x2 / x1 so a
            // change that takes effect halves or doubles the rate: a plain
            // write; the write inside a pause (page 1 0x01 bit 2, 9d); the
            // write followed by the start bit cleared and set again.
            let mut live = J::Arr(Vec::new());
            let methods = [
                "plain",
                "plain",
                "pause",
                "pause",
                "start-toggle",
                "start-toggle",
            ];
            for (m, method) in methods.into_iter().enumerate() {
                let lr = if m % 2 == 0 {
                    OBSERVED_RSAMP_RATIO * 2
                } else {
                    OBSERVED_RSAMP_RATIO
                };
                let b = rsamp_bytes(lr);
                // A stalled write is retried once (seen once: both halves
                // stalled while streaming); every attempt is recorded.
                let mut attempts = Vec::new();
                let mut back = s.rtl.demod_read(RSAMP_PAGE, RSAMP_MSB, 4);
                let mut extra = Vec::new();
                if method == "pause" {
                    extra.push(J::from(format!(
                        "pause set: {:?}",
                        s.rtl.demod_rmw(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, PAUSE_BIT)
                    )));
                }
                for _ in 0..2 {
                    let w1 = s.rtl.demod_write(RSAMP_PAGE, RSAMP_MSB, &b[..2]);
                    let w2 = s.rtl.demod_write(RSAMP_PAGE, RSAMP_MSB + 2, &b[2..]);
                    back = s.rtl.demod_read(RSAMP_PAGE, RSAMP_MSB, 4);
                    let ok = back.as_ref().is_ok_and(|r| r[..] == b[..]);
                    attempts.push(J::Arr(
                        [w1, w2]
                            .into_iter()
                            .map(|w| J::from(w.err().map(|e| e.to_string())))
                            .collect(),
                    ));
                    if ok {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                match method {
                    "pause" => extra.push(J::from(format!(
                        "pause cleared: {:?}",
                        s.rtl.demod_rmw(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, 0)
                    ))),
                    "start-toggle" => {
                        extra.push(J::from(format!(
                            "start bit cleared: {:?}",
                            s.rtl.demod_rmw(START_PAGE, START_OFF, START_VALUE, 0)
                        )));
                        extra.push(J::from(format!(
                            "start bit set: {:?}",
                            s.rtl
                                .demod_rmw(START_PAGE, START_OFF, START_VALUE, START_VALUE)
                        )));
                    }
                    _ => {}
                }
                s.bulk.drain();
                let (lws, ld, lsecs) = capture(&mut s.bulk, 5, step3::WINDOW);
                println!("    live ({method}) -> 0x{lr:07X}: {}", short(&ld, lsecs));
                live.push(
                    J::obj()
                        .with("method", method)
                        .with("controls", J::Arr(extra))
                        .with("rsamp_ratio", hex32(lr))
                        .with("attempts_errors", J::Arr(attempts))
                        .with("readback", back.ok().map(|b| hex_bytes(&b)))
                        .with("windows", lws)
                        .with("summary", summary(&ld, lsecs, output_rate(lr))),
                );
            }
            run.set("live_changes", live);
        }
        s.bulk.close();
        runs.push(run);
    }
    let rec = header("rate", "test 2a: the rate tracks rsamp_ratio — the observed 0xE10000, x2, x1/2 and the 2.4 Msps value (f_symbol 9.6 MHz), each from a power cycle; predicted bytes/s = 2 x f_symbol / 4 (findings 9f)", o).with("runs", runs);
    crate::write_record("step10-2a-rate.json", &rec);
    Ok(())
}

/// Test 2c: page 1 0x01 bit 2 set and cleared while streaming.
fn pause(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let mut s = fresh(&plan)?;
    let rate = output_rate(plan.rsamp_ratio);
    let (wb, db, sb) = capture(&mut s.bulk, 3, step3::WINDOW);
    let short_w = Duration::from_millis(10);
    let set = s.rtl.demod_rmw(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, PAUSE_BIT);
    let (wp, dp, sp) = capture(&mut s.bulk, 10, short_w);
    let clear = s.rtl.demod_rmw(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, 0);
    let (wr, dr, sr) = capture(&mut s.bulk, 10, short_w);
    let (wa, da, sa) = capture(&mut s.bulk, 3, step3::WINDOW);
    println!(
        "  before: {}\n  paused: {} B in 10 x 10 ms\n  resumed: {} B in 10 x 10 ms; after: {}",
        short(&db, sb),
        dp.len(),
        dr.len(),
        short(&da, sa)
    );
    s.bulk.close();
    let rmw = |r: usb::R<(u8, u8)>| {
        r.map(|(a, b)| J::obj().with("before", hex8(a)).with("after", hex8(b)))
            .unwrap_or_else(|e| J::from(e.to_string()))
    };
    let rec = header("pause", "test 2c: page 1 0x01 bit 2 (findings 9d) set while streaming, ten 10 ms windows, cleared, ten 10 ms windows", o)
        .with("session", s.rec)
        .with("before", J::obj().with("windows", wb).with("summary", summary(&db, sb, rate)))
        .with("set_bit2", rmw(set))
        .with("paused", J::obj().with("windows", wp).with("bytes", dp.len()).with("secs", sp))
        .with("clear_bit2", rmw(clear))
        .with("resumed", J::obj().with("windows", wr).with("bytes", dr.len()).with("secs", sr))
        .with("after", J::obj().with("windows", wa).with("summary", summary(&da, sa, rate)));
    crate::write_record("step10-2c-pause.json", &rec);
    Ok(())
}

/// Test 2b: the tuner programmed (findings 4: image, PLL at 300 MHz RF +
/// the observed IF, high side), the tone off, at offsets, off again.
fn tone(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let rate = output_rate(plan.rsamp_ratio);
    let mut src = Source::open()?;
    src.off()?;
    let mut s = fresh(&plan)?;
    let (_, idle, idle_s) = capture(&mut s.bulk, 3, step3::WINDOW);
    println!("  tuner idle: {}", short(&idle, idle_s));
    let pll = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?;
    let tun = program_tuner(&s.rtl, &o.front, &pll);
    let centre = pll.lo_hz().unwrap_or(0.0) - iffreq_hz(OBSERVED_IFFREQ);
    s.bulk.drain();
    let (w_off, off, off_s) = capture(&mut s.bulk, o.windows, step3::WINDOW);
    println!("  tuner programmed, tone off: {}", short(&off, off_s));
    let off_p = confirm_write::spectrum_power(&off, true).map(|x| x.0);
    let mut states = J::Arr(Vec::new());
    for ofs in TONE_OFFSETS_HZ {
        let st = src.level(centre + ofs, o.level)?;
        s.bulk.drain();
        let (w, d, secs) = capture(&mut s.bulk, 5, step3::WINDOW);
        let ts = tone_state(&d, Some(ofs / rate), off_p.as_deref(), rate);
        println!(
            "  tone {:+.0} Hz: {} | {}",
            ofs,
            short(&d, secs),
            ts.render()
        );
        states.push(
            J::obj()
                .with("offset_hz", ofs)
                .with("rf_hz", centre + ofs)
                .with("source", st)
                .with("windows", w)
                .with("summary", summary(&d, secs, rate))
                .with("spectrum", ts),
        );
    }
    let st = src.off()?;
    s.bulk.drain();
    let (w_off2, off2, off2_s) = capture(&mut s.bulk, 5, step3::WINDOW);
    let null = tone_state(&off2, None, off_p.as_deref(), rate);
    src.quit();
    s.bulk.close();
    let rec = header("tone", "test 2b: the stream with the tuner programmed and the tone keyed at offsets from the DDC's centre RF; a line must rise at offset / rate cycles per sample (even bytes I, odd Q) and move with the tone", o)
        .with("session", s.rec)
        .with("tuner_idle", summary(&idle, idle_s, rate))
        .with("tuner", tun)
        .with("ddc_centre_rf_hz", centre)
        .with("output_rate_hz", rate)
        .with("tone_off", J::obj().with("windows", w_off).with("summary", summary(&off, off_s, rate)).with("spectrum", tone_state(&off, None, None, rate)).with("bands_db", bands_j(&bands(&off))))
        .with("tone_states", states)
        .with("tone_off_again", J::obj().with("source", st).with("windows", w_off2).with("summary", summary(&off2, off2_s, rate)).with("spectrum_null", null));
    crate::write_record("step10-2b-tone.json", &rec);
    Ok(())
}

/// Write the 20 coefficient registers, one byte each; the errors and
/// whether the read-back matches.
fn write_coeffs(rtl: &Rtl, c: &[u8; COEFF_COUNT]) -> J {
    let mut errs = Vec::new();
    for (k, v) in c.iter().enumerate() {
        if let Err(e) = rtl.demod_write8(COEFF_PAGE, COEFF_FIRST + k as u8, *v) {
            errs.push(J::from(format!("0x{:02X}: {e}", COEFF_FIRST + k as u8)));
        }
    }
    let back = rtl.demod_read(COEFF_PAGE, COEFF_FIRST, COEFF_COUNT as u16);
    J::obj()
        .with("errors", J::Arr(errs))
        .with(
            "readback_matches",
            back.as_ref().ok().map(|b| b[..] == c[..]),
        )
        .with("readback", back.ok().map(|b| hex_bytes(&b)))
}

/// Test 3, the layout: with the stream running and the tuner programmed,
/// the 20 registers written live — the observed values, all zero, then
/// one register at a time non-zero (values 0x01, 0x04, 0x10, 0x40, 0x80)
/// with the rest zero — and per variant the output's power and its band
/// shape (tuner noise, tone off), plus a tone at a fixed offset.
fn coeff_probe(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let rate = output_rate(plan.rsamp_ratio);
    let mut s = fresh(&plan)?;
    let pll = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?;
    let tun = program_tuner(&s.rtl, &o.front, &pll);
    let mut variants: Vec<(String, [u8; COEFF_COUNT])> = vec![
        ("observed".into(), OBSERVED_COEFFS_FIXTURE),
        ("zeros".into(), [0; COEFF_COUNT]),
        ("observed again".into(), OBSERVED_COEFFS_FIXTURE),
    ];
    for k in 0..COEFF_COUNT {
        for v in [0x01u8, 0x04, 0x10, 0x40, 0x80] {
            let mut c = [0u8; COEFF_COUNT];
            c[k] = v;
            variants.push((format!("0x{:02X}={v:02X}", COEFF_FIRST + k as u8), c));
        }
    }
    variants.push(("observed last".into(), OBSERVED_COEFFS_FIXTURE));
    let mut out = J::Arr(Vec::new());
    for (name, c) in &variants {
        source::check()?;
        let w = write_coeffs(&s.rtl, c);
        std::thread::sleep(Duration::from_millis(20));
        s.bulk.drain();
        let (_, d, secs) = capture(&mut s.bulk, 3, step3::WINDOW);
        let b = bands(&d);
        let cc = Content::of(&d, secs);
        println!(
            "  {name:>14}: var {:8.2}/{:8.2} mean {:6.2}/{:6.2} bands {}",
            cc.var_u[0],
            cc.var_u[1],
            cc.mean_u[0],
            cc.mean_u[1],
            b.as_ref()
                .map(|v| v
                    .iter()
                    .map(|x| format!("{x:.0}"))
                    .collect::<Vec<_>>()
                    .join(" "))
                .unwrap_or_default()
        );
        out.push(
            J::obj()
                .with("variant", name.as_str())
                .with("coeffs", hex_bytes(c))
                .with("write", w)
                .with("summary", summary(&d, secs, rate))
                .with("bands_db", bands_j(&b)),
        );
    }
    s.bulk.close();
    let rec = header("coeff-probe", "test 3 (layout): page 1 0x1C-0x2F written live while streaming with the tuner programmed (tone off: the tuner's noise is the stimulus); per variant the output's moments and 32-band spectrum (dB, ascending frequency -0.5..0.5 cycles/sample)", o)
        .with("session", s.rec)
        .with("tuner", tun)
        .with("variants", out);
    crate::write_record("step10-3a-coeff-probe.json", &rec);
    Ok(())
}

/// The layout the coefficient probe gave (data/step10-3a-coeff-probe.json,
/// bit weights against the tuner's noise): page 1 0x1C-0x23 hold eight
/// 8-bit fields c0-c7; 0x24-0x2F hold eight 12-bit fields c8-c15, two per
/// three bytes, MSB first: c(2p+8) = byte0 << 4 | byte1 >> 4, c(2p+9) =
/// (byte1 & 0x0F) << 8 | byte2. The weights line up with one LSB weight
/// for both widths (0x24 bit 4 = c8 bit 8 and 0x25 bit 0 = c9 bit 8 give
/// the same power; an 8-bit field's bit 7 the power of a 12-bit field's
/// bit 7). Signed two's complement in each width: the sign tests of
/// `coeff-response` decide it.
pub const TAPS: usize = 16;
pub const NARROW_TAPS: usize = 8;

/// Encode 16 taps into the 20 registers (8-bit fields, then 12-bit pairs).
pub fn encode(c: &[i32; TAPS]) -> [u8; COEFF_COUNT] {
    let mut out = [0u8; COEFF_COUNT];
    for k in 0..NARROW_TAPS {
        out[k] = c[k] as i8 as u8;
    }
    for p in 0..(TAPS - NARROW_TAPS) / 2 {
        let a = (c[NARROW_TAPS + 2 * p] & 0xFFF) as u32;
        let b = (c[NARROW_TAPS + 2 * p + 1] & 0xFFF) as u32;
        let o = NARROW_TAPS + 3 * p;
        out[o] = (a >> 4) as u8;
        out[o + 1] = ((a & 0xF) << 4 | b >> 8) as u8;
        out[o + 2] = b as u8;
    }
    out
}

/// Decode the 20 registers into 16 signed taps (the inverse of [`encode`]).
pub fn decode(r: &[u8; COEFF_COUNT]) -> [i32; TAPS] {
    let mut c = [0i32; TAPS];
    for k in 0..NARROW_TAPS {
        c[k] = r[k] as i8 as i32;
    }
    let s12 = |v: u32| {
        if v & 0x800 != 0 {
            v as i32 - 0x1000
        } else {
            v as i32
        }
    };
    for p in 0..(TAPS - NARROW_TAPS) / 2 {
        let o = NARROW_TAPS + 3 * p;
        c[NARROW_TAPS + 2 * p] = s12((r[o] as u32) << 4 | (r[o + 1] as u32) >> 4);
        c[NARROW_TAPS + 2 * p + 1] = s12(((r[o + 1] as u32) & 0xF) << 8 | r[o + 2] as u32);
    }
    c
}

/// One tap set alone.
fn single(k: usize, v: i32) -> [i32; TAPS] {
    let mut c = [0; TAPS];
    c[k] = v;
    c
}

/// Our own low-pass, designed here (not from any table): a Hamming-
/// windowed sinc of [`OWN_LENGTH`] taps, symmetric, the 16 fields holding
/// one half from the outermost (c0) to the one beside the centre (c15).
/// ASSUMPTIONS, stated because the bench did not settle them (findings
/// step 10, data/step10-3b-coeff-response.json): the taps run at the ADC
/// rate [`OWN_FS_HZ`] (the DDC's programmable low-pass of [RTL2832U] 7.3
/// sits before the resampler), the length is even (no single centre tap),
/// and the DC gain is [`OWN_DC_GAIN`] (a power of two of our choosing).
/// The cutoff passes the 2.048 Msps output's +-1.024 MHz with margin.
pub const OWN_FS_HZ: f64 = XTAL_HZ;
pub const OWN_CUTOFF_HZ: f64 = 1.2e6;
pub const OWN_LENGTH: usize = 2 * TAPS;
pub const OWN_DC_GAIN: f64 = 4096.0;

/// The design's 16 stored taps (one half; the other mirrors it).
pub fn own_lowpass_taps() -> [i32; TAPS] {
    let x = 2.0 * OWN_CUTOFF_HZ / OWN_FS_HZ;
    let centre = (OWN_LENGTH as f64 - 1.0) / 2.0;
    let h: Vec<f64> = (0..TAPS)
        .map(|k| {
            let d = centre - k as f64;
            let t = std::f64::consts::PI * x * d;
            let sinc = if t == 0.0 { 1.0 } else { t.sin() / t };
            let w =
                0.54 - 0.46 * (std::f64::consts::TAU * k as f64 / (OWN_LENGTH as f64 - 1.0)).cos();
            x * sinc * w
        })
        .collect();
    let sum: f64 = 2.0 * h.iter().sum::<f64>();
    let mut c = [0i32; TAPS];
    for k in 0..TAPS {
        c[k] = (h[k] / sum * OWN_DC_GAIN).round() as i32;
    }
    c
}

/// The design in the register layout.
pub fn own_lowpass() -> [u8; COEFF_COUNT] {
    encode(&own_lowpass_taps())
}

/// An offset's position in the output after decimation (cycles per
/// sample, folded into -0.5..0.5).
fn folded(ofs: f64, rate: f64) -> f64 {
    let c = ofs / rate;
    c - c.round()
}

/// The tone's offsets from the DDC centre RF for the response: from, to,
/// step (Hz). Beyond +-1.024 MHz the tone reaches the output only as an
/// alias, which the strongest bin still measures.
pub const RESPONSE_SWEEP_HZ: (f64, f64, f64) = (-5.0e6, 5.0e6, 0.05e6);
/// The single-tap value for the 12-bit fields: well above the output the
/// all-zero set still carries (the first run: a 512 tap only ~14 dB over
/// it in band).
pub const RESPONSE_TAP: i32 = 1024;

/// Write a tap set, retrying once if the read-back differs (writes made
/// while streaming stall now and then: data/step10-2a-rate.json, the
/// first coeff-response run).
fn write_coeffs_checked(rtl: &Rtl, regs: &[u8; COEFF_COUNT]) -> J {
    let first = write_coeffs(rtl, regs);
    if first.render().contains("\"readback_matches\":true") {
        return first;
    }
    std::thread::sleep(Duration::from_millis(20));
    J::obj()
        .with("first", first)
        .with("retry", write_coeffs(rtl, regs))
}

/// The tap sets the response compares: the fixture, all zero, single taps
/// across the layout, and sum / difference pairs for the sign question.
fn response_sets() -> Vec<(String, [u8; COEFF_COUNT])> {
    let mut sets: Vec<(String, [u8; COEFF_COUNT])> = vec![
        ("observed (fixture)".into(), OBSERVED_COEFFS_FIXTURE),
        ("all zero".into(), [0; COEFF_COUNT]),
    ];
    for k in (NARROW_TAPS..TAPS).rev() {
        sets.push((
            format!("c{k} = +{RESPONSE_TAP} alone"),
            encode(&single(k, RESPONSE_TAP)),
        ));
    }
    for k in [7, 0] {
        sets.push((format!("c{k} = +127 alone"), encode(&single(k, 127))));
    }
    for k in [14, 8] {
        let mut pair = single(15, RESPONSE_TAP);
        pair[k] = -RESPONSE_TAP;
        sets.push((
            format!("c15 = +{RESPONSE_TAP}, c{k} = -{RESPONSE_TAP}"),
            encode(&pair),
        ));
    }
    sets
}

/// Test 3 (the filter's response): the tone stepped across
/// RESPONSE_SWEEP_HZ around the DDC centre; at each tone frequency every
/// tap set written live in turn and the tone's output power measured
/// (strongest non-DC bin, absolute dB). Single taps show where their
/// nulls fall; the pairs show how two fields combine. Then a two-point
/// check of a live pset_iffreq write (whether the DDC follows it).
fn coeff_response(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let rate = output_rate(plan.rsamp_ratio);
    let if_obs = iffreq_hz(OBSERVED_IFFREQ);
    let mut src = Source::open()?;
    src.off()?;
    let mut s = fresh(&plan)?;
    let pll = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?;
    let tun = program_tuner(&s.rtl, &o.front, &pll);
    let centre = pll.lo_hz().unwrap_or(0.0) - if_obs;
    let sets = response_sets();
    let mut per_set: Vec<(Vec<J>, Vec<f64>)> =
        sets.iter().map(|_| (Vec::new(), Vec::new())).collect();
    let mut rewrites = J::Arr(Vec::new());
    let mut d = RESPONSE_SWEEP_HZ.0;
    while d <= RESPONSE_SWEEP_HZ.1 + 1.0 {
        source::check()?;
        src.level(centre + d, o.level)?;
        for (n, (name, regs)) in sets.iter().enumerate() {
            let w = write_coeffs_checked(&s.rtl, regs);
            if !w.render().starts_with("{\"errors\":[]") {
                rewrites.push(
                    J::obj()
                        .with("delta_hz", d)
                        .with("set", name.as_str())
                        .with("write", w),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
            s.bulk.drain();
            let (_, data, _) = capture(&mut s.bulk, 1, Duration::from_millis(50));
            if let Some((p, ..)) = confirm_write::spectrum_power(&data, true) {
                let peak = (0..FFT_N)
                    .filter(|k| !is_dc(*k))
                    .max_by(|a, b| {
                        p[*a]
                            .partial_cmp(&p[*b])
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap_or(0);
                let pk = 10.0 * p[peak].max(f64::MIN_POSITIVE).log10();
                let md = 10.0 * median(&p).log10();
                per_set[n].1.push(pk);
                per_set[n].0.push(
                    J::obj()
                        .with("delta_hz", d)
                        .with("peak_bin", peak)
                        .with("peak_hz", confirm_write::cycles(peak) * rate)
                        .with("peak_db", (pk * 100.0).round() / 100.0)
                        .with("median_db", (md * 100.0).round() / 100.0),
                );
            }
        }
        d += RESPONSE_SWEEP_HZ.2;
    }
    let mut out = J::Arr(Vec::new());
    for ((name, regs), (pts, line)) in sets.iter().zip(per_set) {
        println!(
            "  {name:>24}: {}",
            line.iter()
                .map(|x| format!("{x:.0}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        out.push(
            J::obj()
                .with("set", name.as_str())
                .with("registers", hex_bytes(regs))
                .with(
                    "taps",
                    J::Arr(decode(regs).iter().map(|x| J::from(*x)).collect()),
                )
                .with("points", J::Arr(pts)),
        );
    }
    // Live pset_iffreq: the observed taps, the tone at the centre + 300 kHz,
    // the DDC moved by +500 kHz; a DDC that follows moves the line.
    write_coeffs_checked(&s.rtl, &OBSERVED_COEFFS_FIXTURE);
    src.level(centre + 300e3, o.level)?;
    let mut live = J::Arr(Vec::new());
    for shift in [0.0, 500e3, 0.0] {
        let b = tuner::pset_iffreq_bytes(if_obs + shift);
        let we = s
            .rtl
            .demod_write(
                IFFREQ_PAGE,
                IFFREQ_MSB,
                &if shift == 0.0 { OBSERVED_IFFREQ } else { b },
            )
            .err();
        let back = s.rtl.demod_read(IFFREQ_PAGE, IFFREQ_MSB, 3);
        std::thread::sleep(Duration::from_millis(20));
        s.bulk.drain();
        let (_, data, _) = capture(&mut s.bulk, 2, step3::WINDOW);
        let ts = tone_state(&data, Some(300e3 / rate), None, rate);
        println!("  live pset_iffreq shift {shift:+.0}: {}", ts.render());
        live.push(
            J::obj()
                .with("ddc_shift_hz", shift)
                .with("error", we.map(|e| e.to_string()))
                .with("readback", back.ok().map(|b| hex_bytes(&b)))
                .with("spectrum", ts),
        );
    }
    src.quit();
    s.bulk.close();
    let rec = header("coeff-response", "test 3 (the filter): the tone stepped -5..+5 MHz (0.05 MHz) around the observed DDC centre RF; at each step every tap set (encode(): the probe's layout) written live and the strongest non-DC output bin measured (absolute dB, 50 ms); then a live pset_iffreq write checked against the tone's line", o)
        .with("session", s.rec)
        .with("tuner", tun)
        .with("ddc_centre_rf_hz", centre)
        .with("sets", out)
        .with("writes_retried_or_failed", rewrites)
        .with("live_pset_iffreq", live);
    crate::write_record("step10-3b-coeff-response.json", &rec);
    Ok(())
}

/// Test 3: the stream from a power cycle with the observed values, our
/// own low-pass and all zeros; the tuner programmed; noise and a tone.
fn coeffs(o: &Opts) -> Result<(), String> {
    let rate = output_rate(OBSERVED_RSAMP_RATIO);
    let mut src = Source::open()?;
    src.off()?;
    let mut runs = J::Arr(Vec::new());
    for (name, c) in [
        ("observed (fixture)", OBSERVED_COEFFS_FIXTURE),
        ("own low-pass", own_lowpass()),
        ("all zero", [0u8; COEFF_COUNT]),
    ] {
        src.off()?;
        let plan = Plan {
            coeffs: c,
            ..Plan::observed()
        };
        let mut s = fresh(&plan)?;
        let (_, idle, idle_s) = capture(&mut s.bulk, 3, step3::WINDOW);
        let pll = Pll::program(
            lo_for_rf(TONE_RF_HZ),
            TONE_SEL_DIV,
            tuner::REF_DIV2_BY_DOCUMENT,
        )?;
        let tun = program_tuner(&s.rtl, &o.front, &pll);
        let centre = pll.lo_hz().unwrap_or(0.0) - iffreq_hz(OBSERVED_IFFREQ);
        s.bulk.drain();
        let (_, off, off_s) = capture(&mut s.bulk, 5, step3::WINDOW);
        let off_p = confirm_write::spectrum_power(&off, true).map(|x| x.0);
        let mut states = J::Arr(Vec::new());
        for ofs in TONE_OFFSETS_HZ.into_iter().chain(ALIAS_OFFSETS_HZ) {
            let st = src.level(centre + ofs, o.level)?;
            s.bulk.drain();
            let (_, d, secs) = capture(&mut s.bulk, 3, step3::WINDOW);
            let ts = tone_state(&d, Some(folded(ofs, rate)), off_p.as_deref(), rate);
            let snr = |k: &str| match &ts {
                J::Obj(f) => f
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.render())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            println!(
                "  {name}: tone {ofs:+.0}: peak bin {} (predicted {}), SNR at predicted {} dB, var {:.1}",
                snr("peak_bin"),
                snr("predicted_bin"),
                snr("snr_db_at_predicted_bin_pm2"),
                Content::of(&d, secs).var_u[0]
            );
            states.push(
                J::obj()
                    .with("offset_hz", ofs)
                    .with("source", st)
                    .with("summary", summary(&d, secs, rate))
                    .with("spectrum", ts),
            );
        }
        src.off()?;
        println!(
            "  {name}: first byte {:?} ms; idle {}; noise {}",
            s.first_byte_ms,
            short(&idle, idle_s),
            short(&off, off_s)
        );
        runs.push(
            J::obj()
                .with("variant", name)
                .with(
                    "taps",
                    J::Arr(decode(&c).iter().map(|x| J::from(*x)).collect()),
                )
                .with("session", s.rec)
                .with("streams", s.first_byte_ms.is_some())
                .with("tuner_idle", summary(&idle, idle_s, rate))
                .with("tuner", tun)
                .with(
                    "tone_off",
                    J::obj()
                        .with("summary", summary(&off, off_s, rate))
                        .with("bands_db", bands_j(&bands(&off))),
                )
                .with("tone_states", states),
        );
        s.bulk.close();
    }
    src.quit();
    let rec = header("coeffs", "test 3: the stream from a power cycle with the observed coefficient values (fixture), with our own low-pass (own_lowpass) and with all 20 = 0; the tuner programmed; noise band shape and the tone at offsets in the band and in the alias zones (predicted bin folded into the output band)", o)
        .with(
            "own_design",
            J::obj()
                .with("method", "Hamming-windowed sinc, symmetric, computed by own_lowpass_taps(); the stored half outermost first")
                .with("assumed_tap_rate_hz", OWN_FS_HZ)
                .with("cutoff_hz", OWN_CUTOFF_HZ)
                .with("length", OWN_LENGTH)
                .with("dc_gain_sum_of_all_taps", OWN_DC_GAIN)
                .with("taps_c0_to_c15", J::Arr(own_lowpass_taps().iter().map(|x| J::from(*x)).collect()))
                .with("registers", hex_bytes(&own_lowpass())),
        )
        .with("runs", runs);
    crate::write_record("step10-3c-coeffs.json", &rec);
    Ok(())
}

/// The bytes/s of each window of a [`capture`] record.
fn window_rates(ws: &J) -> Vec<f64> {
    let J::Arr(v) = ws else { return Vec::new() };
    v.iter()
        .filter_map(|o| match o {
            J::Obj(f) => f
                .iter()
                .find(|(k, _)| k == "bytes_per_s")
                .and_then(|(_, v)| match v {
                    J::Num(x) => Some(*x),
                    J::Int(x) => Some(*x as f64),
                    _ => None,
                }),
            _ => None,
        })
        .collect()
}

/// One minimisation run: the plan from a power cycle; if it streams, the
/// tuner programmed and the (already keyed) tone looked for.
fn mini_run(plan: &Plan, o: &Opts, tone_ofs: Option<f64>) -> Result<(J, bool, J), String> {
    let (rec, ok, brief, s) = mini_session(plan, o, tone_ofs, 5)?;
    s.bulk.close();
    Ok((rec, ok, brief))
}

/// [`mini_run`] with `n` windows after the tuner, the session kept.
fn mini_session(
    plan: &Plan,
    o: &Opts,
    tone_ofs: Option<f64>,
    n: usize,
) -> Result<(J, bool, J, Session), String> {
    let mut s = fresh(plan)?;
    let rate = output_rate(plan.rsamp_ratio);
    let (_, idle, idle_s) = capture(&mut s.bulk, 3, step3::WINDOW);
    let pll = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?;
    let tun = program_tuner(&s.rtl, &o.front, &pll);
    s.bulk.drain();
    let (ws, d, secs) = capture(&mut s.bulk, n, step3::WINDOW);
    let rates = window_rates(&ws);
    let c = Content::of(&d, secs);
    let samples =
        d.len() >= content::MIN_BYTES_FOR_PERIOD && c.period_score.is_some_and(|p| p < 0.5);
    let ts = tone_ofs.map(|f| tone_state(&d, Some(f / rate), None, rate));
    println!(
        "    first byte {:?} ms; idle {}; tuned {}; tone {}",
        s.first_byte_ms.map(|m| (m * 1e3).round() / 1e3),
        short(&idle, idle_s),
        short(&d, secs),
        ts.as_ref().map(J::render).unwrap_or_default()
    );
    let tun_ok = !tun.render().contains("error");
    let brief = J::obj()
        .with("first_byte_ms", s.first_byte_ms)
        .with("bytes_before_start", s.bytes_before_start)
        .with("idle", summary(&idle, idle_s, rate))
        .with("tuned", summary(&d, secs, rate))
        .with(
            "tuned_window_rates_min_max",
            J::Arr(vec![
                J::from(rates.iter().copied().fold(f64::INFINITY, f64::min)),
                J::from(rates.iter().copied().fold(0.0, f64::max)),
            ]),
        )
        .with("tone", ts)
        .with("tuner_programmed_without_error", tun_ok)
        .with("samples", samples);
    let rec = std::mem::replace(&mut s.rec, J::Null);
    Ok((rec, samples, brief, s))
}

/// The items test 4 found removable alone, in three groups.
pub const USB_GROUP: [Item; 4] = [
    Item::UsbSysctl,
    Item::EpaMaxPkt,
    Item::EpaFifoReset,
    Item::ClearHalt,
];
pub const RESET_GROUP: [Item; 3] = [Item::DemodCtl1, Item::DemodReset, Item::Wait65];
pub const REST_GROUP: [Item; 7] = [
    Item::P1x3E,
    Item::P0x18,
    Item::P1x92,
    Item::P1x93,
    Item::P1x94,
    Item::P0x61,
    Item::P0x20,
];

/// The content plan with `add` put back.
fn content_plus(add: &[Item]) -> Plan {
    let c = Plan::content();
    Plan {
        skip: c.skip.into_iter().filter(|i| !add.contains(i)).collect(),
        ..c
    }
}

/// Test 4c: which of the removable groups keeps the rate steady (the
/// content plan's windows wandered 3.35-4.13 MB/s, data/step10-4b-
/// minimal.json), 20 windows per plan from a power cycle; then, in the
/// full plan's session, restarts without a power cycle: the start bit
/// alone, the USB group + start, the plan without the USB group + start,
/// the whole plan + start — each after a stop (start bit cleared).
fn steady(o: &Opts) -> Result<(), String> {
    let mut src = Source::open()?;
    let tone_ofs = 300e3;
    let centre = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?
    .lo_hz()
    .unwrap_or(0.0)
        - iffreq_hz(OBSERVED_IFFREQ);
    let tone_j = src.level(centre + tone_ofs, o.level)?;
    let full = Plan {
        coeffs: own_lowpass(),
        ..Plan::observed()
    };
    let mut plans: Vec<(String, Plan)> = vec![
        ("full sequence (own taps)".into(), full.clone()),
        ("content plan".into(), Plan::content()),
        ("content + USB group".into(), content_plus(&USB_GROUP)),
        ("content + reset group".into(), content_plus(&RESET_GROUP)),
        ("content + rest group".into(), content_plus(&REST_GROUP)),
    ];
    for item in REST_GROUP {
        plans.push((format!("content + {item:?}"), content_plus(&[item])));
    }
    let o20 = Opts {
        windows: 20,
        ..o.clone()
    };
    let mut runs = J::Arr(Vec::new());
    let mut restarts = J::Arr(Vec::new());
    for (n, (what, plan)) in plans.into_iter().enumerate() {
        source::check()?;
        println!("  {what}:");
        let (rec, ok, brief, mut s) = mini_session(&plan, &o20, Some(tone_ofs), o20.windows)?;
        println!(
            "    window rates min/max {}",
            match &brief {
                J::Obj(f) => f
                    .iter()
                    .find(|(k, _)| k == "tuned_window_rates_min_max")
                    .map(|(_, v)| v.render())
                    .unwrap_or_default(),
                _ => String::new(),
            }
        );
        runs.push(
            J::obj()
                .with("plan", what.as_str())
                .with("samples", ok)
                .with("brief", brief)
                .with("session", rec),
        );
        if n == 0 {
            restarts = restart_variants(&mut s, &full, tone_ofs);
        }
        s.bulk.close();
    }
    src.quit();
    let rec = header("steady", "test 4c: the content plan (test 4b) with each group of test 4's removable items put back, then each item of the group that mattered alone, 20 windows each from a power cycle, against the full sequence; and, in the full sequence's session before any other power cycle, restarts without a power cycle (stop = page 0 0x06 bit 7 cleared)", o)
        .with("groups", J::obj().with("usb", J::Arr(USB_GROUP.iter().map(|i| J::from(format!("{i:?}"))).collect())).with("reset", J::Arr(RESET_GROUP.iter().map(|i| J::from(format!("{i:?}"))).collect())).with("rest", J::Arr(REST_GROUP.iter().map(|i| J::from(format!("{i:?}"))).collect())))
        .with("tone", tone_j)
        .with("runs", runs)
        .with("restarts_in_the_full_sequence_session", restarts);
    crate::write_record("step10-4c-steady.json", &rec);
    Ok(())
}

/// Restarts without a power cycle, in order, each after a stop (page 0
/// 0x06 bit 7 cleared): the start bit alone; the USB group then the
/// start; every item but the USB group then the start; every item then
/// the start; DEMOD_CTL <- 0x20 (demodulator PLL off, as the vendor left
/// it between driver load and stream start, findings 9a) then every item
/// then the start.
fn restart_variants(s: &mut Session, full: &Plan, tone_ofs: f64) -> J {
    let mut restarts = J::Arr(Vec::new());
    {
        let rate = output_rate(full.rsamp_ratio);
        let only = |keep: &[Item]| Plan {
            skip: ITEMS.into_iter().filter(|i| !keep.contains(i)).collect(),
            ..full.clone()
        };
        let without_usb = Plan {
            skip: USB_GROUP.to_vec(),
            ..full.clone()
        };
        let variants: Vec<(&str, Option<Plan>, bool)> = vec![
            ("start bit alone", None, false),
            ("USB group, then start", Some(only(&USB_GROUP)), false),
            (
                "every item but the USB group, then start",
                Some(without_usb),
                false,
            ),
            ("every item, then start", Some(full.clone()), false),
            (
                "DEMOD_CTL <- 0x20, every item, then start",
                Some(full.clone()),
                true,
            ),
        ];
        for (what, plan, pll_off) in variants {
            let stop = s
                .rtl
                .demod_rmw(START_PAGE, START_OFF, START_VALUE, 0)
                .map(|(a, b)| format!("0x{a:02X} -> 0x{b:02X}"))
                .map_err(|e| e.to_string());
            std::thread::sleep(Duration::from_millis(10));
            s.bulk.drain();
            let (_, d_stop, _) = capture(&mut s.bulk, 2, step3::WINDOW);
            let pll = pll_off.then(|| exec(&s.rtl, &mut s.bulk, &Op::Sys(DEMOD_CTL, 0x20)));
            let (items, _) = match &plan {
                Some(p) => apply_items(&s.rtl, &mut s.bulk, p),
                None => (J::Arr(Vec::new()), 0),
            };
            let start = s
                .rtl
                .demod_write8(START_PAGE, START_OFF, START_VALUE)
                .err()
                .map(|e| e.to_string());
            let fb = s.bulk.first_byte(FIRST_BYTE_DEADLINE);
            // Plans with the reset group reset the demodulator (and its
            // I2C repeater); the tuner keeps its programming.
            let (ws, d, secs) = capture(&mut s.bulk, 5, step3::WINDOW);
            let rates = window_rates(&ws);
            let ts = tone_state(&d, Some(tone_ofs / rate), None, rate);
            println!(
                "  restart, {what}: {} B while stopped; first byte {:?}; {} | rates {:?}",
                d_stop.len(),
                fb.latency,
                short(&d, secs),
                rates.iter().map(|r| (r / 1e3).round()).collect::<Vec<_>>()
            );
            restarts.push(
                J::obj()
                    .with("restart", what)
                    .with("stop", J::from(format!("{stop:?}")))
                    .with("bytes_while_stopped_200ms", d_stop.len())
                    .with("pll_off_first", pll)
                    .with("items", items)
                    .with("start_error", start)
                    .with(
                        "first_byte_ms_16k_transfers",
                        fb.latency.map(|l| l.as_secs_f64() * 1e3),
                    )
                    .with("windows", ws)
                    .with("summary", summary(&d, secs, rate))
                    .with("tone", ts),
            );
        }
    }
    restarts
}

/// Test 4b: the minimal sequence and its variants, each from a power
/// cycle with the tone keyed at the DDC centre RF + 300 kHz (stopping and
/// restarting without a power cycle: [`steady`]).
fn minimal(o: &Opts) -> Result<(), String> {
    let mut src = Source::open()?;
    let tone_ofs = 300e3;
    let centre = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?
    .lo_hz()
    .unwrap_or(0.0)
        - iffreq_hz(OBSERVED_IFFREQ);
    let tone_j = src.level(centre + tone_ofs, o.level)?;
    let content_power_on_taps = Plan {
        skip: Plan::content()
            .skip
            .into_iter()
            .chain([Item::Coeffs])
            .collect(),
        ..Plan::content()
    };
    let plans: Vec<(&str, Plan)> = vec![
        (
            "minimal: AdcEnable (0xA8, 0xE8), page 0 0x19 = 0x25, start",
            Plan::minimal(),
        ),
        (
            "minimal with page 0 0x19 = 0x21",
            Plan {
                p0x19: 0x21,
                ..Plan::minimal()
            },
        ),
        (
            "minimal with page 0 0x19 = 0x24",
            Plan {
                p0x19: 0x24,
                ..Plan::minimal()
            },
        ),
        (
            "minimal with DEMOD_CTL <- 0xE8 as one write",
            Plan {
                adc_single_write: true,
                ..Plan::minimal()
            },
        ),
        (
            "content: minimal + 0x15, 0x16-0x18, IF, Spec_inv, rsamp, own taps, page 0 0x17",
            Plan::content(),
        ),
        (
            "content with the power-on taps (Coeffs not written)",
            content_power_on_taps,
        ),
        (
            "driver: DEMOD_CTL <- 0xE8, 0x16-0x18, IF, Spec_inv, rsamp, own taps, page 0 0x19, page 1 0x93, start",
            Plan::driver(),
        ),
    ];
    let mut runs = J::Arr(Vec::new());
    for (what, plan) in plans {
        source::check()?;
        println!("  {what}:");
        let (rec, ok, brief, s) = mini_session(&plan, o, Some(tone_ofs), o.windows)?;
        runs.push(
            J::obj()
                .with("plan", what)
                .with("samples", ok)
                .with("brief", brief)
                .with("session", rec),
        );
        s.bulk.close();
    }
    src.quit();
    let rec = header("minimal", "test 4b: the minimal sequence of test 4 and its variants from a power cycle (page 0 0x19's two changed bits apart; DEMOD_CTL 0xE8 as one write), the minimal sequence plus the content items (our own taps; the power-on taps), each with the tuner programmed and the tone keyed at the DDC centre RF + 300 kHz (stop and restart: test 4c)", o)
        .with("tone", tone_j)
        .with("runs", runs);
    crate::write_record("step10-4b-minimal.json", &rec);
    Ok(())
}

/// Test 4: each item removed in turn (a power cycle each); then the
/// start directly after the demodulator reset; then every item whose
/// removal kept the stream, removed together.
fn minimise(o: &Opts) -> Result<(), String> {
    let mut src = source::command().map(|_| Source::open()).transpose()?;
    let tone_ofs = 300e3;
    let centre = Pll::program(
        lo_for_rf(TONE_RF_HZ),
        TONE_SEL_DIV,
        tuner::REF_DIV2_BY_DOCUMENT,
    )?
    .lo_hz()
    .unwrap_or(0.0)
        - iffreq_hz(OBSERVED_IFFREQ);
    let tone_j = match src.as_mut() {
        Some(s) => Some(s.level(centre + tone_ofs, o.level)?),
        None => None,
    };
    let ofs = src.as_ref().map(|_| tone_ofs);
    let mut runs = J::Arr(Vec::new());
    println!("  full sequence:");
    let (rec, ok, brief) = mini_run(&Plan::observed(), o, ofs)?;
    runs.push(
        J::obj()
            .with("removed", J::Arr(Vec::new()))
            .with("samples", ok)
            .with("brief", brief)
            .with("session", rec),
    );
    let mut unneeded = Vec::new();
    for item in ITEMS {
        source::check()?;
        println!("  without {item:?}:");
        let plan = Plan {
            skip: vec![item],
            ..Plan::observed()
        };
        let (rec, ok, brief) = mini_run(&plan, o, ofs)?;
        if ok {
            unneeded.push(item);
        }
        runs.push(
            J::obj()
                .with("removed", J::Arr(vec![J::from(format!("{item:?}"))]))
                .with("samples", ok)
                .with("brief", brief)
                .with("session", rec),
        );
    }
    let mut extra: Vec<(&str, Vec<Item>)> = vec![(
        "all of findings 9b items 6-21 (the start directly after the ADC enable)",
        DEMOD_ITEMS.to_vec(),
    )];
    extra.push((
        "every item whose removal alone kept the stream",
        unneeded.clone(),
    ));
    for (what, skip) in extra {
        println!("  without {what}: {skip:?}");
        let plan = Plan {
            skip: skip.clone(),
            ..Plan::observed()
        };
        let (rec, ok, brief) = mini_run(&plan, o, ofs)?;
        runs.push(
            J::obj()
                .with("removed_what", what)
                .with(
                    "removed",
                    J::Arr(skip.iter().map(|i| J::from(format!("{i:?}"))).collect()),
                )
                .with("samples", ok)
                .with("brief", brief)
                .with("session", rec),
        );
    }
    if let Some(s) = src {
        s.quit();
    }
    let rec = header("minimise", "test 4: from a power cycle each, the observed sequence with one item removed; then the 9b items 6-21 removed together; then every item whose single removal kept a sample stream, removed together. samples = period score < 0.5 with enough bytes. With a source the tone stays keyed at the DDC centre RF + 300 kHz throughout, the tuner programmed after the start in every run", o)
        .with("tone", tone_j)
        .with("items_order", J::Arr(ITEMS.iter().map(|i| J::from(format!("{i:?}"))).collect()))
        .with("removable_alone", J::Arr(unneeded.iter().map(|i| J::from(format!("{i:?}"))).collect()))
        .with("runs", runs);
    crate::write_record("step10-4-minimise.json", &rec);
    Ok(())
}

/// The PLL findings 9e reports for 5A: SEL_DIV 011, Nint 100, SDM 7646,
/// REF_DIV2 1.
pub const SELDIV_PLL: Pll = Pll {
    sel_div: 0b011,
    ref_div2: true,
    nint: 100,
    sdm: 7646,
};
/// The sweep: the tone stepped across the source's range with the PLL
/// held (no divider hypothesis): from, to, step (Hz).
pub const SWEEP_HZ: (f64, f64, f64) = (50e6, 400e6, 1.5e6);
/// A step "answers" when its strongest rise over the off reference
/// exceeds this (dB).
pub const ANSWER_DB: f64 = 15.0;

/// Test 5: SEL_DIV 011 against the tone: the tone where /16 predicts,
/// then the hypothesis-free sweep; /8 at the same Nint as the control.
fn seldiv(o: &Opts) -> Result<(), String> {
    let plan = Plan::observed();
    let rate = output_rate(plan.rsamp_ratio);
    let if_ddc = iffreq_hz(OBSERVED_IFFREQ);
    let mut src = Source::open()?;
    src.off()?;
    let mut s = fresh(&plan)?;
    let mut parts = J::Arr(Vec::new());
    for (name, pll) in [
        ("SEL_DIV 011", SELDIV_PLL),
        (
            "control: SEL_DIV 010 (/8) at the same Nint/SDM",
            Pll {
                sel_div: 0b010,
                ..SELDIV_PLL
            },
        ),
    ] {
        let tun = program_tuner(&s.rtl, &o.front, &pll);
        src.off()?;
        s.bulk.drain();
        let (_, off, _) = capture(&mut s.bulk, 5, step3::WINDOW);
        let off_p = confirm_write::spectrum_power(&off, true).map(|x| x.0);
        let mut predicted = J::Arr(Vec::new());
        for div in [16u32, 8, 1] {
            let lo = pll.lo_hz_for(div);
            let centre = lo - if_ddc;
            if !(1e6..=6e9).contains(&centre) {
                continue;
            }
            for ofs in [200e3, -300e3] {
                let st = src.level(centre + ofs, o.level)?;
                s.bulk.drain();
                let (_, d, _) = capture(&mut s.bulk, 3, step3::WINDOW);
                let ts = tone_state(&d, Some(ofs / rate), off_p.as_deref(), rate);
                println!(
                    "  {name}: /{div} predicts LO {lo:.0}, tone {:.0}: {}",
                    centre + ofs,
                    ts.render()
                );
                predicted.push(
                    J::obj()
                        .with("divider_hypothesis", div)
                        .with("lo_hz", lo)
                        .with("rf_hz", centre + ofs)
                        .with("offset_hz", ofs)
                        .with("source", st)
                        .with("spectrum", ts),
                );
            }
        }
        let mut answers = J::Arr(Vec::new());
        let mut steps = 0;
        let mut f = SWEEP_HZ.0;
        while f <= SWEEP_HZ.1 {
            source::check()?;
            src.level(f, o.level)?;
            s.bulk.drain();
            let (_, d, _) = capture(&mut s.bulk, 1, step3::WINDOW);
            steps += 1;
            if let (Some((p, ..)), Some(r)) =
                (confirm_write::spectrum_power(&d, true), off_p.as_deref())
            {
                let rise = |k: usize| {
                    10.0 * (p[k].max(f64::MIN_POSITIVE) / r[k].max(f64::MIN_POSITIVE)).log10()
                };
                let top = (0..FFT_N)
                    .filter(|k| !is_dc(*k))
                    .max_by(|a, b| {
                        rise(*a)
                            .partial_cmp(&rise(*b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap_or(0);
                if rise(top) > ANSWER_DB {
                    let bb = confirm_write::cycles(top) * rate;
                    println!(
                        "  {name}: sweep {f:.0} Hz answers: bin {top}, {bb:+.0} Hz, rise {:.1} dB",
                        rise(top)
                    );
                    answers.push(
                        J::obj()
                            .with("tone_hz", f)
                            .with("bin", top)
                            .with("baseband_hz", bb)
                            .with("rise_db", rise(top)),
                    );
                }
            }
            f += SWEEP_HZ.2;
        }
        src.off()?;
        parts.push(
            J::obj()
                .with("pll", name)
                .with("tuner", tun)
                .with("predicted_points", predicted)
                .with(
                    "sweep",
                    J::obj()
                        .with("from_hz", SWEEP_HZ.0)
                        .with("to_hz", SWEEP_HZ.1)
                        .with("step_hz", SWEEP_HZ.2)
                        .with("steps", steps)
                        .with("answer_threshold_db", ANSWER_DB)
                        .with("answers", answers),
                ),
        );
    }
    src.quit();
    s.bulk.close();
    let rec = header("seldiv", "test 5: the tuner's SEL_DIV 011 (findings 9e: Nint 100, SDM 7646, REF_DIV2 1) with the stream as the receiver: the tone where /16, /8 and /1 predict the DDC centre (LO - 3.570 MHz, high side), then stepped 50-400 MHz with the PLL held; the same for SEL_DIV 010 (/8) as the control", o)
        .with("session", s.rec)
        .with("ddc_if_hz", if_ddc)
        .with("output_rate_hz", rate)
        .with("parts", parts);
    crate::write_record("step10-5-seldiv.json", &rec);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_words_decode() {
        assert_eq!(rsamp_bytes(OBSERVED_RSAMP_RATIO), [0x03, 0x84, 0x00, 0x00]);
        assert!((f_symbol(OBSERVED_RSAMP_RATIO) - 8.192e6).abs() < 1e-3);
        assert!((output_rate(OBSERVED_RSAMP_RATIO) - 2.048e6).abs() < 1e-3);
        assert!((iffreq_hz(OBSERVED_IFFREQ) - 519_918.0 / TWO_POW_22 * XTAL_HZ).abs() < 1e-6);
        // findings 9h item 9: LO 180.21 MHz, the DDC centre 176.64 MHz (DAB 5B) if SEL_DIV 011 = /16.
        // The layout decodes the fixture to a smooth taper and round-trips.
        let taps = decode(&OBSERVED_COEFFS_FIXTURE);
        assert_eq!(
            taps,
            [
                -54, -36, -41, -40, -32, -14, 14, 53, 101, 156, 215, 273, 327, 372, 404, 421
            ]
        );
        assert_eq!(encode(&taps), OBSERVED_COEFFS_FIXTURE);
        assert!(
            (SELDIV_PLL.lo_hz_for(16) - iffreq_hz(OBSERVED_IFFREQ) - 176.640e6).abs() < 2e3,
            "{}",
            SELDIV_PLL.lo_hz_for(16)
        );
    }

    #[test]
    fn own_design_fits_the_fields() {
        let c = own_lowpass_taps();
        assert!(
            c[..NARROW_TAPS].iter().all(|v| (-128..=127).contains(v)),
            "{c:?}"
        );
        assert!(
            c[NARROW_TAPS..].iter().all(|v| (-2048..=2047).contains(v)),
            "{c:?}"
        );
        assert!(
            (2 * c.iter().sum::<i32>() - OWN_DC_GAIN as i32).abs() <= TAPS as i32,
            "{c:?}"
        );
        assert_eq!(decode(&own_lowpass()), c);
        // A low-pass: the taps rise toward the centre.
        assert!(c[TAPS - 1] > c[NARROW_TAPS] && c[NARROW_TAPS] > c[0]);
        assert_ne!(own_lowpass(), OBSERVED_COEFFS_FIXTURE);
    }

    #[test]
    fn the_guard_refuses_0x1c() {
        assert!(
            covers_hazard(0x1C, 1)
                && covers_hazard(0x18, 8)
                && !covers_hazard(0x00, 4)
                && !covers_hazard(0x1D, 2)
        );
    }
}
