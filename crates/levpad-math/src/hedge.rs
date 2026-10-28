//! Hedged mode (M5): split a new position between the unhedged bucket and the hedged bucket,
//! size the hedge, price constant-product swaps the way the venue fills them, and decide when a
//! hedged position must be liquidated. Design: `docs/HEDGED.md`. Model: `sim/hedge.py`.
//!
//! Venue model. DBC (curve phase) and DAMM v2 (graduated) both quote as a constant-product pool
//! over virtual reserves inside one liquidity segment:
//! `quote_v = L * sqrt_p >> 128` and `base_v = L / sqrt_p`, with `sqrt_p` a Q64.64 square root of
//! the price in lamports per base atom. Fees are charged on the quote side in both directions
//! (DBC `CollectFeeMode::QuoteToken`, DAMM v2 `collect_fee_mode = OnlyB` with SOL as token B):
//! a buy pays the fee out of the quote input, a sell pays it out of the quote output. Fees are
//! rounded up, so every estimate here is at most as good for the engine as the real fill.
//!
//! These estimates bound and check; they never set a fill. The engine fills the hedged part at
//! the real swap's token balance deltas and uses these functions for `min_out`, for the split,
//! and for the liquidation check.

use crate::position::{notional_at, pnl, size_for, Side};
use crate::{bps_of, div_q64, mul_div, mul_shr64, MathError, BPS, Q64};

/// Virtual reserves of a constant-product venue. `base` in atoms, `quote` in lamports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    pub base: u128,
    pub quote: u128,
}

impl Reserves {
    /// Spot price, Q64.64 lamports per atom.
    pub fn price(&self) -> Result<u128, MathError> {
        div_q64(self.quote, self.base)
    }
}

/// Integer square root, floored.
pub fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // start above the root and walk down (Newton), never overflowing
    let bits = 128 - n.leading_zeros();
    let mut x = 1u128 << ((bits + 1) / 2);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Q64.64 square root of a Q64.64 price (`sqrt(p / 2^64) * 2^64 = sqrt(p << 64)`).
pub fn sqrt_price_q64(price: u128) -> u128 {
    if price < Q64 {
        isqrt(price << 64)
    } else {
        // price << 64 would overflow; lose the low 32 bits of precision instead
        isqrt(price) << 32
    }
}

/// Virtual reserves from Meteora liquidity and sqrt price (both DBC and DAMM v2 use
/// `delta_quote = L * delta_sqrt_p >> 128` and `delta_base = L * delta_sqrt_p / (sqrt_lo * sqrt_hi)`).
pub fn reserves_from_liquidity(liquidity: u128, sqrt_price: u128) -> Result<Reserves, MathError> {
    if sqrt_price == 0 {
        return Err(MathError::DivByZero);
    }
    let quote = mul_shr64(liquidity, sqrt_price)? >> 64;
    let base = liquidity / sqrt_price;
    Ok(Reserves { base, quote })
}

/// The same pool moved to `price` along its own curve. Used to value a hedge at the TWAP mark
/// with the depth the pool really has, so a one-slot spot dump cannot trigger a liquidation.
pub fn reserves_at_price(liquidity: u128, price: u128) -> Result<Reserves, MathError> {
    reserves_from_liquidity(liquidity, sqrt_price_q64(price))
}

