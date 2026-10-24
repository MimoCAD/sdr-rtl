//! The decimator's inner products, each on interleaved I/Q: even lanes I,
//! odd lanes Q. One module per instruction set:
//!
//! - [`aarch64`]: NEON, in every aarch64 target.
//! - [`x86_64`]: SSE2, in every x86-64 (amd64) target.
//! - [`avx2`]: AVX2 with FMA (fused multiply-add), where the CPU has them.
//! - [`avx512`]: AVX-512F, where the CPU has it.
//! - [`portable`]: plain Rust the compiler vectorizes as it can; the
//!   reference the others are tested against, and every other target.
//!
//! The f32 products the decimator runs on ([`dot`], [`dot2`]) exist in
//! each; [`Isa`] picks one, from what the CPU reports at run time where
//! the target does not promise it. The comparison kernels ([`folded`],
//! [`dot_i16`], [`dot2_i16`], findings 29.3) are NEON on aarch64 and
//! portable elsewhere.
//!
//! - [`dot`]: Σ x·h in f32.
//! - [`dot2`]: two outputs from one pass over the taps, each summed
//!   exactly as [`dot`] sums it on the same instruction set.
//! - [`folded`]: the taps' symmetry used, h·(x[s] + x[n − 1 − s]).
//! - [`dot_i16`], [`dot2_i16`]: 16-bit samples and taps, 32-bit sums —
//!   exact, so in any order the same.
//!
//! The decimator's whole f32 output loop ([`outputs`], one [`Pass`] per
//! `process` call) is compiled in each module from one definition
//! (`f32_outputs!`), under that module's instruction set: the set is
//! chosen once a call, and the products inline into the loop. A product
//! reached through a call per output cannot inline across the change of
//! instruction set, and on an x86-64 with AVX-512 that call cost more
//! than the wider registers saved (findings 29.3).

/// The f32 outputs of one [`Pass`], `dot` and `dot2` being the products
/// of the module it is expanded in: from output `next` on, one per pass
/// over the taps, or two where `pairs` and both are complete.
macro_rules! f32_outputs {
    ($pass:expr, $next:expr, $out:expr) => {{
        let p: &$crate::decim::kernels::Pass = $pass;
        let next: &mut u64 = $next;
        let out: &mut std::vec::Vec<sdr_core::Iq> = $out;
        let lanes_per_sample = $crate::decim::kernels::LANES_PER_SAMPLE;
        // The window's last sample is input `inputs` − 1.
        let held = p.window.len() / lanes_per_sample;
        let lanes = p.span * lanes_per_sample;
        let step = p.k as usize * lanes_per_sample;
        let sample = |i: f32, q: f32| sdr_core::Iq {
            i: $crate::decim::sample_f32(i),
            q: $crate::decim::sample_f32(q),
        };
        while *next < p.inputs {
            // The output's first lane, from its newest input counted back
            // from the window's end.
            let a = (held - (p.inputs - 1 - *next) as usize - p.span) * lanes_per_sample;
            if p.pairs && *next + p.k < p.inputs {
                let ((i, q), (i2, q2)) = dot2(
                    &p.window[a..a + lanes],
                    &p.window[a + step..a + step + lanes],
                    p.kernel,
                );
                out.push(sample(i, q));
                out.push(sample(i2, q2));
                *next += 2 * p.k;
            } else {
                let (i, q) = dot(&p.window[a..a + lanes], p.kernel);
                out.push(sample(i, q));
                *next += p.k;
            }
        }
    }};
}

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "x86_64")]
pub mod avx2;
#[cfg(target_arch = "x86_64")]
pub mod avx512;
pub mod portable;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

use std::vec::Vec;

/// A block, in lanes: 16 f32 (or i32) lanes, eight samples. Windows and
/// kernels for [`dot`], [`dot2`], [`dot_i16`] and [`dot2_i16`] are a whole
/// number of blocks.
pub const BLOCK_LANES: usize = 16;
/// Lanes per sample: I and Q.
pub const LANES_PER_SAMPLE: usize = 2;
/// Lanes in one 128-bit register of f32 or i32 (NEON, SSE2), and samples
/// in it.
const VECTOR_LANES: usize = 4;
const VECTOR_SAMPLES: usize = VECTOR_LANES / LANES_PER_SAMPLE;

