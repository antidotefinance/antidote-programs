//! Synthetic positions against the house pool: fills, PnL, margin, settlement and the
//! net-exposure cap.

use crate::schedule::{eval, Point};
use crate::{bps_of, mul_div, mul_shr64, MathError, BPS, Q64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Long = 0,
    Short = 1,
}

/// Fills take whichever of spot and TWAP is worse for the trader, so neither a spot pump nor a
/// lagging average can be used to enter cheap or exit rich.
pub fn fill_price(side: Side, opening: bool, spot: u128, twap: u128) -> u128 {
    let buying = matches!((side, opening), (Side::Long, true) | (Side::Short, false));
    if buying {
        spot.max(twap)
    } else {
        spot.min(twap)
    }
}

/// Base atoms bought by `notional` lamports at `price`.
pub fn size_for(notional: u128, price: u128) -> Result<u128, MathError> {
    mul_div(notional, Q64, price)
}

/// Lamport value of `size` atoms at `price`.
pub fn notional_at(size: u128, price: u128) -> Result<u128, MathError> {
    mul_shr64(size, price)
}

/// Signed PnL in lamports.
pub fn pnl(side: Side, size: u128, entry: u128, exit: u128) -> Result<i128, MathError> {
    let at_entry = notional_at(size, entry)? as i128;
    let at_exit = notional_at(size, exit)? as i128;
    Ok(match side {
        Side::Long => at_exit - at_entry,
        Side::Short => at_entry - at_exit,
    })
}

/// Liquidatable when equity has fallen to the maintenance margin of the current notional.
pub fn liquidatable(collateral: u128, pnl: i128, notional_now: u128, maint_bps: u64) -> Result<bool, MathError> {
    let equity = collateral as i128 + pnl;
    let maint = bps_of(notional_now, maint_bps)? as i128;
    Ok(equity <= maint)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// Lamports owed to the trader: collateral back plus net profit, or what is left after a loss.
    pub owed: u128,
    /// Profit fee kept by the house.
    pub profit_fee: u128,
    /// Profit cut by the payout cap.
    pub capped: u128,
}

/// Close a position. A winner pays `profit_fee_bps(roi)` of the profit, where roi is profit over
/// collateral in bps; the profit after fee is capped at `max_roi_bps` of collateral.
pub fn settle(
    collateral: u128,
    pnl: i128,
    fee_curve: &[Point],
    fee_points: usize,
    max_roi_bps: u64,
) -> Result<Settlement, MathError> {
    settle_with(collateral, pnl, fee_curve, fee_points, max_roi_bps, |_| Ok(0))
}

/// The winner haircut by hold time (papertrade's and Rollbit's "small, fast wins keep less"):
/// extra bps of profit from `hold_curve` at `hold_secs`, falling linearly to 0 as the return on
/// collateral rises to `taper_roi_bps` (0 = no taper). 0 when the curve is empty.
pub fn hold_surcharge_bps(hold_curve: &[Point], hold_points: usize, hold_secs: u64, roi_bps: u64, taper_roi_bps: u64) -> Result<u64, MathError> {
    if hold_points == 0 {
        return Ok(0);
    }
    let s = eval(hold_curve, hold_points, hold_secs)?;
    if taper_roi_bps == 0 {
        return Ok(s);
    }
    let left = taper_roi_bps.saturating_sub(roi_bps);
    Ok(mul_div(s as u128, left as u128, taper_roi_bps as u128)? as u64)
}

