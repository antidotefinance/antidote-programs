//! Raw readers for the two venues a launched coin trades on. Offsets are from the Meteora sources
//! (DBC release 0.2.2, commit 91e696e; DAMM v2 cp-amm release 0.2.5) and are read back in the
//! LiteSVM suite against the devnet builds.

use anchor_lang::prelude::*;
use levpad_math::{mul_div, mul_shr64, price_from_sqrt_q64, MathError, Q64};

pub const DBC_PROGRAM: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
/// DBC `const_pda::pool_authority` (["pool_authority"] under DBC). It owns DBC's vaults, signs
/// the migration, and is the `pool_creator_authority` of every DAMM v2 config DBC migrates into.
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const DAMM_V2_PROGRAM: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// DBC `TransferHookPool` (zero copy, 8 + 416 bytes).
pub mod dbc {
    pub const DISC: [u8; 8] = [0xed, 0xdb, 0xb8, 0x17, 0x2a, 0xbd, 0xa9, 0x23];
    pub const LEN: usize = 8 + 416;
    pub const CONFIG: usize = 72;
    pub const CREATOR: usize = 104;
    pub const BASE_MINT: usize = 136;
    pub const BASE_VAULT: usize = 168;
    pub const QUOTE_VAULT: usize = 200;
    pub const BASE_RESERVE: usize = 232;
    pub const QUOTE_RESERVE: usize = 240;
    pub const SQRT_PRICE: usize = 280;
    pub const IS_MIGRATED: usize = 305;
    /// 0 PreBondingCurve, 1 PostBondingCurve, 2 LockedVesting, 3 CreatedPool.
    pub const MIGRATION_PROGRESS: usize = 308;
}

/// DAMM v2 accounts (zero copy).
pub mod damm {
    pub const POOL_DISC: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];
    pub const POOL_LEN: usize = 1112;
    pub const TOKEN_A_MINT: usize = 168;
    pub const TOKEN_B_MINT: usize = 200;
    pub const LIQUIDITY: usize = 360;
    pub const SQRT_MIN_PRICE: usize = 424;
    pub const SQRT_MAX_PRICE: usize = 440;
    pub const SQRT_PRICE: usize = 456;
    pub const CREATOR: usize = 648;
    pub const CONFIG_DISC: [u8; 8] = [0x9b, 0x0c, 0xaa, 0xe0, 0x1e, 0xfa, 0xcc, 0x82];
    pub const CONFIG_LEN: usize = 328;
    pub const CONFIG_POOL_CREATOR_AUTHORITY: usize = 40;
    pub const POOL_PREFIX: &[u8] = b"pool";
}

/// pump.fun: the bonding curve program and PumpSwap, the AMM a completed curve migrates into.
/// Layouts are from the programs' public IDLs (pump-sdk 4.1.0, pump-swap-sdk 2.1.0) and were
/// read back from mainnet accounts on 2026-10-19 (`tests/fixtures/pump`, slot 458676253).
pub mod pump {
    use anchor_lang::prelude::*;

    pub const PUMP_PROGRAM: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
    pub const PUMP_AMM_PROGRAM: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
    pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

    /// `BondingCurve` under the pump program at ["bonding-curve", mint]. Older curves are shorter
    /// (81 to 150 bytes); only the fields up to `complete` are required.
    pub const CURVE_SEED: &[u8] = b"bonding-curve";
    pub const CURVE_DISC: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];
    pub const CURVE_VIRTUAL_TOKEN: usize = 8;
    pub const CURVE_VIRTUAL_QUOTE: usize = 16;
    pub const CURVE_REAL_TOKEN: usize = 24;
    pub const CURVE_REAL_QUOTE: usize = 32;
    pub const CURVE_COMPLETE: usize = 48;
    pub const CURVE_MIN_LEN: usize = 49;
    /// `quote_mint` (all zero = SOL); curves shorter than its end are SOL-quoted.
    pub const CURVE_QUOTE_MINT: usize = 83;

    /// The canonical PumpSwap pool a curve migrates into: index 0, created by the pump program's
    /// ["pool-authority", mint] PDA, at ["pool", index u16 le, creator, base_mint, quote_mint].
    pub const POOL_AUTHORITY_SEED: &[u8] = b"pool-authority";
    pub const POOL_SEED: &[u8] = b"pool";
    pub const POOL_DISC: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
    pub const POOL_INDEX: usize = 9;
    pub const POOL_CREATOR: usize = 11;
    pub const POOL_BASE_MINT: usize = 43;
    pub const POOL_QUOTE_MINT: usize = 75;
    pub const POOL_BASE_VAULT: usize = 139;
    pub const POOL_QUOTE_VAULT: usize = 171;
    pub const POOL_MIN_LEN: usize = 203;
    /// i128 added to the quote vault's balance in every swap (0 on pools without the field).
    pub const POOL_VIRTUAL_QUOTE: usize = 245;
    /// Protocol and creator fees left in the quote vault until swept (not swap liquidity).
    pub const POOL_PROTOCOL_FEES: usize = 271;
    pub const POOL_CREATOR_FEES: usize = 279;
}

/// What a pump.fun venue says about the coin: Q64.64 lamports per base atom and the real SOL
/// liquidity behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PumpQuote {
    pub price: u128,
    pub quote_reserve: u64,
}

