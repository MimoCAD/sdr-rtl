//! The vendor-request layer over nusb: the Get and Set of [RTL2832U]
//! Tables 27-28 with the wValue forms the bench established
//! (`protocol`), and the register accessors built on them. Every write
//! that can be read back is read back here; the one register file that
//! cannot be (the tuner's R16-R31, findings step 4a) is marked where it
//! is written. Ported from the harness's request layer
//! (`examples/harness/usb.rs`), which every bench record was taken
//! through.
//!
//! Every request is async: awaited in a browser (WebUSB), completed in
//! place natively (nusb's native requests block), see `complete!`.
//!
//! The requests go through a [`Transport`]: nusb's device
//! ([`NusbTransport`]) on hardware, or a simulated dongle in tests
//! (`fake`), so every error path can be exercised without one.

use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use std::boxed::Box;
use std::fmt::Display;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient, TransferError};
use sdr_core::sync::Lock;
use sdr_core::time::{Instant, sleep};
use sdr_core::usb_stream::BoxedBulkIn;
use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::eeprom::{
    EEPROM_ADDRESS_SPACE, EEPROM_BLOCK, EEPROM_I2C_ADDRESS, EEPROM_REQUEST_MAX, EEPROM_WRITE_CHUNK,
    block0_addresses_eeprom, wvalue_eeprom,
};
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
/// The refused requests kept for [`Link::refusals`], the most recent.
pub const REFUSALS_KEPT: usize = 64;
/// A demodulator write is refused unless a control read was accepted
/// shortly before it (findings 21: on one Linux host, any block's read
/// 280 ms before let it through, 320 ms did not; USB and system writes
/// were never refused). A demodulator write with no read for this long
/// is preceded by `Link::wake_read`; one refused is retried after it.
pub const DEMOD_WRITE_READ_WITHIN: Duration = Duration::from_millis(100);

/// One request the chip refused (STALL) and that was tried again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub value: u16,
    pub index: u16,
    /// A Set (OUT); else a Get.
    pub set: bool,
    pub context: &'static str,
    /// When the refused attempt was sent, and how long it took to be
    /// refused.
    pub at: Instant,
    pub took: Duration,
}

/// A future a [`Transport`] hands back: `Send` natively, where the
/// receiver crosses threads.
#[cfg(not(target_arch = "wasm32"))]
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// A future a [`Transport`] hands back.
#[cfg(target_arch = "wasm32")]
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// `Send + Sync` natively; nothing in a browser.
#[cfg(not(target_arch = "wasm32"))]
pub trait TransportBounds: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> TransportBounds for T {}
/// `Send + Sync` natively; nothing in a browser.
#[cfg(target_arch = "wasm32")]
pub trait TransportBounds {}
#[cfg(target_arch = "wasm32")]
impl<T> TransportBounds for T {}

/// What the driver needs of the USB device: the vendor control requests
/// of [RTL2832U] Table 27 (bmRequestType 0xC0 / 0x40, bRequest 0) and the
/// bulk IN endpoint the samples arrive on.
pub trait Transport: TransportBounds {
    /// A vendor Get: `len` bytes for (wValue, wIndex).
    fn control_in(
        &self,
        value: u16,
        index: u16,
        len: u16,
    ) -> TransportFuture<'_, core::result::Result<Vec<u8>, TransferError>>;
    /// A vendor Set: `data` in the data stage.
    fn control_out<'a>(
        &'a self,
        value: u16,
        index: u16,
        data: &'a [u8],
    ) -> TransportFuture<'a, core::result::Result<(), TransferError>>;
    /// Claim the stream's interface (detaching a kernel driver bound to it
    /// where the OS has one) and open its bulk IN endpoint.
    fn open_bulk_in(&self) -> TransportFuture<'_, Result<BoxedBulkIn>>;
}

