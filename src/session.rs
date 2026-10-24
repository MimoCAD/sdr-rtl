//! The receiver as a stream of samples that cannot lose any unseen.
//!
//! The RTL2832U puts nothing in its sample stream but the samples: no
//! timestamp, no sequence number, no mark where anything was lost. A
//! sample's place in the stream is its time — exactly, while nothing is
//! lost. So [`Stream`] owns the receiver, and offers only what keeps that:
//!
//! - **Reading.** [`Stream::read`] hands out the wire bytes as the chip
//!   sends them (unsigned 8-bit I then Q, the layout RTL-SDR tools write);
//!   [`Stream::recv_into`] the same samples widened to [`Iq`]. (Natively,
//!   `blocking::Stream` is a [`std::io::Read`].) At a rate the chip does
//!   not make itself, the chip runs at 2-4 times it (findings 25):
//!   `recv_into` hands out the decimated samples at the rate set, `read`
//!   the chip's own bytes at the chip's rate ([`Stream::chip_rate`]); the
//!   stream's first read decides which it hands out, and [`Stream::rate`],
//!   the counts and the breaks follow. The transfer queue is kept
//!   at least [`MIN_QUEUE_DEPTH`] deep: while the chip serves a control
//!   request, and after a pause, it ends each waiting transfer early, so
//!   the count of transfers in flight is what a request can use up
//!   (findings 16.7).
//! - **Changing.** [`Stream::reconfigure`] (and [`Stream::retune`],
//!   [`Stream::set_gain`], …) runs a change inside a halt: one write
//!   clearing the start bit, a short drain, the engine's hold (every
//!   sample produced before it kept, in order), the FIFO reset (its last
//!   ~0.5-0.9 ms of the old configuration discarded by design), the chip's
//!   dropped-block counter read while nothing flows, the change, the queue
//!   resubmitted, one write setting the start bit. The result is a
//!   [`Break`]: every sample before `at_sample` is the old configuration,
//!   every one from `settled_at` the new. 128 transfers deep, a change
//!   lost nothing at 1.024-2.88 Msps (findings 16.7-16.9, 19) nor at
//!   0.24, 0.288, 0.30, 0.905, 0.96 and 1.0 Msps (findings 25), except in
//!   the first ~150 ms after a start at ≥ 2.4 Msps, so a change asked for
//!   sooner than [`CHANGE_AFTER_START`] waits, receiving into the backlog
//!   meanwhile. (Until findings 19 the halt was page 1 0x01 bit 2, the
//!   "pause"; it also reset the demodulator's signal state, the tone ~30×
//!   down after every change for hundreds of ms.)
//! - **Two at once.** [`Stream::split`] gives a [`ReadHalf`] (reading) and
//!   a [`Control`] (changing), for two threads natively or two tasks in a
//!   browser; [`Stream::reunite`] joins them. What the stream knows (rate,
//!   frequency, gain, breaks, samples delivered) any handle reports at once,
//!   without waiting for the other.
//! - **Finishing.** [`Stream::finish`] halts with one write, drains, hands
//!   the last samples out ([`Finished::tail`]), stops with nothing flowing
//!   and reads the counter then: [`StreamReport::dropped_blocks`] is the
//!   chip's own count of every block lost in the run.
//!
//! Every method that waits is async: in a browser it suspends; natively it
//! completes in place (nusb's native requests block), and `blocking` wraps
//! each one. Nothing here reads the chip while samples flow: what such a
//! request costs is the host's (findings 16.8).

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::boxed::Box;
use std::string::{String, ToString};
use std::sync::{Arc, Mutex, MutexGuard};
use std::vec::Vec;

use sdr_core::gain::StageReading;
use sdr_core::sync::Lock;
use sdr_core::time::Instant;
use sdr_core::{Error, Iq, RecvInfo, Result, TuneRequest, TuneResult};

use crate::decim::Decimator;
use crate::stream::{DroppedBlocks, RtlStream, WIRE_FORMAT};
use crate::{Rtl, Stage};

/// The fewest transfers a stream keeps in flight: with full-packet mode
/// ~440 ms at 2.4 Msps (findings 18). Measured before it, when a transfer
/// held one block: at 16 a finish's pause was refused in 6-7 of 8 runs and
/// a paused change lost ~16 blocks, at 128 neither (findings 16.7, 16.8).
pub const MIN_QUEUE_DEPTH: usize = 128;
/// After the pause write, before the hold: the FIFO's last blocks onto
/// the wire (findings 16.4's clean stop waited this long).
pub const DRAIN: Duration = Duration::from_millis(5);
/// A change is made no sooner than this after the start: a change 150 ms
/// after it lost 2-20 blocks in 5 of 8 runs at ≥ 2.4 Msps on one host, and
/// none in 5 runs with the first change at 1 s (findings 16.8).
pub const CHANGE_AFTER_START: Duration = Duration::from_secs(1);
/// The samples after a break that may still carry the old configuration.
/// Findings 19 (2026-10-11, a USRP tone): quiesced with the start bit,
/// the new configuration begins at `at_sample` itself — −13 to +9 per
/// sample over 400 breaks at 1.024-2.88 Msps, inside the estimator's ±8;
/// −7 to +9 over 240 breaks at 0.24-0.30 and 0.905-1.0 Msps (findings 25).
/// (With the old pause bit a 64-sample window put it 5-32 after, the
/// near-silence that bit left.) 40 keeps a wide margin, for hosts not yet
/// measured.
pub const BREAK_SETTLE_SAMPLES: u64 = 40;
/// The longest a receive holds the stream at a time: a change asked for
/// on another thread or task waits at most about this long.
const RECV_SLICE: Duration = Duration::from_millis(20);
/// How long `finish` waits on each backlog chunk (it is already in hand).
const TAIL_WAIT_S: f64 = 0.0;
/// How long one `read` waits per round before trying again.
const READ_ROUND_S: f64 = 1.0;
/// After a change, the attempts at writing the stream back to running
/// (packing on, the resume write), each attempt after the request layer's
/// own stall retries.
pub const RESUME_ATTEMPTS: u32 = 3;
/// A running stream that hands out nothing for this long is an error, not
/// a wait: at the slowest rate (1.024 Msps) a transfer fills in 8 ms, and
/// the longest quiet the driver makes on purpose, a change's pause, is
/// ~60 ms (findings 16.7, 16.9), so 2 s is a dongle that has stopped.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(2);
/// The breaks a stream keeps, the most recent (every one is counted in
/// [`StreamReport::breaks_total`]): a receiver retuning around the clock
/// makes ~14,000 a day.
pub const BREAKS_KEPT: usize = 256;

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
    /// The last [`BREAKS_KEPT`] breaks, oldest first.
    pub breaks: Vec<Break>,
    /// Every break the run made.
    pub breaks_total: u64,
    /// Samples the host-side ledger inferred lost in the run (the chip's
    /// clock against what came off the wire): loss the chip's counter may
    /// not see.
    pub host_lost_samples: u64,
    /// Samples the stream fell behind the clock by, beyond its drift
    /// (`clockwatch`, findings 27): loss neither the chip's counter nor
    /// the ledger saw — single blocks the chip sheds without counting. In
    /// the chip's samples.
    pub clock_lost_samples: u64,
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
    /// read handed out: the chip's, at its rate (decimating, not the rate
    /// set; [`Finished::tail_samples`] is).
    pub tail: Vec<u8>,
    /// Decimating: the tail through the decimator.
    decimated_tail: Option<Vec<Iq>>,
}

