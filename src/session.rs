//! The receiver as a stream of samples that cannot lose any unseen.
//!
//! The RTL2832U puts nothing in its sample stream but the samples: no
//! timestamp, no sequence number, no mark where anything was lost. A
//! sample's place in the stream is its time — exactly, while nothing is
//! lost. So [`Stream`] owns the receiver, and offers only what keeps that:
//!
//! - **Reading.** [`std::io::Read`] hands out the wire bytes as the chip
//!   sends them (unsigned 8-bit I then Q, the layout RTL-SDR tools write),
//!   so `std::io::copy(&mut stream, &mut file)` makes a capture any of them
//!   reads; [`Stream::recv_into`] hands out the same samples widened to
//!   [`Iq`]. The transfer queue is kept at least [`MIN_QUEUE_DEPTH`] deep:
//!   while the chip serves a control request, and after a pause, it ends
//!   each waiting transfer early, so the count of transfers in flight is
//!   what a request can use up (findings 16.7).
//! - **Changing.** [`Stream::reconfigure`] (and [`Stream::retune`],
//!   [`Stream::set_gain`], …) runs a change inside a pause: one pause
//!   write, a short drain, the engine's hold (every sample produced before
//!   it kept, in order), the FIFO reset (its last ~0.5-0.9 ms of the old
//!   configuration discarded by design), the chip's dropped-block counter
//!   read while nothing flows, the change, the queue resubmitted, one
//!   resume write. The result is a [`Break`]: every sample before
//!   `at_sample` is the old configuration, every one from `settled_at` the
//!   new. 128 transfers deep, a paused change lost nothing at 1.2, 1.8, 2.4
//!   and 2.88 Msps on two hosts (findings 16.7, 16.8), except in the first
//!   ~150 ms after a start at ≥ 2.4 Msps, so a change asked for sooner than
//!   [`CHANGE_AFTER_START`] waits, receiving into the backlog meanwhile.
//! - **Two threads.** [`Stream::split`] gives a [`ReadHalf`] (reading) and
//!   a [`Control`] (changing, and what the stream knows), each usable on
//!   its own thread; [`Stream::reunite`] joins them again.
//! - **Finishing.** [`Stream::finish`] pauses with one write, drains, hands
//!   the last samples out ([`Finished::tail`]), stops with nothing flowing
//!   and reads the counter then: [`StreamReport::dropped_blocks`] is the
//!   chip's own count of every block lost in the run.
//!
//! Nothing here reads the chip while samples flow: what such a request
//! costs is the host's (findings 16.8).

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sdr_core::gain::{Stage, StageReading};
use sdr_core::{Error, Iq, RecvInfo, Result, RxSource, TuneRequest, TuneResult, elevate_current_thread};

use crate::Rtl;
use crate::stream::{DroppedBlocks, RtlStream, WIRE_FORMAT};

/// The fewest transfers a stream keeps in flight: at 16 a finish's pause
/// was refused in 6-7 of 8 runs at 2.4 Msps and a paused change lost ~16
/// blocks; at 64 and 128 none were refused on either host, and at 128 a
/// paused change lost nothing at 1.2-2.88 Msps (findings 16.7, 16.8).
pub const MIN_QUEUE_DEPTH: usize = 128;
/// After the pause write, before the hold: the FIFO's last blocks onto
/// the wire (findings 16.4's clean stop waited this long).
pub const DRAIN: Duration = Duration::from_millis(5);
/// A change is made no sooner than this after the start: a change 150 ms
/// after it lost 2-20 blocks in 5 of 8 runs at ≥ 2.4 Msps on one host, and
/// none in 5 runs with the first change at 1 s (findings 16.8).
pub const CHANGE_AFTER_START: Duration = Duration::from_secs(1);
/// The samples after a break that still carry the old configuration: the
/// chip's filter pipeline flushing, 13-28 measured (findings 16.6).
pub const BREAK_SETTLE_SAMPLES: u64 = 32;
/// The longest a receive holds the stream at a time: a change asked for
/// on another thread waits at most about this long.
const RECV_SLICE: Duration = Duration::from_millis(20);
/// How long `finish` waits on each backlog chunk (it is already in hand).
const TAIL_WAIT_S: f64 = 0.0;

