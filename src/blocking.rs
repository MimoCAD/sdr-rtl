//! The same receiver, blocking: each method runs its async counterpart to
//! completion on the calling thread (`sdr_core::block_on`). Native only —
//! a browser cannot block. The streams here are [`std::io::Read`] and
//! `sdr_core::RxSource`.
//!
//! ```no_run
//! use std::io::Read;
//! use sdr_core::TuneRequest;
//! use sdr_rtl::blocking::Rtl;
//!
//! let mut rtl = Rtl::open("")?;
//! rtl.set_rate(2.4e6)?;
//! rtl.set_freq(&TuneRequest::auto(100.1e6))?;
//! let (mut reader, control) = rtl.into_stream(0, 0).map_err(|e| e.error)?.split();
//! let mut buf = [0u8; 16384];
//! let n = reader.read(&mut buf)?; // I, Q, I, Q, … unsigned bytes
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::boxed::Box;
use std::io;
use std::string::ToString;
use std::vec::Vec;

use sdr_core::gain::{Stage, StageReading};
use sdr_core::{DeviceDesc, Error, Iq, RecvInfo, Result, RxSource, TuneRequest, TuneResult, block_on, elevate_current_thread, open_with_deadline};

use crate::session::{self, Break, StreamReport};
use crate::{BACKEND, Eeprom, EepromWriteReport, RtlTuner, RtlTunerStatus};

/// Every attached dongle ([`crate::list`]).
pub fn list() -> Vec<DeviceDesc> {
    block_on(crate::list())
}

/// An open dongle ([`crate::Rtl`]), blocking.
pub struct Rtl(crate::Rtl);

impl From<crate::Rtl> for Rtl {
    fn from(r: crate::Rtl) -> Rtl {
        Rtl(r)
    }
}

impl Rtl {
    /// [`crate::Rtl::open`], bounded by `sdr_core::open_with_deadline`: a
    /// dongle that enumerates but never answers fails within seconds
    /// instead of hanging the caller.
    pub fn open(spec: &str) -> Result<Rtl> {
        let spec = spec.to_string();
        open_with_deadline(BACKEND, move || block_on(crate::Rtl::open(&spec))).map(Rtl)
    }

    /// The async receiver.
    pub fn into_async(self) -> crate::Rtl {
        self.0
    }

    pub fn label(&self) -> &str {
        self.0.label()
    }

    pub fn tuner_kind(&self) -> RtlTuner {
        self.0.tuner_kind()
    }

    /// [`crate::Rtl::set_rate`].
    pub fn set_rate(&mut self, sps: f64) -> Result<()> {
        block_on(self.0.set_rate(sps))
    }

    /// [`crate::Rtl::rate`].
    pub fn rate(&self) -> Result<f64> {
        block_on(self.0.rate())
    }

    /// [`crate::Rtl::rate_set`].
    pub fn rate_set(&self) -> f64 {
        self.0.rate_set()
    }

    /// [`crate::Rtl::set_freq`].
    pub fn set_freq(&mut self, req: &TuneRequest) -> Result<TuneResult> {
        block_on(self.0.set_freq(req))
    }

    pub fn freq(&self) -> Result<f64> {
        self.0.freq()
    }

    /// [`crate::Rtl::set_gain`].
    pub fn set_gain(&mut self, db: f64) -> Result<()> {
        block_on(self.0.set_gain(db))
    }

    pub fn gain(&self) -> f64 {
        self.0.gain()
    }

    pub fn gain_stages(&self) -> (f64, f64, f64) {
        self.0.gain_stages()
    }

    /// [`crate::Rtl::set_gain_stages`].
    pub fn set_gain_stages(&mut self, lna_db: f64, mixer_db: f64, vga_db: f64) -> Result<()> {
        block_on(self.0.set_gain_stages(lna_db, mixer_db, vga_db))
    }

    pub fn stage_levels_db(&self, stage: Stage) -> Option<Vec<f64>> {
        self.0.stage_levels_db(stage)
    }

    pub fn stage_readings(&self) -> Vec<StageReading> {
        self.0.stage_readings()
    }

    /// [`crate::Rtl::set_stage_level`].
    pub fn set_stage_level(&mut self, stage: Stage, level: usize) -> Result<()> {
        block_on(self.0.set_stage_level(stage, level))
    }

    /// [`crate::Rtl::set_agc`].
    pub fn set_agc(&mut self, tuner_agc: bool, demod_agc: bool) -> Result<()> {
        block_on(self.0.set_agc(tuner_agc, demod_agc))
    }

    pub fn tuner_agc(&self) -> bool {
        self.0.tuner_agc()
    }

    pub fn demod_agc(&self) -> bool {
        self.0.demod_agc()
    }

