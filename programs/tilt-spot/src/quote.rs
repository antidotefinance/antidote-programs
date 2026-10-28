//! Quotes other than SOL (multi-quote launches, 2026-10-23; SPEC.md "Multi-quote launches").
//!
//! A coin quoted in USDC or TSLAx books its position, its fees and its loss in atoms of that quote,
//! so the fee cap needs no price. Only the grant is valued in SOL, at the sell: `grant x price`,
//! where the price is lamports per quote atom from the quote's `QuoteBook`.
//!
//! **Source.** Anyone samples the spot price of the quote's leg pools into the book
//! (`sample_quote`, at most once per `sample_gap_slots`): leg 0 pairs the quote with SOL or with a
//! middle mint (USDC), leg 1 pairs the middle mint with SOL. Each sample adds the previous spot
//! times the slots since it to a cumulative, so two samples give the TWAP between them. The
//! pools are read raw (DAMM v2, Raydium CLMM, Orca Whirlpool: the deepest stable and xStock pools
//! on mainnet are CLMMs), in raw atoms on both sides: the scaled UI multiplier of an xStock never
//! enters (it changes only what wallets display).
//!
//! **Which price.** Grants use min(long TWAP, recent TWAP): pushing the quote's price up to inflate
//! a grant has to hold over the whole long window, and a push down only shrinks grants. levfee's
//! quote sales use max(long, recent) (`price_high`), so a push down cannot buy its quote cheaply.
//! With no fresh sample there is no price, and grant value waits (`QuotePending`).

use crate::errors::{m, SpotError};
use crate::state::*;
use anchor_lang::prelude::*;
use levpad_hook::venue::{self, DAMM_V2_PROGRAM, WSOL};
use levpad_math::{mul_div, mul_shr64, price_from_sqrt_q64, Q64};

pub const RAYDIUM_CLMM: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const ORCA_WHIRLPOOL: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Raydium CLMM `PoolState` (1,544 bytes): discriminator, then token_mint_0 at 73, token_mint_1
/// at 105, sqrt_price_x64 at 253 (read back from mainnet pools in tests/tests/multi_quote.rs).
pub mod clmm {
    pub const DISC: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];
    pub const LEN: usize = 1544;
    pub const MINT_0: usize = 73;
    pub const MINT_1: usize = 105;
    pub const SQRT_PRICE: usize = 253;
}
/// Orca `Whirlpool` (653 bytes): sqrt_price at 65, token_mint_a at 101, token_mint_b at 181.
pub mod whirlpool {
    pub const DISC: [u8; 8] = [0x3f, 0x95, 0xd1, 0x0c, 0xe1, 0x80, 0x63, 0x09];
    pub const LEN: usize = 653;
    pub const SQRT_PRICE: usize = 65;
    pub const MINT_A: usize = 101;
    pub const MINT_B: usize = 181;
}

#[event]
pub struct QuoteSampled {
    pub mint: Pubkey,
    pub slot: u64,
    pub cum: u128,
    /// The price recorded, Q64.64 lamports per quote atom: the pools' spot clamped (R7).
    pub spot: u128,
    /// The pools' spot now, before the clamp (appended 2026-10-23).
    pub raw: u128,
}

/// A pool's (mint 0, mint 1, Q64.64 price of mint 0 in atoms of mint 1), by kind.
pub fn read_leg(kind: u8, ai: &AccountInfo) -> Result<(Pubkey, Pubkey, u128)> {
    let d = ai.try_borrow_data()?;
    let (m0, m1, sqrt) = match kind {
        LEG_DAMM_V2 => {
            require!(*ai.owner == DAMM_V2_PROGRAM && d.len() >= venue::damm::POOL_LEN && d[..8] == venue::damm::POOL_DISC, SpotError::WrongAccount);
            (venue::key_at(&d, venue::damm::TOKEN_A_MINT), venue::key_at(&d, venue::damm::TOKEN_B_MINT), venue::u128_at(&d, venue::damm::SQRT_PRICE))
        }
        LEG_RAYDIUM_CLMM => {
            require!(*ai.owner == RAYDIUM_CLMM && d.len() >= clmm::LEN && d[..8] == clmm::DISC, SpotError::WrongAccount);
            (venue::key_at(&d, clmm::MINT_0), venue::key_at(&d, clmm::MINT_1), venue::u128_at(&d, clmm::SQRT_PRICE))
        }
        LEG_ORCA_WHIRLPOOL => {
            require!(*ai.owner == ORCA_WHIRLPOOL && d.len() >= whirlpool::LEN && d[..8] == whirlpool::DISC, SpotError::WrongAccount);
            (venue::key_at(&d, whirlpool::MINT_A), venue::key_at(&d, whirlpool::MINT_B), venue::u128_at(&d, whirlpool::SQRT_PRICE))
        }
        _ => return err!(SpotError::WrongAccount),
    };
    let p = m(price_from_sqrt_q64(sqrt))?;
    require!(p > 0, SpotError::NoQuotePrice);
    Ok((m0, m1, p))
}

