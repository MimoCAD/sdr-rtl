//! A simulated dongle for tests: an RTL2832U + R820T register file behind
//! a [`Transport`], so the driver's bring-up, stream and changes run
//! without hardware and any request can be made to fail.
//!
//! What it models, and no more:
//! - every register the driver writes reads back as written (the
//!   demodulator's pages, the system and USB blocks, the tuner's R5-R15,
//!   the EEPROM), multi-byte requests auto-incrementing;
//! - the tuner answers only with the I2C repeater open (page 1 0x01 bit 3,
//!   findings step 2), reads arrive bit-reversed, R0 is the check byte
//!   0x96 and R2 the VCO indicator ([`FakeRtl::set_vco_indicator`]);
//! - samples flow on the bulk endpoint only while the start bit (page 0
//!   0x06 bit 7) is set and the pause bit (page 1 0x01 bit 2) clear, paced
//!   at the output rate; sample k carries I = k mod 256, Q = I ^ 0x5A, so a
//!   test can see order and loss;
//! - while USB_EPA_CTL holds the FIFO reset or the stall (bit 9 or 4),
//!   every bulk transfer stalls, as on a dongle Linux's kernel driver let
//!   go of ([`FakeRtl::set_epa_ctl`], findings 21);
//! - optionally, a demodulator write refused unless a read was answered
//!   shortly before it ([`FakeRtl::set_demod_write_needs_read`]);
//! - the dropped-block counter (USB 0x163) reads what the test sets.
//!
//! [`FakeRtl::fail`] makes chosen requests fail; [`FakeRtl::requests`] is
//! the log of every request, in order.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::boxed::Box;
use std::collections::HashMap;
use std::string::String;
use std::sync::{Arc, Mutex, MutexGuard};
use std::vec::Vec;

use nusb::transfer::TransferError;
use sdr_core::Result;
use sdr_core::time::Instant;
use sdr_core::usb_stream::BoxedBulkIn;
use sdr_core::usb_stream::mock::{self, Event, MockHandle};

use crate::protocol::*;
use crate::usb::{Transport, TransportFuture};

/// The output rate the fake paces its samples at, samples per second: the
/// driver's default (`demod::DEFAULT_OUTPUT_RATE_SPS`).
pub const FAKE_RATE_SPS: f64 = 2_048_000.0;
/// The VCO indicator the fake's tuner reports unless told otherwise: inside
/// the locked span 64-127 (findings 14).
pub const FAKE_VCO_LOCKED: u8 = 92;

/// What a request addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Sys(u16),
    Usb(u16),
    Demod { page: u8, off: u8 },
    TunerWrite(u8),
    TunerRead,
    Eeprom(u8),
    Other { value: u16, index: u16 },
}

/// One request, as the fake decoded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// A Set (OUT); else a Get.
    pub set: bool,
    pub target: Target,
    /// A Set's data; empty for a Get.
    pub data: Vec<u8>,
    /// A Get's length.
    pub len: u16,
    /// It was failed by a rule.
    pub failed: bool,
}

impl Request {
    /// A Set of one demodulator register whose first byte has `bit` set
    /// (`on`) or clear.
    pub fn sets_demod_bit(&self, page: u8, off: u8, bit: u8, on: bool) -> bool {
        self.set
            && self.target == Target::Demod { page, off }
            && self.data.first().is_some_and(|b| (b & bit != 0) == on)
    }

    /// The pause-bit write ([`PAUSE_BIT`] set; bench comparisons only since
    /// findings 19).
    pub fn is_pause(&self) -> bool {
        self.sets_demod_bit(PAUSE_PAGE, PAUSE_OFF, PAUSE_BIT, true)
    }

    /// The halt a change quiesces with ([`START_BIT`] cleared).
    pub fn is_halt(&self) -> bool {
        self.sets_demod_bit(START_PAGE, START_OFF, START_BIT, false)
    }

    /// The restart that ends a change ([`START_BIT`] set; the stream's
    /// first start writes it too).
    pub fn is_restart(&self) -> bool {
        self.sets_demod_bit(START_PAGE, START_OFF, START_BIT, true)
    }
}

type Matcher = Box<dyn Fn(&Request) -> bool + Send>;

struct Rule {
    matches: Matcher,
    error: TransferError,
    /// Matching requests let through before the rule fails any.
    skip: usize,
    /// Matching requests failed, then the rule is spent.
    times: usize,
}