    /// [`crate::Rtl::set_bandwidth`].
    pub fn set_bandwidth(&mut self, hz: f64) -> Result<()> {
        block_on(self.0.set_bandwidth(hz))
    }

    pub fn bandwidth(&self) -> Option<f64> {
        self.0.bandwidth()
    }

    /// [`crate::Rtl::set_bias_tee`].
    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        self.0.set_bias_tee(on)
    }

    /// [`crate::Rtl::tuner_status`].
    pub fn tuner_status(&mut self) -> Result<RtlTunerStatus> {
        block_on(self.0.tuner_status())
    }

    /// [`crate::Rtl::taps`].
    pub fn taps(&self) -> Result<[u8; crate::taps::TAP_REGS]> {
        block_on(self.0.taps())
    }

    /// [`crate::Rtl::read_eeprom`].
    pub fn read_eeprom(&self) -> Result<Eeprom> {
        block_on(self.0.read_eeprom())
    }

    /// [`crate::Rtl::write_eeprom`].
    pub fn write_eeprom(&mut self, new: &Eeprom) -> Result<EepromWriteReport> {
        block_on(self.0.write_eeprom(new))
    }

    /// [`crate::Rtl::into_stream`].
    pub fn into_stream(self, transfer_bytes: usize, queue_depth: usize) -> core::result::Result<Stream, StreamError> {
        block_on(self.0.into_stream(transfer_bytes, queue_depth)).map(Stream).map_err(|e| StreamError { rtl: Box::new(Rtl(*e.rtl)), error: e.error })
    }

    /// The request layer, for bench programs (`--features bench`).
    #[cfg(feature = "bench")]
    pub fn link(&self) -> Link<'_> {
        Link(self.0.link())
    }

    /// [`crate::Rtl::rx_stream`], for bench programs.
    #[cfg(feature = "bench")]
    pub fn rx_stream(&mut self, transfer_bytes: usize, queue_depth: usize) -> Result<RtlStream> {
        block_on(self.0.rx_stream(transfer_bytes, queue_depth)).map(RtlStream)
    }

    /// [`crate::Rtl::rx_stream_with_taps`], for bench programs.
    #[cfg(feature = "bench")]
    pub fn rx_stream_with_taps(&mut self, transfer_bytes: usize, queue_depth: usize, tap_regs: [u8; crate::taps::TAP_REGS]) -> Result<RtlStream> {
        block_on(self.0.rx_stream_with_taps(transfer_bytes, queue_depth, tap_regs)).map(RtlStream)
    }
}

/// A stream could not be made; the receiver is handed back.
#[derive(Debug)]
pub struct StreamError {
    pub rtl: Box<Rtl>,
    pub error: Error,
}

impl core::fmt::Debug for Rtl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rtl").field("label", &self.0.label()).finish_non_exhaustive()
    }
}

impl core::fmt::Display for StreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.error.fmt(f)
    }
}

impl core::error::Error for StreamError {}

/// A finished stream ([`crate::Finished`]): the receiver back, blocking.
pub struct Finished {
    pub rtl: Rtl,
    pub report: StreamReport,
    /// The last wire bytes (unsigned 8-bit I then Q).
    pub tail: Vec<u8>,
}

impl Finished {
    /// The tail widened to [`Iq`].
    pub fn tail_samples(&self) -> Vec<Iq> {
        session::widen(&self.tail)
    }
}

impl From<crate::Finished> for Finished {
    fn from(f: crate::Finished) -> Finished {
        Finished { rtl: Rtl(f.rtl), report: f.report, tail: f.tail }
    }
}

/// [`crate::Stream`], blocking.
pub struct Stream(crate::Stream);

/// [`crate::ReadHalf`], blocking.
pub struct ReadHalf(crate::ReadHalf);

/// [`crate::Control`], blocking. Cheap to clone.
#[derive(Clone)]
pub struct Control(crate::Control);

/// What every handle reports (no request, no wait).
macro_rules! reports {
    ($t:ident) => {
        impl $t {
            pub fn rate(&self) -> f64 {
                self.0.rate()
            }

            pub fn freq(&self) -> Result<f64> {
                self.0.freq()
            }

            pub fn gain(&self) -> f64 {
                self.0.gain()
            }

            pub fn stage_readings(&self) -> Vec<StageReading> {
                self.0.stage_readings()
            }

            pub fn label(&self) -> std::string::String {
                self.0.label()
            }

            pub fn delivered(&self) -> u64 {
                self.0.delivered()
            }

            pub fn breaks(&self) -> Vec<Break> {
                self.0.breaks()
            }

            pub fn stall_retries(&self) -> u64 {
                self.0.stall_retries()
            }

            pub fn max_num_samps(&self) -> usize {
                self.0.max_num_samps()
            }
        }
    };
}