/// `settle`, with the profit fee raised by `extra_bps(roi_bps)` (the hold-time surcharge). The total
/// fee is at most 100% of the profit.
pub fn settle_with(
    collateral: u128,
    pnl: i128,
    fee_curve: &[Point],
    fee_points: usize,
    max_roi_bps: u64,
    extra_bps: impl Fn(u64) -> Result<u64, MathError>,
) -> Result<Settlement, MathError> {
    if pnl <= 0 {
        let loss = (-pnl) as u128;
        return Ok(Settlement { owed: collateral.saturating_sub(loss), profit_fee: 0, capped: 0 });
    }
    let profit = pnl as u128;
    let roi_bps = mul_div(profit, BPS as u128, collateral.max(1))?;
    let roi_bps = u64::try_from(roi_bps).unwrap_or(u64::MAX);
    let fee_bps = eval(fee_curve, fee_points, roi_bps)?.saturating_add(extra_bps(roi_bps)?).min(BPS);
    let profit_fee = bps_of(profit, fee_bps)?;
    let net = profit - profit_fee;
    let cap = bps_of(collateral, max_roi_bps)?;
    let paid = net.min(cap);
    Ok(Settlement { owed: collateral + paid, profit_fee, capped: net - paid })
}

/// Net exposure the house carries on one coin, in lamports at `price`.
pub fn net_exposure(long_size: u128, short_size: u128, price: u128) -> Result<u128, MathError> {
    notional_at(long_size.abs_diff(short_size), price)
}

/// Whether adding `size` on `side` keeps the house's net exposure within `cap`, or at least
/// does not make it worse (a trade that reduces the imbalance is always allowed).
pub fn within_net_cap(
    long_size: u128,
    short_size: u128,
    side: Side,
    size: u128,
    price: u128,
    cap: u128,
) -> Result<bool, MathError> {
    let before = net_exposure(long_size, short_size, price)?;
    let (l, s) = match side {
        Side::Long => (long_size.checked_add(size).ok_or(MathError::Overflow)?, short_size),
        Side::Short => (long_size, short_size.checked_add(size).ok_or(MathError::Overflow)?),
    };
    let after = net_exposure(l, s, price)?;
    Ok(after <= cap || after <= before)
}

