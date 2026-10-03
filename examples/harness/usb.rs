//! The harness's own minimal vendor-request layer over nusb, written from
//! [RTL2832U] Tables 27-28, plus the bulk-IN byte counter and the
//! power-cycle recovery. Nothing here uses the sdr-rtl library.

use std::process::Command;
use std::time::{Duration, Instant};

use nusb::descriptors::TransferType;
use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, Direction, In, Recipient, TransferError};
use nusb::{DeviceInfo, Endpoint, Interface, MaybeFuture};

use crate::json::{J, hex8, hex16};
use crate::regs::*;

/// The identities the harness accepts; it refuses to write unless exactly
/// one such device is attached.
pub const ACCEPTED_IDS: [(u16, u16); 3] = [(0x0bda, 0x2838), (0x0bda, 0x2832), (0xffff, 0xffff)];

const CONTROL_TIMEOUT: Duration = Duration::from_millis(300);

pub fn matching() -> Vec<DeviceInfo> {
    match nusb::list_devices().wait() {
        Ok(list) => list
            .filter(|d| ACCEPTED_IDS.contains(&(d.vendor_id(), d.product_id())))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Exactly one accepted device, or a refusal naming how many were seen.
pub fn guard() -> Result<DeviceInfo, String> {
    let mut found = matching();
    match found.len() {
        1 => Ok(found.remove(0)),
        n => Err(format!(
            "refusing: {n} devices matching 0bda:2838 / 0bda:2832 / ffff:ffff are attached; exactly one is required"
        )),
    }
}

/// How the demodulator's register offset is placed into wValue. Table 27
/// prints only "Reg's offset in page0~4"; the bench decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemodForm {
    /// wValue = offset | k << 8
    Low(u8),
    /// wValue = offset << 8 | k
    High(u8),
}

impl DemodForm {
    pub fn wvalue(self, offset: u8) -> u16 {
        match self {
            DemodForm::Low(k) => offset as u16 | (k as u16) << 8,
            DemodForm::High(k) => (offset as u16) << 8 | k as u16,
        }
    }

    pub fn describe(self) -> String {
        match self {
            DemodForm::Low(k) => format!("offset | 0x{k:02X}<<8"),
            DemodForm::High(k) => format!("offset<<8 | 0x{k:02X}"),
        }
    }
}

/// The demodulator wValue encoding the bench established in step 1
/// (data/step1-demod-encoding.json): with the demodulator PLL enabled
/// (DEMOD_CTL bit 7, Table 15), wValue = offset << 8 | 0x20 reads 58 of
/// the 59 non-zero documented reset bytes (100 of 101 in all); of the
/// 512 forms only | 0x20 and | 0x21 score above 3, tied. 0x20 is taken:
/// the write-address placement Table 27 uses for "IICAdd" (the tuner's
/// 0x34), and the form step 1D's writes verified by read-back
/// (data/step1-writes.json). The demodulator's own I2C slave of Fig. 5.
pub const DEMOD_FORM: Option<DemodForm> = Some(DemodForm::High(0x20));
/// Byte order of multi-byte system/USB registers established in step 1
/// (data/step1-sys-usb-encoding.json): true = little-endian. After a power
/// cycle I2CMSTR, USB_SYSCTL and USB_EPA_CTL match their documented resets
/// little-endian (3); big-endian matches only USB_EPA_CTL's zero (1).
pub const REG32_LITTLE_ENDIAN: Option<bool> = Some(true);

pub struct Rtl {
    pub dev: nusb::Device,
    pub demod_form: Option<DemodForm>,
}

#[derive(Debug)]
pub enum Fail {
    Transfer(TransferError),
    Short(usize),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Transfer(e) => write!(f, "{e:?}"),
            Fail::Short(n) => write!(f, "short read ({n} bytes)"),
        }
    }
}

impl From<TransferError> for Fail {
    fn from(e: TransferError) -> Fail {
        Fail::Transfer(e)
    }
}

pub type R<T> = Result<T, Fail>;

impl Rtl {
    pub fn open() -> Result<Rtl, String> {
        let info = guard()?;
        let dev = info.open().wait().map_err(|e| format!("open: {e}"))?;
        Ok(Rtl { dev, demod_form: DEMOD_FORM })
    }