/// The instruction set the f32 products run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    /// Plain Rust ([`portable`]).
    Portable,
    /// NEON ([`aarch64`]).
    Neon,
    /// SSE2 ([`x86_64`]).
    Sse2,
    /// AVX2 and FMA ([`avx2`]).
    Avx2,
    /// AVX-512F ([`avx512`]).
    Avx512,
}

impl Isa {
    /// Whether this machine runs it: NEON on every aarch64, SSE2 on every
    /// x86-64, AVX2 + FMA and AVX-512F where the CPU reports them.
    pub fn is_available(self) -> bool {
        match self {
            Isa::Portable => true,
            Isa::Neon => cfg!(target_arch = "aarch64"),
            Isa::Sse2 => cfg!(target_arch = "x86_64"),
            #[cfg(target_arch = "x86_64")]
            Isa::Avx2 => {
                std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
            }
            #[cfg(target_arch = "x86_64")]
            Isa::Avx512 => std::is_x86_feature_detected!("avx512f"),
            #[cfg(not(target_arch = "x86_64"))]
            Isa::Avx2 | Isa::Avx512 => false,
        }
    }

    /// Every instruction set this machine runs, the plainest first.
    pub fn available() -> Vec<Isa> {
        [Isa::Portable, Isa::Neon, Isa::Sse2, Isa::Avx2, Isa::Avx512]
            .into_iter()
            .filter(|i| i.is_available())
            .collect()
    }

    /// The widest this machine runs.
    pub fn best() -> Isa {
        *Isa::available()
            .last()
            .expect("the portable form runs everywhere")
    }
}

fn check(x: &[impl Copy], h: &[impl Copy]) {
    assert!(
        x.len() == h.len() && x.len().is_multiple_of(BLOCK_LANES),
        "{} lanes against {}",
        x.len(),
        h.len()
    );
}

/// The I and Q sums of lanes I, Q, I, Q, …: the even lanes' and the odd
/// lanes', each from the first lane up.
#[inline]
fn split<T: core::ops::Add<Output = T> + Copy, const N: usize>(l: &[T; N]) -> (T, T) {
    let (mut i, mut q) = (l[0], l[1]);
    for p in l[LANES_PER_SAMPLE..].as_chunks::<LANES_PER_SAMPLE>().0 {
        i = i + p[0];
        q = q + p[1];
    }
    (i, q)
}

/// One `process` call's f32 work: the window (interleaved I/Q, its last
/// sample input `inputs` − 1), the taps oldest first (each twice), and
/// how outputs are made.
pub struct Pass<'a> {
    pub window: &'a [f32],
    pub kernel: &'a [f32],
    /// Samples each output reaches (the taps padded to whole blocks).
    pub span: usize,
    /// The decimation.
    pub k: u64,
    /// Two outputs per pass over the taps where both are complete.
    pub pairs: bool,
    /// Inputs taken so far.
    pub inputs: u64,
}

/// The outputs of `p` from output `next` on, onto `out`, on `isa`; `next`
/// is left at the first output not yet complete. `isa` must be available
/// here ([`Isa::is_available`]): the decimator takes only those.
pub fn outputs(isa: Isa, p: &Pass, next: &mut u64, out: &mut Vec<sdr_core::Iq>) {
    debug_assert!(isa.is_available(), "{isa:?} on a CPU without it");
    match isa {
        Isa::Portable => portable::outputs(p, next, out),
        #[cfg(target_arch = "aarch64")]
        Isa::Neon => aarch64::outputs(p, next, out),
        #[cfg(target_arch = "x86_64")]
        Isa::Sse2 => x86_64::outputs(p, next, out),
        // SAFETY: an available `Isa::Avx2` means the CPU reported AVX2 and
        // FMA.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx2 => unsafe { avx2::outputs(p, next, out) },
        // SAFETY: an available `Isa::Avx512` means the CPU reported
        // AVX-512F.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx512 => unsafe { avx512::outputs(p, next, out) },
        #[allow(unreachable_patterns)]
        _ => unreachable!("{isa:?} is not built for this target"),
    }
}