/// A pump.fun bonding curve: `virtual_quote / virtual_token` (the curve's marginal price; a buy of
/// `dx` lamports gets `dx * vt / (vq + dx)` atoms) and the real SOL it holds. None when the
/// reserves are zero (the migration empties them) or the layout is short.
pub fn pump_curve_quote(d: &[u8]) -> Option<PumpQuote> {
    if d.len() < pump::CURVE_MIN_LEN || d[..8] != pump::CURVE_DISC {
        return None;
    }
    let vt = u64_at(d, pump::CURVE_VIRTUAL_TOKEN);
    let vq = u64_at(d, pump::CURVE_VIRTUAL_QUOTE);
    if vt == 0 || vq == 0 {
        return None;
    }
    let price = mul_div(vq as u128, Q64, vt as u128).ok()?;
    (price > 0).then_some(PumpQuote { price, quote_reserve: u64_at(d, pump::CURVE_REAL_QUOTE) })
}

/// The curve is SOL-quoted: no `quote_mint` field, or the field is all zero.
pub fn pump_curve_sol_quoted(d: &[u8]) -> bool {
    let o = pump::CURVE_QUOTE_MINT;
    d.len() < o + 32 || d[o..o + 32].iter().all(|&b| b == 0)
}

pub fn pump_curve_complete(d: &[u8]) -> bool {
    d.len() >= pump::CURVE_MIN_LEN && d[pump::CURVE_COMPLETE] != 0
}

/// A PumpSwap pool from its two vault balances: the swap prices `(quote + virtual_quote) / base`
/// (pump-swap-sdk `buyBaseInput` and `sellBaseInput`), and the real liquidity is the quote vault
/// less the fee buckets the v2 trades leave in it (`realQuoteReserves`).
pub fn pump_pool_quote(pool: &[u8], base_amount: u64, quote_amount: u64) -> Option<PumpQuote> {
    if base_amount == 0 {
        return None;
    }
    let virt: i128 = if pool.len() >= pump::POOL_VIRTUAL_QUOTE + 16 {
        i128::from_le_bytes(pool[pump::POOL_VIRTUAL_QUOTE..pump::POOL_VIRTUAL_QUOTE + 16].try_into().unwrap())
    } else {
        0
    };
    let eff = (quote_amount as i128).checked_add(virt)?;
    if eff <= 0 {
        return None;
    }
    let price = mul_div(eff as u128, Q64, base_amount as u128).ok()?;
    let fees = if pool.len() >= pump::POOL_CREATOR_FEES + 8 {
        u64_at(pool, pump::POOL_PROTOCOL_FEES).saturating_add(u64_at(pool, pump::POOL_CREATOR_FEES))
    } else {
        0
    };
    (price > 0).then_some(PumpQuote { price, quote_reserve: quote_amount.saturating_sub(fees) })
}

/// An SPL Token or Token-2022 account's (mint, amount), when `owner` is one of the two programs.
pub fn token_account(owner: &Pubkey, d: &[u8]) -> Option<(Pubkey, u64)> {
    if (*owner != pump::TOKEN && *owner != TOKEN_2022) || d.len() < 165 {
        return None;
    }
    Some((key_at(d, 0), u64_at(d, 64)))
}

pub fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
pub fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
pub fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

/// A DBC transfer-hook pool for `mint`, or None. Never errors.
pub fn is_dbc_pool(owner: &Pubkey, d: &[u8], mint: &Pubkey) -> bool {
    *owner == DBC_PROGRAM && d.len() >= dbc::LEN && d[..8] == dbc::DISC && d[dbc::BASE_MINT..dbc::BASE_MINT + 32] == mint.to_bytes()
}

/// What a DAMM v2 pool says about the coin: Q64.64 lamports per base atom and the quote reserve.
pub struct DammQuote {
    pub price: u128,
    pub quote_reserve: u64,
}

/// Reads a DAMM v2 pool between `base` and WSOL. DAMM v2's `sqrt_price` is the Q64.64 square root
/// of token B per token A, and liquidity is concentrated on [sqrt_min, sqrt_max]:
/// `amount_b = L * (s - s_min) >> 128`, `amount_a = L * (s_max - s) / (s * s_max)`.
pub fn damm_quote(d: &[u8], base: &Pubkey) -> core::result::Result<DammQuote, MathError> {
    let a = key_at(d, damm::TOKEN_A_MINT);
    let s = u128_at(d, damm::SQRT_PRICE);
    let l = u128_at(d, damm::LIQUIDITY);
    let smin = u128_at(d, damm::SQRT_MIN_PRICE);
    let smax = u128_at(d, damm::SQRT_MAX_PRICE);
    if s == 0 || s < smin || s > smax {
        return Err(MathError::Overflow);
    }
    let b_per_a = price_from_sqrt_q64(s)?;
    if a == *base {
        // quote is B
        let q = mul_shr64(l, s - smin)? >> 64; // floor(floor(L * ds / 2^64) / 2^64) = L * ds >> 128
        let q = u64::try_from(q).map_err(|_| MathError::Overflow)?;
        Ok(DammQuote { price: b_per_a, quote_reserve: q })
    } else {
        // quote is A: invert the price
        if b_per_a == 0 {
            return Err(MathError::DivByZero);
        }
        let price = mul_div(Q64, Q64, b_per_a)?;
        let q = mul_div(l, smax - s, s)? / smax;
        let q = u64::try_from(q).map_err(|_| MathError::Overflow)?;
        Ok(DammQuote { price, quote_reserve: q })
    }
}