#[derive(Default)]
struct Regs {
    demod: HashMap<(u8, u8), u8>,
    sys: HashMap<u16, u8>,
    usb: HashMap<u16, u8>,
    tuner: [u8; 32],
    eeprom: Vec<u8>,
    dropped: u8,
    /// [`FakeRtl::queue_vco_indicators`]: R2 for the next tuner reads.
    r2_queue: std::collections::VecDeque<u8>,
    /// [`FakeRtl::set_demod_write_needs_read`].
    demod_write_needs_read: Option<Duration>,
    last_get: Option<Instant>,
    rules: Vec<Rule>,
    log: Vec<Request>,
}

/// What the bulk source reads without the register lock.
struct Flow {
    started: AtomicBool,
    paused: AtomicBool,
    /// USB_EPA_CTL holds the FIFO reset or the stall: transfers stall.
    held: AtomicBool,
    /// Samples sent since the stream was set running.
    sent: AtomicU64,
    /// When the samples began to flow, for pacing.
    since: Mutex<Option<Instant>>,
}

/// The simulated dongle. Hand it to [`crate::Rtl::open_transport`] as an
/// `Arc`; keep a clone to script it.
pub struct FakeRtl {
    regs: Mutex<Regs>,
    flow: Arc<Flow>,
    bulk: Mutex<Option<MockHandle>>,
}

impl FakeRtl {
    pub fn new() -> Arc<FakeRtl> {
        let mut regs = Regs::default();
        regs.tuner[0] = TUNER_R0_CHECK;
        regs.tuner[2] = FAKE_VCO_LOCKED;
        regs.demod
            .insert((PAUSE_PAGE, PAUSE_OFF), PAGE1_0X01_POWER_ON);
        regs.eeprom = vec![0xFF; 256];
        Arc::new(FakeRtl {
            regs: Mutex::new(regs),
            flow: Arc::new(Flow {
                started: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                held: AtomicBool::new(false),
                sent: AtomicU64::new(0),
                since: Mutex::new(None),
            }),
            bulk: Mutex::new(None),
        })
    }

    fn regs(&self) -> MutexGuard<'_, Regs> {
        self.regs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Fail matching requests with `error`: the first `skip` let through,
    /// then `times` failed (each failed request is not applied).
    pub fn fail(
        &self,
        matches: impl Fn(&Request) -> bool + Send + 'static,
        error: TransferError,
        skip: usize,
        times: usize,
    ) {
        self.regs().rules.push(Rule {
            matches: Box::new(matches),
            error,
            skip,
            times,
        });
    }