/// One break a change made on purpose.
///
/// The endpoint FIFO keeps its fill through a pause, so at each break it
/// is reset once everything it delivered is held: the old configuration's
/// last ~0.5-0.9 ms are **discarded by design** — the chip's counter does
/// not count them — and the old segment ends at `at_sample` (findings
/// 16.5, 16.6). After it the first [`BREAK_SETTLE_SAMPLES`] still carry the
/// old configuration (the filter pipeline); from `settled_at` on, the new.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Break {
    /// The samples handed out before the pause: everything before sample
    /// `at_sample` (from 0) is the old configuration.
    pub at_sample: u64,
    /// From sample `settled_at` on, the new configuration.
    pub settled_at: u64,
    /// What the pause write cost, just before the break: counted by the
    /// chip, read while paused.
    pub dropped_before: DroppedBlocks,
    /// The time from the pause write to the resume write, host clock.
    pub quiet: Duration,
    /// The RF the samples after the break are centred on.
    pub rf_hz: f64,
}

/// What a finished stream did.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamReport {
    /// Samples handed out, the tail included.
    pub samples: u64,
    /// Blocks (94 samples each) the chip dropped in the run: at the
    /// breaks, between them and at the finish.
    pub dropped_blocks: u64,
    /// A read of the counter found it stopped at 255: at least this many.
    pub saturated: bool,
    pub breaks: Vec<Break>,
    /// Requests the chip refused and that were tried again, the run's
    /// share (findings 16.6, 16.7).
    pub stall_retries: u64,
    /// A failed change could not be undone during the run.
    pub configuration_unknown: bool,
}

/// A finished stream: the receiver back, the report, and the samples
/// still on their way when it finished.
pub struct Finished {
    pub rtl: Rtl,
    pub report: StreamReport,
    /// The last wire bytes (unsigned 8-bit I then Q), after everything a
    /// read handed out.
    pub tail: Vec<u8>,
}

impl Finished {
    /// The tail widened as [`Stream::recv_into`] widens.
    pub fn tail_samples(&self) -> Vec<Iq> {
        let (pairs, _) = self.tail.as_chunks::<2>();
        pairs.iter().map(|&[i, q]| WIRE_FORMAT.widen(i, q)).collect()
    }
}

/// A stream could not be made; the receiver is handed back.
pub struct StreamError {
    pub rtl: Box<Rtl>,
    pub error: Error,
}

impl fmt::Debug for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamError").field("error", &self.error).finish_non_exhaustive()
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for StreamError {}

/// An error re-labelled with the step it failed at.
fn at(step: &'static str) -> impl Fn(Error) -> Error {
    move |e| Error::Device { backend: crate::BACKEND, context: step, message: e.to_string() }
}

fn finished_error() -> Error {
    Error::Device { backend: crate::BACKEND, context: "stream", message: "the stream has finished".into() }
}

/// The receiver and the raw stream.
struct Core {
    rtl: Rtl,
    stream: RtlStream,
    started_at: Option<Instant>,
    breaks: Vec<Break>,
    dropped_blocks: u64,
    saturated: bool,
    /// The wait after the pause write (`DRAIN`; bench programs may set it).
    drain: Duration,
    /// Discard the FIFO's held blocks at each break (findings 16.6); on
    /// unless a bench program turns it off.
    fifo_reset: bool,
    /// A failed change could not be undone: the receiver's configuration
    /// is not known (`freq` errs from then on).
    unknown: bool,
    /// The link's stall retries when the stream was made.
    stalls_at_open: u64,
}

impl Core {
    fn start(&mut self) -> Result<()> {
        if self.started_at.is_none() {
            self.stream.start().map_err(at("start: the sequence"))?;
            self.started_at = Some(Instant::now());
        }
        Ok(())
    }

    fn count(&mut self, d: DroppedBlocks) {
        self.dropped_blocks += d.blocks as u64;
        self.saturated |= d.saturated;
    }

    /// Pause, drain, hold, reset the FIFO, read the counter (paused: free).
    fn quiesce(&mut self) -> Result<(u64, DroppedBlocks)> {
        self.stream.pause_blind().map_err(at("pause: the pause write"))?;
        std::thread::sleep(self.drain);
        let pos = self.stream.hold().map_err(at("pause: hold"))?;
        if self.fifo_reset {
            self.stream.reset_fifo().map_err(at("pause: FIFO reset"))?;
        }
        let d = self.stream.take_dropped_blocks().map_err(at("pause: the counter"))?;
        self.count(d);
        Ok((pos, d))
    }

