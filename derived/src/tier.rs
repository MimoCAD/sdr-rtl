//! Streams whose methods cannot lose samples.
//!
//! The RTL2832U puts nothing in its sample stream but the samples: no
//! timestamp, no sequence number, no mark where anything was lost. A
//! sample's place in the stream is its time — exactly, while nothing is
//! lost. What loses samples is the control pipe: every request served
//! while the samples flow stalls the chip's 20-block FIFO (findings 16:
//! ~850-2,100 samples a request at 2.4 Msps; a retune or a gain change
//! loses its whole 20-40 ms), and the only witness, the chip's
//! dropped-block counter, is itself read through that pipe.
//!
//! So the stream owns the receiver, and each kind offers only what it can
//! do without loss at its rate (findings 16.2-16.4, measured on the
//! M5 Max; another host's request latency moves the limits):
//! - [`LosslessStream`], ≤ [`LOSSLESS_MAX_SPS`]: anything the receiver
//!   does ([`LosslessStream::reconfigure`]: retune, gain, …) inside a
//!   pause; nothing is lost, every sample before [`Break::at_sample`] is
//!   the old configuration and every one from [`Break::settled_at`] the new
//!   (between them [`BREAK_SETTLE_SAMPLES`]: the filter pipeline; the FIFO's
//!   held blocks are discarded at the break, findings 16.6),
//!   and at this rate the pause itself loses nothing (100 paused retunes
//!   of 100; 34 more in 16.5 with no phase jump inside a segment).
//!   After any start the first ~5-10 ms carry a small phase wobble at
//!   these rates (~37° steps every 192-256 samples, nothing lost).
//! - [`AccountedStream`], ≤ [`ACCOUNTED_MAX_SPS`], and [`RestrictedStream`],
//!   any rate: receive and finish, nothing else. To retune, finish and make
//!   a new stream (a break by construction).
//!
//! Every kind finishes by pausing with one write, draining, handing the
//! last samples out ([`Finished::tail`]), stopping with nothing flowing and
//! reading the counter then, for free: [`StreamReport::dropped_blocks`] is
//! the exact count of samples lost in the run. To 1.2 Msps it read 0 in
//! every run (findings 16.4). Above it the pause write itself costs a
//! few blocks (15 at 2.4 Msps) in the run's last millisecond, counted.
//! `RestrictedStream` also starts paused and resumes with one write (the
//! plain start dropped ~37 blocks at 2.4 Msps; starting paused, none).
//!
//! Nothing here reads the counter while samples flow.

use std::fmt;
use std::time::{Duration, Instant};

use sdr_core::{Error, Iq, RecvInfo, Result, RxSource, TuneRequest, TuneResult, elevate_current_thread};

use crate::Rtl;
use crate::stream::{DroppedBlocks, RtlStream};

/// The highest output rate a [`LosslessStream`] runs at: a paused retune
/// lost nothing at 1.024 and 1.2 Msps (50 of 50 each) and 3 blocks once in
/// 50 at 1.4 (findings 16.3).
pub const LOSSLESS_MAX_SPS: f64 = 1.2e6;
/// The highest output rate an [`AccountedStream`] runs at: to 1.8 Msps a
/// control request's stall outlasts the 20-block FIFO only rarely (counter
/// reads at 5 ms cost 0.02-0.035 blocks each); at 2.048 it overflows on
/// every request (findings 16.1, 16.3).
pub const ACCOUNTED_MAX_SPS: f64 = 1.8e6;
/// After the pause write, before the hold: the FIFO's last blocks onto
/// the wire (findings 16.4's clean stop waited this long).
pub const DRAIN: Duration = Duration::from_millis(5);
/// How long `finish` waits on each backlog chunk (it is already in hand).
const TAIL_WAIT_S: f64 = 0.0;

/// Which kind of stream a rate allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Lossless,
    Accounted,
    Restricted,
}

impl Tier {
    /// The most capable kind the rate allows.
    pub fn for_rate(sps: f64) -> Tier {
        if sps <= LOSSLESS_MAX_SPS {
            Tier::Lossless
        } else if sps <= ACCOUNTED_MAX_SPS {
            Tier::Accounted
        } else {
            Tier::Restricted
        }
    }

    /// The highest rate the kind allows.
    pub fn max_sps(self) -> f64 {
        match self {
            Tier::Lossless => LOSSLESS_MAX_SPS,
            Tier::Accounted => ACCOUNTED_MAX_SPS,
            Tier::Restricted => f64::INFINITY,
        }
    }
}

/// The samples after a break that still carry the old configuration: the
/// chip's filter pipeline flushing, 14-27 measured (findings 16.6).
pub const BREAK_SETTLE_SAMPLES: u64 = 32;

