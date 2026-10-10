//! The raw-sample stream: the start sequence the bench reduced the
//! vendor driver's observed sequence to (findings 10.4, "the driver
//! sequence, measured as one plan"), stop and restart by page 0 0x06
//! bit 7, pause by page 1 0x01 bit 2, and the receive side over sdr-core's
//! bulk-IQ engine.
//!
//! The wire (findings 10.1, 10.2b; STATUS "Wire sample format"): bulk IN
//! endpoint 0x81 of interface 0 (findings step 1A: "interface 0 bulk IN
//! 0x81, max packet 512"), 8-bit unsigned interleaved pairs, even byte
//! I, odd byte Q — a tone above the centre RF reads at positive
//! frequency with Spec_inv 1 and the high-side LO; the idle centre is
//! ≈ 126.5. `WireSample::OffsetU8` widens each byte as (b − 128) << 8.
//!
//! What the bench did and this module repeats:
//! - the bulk endpoint polled before the first write (findings 10: "The
//!   bulk endpoint 0x81 is polled from before the first write");
//! - the nine items of the driver plan in order, each write read back
//!   ([`start_sequence`]); first bytes 0.016 ms after the start write,
//!   4.092-4.126 MB/s over 20 windows at 2.048 Msps (findings 10.4);
//! - stop = page 0 0x06 bit 7 cleared (0 bytes), restart = the bit set
//!   again (≤ 3.1 ms, steady, with the tone; findings 10.4) — so a stream
//!   stopped and started again writes the start bit alone;
//! - pause = page 1 0x01 0x10 → 0x14, resume = back to 0x10 (findings
//!   10.2c).
//!
//! What it leaves to the caller, by choice: the rate is set before the
//! stream exists (`Rtl::set_rate`; live ratio changes were not dependable,
//! findings 10.2a), and the tuner may be retuned while streaming (the
//! bench tuned after the start in every minimisation run, findings 10.4).
//!
//! No USB-block register is written here: findings 10.4 found none
//! needed after a power cycle, and USB-block offset 0x1C is the test-mode
//! hazard (derivation/HAZARD-usb-0x1C.md). The tap registers at
//! demodulator page 1 0x1C-0x2F are a different block.

use sdr_core::usb_stream::{BulkIqStream, WireSample};
use sdr_core::{Iq, IqStream, RecvInfo, Result, RxSource, elevate_current_thread};

use crate::demod::{DDC_IF_HZ, output_rate_of_ratio, pset_iffreq_bytes, rsamp_transfers};
use crate::protocol::*;
use crate::taps::TAP_REGS;
use crate::usb::Link;

/// The interface and endpoint of the bulk IN pipe (findings step 1A:
/// "interface 0 bulk IN 0x81, max packet 512; interface 1 has no
/// endpoints"; the power-on USB_EPA_CFG 0xD1 reads as enabled, bulk, IN,
/// endpoint 1, matching the descriptor).
pub const BULK_INTERFACE: u8 = 0;
pub const BULK_IN_ENDPOINT: u8 = 0x81;

/// 8-bit unsigned I/Q pairs, even byte I (findings 10.2b).
pub const WIRE_FORMAT: WireSample = WireSample::OffsetU8;

/// Where one byte of a write lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// The system block (wValue 0x3000 + offset).
    Sys,
    /// A demodulator page.
    Page(u8),
}

/// One control transfer of the sequence, read back after it is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// One system byte; `not_read_back` masks bits that never read back.
    Sys { off: u16, value: u8, not_read_back: u8 },
    /// Consecutive demodulator registers from `reg`, one transfer.
    Demod { page: u8, reg: u8, data: Vec<u8> },
}

impl Write {
    fn demod(page: u8, reg: u8, data: &[u8]) -> Write {
        Write::Demod { page, reg, data: data.to_vec() }
    }

    /// Every byte the transfer writes: (space, register, value).
    pub fn bytes(&self) -> Vec<(Space, u16, u8)> {
        match self {
            Write::Sys { off, value, .. } => vec![(Space::Sys, *off, *value)],
            Write::Demod { page, reg, data } => data.iter().enumerate().map(|(k, &v)| (Space::Page(*page), *reg as u16 + k as u16, v)).collect(),
        }
    }