    fn freq(&self) -> Result<f64> {
        if self.unknown {
            return Err(Error::Device {
                backend: crate::BACKEND,
                context: "freq",
                message: "a reconfiguration failed and the last good state could not be written back: the receiver's configuration is not known".into(),
            });
        }
        self.rtl.freq()
    }

    fn reconfigure<T>(&mut self, f: impl FnOnce(&mut Rtl) -> Result<T>) -> Result<(T, Break)> {
        let Some(started_at) = self.started_at else {
            // Nothing flows yet: no break.
            let t = f(&mut self.rtl)?;
            let zero = DroppedBlocks { blocks: 0, saturated: false };
            return Ok((t, Break { at_sample: 0, settled_at: 0, dropped_before: zero, quiet: Duration::ZERO, rf_hz: self.rtl.freq().unwrap_or(f64::NAN) }));
        };
        if let Some(wait) = CHANGE_AFTER_START.checked_sub(started_at.elapsed()) {
            // Too soon after the start: keep receiving into the backlog.
            self.stream.absorb(wait).map_err(at("settle"))?;
        }
        let t0 = Instant::now();
        let (at_sample, dropped_before) = self.quiesce()?;
        let r = f(&mut self.rtl);
        if r.is_err() {
            // The change may have stopped half way (a retune is several
            // writes): the last good state written whole again, so the
            // hardware is what `Rtl` says it is. If that fails too, the
            // stream no longer knows its configuration.
            if self.rtl.reassert().is_err() {
                self.unknown = true;
            }
        }
        self.stream.release().map_err(at("resume: release"))?;
        self.stream.resume_blind().map_err(at("resume: the resume write"))?;
        let rf_hz = self.freq().unwrap_or(f64::NAN);
        let b = Break { at_sample, settled_at: at_sample + BREAK_SETTLE_SAMPLES, dropped_before, quiet: t0.elapsed(), rf_hz };
        self.breaks.push(b);
        r.map(|t| (t, b))
    }

    fn finish(mut self) -> Result<Finished> {
        let mut tail = Vec::new();
        if self.started_at.is_some() {
            self.quiesce()?;
            let mut buf = vec![0u8; 2 * self.stream.max_num_samps()];
            loop {
                let (n, _) = self.stream.recv_bytes(&mut buf, TAIL_WAIT_S).map_err(at("finish: the tail"))?;
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
            }
            // The stop's writes with nothing flowing, then the counter for
            // whatever the hold missed (none expected), then the register
            // back to its power-on state.
            self.stream.stop().map_err(at("finish: stop"))?;
            let d = self.stream.take_dropped_blocks().map_err(at("finish: the counter"))?;
            self.count(d);
            self.stream.resume_blind().map_err(at("finish: the pause bit back"))?;
        }
        let samples = self.stream.delivered();
        let stall_retries = self.rtl.link_ref().stall_retries() - self.stalls_at_open;
        let Core { rtl, stream, breaks, dropped_blocks, saturated, unknown, .. } = self;
        drop(stream);
        Ok(Finished { rtl, report: StreamReport { samples, dropped_blocks, saturated, breaks, stall_retries, configuration_unknown: unknown }, tail })
    }
}

/// What a stream's handles share.
struct Shared {
    core: Mutex<Option<Core>>,
    /// A change waits for the stream: the receive loop lets it in first
    /// (std's mutex is not fair, and the loop re-locks at once).
    control_waiting: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<Core>> {
        self.core.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The stream, ahead of the receive loop.
    fn control(&self) -> MutexGuard<'_, Option<Core>> {
        self.control_waiting.store(true, Ordering::Release);
        let g = self.lock();
        self.control_waiting.store(false, Ordering::Release);
        g
    }

    fn with<T>(&self, f: impl FnOnce(&mut Core) -> Result<T>) -> Result<T> {
        match self.control().as_mut() {
            Some(c) => f(c),
            None => Err(finished_error()),
        }
    }

