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
/// (data/step1-demod-encoding.json). None until step 1 has run.
pub const DEMOD_FORM: Option<DemodForm> = None;
/// Byte order of multi-byte system/USB registers established in step 1
/// (data/step1-sys-usb-encoding.json): true = little-endian. None until then.
pub const REG32_LITTLE_ENDIAN: Option<bool> = None;

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

/// Keeps a few bulk IN transfers queued and counts what arrives.
pub struct BulkCounter {
    _iface: Interface,
    ep: Endpoint<Bulk, In>,
}

pub struct Window {
    pub bytes: usize,
    pub elapsed: Duration,
    pub completions: usize,
    pub errors: Vec<String>,
    pub stale_bytes: usize,
}

impl Window {
    pub fn rate(&self) -> f64 {
        self.bytes as f64 / self.elapsed.as_secs_f64()
    }
}

impl BulkCounter {
    pub const QUEUED: usize = 4;

    pub fn open(rtl: &Rtl, buf_len: usize) -> Result<BulkCounter, String> {
        let (ifn, addr, _mps) = rtl.bulk_in().ok_or("no bulk IN endpoint in the descriptors")?;
        let iface = rtl
            .dev
            .claim_interface(ifn)
            .wait()
            .map_err(|e| format!("claim interface {ifn}: {e}"))?;
        let mut ep = iface
            .endpoint::<Bulk, In>(addr)
            .map_err(|e| format!("endpoint 0x{addr:02X}: {e}"))?;
        for _ in 0..Self::QUEUED {
            let b = ep.allocate(buf_len);
            ep.submit(b);
        }
        Ok(BulkCounter { _iface: iface, ep })
    }

    /// Discard whatever completed before now, then count bytes for `window`.
    /// If `keep` is given, received bytes are appended to it.
    pub fn measure(&mut self, window: Duration, mut keep: Option<&mut Vec<u8>>) -> Window {
        let mut stale = 0;
        while let Some(c) = self.ep.wait_next_complete(Duration::ZERO) {
            stale += c.actual_len;
            let mut b = c.buffer;
            b.clear();
            self.ep.submit(b);
        }
        let start = Instant::now();
        let deadline = start + window;
        let mut bytes = 0;
        let mut completions = 0;
        let mut errors = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(c) = self.ep.wait_next_complete(deadline - now) {
                completions += 1;
                bytes += c.actual_len;
                if let Some(k) = keep.as_deref_mut() {
                    k.extend_from_slice(&c.buffer[..c.actual_len]);
                }
                if let Err(e) = c.status {
                    if errors.len() < 8 {
                        errors.push(format!("{e:?}"));
                    }
                    if matches!(e, TransferError::Disconnected) {
                        break;
                    }
                }
                let mut b = c.buffer;
                b.clear();
                self.ep.submit(b);
            }
        }
        Window { bytes, elapsed: start.elapsed(), completions, errors, stale_bytes: stale }
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

/// Power-cycle the dongle; returns a record of the cycle.
pub fn power_cycle() -> Result<J, String> {
    let before = matching().len();
    let off = vbus(false)?;
    let gone = wait_count(0, Duration::from_secs(8));
    std::thread::sleep(Duration::from_millis(500));
    let on = vbus(true)?;
    let back = wait_count(1, Duration::from_secs(15));
    // Enumeration settle time.
    std::thread::sleep(Duration::from_secs(2));
    let after = matching().len();
    let rec = J::obj()
        .with("devices_before", before)
        .with("vbus_off_reply", off)
        .with("disappeared_after_ms", gone.map(|d| d.as_millis() as u64))
        .with("vbus_on_reply", on)
        .with("reappeared_after_ms", back.map(|d| d.as_millis() as u64))
        .with("devices_after_settle", after);
    if gone.is_none() || back.is_none() || after != 1 {
        return Err(format!("power cycle did not complete: {}", rec.render()));
    }
    Ok(rec)
}