    /// Make the transfer and read it back.
    pub fn apply(&self, link: &Link) -> Result<()> {
        match self {
            Write::Sys { off, value, not_read_back } => link.sys_write8_verified(*off, *value, *not_read_back),
            Write::Demod { page, reg, data } => link.demod_write_verified(*page, *reg, data),
        }
    }
}

/// The nine items of findings 10.4's driver sequence, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// 1. DEMOD_CTL ← 0xE8: PLL, both ADCs, reset released.
    AdcEnable,
    /// 2. Page 1 0x16-0x17 ← 00 00, 0x18 ← 00: the frequency offset zeroed.
    FreqOffset,
    /// 3. pset_iffreq, page 1 0x19-0x1A then 0x1B: the IF.
    IfFreq,
    /// 4. Page 1 0x15 ← 0x01: Spec_inv.
    SpecInv,
    /// 5. rsamp_ratio, page 1 0x9F-0xA0 then 0xA1-0xA2: the rate.
    Rsamp,
    /// 6. The 20 tap registers, page 1 0x1C-0x2F, one byte each, ascending.
    Taps,
    /// 7. Page 0 0x19 ← 0x25: the stream's enable.
    StreamEnable,
    /// 8. Page 1 0x93 ← 0xF0: a steady rate.
    RateSteady,
    /// 9. Page 0 0x06 ← 0x80: the start.
    Start,
}

/// One item and its transfers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub item: Item,
    pub writes: Vec<Write>,
}

/// The start sequence for a resampler ratio and a tap image: findings
/// 10.4's nine items in the order measured as one plan (the bench's
/// `Plan::driver`, `examples/harness/stream_confirm.rs`), with the
/// transfers grouped as findings 9b observed and the bench repeated.
/// The IF is [`DDC_IF_HZ`]; the tuner's LO is placed for it by
/// `Rtl::set_freq`.
pub fn start_sequence(ratio: u32, taps: &[u8; TAP_REGS]) -> Vec<Step> {
    let iff = pset_iffreq_bytes(DDC_IF_HZ);
    let rs = rsamp_transfers(ratio);
    let cleared = [FREQ_OFFSET_CLEARED; (FREQ_OFFSET_LAST - FREQ_OFFSET_FIRST) as usize];
    vec![
        Step { item: Item::AdcEnable, writes: vec![Write::Sys { off: DEMOD_CTL, value: DEMOD_CTL_STREAM, not_read_back: DEMOD_CTL_NOT_READ_BACK }] },
        Step {
            item: Item::FreqOffset,
            writes: vec![Write::demod(FREQ_OFFSET_PAGE, FREQ_OFFSET_FIRST, &cleared), Write::demod(FREQ_OFFSET_PAGE, FREQ_OFFSET_LAST, &[FREQ_OFFSET_CLEARED])],
        },
        Step { item: Item::IfFreq, writes: vec![Write::demod(IFFREQ_PAGE, IFFREQ_MSB, &iff[..2]), Write::demod(IFFREQ_PAGE, IFFREQ_MSB + 2, &iff[2..])] },
        Step { item: Item::SpecInv, writes: vec![Write::demod(SPEC_INV_PAGE, SPEC_INV_OFF, &[SPEC_INV_STREAM])] },
        Step { item: Item::Rsamp, writes: rs.iter().map(|(reg, pair)| Write::demod(RSAMP_PAGE, *reg, pair)).collect() },
        Step { item: Item::Taps, writes: taps.iter().enumerate().map(|(k, &v)| Write::demod(TAP_PAGE, TAP_FIRST + k as u8, &[v])).collect() },
        Step { item: Item::StreamEnable, writes: vec![Write::demod(STREAM_ENABLE_PAGE, STREAM_ENABLE_OFF, &[STREAM_ENABLE_VALUE])] },
        Step { item: Item::RateSteady, writes: vec![Write::demod(RATE_STEADY_PAGE, RATE_STEADY_OFF, &[RATE_STEADY_VALUE])] },
        Step { item: Item::Start, writes: vec![restart_write()] },
    ]
}