/// One break a [`LosslessStream`] made on purpose.
///
/// The endpoint FIFO keeps its fill through a pause (~6 blocks at
/// 1.024 Msps to ~10 at 1.2) and would hand it out after the resume, so
/// at each break the FIFO is reset (USB_EPA_CTL bit 9) once everything it
/// delivered is held: those last ~0.5-0.9 ms of the old configuration are
/// **discarded by design** — the chip's counter does not count them — and
/// the old segment ends at `at_sample` (findings 16.5, 16.6). After it,
/// the first [`BREAK_SETTLE_SAMPLES`] still carry the old configuration
/// (the filter pipeline); from `settled_at` on, the new one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Break {
    /// The samples handed out before the pause: everything before sample
    /// `at_sample` (from 0) is the old configuration.
    pub at_sample: u64,
    /// From sample `settled_at` on, the new configuration: `at_sample` +
    /// [`BREAK_SETTLE_SAMPLES`].
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
    pub tier: Tier,
    /// Samples handed out, the tail included.
    pub samples: u64,
    /// Blocks (94 samples each) the chip dropped in the run: at the
    /// breaks, between them and at the finish.
    pub dropped_blocks: u64,
    /// A read of the counter found it stopped at 255: at least this many.
    pub saturated: bool,
    pub breaks: Vec<Break>,
    /// Requests the chip refused and that were tried again, the run's
    /// share (findings 16.6).
    pub stall_retries: u64,
    /// A failed change could not be undone during the run.
    pub configuration_unknown: bool,
}

