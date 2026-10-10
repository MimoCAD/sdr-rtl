//! The vendor-request layer over nusb: the Get and Set of [RTL2832U]
//! Tables 27-28 with the wValue forms the bench established
//! (`protocol`), and the register accessors built on them. Every write
//! that can be read back is read back here; the one register file that
//! cannot be (the tuner's R16-R31, findings step 4a) is marked where it
//! is written. Ported from the harness's request layer
//! (`examples/harness/usb.rs`), which every bench record was taken
//! through.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError};
use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::eeprom::{EEPROM_ADDRESS_SPACE, EEPROM_BLOCK, EEPROM_I2C_ADDRESS, EEPROM_REQUEST_MAX, EEPROM_WRITE_CHUNK, block0_addresses_eeprom, wvalue_eeprom};
use crate::image::{CLK_OUT_ENB, R15};
use crate::protocol::*;

/// The control-transfer timeout the harness used for every bench
/// request (`examples/harness/usb.rs` CONTROL_TIMEOUT): a demodulator
/// access NAKs its data stage for 1-5 ms (findings step 3, the detector
/// on the wire), far inside it.
pub const CONTROL_TIMEOUT: Duration = Duration::from_millis(300);

/// How often a refused (STALLed) request is tried again, and how far apart.
pub const STALL_RETRIES: u32 = 3;
pub const STALL_RETRY_WAIT: Duration = Duration::from_millis(1);

/// An open dongle's control pipe. Clones share the device and the state
/// below: the receiver and its stream each hold one, possibly on two
/// threads.
#[derive(Clone)]
pub struct Link {
    dev: nusb::Device,
    shared: Arc<Shared>,
}

/// What the clones of one link share.
struct Shared {
    /// Held across every read-modify-write, so two clones never
    /// interleave on one register (page 1 0x01 carries both the I2C
    /// repeater and the pause bit).
    rmw: Mutex<()>,
    /// A stream exists (from `Rtl::rx_stream` until the stream drops).
    stream_open: AtomicBool,
    /// Requests the chip refused (STALL) and that were tried again.
    stall_retries: AtomicU64,
}

fn usb_error(context: &'static str, e: impl Display) -> Error {
    Error::Usb { backend: BACKEND, context, message: e.to_string() }
}

fn device_error(context: &'static str, message: String) -> Error {
    Error::Device { backend: BACKEND, context, message }
}

/// Every request passes here first: block 0 addressed to the EEPROM's
/// slave (0xA0 or 0xA1 in wValue's low byte) is refused, Get or Set —
/// block 0's page write would put the page number into the EEPROM's
/// byte 0, the signature (findings 5c, 17; eeprom.md §8;
/// `eeprom::block0_addresses_eeprom`).
fn refuse_block0_eeprom(value: u16, index: u16, context: &'static str) -> Result<()> {
    if block0_addresses_eeprom(value, index) {
        return Err(device_error(
            context,
            format!(
                "refused wValue 0x{value:04X} wIndex 0x{index:04X}: block 0 at the EEPROM's slave 0x{EEPROM_I2C_ADDRESS:02X} writes its page number into the EEPROM's byte 0, the signature (findings 17); the EEPROM is reached through block 3"
            ),
        ));
    }
    Ok(())
}

impl Link {
    pub fn new(dev: nusb::Device) -> Link {
        Link { dev, shared: Arc::new(Shared { rmw: Mutex::new(()), stream_open: AtomicBool::new(false), stall_retries: AtomicU64::new(0) }) }
    }

    pub fn device(&self) -> &nusb::Device {
        &self.dev
    }

