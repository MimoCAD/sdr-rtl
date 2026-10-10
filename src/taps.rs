//! The demodulator's 20 tap registers, page 1 0x1C-0x2F: their layout
//! (findings 10.3) and this driver's own low-pass design to fill them.
//!
//! No document names these registers. What the bench established:
//! - **Layout**, from bit weights against the tuner's noise
//!   (`data/step10-3a-coeff-probe.json`): c0-c7 are 8-bit fields at
//!   0x1C-0x23; c8-c15 are 12-bit fields, two per three bytes, MSB first:
//!   c(8+2p) = byte0 << 4 | byte1 >> 4, c(9+2p) = (byte1 & 0x0F) << 8 |
//!   byte2. Both widths share one LSB weight.
//! - **Signs**: two's complement in each width (c15 = +1024 with c8 =
//!   0xC00 cancels within ±0.2 MHz of the centre, so 0xC00 reads as
//!   −1024; `data/step10-3b-coeff-response.json`).
//! - **Role**: needed for content, not for the stream (all zero gives a
//!   constant 0x80 output); our design, the observed set and the
//!   power-on set carry a tone alike (`data/step10-3c-coeffs.json`).
//! - **What they filter** (findings 12.4): a 32-tap symmetric FIR at
//!   f_xtal, field k setting taps k and 31 − k, ahead of a fixed chain —
//!   the assumption this design was made under, confirmed. Seven
//!   clean-room alternatives were benched against it; none beat it on both
//!   aliases and flatness (findings 12.7), so it stays.
//!
//! The values the vendor driver was seen to write (findings 9b item 13)
//! are not in this driver, not even as a test vector: the design below
//! is computed here from its stated parameters, and the test compares it
//! with the taps findings 10.3 records for that design.

use alloc::format;
#[cfg(feature = "std")]
use alloc::vec::Vec;

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::tables::pll::XTAL_HZ;

pub use crate::protocol::{TAP_FIRST, TAP_PAGE};

/// The register count, 0x1C-0x2F (findings 10.3).
pub const TAP_REGS: usize = 20;
/// The tap fields the registers hold.
pub const TAPS: usize = 16;
/// c0-c7 are 8-bit, c8-c15 12-bit (findings 10.3).
pub const NARROW_TAPS: usize = 8;
pub const NARROW_BITS: u32 = 8;
pub const WIDE_BITS: u32 = 12;
/// Two 12-bit fields per three bytes (findings 10.3).
pub const WIDE_GROUP_BYTES: usize = 3;
/// The four bits byte 1 of a group splits between its two fields.
const NIBBLE: u32 = 4;
const NIBBLE_MASK: u32 = (1 << NIBBLE) - 1;