/// Net-exposure cap for a coin: `cap_bps` of the venue's quote reserve (virtual + real).
/// sim/attack.py shows why this tracks the curve fee: with no one selling into a pump, an
/// attack pays once net notional passes roughly the fee times the quote reserve.
pub fn net_cap(quote_reserve: u128, cap_bps: u64) -> Result<u128, MathError> {
    bps_of(quote_reserve, cap_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u128 = 1_000_000_000;

    #[test]
    fn hold_surcharge_hits_small_fast_wins() {
        // +20% of profit at 0 s, +10% at 60 s, 0 from 600 s; tapering to 0 at a 100% return
        let mut c = [Point::default(); 8];
        c[0] = Point { x: 0, y: 2_000 };
        c[1] = Point { x: 60, y: 1_000 };
        c[2] = Point { x: 600, y: 0 };
        assert_eq!(hold_surcharge_bps(&c, 0, 0, 0, 0).unwrap(), 0);
        assert_eq!(hold_surcharge_bps(&c, 3, 0, 0, 10_000).unwrap(), 2_000);
        assert_eq!(hold_surcharge_bps(&c, 3, 30, 0, 10_000).unwrap(), 1_500);
        assert_eq!(hold_surcharge_bps(&c, 3, 30, 5_000, 10_000).unwrap(), 750);
        assert_eq!(hold_surcharge_bps(&c, 3, 30, 20_000, 10_000).unwrap(), 0);
        assert_eq!(hold_surcharge_bps(&c, 3, 10_000, 0, 10_000).unwrap(), 0);
        assert_eq!(hold_surcharge_bps(&c, 3, 60, 20_000, 0).unwrap(), 1_000);
        // a 10% win held 30 s: 10% fee from the ROI curve plus 1,350 bps -> keeps 76.5%
        let mut f = [Point::default(); 8];
        f[0] = Point { x: 0, y: 1_000 };
        let st = settle_with(SOL, (SOL / 10) as i128, &f, 1, 1_000_000, |roi| hold_surcharge_bps(&c, 3, 30, roi, 10_000)).unwrap();
        assert_eq!(st.profit_fee, (SOL / 10) * 2_350 / 10_000);
        // net received never falls as the profit grows (taper keeps it monotone)
        let mut last = 0;
        for k in 1..200u128 {
            let st = settle_with(SOL, (SOL * k / 100) as i128, &f, 1, 1_000_000, |roi| hold_surcharge_bps(&c, 3, 0, roi, 10_000)).unwrap();
            assert!(st.owed >= last);
            last = st.owed;
        }
        // the total fee never passes 100% of the profit
        let mut big = [Point::default(); 8];
        big[0] = Point { x: 0, y: 9_000 };
        let st = settle_with(SOL, SOL as i128, &big, 1, 1_000_000, |_| Ok(5_000)).unwrap();
        assert_eq!(st.owed, SOL);
    }

    #[test]
    fn worse_of_fills() {
        assert_eq!(fill_price(Side::Long, true, 10, 12), 12);
        assert_eq!(fill_price(Side::Long, false, 10, 12), 10);
        assert_eq!(fill_price(Side::Short, true, 10, 12), 10);
        assert_eq!(fill_price(Side::Short, false, 10, 12), 12);
    }

    #[test]
    fn long_and_short_pnl() {
        let entry = Q64 >> 10; // 1/1024 lamport per atom
        let size = size_for(10 * SOL, entry).unwrap();
        assert_eq!(notional_at(size, entry).unwrap(), 10 * SOL);
        let up = entry + entry / 4;
        assert_eq!(pnl(Side::Long, size, entry, up).unwrap(), (10 * SOL / 4) as i128);
        assert_eq!(pnl(Side::Short, size, entry, up).unwrap(), -((10 * SOL / 4) as i128));
        // an inexact price floors toward zero by at most one lamport
        let odd = Q64 / 1000;
        let s2 = size_for(10 * SOL, odd).unwrap();
        assert!(10 * SOL - notional_at(s2, odd).unwrap() <= 1);
    }

    #[test]
    fn liquidation_threshold() {
        // 1 SOL collateral, 10 SOL notional, maintenance 5% of notional = 0.5 SOL
        assert!(!liquidatable(SOL, -(SOL as i128) / 2 + 1, 10 * SOL, 500).unwrap());
        assert!(liquidatable(SOL, -(SOL as i128) / 2, 10 * SOL, 500).unwrap());
    }

    #[test]
    fn settle_loss_win_and_cap() {
        let curve = [Point { x: 0, y: 2000 }, Point { x: 10_000, y: 1000 }];
        let lost = settle(SOL, -(SOL as i128) * 2, &curve, 2, 90_000).unwrap();
        assert_eq!(lost.owed, 0);
        // +100% roi pays a 10% fee on profit
        let won = settle(SOL, SOL as i128, &curve, 2, 90_000).unwrap();
        assert_eq!(won.profit_fee, SOL / 10);
        assert_eq!(won.owed, SOL + SOL * 9 / 10);
        // +2000% roi, fee 10%, net 18 SOL, capped at 9 SOL (900%)
        let huge = settle(SOL, 20 * SOL as i128, &curve, 2, 90_000).unwrap();
        assert_eq!(huge.owed, 10 * SOL);
        assert_eq!(huge.capped, 9 * SOL);
    }

    #[test]
    fn net_cap_allows_reducing_trades() {
        let p = Q64;
        assert!(within_net_cap(0, 0, Side::Long, 5, p, 5).unwrap());
        assert!(!within_net_cap(0, 0, Side::Long, 6, p, 5).unwrap());
        // already over cap long; a short that shrinks the imbalance is fine
        assert!(within_net_cap(10, 0, Side::Short, 3, p, 5).unwrap());
        assert!(!within_net_cap(10, 0, Side::Long, 1, p, 5).unwrap());
        // a short that flips the imbalance past the cap the other way is not
        assert!(!within_net_cap(10, 0, Side::Short, 30, p, 5).unwrap());
    }
}