    /// One receive of at most `timeout_secs`, in slices of [`RECV_SLICE`]
    /// with the stream let go between them, so a change on another thread
    /// waits at most one slice. `done` says whether a slice's result ends
    /// the wait.
    fn recv<T>(&self, timeout_secs: f64, mut slice: impl FnMut(&mut Core, f64) -> Result<T>, done: impl Fn(&T) -> bool) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.0));
        loop {
            while self.control_waiting.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let r = {
                let mut g = self.lock();
                let core = g.as_mut().ok_or_else(finished_error)?;
                core.start()?;
                slice(core, left.min(RECV_SLICE).as_secs_f64())?
            };
            if done(&r) || Instant::now() >= deadline {
                return Ok(r);
            }
        }
    }

    fn recv_into(&self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.recv(timeout_secs, |c, t| c.stream.recv_into(buf, t), |i| !i.timed_out)
    }

    /// `io::Read`: blocks until at least one byte (or the stream finishes:
    /// end of file).
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.recv(1.0, |c, t| c.stream.recv_bytes(buf, t), |(n, _)| *n > 0) {
                Ok((n, _)) if n > 0 => return Ok(n),
                Ok(_) => continue,
                Err(_) if self.lock().is_none() => return Ok(0),
                Err(e) => return Err(io::Error::other(e)),
            }
        }
    }

    fn max_num_samps(&self) -> usize {
        self.lock().as_ref().map(|c| c.stream.max_num_samps()).unwrap_or(0)
    }

    fn get<T>(&self, f: impl FnOnce(&Core) -> T) -> Result<T> {
        self.control().as_ref().map(f).ok_or_else(finished_error)
    }
}

/// The receiver streaming: see the module documentation.
pub struct Stream {
    shared: Arc<Shared>,
}

/// The reading half of a [`Stream`] ([`Stream::split`]).
pub struct ReadHalf {
    shared: Arc<Shared>,
}

/// The changing half of a [`Stream`] ([`Stream::split`]): changes, and what
/// the stream knows. Cheap to clone; every clone is the same stream's.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

/// What every handle reports, without a request to the chip.
macro_rules! reports {
    ($t:ident) => {
        impl $t {
            /// The output rate (fixed for the stream's life).
            pub fn rate(&self) -> Result<f64> {
                self.shared.get(|c| c.stream.rate())
            }

            /// The RF the samples are centred on, from the words written.
            /// Errs once a failed change could not be undone (the
            /// configuration is then not known).
            pub fn freq(&self) -> Result<f64> {
                self.shared.get(|c| c.freq())?
            }

            /// The sum of the gain stages' dB, as last written.
            pub fn gain(&self) -> Result<f64> {
                self.shared.get(|c| c.rtl.gain())
            }

            /// Each gain stage as last written.
            pub fn stage_readings(&self) -> Result<Vec<StageReading>> {
                self.shared.get(|c| c.rtl.stage_readings())
            }

            /// The receiver's label (product and serial, from the USB
            /// descriptors read at open).
            pub fn label(&self) -> Result<String> {
                self.shared.get(|c| c.rtl.label().to_string())
            }

            /// Samples handed out so far.
            pub fn delivered(&self) -> Result<u64> {
                self.shared.get(|c| c.stream.delivered())
            }

            /// The breaks made so far.
            pub fn breaks(&self) -> Result<Vec<Break>> {
                self.shared.get(|c| c.breaks.clone())
            }

            /// Requests the chip refused and that were tried again since
            /// the receiver was opened (findings 16.6, 16.7).
            pub fn stall_retries(&self) -> Result<u64> {
                self.shared.get(|c| c.rtl.link_ref().stall_retries())
            }
        }
    };
}

/// What changes the receiver, inside a pause.
macro_rules! changes {
    ($t:ident) => {
        impl $t {
            /// Run `f` on the receiver inside a pause; returns what `f`
            /// returned and the [`Break`]. If `f` fails, the receiver's
            /// last good state is written back, the stream resumes and the
            /// break is recorded. Before the first read (not started) `f`
            /// runs directly and the break is empty; within
            /// [`CHANGE_AFTER_START`] of the start the change waits,
            /// receiving into the backlog.
            pub fn reconfigure<T>(&self, f: impl FnOnce(&mut Rtl) -> Result<T>) -> Result<(T, Break)> {
                self.shared.with(|c| c.reconfigure(f))
            }

            /// [`Rtl::set_freq`] inside a pause.
            pub fn retune(&self, req: &TuneRequest) -> Result<(TuneResult, Break)> {
                self.reconfigure(|rtl| rtl.set_freq(req))
            }

            /// [`Rtl::set_gain`] inside a pause.
            pub fn set_gain(&self, db: f64) -> Result<Break> {
                self.reconfigure(|rtl| rtl.set_gain(db)).map(|(_, b)| b)
            }

            /// [`Rtl::set_stage_level`] inside a pause.
            pub fn set_stage_level(&self, stage: Stage, level: usize) -> Result<Break> {
                self.reconfigure(|rtl| rtl.set_stage_level(stage, level)).map(|(_, b)| b)
            }

            /// [`Rtl::set_bandwidth`] inside a pause.
            pub fn set_bandwidth(&self, hz: f64) -> Result<Break> {
                self.reconfigure(|rtl| rtl.set_bandwidth(hz)).map(|(_, b)| b)
            }
        }
    };
}

