//! RTL bulk RX: a thin wrapper over the shared nusb streaming engine with
//! the RTL2832U's wire format (u8 offset-binary) and sizing defaults.
//! At 2.4 Msps a 16 KiB transfer is ~3.4 ms — fine enough resolution for
//! the gap-inference telemetry (see `usb_stream.rs`), with ~110 ms of
//! queue to ride out scheduler stalls.
//!
//! The stream owns its readiness ordering: `start()` resets the chip's
//! endpoint FIFO and primes the transfer queue back to back, so the
//! stream begins within microseconds of the FIFO starting fresh. (The
//! RTL2832U produces continuously and can't be gated like the HackRF's
//! transceiver mode — adjacency is the best readiness this hardware
//! allows, and its FIFO backpressure covers the sub-millisecond gap.)

use nusb::transfer::{Bulk, In};

use sdr_core::usb_stream::{BulkIqStream, WireSample};
use sdr_core::{Iq, IqStream, RecvInfo, Result, RxSource};

use crate::usb::Rtl2832;

pub(crate) const DEFAULT_TRANSFER_BYTES: usize = 16 * 1024;
pub(crate) const DEFAULT_QUEUE_DEPTH: usize = 32;

/// A running RTL RX stream.
pub struct RtlStream {
    inner: BulkIqStream,
    device: nusb::Device,
}

impl RtlStream {
    pub(crate) fn new(
        device: nusb::Device,
        endpoint: nusb::Endpoint<Bulk, In>,
        transfer_bytes: usize,
        queue_depth: usize,
        sample_rate: f64,
    ) -> RtlStream {
        RtlStream {
            inner: BulkIqStream::new(
                "rtl",
                endpoint,
                WireSample::OffsetU8,
                transfer_bytes,
                queue_depth,
                sample_rate,
            ),
            device,
        }
    }

    /// Reset the bulk endpoint FIFO (same register write as
    /// `RtlUsb::reset_buffer`, reachable from the stream side).
    fn reset_fifo(&self) -> Result<()> {
        self.device.reset_endpoint_fifo()
    }

    pub fn max_num_samps(&self) -> usize {
        self.inner.max_num_samps()
    }

    pub fn start(&mut self) -> Result<()> {
        // Fresh FIFO, then transfers waiting for it — back to back, no
        // app code in between.
        self.reset_fifo()?;
        self.inner.start()
    }

    pub fn stop(&mut self) -> Result<()> {
        self.inner.stop()
    }

    /// Blocking receive; timeout and inferred loss are non-fatal flags on
    /// the `RecvInfo` (`link` carries the USB timing detail).
    pub fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.inner.recv_into(buf, timeout_secs)
    }

    /// Start continuous streaming and move this stream onto a worker
    /// thread; see [`IqStream`].
    pub fn into_stream(self, depth: usize) -> Result<IqStream> {
        IqStream::spawn(self, depth)
    }
}

impl RxSource for RtlStream {
    fn max_num_samps(&self) -> usize {
        RtlStream::max_num_samps(self)
    }

    fn start(&mut self) -> Result<()> {
        RtlStream::start(self)
    }

    fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        RtlStream::recv_into(self, buf, timeout_secs)
    }

    fn setup_worker_thread(&self) {
        sdr_core::elevate_current_thread();
    }
}