/// What changes the receiver, inside a pause.
macro_rules! changes {
    ($t:ident) => {
        impl $t {
            /// [`crate::Stream::reconfigure`].
            pub fn reconfigure<T>(&self, f: impl AsyncFnOnce(&mut crate::Rtl) -> Result<T>) -> Result<(T, Break)> {
                block_on(self.0.reconfigure(f))
            }

            /// [`crate::Stream::retune`].
            pub fn retune(&self, req: &TuneRequest) -> Result<(TuneResult, Break)> {
                block_on(self.0.retune(req))
            }

            /// [`crate::Stream::set_gain`].
            pub fn set_gain(&self, db: f64) -> Result<Break> {
                block_on(self.0.set_gain(db))
            }

            /// [`crate::Stream::set_stage_level`].
            pub fn set_stage_level(&self, stage: Stage, level: usize) -> Result<Break> {
                block_on(self.0.set_stage_level(stage, level))
            }

            /// [`crate::Stream::set_bandwidth`].
            pub fn set_bandwidth(&self, hz: f64) -> Result<Break> {
                block_on(self.0.set_bandwidth(hz))
            }
        }
    };
}

/// What reads the samples.
macro_rules! reads {
    ($t:ident) => {
        impl $t {
            /// [`crate::Stream::recv_into`].
            pub fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
                block_on(self.0.recv_into(buf, timeout_secs))
            }

            /// Start the stream now rather than at the first read.
            pub fn start(&mut self) -> Result<()> {
                block_on(self.0.start())
            }
        }

        /// The wire bytes: unsigned 8-bit I then Q, as the chip sends them.
        /// Blocks until at least one byte; end of file once the stream has
        /// finished.
        impl io::Read for $t {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                block_on(self.0.read(buf)).map_err(io::Error::other)
            }
        }

        impl RxSource for $t {
            fn max_num_samps(&self) -> usize {
                self.0.max_num_samps()
            }

            fn start(&mut self) -> Result<()> {
                block_on(self.0.start())
            }

            fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
                block_on(self.0.recv_into(buf, timeout_secs))
            }

            fn setup_worker_thread(&self) {
                elevate_current_thread();
            }
        }
    };
}

reports!(Stream);
reports!(ReadHalf);
reports!(Control);
changes!(Stream);
changes!(Control);
reads!(Stream);
reads!(ReadHalf);

impl Stream {
    /// The async stream.
    pub fn into_async(self) -> crate::Stream {
        self.0
    }

    /// The two halves ([`crate::Stream::split`]).
    pub fn split(self) -> (ReadHalf, Control) {
        let (r, c) = self.0.split();
        (ReadHalf(r), Control(c))
    }

    /// The halves joined again ([`crate::Stream::reunite`]).
    pub fn reunite(read: ReadHalf, control: Control) -> core::result::Result<Stream, (ReadHalf, Control)> {
        crate::Stream::reunite(read.0, control.0).map(Stream).map_err(|(r, c)| (ReadHalf(r), Control(c)))
    }

    /// [`crate::Stream::finish`].
    pub fn finish(self) -> Result<Finished> {
        block_on(self.0.finish()).map(Finished::from)
    }

    /// For bench programs: [`crate::Stream::set_drain`].
    #[cfg(feature = "bench")]
    pub fn set_drain(&self, d: core::time::Duration) {
        block_on(self.0.set_drain(d))
    }

    /// For bench programs: [`crate::Stream::set_fifo_reset`].
    #[cfg(feature = "bench")]
    pub fn set_fifo_reset(&self, on: bool) {
        block_on(self.0.set_fifo_reset(on))
    }
}

/// The request layer ([`crate::usb::Link`]), blocking, for bench programs.
#[cfg(feature = "bench")]
pub struct Link<'a>(&'a crate::usb::Link);

