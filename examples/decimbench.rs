//! The decimator's cost and accuracy per kernel (findings 29.3), no
//! device: `decim::Decimator` fed noise-like samples in 16,384-sample
//! receives, as the stream feeds it, for each factor and each
//! `decim::Kernel` (the f32 ones on every `decim::Isa` the machine runs:
//! portable, NEON, SSE2, AVX2, AVX-512); the time per input sample, what that is as a share of
//! one core at a chip rate, and how far the outputs lie from the filter's
//! own f64 sum (the f32 taps) over the first outputs.
//!
//!   decimbench [--seconds S]   (input per run, in seconds at 2.4 Msps)
//!
//! One JSON line per factor and kernel.

use std::time::Instant;

use sdr_core::Iq;
use sdr_rtl::decim::{Decimator, Isa, Kernel, kernels};

/// The chip rate the share of a core is given at.
const CHIP_RATE_SPS: f64 = 2.4e6;
/// One receive's worth of samples (16 KiB transfers, findings 18).
const RECEIVE_SAMPLES: usize = 8_192 * 2;
/// Outputs held against the f64 sum.
const CHECKED_OUTPUTS: usize = 200_000;

/// Output m from the f32 taps in f64, rounded and held to full scale.
fn reference(taps: &[f32], k: usize, input: &[Iq], m: usize) -> (f64, f64) {
    let p = m * k + k - 1;
    let (mut i, mut q) = (0.0, 0.0);
    for (j, t) in taps.iter().enumerate() {
        if let Some(s) = p.checked_sub(j).map(|n| input[n]) {
            i += *t as f64 * s.i as f64;
            q += *t as f64 * s.q as f64;
        }
    }
    let held = |v: f64| v.round().clamp(i16::MIN as f64, i16::MAX as f64);
    (held(i), held(q))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: f64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);
    let total = (seconds * CHIP_RATE_SPS) as usize;
    // An 8-bit wire sample widened (× 256 about mid-scale), stepped by a
    // linear congruential generator: no two receives alike.
    let mut state: u32 = 0x1234_5678;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((state >> 24) as i16 - 128) * 256
    };
    let input: Vec<Iq> = (0..RECEIVE_SAMPLES * 64)
        .map(|_| Iq {
            i: next(),
            q: next(),
        })
        .collect();
    if args.iter().any(|a| a == "--parts") {
        return parts(&input, total);
    }
    if args.iter().any(|a| a == "--spans") {
        return spans(&input, total);
    }
    // The f32 kernels on every instruction set this machine runs; the
    // comparison kernels, which have no per-set forms, once.
    let runs: Vec<(Kernel, Isa)> = Kernel::ALL
        .iter()
        .flat_map(|&how| {
            let sets = if matches!(how, Kernel::F32 | Kernel::F32Pairs) {
                Isa::available()
            } else {
                vec![Isa::best()]
            };
            sets.into_iter().map(move |isa| (how, isa))
        })
        .collect();
    for k in 2..=4u32 {
        for &(how, isa) in &runs {
            let mut d =
                Decimator::with_kernel_on(k, how, isa).ok_or("an available instruction set")?;
            let mut out = Vec::with_capacity(RECEIVE_SAMPLES);
            let mut first: Vec<Iq> = Vec::new();
            let mut checksum: i64 = 0;
            let mut fed = 0usize;
            let t = Instant::now();
            while fed < total {
                for chunk in input.chunks(RECEIVE_SAMPLES) {
                    out.clear();
                    d.process(chunk, &mut out);
                    checksum += out.iter().map(|s| s.i as i64 - s.q as i64).sum::<i64>();
                    if first.len() < CHECKED_OUTPUTS && fed < input.len() {
                        first.extend_from_slice(&out);
                    }
                    fed += chunk.len();
                }
            }
            let secs = t.elapsed().as_secs_f64();
            // Against the f32 taps' f64 sum: the largest step off, and how
            // many outputs (I or Q) are off at all.
            let (mut worst, mut off) = (0.0f64, 0usize);
            let checked = first.len().min(CHECKED_OUTPUTS);
            for (m, o) in first[..checked].iter().enumerate() {
                let (i, q) = reference(d.taps(), k as usize, &input, m);
                for e in [(o.i as f64 - i).abs(), (o.q as f64 - q).abs()] {
                    worst = worst.max(e);
                    off += (e > 0.0) as usize;
                }
            }
            let ns_per_input = secs * 1e9 / fed as f64;
            println!(
                "{{\"kind\":\"decim\",\"k\":{k},\"kernel\":\"{how:?}\",\"isa\":\"{isa:?}\",\"taps\":{},\"inputs\":{fed},\"secs\":{secs:.3},\"ns_per_input\":{ns_per_input:.2},\"core_share_at_chip_2_4_msps\":{:.2},\"checked_outputs\":{checked},\"values_off\":{off},\"worst_steps_off\":{worst},\"checksum\":{checksum}}}",
                d.len(),
                ns_per_input * 1e-9 * CHIP_RATE_SPS * 100.0
            );
        }
    }
    Ok(())
}