    fn rmw_lock(&self) -> MutexGuard<'_, ()> {
        // A panic while holding the lock leaves no register half-written
        // that the next holder could not read back; take it regardless.
        self.shared.rmw.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark a stream as existing; false when one already does.
    pub fn claim_stream(&self) -> bool {
        self.shared.stream_open.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// The stream is gone.
    pub fn release_stream(&self) {
        self.shared.stream_open.store(false, Ordering::Release);
    }

    /// Whether a stream exists.
    pub fn stream_open(&self) -> bool {
        self.shared.stream_open.load(Ordering::Acquire)
    }

    /// A vendor Get: bmRequestType 0xC0 (vendor, device, IN), bRequest
    /// don't care, wLength = bytes to read (Table 27). A block 0 request
    /// to the EEPROM's slave is refused ([`refuse_block0_eeprom`]).
    pub fn get(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
        refuse_block0_eeprom(value, index, context)?;
        let v = self
            .retry_stall(|| {
                self.dev
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
                    .wait()
            })
            .map_err(|e| usb_error(context, e))?;
        if v.len() != len as usize {
            return Err(usb_error(context, format!("short read: {} of {len} bytes", v.len())));
        }
        Ok(v)
    }

    /// A vendor Set: bmRequestType 0x40 (vendor, device, OUT), the bytes
    /// in the data stage (Table 27). A block 0 request to the EEPROM's
    /// slave is refused ([`refuse_block0_eeprom`]).
    pub fn set(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
        refuse_block0_eeprom(value, index, context)?;
        self.retry_stall(|| {
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
                .wait()
        })
        .map_err(|e| usb_error(context, e))
    }

    /// A request the chip refused with a STALL tried again, up to
    /// [`STALL_RETRIES`] times, [`STALL_RETRY_WAIT`] apart: now and then the
    /// chip refuses a request (findings 16.4, 16.6 — "endpoint stalled",
    /// intermittent, no order or rate isolated). Every request here is
    /// safe to repeat: reads, writes of whole values, and read-modify-
    /// writes judged by their read-back. Each retry is counted
    /// ([`Link::stall_retries`]).
    fn retry_stall<T>(&self, mut f: impl FnMut() -> std::result::Result<T, TransferError>) -> std::result::Result<T, TransferError> {
        let mut tries = 0;
        loop {
            match f() {
                Err(TransferError::Stall) if tries < STALL_RETRIES => {
                    tries += 1;
                    self.shared.stall_retries.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(STALL_RETRY_WAIT);
                }
                r => return r,
            }
        }
    }

    /// Requests the chip refused and that were tried again, since open.
    pub fn stall_retries(&self) -> u64 {
        self.shared.stall_retries.load(Ordering::Relaxed)
    }

    // ---- System block: wValue = 0x3000 + offset (findings step 1A) ----

    pub fn sys_read8(&self, off: u16) -> Result<u8> {
        Ok(self.get(wvalue_sys(off), windex(BLOCK_SYS, ACCESS_GET, 0), 1, "system register read")?[0])
    }

    fn sys_write8(&self, off: u16, v: u8) -> Result<()> {
        self.set(wvalue_sys(off), windex(BLOCK_SYS, ACCESS_SET, 0), &[v], "system register write")
    }

    /// Read-modify-write one system byte: clear `clear`, set `set`, then
    /// read back and compare the bits touched (DEMOD_CTL's bit 2 never
    /// reads back, findings step 3's block scan, so only the written bits
    /// are judged). Returns the byte read back.
    pub fn sys_rmw8(&self, off: u16, clear: u8, set: u8) -> Result<u8> {
        let _held = self.rmw_lock();
        let old = self.sys_read8(off)?;
        let new = (old & !clear) | set;
        self.sys_write8(off, new)?;
        let back = self.sys_read8(off)?;
        let mask = clear | set;
        if back & mask != new & mask {
            return Err(device_error(
                "system register read-back",
                format!("0x{:04X}: wrote 0x{new:02X}, read 0x{back:02X} (mask 0x{mask:02X})", wvalue_sys(off)),
            ));
        }
        Ok(back)
    }

    /// Read-modify-write one system byte and read it back without judging
    /// it: for the registers whose read-back the bench has not shown
    /// (GPD and GPOE — step 1A read their power-on values, no step wrote
    /// them with a verified read-back). Returns the byte read back.
    pub fn sys_rmw8_unjudged(&self, off: u16, clear: u8, set: u8) -> Result<u8> {
        let _held = self.rmw_lock();
        let old = self.sys_read8(off)?;
        self.sys_write8(off, (old & !clear) | set)?;
        self.sys_read8(off)
    }

    /// Write one whole system byte and read it back; every bit must
    /// agree except `not_read_back` (DEMOD_CTL's bit 2, findings step 3).
    pub fn sys_write8_verified(&self, off: u16, v: u8, not_read_back: u8) -> Result<()> {
        self.sys_write8(off, v)?;
        let back = self.sys_read8(off)?;
        if back & !not_read_back != v & !not_read_back {
            return Err(device_error(
                "system register read-back",
                format!("0x{:04X}: wrote 0x{v:02X}, read 0x{back:02X}", wvalue_sys(off)),
            ));
        }
        Ok(())
    }

    // ---- USB block: wValue = 0x2000 + offset (findings step 1A) ----

    pub fn usb_read8(&self, off: u16) -> Result<u8> {
        Ok(self.get(wvalue_usb(off), windex(BLOCK_USB, ACCESS_GET, 0), 1, "USB register read")?[0])
    }

    /// One USB-block byte written. Offset 0x1C is refused: it is the
    /// test-mode register whose write wedges the chip
    /// (derivation/HAZARD-usb-0x1C.md).
    pub fn usb_write8(&self, off: u16, v: u8) -> Result<()> {
        if off == USB_HAZARD_OFF {
            return Err(device_error("USB register write", format!("USB block 0x{off:03X} is the test-mode register; never written")));
        }
        self.set(wvalue_usb(off), windex(BLOCK_USB, ACCESS_SET, 0), &[v], "USB register write")
    }

    /// `len` USB-block bytes from `off` (a 32-bit register reads
    /// little-endian, findings step 1A).
    pub fn usb_read_n(&self, off: u16, len: u16) -> Result<Vec<u8>> {
        self.get(wvalue_usb(off), windex(BLOCK_USB, ACCESS_GET, 0), len, "USB register read")
    }

    /// USB-block bytes written from `off`; refused if they would cover
    /// offset 0x1C (derivation/HAZARD-usb-0x1C.md).
    pub fn usb_write_n(&self, off: u16, data: &[u8]) -> Result<()> {
        if (off..off + data.len() as u16).contains(&USB_HAZARD_OFF) {
            return Err(device_error("USB register write", format!("0x{off:03X}+{}: covers the test-mode register 0x01C; never written", data.len())));
        }
        self.set(wvalue_usb(off), windex(BLOCK_USB, ACCESS_SET, 0), data, "USB register write")
    }

    // ---- Demodulator: wValue = offset << 8 | 0x20 (findings step 1B/1C) ----

    /// `len` bytes from `off` on `page`; multi-byte reads auto-increment
    /// (findings step 1D).
    pub fn demod_read(&self, page: u8, off: u8, len: u16) -> Result<Vec<u8>> {
        self.get(wvalue_demod(off), windex(BLOCK_DEMOD, ACCESS_GET, page as u16), len, "demodulator register read")
    }

    pub fn demod_read8(&self, page: u8, off: u8) -> Result<u8> {
        Ok(self.demod_read(page, off, 1)?[0])
    }

    fn demod_write8(&self, page: u8, off: u8, v: u8) -> Result<()> {
        self.demod_write(page, off, &[v])
    }

    /// Consecutive registers from `off` in one Set (wLength = the byte
    /// count), as the stream sequence writes page 1 0x16-0x17, 0x19-0x1A
    /// and the rsamp_ratio pairs (findings 9b; the harness's `demod_write`,
    /// `examples/harness/usb.rs`).
    pub fn demod_write(&self, page: u8, off: u8, data: &[u8]) -> Result<()> {
        self.set(wvalue_demod(off), windex(BLOCK_DEMOD, ACCESS_SET, page as u16), data, "demodulator register write")
    }

    /// [`Link::demod_write`], then the same registers read back in one
    /// auto-incrementing Get (findings step 1D) and compared whole: every
    /// write of the stream sequence read back as written on the bench
    /// (findings 10.1: "All 47 read-backs match").
    pub fn demod_write_verified(&self, page: u8, off: u8, data: &[u8]) -> Result<()> {
        self.demod_write(page, off, data)?;
        let back = self.demod_read(page, off, data.len() as u16)?;
        if back != data {
            return Err(device_error(
                "demodulator register read-back",
                format!("page {page} 0x{off:02X}: wrote {data:02X?}, read {back:02X?}"),
            ));
        }
        Ok(())
    }

    /// Read-modify-write one demodulator byte: the bits of `mask` take
    /// `value`, the rest are kept; the read-back must agree on `mask`.
    /// Returns the byte read back.
    pub fn demod_rmw8(&self, page: u8, off: u8, mask: u8, value: u8) -> Result<u8> {
        let _held = self.rmw_lock();
        let old = self.demod_read8(page, off)?;
        let new = (old & !mask) | (value & mask);
        self.demod_write8(page, off, new)?;
        let back = self.demod_read8(page, off)?;
        if back & mask != new & mask {
            return Err(device_error(
                "demodulator register read-back",
                format!("page {page} 0x{off:02X}: wrote 0x{new:02X}, read 0x{back:02X} (mask 0x{mask:02X})"),
            ));
        }
        Ok(back)
    }

    // ---- The EEPROM: block 3, wValue = offset << 8 | 0xA0 (findings 17) ----

    /// `len` EEPROM bytes from `offset`: one block 3 Get, at most 16 bytes
    /// (one I²C transaction, eeprom.md §1, §8), within the 256 offsets.
    pub fn eeprom_read(&self, offset: u8, len: usize) -> Result<Vec<u8>> {
        if len == 0 || len > EEPROM_REQUEST_MAX || offset as usize + len > EEPROM_ADDRESS_SPACE {
            return Err(device_error("EEPROM read", format!("0x{offset:02X}+{len}: 1-{EEPROM_REQUEST_MAX} bytes within the {EEPROM_ADDRESS_SPACE} offsets")));
        }
        self.get(wvalue_eeprom(offset), windex(EEPROM_BLOCK, ACCESS_GET, 0), len as u16, "EEPROM read")
    }

    /// EEPROM bytes written from `offset`: one block 3 Set, unverified
    /// here (`eeprom::write` reads the part back). At most
    /// `EEPROM_WRITE_CHUNK` (8) bytes and never across an 8-byte boundary,
    /// so the firmware makes it exactly one I²C write (eeprom.md §8: a
    /// second write in one request meets the part in its write cycle and
    /// STALLs) and it stays inside one write page (findings 17). The caller waits out the
    /// write cycle (`eeprom::EEPROM_WRITE_CYCLE`) before the next request.
    pub fn eeprom_write(&self, offset: u8, data: &[u8]) -> Result<()> {
        let start = offset as usize;
        if data.is_empty() || start / EEPROM_WRITE_CHUNK != (start + data.len() - 1) / EEPROM_WRITE_CHUNK {
            return Err(device_error(
                "EEPROM write",
                format!("0x{offset:02X}+{}: 1-{EEPROM_WRITE_CHUNK} bytes inside one {EEPROM_WRITE_CHUNK}-byte-aligned chunk", data.len()),
            ));
        }
        self.set(wvalue_eeprom(offset), windex(EEPROM_BLOCK, ACCESS_SET, 0), data, "EEPROM write")
    }

    // ---- Tuner: wValue (reg << 8) | 0x34 to write, 0x0034 to read (step 2) ----

    /// Run tuner accesses with the I2C repeater open (page 1 0x01 bit 3,
    /// Table 3; findings step 2: the tuner answers only with it set), and
    /// close it after, also when an access failed (the access's error is
    /// the one returned). Every tuner access of the driver goes through
    /// here, so between accesses page 1 0x01 rests at its power-on 0x10,
    /// the state the pause bit was measured from (findings 10.2c) and
    /// the vendor driver kept (findings 9a).
    pub fn with_tuner<T>(&self, f: impl FnOnce(&Link) -> Result<T>) -> Result<T> {
        self.demod_rmw8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT, IIC_REPEAT_BIT)?;
        let r = f(self);
        let closed = self.demod_rmw8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT, 0);
        let v = r?;
        closed?;
        Ok(v)
    }

    /// One tuner register, unverified: R16-R31 cannot be read back on
    /// this board (a 32-byte read returns R0-R15 twice, findings step 4a),
    /// so the caller verifies R5-R15 through [`Link::tuner_read`] where it
    /// matters. The write of R15 with CLK_OUT_ENB set is refused outright:
    /// on the bench board the demodulator is clocked from the tuner's
    /// clock output, and that write ended every USB request until a power
    /// cycle (findings step 4a). Like every tuner access, it needs the
    /// repeater open: call it inside [`Link::with_tuner`].
    pub fn tuner_write(&self, reg: u8, v: u8) -> Result<()> {
        if reg == R15 && v & CLK_OUT_ENB != 0 {
            return Err(device_error(
                "tuner register write",
                format!(
                    "refused R15 = 0x{v:02X}: CLK_OUT_ENB = 1 switches the tuner's clock output off, and the demodulator is clocked from it (findings step 4a)"
                ),
            ));
        }
        self.set(wvalue_tuner_write(reg), windex(BLOCK_TUNER, ACCESS_SET, 0), &[v], "tuner register write")
    }

    /// `len` bytes from register 0, each bit-reversed ([R820T] p. 17).
    /// The register file wraps after R15 (findings step 4a), so at most 16
    /// distinct registers come back.
    pub fn tuner_read(&self, len: u16) -> Result<Vec<u8>> {
        Ok(self
            .get(wvalue_tuner_read(), windex(BLOCK_TUNER, ACCESS_GET, 0), len, "tuner register read")?
            .into_iter()
            .map(tuner_read_byte)
            .collect())
    }

    /// R0-R4: the check byte (must be 0x96), R1 and R4 (unspecified), R2
    /// VCO_INDICATOR, R3 RF_INDICATOR ([R820T2] Table 1-2).
    pub fn tuner_status(&self) -> Result<[u8; 5]> {
        let v = self.tuner_read(5)?;
        if v[0] != TUNER_R0_CHECK {
            return Err(device_error(
                "tuner check byte",
                format!("R0 read 0x{:02X}, expected 0x{TUNER_R0_CHECK:02X} ([R820T] p. 17)", v[0]),
            ));
        }
        Ok([v[0], v[1], v[2], v[3], v[4]])
    }

    /// Write R5-R15 of an image and verify each by read-back; write
    /// R16-R31 unverified (the read wraps). The image is written in
    /// ascending register order, as every bench record's was.
    pub fn tuner_write_image(&self, img: &[u8; 32]) -> Result<()> {
        for reg in crate::image::IMAGE_FIRST_REG..32 {
            self.tuner_write(reg, img[reg as usize])?;
        }
        self.tuner_verify(img)
    }

    /// R5-R15 of the register file against an image.
    pub fn tuner_verify(&self, img: &[u8; 32]) -> Result<()> {
        let back = self.tuner_read(crate::image::READBACK_REGS as u16)?;
        if back[0] != TUNER_R0_CHECK {
            return Err(device_error("tuner check byte", format!("R0 read 0x{:02X}", back[0])));
        }
        let differ: Vec<String> = (crate::image::IMAGE_FIRST_REG..crate::image::READBACK_REGS)
            .filter(|&r| back[r as usize] != img[r as usize])
            .map(|r| format!("R{r}: wrote 0x{:02X}, read 0x{:02X}", img[r as usize], back[r as usize]))
            .collect();
        if !differ.is_empty() {
            return Err(device_error("tuner register read-back", differ.join("; ")));
        }
        Ok(())
    }
}