/// Every byte of a sequence in write order.
pub fn flatten(steps: &[Step]) -> Vec<(Space, u16, u8)> {
    steps.iter().flat_map(|s| s.writes.iter().flat_map(Write::bytes)).collect()
}

/// The start bit alone: the write that restarts a stopped stream
/// (findings 10.4) and the last item of [`start_sequence`].
pub fn restart_write() -> Write {
    Write::demod(START_PAGE, START_OFF, &[START_VALUE])
}

/// A read-modify-write of some bits of one demodulator register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitWrite {
    pub page: u8,
    pub reg: u8,
    pub mask: u8,
    pub value: u8,
}

impl BitWrite {
    /// The byte the register holds after the write, from what it held.
    pub const fn applied_to(self, current: u8) -> u8 {
        (current & !self.mask) | (self.value & self.mask)
    }

    /// Make the write, read back and checked on the mask.
    pub fn apply(self, link: &Link) -> Result<u8> {
        link.demod_rmw8(self.page, self.reg, self.mask, self.value)
    }
}

/// Stop: page 0 0x06 bit 7 cleared, the rest kept (findings 10.4: 0
/// bytes; the bench's stop was this read-modify-write).
pub const STOP: BitWrite = BitWrite { page: START_PAGE, reg: START_OFF, mask: START_BIT, value: 0 };
/// Pause: page 1 0x01 bit 2 set (findings 10.2c: 0x10 → 0x14).
pub const PAUSE: BitWrite = BitWrite { page: PAUSE_PAGE, reg: PAUSE_OFF, mask: PAUSE_BIT, value: PAUSE_BIT };
/// Resume: page 1 0x01 bit 2 cleared (findings 10.2c: 0x14 → 0x10).
pub const RESUME: BitWrite = BitWrite { page: PAUSE_PAGE, reg: PAUSE_OFF, mask: PAUSE_BIT, value: 0 };

/// Where a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Made, never started: the next start writes the whole sequence.
    Idle,
    /// Started.
    Running,
    /// Stopped after running: the next start writes the start bit alone.
    Stopped,
}

/// A receive stream over the bulk IN endpoint. Made by `Rtl::rx_stream`
/// with the rate the receiver holds; the rate cannot change while it
/// exists (the receiver refuses).
pub struct RtlStream {
    inner: BulkIqStream,
    link: Link,
    ratio: u32,
    taps: [u8; TAP_REGS],
    state: State,
    paused: bool,
}

impl RtlStream {
    /// A stream over `inner` that starts with `ratio` and `taps`. The
    /// link must have been claimed for the stream (`Link::claim_stream`);
    /// dropping the stream releases it.
    pub fn new(inner: BulkIqStream, link: Link, ratio: u32, taps: [u8; TAP_REGS]) -> RtlStream {
        RtlStream { inner, link, ratio, taps, state: State::Idle, paused: false }
    }