/// The range a field of `bits` holds in two's complement.
pub const fn field_range(bits: u32) -> (i32, i32) {
    (-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
}

/// The width of field `k`.
pub const fn field_bits(k: usize) -> u32 {
    if k < NARROW_TAPS { NARROW_BITS } else { WIDE_BITS }
}

// ---- Our own low-pass (findings 10.3, "Our own low-pass") ----
//
// A Hamming-windowed sinc, ported from the bench's `own_lowpass_taps`
// (`examples/harness/stream_confirm.rs`), with the design's parameters as
// findings 10.3 states them. The ASSUMPTIONS are the bench's and remain
// unconfirmed (findings 10.3: "The bench neither confirmed nor refuted
// that assumption"): the taps run at the ADC rate, the filter length is
// even (no single centre tap), the 16 fields hold one half of a
// symmetric filter from the outermost tap (c0) to the one beside the
// centre (c15).

/// The rate the design assumes the taps run at: the ADC's 28.8 MHz
/// ([RTL2832U] p. 10).
pub const DESIGN_FS_HZ: f64 = XTAL_HZ;
/// The cutoff: passes the 2.048 Msps output's ±1.024 MHz with margin.
pub const DESIGN_CUTOFF_HZ: f64 = 1.2e6;
/// 32 taps, symmetric: the 16 fields are one half.
pub const DESIGN_LENGTH: usize = 2 * TAPS;
/// The sum of all 32 taps (a power of two of the design's choosing).
pub const DESIGN_DC_GAIN: f64 = 4096.0;
/// The Hamming window w(n) = A − B·cos(2πn / (N − 1)).
pub const HAMMING_A: f64 = 0.54;
pub const HAMMING_B: f64 = 0.46;

/// The design's 16 stored taps, c0 (outermost) to c15 (beside the
/// centre), as [`compute_design`] computes them (findings 10.3 records
/// the same values). A constant so the `no_std` core needs no sine; the
/// computation is the definition, and a test holds the two equal.
pub const DESIGN: [i32; TAPS] = [-6, -5, -5, -2, 3, 15, 34, 61, 96, 138, 185, 233, 279, 317, 345, 360];

/// The design: h(n) = 2fc · sinc(2fc · (n − (N−1)/2)) · w(n) for
/// n = 0..15, fc = cutoff / fs, scaled so the full symmetric filter sums
/// to the DC gain, rounded to integers.
#[cfg(feature = "std")]
pub fn compute_design() -> [i32; TAPS] {
    let x = 2.0 * DESIGN_CUTOFF_HZ / DESIGN_FS_HZ;
    let centre = (DESIGN_LENGTH as f64 - 1.0) / 2.0;
    let half: Vec<f64> = (0..TAPS)
        .map(|n| {
            let t = core::f64::consts::PI * x * (centre - n as f64);
            let sinc = if t == 0.0 { 1.0 } else { t.sin() / t };
            let w = HAMMING_A - HAMMING_B * (core::f64::consts::TAU * n as f64 / (DESIGN_LENGTH as f64 - 1.0)).cos();
            x * sinc * w
        })
        .collect();
    let sum = 2.0 * half.iter().sum::<f64>();
    let mut c = [0i32; TAPS];
    for (k, h) in half.iter().enumerate() {
        c[k] = (h / sum * DESIGN_DC_GAIN).round() as i32;
    }
    c
}

/// The design's 16 taps ([`DESIGN`]).
pub fn design() -> [i32; TAPS] {
    DESIGN
}

/// The design in the register layout.
pub fn design_registers() -> [u8; TAP_REGS] {
    pack(&design()).expect("the design fits its fields (tested)")
}

/// 16 taps into the 20 registers (findings 10.3's layout), refusing a
/// tap its field cannot hold.
pub fn pack(c: &[i32; TAPS]) -> Result<[u8; TAP_REGS]> {
    for (k, &v) in c.iter().enumerate() {
        let (lo, hi) = field_range(field_bits(k));
        if v < lo || v > hi {
            return Err(Error::Unsupported {
                backend: BACKEND,
                what: format!("tap c{k} = {v} does not fit its {}-bit field ({lo}..={hi})", field_bits(k)),
            });
        }
    }
    let mut out = [0u8; TAP_REGS];
    for k in 0..NARROW_TAPS {
        out[k] = c[k] as i8 as u8;
    }
    let wide_mask = (1u32 << WIDE_BITS) - 1;
    for p in 0..(TAPS - NARROW_TAPS) / 2 {
        let a = c[NARROW_TAPS + 2 * p] as u32 & wide_mask;
        let b = c[NARROW_TAPS + 2 * p + 1] as u32 & wide_mask;
        let o = NARROW_TAPS + WIDE_GROUP_BYTES * p;
        out[o] = (a >> NIBBLE) as u8;
        out[o + 1] = ((a & NIBBLE_MASK) << NIBBLE | b >> u8::BITS) as u8;
        out[o + 2] = b as u8;
    }
    Ok(out)
}

/// The 20 registers back into 16 signed taps (the inverse of [`pack`]).
pub fn unpack(r: &[u8; TAP_REGS]) -> [i32; TAPS] {
    let mut c = [0i32; TAPS];
    for k in 0..NARROW_TAPS {
        c[k] = r[k] as i8 as i32;
    }
    let sign = 1u32 << (WIDE_BITS - 1);
    let s12 = |v: u32| if v & sign != 0 { v as i32 - (1 << WIDE_BITS) } else { v as i32 };
    for p in 0..(TAPS - NARROW_TAPS) / 2 {
        let o = NARROW_TAPS + WIDE_GROUP_BYTES * p;
        c[NARROW_TAPS + 2 * p] = s12((r[o] as u32) << NIBBLE | (r[o + 1] as u32) >> NIBBLE);
        c[NARROW_TAPS + 2 * p + 1] = s12(((r[o + 1] as u32) & NIBBLE_MASK) << u8::BITS | r[o + 2] as u32);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Findings 10.3: "Taps c0–c15: −6 −5 −5 −2 3 15 34 61 96 138 185 233
    /// 279 317 345 360."
    #[test]
    fn the_design_is_findings_10s() {
        assert_eq!(design(), [-6, -5, -5, -2, 3, 15, 34, 61, 96, 138, 185, 233, 279, 317, 345, 360]);
    }

    /// The constant is what the design computes.
    #[cfg(feature = "std")]
    #[test]
    fn the_constant_is_the_computation() {
        assert_eq!(compute_design(), DESIGN);
    }

    #[test]
    fn the_design_is_a_low_pass_with_its_dc_gain() {
        let c = design();
        let full: i32 = 2 * c.iter().sum::<i32>();
        assert!((full - DESIGN_DC_GAIN as i32).abs() <= TAPS as i32, "{full}");
        assert!(c.windows(2).skip(3).all(|w| w[1] > w[0]), "rising toward the centre: {c:?}");
        assert_eq!(unpack(&design_registers()), c);
    }

    #[test]
    fn the_registers_span_0x1c_to_0x2f() {
        assert_eq!((TAP_PAGE, TAP_FIRST), (1, 0x1C));
        assert_eq!(TAP_FIRST as usize + TAP_REGS - 1, 0x2F);
        assert_eq!(NARROW_TAPS + (TAPS - NARROW_TAPS) / 2 * WIDE_GROUP_BYTES, TAP_REGS);
    }

    /// The layout's arithmetic, by hand: c8 = 0xABC and c9 = 0x123 give
    /// AB C1 23; an 8-bit −1 is FF.
    #[test]
    fn packing_places_the_fields_msb_first() {
        let mut c = [0i32; TAPS];
        c[0] = -1;
        c[7] = 127;
        c[8] = 0xABC - 0x1000;
        c[9] = 0x123;
        let r = pack(&c).unwrap();
        assert_eq!(r[0], 0xFF);
        assert_eq!(r[7], 0x7F);
        assert_eq!(&r[8..11], &[0xAB, 0xC1, 0x23]);
        assert!(r[11..].iter().all(|&b| b == 0));
    }

    /// Findings 10.3's sign evidence: the 12-bit pattern 0xC00 is −1024.
    #[test]
    fn twelve_bit_fields_are_twos_complement() {
        let mut r = [0u8; TAP_REGS];
        // c8 = byte 8 << 4 | byte 9 >> 4 = 0xC00.
        r[8] = 0xC0;
        // c15 = (byte 18 & 0x0F) << 8 | byte 19 = 0x400.
        r[18] = 0x04;
        let c = unpack(&r);
        assert_eq!(c[8], -1024);
        assert_eq!(c[15], 1024);
        assert_eq!(pack(&c).unwrap(), r);
    }

    /// Every value of every field round-trips; the ends of each range
    /// pack, one past them refuses.
    #[test]
    fn packing_round_trips_every_field_value() {
        for k in 0..TAPS {
            let (lo, hi) = field_range(field_bits(k));
            for v in lo..=hi {
                let mut c = [0i32; TAPS];
                c[k] = v;
                // A neighbour sharing the byte stays put.
                let n = if k % 2 == 0 { k + 1 } else { k - 1 };
                let (nlo, nhi) = field_range(field_bits(n));
                c[n] = if v % 2 == 0 { nlo } else { nhi };
                let r = pack(&c).unwrap();
                assert_eq!(unpack(&r), c, "c{k} = {v}");
            }
            let mut c = [0i32; TAPS];
            c[k] = hi + 1;
            assert!(pack(&c).is_err(), "c{k} = {}", hi + 1);
            c[k] = lo - 1;
            assert!(pack(&c).is_err(), "c{k} = {}", lo - 1);
        }
        assert_eq!(field_range(NARROW_BITS), (-128, 127));
        assert_eq!(field_range(WIDE_BITS), (-2048, 2047));
    }

    /// Every register image unpacks and packs back to itself (the layout
    /// is a bijection on the 20 bytes).
    #[test]
    fn unpacking_round_trips_register_images() {
        let mut x: u32 = 0x2545_F491;
        for _ in 0..2000 {
            let mut r = [0u8; TAP_REGS];
            for b in r.iter_mut() {
                // xorshift32: a deterministic spread of byte values.
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            assert_eq!(pack(&unpack(&r)).unwrap(), r);
        }
    }
}
