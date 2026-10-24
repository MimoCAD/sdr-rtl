//! Is a rate lossless over a long run (findings 27)? Every dongle named,
//! at once, each its own `Stream` at the driver's defaults, read without
//! pause for the whole run, one tone from the bench source reaching all
//! of them through the splitter.
//!
//!   soak [--rate SPS] [--minutes M] [--dongles SPEC;SPEC…]
//!        [--rates R;R…] [--source-cmd "<command line>" | --no-source] [--inject] [--stall-at S] [--dupcheck]
//!        [--transfer-bytes N]
//!
//! Built with `--features bench`. A spec is `Rtl::open`'s (`serial=…`,
//! with `,ppm=…` so the tone lands where it belongs). Gains 8/8/2, 300 MHz.
//!
//! Three witnesses of loss, per dongle:
//! - the chip's own count of 188-byte blocks dropped (`StreamReport::
//!   dropped_blocks`, `saturated`) and the host's ledger
//!   (`host_lost_samples`), from the finish, and every `recv_into`'s
//!   `overflow`;
//! - the tone's phase, read in 64-sample windows against the tone: a gap
//!   of n samples steps it by 2π · f · n / rate. The tone is placed so one
//!   block (94 samples) steps it by 0.382 of a turn (2.4 rad), so a gap of
//!   any small number of blocks shows (n · 0.382 is never within 0.05 of
//!   a whole turn for n ≤ 12): steps over `JUMP_RAD` are counted;
//! - the samples handed out against the elapsed time × the rate.
//!
//! One JSON line per dongle every minute (the running counts), and one at
//! the end with the report.

mod bench;

use std::f64::consts::TAU;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, TuneRequest};
use sdr_rtl::blocking::Rtl;
use sdr_rtl::{OPEN_RF_HZ, Stage};

const WIN: usize = 64;
/// A window-to-window phase step past the run's own drift this large is
/// a jump (the tone is ~60 dB over the floor: a window's phase noise is
/// milliradians).
const JUMP_RAD: f64 = 0.2;
/// The tone's place: one block's gap is this fraction of a turn.
const TURNS_PER_BLOCK: f64 = 0.382;
const BLOCK_SAMPLES: f64 = 94.0;
/// About where the tone sits, Hz above the centre.
const TONE_NEAR_HZ: f64 = 300e3;
const REPORT_EVERY: Duration = Duration::from_secs(60);
const RECV_TIMEOUT: f64 = 0.5;

/// The tone's baseband frequency near TONE_NEAR_HZ with one block's gap
/// TURNS_PER_BLOCK of a turn: f · 94 / rate = k + TURNS_PER_BLOCK.
fn tone_hz(rate: f64) -> f64 {
    let per_turn = rate / BLOCK_SAMPLES;
    let k = (TONE_NEAR_HZ / per_turn - TURNS_PER_BLOCK).round();
    (k + TURNS_PER_BLOCK) * per_turn
}

/// Gaps of up to this many blocks must step the tone clear of a whole turn.
const GAP_BLOCKS_CHECKED: u32 = 12;

/// How clear of a whole turn a gap of 1..=GAP_BLOCKS_CHECKED of the chip's
/// blocks steps a tone at `f` (a chip block is 94 of the chip's samples,
/// whatever the decimation after it): the worst, in turns.
fn margin(f: f64, chip_rate: f64) -> f64 {
    (1..=GAP_BLOCKS_CHECKED)
        .map(|n| {
            let t = (n as f64 * f * BLOCK_SAMPLES / chip_rate).fract();
            t.min(1.0 - t)
        })
        .fold(f64::INFINITY, f64::min)
}