/// What reads the samples.
macro_rules! reads {
    ($t:ident) => {
        impl $t {
            /// Samples into `buf`, widened to [`Iq`] (full scale ±32768);
            /// at most `timeout_secs` waiting. Starts the stream on the
            /// first call.
            pub fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
                self.shared.recv_into(buf, timeout_secs)
            }

            /// Samples a receive hands out at most.
            pub fn max_num_samps(&self) -> usize {
                self.shared.max_num_samps()
            }
        }

        /// The wire bytes: unsigned 8-bit I then Q, as the chip sends them.
        /// Blocks until at least one byte; end of file once the stream has
        /// finished.
        impl io::Read for $t {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.shared.read(buf)
            }
        }

        impl RxSource for $t {
            fn max_num_samps(&self) -> usize {
                self.shared.max_num_samps()
            }

            fn start(&mut self) -> Result<()> {
                self.shared.with(Core::start)
            }

            fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
                self.shared.recv_into(buf, timeout_secs)
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
    /// A stream of `rtl` at the rate it holds: at least [`MIN_QUEUE_DEPTH`]
    /// transfers in flight (0 for either is the default size). Nothing is
    /// written until the first read (or `start`).
    pub(crate) fn new(mut rtl: Rtl, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<Stream, StreamError> {
        let stalls_at_open = rtl.link_ref().stall_retries();
        match rtl.open_stream(transfer_bytes, queue_depth.max(MIN_QUEUE_DEPTH)) {
            Ok(stream) => {
                let core = Core { rtl, stream, started_at: None, breaks: Vec::new(), dropped_blocks: 0, saturated: false, drain: DRAIN, fifo_reset: true, unknown: false, stalls_at_open };
                Ok(Stream { shared: Arc::new(Shared { core: Mutex::new(Some(core)), control_waiting: AtomicBool::new(false) }) })
            }
            Err(error) => Err(StreamError { rtl: Box::new(rtl), error }),
        }
    }

    /// The two halves, each usable on its own thread.
    pub fn split(self) -> (ReadHalf, Control) {
        (ReadHalf { shared: Arc::clone(&self.shared) }, Control { shared: self.shared })
    }

    /// The halves of one stream joined again; handed back if they are not
    /// the same stream's, or a clone of the control still exists.
    pub fn reunite(read: ReadHalf, control: Control) -> std::result::Result<Stream, (ReadHalf, Control)> {
        if !Arc::ptr_eq(&read.shared, &control.shared) || Arc::strong_count(&read.shared) != 2 {
            return Err((read, control));
        }
        drop(read);
        Ok(Stream { shared: control.shared })
    }

    /// Pause, hand out the samples still on their way, stop with nothing
    /// flowing, and read the dropped-block counter then: the receiver back,
    /// the report, the tail.
    pub fn finish(self) -> Result<Finished> {
        let core = self.shared.control().take().ok_or_else(finished_error)?;
        core.finish()
    }

    /// The wait between the pause write and the hold, for bench programs.
    #[cfg(feature = "bench")]
    pub fn set_drain(&self, d: Duration) {
        let _ = self.shared.with(|c| {
            c.drain = d;
            Ok(())
        });
    }

    /// Discard the FIFO's held blocks at each break (the default), for
    /// bench programs.
    #[cfg(feature = "bench")]
    pub fn set_fifo_reset(&self, on: bool) {
        let _ = self.shared.with(|c| {
            c.fifo_reset = on;
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_floor_and_the_settle_are_the_measured_ones() {
        assert_eq!(MIN_QUEUE_DEPTH, 128);
        assert_eq!(CHANGE_AFTER_START, Duration::from_secs(1));
        assert!(RECV_SLICE < CHANGE_AFTER_START);
    }
}