impl Finished {
    /// The tail as [`Stream::recv_into`] hands samples out: widened, and
    /// decimated when the stream decimates.
    pub fn tail_samples(&self) -> Vec<Iq> {
        self.decimated_tail
            .clone()
            .unwrap_or_else(|| widen(&self.tail))
    }
}

/// Wire bytes widened as the engine widens them.
pub(crate) fn widen(bytes: &[u8]) -> Vec<Iq> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[i, q]| WIRE_FORMAT.widen(i, q))
        .collect()
}

/// A stream could not be made; the receiver is handed back.
pub struct StreamError {
    pub rtl: Box<Rtl>,
    pub error: Error,
}

impl fmt::Debug for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl core::error::Error for StreamError {}

/// An error re-labelled with the step it failed at.
fn at(step: &'static str) -> impl Fn(Error) -> Error {
    move |e| Error::Device {
        backend: crate::BACKEND,
        context: step,
        message: e.to_string(),
    }
}

fn finished_error() -> Error {
    Error::Device {
        backend: crate::BACKEND,
        context: "stream",
        message: "the stream has finished".into(),
    }
}

fn broken_error(why: &str) -> Error {
    Error::Device {
        backend: crate::BACKEND,
        context: "stream",
        message: format!("the stream could not be resumed after a change and has stopped: {why}"),
    }
}

fn silence_error() -> Error {
    Error::Device {
        backend: crate::BACKEND,
        context: "stream",
        message: format!(
            "no samples for {} s while running: the dongle has stopped sending",
            SILENCE_LIMIT.as_secs()
        ),
    }
}

fn mixed_read_error(chosen: Handout, chip_rate: f64, k: u32) -> Error {
    let (chip, out) = (chip_rate / 1e6, chip_rate / k as f64 / 1e6);
    Error::Unsupported {
        backend: crate::BACKEND,
        what: match chosen {
            Handout::WireBytes => format!(
                "recv_into: this stream has been read as wire bytes, the chip's samples at {chip:.6} Msps; the samples decimated by {k} to {out:.6} Msps come from a stream read only with recv_into (both take from one flow)"
            ),
            _ => format!(
                "read: this stream hands out samples decimated by {k} to {out:.6} Msps (recv_into); the chip's wire bytes at {chip:.6} Msps come from a stream read only with read (both take from one flow)"
            ),
        },
    }
}

fn unknown_error() -> Error {
    Error::Device {
        backend: crate::BACKEND,
        context: "freq",
        message: "a reconfiguration failed and the last good state could not be written back: the receiver's configuration is not known".into(),
    }
}

/// How long the reading half naps while a change waits for the stream. A
/// nap, not a yield: natively the reading thread may be realtime (SCHED_RR
/// on Linux, `elevate_current_thread`), and a realtime thread's yield
/// never gives way to the normal-priority thread making the change; a
/// sleep does. 200 µs against a change's ~60-70 ms (findings 16.7, 16.9).
#[cfg(not(target_arch = "wasm32"))]
const CHANGE_WAIT_NAP: Duration = Duration::from_micros(200);

/// Let the thread or task making a change run.
async fn yield_now() {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(CHANGE_WAIT_NAP);
    #[cfg(target_arch = "wasm32")]
    sdr_core::time::sleep(Duration::ZERO).await;
}

/// The receiver and the raw stream.
struct Core {
    rtl: Rtl,
    stream: RtlStream,
    started_at: Option<Instant>,
    breaks: Vec<Break>,
    breaks_total: u64,
    /// The last time samples came, or the stream was set running (the
    /// start, the end of a change): [`SILENCE_LIMIT`] is counted from it.
    last_data: Option<Instant>,
    /// The stream could not be written back to running after a change:
    /// every later read and change fails with this.
    broken: Option<String>,
    dropped_blocks: u64,
    saturated: bool,
    /// The wait after the pause write (`DRAIN`; bench programs may set it).
    drain: Duration,
    /// Discard the FIFO's held blocks at each break (findings 16.6); on
    /// unless a bench program turns it off.
    fifo_reset: bool,
    /// Quiesce a change with page 1 0x01 bit 2 (the "pause" bit) instead
    /// of the start bit; off unless a bench program turns it on. Findings
    /// 19: that bit also resets the demodulator's signal state — the tone
    /// fell ~30× at every break and climbed back over hundreds of ms — so
    /// the driver halts and restarts with the start bit instead.
    quiesce_with_pause: bool,
    /// A failed change could not be undone: the receiver's configuration
    /// is not known (`freq` errs from then on).
    unknown: bool,
    /// The link's stall retries when the stream was made.
    stalls_at_open: u64,
    /// The decimation of a rate the chip does not reach (`decim`); None
    /// when the chip delivers the rate itself.
    decim: Option<Decimator>,
    /// The chip's samples for one decimated receive.
    scratch: Vec<Iq>,
    /// One decimated receive's outputs.
    decimated: Vec<Iq>,
    /// What a decimating stream hands out, decided by its first read.
    handout: Handout,
}

/// What a decimating stream hands out: decided by the first read, kept for
/// the stream's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handout {
    Undecided,
    /// [`Stream::recv_into`]: the samples at the rate set.
    Samples,
    /// [`Stream::read`]: the chip's wire bytes at the chip's rate.
    WireBytes,
}

/// What the stream knows, readable by every handle without the stream's
/// lock: updated after every change and every receive.
#[derive(Clone, Debug)]
struct Snapshot {
    /// The rate handed out.
    rate: f64,
    /// The rate the chip delivers.
    chip_rate: f64,
    /// The decimation between them (1: none).
    decimation: u32,
    /// `None` once a failed change could not be undone.
    freq: Option<f64>,
    gain: f64,
    stages: Vec<StageReading<Stage>>,
    label: String,
    breaks: Vec<Break>,
    breaks_total: u64,
    delivered: u64,
    max_num_samps: usize,
    finished: bool,
}

impl Core {
    /// The decimation (1: none).
    fn factor(&self) -> u32 {
        self.decim.as_ref().map_or(1, Decimator::factor)
    }

    fn snapshot(&self) -> Snapshot {
        let k = self.decimating().map_or(1, Decimator::factor);
        Snapshot {
            rate: self.handed_out_rate(),
            chip_rate: self.stream.rate(),
            decimation: k,
            freq: if self.unknown {
                None
            } else {
                self.rtl.freq().ok()
            },
            gain: self.rtl.gain(),
            stages: self.rtl.stage_readings(),
            label: self.rtl.label().to_string(),
            breaks: self.breaks.clone(),
            breaks_total: self.breaks_total,
            delivered: self.delivered(),
            max_num_samps: (self.stream.max_num_samps() / k as usize).max(1),
            finished: false,
        }
    }

    /// The decimator, unless the stream hands out the chip's samples (no
    /// decimation, or read as wire bytes).
    fn decimating(&self) -> Option<&Decimator> {
        if self.handout == Handout::WireBytes {
            None
        } else {
            self.decim.as_ref()
        }
    }

    /// Samples handed out: the chip's, or the decimator's outputs.
    fn delivered(&self) -> u64 {
        self.decimating()
            .map_or(self.stream.delivered(), Decimator::outputs)
    }

    /// The rate handed out: the rate set, or the chip's once read as wire
    /// bytes.
    fn handed_out_rate(&self) -> f64 {
        self.stream.rate() / self.decimating().map_or(1, Decimator::factor) as f64
    }

