//! SSE2 (in every x86-64 target, so no run-time check): four 128-bit
//! accumulators of four lanes per 16-lane block, as NEON, but a multiply
//! then an add (SSE2 has no fused multiply-add), summed (a0 + a1) +
//! (a2 + a3) at the end, then in the registers the high half onto the
//! low: [I0 + I1, Q0 + Q1].

use super::{BLOCK_LANES, Pass, VECTOR_LANES, check};
use core::arch::x86_64::*;
use sdr_core::Iq;
use std::vec::Vec;

/// The decimator's f32 outputs for one pass, as [`super::outputs`].
pub fn outputs(p: &Pass, next: &mut u64, out: &mut Vec<Iq>) {
    f32_outputs!(p, next, out)
}

/// (a0 + a1) + (a2 + a3).
#[inline]
fn sum4(a: [__m128; 4]) -> __m128 {
    // SAFETY: SSE2 is in the x86-64 baseline.
    unsafe { _mm_add_ps(_mm_add_ps(a[0], a[1]), _mm_add_ps(a[2], a[3])) }
}

/// [I0, Q0, I1, Q1] → (I0 + I1, Q0 + Q1), without leaving the registers:
/// the high pair moved onto the low and added, then lane 0 and lane 1.
/// Shared by the AVX2 and AVX-512 forms once they have folded down to
/// 128 bits.
#[inline]
pub(super) fn split_m128(v: __m128) -> (f32, f32) {
    // SAFETY: SSE2 is in the x86-64 baseline.
    unsafe {
        let p = _mm_add_ps(v, _mm_movehl_ps(v, v));
        (
            _mm_cvtss_f32(p),
            _mm_cvtss_f32(_mm_shuffle_ps::<0b01>(p, p)),
        )
    }
}

#[inline]
pub fn dot(x: &[f32], h: &[f32]) -> (f32, f32) {
    check(x, h);
    // SAFETY: SSE2 is in the x86-64 baseline; each load reads four f32
    // inside a 16-lane chunk of a checked slice.
    let a = unsafe {
        let mut a = [_mm_setzero_ps(); 4];
        for (xc, hc) in x
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(h.as_chunks::<BLOCK_LANES>().0)
        {
            let (xp, hp) = (xc.as_ptr(), hc.as_ptr());
            for (j, acc) in a.iter_mut().enumerate() {
                *acc = _mm_add_ps(
                    *acc,
                    _mm_mul_ps(
                        _mm_loadu_ps(xp.add(j * VECTOR_LANES)),
                        _mm_loadu_ps(hp.add(j * VECTOR_LANES)),
                    ),
                );
            }
        }
        a
    };
    split_m128(sum4(a))
}

#[inline]
pub fn dot2(xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    check(xa, h);
    check(xb, h);
    // SAFETY: as `dot`; each tap vector is loaded once for both.
    let (a, b) = unsafe {
        let (mut a, mut b) = ([_mm_setzero_ps(); 4], [_mm_setzero_ps(); 4]);
        let chunks = xa
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(xb.as_chunks::<BLOCK_LANES>().0)
            .zip(h.as_chunks::<BLOCK_LANES>().0);
        for ((ac, bc), hc) in chunks {
            let (ap, bp, hp) = (ac.as_ptr(), bc.as_ptr(), hc.as_ptr());
            for j in 0..4 {
                let t = _mm_loadu_ps(hp.add(j * VECTOR_LANES));
                a[j] = _mm_add_ps(a[j], _mm_mul_ps(_mm_loadu_ps(ap.add(j * VECTOR_LANES)), t));
                b[j] = _mm_add_ps(b[j], _mm_mul_ps(_mm_loadu_ps(bp.add(j * VECTOR_LANES)), t));
            }
        }
        (a, b)
    };
    (split_m128(sum4(a)), split_m128(sum4(b)))
}