    /// Every request so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.regs().log.clone()
    }

    /// The tuner's R2 (VCO indicator) from now on.
    pub fn set_vco_indicator(&self, v: u8) {
        self.regs().tuner[2] = v;
    }

    /// From now on a demodulator write is refused (STALL) unless a Get was
    /// answered within `within` before it, as one Linux host's dongle did
    /// with 300 ms (findings 21).
    pub fn set_demod_write_needs_read(&self, within: Duration) {
        self.regs().demod_write_needs_read = Some(within);
    }

    /// USB_EPA_CTL as found at open: 0x0210 is what Linux's kernel driver
    /// leaves (findings 21).
    pub fn set_epa_ctl(&self, v: u16) {
        let mut regs = self.regs();
        let [lo, hi] = v.to_le_bytes();
        regs.usb.insert(USB_EPA_CTL, lo);
        regs.usb.insert(USB_EPA_CTL + 1, hi);
        self.update_held(&regs);
    }

    fn update_held(&self, regs: &Regs) {
        let v = u16::from_le_bytes(
            [0, 1].map(|k| regs.usb.get(&(USB_EPA_CTL + k)).copied().unwrap_or(0)),
        );
        self.flow.held.store(
            v & (EPA_CTL_FIFO_RESET | EPA_CTL_STALL) != 0,
            Ordering::Release,
        );
    }

    /// R2 for the next tuner reads, one value a read, before the value of
    /// [`FakeRtl::set_vco_indicator`] again: a tuning whose first divider
    /// does not lock and whose next one does (findings 21).
    pub fn queue_vco_indicators(&self, values: &[u8]) {
        self.regs().r2_queue.extend(values.iter().copied());
    }

    /// Whether samples are flowing (started and not paused).
    pub fn flowing(&self) -> bool {
        self.flow.started.load(Ordering::Acquire) && !self.flow.paused.load(Ordering::Acquire)
    }

    /// The bulk endpoint's handle, once a stream opened it.
    pub fn bulk(&self) -> Option<MockHandle> {
        self.bulk.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// A demodulator byte as the fake holds it.
    pub fn demod(&self, page: u8, off: u8) -> u8 {
        self.regs().demod.get(&(page, off)).copied().unwrap_or(0)
    }

    fn decode(value: u16, index: u16, set: bool) -> Target {
        match block_of(index) {
            BLOCK_DEMOD => Target::Demod {
                page: (index & 0x0F) as u8,
                off: (value >> 8) as u8,
            },
            BLOCK_USB => Target::Usb(value.wrapping_sub(USB_BASE)),
            BLOCK_SYS => Target::Sys(value.wrapping_sub(SYS_BASE)),
            BLOCK_TUNER => match (value & 0xFE) as u8 {
                a if a == TUNER_I2C_WRITE => {
                    if set {
                        Target::TunerWrite((value >> 8) as u8)
                    } else {
                        Target::TunerRead
                    }
                }
                a if a == crate::eeprom::EEPROM_I2C_ADDRESS => Target::Eeprom((value >> 8) as u8),
                _ => Target::Other { value, index },
            },
            _ => Target::Other { value, index },
        }
    }

    /// Log the request and apply the rules: `Some(error)` fails it.
    fn admit(regs: &mut Regs, mut req: Request) -> Option<TransferError> {
        let mut failed = None;
        for rule in regs.rules.iter_mut().filter(|r| r.times > 0) {
            if (rule.matches)(&req) {
                if rule.skip > 0 {
                    rule.skip -= 1;
                } else {
                    rule.times -= 1;
                    failed = Some(rule.error);
                    break;
                }
            }
        }
        req.failed = failed.is_some();
        regs.log.push(req);
        failed
    }

    fn get(
        &self,
        value: u16,
        index: u16,
        len: u16,
    ) -> core::result::Result<Vec<u8>, TransferError> {
        let target = FakeRtl::decode(value, index, false);
        let mut regs = self.regs();
        if let Some(e) = FakeRtl::admit(
            &mut regs,
            Request {
                set: false,
                target,
                data: Vec::new(),
                len,
                failed: false,
            },
        ) {
            return Err(e);
        }
        regs.last_get = Some(Instant::now());
        let n = len as usize;
        Ok(match target {
            Target::Demod { page, off } => (0..n)
                .map(|k| {
                    regs.demod
                        .get(&(page, off.wrapping_add(k as u8)))
                        .copied()
                        .unwrap_or(0)
                })
                .collect(),
            Target::Sys(off) => (0..n)
                .map(|k| regs.sys.get(&(off + k as u16)).copied().unwrap_or(0))
                .collect(),
            Target::Usb(off) => (0..n)
                .map(|k| {
                    let o = off + k as u16;
                    if o == BLK_DROP_COUNTER {
                        regs.dropped
                    } else {
                        regs.usb.get(&o).copied().unwrap_or(0)
                    }
                })
                .collect(),
            Target::TunerRead => {
                if !FakeRtl::repeater_open(&regs) {
                    return Err(TransferError::Stall);
                }
                let r2 = if n > 2 {
                    regs.r2_queue.pop_front()
                } else {
                    None
                };
                (0..n)
                    .map(|k| {
                        if k == 2 {
                            r2.unwrap_or(regs.tuner[2])
                        } else {
                            regs.tuner[k % 16]
                        }
                        .reverse_bits()
                    })
                    .collect()
            }
            Target::Eeprom(off) => (0..n)
                .map(|k| regs.eeprom[(off as usize + k) % 256])
                .collect(),
            Target::TunerWrite(_) | Target::Other { .. } => vec![0; n],
        })
    }

    fn set(&self, value: u16, index: u16, data: &[u8]) -> core::result::Result<(), TransferError> {
        let target = FakeRtl::decode(value, index, true);
        let mut regs = self.regs();
        let locked_out = matches!(target, Target::Demod { .. })
            && regs
                .demod_write_needs_read
                .is_some_and(|w| regs.last_get.is_none_or(|t| t.elapsed() >= w));
        if locked_out {
            regs.log.push(Request {
                set: true,
                target,
                data: data.to_vec(),
                len: 0,
                failed: true,
            });
            return Err(TransferError::Stall);
        }
        if let Some(e) = FakeRtl::admit(
            &mut regs,
            Request {
                set: true,
                target,
                data: data.to_vec(),
                len: 0,
                failed: false,
            },
        ) {
            return Err(e);
        }
        match target {
            Target::Demod { page, off } => {
                for (k, &b) in data.iter().enumerate() {
                    regs.demod.insert((page, off.wrapping_add(k as u8)), b);
                }
                if page == START_PAGE && off == START_OFF {
                    self.set_started(data[0] & START_BIT != 0);
                }
                if page == PAUSE_PAGE && off == PAUSE_OFF {
                    self.flow
                        .paused
                        .store(data[0] & PAUSE_BIT != 0, Ordering::Release);
                }
            }
            Target::Sys(off) => {
                for (k, &b) in data.iter().enumerate() {
                    regs.sys.insert(off + k as u16, b);
                }
            }
            Target::Usb(off) => {
                for (k, &b) in data.iter().enumerate() {
                    let o = off + k as u16;
                    if o == BLK_DROP_COUNTER {
                        regs.dropped = b;
                    } else {
                        regs.usb.insert(o, b);
                    }
                }
                self.update_held(&regs);
            }
            Target::TunerWrite(reg) => {
                if !FakeRtl::repeater_open(&regs) {
                    return Err(TransferError::Stall);
                }
                regs.tuner[(reg as usize) % 32] = data[0];
            }
            Target::Eeprom(off) => {
                for (k, &b) in data.iter().enumerate() {
                    regs.eeprom[(off as usize + k) % 256] = b;
                }
            }
            Target::TunerRead | Target::Other { .. } => {}
        }
        Ok(())
    }

    fn repeater_open(regs: &Regs) -> bool {
        regs.demod
            .get(&(IIC_REPEAT_PAGE, IIC_REPEAT_OFF))
            .is_some_and(|b| b & IIC_REPEAT_BIT != 0)
    }

    fn set_started(&self, on: bool) {
        let was = self.flow.started.swap(on, Ordering::AcqRel);
        if on && !was {
            self.flow.sent.store(0, Ordering::Release);
            *self.flow.since.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        }
    }
}