#[cfg(feature = "bench")]
impl Link<'_> {
    pub fn get(&self, value: u16, index: u16, len: u16, context: &'static str) -> Result<Vec<u8>> {
        block_on(self.0.get(value, index, len, context))
    }

    pub fn set(&self, value: u16, index: u16, data: &[u8], context: &'static str) -> Result<()> {
        block_on(self.0.set(value, index, data, context))
    }

    pub fn sys_read8(&self, off: u16) -> Result<u8> {
        block_on(self.0.sys_read8(off))
    }

    pub fn usb_read8(&self, off: u16) -> Result<u8> {
        block_on(self.0.usb_read8(off))
    }

    pub fn usb_write8(&self, off: u16, v: u8) -> Result<()> {
        block_on(self.0.usb_write8(off, v))
    }

    pub fn usb_read_n(&self, off: u16, len: u16) -> Result<Vec<u8>> {
        block_on(self.0.usb_read_n(off, len))
    }

    pub fn usb_write_n(&self, off: u16, data: &[u8]) -> Result<()> {
        block_on(self.0.usb_write_n(off, data))
    }

    pub fn demod_read(&self, page: u8, off: u8, len: u16) -> Result<Vec<u8>> {
        block_on(self.0.demod_read(page, off, len))
    }

    pub fn demod_read8(&self, page: u8, off: u8) -> Result<u8> {
        block_on(self.0.demod_read8(page, off))
    }

    pub fn demod_write(&self, page: u8, off: u8, data: &[u8]) -> Result<()> {
        block_on(self.0.demod_write(page, off, data))
    }

    pub fn demod_rmw8(&self, page: u8, off: u8, mask: u8, value: u8) -> Result<u8> {
        block_on(self.0.demod_rmw8(page, off, mask, value))
    }

    pub fn eeprom_read(&self, offset: u8, len: usize) -> Result<Vec<u8>> {
        block_on(self.0.eeprom_read(offset, len))
    }

    pub fn eeprom_write(&self, offset: u8, data: &[u8]) -> Result<()> {
        block_on(self.0.eeprom_write(offset, data))
    }

    pub fn tuner_write(&self, reg: u8, v: u8) -> Result<()> {
        block_on(self.0.tuner_write(reg, v))
    }

    pub fn tuner_read(&self, len: u16) -> Result<Vec<u8>> {
        block_on(self.0.tuner_read(len))
    }

    pub fn tuner_status(&self) -> Result<[u8; 5]> {
        block_on(self.0.tuner_status())
    }

    /// [`crate::usb::Link::with_tuner`]: `f` runs with the I²C repeater
    /// open.
    pub fn with_tuner<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        block_on(self.0.with_tuner(async |_| f(self)))
    }

    pub fn stall_retries(&self) -> u64 {
        self.0.stall_retries()
    }

    pub fn refusals(&self) -> Vec<crate::usb::Refusal> {
        self.0.refusals()
    }
}

/// The raw stream ([`crate::RtlStream`]), blocking, for bench programs.
#[cfg(feature = "bench")]
pub struct RtlStream(crate::RtlStream);

#[cfg(feature = "bench")]
impl RtlStream {
    pub fn rate(&self) -> f64 {
        self.0.rate()
    }

    pub fn state(&self) -> crate::stream::State {
        self.0.state()
    }

    pub fn is_paused(&self) -> bool {
        self.0.is_paused()
    }

    pub fn max_num_samps(&self) -> usize {
        self.0.max_num_samps()
    }

    pub fn delivered(&self) -> u64 {
        self.0.delivered()
    }

    pub fn start(&mut self) -> Result<()> {
        block_on(self.0.start())
    }

    pub fn stop(&mut self) -> Result<()> {
        block_on(self.0.stop())
    }

    pub fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        block_on(self.0.recv_into(buf, timeout_secs))
    }

    pub fn recv_bytes(&mut self, buf: &mut [u8], timeout_secs: f64) -> Result<(usize, RecvInfo)> {
        block_on(self.0.recv_bytes(buf, timeout_secs))
    }

    pub fn take_dropped_blocks(&mut self) -> Result<crate::DroppedBlocks> {
        block_on(self.0.take_dropped_blocks())
    }

    pub fn pause(&mut self) -> Result<()> {
        block_on(self.0.pause())
    }

    pub fn resume(&mut self) -> Result<()> {
        block_on(self.0.resume())
    }

    pub fn pause_blind(&mut self) -> Result<()> {
        block_on(self.0.pause_blind())
    }

    pub fn resume_blind(&mut self) -> Result<()> {
        block_on(self.0.resume_blind())
    }

    pub fn halt_blind(&mut self) -> Result<()> {
        block_on(self.0.halt_blind())
    }

    pub fn restart_blind(&mut self) -> Result<()> {
        block_on(self.0.restart_blind())
    }

    pub fn reset_fifo(&mut self) -> Result<()> {
        block_on(self.0.reset_fifo())
    }

    pub fn hold(&mut self) -> Result<u64> {
        block_on(self.0.hold())
    }

    pub fn release(&mut self) -> Result<()> {
        self.0.release()
    }

    pub fn absorb(&mut self, span: core::time::Duration) -> Result<()> {
        block_on(self.0.absorb(span))
    }

    /// Start the stream on a worker thread (`sdr_core::IqStream`), at most
    /// `depth` chunks queued for the consumer.
    pub fn into_stream(self, depth: usize) -> Result<sdr_core::IqStream> {
        sdr_core::IqStream::spawn(self, depth)
    }
}

#[cfg(feature = "bench")]
impl RxSource for RtlStream {
    fn max_num_samps(&self) -> usize {
        self.0.max_num_samps()
    }

    fn start(&mut self) -> Result<()> {
        RtlStream::start(self)
    }

    fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        RtlStream::recv_into(self, buf, timeout_secs)
    }

    fn setup_worker_thread(&self) {
        elevate_current_thread();
    }
}
