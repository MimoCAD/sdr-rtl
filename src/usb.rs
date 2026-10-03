//! nusb control-plane wrapper for the RTL2832U: vendor control transfers
//! into the register blocks, demod page/register access, and the I²C
//! repeater that fronts the tuner.
//!
//! Everything here blocks (nusb's `MaybeFuture::wait`) — configuration is
//! not on the hot path, and the crate's async surface stays confined to
//! `IqStream`. The register sequences are provided methods of
//! [`Rtl2832`] over any [`ControlPipe`], so the tests can pin the exact
//! control traffic they issue.

use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};

use sdr_core::{Error, Result};
use crate::regs::*;

const CTRL_TIMEOUT: Duration = Duration::from_millis(300);

fn usb_err(context: &'static str, e: impl std::fmt::Display) -> Error {
    Error::Usb {
        backend: "rtl",
        context,
        message: e.to_string(),
    }
}

/// The RTL2832U's vendor control transfers, in and out (request 0,
/// vendor type, device recipient).
pub(crate) trait ControlPipe {
    fn control_read(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>>;
    fn control_write(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()>;
}

impl ControlPipe for nusb::Device {
    fn control_read(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
        self.control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: 0,
                value,
                index,
                length: len,
            },
            CTRL_TIMEOUT,
        )
        .wait()
        .map_err(|e| usb_err(context, e))
    }

    fn control_write(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
        self.control_out(
            ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: 0,
                value,
                index,
                data,
            },
            CTRL_TIMEOUT,
        )
        .wait()
        .map_err(|e| usb_err(context, e))
    }
}

/// Open handle: the nusb device plus interface 0 (kept claimed for the
/// bulk endpoint's lifetime).
pub(crate) struct RtlUsb {
    pub(crate) device: nusb::Device,
    pub(crate) interface: nusb::Interface,
}

impl ControlPipe for RtlUsb {
    fn control_read(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
        self.device.control_read(value, index, len, context)
    }

    fn control_write(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
        self.device.control_write(value, index, data, context)
    }
}

impl RtlUsb {
    pub fn new(device: nusb::Device) -> Result<RtlUsb> {
        // On Linux the DVB-T kernel driver may own the interface; detach
        // it (the nusb call is a no-op where that concept doesn't exist).
        #[cfg(target_os = "linux")]
        let interface = device
            .detach_and_claim_interface(0)
            .wait()
            .map_err(|e| usb_err("claim_interface", e))?;
        #[cfg(not(target_os = "linux"))]
        let interface = device
            .claim_interface(0)
            .wait()
            .map_err(|e| usb_err("claim_interface", e))?;
        Ok(RtlUsb { device, interface })
    }

    /// USB strings (manufacturer, product) — how the RTL-SDR Blog V4
    /// identifies itself, since it shares the generic VID/PID.
    pub fn usb_strings(&self) -> (Option<String>, Option<String>) {
        let desc = self.device.device_descriptor();
        let lang = self
            .device
            .get_string_descriptor_supported_languages(CTRL_TIMEOUT)
            .wait()
            .ok()
            .and_then(|mut l| l.next());
        let Some(lang) = lang else {
            return (None, None);
        };
        let fetch = |idx: Option<std::num::NonZero<u8>>| {
            idx.and_then(|i| {
                self.device
                    .get_string_descriptor(i, lang, CTRL_TIMEOUT)
                    .wait()
                    .ok()
            })
        };
        (
            fetch(desc.manufacturer_string_index()),
            fetch(desc.product_string_index()),
        )
    }
}

/// The RTL2832U's registers, reached through its vendor control
/// transfers: every register sequence this driver issues.
pub(crate) trait Rtl2832: ControlPipe {
    /// Read a 1- or 2-byte register from a block (little-endian on read).
    fn read_reg(&self, block: u16, addr: u16, len: usize) -> Result<u16> {
        debug_assert!(len == 1 || len == 2);
        let data = self.control_read(addr, block << WINDEX_BLOCK_SHIFT, len as u16, "read_reg")?;
        let lo = data.first().copied().unwrap_or(0) as u16;
        let hi = data.get(1).copied().unwrap_or(0) as u16;
        Ok(lo | (hi << 8))
    }

    /// Write a 1- or 2-byte register into a block (big-endian on write —
    /// the chip's convention, opposite to reads).
    fn write_reg(&self, block: u16, addr: u16, val: u16, len: usize) -> Result<()> {
        debug_assert!(len == 1 || len == 2);
        let be = val.to_be_bytes();
        let data = if len == 1 { &be[1..2] } else { &be[..] };
        self.control_write(addr, (block << WINDEX_BLOCK_SHIFT) | WINDEX_SET, data, "write_reg")
    }