/// Sample k's wire bytes: I = k mod 256, Q = I ^ 0x5A.
pub fn sample_bytes(k: u64) -> [u8; 2] {
    [k as u8, (k as u8) ^ 0x5A]
}

impl Transport for FakeRtl {
    fn control_in(
        &self,
        value: u16,
        index: u16,
        len: u16,
    ) -> TransportFuture<'_, core::result::Result<Vec<u8>, TransferError>> {
        Box::pin(core::future::ready(self.get(value, index, len)))
    }

    fn control_out<'a>(
        &'a self,
        value: u16,
        index: u16,
        data: &'a [u8],
    ) -> TransportFuture<'a, core::result::Result<(), TransferError>> {
        Box::pin(core::future::ready(self.set(value, index, data)))
    }

    fn open_bulk_in(&self) -> TransportFuture<'_, Result<BoxedBulkIn>> {
        let (ep, handle) = mock::endpoint();
        let flow = self.flow.clone();
        handle.set_source(move |want| {
            if flow.held.load(Ordering::Acquire) {
                return Some(Event::Fail(TransferError::Stall, Vec::new()));
            }
            if !flow.started.load(Ordering::Acquire) || flow.paused.load(Ordering::Acquire) {
                return None;
            }
            // Paced: no more than the rate has produced since the start.
            let since = (*flow.since.lock().unwrap_or_else(|p| p.into_inner()))?;
            let due = (since.elapsed().as_secs_f64() * FAKE_RATE_SPS) as u64;
            let sent = flow.sent.load(Ordering::Acquire);
            let n = (want as u64 / 2).min(due.saturating_sub(sent));
            if n == 0 {
                return None;
            }
            flow.sent.store(sent + n, Ordering::Release);
            // The pattern repeats every 256 samples: copied from one cycle.
            let cycle: Vec<u8> = (0..512u64).flat_map(sample_bytes).collect();
            let start = (sent % 256) as usize * 2;
            let mut bytes = Vec::with_capacity(n as usize * 2);
            while bytes.len() < n as usize * 2 {
                let from = if bytes.is_empty() { start } else { 0 };
                let take = (n as usize * 2 - bytes.len()).min(cycle.len() - from);
                bytes.extend_from_slice(&cycle[from..from + take]);
            }
            Some(Event::Data(bytes))
        });
        *self.bulk.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        Box::pin(core::future::ready(Ok(Box::new(ep) as BoxedBulkIn)))
    }
}

/// A label for the fake.
pub fn label() -> String {
    "Fake RTL2832U (serial 00000001)".into()
}