/// One tone for dongles at different rates: the frequency from 150 kHz to
/// the narrowest window's 0.35 (or 400 kHz), in 50 Hz steps, whose worst
/// margin over every chip rate is the largest. One rate the chip runs at
/// itself keeps [`tone_hz`]'s placement (the runs before per-dongle rates).
fn tone_for(chip_rates: &[f64], output_rates: &[f64]) -> (f64, f64) {
    let same = chip_rates.windows(2).all(|w| w[0] == w[1])
        && output_rates.windows(2).all(|w| w[0] == w[1]);
    if same && chip_rates[0] == output_rates[0] {
        let f = tone_hz(output_rates[0]);
        return (
            f,
            chip_rates
                .iter()
                .map(|&c| margin(f, c))
                .fold(f64::INFINITY, f64::min),
        );
    }
    let top = output_rates
        .iter()
        .fold(f64::INFINITY, |a, &r| a.min(r * 0.35))
        .min(400e3);
    let mut best = (150e3, -1.0);
    let mut f = 150e3;
    while f <= top {
        let m = chip_rates
            .iter()
            .map(|&c| margin(f, c))
            .fold(f64::INFINITY, f64::min);
        if m > best.1 {
            best = (f, m);
        }
        f += 50.0;
    }
    best
}

/// The phase of each 64-sample window against the tone, streamed: the
/// step from the last window less the run's slow drift (an average of
/// the steps), and the jumps.
struct PhaseTrack {
    cycles_per_sample: f64,
    n: u64,
    partial: Vec<Iq>,
    last: Option<f64>,
    drift: f64,
    windows: u64,
    jumps: u64,
    largest: f64,
    /// The first JUMPS_LOGGED jumps: (sample index of the window, the
    /// step past the drift in rad, the window's amplitude over the run's
    /// typical one).
    log: Vec<(u64, f64, f64)>,
    /// The windows' amplitude, learned slowly.
    amp: f64,
}

const JUMPS_LOGGED: usize = 200;

impl PhaseTrack {
    fn new(tone: f64, rate: f64) -> PhaseTrack {
        PhaseTrack {
            cycles_per_sample: tone / rate,
            n: 0,
            partial: Vec::with_capacity(WIN),
            last: None,
            drift: 0.0,
            windows: 0,
            jumps: 0,
            largest: 0.0,
            log: Vec::new(),
            amp: 0.0,
        }
    }

    fn feed(&mut self, xs: &[Iq]) {
        for &x in xs {
            self.partial.push(x);
            if self.partial.len() == WIN {
                self.window();
                self.partial.clear();
            }
        }
    }

    fn window(&mut self) {
        let (mut re, mut im) = (0.0, 0.0);
        for (j, v) in self.partial.iter().enumerate() {
            // The tone's phase at this sample, from the sample's index (its
            // fraction of a turn, so the product stays exact over hours).
            let turns = ((self.n + j as u64) as f64 * self.cycles_per_sample).fract();
            let ph = -TAU * turns;
            let (c, s) = (ph.cos(), ph.sin());
            re += v.i as f64 * c - v.q as f64 * s;
            im += v.i as f64 * s + v.q as f64 * c;
        }
        let at = self.n;
        self.n += WIN as u64;
        let phase = im.atan2(re);
        let amp = (re * re + im * im).sqrt() / WIN as f64;
        if let Some(last) = self.last {
            let wrap = |d: f64| d - TAU * (d / TAU).round();
            let step = wrap(phase - last);
            let off = wrap(step - self.drift);
            if off.abs() > JUMP_RAD {
                self.jumps += 1;
                self.largest = self.largest.max(off.abs());
                if self.log.len() < JUMPS_LOGGED {
                    self.log
                        .push((at, off, amp / self.amp.max(f64::MIN_POSITIVE)));
                }
            } else {
                self.amp += (amp - self.amp) / if self.amp == 0.0 { 1.0 } else { 256.0 };
                // The drift: the tone's frequency against the expected,
                // learned slowly.
                self.drift += (step - self.drift) / 256.0;
            }
        }
        self.last = Some(phase);
        self.windows += 1;
    }
}

/// `--dupcheck`: the samples in 256-sample chunks (one 512-byte USB
/// packet each, as a transfer starts on a packet), each hashed and looked
/// up among the last DUP_WINDOW chunks. Noise never repeats exactly, so a
/// chunk equal to a recent one is data delivered twice. Counts the
/// repeats and logs the first DUPS_LOGGED (chunk index, distance back).
const DUP_CHUNK: usize = 256;
const DUP_WINDOW: usize = 32 * 512;
const DUPS_LOGGED: usize = 40;

#[derive(Default)]
struct DupCheck {
    seen: std::collections::HashMap<u64, u64>,
    order: std::collections::VecDeque<u64>,
    partial: Vec<Iq>,
    chunks: u64,
    dups: u64,
    log: Vec<(u64, u64)>,
}