    /// Write a 16-bit register in its own (little-endian) byte order.
    fn write_reg16(&self, block: u16, addr: u16, value: u16) -> Result<()> {
        self.control_write(addr, (block << WINDEX_BLOCK_SHIFT) | WINDEX_SET, &value.to_le_bytes(), "write_reg")
    }

    /// Read one byte of a demodulator register.
    fn demod_read(&self, reg: DemodReg) -> Result<u8> {
        let value = (reg.addr << DEMOD_OFFSET_SHIFT) | DEMOD_VALUE_TAG;
        let data = self.control_read(value, reg.page, 1, "demod_read_reg")?;
        Ok(data.first().copied().unwrap_or(0))
    }

    /// Write demodulator register bytes from `reg` on, then perform the
    /// read that completes the transaction (the chip latches demod
    /// writes on the next access).
    fn demod_write(&self, reg: DemodReg, data: &[u8]) -> Result<()> {
        debug_assert!(data.len() == 1 || data.len() == 2);
        let value = (reg.addr << DEMOD_OFFSET_SHIFT) | DEMOD_VALUE_TAG;
        self.control_write(value, WINDEX_SET | reg.page, data, "demod_write_reg")?;
        self.demod_read(DEMOD_WRITE_LATCH)?;
        Ok(())
    }

    /// Write a demodulator register's fields (all eight bits bound).
    fn demod_write_bits(&self, bits: DemodBits) -> Result<()> {
        self.demod_write(bits.reg, &[bits.byte()])
    }

    /// I²C write through the repeater: `buf[0]` is the target register.
    fn i2c_write(&self, i2c_addr: u16, buf: &[u8]) -> Result<()> {
        self.control_write(i2c_addr, (BLOCK_IIC << WINDEX_BLOCK_SHIFT) | WINDEX_SET, buf, "i2c_write")
    }

    fn i2c_read(&self, i2c_addr: u16, len: u16) -> Result<Vec<u8>> {
        self.control_read(i2c_addr, BLOCK_IIC << WINDEX_BLOCK_SHIFT, len, "i2c_read")
    }

    /// Read one tuner register by write-address-then-read.
    fn i2c_read_reg(&self, i2c_addr: u16, reg: u8) -> Result<u8> {
        self.i2c_write(i2c_addr, &[reg])?;
        let data = self.i2c_read(i2c_addr, 1)?;
        data.first().copied().ok_or_else(|| Error::Device {
            backend: "rtl",
            context: "i2c_read_reg",
            message: "empty I2C read".into(),
        })
    }

    /// Page 1, 0x01 with the repeater and the soft reset as given.
    fn write_p1_01(&self, repeater: bool, soft_reset: bool) -> Result<()> {
        self.demod_write_bits(
            P1_01_HIGH_BITS
                .set(0)
                .with(P1_01_BIT4.set(1))
                .with(IIC_REPEAT.set(u8::from(repeater)))
                .with(SOFT_RESET.set(u8::from(soft_reset)))
                .with(P1_01_LOW_BITS.set(0)),
        )
    }

    /// The demod's I²C repeater gates all tuner access; every tuner
    /// transaction must be bracketed by on/off (Table 3's `IIC_repeat`).
    fn set_i2c_repeater(&self, enable: bool) -> Result<()> {
        self.write_p1_01(enable, false)
    }

