//! Plain Rust: sixteen independent lane sums per block, which the compiler
//! keeps in vector registers where it can, summed as the 128-bit forms sum
//! their four registers. The reference the others are tested against, and
//! the form on targets with none of its own.

use super::{
    BLOCK_LANES, LANES_PER_SAMPLE, Pass, VECTOR_LANES, VECTOR_SAMPLES, check, folded_sizes,
    folded_tail, split,
};
use sdr_core::Iq;
use std::vec::Vec;

/// The decimator's f32 outputs for one pass, as [`super::outputs`].
pub fn outputs(p: &Pass, next: &mut u64, out: &mut Vec<Iq>) {
    f32_outputs!(p, next, out)
}

/// One accumulator lane per block lane, summed as NEON and SSE2 sum their
/// four registers: (a0 + a1) + (a2 + a3), lane by lane.
fn reduce<T: core::ops::Add<Output = T> + Copy + Default>(
    acc: &[T; BLOCK_LANES],
) -> [T; VECTOR_LANES] {
    let mut l = [T::default(); VECTOR_LANES];
    for (j, v) in l.iter_mut().enumerate() {
        *v = (acc[j] + acc[VECTOR_LANES + j])
            + (acc[2 * VECTOR_LANES + j] + acc[3 * VECTOR_LANES + j]);
    }
    l
}

pub fn dot(x: &[f32], h: &[f32]) -> (f32, f32) {
    check(x, h);
    let mut acc = [0.0f32; BLOCK_LANES];
    for (xc, hc) in x
        .as_chunks::<BLOCK_LANES>()
        .0
        .iter()
        .zip(h.as_chunks::<BLOCK_LANES>().0)
    {
        for l in 0..BLOCK_LANES {
            acc[l] += xc[l] * hc[l];
        }
    }
    split(&reduce(&acc))
}

pub fn dot2(xa: &[f32], xb: &[f32], h: &[f32]) -> ((f32, f32), (f32, f32)) {
    (dot(xa, h), dot(xb, h))
}

pub fn folded(w: &[f32], hf: &[f32], hc: f32) -> (f32, f32) {
    let (n, c) = folded_sizes(w, hf);
    let steps = c / VECTOR_SAMPLES;
    let mut acc = [0.0f32; BLOCK_LANES];
    for t in 0..steps {
        let s = t * VECTOR_SAMPLES;
        let r = (t % 4) * VECTOR_LANES;
        for v in 0..VECTOR_SAMPLES {
            for iq in 0..LANES_PER_SAMPLE {
                let f = (s + v) * LANES_PER_SAMPLE + iq;
                let b = (n - 1 - s - v) * LANES_PER_SAMPLE + iq;
                acc[r + v * LANES_PER_SAMPLE + iq] += (w[f] + w[b]) * hf[f];
            }
        }
    }
    let mut l = reduce(&acc);
    folded_tail(w, hf, hc, n, c, steps * VECTOR_SAMPLES, &mut l);
    split(&l)
}

pub fn dot_i16(x: &[i16], h: &[i16]) -> (i32, i32) {
    check(x, h);
    let mut acc = [0i32; BLOCK_LANES];
    for (xc, hc) in x
        .as_chunks::<BLOCK_LANES>()
        .0
        .iter()
        .zip(h.as_chunks::<BLOCK_LANES>().0)
    {
        for l in 0..BLOCK_LANES {
            acc[l] += xc[l] as i32 * hc[l] as i32;
        }
    }
    split(&reduce(&acc))
}

pub fn dot2_i16(xa: &[i16], xb: &[i16], h: &[i16]) -> ((i32, i32), (i32, i32)) {
    (dot_i16(xa, h), dot_i16(xb, h))
}
