//! NEON (in every aarch64 target, so no run-time check): four 128-bit
//! accumulators of four lanes per 16-lane block, fused multiply-adds,
//! summed (a0 + a1) + (a2 + a3) at the end, then in the registers the
//! high half onto the low: [I0 + I1, Q0 + Q1].

use super::{
    BLOCK_LANES, LANES_PER_SAMPLE, Pass, VECTOR_LANES, VECTOR_SAMPLES, check, folded_sizes,
    folded_tail, split,
};
use core::arch::aarch64::*;
use sdr_core::Iq;
use std::vec::Vec;

/// The decimator's f32 outputs for one pass, as [`super::outputs`].
pub fn outputs(p: &Pass, next: &mut u64, out: &mut Vec<Iq>) {
    f32_outputs!(p, next, out)
}

/// (a0 + a1) + (a2 + a3).
#[inline]
fn sum4_f32(a: [float32x4_t; 4]) -> float32x4_t {
    // SAFETY: NEON is in the aarch64 baseline.
    unsafe { vaddq_f32(vaddq_f32(a[0], a[1]), vaddq_f32(a[2], a[3])) }
}

/// [I0, Q0, I1, Q1] → (I0 + I1, Q0 + Q1), without leaving the registers.
#[inline]
fn split_f32(v: float32x4_t) -> (f32, f32) {
    // SAFETY: NEON is in the aarch64 baseline.
    unsafe {
        let p = vadd_f32(vget_low_f32(v), vget_high_f32(v));
        (vget_lane_f32::<0>(p), vget_lane_f32::<1>(p))
    }
}

/// [`split_f32`] for 32-bit integers, after (a0 + a1) + (a2 + a3).
#[inline]
fn split_i32(a: [int32x4_t; 4]) -> (i32, i32) {
    // SAFETY: NEON is in the aarch64 baseline.
    unsafe {
        let v = vaddq_s32(vaddq_s32(a[0], a[1]), vaddq_s32(a[2], a[3]));
        let p = vadd_s32(vget_low_s32(v), vget_high_s32(v));
        (vget_lane_s32::<0>(p), vget_lane_s32::<1>(p))
    }
}

/// The lanes, for the folded form's scalar tail.
fn lanes_f32(v: float32x4_t) -> [f32; VECTOR_LANES] {
    let mut l = [0.0; VECTOR_LANES];
    // SAFETY: NEON is in the aarch64 baseline; the store writes four f32
    // into an array of four.
    unsafe { vst1q_f32(l.as_mut_ptr(), v) };
    l
}

#[inline]
pub fn dot(x: &[f32], h: &[f32]) -> (f32, f32) {
    check(x, h);
    // SAFETY: NEON is in the aarch64 baseline; every load reads four f32
    // inside a 16-lane chunk of a checked slice.
    unsafe {
        let mut a = [vdupq_n_f32(0.0); 4];
        for (xc, hc) in x
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(h.as_chunks::<BLOCK_LANES>().0)
        {
            let (xp, hp) = (xc.as_ptr(), hc.as_ptr());
            for (j, acc) in a.iter_mut().enumerate() {
                *acc = vfmaq_f32(
                    *acc,
                    vld1q_f32(xp.add(j * VECTOR_LANES)),
                    vld1q_f32(hp.add(j * VECTOR_LANES)),
                );
            }
        }
        split_f32(sum4_f32(a))
    }
}

#[inline]
pub fn dot2(xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    check(xa, h);
    check(xb, h);
    // SAFETY: as `dot`; each tap vector is loaded once for both.
    unsafe {
        let (mut a, mut b) = ([vdupq_n_f32(0.0); 4], [vdupq_n_f32(0.0); 4]);
        let chunks = xa
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(xb.as_chunks::<BLOCK_LANES>().0)
            .zip(h.as_chunks::<BLOCK_LANES>().0);
        for ((ac, bc), hc) in chunks {
            let (ap, bp, hp) = (ac.as_ptr(), bc.as_ptr(), hc.as_ptr());
            for j in 0..4 {
                let t = vld1q_f32(hp.add(j * VECTOR_LANES));
                a[j] = vfmaq_f32(a[j], vld1q_f32(ap.add(j * VECTOR_LANES)), t);
                b[j] = vfmaq_f32(b[j], vld1q_f32(bp.add(j * VECTOR_LANES)), t);
            }
        }
        (split_f32(sum4_f32(a)), split_f32(sum4_f32(b)))
    }
}

