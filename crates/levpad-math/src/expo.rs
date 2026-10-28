//! Fixed-point log2 and exp2 in Q64.64 (the loss receipts' redeem delay, tilt-spot `receipt.rs`).
//!
//! `log2_q64(x)` takes x > 0 as Q64.64 and returns log2(x) as a signed Q64.64 (i128). `exp2_q64(y)`
//! takes a signed Q64.64 y and returns 2^y as Q64.64, or None when it does not fit. Integers only:
//! the integer part from the highest set bit, the fraction of log2 by repeated squaring (one bit per
//! step, 48 steps), 2^f for f in [0, 1) by the series of e^(f ln 2). Both agree with f64 to about
//! 1e-12 relative over the ranges the delay uses (tests below).

use crate::Q64;

/// ln 2 in Q64.64.
const LN2_Q64: u128 = 12_786_308_645_202_655_660;
/// Fraction bits computed by `log2_q64` (the rest are zero).
const LOG2_BITS: u32 = 48;

/// (a x b) >> 64 for values under 2^66 (no overflow for the operands used here).
fn mul64(a: u128, b: u128) -> u128 {
    let (ah, al) = (a >> 64, a & (Q64 - 1));
    let (bh, bl) = (b >> 64, b & (Q64 - 1));
    ((ah * bh) << 64) + ah * bl + al * bh + ((al * bl) >> 64)
}

/// log2(x) for x > 0 in Q64.64; None for x = 0.
pub fn log2_q64(x: u128) -> Option<i128> {
    if x == 0 {
        return None;
    }
    let msb = 127 - x.leading_zeros() as i32;
    let int = msb - 64;
    // y = x / 2^int in [1, 2), Q64.64
    let mut y = if int >= 0 { x >> int } else { x << (-int) };
    let mut frac: u128 = 0;
    for i in 1..=LOG2_BITS {
        y = mul64(y, y);
        if y >= 2 * Q64 {
            y >>= 1;
            frac |= 1u128 << (64 - i);
        }
    }
    Some(((int as i128) << 64) + frac as i128)
}

/// 2^y in Q64.64 for a signed Q64.64 y; None when the result is 2^63 or more.
pub fn exp2_q64(y: i128) -> Option<u128> {
    let int = y >> 64; // floor
    let frac = (y - (int << 64)) as u128; // [0, 1) in Q64.64
    if int >= 63 {
        return None;
    }
    // 2^frac = e^(frac ln 2) = sum z^k / k!
    let z = mul64(frac, LN2_Q64);
    let mut term = Q64;
    let mut sum = Q64;
    for k in 1..=30u128 {
        term = mul64(term, z) / k;
        if term == 0 {
            break;
        }
        sum += term;
    }
    Some(if int >= 0 { sum << int } else if int > -128 { sum >> (-int) } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(x: f64) -> u128 {
        (x * 18446744073709551616.0) as u128
    }
    fn f(x: i128) -> f64 {
        x as f64 / 18446744073709551616.0
    }

    #[test]
    fn log2_matches_f64() {
        for x in [1.0f64, 1.5, 2.0, 3.0, 0.5, 0.25, 0.001, 7.3, 1000.0, 1e6, 1e12, 1.0000001, 0.9999999, 123456.789] {
            let got = f(log2_q64(q(x)).unwrap());
            let want = x.log2();
            assert!((got - want).abs() < 1e-11, "log2({x}) = {got}, want {want}");
        }
        assert_eq!(log2_q64(0), None);
        assert_eq!(log2_q64(Q64), Some(0));
        assert_eq!(log2_q64(4 * Q64), Some(2 << 64));
    }

    #[test]
    fn exp2_matches_f64() {
        for y in [0.0f64, 0.5, 1.0, 1.25, 2.5, 5.584962500721156, -0.5, -3.0, 10.0, 0.0001, 0.9999] {
            let got = exp2_q64((y * 18446744073709551616.0) as i128).unwrap() as f64 / 18446744073709551616.0;
            let want = y.exp2();
            assert!((got - want).abs() / want < 1e-12, "2^{y} = {got}, want {want}");
        }
        assert_eq!(exp2_q64(0), Some(Q64));
        assert_eq!(exp2_q64(3 << 64), Some(8 * Q64));
        assert_eq!(exp2_q64(63 << 64), None);
    }

    #[test]
    fn round_trip() {
        for x in [1.0f64, 2.0, 3.7, 48.0, 0.3] {
            let l = log2_q64(q(x)).unwrap();
            let back = exp2_q64(l).unwrap() as f64 / 18446744073709551616.0;
            assert!((back - x).abs() / x < 1e-11, "{x} -> {back}");
        }
    }
}