fn ceil_bps(x: u128, bps: u64) -> Result<u128, MathError> {
    let num = x.checked_mul(bps as u128).ok_or(MathError::Overflow)?;
    Ok(num.div_ceil(BPS as u128))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swap {
    /// Base atoms bought (buy) or sold (sell).
    pub base: u128,
    /// Lamports paid in, fee included (buy), or received, fee taken out (sell).
    pub quote: u128,
    /// Fee in lamports.
    pub fee: u128,
    pub after: Reserves,
}

impl Swap {
    /// What the swap cost or paid per atom, Q64.64. This is the hedged part's fill.
    pub fn exec_price(&self) -> Result<u128, MathError> {
        div_q64(self.quote, self.base)
    }
}

/// Exact-in buy of base with `quote_in` lamports, fee on the quote input.
pub fn buy(r: Reserves, quote_in: u128, fee_bps: u64) -> Result<Swap, MathError> {
    if fee_bps >= BPS {
        return Err(MathError::Overflow);
    }
    let fee = ceil_bps(quote_in, fee_bps)?;
    let net = quote_in - fee.min(quote_in);
    let q2 = r.quote.checked_add(net).ok_or(MathError::Overflow)?;
    let base_out = mul_div(r.base, net, q2)?;
    Ok(Swap { base: base_out, quote: quote_in, fee, after: Reserves { base: r.base - base_out, quote: q2 } })
}

/// Exact-in sell of `base_in` atoms, fee on the quote output.
pub fn sell(r: Reserves, base_in: u128, fee_bps: u64) -> Result<Swap, MathError> {
    if fee_bps >= BPS {
        return Err(MathError::Overflow);
    }
    let b2 = r.base.checked_add(base_in).ok_or(MathError::Overflow)?;
    let gross = mul_div(r.quote, base_in, b2)?;
    let fee = ceil_bps(gross, fee_bps)?;
    Ok(Swap { base: base_in, quote: gross - fee.min(gross), fee, after: Reserves { base: b2, quote: r.quote - gross } })
}

/// `min_out` for a CPI swap: the estimate less a tolerance. The swap reverts below it.
pub fn min_out(expected: u128, tolerance_bps: u64) -> Result<u128, MathError> {
    Ok(expected - bps_of(expected, tolerance_bps.min(BPS))?)
}

/// What the house carries on one coin. Sizes in base atoms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Buckets {
    /// Unhedged long size: synthetic, priced by the oracle.
    pub long_u: u128,
    /// All short size. Shorts are always unhedged: the engine cannot borrow the coin.
    pub short: u128,
    /// Hedged long size: atoms the engine holds for specific positions (pass-through).
    pub long_h: u128,
    /// Atoms the engine holds for itself to keep the unhedged bucket inside its cap as the price
    /// drifts (the rebalance crank's inventory). Counts against the unhedged bucket's net.
    pub house_hedge: u128,
}

impl Buckets {
    /// Signed net of the unhedged bucket in atoms: positive means traders are net long against
    /// the house.
    pub fn net_unhedged(&self) -> Result<i128, MathError> {
        let l = i128::try_from(self.long_u).map_err(|_| MathError::Overflow)?;
        let s = i128::try_from(self.short).map_err(|_| MathError::Overflow)?;
        let h = i128::try_from(self.house_hedge).map_err(|_| MathError::Overflow)?;
        Ok(l - s - h)
    }

    /// Atoms the engine holds in its base vault.
    pub fn inventory(&self) -> Result<u128, MathError> {
        self.long_h.checked_add(self.house_hedge).ok_or(MathError::Overflow)
    }
}

fn signed_notional(atoms: i128, price: u128) -> Result<i128, MathError> {
    let v = notional_at(atoms.unsigned_abs(), price)?;
    let v = i128::try_from(v).map_err(|_| MathError::Overflow)?;
    Ok(if atoms < 0 { -v } else { v })
}

/// How a new position's notional splits between the buckets, in lamports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    pub unhedged: u128,
    pub hedged: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Largest unhedged net notional, either direction (`cap_bps` of the quote reserve).
    pub unhedged: u128,
    /// Largest inventory value (`hedge_cap_bps` of the quote reserve). 0 turns hedging off.
    pub hedge: u128,
    /// A hedged part smaller than this is rounded up to it (or to the whole position), so the
    /// engine never pays a swap's fixed costs for dust.
    pub min_hedge: u128,
}

/// Split a new position. Longs fill the unhedged bucket's room first (no swap, no slippage) and
/// hedge the rest. Shorts are unhedged only. `None` means the position must be rejected: the
/// hedged part does not fit, or a short would push the unhedged net past the cap.
pub fn split_open(b: &Buckets, side: Side, notional: u128, price: u128, caps: &Caps) -> Result<Option<Split>, MathError> {
    let net = signed_notional(b.net_unhedged()?, price)?;
    let cap = i128::try_from(caps.unhedged).map_err(|_| MathError::Overflow)?;
    let n = i128::try_from(notional).map_err(|_| MathError::Overflow)?;
    match side {
        Side::Short => {
            let after = net - n;
            // a short that shrinks the imbalance is always fine, as in `within_net_cap`
            let ok = after.unsigned_abs() <= caps.unhedged || after.unsigned_abs() <= net.unsigned_abs();
            Ok(ok.then_some(Split { unhedged: notional, hedged: 0 }))
        }
        Side::Long => {
            let room = (cap - net).max(0) as u128;
            let mut u = notional.min(room);
            let mut h = notional - u;
            if h > 0 && h < caps.min_hedge {
                h = caps.min_hedge.min(notional);
                u = notional - h;
            }
            if h == 0 {
                return Ok(Some(Split { unhedged: u, hedged: 0 }));
            }
            let held = notional_at(b.inventory()?, price)?;
            let fits = held.checked_add(h).ok_or(MathError::Overflow)? <= caps.hedge;
            Ok(fits.then_some(Split { unhedged: u, hedged: h }))
        }
    }
}