    /// A vendor Get: bmRequestType 0xC0, bRequest don't care.
    pub fn get(&self, value: u16, index: u16, len: u16) -> R<Vec<u8>> {
        let v = self
            .dev
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: BREQUEST,
                    value,
                    index,
                    length: len,
                },
                CONTROL_TIMEOUT,
            )
            .wait()?;
        if v.len() != len as usize {
            return Err(Fail::Short(v.len()));
        }
        Ok(v)
    }

    /// A vendor Set: bmRequestType 0x40.
    pub fn set(&self, value: u16, index: u16, data: &[u8]) -> R<()> {
        self.dev
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: BREQUEST,
                    value,
                    index,
                    data,
                },
                CONTROL_TIMEOUT,
            )
            .wait()?;
        Ok(())
    }

    // System and USB blocks: wValue = base + offset (step 1,
    // data/step1-sys-usb-encoding.json).

    pub fn sys_read(&self, off: u16, len: u16) -> R<Vec<u8>> {
        self.get(SYS_BASE + off, windex(BLOCK_SYS, ACCESS_GET, 0), len)
    }
    pub fn sys_write(&self, off: u16, data: &[u8]) -> R<()> {
        self.set(SYS_BASE + off, windex(BLOCK_SYS, ACCESS_SET, 0), data)
    }
    pub fn sys_read8(&self, off: u16) -> R<u8> {
        Ok(self.sys_read(off, 1)?[0])
    }
    pub fn sys_write8(&self, off: u16, v: u8) -> R<()> {
        self.sys_write(off, &[v])
    }
    pub fn usb_read(&self, off: u16, len: u16) -> R<Vec<u8>> {
        self.get(USB_BASE + off, windex(BLOCK_USB, ACCESS_GET, 0), len)
    }
    pub fn usb_write(&self, off: u16, data: &[u8]) -> R<()> {
        self.set(USB_BASE + off, windex(BLOCK_USB, ACCESS_SET, 0), data)
    }
    /// 32-bit USB register in the byte order step 1 established.
    pub fn usb_read32(&self, off: u16) -> R<u32> {
        let b = self.usb_read(off, 4)?;
        Ok(from_bytes32([b[0], b[1], b[2], b[3]]))
    }
    pub fn usb_write32(&self, off: u16, v: u32) -> R<()> {
        self.usb_write(off, &to_bytes32(v))
    }

    fn form(&self) -> DemodForm {
        self.demod_form.expect("the demodulator wValue encoding is not established yet (run step 1)")
    }
    pub fn demod_read(&self, page: u8, off: u8, len: u16) -> R<Vec<u8>> {
        self.get(self.form().wvalue(off), windex(BLOCK_DEMOD, ACCESS_GET, page as u16), len)
    }
    pub fn demod_read8(&self, page: u8, off: u8) -> R<u8> {
        Ok(self.demod_read(page, off, 1)?[0])
    }
    pub fn demod_write8(&self, page: u8, off: u8, v: u8) -> R<()> {
        self.set(self.form().wvalue(off), windex(BLOCK_DEMOD, ACCESS_SET, page as u16), &[v])
    }
    /// Write one byte and read it back; Ok(readback).
    pub fn demod_write8_verify(&self, page: u8, off: u8, v: u8) -> R<u8> {
        self.demod_write8(page, off, v)?;
        self.demod_read8(page, off)
    }
    /// Read-modify-write: set `bits` to `value` within `mask`.
    pub fn demod_rmw(&self, page: u8, off: u8, mask: u8, value: u8) -> R<(u8, u8)> {
        let old = self.demod_read8(page, off)?;
        let new = (old & !mask) | (value & mask);
        let back = self.demod_write8_verify(page, off, new)?;
        Ok((old, back))
    }

    /// The tuner block: wValue = (offset << 8) + I2C address (Table 27).
    pub fn tuner_get(&self, value: u16, len: u16) -> R<Vec<u8>> {
        self.get(value, windex(BLOCK_TUNER, ACCESS_GET, 0), len)
    }
    pub fn tuner_set(&self, value: u16, data: &[u8]) -> R<()> {
        self.set(value, windex(BLOCK_TUNER, ACCESS_SET, 0), data)
    }

    /// The device and configuration descriptors (identity fields only:
    /// no serial, no port path).
    pub fn descriptor_record(&self) -> J {
        let dd = self.dev.device_descriptor();
        let mut cfgs = J::Arr(Vec::new());
        for c in self.dev.configurations() {
            let mut ifaces = J::Arr(Vec::new());
            for i in c.interface_alt_settings() {
                let mut eps = J::Arr(Vec::new());
                for e in i.endpoints() {
                    eps.push(
                        J::obj()
                            .with("address", hex8(e.address()))
                            .with("direction", if e.direction() == Direction::In { "IN" } else { "OUT" })
                            .with("transfer_type", format!("{:?}", e.transfer_type()))
                            .with("max_packet_size", e.max_packet_size())
                            .with("interval", e.interval()),
                    );
                }
                ifaces.push(
                    J::obj()
                        .with("interface", i.interface_number())
                        .with("alt_setting", i.alternate_setting())
                        .with("class", hex8(i.class()))
                        .with("subclass", hex8(i.subclass()))
                        .with("protocol", hex8(i.protocol()))
                        .with("endpoints", eps),
                );
            }
            cfgs.push(
                J::obj()
                    .with("configuration_value", c.configuration_value())
                    .with("attributes", hex8(c.attributes()))
                    .with("max_power_2mA", c.max_power())
                    .with("interfaces", ifaces),
            );
        }
        J::obj()
            .with("vid", hex16(dd.vendor_id()))
            .with("pid", hex16(dd.product_id()))
            .with("bcdUSB", hex16(dd.usb_version()))
            .with("bcdDevice", hex16(dd.device_version()))
            .with("class", hex8(dd.class()))
            .with("max_packet_size_0", dd.max_packet_size_0())
            .with("speed", format!("{:?}", self.dev.speed()))
            .with("configurations", cfgs)
    }

    /// The (interface, endpoint address, max packet) of the first bulk IN
    /// endpoint in the active configuration.
    pub fn bulk_in(&self) -> Option<(u8, u8, usize)> {
        let cfg = self.dev.active_configuration().ok()?;
        for i in cfg.interface_alt_settings() {
            for e in i.endpoints() {
                if e.direction() == Direction::In && e.transfer_type() == TransferType::Bulk {
                    return Some((i.interface_number(), e.address(), e.max_packet_size()));
                }
            }
        }
        None
    }
}