impl DupCheck {
    fn feed(&mut self, xs: &[Iq]) {
        for &x in xs {
            self.partial.push(x);
            if self.partial.len() == DUP_CHUNK {
                self.chunk();
                self.partial.clear();
            }
        }
    }

    fn chunk(&mut self) {
        // FNV-1a over the samples' bytes.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for v in &self.partial {
            for b in v.i.to_le_bytes().into_iter().chain(v.q.to_le_bytes()) {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        if let Some(&at) = self.seen.get(&h) {
            self.dups += 1;
            if self.log.len() < DUPS_LOGGED {
                self.log.push((self.chunks, self.chunks - at));
            }
        }
        self.seen.insert(h, self.chunks);
        self.order.push_back(h);
        if self.order.len() > DUP_WINDOW
            && let Some(old) = self.order.pop_front()
            && self
                .seen
                .get(&old)
                .is_some_and(|&i| self.chunks - i >= DUP_WINDOW as u64)
        {
            self.seen.remove(&old);
        }
        self.chunks += 1;
    }
}

/// Linux's clock ticks per second for /proc's CPU times (USER_HZ, 100 on
/// every architecture Linux builds for).
const USER_HZ: f64 = 100.0;

/// This process's CPU time so far, user and system, in seconds, from
/// /proc/self/stat (fields 14 and 15); None off Linux.
fn cpu_seconds() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name (field 2) is in parentheses and may hold spaces:
    // count from after its closing one (field 3 onward).
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| {
        f.get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .map(|t| t / USER_HZ)
    };
    Some((tick(11)?, tick(12)?))
}

/// `--inject`'s gaps: one block at 30 s, two at 40 s, and 4,096 samples
/// (0.19 of a turn at 2.88 Msps) at 50 s — three jumps to be counted.
const INJECTED: [(u64, usize); 3] = [(30, 94), (40, 188), (50, 4096)];

/// `--stall-at`'s pause: longer than the driver's whole transfer queue
/// (≥ 128 × 8,192 samples, 0.36 s at 2.88 Msps).
const STALL: Duration = Duration::from_secs(2);

/// The run's switches, as the command line sets them.
#[derive(Clone, Copy)]
struct Switches {
    /// `--inject`.
    injecting: bool,
    /// `--stall-at S`.
    stall_at: Option<Duration>,
    /// `--dupcheck`, `--dup-inject`.
    dupcheck: bool,
    dup_inject: bool,
    /// `--transfer-bytes N` (0: the driver's).
    transfer_bytes: usize,
}