/// `--parts`: the decimator's two halves timed apart, per input sample.
/// `convert`: a receive widened into the f32 window and the window
/// trimmed back to span − 1 samples, as `Decimator::process` does it on
/// every instruction set. `outputs`: one instruction set's whole output
/// loop (`kernels::outputs`, two outputs per pass) over a window filled
/// in advance. One JSON line each.
fn parts(input: &[Iq], total: usize) -> Result<(), String> {
    let chunk = &input[..RECEIVE_SAMPLES];
    for k in 2..=4u32 {
        let taps = Decimator::new(k).taps().to_vec();
        // As `Decimator` builds them: the taps padded with zeros at the
        // oldest end to whole blocks, oldest first, each twice.
        let span = taps
            .len()
            .next_multiple_of(kernels::BLOCK_LANES / kernels::LANES_PER_SAMPLE);
        let kernel: Vec<f32> = (0..span)
            .flat_map(|s| [taps.get(span - 1 - s).copied().unwrap_or(0.0); 2])
            .collect();
        let reps = total / RECEIVE_SAMPLES;
        // The conversion, the same on every set.
        let mut window: Vec<f32> = vec![0.0; (span - 1) * 2];
        let t = Instant::now();
        for _ in 0..reps {
            window.extend(chunk.iter().flat_map(|s| [s.i as f32, s.q as f32]));
            let drop = window.len() - (span - 1) * 2;
            window.drain(..drop);
        }
        let convert = t.elapsed().as_secs_f64() * 1e9 / (reps * RECEIVE_SAMPLES) as f64;
        println!(
            "{{\"kind\":\"part\",\"k\":{k},\"part\":\"convert\",\"ns_per_input\":{convert:.2}}}"
        );
        // The output loop alone, per set, over one filled window.
        let mut filled: Vec<f32> = vec![0.0; (span - 1) * 2];
        filled.extend(chunk.iter().flat_map(|s| [s.i as f32, s.q as f32]));
        let pass = kernels::Pass {
            window: &filled,
            kernel: &kernel,
            span,
            k: k as u64,
            pairs: true,
            inputs: RECEIVE_SAMPLES as u64,
        };
        let mut out: Vec<Iq> = Vec::with_capacity(RECEIVE_SAMPLES);
        for isa in Isa::available() {
            let mut checksum: i64 = 0;
            let t = Instant::now();
            for _ in 0..reps {
                out.clear();
                let mut next = k as u64 - 1;
                kernels::outputs(isa, &pass, &mut next, &mut out);
                checksum += out.iter().map(|s| s.i as i64 - s.q as i64).sum::<i64>();
            }
            let ns = t.elapsed().as_secs_f64() * 1e9 / (reps * RECEIVE_SAMPLES) as f64;
            println!(
                "{{\"kind\":\"part\",\"k\":{k},\"part\":\"outputs\",\"isa\":\"{isa:?}\",\"ns_per_input\":{ns:.2},\"checksum\":{checksum}}}"
            );
        }
    }
    Ok(())
}

/// `--spans`: one instruction set's output loop (÷2, two outputs per
/// pass) with taps of 64 to 2,048 samples (arbitrary values, the window
/// filled in advance): the time per output and per tap, per set. A cost
/// fixed per output shrinks per tap as the span grows; a cost per tap
/// does not.
fn spans(input: &[Iq], total: usize) -> Result<(), String> {
    const K: u64 = 2;
    for span in [64usize, 128, 256, 512, 1024, 2048] {
        let kernel: Vec<f32> = (0..span * 2)
            .map(|n| ((n * 7919) % 1001) as f32 / 1001.0 - 0.5)
            .collect();
        let samples = RECEIVE_SAMPLES.min(input.len());
        let mut filled: Vec<f32> = vec![0.0; (span - 1) * 2];
        filled.extend(
            input[..samples]
                .iter()
                .flat_map(|s| [s.i as f32, s.q as f32]),
        );
        let pass = kernels::Pass {
            window: &filled,
            kernel: &kernel,
            span,
            k: K,
            pairs: true,
            inputs: samples as u64,
        };
        let outputs_per_pass = samples as u64 / K;
        // The same taps-times-outputs work at every span.
        let reps = (total as u64 * 64 / span as u64 / samples as u64).max(1);
        let mut out: Vec<Iq> = Vec::with_capacity(samples);
        for isa in Isa::available() {
            let t = Instant::now();
            for _ in 0..reps {
                out.clear();
                let mut next = K - 1;
                kernels::outputs(isa, &pass, &mut next, &mut out);
            }
            let ns_output = t.elapsed().as_secs_f64() * 1e9 / (reps * outputs_per_pass) as f64;
            println!(
                "{{\"kind\":\"span\",\"span\":{span},\"isa\":\"{isa:?}\",\"ns_per_output\":{ns_output:.2},\"ps_per_tap\":{:.1}}}",
                ns_output * 1e3 / span as f64
            );
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("decimbench: {e}");
        std::process::exit(1);
    }
}
