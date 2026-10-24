//! Output rates the chip cannot resample at, by whole-number decimation of
//! one it can (findings 25).
//!
//! The chip is used directly at 0.24-0.30 and 0.905-2.88 Msps
//! (`demod::OUTPUT_RATE_LOW_SPS`, `demod::OUTPUT_RATE_GOOD_SPS`): between,
//! it resamples at another rate. Every rate from 0.30 to 0.905 Msps is a
//! whole 2, 3 or 4 times under one
//! inside those spans (`demod::plan_for_output_rate`), so the stream runs
//! the chip at k × the rate and this filter takes every k-th sample of a
//! low-pass.
//!
//! The low-pass is a Kaiser-windowed sinc at the input rate:
//! - flat to [`PASS_EDGE`] × the output rate (within [`PASS_RIPPLE_DB`]);
//! - at least [`STOP_ATTENUATION_DB`] down from [`STOP_EDGE`] × the output
//!   rate to the input's Nyquist frequency, so nothing that folds into
//!   the passband comes through louder than that;
//! - unity at DC.
//!
//! The filter is causal: output m is made when input m·k + k − 1 arrives,
//! from that input and the [`Decimator::len`] − 1 before it. A
//! configuration change at input A (a break) first touches output
//! [`Decimator::first_output_touching`]`(A)`; the outputs from
//! [`Decimator::first_output_clear_of`]`(A + s)` on hold nothing from
//! before input A + s.

use std::f64::consts::PI;
use std::vec::Vec;

use sdr_core::Iq;

/// The passband's edge, a fraction of the output rate: flat to ±0.42 of
/// it (a 0.5 Msps output passes ±210 kHz).
pub const PASS_EDGE: f64 = 0.42;
/// The stopband's edge, a fraction of the output rate: everything from
/// here to the input's Nyquist frequency folds onto −(1 − x) … and
/// stays outside ±[`PASS_EDGE`] (1 − 0.58 = 0.42).
pub const STOP_EDGE: f64 = 0.58;
/// The least attenuation in the stopband.
pub const STOP_ATTENUATION_DB: f64 = 70.0;
/// The largest deviation from unity in the passband.
pub const PASS_RIPPLE_DB: f64 = 0.1;

/// Kaiser's window parameter for an attenuation (Kaiser 1974, as Oppenheim
/// & Schafer state it): 0.1102 (A − 8.7) above 50 dB.
const KAISER_BETA_SLOPE: f64 = 0.1102;
const KAISER_BETA_OFFSET_DB: f64 = 8.7;
/// Kaiser's length estimate: (A − 8) / (2.285 Δω) + 1.
const KAISER_LENGTH_OFFSET_DB: f64 = 8.0;
const KAISER_LENGTH_SCALE: f64 = 2.285;
/// Designed for this much more than [`STOP_ATTENUATION_DB`]: Kaiser's
/// estimates fell 1.6 dB short of it at k = 2 with the taps in f32.
const DESIGN_MARGIN_DB: f64 = 5.0;
/// The modified Bessel function's series is summed until a term is this
/// small against the sum.
const BESSEL_EPSILON: f64 = 1e-12;
/// Full scale of a widened sample (`Iq`), as f32: the tests' reference
/// for the output's clamp.
#[cfg(test)]
const SAMPLE_MAX: f32 = i16::MAX as f32;
#[cfg(test)]
const SAMPLE_MIN: f32 = i16::MIN as f32;

/// I0, the zeroth-order modified Bessel function of the first kind, by its
/// series Σ ((x/2)^j / j!)².
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut j) = (1.0, 1.0, 1.0);
    loop {
        term *= (x / (2.0 * j)) * (x / (2.0 * j));
        sum += term;
        if term < sum * BESSEL_EPSILON {
            return sum;
        }
        j += 1.0;
    }
}

/// The taps for decimation by `k`, unity at DC: a sinc cut off at half the
/// output rate under a Kaiser window sized for [`STOP_ATTENUATION_DB`] over
/// the transition [`PASS_EDGE`]…[`STOP_EDGE`].
pub fn design(k: u32) -> Vec<f32> {
    design_with_margin(k, DESIGN_MARGIN_DB)
}

