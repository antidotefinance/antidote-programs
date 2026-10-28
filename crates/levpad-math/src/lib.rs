//! levpad perp engine math.
//!
//! Units: collateral, notional, PnL and pool balances are lamports. Sizes are base-token atoms.
//! Prices are Q64.64 lamports per base atom (`u128`), so `notional = size * price >> 64`.
//! Ratios are basis points (`BPS = 10_000`).

#![cfg_attr(not(test), no_std)]

pub mod expo;
pub mod hedge;
pub mod oracle;
pub mod position;
pub mod schedule;
pub mod vest;

pub const BPS: u64 = 10_000;
pub const Q64: u128 = 1u128 << 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    Overflow,
    DivByZero,
    /// The oracle has less history than the window asks for.
    ShortHistory,
    /// A schedule has no points or its x values are not strictly increasing.
    BadSchedule,
}

/// (a * b) >> 64 without overflowing the intermediate product.
pub fn mul_shr64(a: u128, b: u128) -> Result<u128, MathError> {
    let (a_hi, a_lo) = (a >> 64, a & (Q64 - 1));
    let (b_hi, b_lo) = (b >> 64, b & (Q64 - 1));
    // a*b = hh<<128 + (hl+lh)<<64 + ll ; shifted right by 64
    let hh = a_hi.checked_mul(b_hi).ok_or(MathError::Overflow)?;
    let hl = a_hi * b_lo; // each < 2^128
    let lh = a_lo * b_hi;
    let ll = a_lo * b_lo;
    let hh_shift = hh.checked_mul(Q64).ok_or(MathError::Overflow)?;
    hh_shift
        .checked_add(hl)
        .and_then(|v| v.checked_add(lh))
        .and_then(|v| v.checked_add(ll >> 64))
        .ok_or(MathError::Overflow)
}

/// (a << 64) / b, the Q64.64 quotient of two integers.
pub fn div_q64(a: u128, b: u128) -> Result<u128, MathError> {
    if b == 0 {
        return Err(MathError::DivByZero);
    }
    if a < Q64 {
        return Ok((a << 64) / b);
    }
    // long division in two halves to keep a<<64 from overflowing
    let q_hi = a / b;
    let r = a % b;
    let q_lo = mul_div(r, Q64, b)?;
    q_hi.checked_mul(Q64)
        .and_then(|v| v.checked_add(q_lo))
        .ok_or(MathError::Overflow)
}

/// floor(a * b / c) for values whose product may exceed u128, as long as the result fits.
pub fn mul_div(a: u128, b: u128, c: u128) -> Result<u128, MathError> {
    if c == 0 {
        return Err(MathError::DivByZero);
    }
    if let Some(p) = a.checked_mul(b) {
        return Ok(p / c);
    }
    // 256-bit product as (hi, lo), then shift-subtract division.
    let (hi, lo) = mul_wide(a, b);
    if hi >= c {
        return Err(MathError::Overflow);
    }
    let mut rem = hi;
    let mut q: u128 = 0;
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        q <<= 1;
        if carry == 1 || rem >= c {
            rem = rem.wrapping_sub(c);
            q |= 1;
        }
    }
    Ok(q)
}

fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let (a_hi, a_lo) = (a >> 64, a & (Q64 - 1));
    let (b_hi, b_lo) = (b >> 64, b & (Q64 - 1));
    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;
    let mid = (ll >> 64) + (lh & (Q64 - 1)) + (hl & (Q64 - 1));
    let lo = (ll & (Q64 - 1)) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

/// x * bps / 10_000, floored.
pub fn bps_of(x: u128, bps: u64) -> Result<u128, MathError> {
    mul_div(x, bps as u128, BPS as u128)
}

/// Spot price of a constant-product venue: quote reserve over base reserve, Q64.64.
pub fn cpmm_price(quote_reserve: u128, base_reserve: u128) -> Result<u128, MathError> {
    div_q64(quote_reserve, base_reserve)
}

/// Price from a Meteora-style sqrt price (Q64.64 sqrt of base-atom price in quote atoms).
pub fn price_from_sqrt_q64(sqrt_price: u128) -> Result<u128, MathError> {
    mul_shr64(sqrt_price, sqrt_price)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_matches_wide() {
        let a = u128::MAX / 3;
        let b = 7u128;
        assert_eq!(mul_div(a, b, 7).unwrap(), a);
        let a = 1u128 << 100;
        let b = 1u128 << 100;
        assert_eq!(mul_div(a, b, 1u128 << 90).unwrap(), 1u128 << 110);
        assert_eq!(mul_div(a, b, 1).unwrap_err(), MathError::Overflow);
    }

    #[test]
    fn q64_round_trip() {
        let p = div_q64(3, 2).unwrap();
        assert_eq!(p, Q64 + Q64 / 2);
        assert_eq!(mul_shr64(p, 2 * Q64).unwrap(), 3 * Q64);
        let a = (1u128 << 65) + 7;
        assert_eq!(div_q64(a, 5).unwrap(), mul_div(a, Q64, 5).unwrap());
        assert_eq!(div_q64(1u128 << 70, 3).unwrap_err(), MathError::Overflow);
        let s = Q64 * 3;
        assert_eq!(price_from_sqrt_q64(s).unwrap(), 9 * Q64);
    }
}