    /// Power up the USB/system side and the demod, reset it, and leave the
    /// chip ready for SDR-mode configuration. Mirrors librtlsdr's
    /// baseband init order exactly.
    fn init_baseband(&self, fir: &[i32; 16]) -> Result<()> {
        // A dummy write probes whether the device is in a sane state; if
        // it goes nowhere, reset once and continue.
        self.write_reg(BLOCK_USB, USB_SYSCTL, u16::from(USB_SYSCTL_FULL_PACKET_MODE | USB_SYSCTL_DMA_ENABLE), 1)?;

        self.write_reg16(BLOCK_USB, USB_EPA_MAXPKT, USB_EPA_MAX_PACKET_BYTES)?;
        self.write_reg16(BLOCK_USB, USB_EPA_CTL, USB_EPA_CTL_FIFO_RESET | USB_EPA_CTL_STALL)?;

        // Power on the demod: its PLL and both ADCs, out of reset.
        self.write_reg(BLOCK_SYS, DEMOD_CTL_1, u16::from(DEMOD_CTL_1_POWER_ON), 1)?;
        let demod_on = DEMOD_CTL_PLL_ENABLE | DEMOD_CTL_ADC_I_ENABLE | DEMOD_CTL_RESET_RELEASED | DEMOD_CTL_ADC_Q_ENABLE;
        self.write_reg(BLOCK_SYS, DEMOD_CTL, u16::from(demod_on), 1)?;

        self.reset_demod()?;

        // No spectrum inversion, no adjacent-channel rejection.
        self.demod_write_bits(P1_15_HIGH_BITS.set(0).with(EN_ACI.set(0)).with(SPEC_INV.set(0)))?;
        self.demod_write(DDC_SHIFT, &[0, 0])?;

        // Clear the DDC shift and the IF's top two octets (librtlsdr's
        // 0x16-0x1a; set_if_freq writes all three later).
        for addr in DDC_SHIFT.addr..DDC_SHIFT.addr + DDC_SHIFT_LEN + 2 {
            self.demod_write(DemodReg { page: DDC_SHIFT.page, addr }, &[0])?;
        }

        self.set_fir(fir)?;

        // SDR mode on, DAGC off.
        self.demod_write_bits(operating_mode(false, false))?;

        self.demod_write(FSM_STATE_HOLD_HIGH, &FSM_STATE_HOLD[..1])?;
        self.demod_write(FSM_STATE_HOLD_LOW, &FSM_STATE_HOLD[1..])?;

        // Digital AGC off; the IF and RF analog AGC loops off.
        self.demod_write_bits(P1_11_HIGH_BITS.set(0).with(EN_DAGC.set(0)))?;
        self.demod_write_bits(
            EN_IF_AGC.set(0).with(EN_RF_AGC.set(0)).with(AAGC_HOLD.set(0)).with(LOOP_GAIN2_3_0.set(0)).with(P1_04_BIT0.set(0)),
        )?;

        // The PID filter off (DVB leftovers): output disabled, matched
        // PIDs rejected, errors passed.
        self.demod_write_bits(ENABLE_PID.set(0).with(MODE_PID.set(1)).with(ERR_PASS.set(1)).with(P0_61_LOW_BITS.set(0)))?;

        // Default ADC_I/ADC_Q datapath.
        self.demod_write_bits(
            MPEG_IO_OPT_2_2.set(1).with(P0_06_BIT6.set(0)).with(OPT_ADC_IQ.set(0)).with(P0_06_LOW_BITS.set(0)),
        )?;

        // Zero-IF input, DC cancellation, IQ estimation/compensation.
        self.demod_write_bits(zero_if(true))?;

        // Stop the 4.096 MHz clock output on TP_CK0.
        self.demod_write_bits(REG_GPE.set(1).with(P0_0D_MIDDLE_BITS.set(0)).with(REG_MONSEL.set(0)).with(REG_MON.set(3)))?;

        Ok(())
    }

    /// Soft-reset the demod (the kernel's `SOFT_RST`, page 1 0x01 bit 2).
    fn reset_demod(&self) -> Result<()> {
        self.write_p1_01(false, true)?;
        self.write_p1_01(false, false)?;
        Ok(())
    }

    /// Load the 16-tap baseband FIR: 8 × i8, then 8 × i12 packed into 12
    /// bytes, into [`FIR_COEFFICIENTS`].
    fn set_fir(&self, fir: &[i32; 16]) -> Result<()> {
        let mut packed = [0u8; 20];
        for i in 0..8 {
            let v = fir[i];
            debug_assert!((-128..=127).contains(&v));
            packed[i] = v as u8;
        }
        for i in (0..8).step_by(2) {
            let v0 = fir[8 + i];
            let v1 = fir[8 + i + 1];
            debug_assert!((-2048..=2047).contains(&v0) && (-2048..=2047).contains(&v1));
            packed[8 + i * 3 / 2] = (v0 >> 4) as u8;
            packed[8 + i * 3 / 2 + 1] = ((v0 << 4) | ((v1 >> 8) & 0x0f)) as u8;
            packed[8 + i * 3 / 2 + 2] = v1 as u8;
        }
        for (i, byte) in packed.iter().enumerate() {
            let reg = DemodReg { page: FIR_COEFFICIENTS.page, addr: FIR_COEFFICIENTS.addr + i as u16 };
            self.demod_write(reg, &[*byte])?;
        }
        Ok(())
    }