pub fn folded(w: &[f32], hf: &[f32], hc: f32) -> (f32, f32) {
    let (n, c) = folded_sizes(w, hf);
    let steps = c / VECTOR_SAMPLES;
    // SAFETY: the forward load at pair s reads samples s, s + 1 (s + 1 <
    // c); the backward one n − 2 − s, n − 1 − s (≤ n − 1): all inside
    // `w`; the taps' load lanes 2s .. 2s + 4 ≤ 2c of `hf`.
    let mut l = unsafe {
        let (wp, hp) = (w.as_ptr(), hf.as_ptr());
        let mut a = [vdupq_n_f32(0.0); 4];
        for t in 0..steps {
            let s = t * VECTOR_SAMPLES;
            let fwd = vld1q_f32(wp.add(s * LANES_PER_SAMPLE));
            let back = vld1q_f32(wp.add((n - 2 - s) * LANES_PER_SAMPLE));
            // [n − 2 − s, n − 1 − s] → [n − 1 − s, n − 2 − s].
            let rev = vextq_f32::<2>(back, back);
            a[t % 4] = vfmaq_f32(
                a[t % 4],
                vaddq_f32(fwd, rev),
                vld1q_f32(hp.add(s * LANES_PER_SAMPLE)),
            );
        }
        lanes_f32(sum4_f32(a))
    };
    folded_tail(w, hf, hc, n, c, steps * VECTOR_SAMPLES, &mut l);
    split(&l)
}

pub fn dot_i16(x: &[i16], h: &[i16]) -> (i32, i32) {
    check(x, h);
    // SAFETY: every load reads eight i16 inside a 16-lane chunk.
    unsafe {
        let mut a = [vdupq_n_s32(0); 4];
        for (xc, hc) in x
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(h.as_chunks::<BLOCK_LANES>().0)
        {
            let (x0, x1) = (vld1q_s16(xc.as_ptr()), vld1q_s16(xc.as_ptr().add(8)));
            let (h0, h1) = (vld1q_s16(hc.as_ptr()), vld1q_s16(hc.as_ptr().add(8)));
            a[0] = vmlal_s16(a[0], vget_low_s16(x0), vget_low_s16(h0));
            a[1] = vmlal_high_s16(a[1], x0, h0);
            a[2] = vmlal_s16(a[2], vget_low_s16(x1), vget_low_s16(h1));
            a[3] = vmlal_high_s16(a[3], x1, h1);
        }
        split_i32(a)
    }
}

pub fn dot2_i16(xa: &[i16], xb: &[i16], h: &[i16]) -> ((i32, i32), (i32, i32)) {
    check(xa, h);
    check(xb, h);
    // SAFETY: as `dot_i16`.
    unsafe {
        let (mut a, mut b) = ([vdupq_n_s32(0); 4], [vdupq_n_s32(0); 4]);
        let chunks = xa
            .as_chunks::<BLOCK_LANES>()
            .0
            .iter()
            .zip(xb.as_chunks::<BLOCK_LANES>().0)
            .zip(h.as_chunks::<BLOCK_LANES>().0);
        for ((ac, bc), hc) in chunks {
            let (h0, h1) = (vld1q_s16(hc.as_ptr()), vld1q_s16(hc.as_ptr().add(8)));
            let (a0, a1) = (vld1q_s16(ac.as_ptr()), vld1q_s16(ac.as_ptr().add(8)));
            let (b0, b1) = (vld1q_s16(bc.as_ptr()), vld1q_s16(bc.as_ptr().add(8)));
            a[0] = vmlal_s16(a[0], vget_low_s16(a0), vget_low_s16(h0));
            a[1] = vmlal_high_s16(a[1], a0, h0);
            a[2] = vmlal_s16(a[2], vget_low_s16(a1), vget_low_s16(h1));
            a[3] = vmlal_high_s16(a[3], a1, h1);
            b[0] = vmlal_s16(b[0], vget_low_s16(b0), vget_low_s16(h0));
            b[1] = vmlal_high_s16(b[1], b0, h0);
            b[2] = vmlal_s16(b[2], vget_low_s16(b1), vget_low_s16(h1));
            b[3] = vmlal_high_s16(b[3], b1, h1);
        }
        (split_i32(a), split_i32(b))
    }
}