    /// The first sample handed out after a break at the chip's sample
    /// `chip_at`, and the first clear of it ([`BREAK_SETTLE_SAMPLES`] of
    /// the chip's on, then the decimator's reach).
    fn break_samples(&self, chip_at: u64) -> (u64, u64) {
        match self.decimating() {
            None => (chip_at, chip_at + BREAK_SETTLE_SAMPLES),
            Some(d) => (
                d.first_output_touching(chip_at),
                d.first_output_clear_of(chip_at + BREAK_SETTLE_SAMPLES),
            ),
        }
    }

    /// Decimating, the first read decides what the stream hands out from
    /// then on; the other kind of read is refused, with the reason (both
    /// take from the one flow of the chip's samples).
    fn choose(&mut self, want: Handout) -> Result<()> {
        if self.decim.is_none() || self.handout == want {
            return Ok(());
        }
        if self.handout == Handout::Undecided {
            self.handout = want;
            return Ok(());
        }
        Err(mixed_read_error(
            self.handout,
            self.stream.rate(),
            self.factor(),
        ))
    }

    /// Samples into `buf`: the chip's, or (decimating) the chip's for
    /// `buf.len()` × k through the decimator, at most `buf.len()` out.
    async fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.choose(Handout::Samples)?;
        let Core {
            stream,
            decim,
            scratch,
            decimated,
            ..
        } = self;
        let Some(d) = decim.as_mut() else {
            return stream.recv_into(buf, timeout_secs).await;
        };
        let want = buf.len() * d.factor() as usize;
        if scratch.len() < want {
            scratch.resize(want, Iq::default());
        }
        let mut info = stream.recv_into(&mut scratch[..want], timeout_secs).await?;
        decimated.clear();
        d.process(&scratch[..info.n_samples], decimated);
        buf[..decimated.len()].copy_from_slice(decimated);
        info.n_samples = decimated.len();
        Ok(info)
    }

    /// The chip's wire bytes, at the chip's rate (decimating, that makes
    /// this a stream of the chip's samples: [`Stream::rate`] then reports
    /// the chip's rate).
    async fn recv_bytes(&mut self, buf: &mut [u8], timeout_secs: f64) -> Result<(usize, RecvInfo)> {
        self.choose(Handout::WireBytes)?;
        self.stream.recv_bytes(buf, timeout_secs).await
    }

    async fn start(&mut self) -> Result<()> {
        if let Some(why) = &self.broken {
            return Err(broken_error(why));
        }
        if self.started_at.is_none() {
            self.stream
                .start()
                .await
                .map_err(at("start: the sequence"))?;
            self.started_at = Some(Instant::now());
            self.last_data = self.started_at;
        }
        Ok(())
    }

    fn push_break(&mut self, b: Break) {
        if self.breaks.len() == BREAKS_KEPT {
            self.breaks.remove(0);
        }
        self.breaks.push(b);
        self.breaks_total += 1;
    }

    /// After a change, successful or not: the engine released, full-packet
    /// mode back on and the resume written, each tried even when one before
    /// it failed, up to [`RESUME_ATTEMPTS`] times. The resume write is what
    /// decides: written, samples flow (a packing failure is still returned
    /// as the change's error); not written, the stream is marked broken.
    async fn resume_after_change(&mut self) -> Result<()> {
        let released = self.stream.release().map_err(at("resume: release"));
        let mut packing = Ok(());
        let mut resumed = Ok(());
        for _ in 0..RESUME_ATTEMPTS {
            packing = self
                .stream
                .set_packing(true)
                .await
                .map_err(at("resume: packing on"));
            resumed = if self.quiesce_with_pause {
                self.stream.resume_blind().await
            } else {
                self.stream.restart_blind().await
            }
            .map_err(at("resume: the resume write"));
            if packing.is_ok() && resumed.is_ok() {
                break;
            }
        }
        if let Err(e) = &resumed {
            self.broken = Some(e.to_string());
        }
        self.last_data = Some(Instant::now());
        released.and(resumed).and(packing)
    }

    fn count(&mut self, d: DroppedBlocks) {
        self.dropped_blocks += d.blocks as u64;
        self.saturated |= d.saturated;
    }

    /// Pause, drain, hold, reset the FIFO, read the counter (paused: free).
    /// Full-packet mode is turned off once paused, so a transfer the last
    /// packets only partly filled ends now, not after the resume with the
    /// new configuration's samples (a browser cannot cancel it).
    async fn quiesce(&mut self) -> Result<(u64, DroppedBlocks)> {
        if self.quiesce_with_pause {
            self.stream.pause_blind().await
        } else {
            self.stream.halt_blind().await
        }
        .map_err(at("pause: the halt write"))?;
        sdr_core::time::sleep(self.drain).await;
        self.stream
            .set_packing(false)
            .await
            .map_err(at("pause: packing off"))?;
        sdr_core::time::sleep(self.drain).await;
        let pos = self.stream.hold().await.map_err(at("pause: hold"))?;
        if self.fifo_reset {
            self.stream
                .reset_fifo()
                .await
                .map_err(at("pause: FIFO reset"))?;
        }
        let d = self
            .stream
            .take_dropped_blocks()
            .await
            .map_err(at("pause: the counter"))?;
        self.count(d);
        Ok((pos, d))
    }

    async fn reconfigure<T>(
        &mut self,
        f: impl AsyncFnOnce(&mut Rtl) -> Result<T>,
    ) -> Result<(T, Break)> {
        if let Some(why) = &self.broken {
            return Err(broken_error(why));
        }
        let Some(started_at) = self.started_at else {
            // Nothing flows yet: no break.
            // (Nothing has been handed out: sample 0 is the chip's and the
            // decimator's alike.)
            let t = f(&mut self.rtl).await?;
            let zero = DroppedBlocks {
                blocks: 0,
                saturated: false,
            };
            return Ok((
                t,
                Break {
                    at_sample: 0,
                    settled_at: 0,
                    dropped_before: zero,
                    quiet: Duration::ZERO,
                    rf_hz: self.rtl.freq().unwrap_or(f64::NAN),
                },
            ));
        };
        if let Some(wait) = CHANGE_AFTER_START.checked_sub(started_at.elapsed()) {
            // Too soon after the start: keep receiving into the backlog.
            self.stream.absorb(wait).await.map_err(at("settle"))?;
        }
        // The halt's wake read (findings 21), if due, before the quiet is
        // timed: samples still flow while it is made.
        self.rtl
            .link
            .ready_demod_write()
            .await
            .map_err(at("pause: wake read"))?;
        let t0 = Instant::now();
        let quiesced = self.quiesce().await;
        let r = match &quiesced {
            Ok(_) => {
                let r = f(&mut self.rtl).await;
                if r.is_err() {
                    // The change may have stopped half way (a retune is
                    // several writes): the last good state written whole
                    // again, so the hardware is what `Rtl` says it is. If
                    // that fails too, the stream no longer knows its
                    // configuration.
                    if self.rtl.reassert().await.is_err() {
                        self.unknown = true;
                    }
                }
                r
            }
            // The pause failed part way: no change made, and whatever was
            // paused is resumed below.
            Err(e) => Err(Error::Device {
                backend: crate::BACKEND,
                context: "pause",
                message: e.to_string(),
            }),
        };
        let resumed = self.resume_after_change().await;
        // A pause that failed part way has no position from the hold: the
        // break is placed at the samples handed out by then, and what the
        // pause cost is not known (counted as nothing).
        let (chip_at, dropped_before) = quiesced.unwrap_or((
            self.stream.delivered(),
            DroppedBlocks {
                blocks: 0,
                saturated: false,
            },
        ));
        let rf_hz = if self.unknown {
            f64::NAN
        } else {
            self.rtl.freq().unwrap_or(f64::NAN)
        };
        // In the samples handed out: the chip's, or the decimator's.
        let (at_sample, settled_at) = self.break_samples(chip_at);
        let b = Break {
            at_sample,
            settled_at,
            dropped_before,
            quiet: t0.elapsed(),
            rf_hz,
        };
        self.push_break(b);
        let t = r?;
        resumed?;
        Ok((t, b))
    }

    async fn finish(mut self) -> Result<Finished> {
        let mut tail = Vec::new();
        if self.started_at.is_some() {
            self.quiesce().await?;
            let mut buf = vec![0u8; 2 * self.stream.max_num_samps()];
            loop {
                let (n, _) = self
                    .stream
                    .recv_bytes(&mut buf, TAIL_WAIT_S)
                    .await
                    .map_err(at("finish: the tail"))?;
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
            }
            // The stop's writes with nothing flowing, then the counter for
            // whatever the hold missed (none expected), then the register
            // back to its power-on state.
            self.stream.stop().await.map_err(at("finish: stop"))?;
            let d = self
                .stream
                .take_dropped_blocks()
                .await
                .map_err(at("finish: the counter"))?;
            self.count(d);
            if self.quiesce_with_pause {
                self.stream
                    .resume_blind()
                    .await
                    .map_err(at("finish: the pause bit back"))?;
            }
        }
        // Decimating: the tail through the decimator too, so the samples
        // handed out go on from the last read without a gap (read as wire
        // bytes, the tail is the chip's as it is).
        let raw = self.handout == Handout::WireBytes;
        let decimated_tail = self.decim.as_mut().filter(|_| !raw).map(|d| {
            let mut out = Vec::new();
            d.process(&widen(&tail), &mut out);
            out
        });
        let samples = self.delivered();
        let host_lost_samples = self.stream.host_lost_samples();
        let clock_lost_samples = self.stream.clock_lost_samples();
        let stall_retries = self.rtl.link_ref().stall_retries() - self.stalls_at_open;
        let Core {
            rtl,
            stream,
            breaks,
            breaks_total,
            dropped_blocks,
            saturated,
            unknown,
            ..
        } = self;
        drop(stream);
        Ok(Finished {
            rtl,
            report: StreamReport {
                samples,
                dropped_blocks,
                saturated,
                breaks,
                breaks_total,
                host_lost_samples,
                clock_lost_samples,
                stall_retries,
                configuration_unknown: unknown,
            },
            tail,
            decimated_tail,
        })
    }
}