    /// The output rate, f_symbol / 4 of the ratio (findings 10.2a).
    pub fn rate(&self) -> f64 {
        output_rate_of_ratio(self.ratio)
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Start: transfers queued first, then the whole [`start_sequence`]
    /// the first time, the start bit alone after a [`RtlStream::stop`].
    /// A failed write leaves the state as it was and the transfers
    /// cancelled. Starting a running stream does nothing.
    pub fn start(&mut self) -> Result<()> {
        let writes: Vec<Write> = match self.state {
            State::Running => return Ok(()),
            State::Idle => start_sequence(self.ratio, &self.taps).into_iter().flat_map(|s| s.writes).collect(),
            State::Stopped => vec![restart_write()],
        };
        self.inner.start()?;
        for w in &writes {
            if let Err(e) = w.apply(&self.link) {
                let _ = self.inner.stop();
                return Err(e);
            }
        }
        self.state = State::Running;
        Ok(())
    }

    /// Stop: the start bit cleared ([`STOP`]), then the queued transfers
    /// cancelled. Stopping a stream that is not running does nothing.
    pub fn stop(&mut self) -> Result<()> {
        if self.state != State::Running {
            return Ok(());
        }
        let wrote = STOP.apply(&self.link);
        let cancelled = self.inner.stop();
        self.state = State::Stopped;
        wrote?;
        cancelled
    }

    /// Pause: page 1 0x01 bit 2 set ([`PAUSE`]); the endpoint goes quiet
    /// and the stream's ledger counts the paused interval as samples not
    /// delivered (they never existed: a time discontinuity).
    pub fn pause(&mut self) -> Result<()> {
        PAUSE.apply(&self.link)?;
        self.paused = true;
        Ok(())
    }

    /// Resume: page 1 0x01 bit 2 cleared ([`RESUME`]).
    pub fn resume(&mut self) -> Result<()> {
        RESUME.apply(&self.link)?;
        self.paused = false;
        Ok(())
    }

    /// Start the stream on a worker thread (`sdr_core::IqStream`), at
    /// most `depth` chunks queued for the consumer.
    pub fn into_stream(self, depth: usize) -> Result<IqStream> {
        IqStream::spawn(self, depth)
    }
}

impl RxSource for RtlStream {
    fn max_num_samps(&self) -> usize {
        self.inner.max_num_samps()
    }

    fn start(&mut self) -> Result<()> {
        RtlStream::start(self)
    }

    fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.inner.recv_into(buf, timeout_secs)
    }

    fn setup_worker_thread(&self) {
        elevate_current_thread();
    }
}