fn little() -> bool {
    REG32_LITTLE_ENDIAN.expect("the register byte order is not established yet (run step 1)")
}
pub fn from_bytes32(b: [u8; 4]) -> u32 {
    if little() { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
}
pub fn to_bytes32(v: u32) -> [u8; 4] {
    if little() { v.to_le_bytes() } else { v.to_be_bytes() }
}

/// Keeps [`BulkCounter::QUEUED`] bulk IN transfers in flight at all times
/// and counts what arrives.
///
/// The host polls a bulk IN endpoint only while it has a transfer queued
/// for it; with none queued the bus shows no IN tokens and a stream would
/// go unseen. So every completion is resubmitted at once, the queue is
/// topped up at the start of each window, and the [`Check`] records what
/// the host did. A completion with `Stall` means the endpoint answered
/// STALL and the host halted its pipe: per USB 2.0 §9.4.5 a halted
/// endpoint stays halted until the host sends ClearFeature(ENDPOINT_HALT)
/// (§9.4.1), which also resets the data toggle, so the counter cancels
/// what is still queued (nusb: no transfers pending while clearing),
/// sends the clear, records its result and queues again.
pub struct BulkCounter {
    _iface: Interface,
    ep: Endpoint<Bulk, In>,
    buf_len: usize,
    check: Check,
    /// Clear a halt when one is seen (always, except in detector-check's
    /// phase that shows what resubmitting into a halted pipe does).
    pub clear_halts: bool,
    /// A Stall was seen and no ClearFeature(ENDPOINT_HALT) has succeeded
    /// since. While halted the host sends no IN tokens (measured:
    /// data/step3-detector-check.json) and transfers resubmitted into the
    /// pipe neither complete nor fail, so a window looks empty, not broken.
    halted: bool,
}

/// The detector's self-check over one span (from the previous window's
/// end to this window's end, so the control phase before a window is
/// covered too).
#[derive(Clone, Debug, Default)]
pub struct Check {
    /// Transfers submitted (resubmissions and top-ups).
    pub submitted: usize,
    /// Completions taken, with or without error.
    pub completed: usize,
    /// Completions whose status was an error.
    pub errored: usize,
    /// (error, count) in order of first appearance; none is dropped.
    pub errors: Vec<(String, usize)>,
    /// `Stall` completions: the endpoint was halted.
    pub halts: usize,
    /// The result of each ClearFeature(ENDPOINT_HALT) sent.
    pub clears: Vec<String>,
    /// A `Disconnected` completion was seen; nothing is resubmitted after it.
    pub disconnected: bool,
    /// The endpoint was halted (a Stall not yet cleared) at any point in
    /// the span, including at its start or end.
    pub halted: bool,
    /// Transfers in flight when the span began, the fewest seen at any
    /// moment (a completion taken, before its resubmission), and at the end.
    pub in_flight_start: usize,
    pub in_flight_min: usize,
    pub in_flight_end: usize,
}

impl Check {
    fn note(&mut self, e: TransferError) {
        self.errored += 1;
        let k = format!("{e:?}");
        match self.errors.iter_mut().find(|(n, _)| *n == k) {
            Some((_, c)) => *c += 1,
            None => self.errors.push((k, 1)),
        }
    }

    /// The host kept at least one transfer queued throughout, with no
    /// error, halt or disconnection.
    pub fn clean(&self) -> bool {
        self.errored == 0 && self.halts == 0 && !self.halted && !self.disconnected && self.in_flight_min > 0
    }

    /// "Stall x2", one entry per error kind.
    pub fn error_list(&self) -> Vec<String> {
        self.errors.iter().map(|(k, n)| if *n == 1 { k.clone() } else { format!("{k} x{n}") }).collect()
    }

    /// Compact form for the scan records: [submitted, completed,
    /// errored, halts, fewest in flight, halted (0/1)].
    pub fn compact(&self) -> J {
        J::Arr(vec![
            J::from(self.submitted),
            J::from(self.completed),
            J::from(self.errored),
            J::from(self.halts),
            J::from(self.in_flight_min),
            J::from(self.halted as u8),
        ])
    }

    pub fn record(&self) -> J {
        J::obj()
            .with("submitted", self.submitted)
            .with("completed", self.completed)
            .with("errored", self.errored)
            .with("errors", J::Arr(self.error_list().into_iter().map(J::from).collect()))
            .with("halts", self.halts)
            .with("halt_clears", J::Arr(self.clears.iter().map(|c| J::from(c.as_str())).collect()))
            .with("disconnected", self.disconnected)
            .with("halted", self.halted)
            .with("in_flight_start", self.in_flight_start)
            .with("in_flight_min", self.in_flight_min)
            .with("in_flight_end", self.in_flight_end)
            .with("clean", self.clean())
    }
}

pub struct Window {
    pub bytes: usize,
    pub elapsed: Duration,
    pub completions: usize,
    /// The check's errors, one entry per kind.
    pub errors: Vec<String>,
    pub stale_bytes: usize,
    /// The first [`Window::SIZES_KEPT`] completion lengths, in order.
    pub sizes: Vec<usize>,
    pub check: Check,
}

impl Window {
    pub const SIZES_KEPT: usize = 64;

    pub fn rate(&self) -> f64 {
        self.bytes as f64 / self.elapsed.as_secs_f64()
    }
}

/// The first-byte wait's result.
pub struct FirstByte {
    /// From the call to the first non-empty completion (zero if one had
    /// already completed); None when nothing arrived before the deadline.
    pub latency: Option<Duration>,
    /// The bytes of every completion taken, the first non-empty one last.
    pub bytes: usize,
    pub check: Check,
}

impl BulkCounter {
    pub const QUEUED: usize = 4;

    /// The detector in words, for a record's setup line; `fields` says
    /// how the line carries the self-check.
    pub fn describe(buf_len: usize, fields: &str) -> String {
        format!(
            "{} bulk IN transfers of {buf_len} bytes kept in flight: every completion resubmitted at once, topped up before each wait; a Stall completion = endpoint halted -> queued transfers cancelled, ClearFeature(ENDPOINT_HALT) sent (USB 2.0 9.4.1, 9.4.5), its result recorded, queue refilled; every error completion counted by kind. {fields}",
            Self::QUEUED
        )
    }

    pub fn open(rtl: &Rtl, buf_len: usize) -> Result<BulkCounter, String> {
        let (ifn, addr, _mps) = rtl.bulk_in().ok_or("no bulk IN endpoint in the descriptors")?;
        let iface = rtl
            .dev
            .claim_interface(ifn)
            .wait()
            .map_err(|e| format!("claim interface {ifn}: {e}"))?;
        let ep = iface
            .endpoint::<Bulk, In>(addr)
            .map_err(|e| format!("endpoint 0x{addr:02X}: {e}"))?;
        let mut c = BulkCounter { _iface: iface, ep, buf_len, check: Check::default(), clear_halts: true, halted: false };
        c.top_up();
        c.check = c.fresh_check();
        Ok(c)
    }

    fn fresh_check(&self) -> Check {
        let n = self.ep.pending();
        Check { in_flight_start: n, in_flight_min: n, halted: self.halted, ..Check::default() }
    }

    /// Queue new transfers until [`Self::QUEUED`] are in flight.
    fn top_up(&mut self) {
        while !self.check.disconnected && self.ep.pending() < Self::QUEUED {
            let b = self.ep.allocate(self.buf_len);
            self.ep.submit(b);
            self.check.submitted += 1;
        }
    }

    /// Before a wait: retry the clear of a halt that a failed clear left in
    /// place, then top up. Returns the bytes of transfers cancelled for it.
    fn ready(&mut self) -> usize {
        let n = if self.halted && self.clear_halts { self.clear_halt(None) } else { 0 };
        self.top_up();
        n
    }

    /// The check since the previous one was taken; a new span starts.
    pub fn take_check(&mut self) -> Check {
        self.top_up();
        let mut c = std::mem::take(&mut self.check);
        c.in_flight_end = self.ep.pending();
        c.halted |= self.halted;
        self.check = self.fresh_check();
        c
    }

    /// Account one completion and resubmit its buffer; on a halt, clear
    /// it. Returns the bytes received (the completion's, plus any held by
    /// transfers cancelled for the clear).
    fn take(&mut self, c: nusb::transfer::Completion, mut keep: Option<&mut Vec<u8>>, sizes: Option<&mut Vec<usize>>) -> usize {
        self.check.completed += 1;
        self.check.in_flight_min = self.check.in_flight_min.min(self.ep.pending());
        let mut n = c.actual_len;
        if let Some(k) = keep.as_deref_mut() {
            k.extend_from_slice(&c.buffer[..n]);
        }
        if let Some(s) = sizes
            && s.len() < Window::SIZES_KEPT
        {
            s.push(n);
        }
        let mut halted = false;
        if let Err(e) = c.status {
            self.check.note(e);
            match e {
                TransferError::Stall => {
                    self.check.halts += 1;
                    self.check.halted = true;
                    self.halted = true;
                    halted = true;
                }
                TransferError::Disconnected => self.check.disconnected = true,
                _ => {}
            }
        }
        if halted && self.clear_halts {
            n += self.clear_halt(keep);
        } else if !self.check.disconnected {
            let mut b = c.buffer;
            b.clear();
            self.ep.submit(b);
            self.check.submitted += 1;
        }
        n
    }

    /// Cancel, clear the halt and queue again, whether or not one was seen.
    pub fn force_clear_halt(&mut self) -> usize {
        self.clear_halt(None)
    }

    /// Cancel what is queued, take every cancelled transfer (counted, bytes
    /// kept), send ClearFeature(ENDPOINT_HALT), record its result, and
    /// queue again.
    fn clear_halt(&mut self, mut keep: Option<&mut Vec<u8>>) -> usize {
        let mut n = 0;
        self.ep.cancel_all();
        let end = Instant::now() + Duration::from_secs(1);
        while self.ep.pending() > 0 {
            let now = Instant::now();
            if now >= end {
                break;
            }
            if let Some(c) = self.ep.wait_next_complete(end - now) {
                self.check.completed += 1;
                n += c.actual_len;
                if let Some(k) = keep.as_deref_mut() {
                    k.extend_from_slice(&c.buffer[..c.actual_len]);
                }
                if let Err(e) = c.status {
                    self.check.note(e);
                    match e {
                        TransferError::Stall => {
                            self.check.halts += 1;
                            self.check.halted = true;
                        }
                        TransferError::Disconnected => self.check.disconnected = true,
                        _ => {}
                    }
                }
            }
        }
        self.check.in_flight_min = self.check.in_flight_min.min(self.ep.pending());
        let left = self.ep.pending();
        let r = self.ep.clear_halt().wait();
        let mut what = match r {
            Ok(()) => {
                self.halted = false;
                "ok".to_string()
            }
            Err(e) => format!("error: {e}"),
        };
        if left > 0 {
            what.push_str(&format!(" ({left} transfers still pending at the clear)"));
        }
        self.check.clears.push(what);
        self.top_up();
        n
    }

    /// Wait up to `timeout` for a completion; None when nothing completed
    /// or nothing is in flight (after a disconnection), in which case the
    /// wait is slept out so a window keeps its length.
    fn next(&mut self, timeout: Duration) -> Option<nusb::transfer::Completion> {
        if self.ep.pending() == 0 {
            std::thread::sleep(timeout);
            return None;
        }
        self.ep.wait_next_complete(timeout)
    }

    /// Discard whatever completed before now, then count bytes for `window`.
    /// If `keep` is given, received bytes are appended to it. The window's
    /// [`Check`] covers the span since the previous window ended.
    pub fn measure(&mut self, window: Duration, mut keep: Option<&mut Vec<u8>>) -> Window {
        let mut stale = self.ready();
        while let Some(c) = self.next(Duration::ZERO) {
            stale += self.take(c, None, None);
        }
        let start = Instant::now();
        let deadline = start + window;
        let mut bytes = 0;
        let mut completions = 0;
        let mut sizes = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(c) = self.next(deadline - now) {
                completions += 1;
                bytes += self.take(c, keep.as_deref_mut(), Some(&mut sizes));
            }
        }
        let elapsed = start.elapsed();
        let check = self.take_check();
        Window { bytes, elapsed, completions, errors: check.error_list(), stale_bytes: stale, sizes, check }
    }

    /// Take every transfer that has already completed (counted in the
    /// check, resubmitted) and return the bytes it held (discarded).
    pub fn drain(&mut self) -> usize {
        let mut n = self.ready();
        while let Some(c) = self.next(Duration::ZERO) {
            n += self.take(c, None, None);
        }
        n
    }

    /// Count bytes from now until the first non-empty completion or
    /// `deadline`. Completions already waiting count, with latency zero.
    /// The check covers the span since the previous window ended.
    pub fn first_byte(&mut self, deadline: Duration) -> FirstByte {
        let mut bytes = self.ready();
        let start = Instant::now();
        let end = start + deadline;
        let mut latency = None;
        loop {
            let now = Instant::now();
            if now >= end {
                break;
            }
            if let Some(c) = self.next(end - now) {
                let n = self.take(c, None, None);
                bytes += n;
                if n > 0 {
                    latency = Some(start.elapsed());
                    break;
                }
            }
        }
        FirstByte { latency, bytes, check: self.take_check() }
    }

    /// Cancel and drain every queued transfer.
    pub fn close(mut self) {
        self.ep.cancel_all();
        while self.ep.pending() > 0 {
            if self.ep.wait_next_complete(Duration::from_secs(1)).is_none() {
                break;
            }
        }
    }
}