/// The price of `from` in atoms of the leg's other mint (Q64.64), and that mint.
fn leg_price(kind: u8, ai: &AccountInfo, from: &Pubkey) -> Result<(Pubkey, u128)> {
    let (m0, m1, p) = read_leg(kind, ai)?;
    if m0 == *from {
        Ok((m1, p))
    } else {
        require!(m1 == *from, SpotError::WrongAccount);
        Ok((m0, m(mul_div(Q64, Q64, p))?))
    }
}

/// Spot lamports per quote atom (Q64.64) from the entry's legs, read now.
pub fn spot(e: &QuoteEntry, leg0: &AccountInfo, leg1: Option<&AccountInfo>) -> Result<u128> {
    require_keys_eq!(leg0.key(), e.legs[0].pool, SpotError::WrongAccount);
    let (mid, p0) = leg_price(e.legs[0].kind, leg0, &e.mint)?;
    if mid == WSOL {
        require!(e.legs[1].kind == LEG_NONE, SpotError::WrongAccount);
        return Ok(p0);
    }
    let l1 = leg1.ok_or(SpotError::WrongAccount)?;
    require_keys_eq!(l1.key(), e.legs[1].pool, SpotError::WrongAccount);
    let (sol, p1) = leg_price(e.legs[1].kind, l1, &mid)?;
    require_keys_eq!(sol, WSOL, SpotError::WrongAccount);
    let p = m(mul_shr64(p0, p1))?;
    require!(p > 0, SpotError::NoQuotePrice);
    Ok(p)
}

fn back(b: &QuoteBook, i: usize) -> Option<&PxSample> {
    (i < b.len as usize).then(|| &b.samples[(b.head as usize + QPX_SAMPLES - i) % QPX_SAMPLES])
}

/// The step a new sample may take from `last`, `bps` per second for `secs` seconds, as a ratio:
/// (last / (1 + s), last x (1 + s)), s = bps x secs / 10,000 (saturating).
pub fn band(last: u128, bps_per_sec: u16, secs: i64) -> (u128, u128) {
    let s = (bps_per_sec as u128).saturating_mul(secs.max(0) as u128);
    let up = 10_000u128.saturating_add(s);
    (mul_div(last, 10_000, up).unwrap_or(0), mul_div(last, up, 10_000).unwrap_or(u128::MAX))
}

/// Record `spot` at `slot` (unix time `now`) when the last sample is at least `sample_gap_slots`
/// old. Returns whether a sample was taken.
///
/// Red team R7 (2026-10-23): anyone samples, so a pump, sample and dump in one transaction sets
/// the sample. While the book has a price, a sample is clamped into `band` of the previous one
/// (`max_step_bps_per_sec` x the seconds since it): one atomic sample moves the book by at most
/// one step, and owning every sample walks it at most that fast. A book with no price has no
/// anchor to clamp to, so it earns one: it restarts (the samples are dropped) when the legs
/// changed or the newest sample is older than `max_age_slots`, and while it warms up a sample
/// outside the band restarts it at that sample, so a price only exists after a long window of
/// samples that each moved within the band of the one before.
pub fn sample(b: &mut QuoteBook, e: &QuoteEntry, spot: u128, slot: u64, now: i64) -> Result<bool> {
    let legs = [e.legs[0].pool, e.legs[1].pool];
    if b.legs != legs {
        b.legs = legs;
        b.len = 0;
        b.head = 0;
    }
    if let Some(p) = back(b, 0) {
        if slot < p.slot.saturating_add(e.sample_gap_slots.max(1) as u64) {
            return Ok(false);
        }
        if slot > p.slot.saturating_add(e.max_age_slots as u64) {
            b.len = 0;
            b.head = 0;
        }
    }
    let mut rec = spot;
    if b.len > 0 {
        // a book sampled before the clamp existed has no time: its first sample repeats the price
        let secs = if b.last_ts == 0 { 0 } else { now.saturating_sub(b.last_ts) };
        let step = if e.max_step_bps_per_sec == 0 { DEFAULT_QUOTE_STEP_BPS_PER_SEC } else { e.max_step_bps_per_sec };
        let (lo, hi) = band(b.last_price, step, secs);
        if spot < lo || spot > hi {
            if twaps(b, e, slot).is_some() {
                rec = spot.clamp(lo, hi);
            } else {
                b.len = 0;
                b.head = 0;
            }
        }
    }
    let cum = match back(b, 0).copied() {
        Some(p) => p.cum.wrapping_add(b.last_price.wrapping_mul((slot - p.slot) as u128)),
        None => 0,
    };
    let head = if b.len == 0 { 0 } else { (b.head as usize + 1) % QPX_SAMPLES };
    b.samples[head] = PxSample { slot, cum };
    b.head = head as u8;
    b.len = (b.len as usize + 1).min(QPX_SAMPLES) as u8;
    b.last_price = rec;
    b.last_ts = now;
    emit!(QuoteSampled { mint: b.mint, slot, cum, spot: rec, raw: spot });
    Ok(true)
}