/// [`design`] for [`STOP_ATTENUATION_DB`] + `margin_db`.
fn design_with_margin(k: u32, margin_db: f64) -> Vec<f32> {
    // By 1: every sample as it is.
    if k <= 1 {
        return vec![1.0];
    }
    let k = k as f64;
    // At the input rate, cycles per sample.
    let cutoff = 0.5 / k;
    let transition = (STOP_EDGE - PASS_EDGE) / k;
    let a = STOP_ATTENUATION_DB + margin_db;
    let beta = KAISER_BETA_SLOPE * (a - KAISER_BETA_OFFSET_DB);
    let n = ((a - KAISER_LENGTH_OFFSET_DB) / (KAISER_LENGTH_SCALE * 2.0 * PI * transition)).ceil()
        as usize
        + 1;
    // Odd, so the centre is a sample.
    let n = n | 1;
    let m = (n - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(beta);
    let h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - m;
            let sinc = if t == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * t).sin() / (PI * t)
            };
            let w = bessel_i0(beta * (1.0 - (t / m) * (t / m)).max(0.0).sqrt()) / i0_beta;
            sinc * w
        })
        .collect();
    let dc: f64 = h.iter().sum();
    h.iter().map(|v| (v / dc) as f32).collect()
}

/// The filter's response at `f` cycles per input sample, as a magnitude
/// (1 at DC).
pub fn response(taps: &[f32], f: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (i, t) in taps.iter().enumerate() {
        let ph = -2.0 * PI * f * i as f64;
        re += *t as f64 * ph.cos();
        im += *t as f64 * ph.sin();
    }
    re.hypot(im)
}

pub mod kernels;

pub use kernels::Isa;
use kernels::{BLOCK_LANES, LANES_PER_SAMPLE};

/// How the decimator sums each output (the same filter in every case):
/// [`Kernel::F32`] as since 2026-10-19; the others compared against it
/// on the bench (findings 29.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kernel {
    /// f32 taps and samples, one output per pass over the taps.
    F32,
    /// [`Kernel::F32`], two outputs per pass: each tap loaded once for
    /// both, each output summed exactly as [`Kernel::F32`] sums it.
    F32Pairs,
    /// f32, the taps' symmetry used: half the multiplies.
    F32Folded,
    /// 16-bit samples and taps (the taps rounded to 2⁻¹⁵, or 2⁻¹⁴ where
    /// one would not fit), 32-bit sums: exact for those taps.
    I16,
    /// [`Kernel::I16`], two outputs per pass.
    I16Pairs,
}

impl Kernel {
    pub const ALL: [Kernel; 5] = [
        Kernel::F32,
        Kernel::F32Pairs,
        Kernel::F32Folded,
        Kernel::I16,
        Kernel::I16Pairs,
    ];

    fn integer(self) -> bool {
        matches!(self, Kernel::I16 | Kernel::I16Pairs)
    }

    fn pairs(self) -> bool {
        matches!(self, Kernel::F32Pairs | Kernel::I16Pairs)
    }
}

/// The taps in 16 bits: each × 2^shift rounded, the centre tap then
/// moved so they sum to exactly 2^shift (unity at DC). The shift is 15
/// unless a tap would not fit, then 14.
pub fn quantize(taps: &[f32]) -> (Vec<i16>, u32) {
    let largest = taps.iter().fold(0.0f64, |m, t| m.max((*t as f64).abs()));
    let shift = if largest * (1u32 << 15) as f64 <= i16::MAX as f64 {
        15
    } else {
        14
    };
    let scale = (1u32 << shift) as f64;
    let mut q: Vec<i16> = taps
        .iter()
        .map(|t| (*t as f64 * scale).round() as i16)
        .collect();
    let short = (1i32 << shift) - q.iter().map(|v| *v as i32).sum::<i32>();
    let centre = (q.len() - 1) / 2;
    q[centre] = (q[centre] as i32 + short) as i16;
    (q, shift)
}

/// Decimation by k of a stream of samples, in order, across calls.
#[derive(Clone, Debug)]
pub struct Decimator {
    k: u32,
    taps: Vec<f32>,
    how: Kernel,
    /// The instruction set the f32 kernels run on.
    isa: Isa,
    /// The samples each output reaches, the taps padded with zeros at the
    /// oldest end to a whole number of [`BLOCK_LANES`].
    span: usize,
    /// The taps for a window held oldest first, each twice (I, Q lanes).
    kernel: Vec<f32>,
    /// The first (len − 1) / 2 taps oldest first, each twice, and the
    /// centre tap ([`Kernel::F32Folded`]).
    folded: Vec<f32>,
    centre: f32,
    /// `kernel` in 16 bits ([`quantize`]) and its shift.
    kernel_i16: Vec<i16>,
    shift: u32,
    /// The inputs as interleaved I, Q — in f32, or as they come for the
    /// integer kernels: the last `span` − 1 before this call (zeros
    /// before the first), then the call's own.
    window: Vec<f32>,
    window_i16: Vec<i16>,
    /// Inputs taken so far.
    inputs: u64,
    /// The input whose arrival makes the next output: m·k + k − 1.
    next_newest: u64,
}