/// What a stream's handles share.
struct Shared {
    core: Lock<Option<Core>>,
    snapshot: Mutex<Snapshot>,
    /// The receiver's request layer, for its stall count.
    link: crate::usb::Link,
    /// A change waits for the stream: the reading half lets it in before
    /// its next round.
    control_waiting: AtomicBool,
}

impl Shared {
    fn snap(&self) -> MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, core: &Core) {
        *self.snap() = core.snapshot();
    }

    /// Run `f` on the stream, ahead of the reading half.
    async fn change<T>(&self, f: impl AsyncFnOnce(&mut Core) -> Result<T>) -> Result<T> {
        self.control_waiting.store(true, Ordering::Release);
        let mut g = self.core.lock().await;
        self.control_waiting.store(false, Ordering::Release);
        let core = g.as_mut().ok_or_else(finished_error)?;
        let r = f(core).await;
        self.record(core);
        r
    }

    /// Receive for at most `timeout_secs`, in rounds of [`RECV_SLICE`] with
    /// the stream let go between them, so a change waits at most one round.
    /// `done` says whether a round's result ends the wait.
    async fn recv<T>(
        &self,
        timeout_secs: f64,
        mut round: impl AsyncFnMut(&mut Core, f64) -> Result<T>,
        done: impl Fn(&T) -> bool,
    ) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.0));
        loop {
            while self.control_waiting.load(Ordering::Acquire) {
                yield_now().await;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let r = {
                let mut g = self.core.lock().await;
                let core = g.as_mut().ok_or_else(finished_error)?;
                core.start().await?;
                let r = round(core, left.min(RECV_SLICE).as_secs_f64()).await?;
                {
                    // The count, and what the first read decided (a
                    // decimating stream read as wire bytes reports the
                    // chip's rate from then on).
                    let mut snap = self.snap();
                    snap.delivered = core.delivered();
                    snap.rate = core.handed_out_rate();
                    snap.decimation = core.decimating().map_or(1, Decimator::factor);
                }
                if done(&r) {
                    core.last_data = Some(Instant::now());
                } else if core.last_data.is_some_and(|t| t.elapsed() > SILENCE_LIMIT) {
                    return Err(silence_error());
                }
                r
            };
            if done(&r) || Instant::now() >= deadline {
                return Ok(r);
            }
        }
    }

    async fn recv_into(&self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
        self.recv(
            timeout_secs,
            async |c, t| c.recv_into(buf, t).await,
            |i| !i.timed_out,
        )
        .await
    }

    /// At least one byte, or 0 once the stream has finished.
    async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.snap().finished {
                return Ok(0);
            }
            let (n, _) = self
                .recv(
                    READ_ROUND_S,
                    async |c, t| c.recv_bytes(buf, t).await,
                    |(n, _)| *n > 0,
                )
                .await?;
            if n > 0 {
                return Ok(n);
            }
        }
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

/// The changing half of a [`Stream`] ([`Stream::split`]). Cheap to clone;
/// every clone is the same stream's.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

