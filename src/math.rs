//! `floor` and `round` for `no_std` (the `f64` methods are std's), exact:
//! the same value as std's for every finite value below 2^52 in
//! magnitude (a zero's sign aside), the range the synthesizer's and the
//! resampler's arithmetic stays far inside.

/// The integer part, toward zero (exact below 2^52: the cast drops only
/// the fraction).
fn trunc(x: f64) -> f64 {
    x as i64 as f64
}

/// The largest integer not above `x`.
pub(crate) fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if x < t { t - 1.0 } else { t }
}

/// The nearest integer, half away from zero (as `f64::round`). `x − t` is
/// exact for `t = trunc(x)`, so the comparison with one half is too.
pub(crate) fn round(x: f64) -> f64 {
    let t = trunc(x);
    let frac = x - t;
    if frac >= 0.5 {
        t + 1.0
    } else if frac <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn they_are_stds() {
        let mut v = vec![0.0, -0.0, 0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 0.49999999999999994, -0.49999999999999994, 4503599627370495.5, -4503599627370495.5];
        // The synthesizer's and the resampler's values: N in 13..130 with
        // fractions, ratios up to ~2^29.
        let mut x = -1.0e9;
        while x < 1.0e9 {
            v.push(x);
            x += 1_234.567_891_234_5;
        }
        for k in 0..100_000 {
            v.push(13.0 + k as f64 * 0.001_171_875_3);
        }
        for x in v {
            // Values, not bits: −0.0 floors to +0.0 here (std keeps the
            // sign), the same integer to every caller.
            assert_eq!(floor(x), x.floor(), "floor {x}");
            assert_eq!(round(x), x.round(), "round {x}");
        }
    }
}