fn twap(a: &PxSample, b: &PxSample) -> Option<u128> {
    let dt = b.slot.checked_sub(a.slot).filter(|d| *d > 0)?;
    Some(b.cum.wrapping_sub(a.cum) / dt as u128)
}

/// (long TWAP, recent TWAP) at `slot`, or None: no sample within `max_age_slots`, or none
/// `long_window_slots` older than the newest, or the samples are from other legs.
fn twaps(b: &QuoteBook, e: &QuoteEntry, slot: u64) -> Option<(u128, u128)> {
    if b.legs != [e.legs[0].pool, e.legs[1].pool] || e.mint != b.mint {
        return None;
    }
    let newest = back(b, 0)?;
    if slot > newest.slot.saturating_add(e.max_age_slots as u64) {
        return None;
    }
    let recent = twap(back(b, 1)?, newest)?;
    let from = (1..b.len as usize).filter_map(|i| back(b, i)).find(|s| s.slot.saturating_add(e.long_window_slots as u64) <= newest.slot)?;
    let long = twap(from, newest)?;
    (long > 0 && recent > 0).then_some((long, recent))
}

/// Lamports per quote atom for valuing grants: min(long, recent). None = no fresh price.
pub fn price(b: &QuoteBook, e: &QuoteEntry, slot: u64) -> Option<u128> {
    twaps(b, e, slot).map(|(l, r)| l.min(r))
}

/// Lamports per quote atom for levfee's quote sales: max(long, recent).
pub fn price_high(b: &QuoteBook, e: &QuoteEntry, slot: u64) -> Option<u128> {
    twaps(b, e, slot).map(|(l, r)| l.max(r))
}

/// Lamports `atoms` of the quote are worth at `price` (floor).
pub fn lamports_for(atoms: u64, price: u128) -> Result<u64> {
    u64::try_from(m(mul_div(atoms as u128, price, Q64))?).map_err(|_| error!(SpotError::Math))
}

impl QuoteEntry {
    /// An entry `set_quote` accepts: an empty slot (default mint, everything zero), or a mint other
    /// than SOL with leg 0 set, leg 1 set or empty, sampling bounds as the platform price's.
    pub fn validate(&self) -> Result<()> {
        if self.mint == Pubkey::default() {
            require!(*self == QuoteEntry::default(), SpotError::BadParam);
            return Ok(());
        }
        require!(self.mint != WSOL && self.flags & !QUOTE_GRANTS == 0 && self.reserved == [0; 14] && self.max_step_bps_per_sec <= HARD_MAX_QUOTE_STEP_BPS_PER_SEC, SpotError::BadParam);
        let known = |k: u8| (LEG_DAMM_V2..=LEG_ORCA_WHIRLPOOL).contains(&k);
        require!(known(self.legs[0].kind) && self.legs[0].pool != Pubkey::default(), SpotError::BadParam);
        let l1 = &self.legs[1];
        require!((l1.kind == LEG_NONE && l1.pool == Pubkey::default()) || (known(l1.kind) && l1.pool != Pubkey::default()), SpotError::BadParam);
        PriceParams { sample_gap_slots: self.sample_gap_slots, long_window_slots: self.long_window_slots, max_age_slots: self.max_age_slots }.validate_with(QPX_SAMPLES)
    }
}