    /// Drive a SYS-block GPIO pin (the bias tee hangs off GPIO0).
    fn set_gpio(&self, pin: u8, on: bool) -> Result<()> {
        let bit = 1u16 << pin;
        // Direction: output.
        let gpd = self.read_reg(BLOCK_SYS, GPD, 1)?;
        self.write_reg(BLOCK_SYS, GPD, gpd & !bit, 1)?;
        let gpoe = self.read_reg(BLOCK_SYS, GPOE, 1)?;
        self.write_reg(BLOCK_SYS, GPOE, gpoe | bit, 1)?;
        // Level.
        let gpo = self.read_reg(BLOCK_SYS, GPO, 1)?;
        self.write_reg(BLOCK_SYS, GPO, if on { gpo | bit } else { gpo & !bit }, 1)?;
        Ok(())
    }

    /// The R82xx input plan: low-IF with spectrum inversion through one
    /// ADC — §9.1's IF signal mode (`AD_EN_reg1` 1, `AD_EN_reg` 0,
    /// `en_bbin` 0), which librtlsdr's comment calls "In-phase ADC input
    /// only".
    fn configure_low_if_input(&self, if_freq_hz: u32) -> Result<()> {
        /// librtlsdr's value for page 0 0x08's unnamed low bits.
        const P0_08_LOW_BITS_IF_MODE: u8 = 0b00_1101;
        self.demod_write_bits(zero_if(false))?;
        self.demod_write_bits(AD_EN_REG.set(0).with(AD_EN_REG1.set(1)).with(P0_08_LOW_BITS.set(P0_08_LOW_BITS_IF_MODE)))?;
        self.set_if_freq(if_freq_hz)?;
        self.demod_write_bits(P1_15_HIGH_BITS.set(0).with(EN_ACI.set(0)).with(SPEC_INV.set(1)))?;
        Ok(())
    }

    /// Program the demod's IF downconversion frequency, `pset_iffreq`.
    fn set_if_freq(&self, freq_hz: u32) -> Result<()> {
        let fraction = (1u32 << PSET_IFFREQ_FRACTION_BITS) as f64;
        let if_val = -((freq_hz as f64 * fraction / RTL_XTAL_HZ as f64) as i32);
        let [_, top, middle, low] = (if_val as u32).to_be_bytes();
        let octets = [u16::from(top) & PSET_IFFREQ_TOP_BITS, u16::from(middle), u16::from(low)];
        for (i, octet) in octets.iter().enumerate() {
            let reg = DemodReg { page: PSET_IFFREQ.page, addr: PSET_IFFREQ.addr + i as u16 };
            self.demod_write(reg, &[*octet as u8])?;
        }
        Ok(())
    }

    /// Program `rsamp_ratio`, clear the sample frequency correction, and
    /// reset the demod so it takes effect.
    fn set_resampler(&self, rsamp_ratio: u32) -> Result<()> {
        let bytes = rsamp_ratio.to_be_bytes();
        self.demod_write(RSAMP_RATIO, &bytes[..2])?;
        self.demod_write(DemodReg { page: RSAMP_RATIO.page, addr: RSAMP_RATIO.addr + 2 }, &bytes[2..])?;
        // No PPM correction (the survey tools measure offsets instead).
        self.demod_write(SAMPLE_FREQ_CORRECTION_LOW, &[0])?;
        self.demod_write(SAMPLE_FREQ_CORRECTION_HIGH, &[0])?;
        self.reset_demod()
    }

    /// `rsamp_ratio`'s four octets as the chip holds them.
    fn resampler_ratio(&self) -> Result<u32> {
        let mut bytes = [0u8; RSAMP_RATIO_LEN as usize];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = self.demod_read(DemodReg { page: RSAMP_RATIO.page, addr: RSAMP_RATIO.addr + i as u16 })?;
        }
        Ok(u32::from_be_bytes(bytes))
    }

    /// SDR mode with the demod's digital AGC on or off.
    fn set_demod_agc(&self, on: bool) -> Result<()> {
        self.demod_write_bits(operating_mode(on, false))
    }

    /// The ADC test counter in place of samples, or SDR mode.
    fn set_test_mode(&self, on: bool) -> Result<()> {
        self.demod_write_bits(operating_mode(false, on))
    }

    /// Power the demod down (best-effort, on close): PLL and ADCs off,
    /// out of reset.
    fn power_down(&self) -> Result<()> {
        self.write_reg(BLOCK_SYS, DEMOD_CTL, u16::from(DEMOD_CTL_RESET_RELEASED), 1)
    }

    /// Reset the bulk endpoint's FIFO: stall it with the FIFO in reset,
    /// then release both.
    fn reset_endpoint_fifo(&self) -> Result<()> {
        for value in [USB_EPA_CTL_FIFO_RESET | USB_EPA_CTL_STALL, 0] {
            self.control_write(
                USB_EPA_CTL,
                (BLOCK_USB << WINDEX_BLOCK_SHIFT) | WINDEX_SET,
                &value.to_le_bytes(),
                "EPA FIFO reset",
            )?;
        }
        Ok(())
    }
}