/// Σ x·h on `isa`, one product at a time (tests: the decimator runs
/// [`outputs`]).
#[cfg(test)]
pub fn dot(isa: Isa, x: &[f32], h: &[f32]) -> (f32, f32) {
    debug_assert!(isa.is_available(), "{isa:?} on a CPU without it");
    match isa {
        Isa::Portable => portable::dot(x, h),
        #[cfg(target_arch = "aarch64")]
        Isa::Neon => aarch64::dot(x, h),
        #[cfg(target_arch = "x86_64")]
        Isa::Sse2 => x86_64::dot(x, h),
        // SAFETY: an available `Isa::Avx2` means the CPU reported AVX2 and
        // FMA.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx2 => unsafe { avx2::dot(x, h) },
        // SAFETY: an available `Isa::Avx512` means the CPU reported
        // AVX-512F.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx512 => unsafe { avx512::dot(x, h) },
        #[allow(unreachable_patterns)]
        _ => unreachable!("{isa:?} is not built for this target"),
    }
}

/// [`dot`] for two windows from one pass over the taps (tests).
#[cfg(test)]
pub fn dot2(isa: Isa, xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    debug_assert!(isa.is_available(), "{isa:?} on a CPU without it");
    match isa {
        Isa::Portable => portable::dot2(xa, xb, h),
        #[cfg(target_arch = "aarch64")]
        Isa::Neon => aarch64::dot2(xa, xb, h),
        #[cfg(target_arch = "x86_64")]
        Isa::Sse2 => x86_64::dot2(xa, xb, h),
        // SAFETY: as in `dot`.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx2 => unsafe { avx2::dot2(xa, xb, h) },
        // SAFETY: as in `dot`.
        #[cfg(target_arch = "x86_64")]
        Isa::Avx512 => unsafe { avx512::dot2(xa, xb, h) },
        #[allow(unreachable_patterns)]
        _ => unreachable!("{isa:?} is not built for this target"),
    }
}

/// The folded form's sizes: `w` holds n = 2c + 1 samples, `hf` the first
/// c taps (oldest first), each twice.
fn folded_sizes(w: &[f32], hf: &[f32]) -> (usize, usize) {
    let (n, c) = (w.len() / LANES_PER_SAMPLE, hf.len() / LANES_PER_SAMPLE);
    assert!(
        w.len().is_multiple_of(LANES_PER_SAMPLE)
            && hf.len().is_multiple_of(LANES_PER_SAMPLE)
            && n == 2 * c + 1,
        "{n} samples for {c} folded taps"
    );
    (n, c)
}

/// The pairs from `from` to c one at a time, then the centre, onto the
/// lanes `l` (the pair's I and Q into lanes 0 and 1).
fn folded_tail(
    w: &[f32],
    hf: &[f32],
    hc: f32,
    n: usize,
    c: usize,
    from: usize,
    l: &mut [f32; VECTOR_LANES],
) {
    for s in from..c {
        let (f, b) = (s * LANES_PER_SAMPLE, (n - 1 - s) * LANES_PER_SAMPLE);
        l[0] += (w[f] + w[b]) * hf[f];
        l[1] += (w[f + 1] + w[b + 1]) * hf[f + 1];
    }
    l[0] += w[c * LANES_PER_SAMPLE] * hc;
    l[1] += w[c * LANES_PER_SAMPLE + 1] * hc;
}

#[cfg(target_arch = "aarch64")]
pub use aarch64::{dot_i16, dot2_i16, folded};
#[cfg(not(target_arch = "aarch64"))]
pub use portable::{dot_i16, dot2_i16, folded};