impl Drop for RtlStream {
    /// The stream stopped, unpaused (the register back at its power-on
    /// state for the next stream), and the receiver free to change the
    /// rate again. The transfers are cancelled by the engine's own drop.
    fn drop(&mut self) {
        let _ = self.stop();
        if self.paused {
            let _ = RESUME.apply(&self.link);
        }
        self.link.release_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::ratio_for_output_rate;
    use crate::taps::design_registers;

    /// Findings 10.4's driver sequence at 2.048 Msps (rsamp_ratio
    /// 0xE10000, the bench's), byte by byte in write order:
    /// 1. DEMOD_CTL ← 0xE8; 2. page 1 0x16-0x17 ← 00 00, 0x18 ← 00;
    /// 3. pset_iffreq 0x19-0x1B ← 38 11 12; 4. 0x15 ← 0x01;
    /// 5. 0x9F-0xA2 ← 03 84 00 00; 6. the taps at 0x1C-0x2F;
    /// 7. page 0 0x19 ← 0x25; 8. page 1 0x93 ← 0xF0; 9. page 0 0x06 ← 0x80.
    #[test]
    fn the_start_sequence_is_findings_10s_driver_plan() {
        let taps = design_registers();
        let seq = start_sequence(ratio_for_output_rate(2.048e6).unwrap(), &taps);
        let items: Vec<Item> = seq.iter().map(|s| s.item).collect();
        assert_eq!(
            items,
            [Item::AdcEnable, Item::FreqOffset, Item::IfFreq, Item::SpecInv, Item::Rsamp, Item::Taps, Item::StreamEnable, Item::RateSteady, Item::Start]
        );
        let p1 = Space::Page(1);
        let p0 = Space::Page(0);
        let mut want = vec![
            (Space::Sys, 0x0000, 0xE8),
            (p1, 0x16, 0x00),
            (p1, 0x17, 0x00),
            (p1, 0x18, 0x00),
            (p1, 0x19, 0x38),
            (p1, 0x1A, 0x11),
            (p1, 0x1B, 0x12),
            (p1, 0x15, 0x01),
            (p1, 0x9F, 0x03),
            (p1, 0xA0, 0x84),
            (p1, 0xA1, 0x00),
            (p1, 0xA2, 0x00),
        ];
        want.extend(taps.iter().enumerate().map(|(k, &v)| (p1, 0x1C + k as u16, v)));
        want.extend([(p0, 0x19, 0x25), (p1, 0x93, 0xF0), (p0, 0x06, 0x80)]);
        assert_eq!(flatten(&seq), want);
        // The last write is the start, alone in its item.
        assert_eq!(seq.last().unwrap().writes, vec![restart_write()]);
    }

    /// The transfers as observed and benched: 0x16-0x17 and 0x19-0x1A
    /// as 2-byte writes, rsamp as two pairs, each tap its own write,
    /// DEMOD_CTL judged on every bit but bit 2.
    #[test]
    fn the_transfers_are_grouped_as_on_the_wire() {
        let seq = start_sequence(0xE10000, &design_registers());
        let lens: Vec<Vec<usize>> = seq
            .iter()
            .map(|s| {
                s.writes
                    .iter()
                    .map(|w| match w {
                        Write::Sys { .. } => 1,
                        Write::Demod { data, .. } => data.len(),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(lens, vec![vec![1], vec![2, 1], vec![2, 1], vec![1], vec![2, 2], vec![1; TAP_REGS], vec![1], vec![1], vec![1]]);
        assert_eq!(seq[0].writes[0], Write::Sys { off: DEMOD_CTL, value: 0xE8, not_read_back: 1 << 2 });
        assert_eq!(flatten(&seq).len(), 1 + 3 + 3 + 1 + 4 + TAP_REGS + 3);
    }

    /// The rate enters only through rsamp_ratio: 2.4 Msps writes
    /// 0x9F-0xA2 ← 03 00 00 00 (0xC00000, findings 10.2a) where 2.048
    /// writes 03 84 00 00, so only 0xA0 differs; 1.024 Msps (0x1C20000)
    /// writes 07 08 00 00.
    #[test]
    fn the_rate_changes_only_the_resampler_bytes() {
        let taps = design_registers();
        let a = flatten(&start_sequence(0xE10000, &taps));
        let differ = |sps: f64| -> Vec<(Space, u16, u8)> {
            let b = flatten(&start_sequence(ratio_for_output_rate(sps).unwrap(), &taps));
            assert_eq!(a.len(), b.len());
            a.iter().zip(&b).filter(|(x, y)| x != y).map(|(_, y)| *y).collect()
        };
        assert_eq!(differ(2.4e6), vec![(Space::Page(1), 0xA0, 0x00)]);
        assert_eq!(differ(1.024e6), vec![(Space::Page(1), 0x9F, 0x07), (Space::Page(1), 0xA0, 0x08)]);
    }

    /// No write of the sequence or of stop/pause touches the USB block,
    /// let alone its offset 0x1C.
    #[test]
    fn nothing_is_written_to_the_usb_block() {
        for (space, reg, _) in flatten(&start_sequence(0xE10000, &design_registers())) {
            match space {
                Space::Sys => assert_eq!(reg, DEMOD_CTL),
                Space::Page(p) => assert!(p <= 1, "page {p} register 0x{reg:02X}"),
            }
        }
    }

    /// Stop, restart, pause and resume as findings 10.2c and 10.4
    /// measured them.
    #[test]
    fn stop_pause_and_resume_encodings() {
        assert_eq!(STOP.applied_to(START_VALUE), 0x00);
        assert_eq!(restart_write().bytes(), vec![(Space::Page(0), 0x06, 0x80)]);
        assert_eq!(PAUSE.applied_to(PAGE1_0X01_POWER_ON), 0x14);
        assert_eq!(RESUME.applied_to(0x14), 0x10);
        assert_eq!(RESUME.applied_to(PAUSE.applied_to(0x10)), 0x10);
        // With the repeater open (a tuner access in flight) only bit 2 moves.
        assert_eq!(PAUSE.applied_to(0x18), 0x1C);
        assert_eq!(RESUME.applied_to(0x1C), 0x18);
        // The stop keeps the other bits of page 0 0x06 (opt_adc_iq at 0).
        assert_eq!(STOP.applied_to(0xB0), 0x30);
        assert_eq!((STOP.page, STOP.reg), (0, 0x06));
        assert_eq!((PAUSE.page, PAUSE.reg, RESUME.page, RESUME.reg), (1, 0x01, 1, 0x01));
    }
}