impl<T: ControlPipe + ?Sized> Rtl2832 for T {}

/// Page 0 0x19: SDR mode (or the test counter) with DAGC as given.
fn operating_mode(dagc: bool, test_counter: bool) -> DemodBits {
    P0_19_HIGH_BITS
        .set(0)
        .with(DAGC_MODE.set(u8::from(dagc)))
        .with(P0_19_MIDDLE_BITS.set(0))
        .with(SDR_MODE.set(u8::from(!test_counter)))
        .with(TEST_COUNTER.set(u8::from(test_counter)))
        .with(P0_19_BIT0.set(1))
}

/// Page 1 0xb1: DC and IQ correction on, the Zero-IF input as given.
fn zero_if(on: bool) -> DemodBits {
    P1_B1_HIGH_BITS
        .set(0)
        .with(EN_IQ_EST.set(1))
        .with(EN_IQ_COMP.set(1))
        .with(P1_B1_BIT2.set(0))
        .with(EN_DC_EST.set(1))
        .with(EN_BBIN.set(u8::from(on)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::DEFAULT_FIR;
    use std::cell::RefCell;
    use std::fmt::Write as _;

    /// A control pipe that records every transfer and answers each read
    /// with `fill` (so read-modify-writes have something to modify).
    struct Recorder {
        fill: u8,
        log: RefCell<String>,
    }

    impl ControlPipe for Recorder {
        fn control_read(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
            let _ = writeln!(self.log.borrow_mut(), "IN {value:04x} {index:04x} {len} {context}");
            Ok(vec![self.fill; len as usize])
        }
        fn control_write(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
            let _ = writeln!(self.log.borrow_mut(), "OUT {value:04x} {index:04x} {data:02x?} {context}");
            Ok(())
        }
    }

    /// Every register sequence, under two read answers.
    fn transcript() -> String {
        let mut out = String::new();
        for fill in [0x00, 0xa5] {
            let p = Recorder { fill, log: RefCell::new(String::new()) };
            let mut r = String::new();
            let _ = writeln!(r, "init {:?}", p.init_baseband(&DEFAULT_FIR).is_ok());
            for on in [true, false] {
                let _ = writeln!(r, "repeater {on} {:?}", p.set_i2c_repeater(on).is_ok());
                let _ = writeln!(r, "agc {on} {:?}", p.set_demod_agc(on).is_ok());
                let _ = writeln!(r, "test {on} {:?}", p.set_test_mode(on).is_ok());
                let _ = writeln!(r, "gpio0 {on} {:?}", p.set_gpio(0, on).is_ok());
            }
            let _ = writeln!(r, "gpio4 {:?}", p.set_gpio(4, true).is_ok());
            let _ = writeln!(r, "lowif {:?}", p.configure_low_if_input(3_570_000).is_ok());
            for f in [0u32, 2_300_000, 3_570_000, 4_570_000, 7_367_000] {
                let _ = writeln!(r, "if {f} {:?}", p.set_if_freq(f).is_ok());
            }
            for ratio in [0x0c99_9998u32, 0x0e66_6664, 0x0bb8_0000, 0x0fff_fffc] {
                let _ = writeln!(r, "rsamp {ratio:08x} {:?}", p.set_resampler(ratio).is_ok());
            }
            let _ = writeln!(r, "ratio {:?}", p.resampler_ratio().ok());
            let _ = writeln!(r, "reg {:?}", p.read_reg(BLOCK_SYS, GPD, 1).ok());
            let _ = writeln!(r, "i2c {:?}", p.i2c_read_reg(0x34, 0x00).ok());
            let _ = writeln!(r, "i2cw {:?}", p.i2c_write(0x34, &[0x05, 0x83, 0x32]).is_ok());
            let _ = writeln!(r, "fifo {:?}", p.reset_endpoint_fifo().is_ok());
            let _ = writeln!(r, "down {:?}", p.power_down().is_ok());
            let _ = writeln!(out, "== fill {fill:02x}\n{}{r}", p.log.borrow());
        }
        out
    }

    fn fnv1a(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
    }

    /// The RTL2832U control traffic is pinned: naming its registers from
    /// the Realtek datasheet must reproduce it byte for byte (pinned
    /// before that change).
    #[test]
    fn control_traffic_is_pinned() {
        let t = transcript();
        assert_eq!((t.lines().count(), fnv1a(&t)), (480, 10303234540097062584), "{t}");
    }
}
