//! The receive stream: `RtlStream` wraps sdr-core's bulk-IQ engine over
//! the dongle's bulk IN endpoint (interface 0, endpoint 0x81, max packet
//! 512 at high speed — the descriptor the bench recorded, findings step
//! 1A) and implements `RxSource` for `IqStream`.
//!
//! It cannot stream yet. [RTL2832U] documents no raw-sample (SDR) output
//! mode, and the bench has not found one: the documented baseline (low-IF
//! input, ADCs on, FIFO reset, endpoint enabled) delivers 0 bytes; every
//! single bit and every byte value of demodulator pages 0-2, and the
//! first chunks of the system block, the USB block and pages 3-4, stream
//! nothing (findings step 3: 10,240 bit candidates, 196,608 byte
//! candidates, 1,200 block candidates; the only bytes ever seen are
//! one-shot bursts of 188-byte transport-stream blocks). The search is
//! ongoing; until a mode is found, `start` and the stream's construction
//! in the crate root refuse with [`raw_mode_not_derived`].

use sdr_core::usb_stream::BulkIqStream;
use sdr_core::{Error, Iq, RecvInfo, Result, RxSource, elevate_current_thread};

use crate::BACKEND;

/// The interface and endpoint of the bulk IN pipe (findings step 1A:
/// "interface 0 bulk IN 0x81, max packet 512; interface 1 has no
/// endpoints"; the power-on USB_EPA_CFG 0xD1 reads as enabled, bulk, IN,
/// endpoint 1, matching the descriptor).
pub const BULK_INTERFACE: u8 = 0;
pub const BULK_IN_ENDPOINT: u8 = 0x81;

/// The refusal every streaming entry point returns.
pub fn raw_mode_not_derived() -> Error {
    Error::Unsupported {
        backend: BACKEND,
        what: "the raw-sample output mode is not derived: [RTL2832U] documents no SDR mode, and the bench's \
               single-bit, single-byte and block scans of the register spaces (findings step 3) found no \
               register value that streams samples; the search is ongoing"
            .into(),
    }
}

/// A receive stream over the bulk IN endpoint. Construction needs an
/// endpoint the crate root will only hand over once the output mode is
/// derived; `start` refuses until then.
pub struct RtlStream {
    inner: BulkIqStream,
}

impl RtlStream {
    pub fn new(inner: BulkIqStream) -> RtlStream {
        RtlStream { inner }
    }
}

impl RxSource for RtlStream {
    fn max_num_samps(&self) -> usize {
        self.inner.max_num_samps()
    }

    fn start(&mut self) -> Result<()> {
        // The device has not been put into any mode that produces
        // samples; queueing transfers would only wait on a silent pipe.
        Err(raw_mode_not_derived())
    }

    fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.inner.recv_into(buf, timeout_secs)
    }

    fn setup_worker_thread(&self) {
        elevate_current_thread();
    }
}