/// TARGET-A power through the analyzer's own tool.
pub fn vbus(on: bool) -> Result<String, String> {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/Cargo.toml");
    let out = Command::new("cargo")
        .args(["run", "--quiet", "--manifest-path", manifest, "--release", "--bin", "cynthion-witness", "--", "vbus"])
        .arg(if on { "on" } else { "off" })
        .output()
        .map_err(|e| format!("spawn cargo: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        return Err(format!("vbus {}: {} {}", on, text, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(text)
}

/// Poll until the number of accepted devices equals `want`, up to `limit`.
pub fn wait_count(want: usize, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if matching().len() == want {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// One off/on cycle with `off_time` between the device leaving and power
/// returning.
fn cycle_once(off_time: Duration) -> Result<(J, bool), String> {
    let before = matching().len();
    let off = vbus(false)?;
    let gone = wait_count(0, Duration::from_secs(8));
    std::thread::sleep(off_time);
    let on = vbus(true)?;
    let back = wait_count(1, Duration::from_secs(15));
    // Enumeration settle time.
    std::thread::sleep(Duration::from_secs(2));
    let after = matching().len();
    let rec = J::obj()
        .with("devices_before", before)
        .with("vbus_off_reply", off)
        .with("disappeared_after_ms", gone.map(|d| d.as_millis() as u64))
        .with("off_time_ms", off_time.as_millis() as u64)
        .with("vbus_on_reply", on)
        .with("reappeared_after_ms", back.map(|d| d.as_millis() as u64))
        .with("devices_after_settle", after);
    let ok = gone.is_some() && back.is_some() && after == 1;
    Ok((rec, ok))
}

/// Power-cycle the dongle; returns a record of the cycle. Power is off
/// for 500 ms; once (step 2's first ablation cycle) the dongle did not
/// return after that and did after 3 s off, so a failed cycle is retried
/// once with 3 s off, and the record says so.
pub fn power_cycle() -> Result<J, String> {
    let (rec, ok) = cycle_once(Duration::from_millis(500))?;
    if ok {
        return Ok(rec);
    }
    let (retry, ok) = cycle_once(Duration::from_secs(3))?;
    let rec = retry.with("retry_after_failed_cycle", rec);
    if !ok {
        return Err(format!("power cycle did not complete: {}", rec.render()));
    }
    Ok(rec)
}
