//! Leverage-scaled vesting for loss-minted grants (owner decision 2026-10-17).
//!
//! A grant vests faster the higher the leverage of the losing trades it came from: linear in
//! log(leverage), from `slow` seconds at 1x to `fast` seconds at 1000x. The leverage of a
//! (wallet, market, epoch) entry is the loss-weighted mean of `log2(leverage)` over its losing
//! trades, i.e. a loss-weighted geometric mean, so a tiny high-leverage loss barely moves a large
//! low-leverage one.

/// Fractional bits of the fixed-point log2 (`log2(x) * 2^LOG_BITS`).
pub const LOG_BITS: u32 = 20;
/// Leverage endpoints, hundredths: 1x and 1000x.
pub const LEV_LO_X100: u32 = 100;
pub const LEV_HI_X100: u32 = 100_000;

/// `log2(x) * 2^20`, rounded down, for x >= 1 (0 for x = 0).
pub fn log2_fixed(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    let n = 63 - x.leading_zeros() as u64;
    let mut out = n << LOG_BITS;
    // mantissa in [1, 2) as Q62
    let mut z: u128 = ((x as u128) << 62) >> n;
    let two: u128 = 2u128 << 62;
    for i in 1..=LOG_BITS {
        z = (z * z) >> 62;
        if z >= two {
            z >>= 1;
            out |= 1u64 << (LOG_BITS - i);
        }
    }
    out
}

/// `log2(leverage)` in fixed point, leverage in hundredths clamped to [1x, 1000x]; 0 at 1x.
pub fn lev_log(leverage_x100: u32) -> u64 {
    let l = leverage_x100.clamp(LEV_LO_X100, LEV_HI_X100) as u64;
    log2_fixed(l) - log2_fixed(LEV_LO_X100 as u64)
}

/// `lev_log` at 1000x: the top of the scale.
pub fn lev_log_max() -> u64 {
    lev_log(LEV_HI_X100)
}

/// Vest seconds for a loss-weighted mean log-leverage `mean` (from `lev_log`): `slow` at 1x,
/// `fast` at 1000x, linear in between. Requires fast <= slow (else returns slow).
pub fn vest_secs(mean: u64, fast: u32, slow: u32) -> u32 {
    if fast >= slow {
        return slow;
    }
    let top = lev_log_max();
    let m = mean.min(top) as u128;
    let span = (slow - fast) as u128;
    // the reduction rounds down, so rounding never shortens a vest
    slow - (span * m / top as u128) as u32
}

/// The lock scaled by the same factor as the vest (`vest / slow`), so a fast grant is not held
/// behind the full fixed lock: at 1000x with the defaults (lock 1 day, 1 h to 7 days) the lock is
/// 515 s. Rounded up.
pub fn scaled_lock(lock: u32, vest: u32, slow: u32) -> u32 {
    if slow == 0 || vest >= slow {
        return lock;
    }
    ((lock as u64 * vest as u64).div_ceil(slow as u64)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_exact_powers_and_close_elsewhere() {
        assert_eq!(log2_fixed(1), 0);
        assert_eq!(log2_fixed(2), 1 << LOG_BITS);
        assert_eq!(log2_fixed(1024), 10 << LOG_BITS);
        for x in [3u64, 7, 100, 12_345, 100_000, u64::MAX] {
            let want = (x as f64).log2() * (1u64 << LOG_BITS) as f64;
            let got = log2_fixed(x) as f64;
            assert!((want - got).abs() <= 2.0, "x={x} want {want} got {got}");
        }
    }

    #[test]
    fn endpoints_and_geometric_middle() {
        let (fast, slow) = (3_600, 604_800);
        assert_eq!(vest_secs(lev_log(100), fast, slow), slow);
        assert_eq!(vest_secs(lev_log(50), fast, slow), slow, "under 1x clamps to 1x");
        assert_eq!(vest_secs(lev_log(100_000), fast, slow), fast);
        assert_eq!(vest_secs(lev_log(1_000_000), fast, slow), fast, "over 1000x clamps");
        // sqrt(1000) = 31.62x sits halfway on the log scale
        let mid = vest_secs(lev_log(3_162), fast, slow) as i64;
        assert!((mid - (fast + slow) as i64 / 2).abs() < 200, "mid {mid}");
        // monotone: more leverage never vests slower
        let mut last = u32::MAX;
        for l in (100..=100_000).step_by(997) {
            let v = vest_secs(lev_log(l), fast, slow);
            assert!(v <= last);
            last = v;
        }
    }

    #[test]
    fn loss_weighting_keeps_a_tiny_high_leverage_loss_from_speeding_a_big_one() {
        let (fast, slow) = (3_600u32, 604_800u32);
        // 100 SOL lost at 2x, 0.01 SOL lost at 1000x
        let (big, tiny) = (100_000_000_000u128, 10_000_000u128);
        let sum = big * lev_log(200) as u128 + tiny * lev_log(100_000) as u128;
        let mean = (sum / (big + tiny)) as u64;
        let mixed = vest_secs(mean, fast, slow);
        let alone = vest_secs(lev_log(200), fast, slow);
        assert!(alone - mixed < 60, "a 1e-4 share of the loss moves the vest under a minute: {alone} vs {mixed}");
    }

    #[test]
    fn lock_scales_with_the_vest() {
        assert_eq!(scaled_lock(86_400, 604_800, 604_800), 86_400);
        assert_eq!(scaled_lock(86_400, 3_600, 604_800), 515);
        assert_eq!(scaled_lock(0, 3_600, 604_800), 0);
    }
}