/// A sum in f32 as an output sample: rounded half away from zero, held to
/// full scale — `f32::round`'s answer, without its cost. x86-64 before
/// SSE4.1 has no rounding instruction, so `f32::round` there is a
/// library call, two per output (findings 29.3); here the sum is widened
/// to f64, where adding ±0.5 is exact for any |v| < 2^52, and the
/// conversion to an integer truncates (and saturates, NaN to 0).
#[inline]
pub(crate) fn sample_f32(v: f32) -> i16 {
    let v = v as f64;
    ((v + 0.5f64.copysign(v)) as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// A 32-bit sum of 16-bit taps (scaled by 2^shift) as an output sample:
/// rounded half up, held to full scale.
fn sample_i32(v: i32, shift: u32) -> i16 {
    ((v as i64 + (1i64 << (shift - 1))) >> shift).clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

impl Decimator {
    /// Two outputs per pass ([`Kernel::F32Pairs`]): the fastest of the
    /// kernels whose outputs are [`Kernel::F32`]'s, bit for bit
    /// (findings 29.3).
    pub fn new(k: u32) -> Decimator {
        Decimator::with_kernel(k, Kernel::F32Pairs)
    }

    /// The decimator summing each output as `how` says, on the widest
    /// instruction set this machine runs ([`Isa::best`]).
    pub fn with_kernel(k: u32, how: Kernel) -> Decimator {
        Decimator::build(k, how, Isa::best())
    }

    /// [`Decimator::with_kernel`] on a given instruction set; `None`
    /// where this machine does not run it ([`Isa::is_available`]).
    pub fn with_kernel_on(k: u32, how: Kernel, isa: Isa) -> Option<Decimator> {
        isa.is_available().then(|| Decimator::build(k, how, isa))
    }

    fn build(k: u32, how: Kernel, isa: Isa) -> Decimator {
        let k = k.max(1);
        let taps = design(k);
        let span = taps.len().next_multiple_of(BLOCK_LANES / LANES_PER_SAMPLE);
        // Oldest first: window position s meets tap span − 1 − s (the
        // newest input meets the first tap), past the taps a zero.
        let oldest_first = |s: usize| taps.get(span - 1 - s).copied();
        let kernel = (0..span)
            .flat_map(|s| [oldest_first(s).unwrap_or(0.0); LANES_PER_SAMPLE])
            .collect();
        let (q, shift) = quantize(&taps);
        let kernel_i16 = (0..span)
            .flat_map(|s| [q.get(span - 1 - s).copied().unwrap_or(0); LANES_PER_SAMPLE])
            .collect();
        // The taps are symmetric (`design`), so oldest first they are the
        // same as newest first.
        let c = (taps.len() - 1) / 2;
        let folded = taps[..c]
            .iter()
            .flat_map(|t| [*t; LANES_PER_SAMPLE])
            .collect();
        let centre = taps[c];
        let held = (span - 1) * LANES_PER_SAMPLE;
        let (window, window_i16) = if how.integer() {
            (Vec::new(), vec![0; held])
        } else {
            (vec![0.0; held], Vec::new())
        };
        Decimator {
            k,
            taps,
            how,
            isa,
            span,
            kernel,
            folded,
            centre,
            kernel_i16,
            shift,
            window,
            window_i16,
            inputs: 0,
            next_newest: k as u64 - 1,
        }
    }

    pub fn kernel(&self) -> Kernel {
        self.how
    }

    /// The instruction set the f32 kernels run on.
    pub fn isa(&self) -> Isa {
        self.isa
    }

    pub fn factor(&self) -> u32 {
        self.k
    }

    /// The taps (the newest input meets the first).
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    /// Inputs per output's reach: the number of taps.
    pub fn len(&self) -> usize {
        self.taps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Outputs made so far.
    pub fn outputs(&self) -> u64 {
        self.inputs / self.k as u64
    }

    /// The first output whose inputs reach input `a`: output m is made
    /// from inputs up to m·k + k − 1.
    pub fn first_output_touching(&self, a: u64) -> u64 {
        a / self.k as u64
    }

    /// The first output whose inputs all come from input `a` on: its
    /// oldest, m·k + k − len, is at least `a`.
    pub fn first_output_clear_of(&self, a: u64) -> u64 {
        let k = self.k as u64;
        (a + self.taps.len() as u64 - k).div_ceil(k)
    }

    /// Take `input`, append the outputs it completes to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        let integer = self.how.integer();
        if integer {
            self.window_i16
                .extend(input.iter().flat_map(|s| [s.i, s.q]));
        } else {
            self.window
                .extend(input.iter().flat_map(|s| [s.i as f32, s.q as f32]));
        }
        self.inputs += input.len() as u64;
        // The window's last sample is input `inputs` − 1.
        let held = if integer {
            self.window_i16.len()
        } else {
            self.window.len()
        } / LANES_PER_SAMPLE;
        let k = self.k as u64;
        out.reserve(((self.inputs.saturating_sub(self.next_newest)).div_ceil(k)) as usize);
        let (span, lanes) = (self.span, self.span * LANES_PER_SAMPLE);
        // An output's window, from its newest input counted back from
        // the window's end: its first lane.
        let inputs = self.inputs;
        // The f32 kernels: the whole loop on the instruction set, chosen
        // once (`kernels::outputs`); it leaves `next_newest` past the
        // inputs, so the loop below is the comparison kernels'.
        if matches!(self.how, Kernel::F32 | Kernel::F32Pairs) {
            let pass = kernels::Pass {
                window: &self.window,
                kernel: &self.kernel,
                span,
                k,
                pairs: self.how.pairs(),
                inputs,
            };
            kernels::outputs(self.isa, &pass, &mut self.next_newest, out);
        }
        let first = |newest: u64| (held - (inputs - 1 - newest) as usize - span) * LANES_PER_SAMPLE;
        while self.next_newest < self.inputs {
            let a = first(self.next_newest);
            let two = self.how.pairs() && self.next_newest + k < self.inputs;
            let b = a + self.k as usize * LANES_PER_SAMPLE;
            match self.how {
                Kernel::F32 | Kernel::F32Pairs => unreachable!("made by kernels::outputs"),
                Kernel::F32Folded => {
                    // The real taps are the window's last len samples.
                    let w =
                        &self.window[a + (span - self.taps.len()) * LANES_PER_SAMPLE..a + lanes];
                    let (i, q) = kernels::folded(w, &self.folded, self.centre);
                    out.push(Iq {
                        i: sample_f32(i),
                        q: sample_f32(q),
                    });
                }
                Kernel::I16Pairs if two => {
                    let ((i, q), (i2, q2)) = kernels::dot2_i16(
                        &self.window_i16[a..a + lanes],
                        &self.window_i16[b..b + lanes],
                        &self.kernel_i16,
                    );
                    out.push(Iq {
                        i: sample_i32(i, self.shift),
                        q: sample_i32(q, self.shift),
                    });
                    out.push(Iq {
                        i: sample_i32(i2, self.shift),
                        q: sample_i32(q2, self.shift),
                    });
                }
                Kernel::I16 | Kernel::I16Pairs => {
                    let (i, q) = kernels::dot_i16(&self.window_i16[a..a + lanes], &self.kernel_i16);
                    out.push(Iq {
                        i: sample_i32(i, self.shift),
                        q: sample_i32(q, self.shift),
                    });
                }
            }
            self.next_newest += if two { 2 * k } else { k };
        }
        // Keep the last span − 1 inputs for the next call.
        let drop = (held + 1 - span) * LANES_PER_SAMPLE;
        if integer {
            self.window_i16.drain(..drop);
        } else {
            self.window.drain(..drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response in dB at `f` (cycles per input sample).
    fn db(taps: &[f32], f: f64) -> f64 {
        20.0 * response(taps, f).log10()
    }

    #[test]
    fn unity_at_dc_flat_in_the_passband_and_down_in_the_stopband() {
        for k in 2..=4u32 {
            let taps = design(k);
            let kf = k as f64;
            assert!(db(&taps, 0.0).abs() < 1e-6, "k {k}: DC");
            // The passband, ±PASS_EDGE of the output rate.
            let mut worst_pass: f64 = 0.0;
            for j in 0..=200 {
                let f = PASS_EDGE / kf * j as f64 / 200.0;
                worst_pass = worst_pass.max(db(&taps, f).abs());
            }
            assert!(
                worst_pass <= PASS_RIPPLE_DB,
                "k {k}: passband ripple {worst_pass:.4} dB"
            );
            // The stopband, from STOP_EDGE of the output rate to the
            // input's Nyquist frequency.
            let mut least_stop = f64::MAX;
            for j in 0..=2000 {
                let f = STOP_EDGE / kf + (0.5 - STOP_EDGE / kf) * j as f64 / 2000.0;
                least_stop = least_stop.min(-db(&taps, f));
            }
            assert!(
                least_stop >= STOP_ATTENUATION_DB,
                "k {k}: stopband {least_stop:.1} dB"
            );
        }
    }

    /// `input` through a fresh decimator in pieces of the sizes given, in
    /// turn.
    fn in_pieces(k: u32, how: Kernel, input: &[Iq], sizes: &[usize]) -> (Decimator, Vec<Iq>) {
        in_pieces_on(k, how, Isa::best(), input, sizes)
    }

    /// [`in_pieces`] on a given instruction set.
    fn in_pieces_on(
        k: u32,
        how: Kernel,
        isa: Isa,
        input: &[Iq],
        sizes: &[usize],
    ) -> (Decimator, Vec<Iq>) {
        let mut d = Decimator::with_kernel_on(k, how, isa).expect("an available instruction set");
        let mut out = Vec::new();
        let mut at = 0;
        for size in sizes.iter().cycle() {
            if at >= input.len() {
                break;
            }
            let end = (at + size).min(input.len());
            d.process(&input[at..end], &mut out);
            at = end;
        }
        (d, out)
    }

    /// Fed in pieces of any size, the outputs are those of one piece.
    #[test]
    fn pieces_give_the_same_outputs() {
        let input: Vec<Iq> = (0..5_000)
            .map(|n| Iq {
                i: ((n * 37) % 2001 - 1000) as i16 * 20,
                q: ((n * 91) % 1777 - 888) as i16 * 20,
            })
            .collect();
        for how in Kernel::ALL {
            for k in 1..=4u32 {
                let (_, whole) = in_pieces(k, how, &input, &[input.len()]);
                let (d, parts) = in_pieces(k, how, &input, &[1, 7, 64, 333, 1000]);
                assert_eq!(whole, parts, "{how:?} k {k}");
                assert_eq!(whole.len() as u64, input.len() as u64 / k as u64);
                assert_eq!(d.outputs(), whole.len() as u64);
            }
        }
    }

    /// A step at input `a`: nothing changes before
    /// `first_output_touching(a)`, everything has from
    /// `first_output_clear_of(a)` on, and the change lies between.
    #[test]
    fn a_step_is_placed_where_the_mapping_says() {
        for (how, k) in Kernel::ALL
            .iter()
            .flat_map(|h| (2..=4u32).map(move |k| (*h, k)))
        {
            for a in [5_000u64, 5_001, 5_002, 5_003] {
                let input: Vec<Iq> = (0..12_000u64)
                    .map(|n| Iq {
                        i: if n < a { 1000 } else { -3000 },
                        q: 0,
                    })
                    .collect();
                let mut d = Decimator::with_kernel(k, how);
                let mut out = Vec::new();
                d.process(&input, &mut out);
                let touch = d.first_output_touching(a) as usize;
                let clear = d.first_output_clear_of(a) as usize;
                // Past the start-up (the zeros the history began with).
                let warm = d.len().div_ceil(k as usize);
                let first_changed = (warm..out.len())
                    .find(|&m| (out[m].i - 1000).abs() > 1)
                    .expect("a change");
                let last_unsettled = (warm..out.len())
                    .rev()
                    .find(|&m| (out[m].i + 3000).abs() > 1)
                    .expect("a start");
                assert!(
                    first_changed >= touch,
                    "k {k} a {a}: output {first_changed} changed before {touch}"
                );
                assert!(
                    last_unsettled < clear,
                    "k {k} a {a}: output {last_unsettled} unsettled at or after {clear}"
                );
                assert!(
                    clear - touch <= d.len().div_ceil(k as usize),
                    "k {k} a {a}: the span is the filter's reach"
                );
            }
        }
    }

    /// Output m in f64, straight from the definition: Σ taps[j] ·
    /// x[m·k + k − 1 − j], zeros before the first input.
    fn reference(taps: &[f32], k: usize, input: &[Iq]) -> Vec<(f64, f64)> {
        (0..input.len() / k)
            .map(|m| {
                let p = m * k + k - 1;
                let (mut i, mut q) = (0.0, 0.0);
                for (j, t) in taps.iter().enumerate() {
                    if let Some(s) = p.checked_sub(j).map(|n| input[n]) {
                        i += *t as f64 * s.i as f64;
                        q += *t as f64 * s.q as f64;
                    }
                }
                (i, q)
            })
            .collect()
    }

    fn noise(n: usize) -> Vec<Iq> {
        let mut state: u32 = 0x9E37_79B9;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 24) as i16 - 128) * 256
        };
        (0..n)
            .map(|_| Iq {
                i: next(),
                q: next(),
            })
            .collect()
    }

    /// The f32 kernels: every output within one step of the f64 sum,
    /// rounded and held to full scale: only the order of the f32
    /// additions differs (and, folded, the pairs' sums before the
    /// multiply).
    #[test]
    fn the_f32_outputs_are_the_filters_to_one_step() {
        let input = noise(20_000);
        let held = |v: f64| v.round().clamp(SAMPLE_MIN as f64, SAMPLE_MAX as f64);
        for isa in Isa::available() {
            for how in [Kernel::F32, Kernel::F32Pairs, Kernel::F32Folded] {
                for k in 1..=4u32 {
                    let (d, out) = in_pieces_on(k, how, isa, &input, &[1000, 17]);
                    let want = reference(d.taps(), k as usize, &input);
                    assert_eq!(out.len(), want.len());
                    for (m, (o, (i, q))) in out.iter().zip(&want).enumerate() {
                        assert!(
                            (o.i as f64 - held(*i)).abs() <= 1.0
                                && (o.q as f64 - held(*q)).abs() <= 1.0,
                            "{isa:?} {how:?} k {k} output {m}: {o:?} against ({i:.2}, {q:.2})"
                        );
                    }
                }
            }
        }
    }

    /// The integer kernels: every output exactly the sum with the 16-bit
    /// taps, Σ q·x in 64 bits, over 2^shift rounded half up and held to
    /// full scale — the 32-bit sums are exact, so nothing may differ.
    #[test]
    fn the_integer_outputs_are_exact_for_their_taps() {
        let input = noise(20_000);
        for how in [Kernel::I16, Kernel::I16Pairs] {
            for k in 1..=4u32 {
                let (d, out) = in_pieces(k, how, &input, &[1000, 17]);
                let (q, shift) = quantize(d.taps());
                let p_of = |m: usize| m * k as usize + k as usize - 1;
                for (m, o) in out.iter().enumerate() {
                    let (mut si, mut sq) = (0i64, 0i64);
                    for (j, t) in q.iter().enumerate() {
                        if let Some(s) = p_of(m).checked_sub(j).map(|n| input[n]) {
                            si += *t as i64 * s.i as i64;
                            sq += *t as i64 * s.q as i64;
                        }
                    }
                    let as_out = |v: i64| {
                        ((v + (1 << (shift - 1))) >> shift).clamp(i16::MIN as i64, i16::MAX as i64)
                            as i16
                    };
                    assert_eq!(
                        (o.i, o.q),
                        (as_out(si), as_out(sq)),
                        "{how:?} k {k} output {m}"
                    );
                }
            }
        }
    }

    /// Two outputs per pass are the outputs of one per pass, bit for bit.
    #[test]
    fn pairs_are_bit_identical_to_singles() {
        let input = noise(20_000);
        for isa in Isa::available() {
            for (single, pairs) in [
                (Kernel::F32, Kernel::F32Pairs),
                (Kernel::I16, Kernel::I16Pairs),
            ] {
                for k in 1..=4u32 {
                    let (_, a) = in_pieces_on(k, single, isa, &input, &[999, 1, 64]);
                    let (_, b) = in_pieces_on(k, pairs, isa, &input, &[999, 1, 64]);
                    assert_eq!(a, b, "{isa:?} {pairs:?} k {k}");
                }
            }
        }
    }

    /// The instruction sets this machine runs are what the target and
    /// the CPU say, the widest chosen.
    #[test]
    fn the_instruction_sets_are_the_machines() {
        let all = Isa::available();
        assert_eq!(all[0], Isa::Portable);
        assert_eq!(Isa::best(), *all.last().unwrap());
        assert_eq!(all.contains(&Isa::Neon), cfg!(target_arch = "aarch64"));
        assert_eq!(all.contains(&Isa::Sse2), cfg!(target_arch = "x86_64"));
        assert_eq!(Decimator::new(2).isa(), Isa::best());
        for isa in [Isa::Neon, Isa::Sse2, Isa::Avx2, Isa::Avx512] {
            assert_eq!(
                Decimator::with_kernel_on(2, Kernel::F32, isa).is_some(),
                isa.is_available(),
                "{isa:?}"
            );
        }
    }

    /// Bench: the 16-bit taps' stopband and passband against the design
    /// margin (`cargo test --release margin_sweep -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn margin_sweep() {
        for k in 2..=4u32 {
            for margin in [5.0, 7.0, 9.0, 11.0, 13.0, 15.0] {
                let t = design_with_margin(k, margin);
                let (q, shift) = quantize(&t);
                let taps: Vec<f32> = q
                    .iter()
                    .map(|v| *v as f32 / (1u32 << shift) as f32)
                    .collect();
                let kf = k as f64;
                let pass = (0..=200)
                    .map(|j| db(&taps, PASS_EDGE / kf * j as f64 / 200.0).abs())
                    .fold(0.0, f64::max);
                let stop = |t: &[f32]| {
                    (0..=4000)
                        .map(|j| {
                            -db(
                                t,
                                STOP_EDGE / kf + (0.5 - STOP_EDGE / kf) * j as f64 / 4000.0,
                            )
                        })
                        .fold(f64::MAX, f64::min)
                };
                eprintln!(
                    "k {k} margin {margin:>4}: {} taps, f32 stop {:.2} dB, 16-bit stop {:.2} dB, 16-bit ripple {pass:.4} dB",
                    t.len(),
                    stop(&t),
                    stop(&taps)
                );
            }
        }
    }

    /// The folded form rests on the taps being symmetric, exactly.
    #[test]
    fn the_taps_are_symmetric() {
        for k in 1..=4u32 {
            let t = design(k);
            assert!(
                t.iter()
                    .zip(t.iter().rev())
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "k {k}"
            );
        }
    }

    /// The 16-bit taps: unity at DC exactly, the ripple within the
    /// design's limit, and no 32-bit sum can overflow (full-scale samples
    /// of the worst signs included) — but the stopband is not the
    /// design's: rounding the taps to 2⁻¹⁵ leaves 72.7, 67.6 and 65.0 dB
    /// at ÷2, ÷3 and ÷4 against [`STOP_ATTENUATION_DB`]'s 70 (findings
    /// 29.3), which is why [`Kernel::I16`] is not the default.
    #[test]
    fn the_16_bit_taps_keep_dc_and_ripple_but_not_the_stopband() {
        for (k, measured_stop_db) in [(2u32, 72.7), (3, 67.6), (4, 65.0)] {
            let (q, shift) = quantize(&design(k));
            assert_eq!(
                q.iter().map(|v| *v as i32).sum::<i32>(),
                1 << shift,
                "k {k}: DC"
            );
            let taps: Vec<f32> = q
                .iter()
                .map(|v| *v as f32 / (1u32 << shift) as f32)
                .collect();
            let kf = k as f64;
            let worst_pass = (0..=200)
                .map(|j| db(&taps, PASS_EDGE / kf * j as f64 / 200.0).abs())
                .fold(0.0, f64::max);
            assert!(
                worst_pass <= PASS_RIPPLE_DB,
                "k {k}: passband ripple {worst_pass:.4} dB"
            );
            let least_stop = (0..=2000)
                .map(|j| {
                    -db(
                        &taps,
                        STOP_EDGE / kf + (0.5 - STOP_EDGE / kf) * j as f64 / 2000.0,
                    )
                })
                .fold(f64::MAX, f64::min);
            assert!(
                (least_stop - measured_stop_db).abs() < 0.1,
                "k {k}: stopband {least_stop:.2} dB"
            );
            let worst_sum = q.iter().map(|v| (*v as i64).abs()).sum::<i64>() * -(i16::MIN as i64);
            assert!(worst_sum <= i32::MAX as i64, "k {k}: {worst_sum}");
        }
    }

    /// NEON's sums and the portable ones: the f32 ones agree to f32
    /// rounding, the integer ones exactly.
    #[test]
    fn the_kernels_agree_with_their_portable_forms() {
        let s = noise(65);
        let x: Vec<f32> = s[..64]
            .iter()
            .flat_map(|s| [s.i as f32, s.q as f32])
            .collect();
        let x2: Vec<f32> = s[1..]
            .iter()
            .flat_map(|s| [s.i as f32, s.q as f32])
            .collect();
        let h: Vec<f32> = (0..x.len())
            .map(|n| ((n * 7919) % 101) as f32 / 101.0 - 0.5)
            .collect();
        let exact = |x: &[f32], h: &[f32]| {
            x.chunks(2)
                .zip(h.chunks(2))
                .fold((0.0f64, 0.0f64), |(i, q), (s, t)| {
                    (i + s[0] as f64 * t[0] as f64, q + s[1] as f64 * t[1] as f64)
                })
        };
        let near = |got: (f32, f32), want: (f64, f64)| {
            for (g, w) in [(got.0, want.0), (got.1, want.1)] {
                assert!(
                    (g as f64 - w).abs() <= 1e-3 * w.abs().max(1.0),
                    "{g} against {w}"
                );
            }
        };
        // Every instruction set this machine runs: near the exact sums, and
        // two per pass the same bits as one.
        for isa in Isa::available() {
            near(kernels::dot(isa, &x, &h), exact(&x, &h));
            let (a, b) = kernels::dot2(isa, &x, &x2, &h);
            assert_eq!(
                (a, b),
                (kernels::dot(isa, &x, &h), kernels::dot(isa, &x2, &h)),
                "{isa:?}"
            );
            near(a, exact(&x, &h));
            near(b, exact(&x2, &h));
        }
        // Folded: 2c + 1 = 61 samples, the taps symmetric.
        let c = 30;
        let w = &x[..(2 * c + 1) * 2];
        // Each tap twice, as the decimator holds them (I and Q lanes).
        let hf: Vec<f32> = (0..c).flat_map(|s| [h[2 * s]; 2]).collect();
        let hc = 0.25f32;
        let full: Vec<f32> = (0..2 * c + 1)
            .flat_map(|s| {
                let t = if s < c {
                    hf[2 * s]
                } else if s == c {
                    hc
                } else {
                    hf[2 * (2 * c - s)]
                };
                [t, t]
            })
            .collect();
        near(kernels::folded(w, &hf, hc), exact(w, &full));
        near(kernels::portable::folded(w, &hf, hc), exact(w, &full));
        let xi: Vec<i16> = s[..64].iter().flat_map(|s| [s.i, s.q]).collect();
        let xi2: Vec<i16> = s[1..].iter().flat_map(|s| [s.i, s.q]).collect();
        let hi: Vec<i16> = (0..xi.len())
            .map(|n| ((n * 7919) % 2001) as i16 - 1000)
            .collect();
        assert_eq!(
            kernels::dot_i16(&xi, &hi),
            kernels::portable::dot_i16(&xi, &hi)
        );
        assert_eq!(
            kernels::dot2_i16(&xi, &xi2, &hi),
            (kernels::dot_i16(&xi, &hi), kernels::dot_i16(&xi2, &hi))
        );
        assert_eq!(
            kernels::dot2_i16(&xi, &xi2, &hi),
            kernels::portable::dot2_i16(&xi, &xi2, &hi)
        );
    }

    /// The output rounding is `f32::round` then the clamp, bit for bit:
    /// the halves, both sides of every power of two up to full scale and
    /// beyond (where adding 0.5 in f32 would round the sum up a binade),
    /// the specials, and four million f32 bit patterns.
    #[test]
    fn the_output_rounding_is_f32_rounds() {
        let want = |v: f32| v.round().clamp(SAMPLE_MIN, SAMPLE_MAX) as i16;
        let mut cases: Vec<f32> = vec![
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.5,
            -1.5,
            2.5,
            -2.5,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            32767.5,
            -32768.5,
            32766.5,
            -32767.5,
        ];
        for n in 0..40 {
            let p = 2f32.powi(n);
            for base in [p, p - 0.5, p + 0.5, p - 1.0] {
                for v in [
                    base,
                    f32::from_bits(base.to_bits().wrapping_sub(1)),
                    f32::from_bits(base.to_bits() + 1),
                ] {
                    cases.extend([v, -v]);
                }
            }
        }
        let mut state: u32 = 0x2545_F491;
        cases.extend((0..4_000_000).map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            f32::from_bits(state)
        }));
        for v in cases {
            assert_eq!(sample_f32(v), want(v), "{v:e} ({:#010x})", v.to_bits());
        }
    }

    #[test]
    fn one_passes_every_sample_through() {
        let input: Vec<Iq> = (0..100)
            .map(|n| Iq {
                i: n * 100,
                q: -n * 50,
            })
            .collect();
        let mut d = Decimator::new(1);
        let mut out = Vec::new();
        d.process(&input, &mut out);
        assert_eq!(out, input);
    }
}