fn soak(
    spec: &str,
    rate_asked: f64,
    tone: Option<f64>,
    run: Duration,
    out: &Mutex<()>,
    switches: Switches,
) -> Result<(), String> {
    let Switches {
        injecting,
        mut stall_at,
        dupcheck,
        dup_inject,
        transfer_bytes,
    } = switches;
    let mut dup = DupCheck::default();
    let mut dup_injected = false;
    let mut inject: Vec<(Duration, usize)> = if injecting {
        INJECTED
            .iter()
            .map(|&(s, n)| (Duration::from_secs(s), n))
            .collect()
    } else {
        Vec::new()
    };
    let mut rtl = Rtl::open(spec).map_err(|e| e.to_string())?;
    rtl.set_rate(rate_asked).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(OPEN_RF_HZ))
        .map_err(|e| e.to_string())?;
    for (stage, code) in [(Stage::Lna, 8), (Stage::Mixer, 8), (Stage::Vga, 2)] {
        rtl.set_stage_level(stage, code)
            .map_err(|e| e.to_string())?;
    }
    let (rate, decimation, ppm) = (rtl.rate_set(), rtl.decimation(), rtl.ppm());
    let mut s = rtl
        .into_stream(transfer_bytes, 0)
        .map_err(|e| e.error.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    let mut track = PhaseTrack::new(tone.unwrap_or(0.0), rate);
    let (mut samples, mut overflows, mut timeouts) = (0u64, 0u64, 0u64);
    let (mut link_gap_us, mut late, mut deficit) = (0u32, 0u64, 0i64);
    let (mut read_gap, mut last_return) = (Duration::ZERO, Duration::ZERO);
    let t0 = Instant::now();
    let mut next_report = REPORT_EVERY;
    let line = |kind: &str,
                t: Duration,
                samples: u64,
                overflows: u64,
                timeouts: u64,
                track: &PhaseTrack,
                extra: &str| {
        let secs = t.as_secs_f64();
        let _g = out.lock();
        println!(
            "{{\"kind\":\"{kind}\",\"dongle\":{spec:?},\"rate\":{rate},\"decimation\":{decimation},\"ppm\":{ppm},\"tone_hz\":{},\"secs\":{secs:.1},\"samples\":{samples},\"delivered_over_rate\":{:.7},\"overflows\":{overflows},\"timeouts\":{timeouts},\"windows\":{},\"phase_jumps\":{},\"largest_jump_rad\":{:.3},\"drift_rad_per_window\":{:.6}{extra}}}",
            tone.map_or("null".to_string(), |t| format!("{t:.1}")),
            samples as f64 / (rate * secs),
            track.windows,
            track.jumps,
            track.largest,
            track.drift
        );
    };
    while t0.elapsed() < run {
        let info = s
            .recv_into(&mut buf, RECV_TIMEOUT)
            .map_err(|e| e.to_string())?;
        overflows += info.overflow as u64;
        // The link, this minute: the worst gap between transfer
        // completions, the late ones, the ledger's deficit as it stands;
        // and the worst gap between this reader's own returns.
        let now = t0.elapsed();
        read_gap = read_gap.max(now.saturating_sub(last_return));
        last_return = now;
        if let Some(l) = info.link {
            link_gap_us = link_gap_us.max(l.max_gap_us);
            late += l.late_transfers as u64;
            deficit = l.ledger_deficit_samples;
        }
        timeouts += info.timed_out as u64;
        samples += info.n_samples as u64;
        if dupcheck {
            dup.feed(&buf[..info.n_samples]);
            // `--dup-inject`: the checker's own test — one receive fed
            // twice at 20 s; its chunks must all read as repeats.
            if dup_inject && !dup_injected && t0.elapsed() >= Duration::from_secs(20) {
                dup.feed(&buf[..info.n_samples]);
                dup_injected = true;
            }
        }
        // `--inject`: the witness's own test — gaps cut from what the
        // phase reader sees (not from the counts), each to be found.
        // `--stall-at S`: the reader stops for STALL once, at S seconds —
        // the chip's FIFO must overflow and its counter must say so.
        if let Some(at) = stall_at
            && t0.elapsed() >= at
        {
            std::thread::sleep(STALL);
            stall_at = None;
        }
        let mut from = 0;
        if let Some(&(at, gap)) = inject.first()
            && t0.elapsed() >= at
            && info.n_samples > gap
        {
            from = gap;
            inject.remove(0);
        }
        if tone.is_some() {
            track.feed(&buf[from..info.n_samples]);
        }
        if t0.elapsed() >= next_report {
            let extra = format!(
                ",\"minute_link_max_gap_us\":{link_gap_us},\"minute_late_transfers\":{late},\"ledger_deficit_samples\":{deficit},\"minute_read_max_gap_ms\":{:.1},\"process_cpu_s\":{},\"dup_chunks\":{}",
                read_gap.as_secs_f64() * 1e3,
                // The whole process's (every dongle's thread), so far.
                cpu_seconds().map_or("null".to_string(), |(u, s)| format!("[{u:.2},{s:.2}]")),
                if dupcheck {
                    format!(
                        "[{},{},[{}]]",
                        dup.chunks,
                        dup.dups,
                        dup.log
                            .iter()
                            .map(|(a, b)| format!("[{a},{b}]"))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                } else {
                    "null".to_string()
                }
            );
            line(
                "progress",
                t0.elapsed(),
                samples,
                overflows,
                timeouts,
                &track,
                &extra,
            );
            (link_gap_us, late, read_gap) = (0, 0, Duration::ZERO);
            next_report += REPORT_EVERY;
        }
    }
    let t = t0.elapsed();
    let fin = s.finish().map_err(|e| e.to_string())?;
    let r = &fin.report;
    line(
        "final",
        t,
        samples,
        overflows,
        timeouts,
        &track,
        &format!(
            ",\"dropped_blocks\":{},\"saturated\":{},\"host_lost_samples\":{},\"clock_lost_samples\":{},\"stall_retries\":{},\"breaks_total\":{},\"report_samples\":{},\"jump_log\":[{}]",
            r.dropped_blocks,
            r.saturated,
            r.host_lost_samples,
            r.clock_lost_samples,
            r.stall_retries,
            r.breaks_total,
            r.samples,
            track
                .log
                .iter()
                .map(|(n, d, a)| format!("[{n},{d:.3},{a:.3}]"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rate: f64 = get("--rate").and_then(|v| v.parse().ok()).unwrap_or(2.88e6);
    let minutes: f64 = get("--minutes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let injecting = args.iter().any(|a| a == "--inject");
    let stall_at: Option<Duration> = get("--stall-at")
        .and_then(|v| v.parse::<f64>().ok())
        .map(Duration::from_secs_f64);
    let dupcheck = args.iter().any(|a| a == "--dupcheck");
    let dup_inject = args.iter().any(|a| a == "--dup-inject");
    // `--transfer-bytes N`: the stream's transfer size (0, the default,
    // is the driver's 16 KiB).
    let transfer_bytes: usize = get("--transfer-bytes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let dongles: Vec<String> = get("--dongles")
        .map(|v| v.split(';').map(str::to_string).collect())
        .unwrap_or_else(|| vec![rtl_spec()]);
    let cmd = get("--source-cmd").unwrap_or_else(|| SOURCE_CMD.into());
    // `--rates a;b`: a rate per dongle, in `--dongles`' order (else
    // `--rate` for every one).
    let rates: Vec<f64> = match get("--rates") {
        Some(v) => v
            .split(';')
            .map(|r| {
                r.parse::<f64>()
                    .map_err(|_| format!("--rates: {r} is not a rate"))
            })
            .collect::<Result<_, _>>()?,
        None => vec![rate; dongles.len()],
    };
    if rates.len() != dongles.len() {
        return Err(format!(
            "--rates names {} rates for {} dongles",
            rates.len(),
            dongles.len()
        ));
    }
    let chip_rates: Vec<f64> = rates
        .iter()
        .map(|&r| {
            sdr_rtl::demod::plan_for_output_rate(r)
                .map(|p| p.chip_rate())
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let (tone, tone_margin) = tone_for(&chip_rates, &rates);
    // `--no-source`: no tone (a host without the bench transmitter). The
    // phase witness is off then — noise has no phase to follow — and the
    // loss witnesses are the chip's counter, the ledger and `clockwatch`.
    let no_source = args.iter().any(|a| a == "--no-source");
    let mut src = if no_source {
        None
    } else {
        Some(Source::spawn(&cmd, 0, -15)?)
    };
    if let Some(src) = src.as_mut() {
        src.tone(OPEN_RF_HZ + tone)?;
    }
    let tone = if no_source { None } else { Some(tone) };
    println!(
        "{{\"kind\":\"header\",\"step\":\"27\",\"program\":\"examples/soak.rs\",\"rates\":{rates:?},\"chip_rates\":{chip_rates:?},\"minutes\":{minutes},\"dongles\":{},\"tone_hz\":{},\"tone_margin_turns\":{tone_margin:.3},\"jump_rad\":{JUMP_RAD},\"inject\":{injecting},\"transfer_bytes\":{transfer_bytes},\"source\":{:?}}}",
        dongles.len(),
        tone.map_or("null".to_string(), |t| format!("{t:.1}")),
        if no_source {
            "none".to_string()
        } else {
            cmd.clone()
        }
    );
    let out = Mutex::new(());
    let run = Duration::from_secs_f64(minutes * 60.0);
    std::thread::scope(|sc| {
        for (d, &r) in dongles.iter().zip(&rates) {
            let out = &out;
            sc.spawn(move || {
                let switches = Switches {
                    injecting,
                    stall_at,
                    dupcheck,
                    dup_inject,
                    transfer_bytes,
                };
                if let Err(e) = soak(d, r, tone, run, out, switches) {
                    let _g = out.lock();
                    println!("{{\"kind\":\"error\",\"dongle\":{d:?},\"error\":{e:?}}}");
                }
            });
        }
    });
    if let Some(src) = src.as_mut() {
        src.off()?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("soak: {e}");
        std::process::exit(1);
    }
}