/// A finished stream: the receiver back, the report, and the samples
/// still on their way when it finished.
pub struct Finished {
    pub rtl: Rtl,
    pub report: StreamReport,
    pub tail: Vec<Iq>,
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

/// What every kind shares: the receiver it owns and the raw stream.
struct Core {
    rtl: Rtl,
    stream: RtlStream,
    tier: Tier,
    started: bool,
    breaks: Vec<Break>,
    dropped_blocks: u64,
    saturated: bool,
    /// The wait after the pause write (`DRAIN`; bench programs may set it).
    drain: Duration,
    /// Quiesce by clearing the start bit (page 0 0x06) instead of the
    /// pause bit (bench programs may set it; findings 16.5).
    quiet_by_stop: bool,
    /// Bench: the FIFO size written while paused, before the hold.
    squeeze: Option<u8>,
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
    fn new(mut rtl: Rtl, tier: Tier, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<Core, StreamError> {
        let rate = rtl.rate_set();
        if rate > tier.max_sps() {
            let error = Error::Unsupported {
                backend: crate::BACKEND,
                what: format!("{tier:?} stream at {:.3} Msps: the kind allows at most {:.3} Msps (findings 16.3)", rate / 1e6, tier.max_sps() / 1e6),
            };
            return Err(StreamError { rtl: Box::new(rtl), error });
        }
        let stalls_at_open = rtl.link_ref().stall_retries();
        match rtl.open_stream(transfer_bytes, queue_depth) {
            Ok(stream) => Ok(Core { rtl, stream, tier, started: false, breaks: Vec::new(), dropped_blocks: 0, saturated: false, drain: DRAIN, quiet_by_stop: false, squeeze: None, fifo_reset: true, unknown: false, stalls_at_open }),
            Err(error) => Err(StreamError { rtl: Box::new(rtl), error }),
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        if self.tier == Tier::Restricted {
            // Start paused, then resume with one write (findings 16.4).
            self.stream.pause_blind().map_err(at("start: pause"))?;
            self.stream.start().map_err(at("start: the sequence"))?;
            self.stream.resume_blind().map_err(at("start: resume"))?;
        } else {
            self.stream.start().map_err(at("start: the sequence"))?;
        }
        self.started = true;
        Ok(())
    }

    fn count(&mut self, d: DroppedBlocks) {
        self.dropped_blocks += d.blocks as u64;
        self.saturated |= d.saturated;
    }

    /// Pause, drain, hold, read the counter (paused: free): the break.
    fn quiesce(&mut self) -> Result<(u64, DroppedBlocks)> {
        if self.quiet_by_stop {
            self.stream.halt_blind().map_err(at("pause: halt"))?;
        } else {
            self.stream.pause_blind().map_err(at("pause: the pause write"))?;
        }
        std::thread::sleep(self.drain);
        // Bench (findings 16.5): squeeze the FIFO while paused, to see
        // whether it hands its held blocks out.
        let restore = match self.squeeze {
            Some(n) => {
                let v = self.rtl.link_ref().usb_read_n(crate::protocol::USB_EPA_FIFO_CFG, 4)?;
                self.rtl.link_ref().usb_write_n(crate::protocol::USB_EPA_FIFO_CFG, &[n, v[1], v[2], v[3]])?;
                std::thread::sleep(self.drain);
                Some(v)
            }
            None => None,
        };
        let pos = self.stream.hold().map_err(at("pause: hold"))?;
        if let Some(v) = restore {
            self.rtl.link_ref().usb_write_n(crate::protocol::USB_EPA_FIFO_CFG, &[v[0], v[1], v[2], v[3]])?;
        }
        if self.fifo_reset {
            // The FIFO's held blocks discarded (findings 16.6).
            self.stream.reset_fifo().map_err(at("pause: FIFO reset"))?;
        }
        let d = self.stream.take_dropped_blocks().map_err(at("pause: the counter"))?;
        self.count(d);
        Ok((pos, d))
    }

    fn reconfigure<T>(&mut self, f: impl FnOnce(&mut Rtl) -> Result<T>) -> Result<(T, Break)> {
        if !self.started {
            // Nothing flows yet: no break.
            let t = f(&mut self.rtl)?;
            let zero = DroppedBlocks { blocks: 0, saturated: false };
            return Ok((t, Break { at_sample: 0, settled_at: 0, dropped_before: zero, quiet: Duration::ZERO, rf_hz: self.rtl.freq().unwrap_or(f64::NAN) }));
        }
        let t0 = Instant::now();
        let (at_sample, dropped_before) = self.quiesce()?;
        let r = f(&mut self.rtl);
        if r.is_err() {
            // The change may have stopped half way (a tuner retune is
            // several writes): the receiver's last good state written whole
            // again, so the hardware is what `Rtl` says it is. If that
            // fails too, the stream no longer knows its configuration.
            if self.rtl.reassert().is_err() {
                self.unknown = true;
            }
        }
        self.stream.release().map_err(at("resume: release"))?;
        if self.quiet_by_stop {
            self.stream.restart_blind().map_err(at("resume: restart"))?;
        } else {
            self.stream.resume_blind().map_err(at("resume: the resume write"))?;
        }
        let rf_hz = if self.unknown { f64::NAN } else { self.rtl.freq().unwrap_or(f64::NAN) };
        let b = Break { at_sample, settled_at: at_sample + BREAK_SETTLE_SAMPLES, dropped_before, quiet: t0.elapsed(), rf_hz };
        self.breaks.push(b);
        r.map(|t| (t, b))
    }

    fn finish(mut self) -> Result<Finished> {
        let mut tail = Vec::new();
        if self.started {
            self.quiesce()?;
            let mut buf = vec![Iq::default(); self.stream.max_num_samps()];
            loop {
                let info = self.stream.recv_into(&mut buf, TAIL_WAIT_S).map_err(at("finish: the tail"))?;
                if info.n_samples == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..info.n_samples]);
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
        let Core { rtl, stream, tier, breaks, dropped_blocks, saturated, unknown, .. } = self;
        drop(stream);
        Ok(Finished { rtl, report: StreamReport { tier, samples, dropped_blocks, saturated, breaks, stall_retries, configuration_unknown: unknown }, tail })
    }
}

macro_rules! shared {
    ($t:ident) => {
        impl $t {
            pub fn tier(&self) -> Tier {
                self.0.tier
            }

            /// The output rate (fixed for the stream's life).
            pub fn rate(&self) -> f64 {
                self.0.stream.rate()
            }

            /// The RF the samples are centred on, from the words written.
            /// Errs once a failed change could not be undone (the
            /// configuration is then not known; see `reconfigure`).
            pub fn freq(&self) -> Result<f64> {
                if self.0.unknown {
                    return Err(Error::Device {
                        backend: crate::BACKEND,
                        context: "freq",
                        message: "a reconfiguration failed and the last good state could not be written back: the receiver's configuration is not known".into(),
                    });
                }
                self.0.rtl.freq()
            }

            /// Requests the chip refused and that were tried again since
            /// the receiver was opened (findings 16.6).
            pub fn stall_retries(&self) -> u64 {
                self.0.rtl.link_ref().stall_retries()
            }

            /// Samples handed out so far.
            pub fn delivered(&self) -> u64 {
                self.0.stream.delivered()
            }

            /// The breaks made so far.
            pub fn breaks(&self) -> &[Break] {
                &self.0.breaks
            }

            /// Pause, hand out the samples still on their way, stop with
            /// nothing flowing, and read the dropped-block counter then: the
            /// receiver back, the report, the tail.
            pub fn finish(self) -> Result<Finished> {
                self.0.finish()
            }
        }

        impl RxSource for $t {
            fn max_num_samps(&self) -> usize {
                self.0.stream.max_num_samps()
            }

            fn start(&mut self) -> Result<()> {
                self.0.start()
            }

            fn recv_into(&mut self, buf: &mut [Iq], timeout_secs: f64) -> Result<RecvInfo> {
                if !self.0.started {
                    self.0.start()?;
                }
                self.0.stream.recv_into(buf, timeout_secs)
            }

            fn setup_worker_thread(&self) {
                elevate_current_thread();
            }
        }
    };
}

/// A stream at ≤ [`LOSSLESS_MAX_SPS`]: the receiver can be reconfigured
/// while it runs, inside a pause that loses nothing at this rate.
pub struct LosslessStream(Core);

/// A stream at ≤ [`ACCOUNTED_MAX_SPS`]: receive and finish.
pub struct AccountedStream(Core);

/// A stream at any rate: receive and finish; it starts paused and resumes
/// with one write.
pub struct RestrictedStream(Core);

shared!(LosslessStream);
shared!(AccountedStream);
shared!(RestrictedStream);

impl LosslessStream {
    pub(crate) fn new(rtl: Rtl, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<Self, StreamError> {
        Core::new(rtl, Tier::Lossless, transfer_bytes, queue_depth).map(LosslessStream)
    }

    /// Run `f` on the receiver inside a pause: the output paused with one
    /// write, the FIFO drained, every sample produced before it held for
    /// the caller, the counter read (free, paused); `f`; then the queue
    /// resubmitted and the output resumed with one write. Returns what `f`
    /// returned and the [`Break`]: the samples after `at_sample` are the
    /// new configuration's. If `f` fails, the stream still resumes and the
    /// break is recorded. Before the first `recv_into` (not started) `f`
    /// runs directly and the break is empty.
    pub fn reconfigure<T>(&mut self, f: impl FnOnce(&mut Rtl) -> Result<T>) -> Result<(T, Break)> {
        self.0.reconfigure(f)
    }

    /// [`Rtl::set_freq`] inside a pause.
    pub fn retune(&mut self, req: &TuneRequest) -> Result<(TuneResult, Break)> {
        self.0.reconfigure(|rtl| rtl.set_freq(req))
    }

    /// The wait between the pause write and the hold, for bench programs.
    #[cfg(feature = "bench")]
    pub fn set_drain(&mut self, d: Duration) {
        self.0.drain = d;
    }

    /// Quiesce by the start bit instead of the pause bit, for bench
    /// programs.
    #[cfg(feature = "bench")]
    pub fn set_quiet_by_stop(&mut self, on: bool) {
        self.0.quiet_by_stop = on;
    }

    /// Squeeze the FIFO to `n` blocks while paused, before the hold, and
    /// restore it after, for bench programs.
    #[cfg(feature = "bench")]
    pub fn set_squeeze(&mut self, n: Option<u8>) {
        self.0.squeeze = n;
    }

    /// Discard the FIFO's held blocks at each break, for bench programs.
    #[cfg(feature = "bench")]
    pub fn set_fifo_reset(&mut self, on: bool) {
        self.0.fifo_reset = on;
    }

    /// [`Rtl::set_gain`] inside a pause.
    pub fn set_gain(&mut self, db: f64) -> Result<Break> {
        self.0.reconfigure(|rtl| rtl.set_gain(db)).map(|(_, b)| b)
    }
}

impl AccountedStream {
    pub(crate) fn new(rtl: Rtl, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<Self, StreamError> {
        Core::new(rtl, Tier::Accounted, transfer_bytes, queue_depth).map(AccountedStream)
    }
}

impl RestrictedStream {
    pub(crate) fn new(rtl: Rtl, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<Self, StreamError> {
        Core::new(rtl, Tier::Restricted, transfer_bytes, queue_depth).map(RestrictedStream)
    }
}

/// The kind a rate allows, made by [`Rtl::into_stream`].
pub enum TieredStream {
    Lossless(LosslessStream),
    Accounted(AccountedStream),
    Restricted(RestrictedStream),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_by_rate() {
        assert_eq!(Tier::for_rate(1.024e6), Tier::Lossless);
        assert_eq!(Tier::for_rate(1.2e6), Tier::Lossless);
        assert_eq!(Tier::for_rate(1.4e6), Tier::Accounted);
        assert_eq!(Tier::for_rate(1.8e6), Tier::Accounted);
        assert_eq!(Tier::for_rate(2.048e6), Tier::Restricted);
        assert_eq!(Tier::for_rate(2.4e6), Tier::Restricted);
        assert!(Tier::Restricted.max_sps().is_infinite());
    }
}
