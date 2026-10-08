//! The vendor-request layer over nusb: the Get and Set of [RTL2832U]
//! Tables 27-28 with the wValue forms the bench established
//! (`protocol`), and the register accessors built on them. Every write
//! that can be read back is read back here; the one register file that
//! cannot be (the tuner's R16-R31, findings step 4a) is marked where it
//! is written. Ported from the harness's request layer
//! (`examples/harness/usb.rs`), which every bench record was taken
//! through.

use std::fmt::Display;
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};
use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::image::{CLK_OUT_ENB, R15};
use crate::protocol::*;

/// The control-transfer timeout the harness used for every bench
/// request (`examples/harness/usb.rs` CONTROL_TIMEOUT): a demodulator
/// access NAKs its data stage for 1-5 ms (findings step 3, the detector
/// on the wire), far inside it.
pub const CONTROL_TIMEOUT: Duration = Duration::from_millis(300);

/// An open dongle's control pipe.
pub struct Link {
    dev: nusb::Device,
}

fn usb_error(context: &'static str, e: impl Display) -> Error {
    Error::Usb { backend: BACKEND, context, message: e.to_string() }
}

fn device_error(context: &'static str, message: String) -> Error {
    Error::Device { backend: BACKEND, context, message }
}

impl Link {
    pub fn new(dev: nusb::Device) -> Link {
        Link { dev }
    }

    pub fn device(&self) -> &nusb::Device {
        &self.dev
    }

    /// A vendor Get: bmRequestType 0xC0 (vendor, device, IN), bRequest
    /// don't care, wLength = bytes to read (Table 27).
    pub fn get(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
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
            .wait()
            .map_err(|e| usb_error(context, e))?;
        if v.len() != len as usize {
            return Err(usb_error(context, format!("short read: {} of {len} bytes", v.len())));
        }
        Ok(v)
    }

    /// A vendor Set: bmRequestType 0x40 (vendor, device, OUT), the bytes
    /// in the data stage (Table 27).
    pub fn set(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
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
            .map_err(|e| usb_error(context, e))
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
        let old = self.sys_read8(off)?;
        self.sys_write8(off, (old & !clear) | set)?;
        self.sys_read8(off)
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
        self.set(wvalue_demod(off), windex(BLOCK_DEMOD, ACCESS_SET, page as u16), &[v], "demodulator register write")
    }

    /// Read-modify-write one demodulator byte: the bits of `mask` take
    /// `value`, the rest are kept; the read-back must agree on `mask`.
    /// Returns the byte read back.
    pub fn demod_rmw8(&self, page: u8, off: u8, mask: u8, value: u8) -> Result<u8> {
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

    // ---- Tuner: wValue (reg << 8) | 0x34 to write, 0x0034 to read (step 2) ----

    /// One tuner register, unverified: R16-R31 cannot be read back on
    /// this board (a 32-byte read returns R0-R15 twice, findings step 4a),
    /// so the caller verifies R5-R15 through [`Link::tuner_read`] where it
    /// matters. The write of R15 with CLK_OUT_ENB set is refused outright:
    /// on the bench board the demodulator is clocked from the tuner's
    /// clock output, and that write ended every USB request until a power
    /// cycle (findings step 4a).
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