/// Fees the trader pays at open on a split: `open_fee_bps` on the whole notional plus
/// `hedge_fee_bps` on the hedged part. The hedge fee is the premium for liquidation gap risk;
/// the hedge's own swap fee and slippage are already in the hedged fill.
pub fn open_fees(split: &Split, open_fee_bps: u64, hedge_fee_bps: u64) -> Result<u128, MathError> {
    let total = split.unhedged.checked_add(split.hedged).ok_or(MathError::Overflow)?;
    bps_of(total, open_fee_bps)?
        .checked_add(bps_of(split.hedged, hedge_fee_bps)?)
        .ok_or(MathError::Overflow)
}

/// A position as hedged mode stores it. Only longs have a hedged part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgedPosition {
    pub side: Side,
    /// Collateral less open fees.
    pub margin: u128,
    pub size_u: u128,
    /// Unhedged fill, Q64.64.
    pub entry_u: u128,
    /// Atoms the hedge swap delivered for this position.
    pub size_h: u128,
    /// Lamports the hedge swap cost, fee included.
    pub cost_h: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiqCheck {
    /// Equity with both parts at the mark.
    pub equity_mark: i128,
    /// Equity with the hedged part sold into the pool at the mark's depth, fee included.
    pub equity_exec: i128,
    pub liquidatable: bool,
}