/// The [`Transport`] of a dongle opened through nusb. It holds the stream's
/// interface claimed for its whole life (review A3, A4): the claim is
/// exclusive, so a second process opening a dongle this one holds is
/// refused before it writes a register (it would otherwise stop this
/// one's stream with its bring-up), and on Linux the kernel's DVB driver
/// is detached once, at open, instead of rebinding — and reprogramming the
/// chip — whenever a stream ends.
pub struct NusbTransport {
    // Kept with the interface: the device the claim was made on.
    #[allow(dead_code)]
    dev: nusb::Device,
    iface: nusb::Interface,
}

impl NusbTransport {
    /// Claim the stream's interface (nusb's `detach_and_claim_interface`:
    /// on Linux a kernel driver bound to it is detached first). Claiming
    /// here rather than at the stream's start was A/B'd on the bench
    /// (2026-10-11, hwcheck back to back): the same tone level either way.
    pub async fn claim(dev: nusb::Device) -> Result<NusbTransport> {
        let iface = crate::complete!(dev.detach_and_claim_interface(crate::stream::BULK_INTERFACE)).map_err(|e| Error::Usb {
            backend: BACKEND,
            context: "claim interface",
            message: format!("{e} (another program — or another receiver in this one — may have the dongle open)"),
        })?;
        Ok(NusbTransport { dev, iface })
    }
}

impl Transport for NusbTransport {
    fn control_in(
        &self,
        value: u16,
        index: u16,
        len: u16,
    ) -> TransportFuture<'_, core::result::Result<Vec<u8>, TransferError>> {
        let req = ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: BREQUEST,
            value,
            index,
            length: len,
        };
        #[cfg(not(target_arch = "wasm32"))]
        return Box::pin(core::future::ready(nusb::MaybeFuture::wait(
            self.dev.control_in(req, CONTROL_TIMEOUT),
        )));
        #[cfg(target_arch = "wasm32")]
        return Box::pin(async move { self.dev.control_in(req, CONTROL_TIMEOUT).await });
    }

    fn control_out<'a>(
        &'a self,
        value: u16,
        index: u16,
        data: &'a [u8],
    ) -> TransportFuture<'a, core::result::Result<(), TransferError>> {
        let req = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: BREQUEST,
            value,
            index,
            data,
        };
        #[cfg(not(target_arch = "wasm32"))]
        return Box::pin(core::future::ready(nusb::MaybeFuture::wait(
            self.dev.control_out(req, CONTROL_TIMEOUT),
        )));
        #[cfg(target_arch = "wasm32")]
        return Box::pin(async move { self.dev.control_out(req, CONTROL_TIMEOUT).await });
    }

    /// The bulk endpoint on the interface claimed at open.
    fn open_bulk_in(&self) -> TransportFuture<'_, Result<BoxedBulkIn>> {
        let ep = self
            .iface
            .endpoint::<Bulk, In>(crate::stream::BULK_IN_ENDPOINT)
            .map(|ep| Box::new(ep) as BoxedBulkIn)
            .map_err(|e| Error::Usb {
                backend: BACKEND,
                context: "open bulk endpoint",
                message: e.to_string(),
            });
        Box::pin(core::future::ready(ep))
    }
}

/// An open dongle's control pipe. Clones share the device and the state
/// below: the receiver and its stream each hold one, possibly on two
/// threads (or two tasks in a browser).
#[derive(Clone)]
pub struct Link {
    transport: Arc<dyn Transport>,
    shared: Arc<Shared>,
}

/// What the clones of one link share.
struct Shared {
    /// Held across every read-modify-write, so two clones never
    /// interleave on one register (page 1 0x01 carries both the I2C
    /// repeater and the pause bit).
    rmw: Lock<()>,
    /// A stream exists (from `Rtl::into_stream` until the stream drops).
    stream_open: AtomicBool,
    /// Requests the chip refused (STALL) and that were tried again.
    stall_retries: AtomicU64,
    /// Which they were, the last [`REFUSALS_KEPT`].
    refusals: Mutex<Vec<Refusal>>,
    /// When a Get was last accepted ([`DEMOD_WRITE_READ_WITHIN`]).
    last_read: Mutex<Option<Instant>>,
    /// Reads sent only to let a demodulator write through.
    wake_reads: AtomicU64,
    /// The bench turned them off (`Link::set_wake_reads`), to measure
    /// the refusals they prevent.
    wake_off: AtomicBool,
}

