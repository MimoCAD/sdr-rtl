//! AVX-512F: 512-bit registers of sixteen lanes, one to a block. Four
//! accumulators, block b into accumulator b mod 4, fused multiply-adds;
//! summed (a0 + a1) + (a2 + a3) at the end, then in the registers the
//! upper 256 bits onto the lower, the upper 128 onto the lower, and on as
//! SSE2 ([`super::x86_64::split_m128`]). The blocks are taken four at a
//! time into four named accumulators, so they stay in registers (indexed
//! by b mod 4 at run time they were kept on the stack, findings 29.3).
//! Only AVX-512F instructions (the upper half taken as four f64, which
//! AVX-512F has, rather than eight f32, which needs AVX-512DQ). Only
//! where the CPU reports AVX-512F (`Isa::is_available`): each function
//! here is unsafe to call otherwise.

use super::x86_64::split_m128;
use super::{BLOCK_LANES, Pass, check};
use core::arch::x86_64::*;
use sdr_core::Iq;
use std::vec::Vec;

type Block = [f32; BLOCK_LANES];

/// The decimator's f32 outputs for one pass, as [`super::outputs`], the
/// products below inlined into the loop.
///
/// # Safety
///
/// The CPU must have AVX-512F (`Isa::Avx512.is_available()`).
#[target_feature(enable = "avx512f")]
pub fn outputs(p: &Pass, next: &mut u64, out: &mut Vec<Iq>) {
    f32_outputs!(p, next, out)
}

/// (a0 + a1) + (a2 + a3), folded 512 → 256 → 128 bits, then the I and Q
/// sums: all in the registers.
#[inline]
#[target_feature(enable = "avx512f")]
fn sum(a0: __m512, a1: __m512, a2: __m512, a3: __m512) -> (f32, f32) {
    let v = _mm512_add_ps(_mm512_add_ps(a0, a1), _mm512_add_ps(a2, a3));
    let upper = _mm256_castpd_ps(_mm512_extractf64x4_pd::<1>(_mm512_castps_pd(v)));
    let v = _mm256_add_ps(_mm512_castps512_ps256(v), upper);
    split_m128(_mm_add_ps(
        _mm256_castps256_ps128(v),
        _mm256_extractf128_ps::<1>(v),
    ))
}

#[inline]
#[target_feature(enable = "avx512f")]
fn load(b: &Block) -> __m512 {
    // SAFETY: the load reads one 16-lane block.
    unsafe { _mm512_loadu_ps(b.as_ptr()) }
}

/// Σ x·h, as [`super::dot`].
///
/// # Safety
///
/// The CPU must have AVX-512F (`Isa::Avx512.is_available()`).
#[inline]
#[target_feature(enable = "avx512f")]
pub fn dot(x: &[f32], h: &[f32]) -> (f32, f32) {
    check(x, h);
    let (xb, hb) = (
        x.as_chunks::<BLOCK_LANES>().0,
        h.as_chunks::<BLOCK_LANES>().0,
    );
    let ((xq, xr), (hq, hr)) = (xb.as_chunks::<4>(), hb.as_chunks::<4>());
    let [mut a0, mut a1, mut a2, mut a3] = [_mm512_setzero_ps(); 4];
    for (x4, h4) in xq.iter().zip(hq) {
        a0 = _mm512_fmadd_ps(load(&x4[0]), load(&h4[0]), a0);
        a1 = _mm512_fmadd_ps(load(&x4[1]), load(&h4[1]), a1);
        a2 = _mm512_fmadd_ps(load(&x4[2]), load(&h4[2]), a2);
        a3 = _mm512_fmadd_ps(load(&x4[3]), load(&h4[3]), a3);
    }
    // The last 0-3 blocks, into the accumulators in order.
    if let (Some(x), Some(h)) = (xr.first(), hr.first()) {
        a0 = _mm512_fmadd_ps(load(x), load(h), a0);
    }
    if let (Some(x), Some(h)) = (xr.get(1), hr.get(1)) {
        a1 = _mm512_fmadd_ps(load(x), load(h), a1);
    }
    if let (Some(x), Some(h)) = (xr.get(2), hr.get(2)) {
        a2 = _mm512_fmadd_ps(load(x), load(h), a2);
    }
    sum(a0, a1, a2, a3)
}

/// Two windows from one pass over the taps, as [`super::dot2`].
///
/// # Safety
///
/// The CPU must have AVX-512F (`Isa::Avx512.is_available()`).
#[inline]
#[target_feature(enable = "avx512f")]
pub fn dot2(xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    check(xa, h);
    check(xb, h);
    let (ab, bb, hb) = (
        xa.as_chunks::<BLOCK_LANES>().0,
        xb.as_chunks::<BLOCK_LANES>().0,
        h.as_chunks::<BLOCK_LANES>().0,
    );
    let ((aq, ar), (bq, br), (hq, hr)) = (
        ab.as_chunks::<4>(),
        bb.as_chunks::<4>(),
        hb.as_chunks::<4>(),
    );
    let [mut a0, mut a1, mut a2, mut a3] = [_mm512_setzero_ps(); 4];
    let [mut b0, mut b1, mut b2, mut b3] = [_mm512_setzero_ps(); 4];
    for ((a4, b4), h4) in aq.iter().zip(bq).zip(hq) {
        let t = load(&h4[0]);
        a0 = _mm512_fmadd_ps(load(&a4[0]), t, a0);
        b0 = _mm512_fmadd_ps(load(&b4[0]), t, b0);
        let t = load(&h4[1]);
        a1 = _mm512_fmadd_ps(load(&a4[1]), t, a1);
        b1 = _mm512_fmadd_ps(load(&b4[1]), t, b1);
        let t = load(&h4[2]);
        a2 = _mm512_fmadd_ps(load(&a4[2]), t, a2);
        b2 = _mm512_fmadd_ps(load(&b4[2]), t, b2);
        let t = load(&h4[3]);
        a3 = _mm512_fmadd_ps(load(&a4[3]), t, a3);
        b3 = _mm512_fmadd_ps(load(&b4[3]), t, b3);
    }
    // The last 0-3 blocks, into the accumulators in order.
    if let (Some(x), Some(y), Some(h)) = (ar.first(), br.first(), hr.first()) {
        let t = load(h);
        a0 = _mm512_fmadd_ps(load(x), t, a0);
        b0 = _mm512_fmadd_ps(load(y), t, b0);
    }
    if let (Some(x), Some(y), Some(h)) = (ar.get(1), br.get(1), hr.get(1)) {
        let t = load(h);
        a1 = _mm512_fmadd_ps(load(x), t, a1);
        b1 = _mm512_fmadd_ps(load(y), t, b1);
    }
    if let (Some(x), Some(y), Some(h)) = (ar.get(2), br.get(2), hr.get(2)) {
        let t = load(h);
        a2 = _mm512_fmadd_ps(load(x), t, a2);
        b2 = _mm512_fmadd_ps(load(y), t, b2);
    }
    (sum(a0, a1, a2, a3), sum(b0, b1, b2, b3))
}