/// Margin a split needs: each part at its own leverage limit (hundredths, 100 = 1x).
/// The unhedged part can be levered hard because it is small and synthetic; the hedged part
/// cannot, because a gap past its margin is a real loss of pool lamports (sim/hedge.py part c).
pub fn required_margin(split: &Split, max_lev_u_x100: u64, max_lev_h_x100: u64) -> Result<u128, MathError> {
    if max_lev_u_x100 == 0 || max_lev_h_x100 == 0 {
        return Err(MathError::DivByZero);
    }
    let u = mul_div(split.unhedged, 100, max_lev_u_x100 as u128)?;
    let h = mul_div(split.hedged, 100, max_lev_h_x100 as u128)?;
    u.checked_add(h).ok_or(MathError::Overflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maint {
    /// Maintenance on the unhedged notional at the mark (the M1 `maint_bps`).
    pub unhedged_bps: u64,
    /// Maintenance on the hedge's value at the mark. Must cover the one-step gap the market can
    /// show (sim/hedge.py part c), so it is much higher than the unhedged one.
    pub hedged_bps: u64,
    /// Floor on equity with the hedge sold at the mark's depth, bps of that notional.
    pub exec_bps: u64,
}

/// Liquidation check for a position with a hedged part.
///
/// Two triggers, either one is enough:
/// - mark: equity at the mark at or under `unhedged_bps` of the unhedged notional plus
///   `hedged_bps` of the hedge's value, both at the mark, and
/// - exec: equity with the hedge sold into `pool_at_mark` (the pool moved to the mark along its
///   own curve, see `reserves_at_price`) at or under `exec_bps` of that notional.
///
/// The exec trigger catches a hedge that is big against the pool, where the mark overstates
/// what the atoms would really fetch. Both use the mark, not spot, so a one-slot dump cannot
/// hunt a position; the mark for hedged markets should use a short window (`liq_window_slots`).
pub fn hedged_liq_check(
    p: &HedgedPosition,
    mark: u128,
    pool_at_mark: Reserves,
    fee_bps: u64,
    maint: &Maint,
) -> Result<LiqCheck, MathError> {
    let pnl_u = pnl(p.side, p.size_u, p.entry_u, mark)?;
    let margin = i128::try_from(p.margin).map_err(|_| MathError::Overflow)?;
    let cost_h = i128::try_from(p.cost_h).map_err(|_| MathError::Overflow)?;
    let val_h_mark = i128::try_from(notional_at(p.size_h, mark)?).map_err(|_| MathError::Overflow)?;
    let val_h_exec = if p.size_h == 0 {
        0
    } else {
        i128::try_from(sell(pool_at_mark, p.size_h, fee_bps)?.quote).map_err(|_| MathError::Overflow)?
    };
    let notional_u = notional_at(p.size_u, mark)?;
    let equity_mark = margin + pnl_u + val_h_mark - cost_h;
    let equity_exec = margin + pnl_u + val_h_exec - cost_h;
    let maint_mark = (bps_of(notional_u, maint.unhedged_bps)? + bps_of(val_h_mark as u128, maint.hedged_bps)?) as i128;
    let maint_exec = bps_of(notional_u + val_h_exec as u128, maint.exec_bps)? as i128;
    Ok(LiqCheck { equity_mark, equity_exec, liquidatable: equity_mark <= maint_mark || equity_exec <= maint_exec })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiqSettlement {
    /// Lamports returned to the trader: equity left after the reward and the penalty. Hedged
    /// parts carry a high maintenance margin (it must cover a one-step gap), so confiscating the
    /// whole margin as M1 does for synthetic positions would turn a 3% dip into a 30% loss.
    pub to_trader: u128,
    /// Liquidation penalty kept by the pool, on top of getting its hedge outlay back (the pool
    /// receives the hedge proceeds and the position's margin).
    pub to_pool: u128,
    pub reward: u128,
    /// Lamports the pool lost: the hole under zero equity plus the reward it had to pay.
    pub bad_debt: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiqFees {
    /// Liquidator reward, bps of notional at exit.
    pub reward_bps: u64,
    /// Penalty to the pool, bps of notional at exit.
    pub penalty_bps: u64,
    /// The pool pays at least this reward so a position under water still gets liquidated.
    pub min_reward: u128,
}

/// Settle a hedged liquidation once the hedge has been sold for `proceeds`. The unhedged part
/// settles at `exit_u`. Order out of the remaining equity: reward, then penalty, then trader.
pub fn settle_hedged_liquidation(
    p: &HedgedPosition,
    exit_u: u128,
    proceeds: u128,
    fees: &LiqFees,
) -> Result<LiqSettlement, MathError> {
    let pnl_u = pnl(p.side, p.size_u, p.entry_u, exit_u)?;
    let equity = i128::try_from(p.margin).map_err(|_| MathError::Overflow)? + pnl_u
        + i128::try_from(proceeds).map_err(|_| MathError::Overflow)?
        - i128::try_from(p.cost_h).map_err(|_| MathError::Overflow)?;
    if equity <= 0 {
        // the pool spent cost_h, got proceeds back, and keeps the margin; the hole is the rest
        let bad = ((-equity) as u128).checked_add(fees.min_reward).ok_or(MathError::Overflow)?;
        return Ok(LiqSettlement { to_trader: 0, to_pool: 0, reward: fees.min_reward, bad_debt: bad });
    }
    let e = equity as u128;
    let notional = notional_at(p.size_u, exit_u)?.checked_add(proceeds).ok_or(MathError::Overflow)?;
    let reward = bps_of(notional, fees.reward_bps)?.max(fees.min_reward).min(e);
    let penalty = bps_of(notional, fees.penalty_bps)?.min(e - reward);
    Ok(LiqSettlement { to_trader: e - reward - penalty, to_pool: penalty, reward, bad_debt: 0 })
}

/// What the rebalance crank should do to the house's own hedge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebalance {
    None,
    /// Buy base with this many lamports.
    Buy(u128),
    /// Sell this many atoms of the house hedge.
    Sell(u128),
}

/// Keep the unhedged bucket's net inside `cap`. When it is past the cap (in either direction),
/// trade the house hedge so the net comes back to `target_bps` of the cap, at most `max_step`
/// lamports per call. Inside the cap it does nothing (hysteresis: the band between target and
/// cap absorbs ordinary moves without churning fees). The crank prices at the mark and refuses
/// to run when spot is far from it (checked by the program, not here).
pub fn rebalance(b: &Buckets, mark: u128, cap: u128, target_bps: u64, max_step: u128) -> Result<Rebalance, MathError> {
    let net = signed_notional(b.net_unhedged()?, mark)?;
    if net.unsigned_abs() <= cap {
        return Ok(Rebalance::None);
    }
    let target = bps_of(cap, target_bps.min(BPS))?;
    let excess = (net.unsigned_abs() - target).min(max_step);
    if net > 0 {
        Ok(Rebalance::Buy(excess))
    } else {
        let atoms = size_for(excess, mark)?.min(b.house_hedge);
        Ok(if atoms == 0 { Rebalance::None } else { Rebalance::Sell(atoms) })
    }
}

/// Largest hedged notional one position may carry so that selling it at once costs at most
/// `max_slip_bps` (fee included) against the mark: a closed form for `sell` on `r`.
/// Selling q of value at price p into quote reserve x returns q * (1 - f) / (1 + q / x), so the
/// slippage s satisfies 1 - s = (1 - f) / (1 + q / x) and q = x * ((1 - f) / (1 - s) - 1).
pub fn max_hedged_for_slippage(quote_reserve: u128, fee_bps: u64, max_slip_bps: u64) -> Result<u128, MathError> {
    if max_slip_bps <= fee_bps || max_slip_bps >= BPS {
        return Ok(0);
    }
    // x * ((BPS - f) - (BPS - s)) / (BPS - s) = x * (s - f) / (BPS - s)
    mul_div(quote_reserve, (max_slip_bps - fee_bps) as u128, (BPS - max_slip_bps) as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u128 = 1_000_000_000;

    fn pool() -> Reserves {
        // 100 SOL against 1e15 atoms: price 1e-4 lamports per atom
        Reserves { base: 1_000_000_000_000_000, quote: 100 * SOL }
    }

    #[test]
    fn sqrt_helpers() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(u128::MAX), u64::MAX as u128);
        assert_eq!(sqrt_price_q64(4 * Q64), 2 * Q64);
        assert_eq!(sqrt_price_q64(Q64 / 4), Q64 / 2);
    }

    #[test]
    fn reserves_round_trip_through_liquidity() {
        let r = pool();
        let p = r.price().unwrap();
        // L = sqrt(base * quote) in Meteora units is sqrt(base*quote) << 64
        let l = isqrt(r.base * r.quote) << 64;
        let back = reserves_at_price(l, p).unwrap();
        let rel = |a: u128, b: u128| a.abs_diff(b) * 1_000_000 / b;
        assert!(rel(back.quote, r.quote) <= 1, "{back:?}");
        assert!(rel(back.base, r.base) <= 1, "{back:?}");
        // the same pool at 4x the price holds 2x the quote and half the base
        let up = reserves_at_price(l, 4 * p).unwrap();
        assert!(rel(up.quote, 2 * r.quote) <= 1);
        assert!(rel(up.base, r.base / 2) <= 1);
    }

    #[test]
    fn buy_and_sell_price_with_fee_and_impact() {
        let r = pool();
        let p = r.price().unwrap();
        // zero fee, tiny trade: fills at spot within rounding
        let s = buy(r, SOL / 1000, 0).unwrap();
        assert!(s.exec_price().unwrap().abs_diff(p) * 100_000 / p <= 1);
        // 1 SOL in at 1% fee into 100 SOL: net 0.99, exec = p * (100 + 0.99) / 0.99 * 0.99 / ... ;
        // closed form: exec / p = 1 / ((1 - f) * x / (x + net)) = (x + net) / (x (1 - f))
        let s = buy(r, SOL, 100).unwrap();
        assert_eq!(s.fee, SOL / 100);
        let want = mul_div(p, 100 * SOL + 99 * SOL / 100, 99 * SOL).unwrap();
        assert!(s.exec_price().unwrap().abs_diff(want) * 1_000_000 / want <= 1);
        // a round trip at zero fee returns the quote (CPMM path independence), less rounding
        let back = sell(s.after, s.base, 0).unwrap();
        assert!(SOL - SOL / 100 - back.quote <= 1);
        assert!(back.after.quote.abs_diff(r.quote) <= 1);
        // with the fee on both legs the loss is about 2f
        let back = sell(s.after, s.base, 100).unwrap();
        let lost = SOL - back.quote;
        assert!(lost >= SOL * 199 / 10_000 && lost < SOL * 2 / 100, "{lost}");
    }

    #[test]
    fn slippage_bound_matches_sell() {
        let r = pool();
        let p = r.price().unwrap();
        let q = max_hedged_for_slippage(r.quote, 100, 300).unwrap();
        // x * 0.02 / 0.97
        assert_eq!(q, 100 * SOL * 200 / 9_700);
        let atoms = size_for(q, p).unwrap();
        let s = sell(r, atoms, 100).unwrap();
        let slip_bps = (q - s.quote) * 10_000 / q;
        assert!((299..=300).contains(&slip_bps), "{slip_bps}");
        assert_eq!(max_hedged_for_slippage(r.quote, 100, 100).unwrap(), 0);
        assert_eq!(min_out(1_000, 50).unwrap(), 995);
    }

    #[test]
    fn split_fills_unhedged_room_first() {
        let p = Q64;
        let caps = Caps { unhedged: 50, hedge: 500, min_hedge: 10 };
        let b = Buckets::default();
        assert_eq!(split_open(&b, Side::Long, 30, p, &caps).unwrap(), Some(Split { unhedged: 30, hedged: 0 }));
        assert_eq!(split_open(&b, Side::Long, 200, p, &caps).unwrap(), Some(Split { unhedged: 50, hedged: 150 }));
        // dust remainder is rounded up to min_hedge
        assert_eq!(split_open(&b, Side::Long, 53, p, &caps).unwrap(), Some(Split { unhedged: 43, hedged: 10 }));
        // too big for the hedge cap
        assert_eq!(split_open(&b, Side::Long, 600, p, &caps).unwrap(), None);
        // shorts already in the book give longs more unhedged room
        let b = Buckets { short: 40, ..Default::default() };
        assert_eq!(split_open(&b, Side::Long, 100, p, &caps).unwrap(), Some(Split { unhedged: 90, hedged: 10 }));
        // inventory already held eats the hedge cap; the house hedge counts against the net
        let b = Buckets { long_u: 50, long_h: 300, house_hedge: 150, short: 0 };
        assert_eq!(b.net_unhedged().unwrap(), -100);
        assert_eq!(split_open(&b, Side::Long, 150, p, &caps).unwrap(), Some(Split { unhedged: 150, hedged: 0 }));
        assert_eq!(split_open(&b, Side::Long, 200, p, &caps).unwrap(), Some(Split { unhedged: 150, hedged: 50 }));
        assert_eq!(split_open(&b, Side::Long, 210, p, &caps).unwrap(), None);
        // hedging off: anything past the room is rejected
        let off = Caps { hedge: 0, ..caps };
        assert_eq!(split_open(&Buckets::default(), Side::Long, 51, p, &off).unwrap(), None);
    }

    #[test]
    fn shorts_are_unhedged_and_capped() {
        let p = Q64;
        let caps = Caps { unhedged: 50, hedge: 500, min_hedge: 0 };
        let b = Buckets::default();
        assert_eq!(split_open(&b, Side::Short, 50, p, &caps).unwrap(), Some(Split { unhedged: 50, hedged: 0 }));
        assert_eq!(split_open(&b, Side::Short, 51, p, &caps).unwrap(), None);
        // hedged longs do not offset shorts: they are pass-through
        let b = Buckets { long_h: 400, ..Default::default() };
        assert_eq!(split_open(&b, Side::Short, 51, p, &caps).unwrap(), None);
        // unhedged longs do
        let b = Buckets { long_u: 80, ..Default::default() };
        assert_eq!(split_open(&b, Side::Short, 120, p, &caps).unwrap(), Some(Split { unhedged: 120, hedged: 0 }));
        assert_eq!(open_fees(&Split { unhedged: 10_000, hedged: 20_000 }, 10, 50).unwrap(), 30 + 100);
    }

    #[test]
    fn hedged_liquidation_sees_depth() {
        let r = pool();
        let p = r.price().unwrap();
        // 1 SOL margin, 1 SOL unhedged + 9 SOL hedged (10x), hedge bought into the pool
        let hb = buy(r, 9 * SOL, 100).unwrap();
        let pos = HedgedPosition {
            side: Side::Long,
            margin: SOL,
            size_u: size_for(SOL, p).unwrap(),
            entry_u: p,
            size_h: hb.base,
            cost_h: 9 * SOL,
        };
        // mark back at the entry price, pool at the same depth as before the hedge
        let c = hedged_liq_check(&pos, p, r, 100, &M5).unwrap();
        // at the mark the hedge is worth less than it cost (fee + impact on the way in) ...
        assert!(c.equity_mark < SOL as i128);
        // ... and selling 9 SOL into 100 SOL costs another ~9% and the fee, so equity is gone
        assert!(c.equity_exec < 0, "{c:?}");
        assert!(c.liquidatable);
        // a small hedge against a deep pool is not liquidatable at the same mark
        let deep = Reserves { base: r.base * 100, quote: r.quote * 100 };
        let hb = buy(deep, 9 * SOL, 100).unwrap();
        let pos2 = HedgedPosition { size_h: hb.base, ..pos };
        let c2 = hedged_liq_check(&pos2, p, deep, 100, &M5).unwrap();
        assert!(!c2.liquidatable, "{c2:?}");
        // a 20% drop at the mark liquidates it through the mark trigger
        let c3 = hedged_liq_check(&pos2, p * 8 / 10, reserves_at_price(isqrt(deep.base * deep.quote) << 64, p * 8 / 10).unwrap(), 100, &M5).unwrap();
        assert!(c3.equity_mark <= 0 && c3.liquidatable);
        // the same 10x position under a 30% hedged maintenance is liquidatable at once: hedged
        // leverage has to sit under 1 / (hedged maint + entry costs)
        let m30 = Maint { hedged_bps: 3_000, ..M5 };
        assert!(hedged_liq_check(&pos2, p, deep, 100, &m30).unwrap().liquidatable);
    }

    const M5: Maint = Maint { unhedged_bps: 500, hedged_bps: 500, exec_bps: 0 };

    #[test]
    fn blended_margin() {
        let s = Split { unhedged: 1_000, hedged: 2_000 };
        // 20x on the unhedged part, 2x on the hedged part
        assert_eq!(required_margin(&s, 2_000, 200).unwrap(), 50 + 1_000);
        assert_eq!(required_margin(&s, 0, 200).unwrap_err(), MathError::DivByZero);
    }

    #[test]
    fn hedged_liquidation_settlement() {
        let p = Q64;
        let pos = HedgedPosition { side: Side::Long, margin: 100, size_u: 0, entry_u: p, size_h: 1_000, cost_h: 1_000 };
        // sold for 950: equity 50, reward 10% of that
        // sold for 950: equity 50; reward 1% and penalty 2% of the 950 notional; trader keeps the rest
        let fees = LiqFees { reward_bps: 100, penalty_bps: 200, min_reward: 1 };
        let s = settle_hedged_liquidation(&pos, p, 950, &fees).unwrap();
        assert_eq!(s, LiqSettlement { to_trader: 50 - 9 - 19, to_pool: 19, reward: 9, bad_debt: 0 });
        // sold for 1_080 against a 20 equity: reward 10, penalty only what is left
        let s = settle_hedged_liquidation(&HedgedPosition { margin: 0, cost_h: 1_060, ..pos }, p, 1_080, &fees).unwrap();
        assert_eq!(s, LiqSettlement { to_trader: 0, to_pool: 10, reward: 10, bad_debt: 0 });
        // sold for 850: 50 under water, the pool also pays the minimum reward
        let fees = LiqFees { min_reward: 3, ..fees };
        let s = settle_hedged_liquidation(&pos, p, 850, &fees).unwrap();
        assert_eq!(s, LiqSettlement { to_trader: 0, to_pool: 0, reward: 3, bad_debt: 53 });
    }

    #[test]
    fn rebalance_has_a_band() {
        let p = Q64;
        let b = Buckets { long_u: 120, ..Default::default() };
        assert_eq!(rebalance(&b, p, 150, 5_000, 1_000).unwrap(), Rebalance::None);
        // 200 against a cap of 100: buy back to 50
        assert_eq!(rebalance(&b, p, 100, 5_000, 1_000).unwrap(), Rebalance::Buy(70));
        assert_eq!(rebalance(&b, p, 100, 5_000, 20).unwrap(), Rebalance::Buy(20));
        // shorts outgrew the longs and the house holds 90 atoms of its own: sell what it holds
        let b = Buckets { long_u: 10, short: 50, house_hedge: 90, long_h: 0 };
        assert_eq!(rebalance(&b, p, 100, 5_000, 1_000).unwrap(), Rebalance::Sell(80));
        let b = Buckets { long_u: 0, short: 150, house_hedge: 0, long_h: 0 };
        assert_eq!(rebalance(&b, p, 100, 5_000, 1_000).unwrap(), Rebalance::None);
    }
}