fn usb_error(context: &'static str, e: impl Display) -> Error {
    Error::Usb {
        backend: BACKEND,
        context,
        message: e.to_string(),
    }
}

fn device_error(context: &'static str, message: String) -> Error {
    Error::Device {
        backend: BACKEND,
        context,
        message,
    }
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
    pub fn new(transport: Arc<dyn Transport>) -> Link {
        Link {
            transport,
            shared: Arc::new(Shared {
                rmw: Lock::new(()),
                stream_open: AtomicBool::new(false),
                stall_retries: AtomicU64::new(0),
                refusals: Mutex::new(Vec::new()),
                last_read: Mutex::new(None),
                wake_reads: AtomicU64::new(0),
                wake_off: AtomicBool::new(false),
            }),
        }
    }

    /// Bench only: send the wake reads (the default) or not.
    #[cfg(feature = "bench")]
    pub fn set_wake_reads(&self, on: bool) {
        self.shared.wake_off.store(!on, Ordering::Relaxed);
    }

    fn wake_reads_on(&self) -> bool {
        !self.shared.wake_off.load(Ordering::Relaxed)
    }

    fn mark_read(&self) {
        *self
            .shared
            .last_read
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    fn read_within(&self, d: Duration) -> bool {
        self.shared
            .last_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() < d)
    }

    /// One read of DEMOD_CTL, sent only so the demodulator write after it
    /// is not refused ([`DEMOD_WRITE_READ_WITHIN`]); counted
    /// ([`Link::wake_reads`]).
    async fn wake_read(&self) -> core::result::Result<(), TransferError> {
        self.shared.wake_reads.fetch_add(1, Ordering::Relaxed);
        self.transport
            .control_in(wvalue_sys(DEMOD_CTL), windex(BLOCK_SYS, ACCESS_GET, 0), 1)
            .await?;
        self.mark_read();
        Ok(())
    }

    /// Send the wake read now if the next demodulator write would need
    /// one, so a caller timing that write (a break's quiet) does not time
    /// the read with it.
    pub async fn ready_demod_write(&self) -> Result<()> {
        if self.wake_reads_on() && !self.read_within(DEMOD_WRITE_READ_WITHIN) {
            self.wake_read()
                .await
                .map_err(|e| usb_error("wake read", e))?;
        }
        Ok(())
    }

    /// Reads sent only to let a demodulator write through, since open.
    pub fn wake_reads(&self) -> u64 {
        self.shared.wake_reads.load(Ordering::Relaxed)
    }

    /// The stream's bulk IN endpoint ([`Transport::open_bulk_in`]).
    pub async fn open_bulk_in(&self) -> Result<BoxedBulkIn> {
        self.transport.open_bulk_in().await
    }

    /// Mark a stream as existing; false when one already does.
    pub fn claim_stream(&self) -> bool {
        self.shared
            .stream_open
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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
    /// to the EEPROM's slave is refused (`refuse_block0_eeprom`).
    pub async fn get(
        &self,
        value: u16,
        index: u16,
        len: u16,
        context: &'static str,
    ) -> Result<Vec<u8>> {
        refuse_block0_eeprom(value, index, context)?;
        let v = self
            .retry_stall(value, index, false, context, || {
                self.transport.control_in(value, index, len)
            })
            .await
            .map_err(|e| usb_error(context, e))?;
        self.mark_read();
        if v.len() != len as usize {
            return Err(usb_error(
                context,
                format!("short read: {} of {len} bytes", v.len()),
            ));
        }
        Ok(v)
    }

    /// A vendor Set: bmRequestType 0x40 (vendor, device, OUT), the bytes
    /// in the data stage (Table 27). A block 0 request to the EEPROM's
    /// slave is refused (`refuse_block0_eeprom`).
    /// A demodulator write with no read accepted for
    /// [`DEMOD_WRITE_READ_WITHIN`] is preceded by `Link::wake_read`.
    pub async fn set(
        &self,
        value: u16,
        index: u16,
        data: &[u8],
        context: &'static str,
    ) -> Result<()> {
        refuse_block0_eeprom(value, index, context)?;
        if block_of(index) == BLOCK_DEMOD {
            self.ready_demod_write().await?;
        }
        self.retry_stall(value, index, true, context, || {
            self.transport.control_out(value, index, data)
        })
        .await
        .map_err(|e| usb_error(context, e))
    }

    /// A request the chip refused with a STALL tried again, up to
    /// [`STALL_RETRIES`] times, [`STALL_RETRY_WAIT`] apart: now and then the
    /// chip refuses a request (findings 16.4-16.7). Every request here is
    /// safe to repeat: reads, writes of whole values, and read-modify-
    /// writes judged by their read-back. Each retry is counted
    /// ([`Link::stall_retries`]) and kept ([`Link::refusals`]).
    async fn retry_stall<'a, T>(
        &self,
        value: u16,
        index: u16,
        set: bool,
        context: &'static str,
        mut request: impl FnMut() -> TransportFuture<'a, core::result::Result<T, TransferError>>,
    ) -> core::result::Result<T, TransferError> {
        let mut tries = 0;
        loop {
            let at = Instant::now();
            match request().await {
                Err(TransferError::Stall) if tries < STALL_RETRIES => {
                    let took = at.elapsed();
                    tries += 1;
                    self.shared.stall_retries.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut kept = self
                            .shared
                            .refusals
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if kept.len() == REFUSALS_KEPT {
                            kept.remove(0);
                        }
                        kept.push(Refusal {
                            value,
                            index,
                            set,
                            context,
                            at,
                            took,
                        });
                    }
                    sleep(STALL_RETRY_WAIT).await;
                    // A refused demodulator write goes through after a
                    // read (findings 21); a failed read leaves the retry
                    // to be refused or not on its own.
                    if set && block_of(index) == BLOCK_DEMOD && self.wake_reads_on() {
                        let _ = self.wake_read().await;
                    }
                }
                r => return r,
            }
        }
    }

    /// Requests the chip refused and that were tried again, since open.
    pub fn stall_retries(&self) -> u64 {
        self.shared.stall_retries.load(Ordering::Relaxed)
    }

    /// Which requests those were, the last [`REFUSALS_KEPT`], oldest first.
    pub fn refusals(&self) -> Vec<Refusal> {
        self.shared
            .refusals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ---- System block: wValue = 0x3000 + offset (findings step 1A) ----

    pub async fn sys_read8(&self, off: u16) -> Result<u8> {
        Ok(self
            .get(
                wvalue_sys(off),
                windex(BLOCK_SYS, ACCESS_GET, 0),
                1,
                "system register read",
            )
            .await?[0])
    }

    async fn sys_write8(&self, off: u16, v: u8) -> Result<()> {
        self.set(
            wvalue_sys(off),
            windex(BLOCK_SYS, ACCESS_SET, 0),
            &[v],
            "system register write",
        )
        .await
    }

    /// Read-modify-write one system byte: clear `clear`, set `set`, then
    /// read back and compare the bits touched (DEMOD_CTL's bit 2 never
    /// reads back, findings step 3's block scan, so only the written bits
    /// are judged). Returns the byte read back.
    pub async fn sys_rmw8(&self, off: u16, clear: u8, set: u8) -> Result<u8> {
        let _held = self.shared.rmw.lock().await;
        let old = self.sys_read8(off).await?;
        let new = (old & !clear) | set;
        self.sys_write8(off, new).await?;
        let back = self.sys_read8(off).await?;
        let mask = clear | set;
        if back & mask != new & mask {
            return Err(device_error(
                "system register read-back",
                format!(
                    "0x{:04X}: wrote 0x{new:02X}, read 0x{back:02X} (mask 0x{mask:02X})",
                    wvalue_sys(off)
                ),
            ));
        }
        Ok(back)
    }

    /// Read-modify-write one system byte and read it back without judging
    /// it: for the registers whose read-back the bench has not shown
    /// (GPD and GPOE — step 1A read their power-on values, no step wrote
    /// them with a verified read-back). Returns the byte read back.
    pub async fn sys_rmw8_unjudged(&self, off: u16, clear: u8, set: u8) -> Result<u8> {
        let _held = self.shared.rmw.lock().await;
        let old = self.sys_read8(off).await?;
        self.sys_write8(off, (old & !clear) | set).await?;
        self.sys_read8(off).await
    }

    /// Write one whole system byte and read it back; every bit must
    /// agree except `not_read_back` (DEMOD_CTL's bit 2, findings step 3).
    pub async fn sys_write8_verified(&self, off: u16, v: u8, not_read_back: u8) -> Result<()> {
        self.sys_write8(off, v).await?;
        let back = self.sys_read8(off).await?;
        if back & !not_read_back != v & !not_read_back {
            return Err(device_error(
                "system register read-back",
                format!(
                    "0x{:04X}: wrote 0x{v:02X}, read 0x{back:02X}",
                    wvalue_sys(off)
                ),
            ));
        }
        Ok(())
    }

    // ---- USB block: wValue = 0x2000 + offset (findings step 1A) ----

    pub async fn usb_read8(&self, off: u16) -> Result<u8> {
        Ok(self
            .get(
                wvalue_usb(off),
                windex(BLOCK_USB, ACCESS_GET, 0),
                1,
                "USB register read",
            )
            .await?[0])
    }

    /// One USB-block byte written. Offset 0x1C is refused: it is the
    /// test-mode register whose write wedges the chip
    /// (derivation/HAZARD-usb-0x1C.md).
    pub async fn usb_write8(&self, off: u16, v: u8) -> Result<()> {
        if off == USB_HAZARD_OFF {
            return Err(device_error(
                "USB register write",
                format!("USB block 0x{off:03X} is the test-mode register; never written"),
            ));
        }
        self.set(
            wvalue_usb(off),
            windex(BLOCK_USB, ACCESS_SET, 0),
            &[v],
            "USB register write",
        )
        .await
    }

    /// `len` USB-block bytes from `off` (a 32-bit register reads
    /// little-endian, findings step 1A).
    pub async fn usb_read_n(&self, off: u16, len: u16) -> Result<Vec<u8>> {
        self.get(
            wvalue_usb(off),
            windex(BLOCK_USB, ACCESS_GET, 0),
            len,
            "USB register read",
        )
        .await
    }

    /// USB-block bytes written from `off`; refused if they would cover
    /// offset 0x1C (derivation/HAZARD-usb-0x1C.md).
    pub async fn usb_write_n(&self, off: u16, data: &[u8]) -> Result<()> {
        if (off..off + data.len() as u16).contains(&USB_HAZARD_OFF) {
            return Err(device_error(
                "USB register write",
                format!(
                    "0x{off:03X}+{}: covers the test-mode register 0x01C; never written",
                    data.len()
                ),
            ));
        }
        self.set(
            wvalue_usb(off),
            windex(BLOCK_USB, ACCESS_SET, 0),
            data,
            "USB register write",
        )
        .await
    }

    // ---- Demodulator: wValue = offset << 8 | 0x20 (findings step 1B/1C) ----

    /// `len` bytes from `off` on `page`; multi-byte reads auto-increment
    /// (findings step 1D).
    pub async fn demod_read(&self, page: u8, off: u8, len: u16) -> Result<Vec<u8>> {
        self.get(
            wvalue_demod(off),
            windex(BLOCK_DEMOD, ACCESS_GET, page as u16),
            len,
            "demodulator register read",
        )
        .await
    }

    pub async fn demod_read8(&self, page: u8, off: u8) -> Result<u8> {
        Ok(self.demod_read(page, off, 1).await?[0])
    }

    async fn demod_write8(&self, page: u8, off: u8, v: u8) -> Result<()> {
        self.demod_write(page, off, &[v]).await
    }

    /// Consecutive registers from `off` in one Set (wLength = the byte
    /// count), as the stream sequence writes page 1 0x16-0x17, 0x19-0x1A
    /// and the rsamp_ratio pairs (findings 9b; the harness's `demod_write`,
    /// `examples/harness/usb.rs`).
    pub async fn demod_write(&self, page: u8, off: u8, data: &[u8]) -> Result<()> {
        self.set(
            wvalue_demod(off),
            windex(BLOCK_DEMOD, ACCESS_SET, page as u16),
            data,
            "demodulator register write",
        )
        .await
    }

    /// [`Link::demod_write`], then the same registers read back in one
    /// auto-incrementing Get (findings step 1D) and compared whole: every
    /// write of the stream sequence read back as written on the bench
    /// (findings 10.1: "All 47 read-backs match").
    pub async fn demod_write_verified(&self, page: u8, off: u8, data: &[u8]) -> Result<()> {
        self.demod_write(page, off, data).await?;
        let back = self.demod_read(page, off, data.len() as u16).await?;
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
    pub async fn demod_rmw8(&self, page: u8, off: u8, mask: u8, value: u8) -> Result<u8> {
        let _held = self.shared.rmw.lock().await;
        let old = self.demod_read8(page, off).await?;
        let new = (old & !mask) | (value & mask);
        self.demod_write8(page, off, new).await?;
        let back = self.demod_read8(page, off).await?;
        if back & mask != new & mask {
            return Err(device_error(
                "demodulator register read-back",
                format!(
                    "page {page} 0x{off:02X}: wrote 0x{new:02X}, read 0x{back:02X} (mask 0x{mask:02X})"
                ),
            ));
        }
        Ok(back)
    }

    // ---- The EEPROM: block 3, wValue = offset << 8 | 0xA0 (findings 17) ----

    /// `len` EEPROM bytes from `offset`: one block 3 Get, at most 16 bytes
    /// (one I²C transaction, eeprom.md §1, §8), within the 256 offsets.
    pub async fn eeprom_read(&self, offset: u8, len: usize) -> Result<Vec<u8>> {
        if len == 0 || len > EEPROM_REQUEST_MAX || offset as usize + len > EEPROM_ADDRESS_SPACE {
            return Err(device_error(
                "EEPROM read",
                format!(
                    "0x{offset:02X}+{len}: 1-{EEPROM_REQUEST_MAX} bytes within the {EEPROM_ADDRESS_SPACE} offsets"
                ),
            ));
        }
        self.get(
            wvalue_eeprom(offset),
            windex(EEPROM_BLOCK, ACCESS_GET, 0),
            len as u16,
            "EEPROM read",
        )
        .await
    }

    /// EEPROM bytes written from `offset`: one block 3 Set, unverified
    /// here (`eeprom::write` reads the part back). At most
    /// `EEPROM_WRITE_CHUNK` (8) bytes and never across an 8-byte boundary,
    /// so the firmware makes it exactly one I²C write (eeprom.md §8: a
    /// second write in one request meets the part in its write cycle and
    /// STALLs) and it stays inside one write page (findings 17). The
    /// caller waits out the write cycle (`eeprom::EEPROM_WRITE_CYCLE`)
    /// before the next request.
    pub async fn eeprom_write(&self, offset: u8, data: &[u8]) -> Result<()> {
        let start = offset as usize;
        if data.is_empty()
            || start / EEPROM_WRITE_CHUNK != (start + data.len() - 1) / EEPROM_WRITE_CHUNK
        {
            return Err(device_error(
                "EEPROM write",
                format!(
                    "0x{offset:02X}+{}: 1-{EEPROM_WRITE_CHUNK} bytes inside one {EEPROM_WRITE_CHUNK}-byte-aligned chunk",
                    data.len()
                ),
            ));
        }
        self.set(
            wvalue_eeprom(offset),
            windex(EEPROM_BLOCK, ACCESS_SET, 0),
            data,
            "EEPROM write",
        )
        .await
    }

    // ---- Tuner: wValue (reg << 8) | 0x34 to write, 0x0034 to read (step 2) ----

    /// Run tuner accesses with the I2C repeater open (page 1 0x01 bit 3,
    /// Table 3; findings step 2: the tuner answers only with it set), and
    /// close it after, also when an access failed (the access's error is
    /// the one returned). Every tuner access of the driver goes through
    /// here, so between accesses page 1 0x01 rests at its power-on 0x10,
    /// the state the pause bit was measured from (findings 10.2c) and
    /// the vendor driver kept (findings 9a).
    pub async fn with_tuner<T>(&self, f: impl AsyncFnOnce(&Link) -> Result<T>) -> Result<T> {
        self.demod_rmw8(
            IIC_REPEAT_PAGE,
            IIC_REPEAT_OFF,
            IIC_REPEAT_BIT,
            IIC_REPEAT_BIT,
        )
        .await?;
        let r = f(self).await;
        let closed = self
            .demod_rmw8(IIC_REPEAT_PAGE, IIC_REPEAT_OFF, IIC_REPEAT_BIT, 0)
            .await;
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
    pub async fn tuner_write(&self, reg: u8, v: u8) -> Result<()> {
        if reg == R15 && v & CLK_OUT_ENB != 0 {
            return Err(device_error(
                "tuner register write",
                format!(
                    "refused R15 = 0x{v:02X}: CLK_OUT_ENB = 1 switches the tuner's clock output off, and the demodulator is clocked from it (findings step 4a)"
                ),
            ));
        }
        self.set(
            wvalue_tuner_write(reg),
            windex(BLOCK_TUNER, ACCESS_SET, 0),
            &[v],
            "tuner register write",
        )
        .await
    }

    /// `len` bytes from register 0, each bit-reversed ([R820T] p. 17).
    /// The register file wraps after R15 (findings step 4a), so at most 16
    /// distinct registers come back.
    pub async fn tuner_read(&self, len: u16) -> Result<Vec<u8>> {
        Ok(self
            .get(
                wvalue_tuner_read(),
                windex(BLOCK_TUNER, ACCESS_GET, 0),
                len,
                "tuner register read",
            )
            .await?
            .into_iter()
            .map(tuner_read_byte)
            .collect())
    }

    /// R0-R4: the check byte (must be 0x96), R1 and R4 (unspecified), R2
    /// VCO_INDICATOR, R3 RF_INDICATOR ([R820T2] Table 1-2).
    pub async fn tuner_status(&self) -> Result<[u8; 5]> {
        let v = self.tuner_read(5).await?;
        if v[0] != TUNER_R0_CHECK {
            return Err(device_error(
                "tuner check byte",
                format!(
                    "R0 read 0x{:02X}, expected 0x{TUNER_R0_CHECK:02X} ([R820T] p. 17)",
                    v[0]
                ),
            ));
        }
        Ok([v[0], v[1], v[2], v[3], v[4]])
    }

    /// Write R5-R15 of an image and verify each by read-back; write
    /// R16-R31 unverified (the read wraps). The image is written in
    /// ascending register order, as every bench record's was.
    pub async fn tuner_write_image(&self, img: &[u8; 32]) -> Result<()> {
        for reg in crate::image::IMAGE_FIRST_REG..32 {
            self.tuner_write(reg, img[reg as usize]).await?;
        }
        self.tuner_verify(img).await
    }

    /// R5-R15 of the register file against an image.
    pub async fn tuner_verify(&self, img: &[u8; 32]) -> Result<()> {
        let back = self.tuner_read(crate::image::READBACK_REGS as u16).await?;
        if back[0] != TUNER_R0_CHECK {
            return Err(device_error(
                "tuner check byte",
                format!("R0 read 0x{:02X}", back[0]),
            ));
        }
        let differ: Vec<String> = (crate::image::IMAGE_FIRST_REG..crate::image::READBACK_REGS)
            .filter(|&r| back[r as usize] != img[r as usize])
            .map(|r| {
                format!(
                    "R{r}: wrote 0x{:02X}, read 0x{:02X}",
                    img[r as usize], back[r as usize]
                )
            })
            .collect();
        if !differ.is_empty() {
            return Err(device_error("tuner register read-back", differ.join("; ")));
        }
        Ok(())
    }
}
