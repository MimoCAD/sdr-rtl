//! AVX2 with FMA: 256-bit registers of eight lanes, two to a 16-lane
//! block. Four accumulators: even blocks into the first two, odd blocks
//! into the other two, fused multiply-adds; summed (a0 + a1) + (a2 + a3)
//! at the end, then in the registers the upper 128 bits onto the lower
//! and on as SSE2 ([`super::x86_64::split_m128`]). The blocks are taken
//! two at a time into four named accumulators, so they stay in registers
//! (indexed by block parity at run time they were kept on the stack,
//! findings 29.3). Only where the CPU reports AVX2 and FMA
//! (`Isa::is_available`): each function here is unsafe to call otherwise.

use super::x86_64::split_m128;
use super::{BLOCK_LANES, Pass, check};
use core::arch::x86_64::*;
use sdr_core::Iq;
use std::vec::Vec;

type Block = [f32; BLOCK_LANES];

/// Lanes in one 256-bit register of f32.
const LANES: usize = 8;

/// The decimator's f32 outputs for one pass, as [`super::outputs`], the
/// products below inlined into the loop.
///
/// # Safety
///
/// The CPU must have AVX2 and FMA (`Isa::Avx2.is_available()`).
#[target_feature(enable = "avx2,fma")]
pub fn outputs(p: &Pass, next: &mut u64, out: &mut Vec<Iq>) {
    f32_outputs!(p, next, out)
}

/// (a0 + a1) + (a2 + a3), the upper half onto the lower, then the I and
/// Q sums: all in the registers.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn sum(a0: __m256, a1: __m256, a2: __m256, a3: __m256) -> (f32, f32) {
    let v = _mm256_add_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(a2, a3));
    split_m128(_mm_add_ps(
        _mm256_castps256_ps128(v),
        _mm256_extractf128_ps::<1>(v),
    ))
}

/// A block's two halves.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn load(b: &Block) -> (__m256, __m256) {
    // SAFETY: the loads read the two eight-lane halves of one block.
    unsafe {
        (
            _mm256_loadu_ps(b.as_ptr()),
            _mm256_loadu_ps(b.as_ptr().add(LANES)),
        )
    }
}

/// One block's product onto a pair of accumulators.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn fma(lo: &mut __m256, hi: &mut __m256, x: &Block, h: &Block) {
    let ((x0, x1), (h0, h1)) = (load(x), load(h));
    *lo = _mm256_fmadd_ps(x0, h0, *lo);
    *hi = _mm256_fmadd_ps(x1, h1, *hi);
}

/// Two windows' blocks onto two pairs of accumulators, the taps loaded
/// once.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn fma2(
    a: (&mut __m256, &mut __m256),
    b: (&mut __m256, &mut __m256),
    x: &Block,
    y: &Block,
    h: &Block,
) {
    let ((x0, x1), (y0, y1), (h0, h1)) = (load(x), load(y), load(h));
    *a.0 = _mm256_fmadd_ps(x0, h0, *a.0);
    *a.1 = _mm256_fmadd_ps(x1, h1, *a.1);
    *b.0 = _mm256_fmadd_ps(y0, h0, *b.0);
    *b.1 = _mm256_fmadd_ps(y1, h1, *b.1);
}

/// Σ x·h, as [`super::dot`].
///
/// # Safety
///
/// The CPU must have AVX2 and FMA (`Isa::Avx2.is_available()`).
#[inline]
#[target_feature(enable = "avx2,fma")]
pub fn dot(x: &[f32], h: &[f32]) -> (f32, f32) {
    check(x, h);
    let (xb, hb) = (
        x.as_chunks::<BLOCK_LANES>().0,
        h.as_chunks::<BLOCK_LANES>().0,
    );
    let ((xp, xr), (hp, hr)) = (xb.as_chunks::<2>(), hb.as_chunks::<2>());
    let [mut a0, mut a1, mut a2, mut a3] = [_mm256_setzero_ps(); 4];
    for (x2, h2) in xp.iter().zip(hp) {
        fma(&mut a0, &mut a1, &x2[0], &h2[0]);
        fma(&mut a2, &mut a3, &x2[1], &h2[1]);
    }
    // An odd last block goes where an even block does.
    if let (Some(x), Some(h)) = (xr.first(), hr.first()) {
        fma(&mut a0, &mut a1, x, h);
    }
    sum(a0, a1, a2, a3)
}

/// Two windows from one pass over the taps, as [`super::dot2`].
///
/// # Safety
///
/// The CPU must have AVX2 and FMA (`Isa::Avx2.is_available()`).
#[inline]
#[target_feature(enable = "avx2,fma")]
pub fn dot2(xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    check(xa, h);
    check(xb, h);
    let (ab, bb, hb) = (
        xa.as_chunks::<BLOCK_LANES>().0,
        xb.as_chunks::<BLOCK_LANES>().0,
        h.as_chunks::<BLOCK_LANES>().0,
    );
    let ((ap, ar), (bp, br), (hp, hr)) = (
        ab.as_chunks::<2>(),
        bb.as_chunks::<2>(),
        hb.as_chunks::<2>(),
    );
    let [mut a0, mut a1, mut a2, mut a3] = [_mm256_setzero_ps(); 4];
    let [mut b0, mut b1, mut b2, mut b3] = [_mm256_setzero_ps(); 4];
    for ((x2, y2), h2) in ap.iter().zip(bp).zip(hp) {
        fma2(
            (&mut a0, &mut a1),
            (&mut b0, &mut b1),
            &x2[0],
            &y2[0],
            &h2[0],
        );
        fma2(
            (&mut a2, &mut a3),
            (&mut b2, &mut b3),
            &x2[1],
            &y2[1],
            &h2[1],
        );
    }
    if let (Some(x), Some(y), Some(h)) = (ar.first(), br.first(), hr.first()) {
        fma2((&mut a0, &mut a1), (&mut b0, &mut b1), x, y, h);
    }
    (sum(a0, a1, a2, a3), sum(b0, b1, b2, b3))
}