/// What every handle reports, at once (no request, no wait).
macro_rules! reports {
    ($t:ident) => {
        impl $t {
            /// The rate the samples are handed out at: the rate set, or —
            /// for a decimating stream read as wire bytes — the chip's
            /// ([`Self::chip_rate`]).
            pub fn rate(&self) -> f64 {
                self.shared.snap().rate
            }

            /// The rate the chip delivers: the rate set × the decimation.
            pub fn chip_rate(&self) -> f64 {
                self.shared.snap().chip_rate
            }

            /// The decimation applied to what is handed out (1: none, or
            /// read as wire bytes).
            pub fn decimation(&self) -> u32 {
                self.shared.snap().decimation
            }

            /// The RF the samples are centred on, from the words written.
            /// Errs once a failed change could not be undone (the
            /// configuration is then not known).
            pub fn freq(&self) -> Result<f64> {
                self.shared.snap().freq.ok_or_else(unknown_error)
            }

            /// The sum of the gain stages' dB, as last written.
            pub fn gain(&self) -> f64 {
                self.shared.snap().gain
            }

            /// Each gain stage as last written.
            pub fn stage_readings(&self) -> Vec<StageReading<Stage>> {
                self.shared.snap().stages.clone()
            }

            /// The receiver's label (product and serial, from the USB
            /// descriptors read at open).
            pub fn label(&self) -> String {
                self.shared.snap().label.clone()
            }

            /// Samples handed out so far.
            pub fn delivered(&self) -> u64 {
                self.shared.snap().delivered
            }

            /// The breaks made so far, the last [`BREAKS_KEPT`].
            pub fn breaks(&self) -> Vec<Break> {
                self.shared.snap().breaks.clone()
            }

            /// Every break made so far, counted.
            pub fn breaks_total(&self) -> u64 {
                self.shared.snap().breaks_total
            }

            /// Requests the chip refused and that were tried again since
            /// the receiver was opened (findings 16.6, 16.7).
            pub fn stall_retries(&self) -> u64 {
                self.shared.link.stall_retries()
            }

            /// Samples one receive hands out at most.
            pub fn max_num_samps(&self) -> usize {
                self.shared.snap().max_num_samps
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
            pub async fn reconfigure<T>(
                &self,
                f: impl AsyncFnOnce(&mut Rtl) -> Result<T>,
            ) -> Result<(T, Break)> {
                self.shared.change(async |c| c.reconfigure(f).await).await
            }

            /// [`Rtl::set_freq`] inside a pause.
            pub async fn retune(&self, req: &TuneRequest) -> Result<(TuneResult, Break)> {
                self.reconfigure(async |rtl| rtl.set_freq(req).await).await
            }

            /// [`Rtl::set_gain`] inside a pause.
            pub async fn set_gain(&self, db: f64) -> Result<Break> {
                self.reconfigure(async |rtl| rtl.set_gain(db).await)
                    .await
                    .map(|(_, b)| b)
            }

            /// [`Rtl::set_stage_level`] inside a pause.
            pub async fn set_stage_level(&self, stage: Stage, level: usize) -> Result<Break> {
                self.reconfigure(async |rtl| rtl.set_stage_level(stage, level).await)
                    .await
                    .map(|(_, b)| b)
            }

            /// [`Rtl::set_ppm`] inside a pause: the RF tuned again under
            /// the correction; the rate keeps its ratio.
            pub async fn set_ppm(&self, ppm: f64) -> Result<Break> {
                self.reconfigure(async |rtl| rtl.set_ppm(ppm).await)
                    .await
                    .map(|(_, b)| b)
            }

            /// [`Rtl::set_bandwidth`] inside a pause.
            pub async fn set_bandwidth(&self, hz: f64) -> Result<Break> {
                self.reconfigure(async |rtl| rtl.set_bandwidth(hz).await)
                    .await
                    .map(|(_, b)| b)
            }
        }
    };
}

/// What reads the samples.
macro_rules! reads {
    ($t:ident) => {
        impl $t {
            /// The wire bytes into `buf`: unsigned 8-bit I then Q, as the
            /// chip sends them. Waits for at least one byte; 0 once the
            /// stream has finished. Starts the stream on the first read.
            pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
                self.shared.read(buf).await
            }

            /// Samples into `buf`, widened to [`Iq`] (full scale ±32768);
            /// at most `timeout_secs` waiting (`timed_out` then set).
            /// Starts the stream on the first call.
            pub async fn recv_into(
                &mut self,
                buf: &mut [Iq],
                timeout_secs: f64,
            ) -> Result<RecvInfo> {
                self.shared.recv_into(buf, timeout_secs).await
            }

            /// Start the stream now rather than at the first read.
            pub async fn start(&mut self) -> Result<()> {
                self.shared.change(async |c| c.start().await).await
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
    pub(crate) async fn new(
        mut rtl: Rtl,
        transfer_bytes: usize,
        queue_depth: usize,
    ) -> core::result::Result<Stream, StreamError> {
        let stalls_at_open = rtl.link_ref().stall_retries();
        let decimation = rtl.decimation();
        match rtl
            .open_stream(transfer_bytes, queue_depth.max(MIN_QUEUE_DEPTH))
            .await
        {
            Ok(stream) => {
                let link = rtl.link_ref().clone();
                let core = Core {
                    rtl,
                    stream,
                    started_at: None,
                    breaks: Vec::new(),
                    breaks_total: 0,
                    last_data: None,
                    broken: None,
                    dropped_blocks: 0,
                    saturated: false,
                    drain: DRAIN,
                    fifo_reset: true,
                    quiesce_with_pause: false,
                    unknown: false,
                    stalls_at_open,
                    decim: (decimation > 1).then(|| Decimator::new(decimation)),
                    scratch: Vec::new(),
                    decimated: Vec::new(),
                    handout: Handout::Undecided,
                };
                let snapshot = Mutex::new(core.snapshot());
                Ok(Stream {
                    shared: Arc::new(Shared {
                        core: Lock::new(Some(core)),
                        snapshot,
                        link,
                        control_waiting: AtomicBool::new(false),
                    }),
                })
            }
            Err(error) => Err(StreamError {
                rtl: Box::new(rtl),
                error,
            }),
        }
    }

    /// The two halves, each usable on its own thread or task.
    pub fn split(self) -> (ReadHalf, Control) {
        (
            ReadHalf {
                shared: Arc::clone(&self.shared),
            },
            Control {
                shared: self.shared,
            },
        )
    }

    /// The halves of one stream joined again; handed back if they are not
    /// the same stream's, or a clone of the control still exists.
    pub fn reunite(
        read: ReadHalf,
        control: Control,
    ) -> core::result::Result<Stream, (ReadHalf, Control)> {
        if !Arc::ptr_eq(&read.shared, &control.shared) || Arc::strong_count(&read.shared) != 2 {
            return Err((read, control));
        }
        drop(read);
        Ok(Stream {
            shared: control.shared,
        })
    }

    /// Pause, hand out the samples still on their way, stop with nothing
    /// flowing, and read the dropped-block counter then: the receiver back,
    /// the report, the tail.
    pub async fn finish(self) -> Result<Finished> {
        let core = self
            .shared
            .core
            .lock()
            .await
            .take()
            .ok_or_else(finished_error)?;
        self.shared.snap().finished = true;
        core.finish().await
    }

    /// The wait between the pause write and the hold, for bench programs.
    #[cfg(feature = "bench")]
    pub async fn set_drain(&self, d: Duration) {
        let _ = self
            .shared
            .change(async |c| {
                c.drain = d;
                Ok(())
            })
            .await;
    }

    /// Discard the FIFO's held blocks at each break (the default), for
    /// bench programs.
    #[cfg(feature = "bench")]
    pub async fn set_fifo_reset(&self, on: bool) {
        let _ = self
            .shared
            .change(async |c| {
                c.fifo_reset = on;
                Ok(())
            })
            .await;
    }

    /// Quiesce changes with the pause bit (page 1 0x01 bit 2) instead of
    /// the start bit, for bench programs comparing the two (findings 19).
    #[cfg(feature = "bench")]
    pub async fn set_quiesce_with_pause(&self, on: bool) {
        let _ = self
            .shared
            .change(async |c| {
                c.quiesce_with_pause = on;
                Ok(())
            })
            .await;
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

    #[test]
    fn wire_bytes_widen_as_the_engine_does() {
        assert_eq!(
            widen(&[128, 128, 255, 0, 7]),
            vec![
                Iq { i: 0, q: 0 },
                Iq {
                    i: 127 << 8,
                    q: -128 << 8
                }
            ]
        );
    }
}

/// The stream against the simulated dongle (`fake`): order, breaks, and
/// every step of a change failing (review A1).
#[cfg(all(test, not(target_arch = "wasm32")))]
mod fake_tests {
    use std::sync::mpsc;

    use nusb::transfer::TransferError;
    use sdr_core::block_on;

    use super::*;
    use crate::fake::{self, FakeRtl, Request, Target};
    use crate::protocol::{BLK_DROP_COUNTER, SYSCTL_FULL_PACKET, USB_EPA_CTL, USB_SYSCTL};

    /// A scenario that has not finished by then has hung.
    const HANG: Duration = Duration::from_secs(15);

    fn open(fake: &Arc<FakeRtl>) -> Stream {
        block_on(async {
            let rtl = Rtl::open_transport(fake.clone(), fake::label())
                .await
                .expect("open");
            rtl.into_stream(0, 0).await.expect("stream")
        })
    }

    /// Run `f` on its own thread; a panic or a hang fails the test.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(HANG)
            .expect("the scenario hung (or panicked)")
    }

    /// Bytes read until `n` have come; the read's error, if any.
    fn read_n(s: &mut Stream, n: usize, out: &mut Vec<u8>) -> Result<()> {
        let mut buf = [0u8; 10_000];
        let goal = out.len() + n;
        while out.len() < goal {
            let k = block_on(s.read(&mut buf))?;
            out.extend_from_slice(&buf[..k]);
        }
        Ok(())
    }

    /// The bytes are samples 0, 1, 2, … with nothing missing or swapped.
    fn in_order(bytes: &[u8]) -> bool {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .all(|(k, p)| *p == fake::sample_bytes(k as u64))
    }

    #[test]
    fn opens_and_streams_in_order_on_the_fake() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 400_000, &mut got).unwrap();
            assert!(in_order(&got));
            assert!(fake.flowing());
        });
    }

    /// A dongle Linux's kernel driver let go of: USB_EPA_CTL holds the
    /// FIFO reset and the stall (findings 21); the open's FIFO reset
    /// releases both.
    #[test]
    fn streams_from_a_dongle_the_kernel_driver_left_stalled() {
        within(|| {
            let fake = FakeRtl::new();
            fake.set_epa_ctl(0x0210);
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 400_000, &mut got).unwrap();
            assert!(in_order(&got));
        });
    }

    /// A dongle that refuses a demodulator write with no read in the last
    /// 300 ms (findings 21): after 400 ms of reading with no control
    /// request, a retune and the finish go through, none refused.
    #[test]
    fn changes_after_an_idle_stretch_are_not_refused() {
        within(|| {
            let fake = FakeRtl::new();
            fake.set_demod_write_needs_read(Duration::from_millis(300));
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 800_000, &mut got).unwrap();
            block_on(s.retune(&TuneRequest::auto(450e6))).expect("retune");
            read_n(&mut s, 800_000, &mut got).unwrap();
            block_on(s.finish()).expect("finish");
            assert!(in_order(&got));
            assert!(
                !fake.requests().iter().any(|r| r.failed),
                "no request refused"
            );
        });
    }

    #[test]
    fn a_retune_breaks_once_and_the_stream_goes_on_in_order() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 100_000, &mut got).unwrap();
            let (_, b) = block_on(s.retune(&TuneRequest::auto(450e6))).expect("retune");
            read_n(&mut s, 100_000, &mut got).unwrap();
            assert!(in_order(&got), "order across the break");
            assert_eq!(s.breaks(), vec![b]);
            assert!(fake.flowing());
            assert!(
                fake.requests().iter().any(Request::is_halt),
                "a change quiesces with the start bit"
            );
            assert!(
                !fake.requests().iter().any(Request::is_pause),
                "never the pause bit (findings 19)"
            );
        });
    }

    /// A step of a change: its name, and the request it makes.
    type Step = (&'static str, fn(&Request) -> bool);

    /// The steps of a change, by the request each makes.
    fn steps() -> Vec<Step> {
        vec![
            ("the halt write", |r| r.is_halt()),
            ("packing off", |r| {
                r.set && r.target == Target::Usb(USB_SYSCTL) && r.data[0] & SYSCTL_FULL_PACKET == 0
            }),
            ("the FIFO reset", |r| {
                r.set && r.target == Target::Usb(USB_EPA_CTL)
            }),
            ("the counter read", |r| {
                !r.set && r.target == Target::Usb(BLK_DROP_COUNTER)
            }),
            ("the change", |r| matches!(r.target, Target::TunerWrite(_))),
            ("packing on", |r| {
                r.set && r.target == Target::Usb(USB_SYSCTL) && r.data[0] & SYSCTL_FULL_PACKET != 0
            }),
            ("the restart write", |r| r.is_restart()),
        ]
    }

    /// Review A1: any step of a change failing — once, or for good —
    /// never leaves the stream silent. A step that fails once: the change
    /// errs or not, and the stream goes on, in order. A step that fails
    /// for good: the stream goes on (the resume was written), or every
    /// later read fails at once (it could not be).
    #[test]
    fn a_failed_step_never_leaves_the_stream_silent() {
        for (name, step) in steps() {
            for persistent in [false, true] {
                let outcome = within(move || {
                    let fake = FakeRtl::new();
                    let mut s = open(&fake);
                    let mut got = Vec::new();
                    read_n(&mut s, 50_000, &mut got).unwrap();
                    fake.fail(
                        step,
                        TransferError::Fault,
                        0,
                        if persistent { usize::MAX } else { 1 },
                    );
                    let change = block_on(s.retune(&TuneRequest::auto(450e6)));
                    let t = Instant::now();
                    let after = read_n(&mut s, 50_000, &mut got);
                    (
                        change.is_ok(),
                        after.map(|()| in_order(&got)).map_err(|e| e.to_string()),
                        t.elapsed(),
                        fake.flowing(),
                    )
                });
                let (changed, after, took, flowing) = outcome;
                let what = format!(
                    "{name} failing {}",
                    if persistent { "for good" } else { "once" }
                );
                match after {
                    Ok(ordered) => {
                        assert!(ordered, "{what}: the stream went on out of order");
                        assert!(flowing, "{what}: reads came but the fake is not flowing");
                    }
                    Err(e) => {
                        assert!(
                            persistent,
                            "{what}: a step failing once stopped the stream: {e}"
                        );
                        assert!(
                            took <= SILENCE_LIMIT + Duration::from_millis(500),
                            "{what}: the error took {took:?}"
                        );
                    }
                }
                if !persistent && name != "the change" {
                    // The change itself never ran or ran whole: only a
                    // failed change step (or a resume trouble) can err it.
                    let _ = changed;
                }
            }
        }
    }

    /// The resume write failing for good: the change errs, and so does
    /// every read after it, at once — not a stream that waits for ever.
    #[test]
    fn a_resume_that_cannot_be_written_breaks_the_stream() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 50_000, &mut got).unwrap();
            fake.fail(|r| r.is_restart(), TransferError::Fault, 0, usize::MAX);
            assert!(block_on(s.retune(&TuneRequest::auto(450e6))).is_err());
            let t = Instant::now();
            let e = read_n(&mut s, 1, &mut got).expect_err("reads fail");
            assert!(
                t.elapsed() < Duration::from_millis(100),
                "at once, not after a wait"
            );
            assert!(e.to_string().contains("could not be resumed"), "{e}");
            assert!(
                block_on(s.retune(&TuneRequest::auto(460e6))).is_err(),
                "and changes"
            );
        });
    }

    /// Review A5: a retune whose synthesizer reports no lock (R2 outside
    /// the locked span) is refused; the stream writes the last good tuning
    /// back, keeps its frequency and flows on; once R2 reads locked again a
    /// retune takes.
    #[test]
    fn an_unlocked_retune_is_refused_and_the_stream_goes_on() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 50_000, &mut got).unwrap();
            let before = s.freq().unwrap();
            fake.set_vco_indicator(10);
            let e = block_on(s.retune(&TuneRequest::auto(450e6))).expect_err("unlocked");
            assert!(e.to_string().contains("no lock"), "{e}");
            assert_eq!(s.freq().unwrap(), before, "the last good tuning kept");
            read_n(&mut s, 50_000, &mut got).unwrap();
            assert!(in_order(&got));
            fake.set_vco_indicator(fake::FAKE_VCO_LOCKED);
            let (t, _) = block_on(s.retune(&TuneRequest::auto(450e6))).expect("locked");
            assert!((t.actual_rf_freq - 450e6).abs() < 1e3);
        });
    }

    /// No automatic gain by default: the open leaves the demodulator's
    /// digital AGC and both analog loops off (findings 21), and the stream
    /// starting does not turn them on.
    #[test]
    fn the_open_leaves_every_agc_off() {
        use crate::protocol::{
            AAGC_OFF, AAGC_PAGE, DAGC_OFF, DAGC_PAGE, EN_DAGC, EN_IF_AGC, EN_RF_AGC,
        };
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 50_000, &mut got).unwrap();
            assert_eq!(fake.demod(DAGC_PAGE, DAGC_OFF) & EN_DAGC, 0, "digital AGC");
            assert_eq!(
                fake.demod(AAGC_PAGE, AAGC_OFF) & (EN_IF_AGC | EN_RF_AGC),
                0,
                "analog IF and RF loops"
            );
        });
    }

    /// A unit whose VCO does not start where the documented range does
    /// (findings 21): 852.30625 MHz takes /2 first (VCO 1.71 GHz); its R2
    /// says unlocked, so the tuning falls back to /4 and goes through.
    #[test]
    fn a_divider_that_does_not_lock_falls_back_to_the_next() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 50_000, &mut got).unwrap();
            let writes_before = fake.requests().len();
            fake.queue_vco_indicators(&[0]);
            let (t, _) = block_on(s.retune(&TuneRequest::auto(852.30625e6)))
                .expect("the next divider locks");
            assert!((t.actual_rf_freq - 852.30625e6).abs() < 1e3);
            let r16_writes = fake.requests()[writes_before..]
                .iter()
                .filter(|r| r.target == Target::TunerWrite(crate::image::R16))
                .count();
            assert_eq!(r16_writes, 2, "the synthesizer written twice: /2, then /4");
            read_n(&mut s, 50_000, &mut got).unwrap();
            assert!(in_order(&got));
        });
    }

    /// The image-rejection calibration: each tuning writes R8 / R9 with the
    /// nearest point's IMR_G / IMR_P (and only when they change); the
    /// status reports them; no table, 32/32.
    #[test]
    fn each_tuning_writes_the_dongles_imr_setting() {
        use crate::image::{ImrPoint, PWD_AMP, R8, R9};
        within(|| {
            let fake = FakeRtl::new();
            block_on(async {
                let mut rtl = Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .expect("open");
                let last = |reg: u8| {
                    fake.requests()
                        .iter()
                        .rev()
                        .find(|r| r.target == Target::TunerWrite(reg))
                        .and_then(|r| r.data.last().copied())
                };
                rtl.set_freq(&TuneRequest::auto(851e6)).await.unwrap();
                assert_eq!(
                    (
                        rtl.tuner_status().await.unwrap().imr_g,
                        rtl.tuner_status().await.unwrap().imr_p
                    ),
                    (32, 32)
                );
                rtl.set_imr_table(vec![
                    ImrPoint {
                        rf_hz: 420e6,
                        g: 33,
                        p: 0,
                    },
                    ImrPoint {
                        rf_hz: 860e6,
                        g: 7,
                        p: 33,
                    },
                ])
                .await
                .unwrap();
                assert_eq!(
                    last(R8),
                    Some(PWD_AMP | 7),
                    "applied at once to the RF in use"
                );
                assert_eq!(last(R9), Some(33));
                rtl.set_freq(&TuneRequest::auto(460e6)).await.unwrap();
                assert_eq!((last(R8), last(R9)), (Some(PWD_AMP | 33), Some(0)));
                let st = rtl.tuner_status().await.unwrap();
                assert_eq!((st.imr_g, st.imr_p), (33, 0));
                let writes = fake
                    .requests()
                    .iter()
                    .filter(|r| r.target == Target::TunerWrite(R8))
                    .count();
                rtl.set_freq(&TuneRequest::auto(470e6)).await.unwrap();
                assert_eq!(
                    fake.requests()
                        .iter()
                        .filter(|r| r.target == Target::TunerWrite(R8))
                        .count(),
                    writes,
                    "unchanged: not written again"
                );
                assert!(
                    rtl.set_imr_table(vec![ImrPoint {
                        rf_hz: 470e6,
                        g: 64,
                        p: 0
                    }])
                    .await
                    .is_err()
                );
            });
        });
    }

    /// The range's ends (findings 26): 42 and 1700 MHz tune, 1090 among
    /// them; just outside either is refused with the range named.
    #[test]
    fn the_range_reaches_1700_mhz() {
        within(|| {
            let fake = FakeRtl::new();
            block_on(async {
                let mut rtl = Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .expect("open");
                for rf in [42e6, 1090e6, 1700e6] {
                    let t = rtl
                        .set_freq(&TuneRequest::auto(rf))
                        .await
                        .unwrap_or_else(|e| panic!("{rf}: {e}"));
                    assert!(
                        (t.actual_rf_freq - rf).abs() <= crate::pll::sdm_step_hz(2),
                        "{rf}: {}",
                        t.actual_rf_freq
                    );
                }
                for rf in [41.9e6, 1700.1e6] {
                    let e = rtl
                        .set_freq(&TuneRequest::auto(rf))
                        .await
                        .expect_err("outside");
                    assert!(e.to_string().contains("42-1700 MHz"), "{e}");
                }
            });
        });
    }

    /// The crystal correction: idle, the RF in use is kept (tuned again
    /// for RF / scale) and the rate set again for rate / scale; the RF and
    /// rates reported are the true ones. While streaming the RF is tuned
    /// again and the rate keeps its ratio, reported under the correction.
    /// Beyond the limit, refused with nothing changed.
    #[test]
    fn the_crystal_correction_scales_the_tuning_and_the_rate() {
        within(|| {
            let fake = FakeRtl::new();
            block_on(async {
                let mut rtl = Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .expect("open");
                rtl.set_rate(2.4e6).await.unwrap();
                rtl.set_freq(&TuneRequest::auto(1090e6)).await.unwrap();
                let lo0 = rtl.tuner_status().await.unwrap().pll.lo_hz;
                rtl.set_ppm(0.97).await.expect("0.97 ppm");
                let scale = 1.0 + 0.97e-6;
                // The words now put the nominal LO 0.97 ppm under (1061 Hz at
                // this LO), to the SDM step.
                let st = rtl.tuner_status().await.unwrap();
                assert!(
                    (st.pll.lo_hz * scale - lo0).abs() <= crate::pll::sdm_step_hz(2),
                    "{}",
                    st.pll.lo_hz * scale - lo0
                );
                assert!(lo0 - st.pll.lo_hz > 800.0, "moved: {}", lo0 - st.pll.lo_hz);
                assert!(
                    (rtl.freq().unwrap() - 1090e6).abs()
                        <= crate::pll::sdm_step_hz(2) / 2.0 * scale,
                    "{}",
                    rtl.freq().unwrap()
                );
                assert_eq!(st.ppm, 0.97);
                assert!((st.xtal_hz - 28.8e6 * scale).abs() < 1e-6);
                // The rate: the register read back under the correction is the
                // rate asked for, in true time.
                assert!(
                    (rtl.rate_set() - 2.4e6).abs() < 2.4e6 * 0.12e-6,
                    "{}",
                    rtl.rate_set()
                );
                assert!((rtl.rate().await.unwrap() - 2.4e6).abs() < 2.4e6 * 0.12e-6);
                assert!(rtl.set_ppm(crate::PPM_LIMIT * 1.5).await.is_err());
                assert_eq!(rtl.ppm(), 0.97, "a refusal changes nothing");
                let nominal = rtl.rate_set() / scale;
                let s = rtl.into_stream(0, 0).await.expect("stream");
                s.reconfigure(async |r| {
                    r.set_ppm(-2.0).await?;
                    // The ratio kept, reported under the new correction.
                    assert!(
                        (r.rate_set() - nominal * (1.0 - 2e-6)).abs() < 1e-6,
                        "{}",
                        r.rate_set()
                    );
                    assert!(
                        (r.freq().unwrap() - 1090e6).abs() <= crate::pll::sdm_step_hz(2),
                        "{}",
                        r.freq().unwrap()
                    );
                    Ok(())
                })
                .await
                .expect("live");
                s.finish().await.expect("finish");
            });
        });
    }

    /// A dongle that stops sending while running is an error within the
    /// silence limit, not a read that waits for ever.
    #[test]
    fn a_dongle_that_goes_quiet_is_an_error() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = open(&fake);
            let mut got = Vec::new();
            read_n(&mut s, 50_000, &mut got).unwrap();
            // Paused behind the driver's back: nothing more comes.
            block_on(async {
                crate::Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .map(|_| ())
            })
            .ok();
            let t = Instant::now();
            let e = read_n(&mut s, usize::MAX / 4, &mut got).expect_err("silence");
            assert!(e.to_string().contains("no samples"), "{e}");
            assert!(t.elapsed() <= SILENCE_LIMIT + Duration::from_millis(500));
        });
    }

    /// A rate the chip does not reach (0.5 Msps: the chip at 1.0, ÷2,
    /// findings 25): the stream reports the rate set, hands out half the
    /// chip's samples, refuses the wire bytes, and counts a break's place
    /// in the samples it hands out.
    #[test]
    fn a_decimated_rate_hands_out_its_own_samples() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = block_on(async {
                let mut rtl = Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .expect("open");
                rtl.set_rate(0.5e6).await.expect("0.5 Msps");
                assert_eq!(rtl.decimation(), 2);
                assert!((rtl.rate_set() - 0.5e6).abs() < 1.0);
                rtl.into_stream(0, 0).await.expect("stream")
            });
            assert!(
                (s.rate() - 0.5e6).abs() < 1.0,
                "the stream reports {}",
                s.rate()
            );
            let mut buf = vec![Iq::default(); s.max_num_samps()];
            let mut got = 0u64;
            while got < 200_000 {
                got += block_on(s.recv_into(&mut buf, 1.0))
                    .expect("recv")
                    .n_samples as u64;
            }
            assert_eq!(s.delivered(), got, "every sample handed out counted");
            assert_eq!(
                (s.decimation(), (s.chip_rate() - 1.0e6).abs() < 1.0),
                (2, true)
            );
            let e = block_on(s.read(&mut [0u8; 64]))
                .expect_err("read as samples: the wire bytes are another stream's");
            assert!(e.to_string().contains("decimated by 2"), "{e}");
            let (_, b) = block_on(s.retune(&TuneRequest::auto(450e6))).expect("retune");
            assert!(
                b.at_sample >= got,
                "the break is after what was handed out: {} < {got}",
                b.at_sample
            );
            assert!(b.settled_at > b.at_sample);
            assert!(
                b.settled_at - b.at_sample
                    <= (BREAK_SETTLE_SAMPLES + crate::decim::design(2).len() as u64).div_ceil(2)
            );
            let more = block_on(s.recv_into(&mut buf, 1.0))
                .expect("recv after")
                .n_samples;
            assert!(more > 0);
            let f = block_on(s.finish()).expect("finish");
            // Every sample handed out, the decimated tail included.
            assert_eq!(
                f.report.samples,
                got + more as u64 + f.tail_samples().len() as u64
            );
            // The tail is decimated too (the chip's tail / 2, give or take
            // the decimator's phase), here and through `blocking`.
            let chip_tail = f.tail.len() as u64 / 2;
            assert!(
                chip_tail == 0 || (f.tail_samples().len() as u64).abs_diff(chip_tail / 2) <= 1,
                "tail {} for {chip_tail}",
                f.tail_samples().len()
            );
            let tail = f.tail_samples();
            let blocking = crate::blocking::Finished::from(f);
            assert_eq!(
                blocking.tail_samples(),
                tail,
                "the blocking wrapper hands out the same tail"
            );
        });
    }

    /// The same rate read as wire bytes first: the chip's samples, in
    /// order, at the chip's rate, which the stream then reports; counts
    /// and breaks are the chip's; `recv_into` is refused with the reason.
    #[test]
    fn a_decimated_rate_read_as_wire_bytes_is_the_chips_stream() {
        within(|| {
            let fake = FakeRtl::new();
            let mut s = block_on(async {
                let mut rtl = Rtl::open_transport(fake.clone(), fake::label())
                    .await
                    .expect("open");
                rtl.set_rate(0.5e6).await.expect("0.5 Msps");
                rtl.into_stream(0, 0).await.expect("stream")
            });
            let mut got = Vec::new();
            read_n(&mut s, 200_000, &mut got).unwrap();
            assert!(in_order(&got), "the chip's samples, as they come");
            assert!(
                (s.rate() - 1.0e6).abs() < 1.0,
                "read as wire bytes, the stream reports the chip's rate: {}",
                s.rate()
            );
            assert_eq!(s.decimation(), 1);
            assert_eq!(s.delivered(), got.len() as u64 / 2);
            let e = block_on(s.recv_into(&mut [Iq::default(); 64], 1.0))
                .expect_err("read as wire bytes");
            assert!(e.to_string().contains("read as wire bytes"), "{e}");
            let (_, b) = block_on(s.retune(&TuneRequest::auto(450e6))).expect("retune");
            assert!(
                b.at_sample >= got.len() as u64 / 2
                    && b.settled_at == b.at_sample + BREAK_SETTLE_SAMPLES,
                "{b:?}"
            );
            read_n(&mut s, 100_000, &mut got).unwrap();
            assert!(in_order(&got), "in order across the break");
            let f = block_on(s.finish()).expect("finish");
            assert_eq!(
                f.tail_samples().len(),
                f.tail.len() / 2,
                "the tail is the chip's too"
            );
        });
    }

    /// Review A7: the kept breaks stop at BREAKS_KEPT; every one is counted.
    #[test]
    fn breaks_are_kept_to_a_bound_and_all_counted() {
        within(|| {
            let fake = FakeRtl::new();
            let s = open(&fake);
            // Before the start a change makes no break; started, each does.
            let mut s2 = s;
            let mut got = Vec::new();
            read_n(&mut s2, 10_000, &mut got).unwrap();
            block_on(async {
                let mut core = s2.shared.core.lock().await;
                let core = core.as_mut().unwrap();
                for k in 0..(BREAKS_KEPT + 10) {
                    core.push_break(Break {
                        at_sample: k as u64,
                        settled_at: 0,
                        dropped_before: DroppedBlocks {
                            blocks: 0,
                            saturated: false,
                        },
                        quiet: Duration::ZERO,
                        rf_hz: 0.0,
                    });
                }
                assert_eq!(core.breaks.len(), BREAKS_KEPT);
                assert_eq!(core.breaks_total, (BREAKS_KEPT + 10) as u64);
                assert_eq!(core.breaks[0].at_sample, 10, "the oldest dropped first");
            });
        });
    }
}
