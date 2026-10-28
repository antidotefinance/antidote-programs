//! The platform token's anti-farming price (levpad's antifarm.rs, carried over).
//!
//! Grants are sized in lamports of value and converted to tokens at max(long TWAP, recent TWAP).
//! A dump lowers neither unless it is held for the whole long window; a pump only raises the
//! price, so it only shrinks grants.
//!
//! **Source.** The platform token launched with levpad-hook, whose ring ["obs", mint] keeps a
//! wrapping cumulative price (Q64.64 lamports per atom, times slots) on every curve swap and, after
//! graduation, on every DAMM v2 poke. The ring covers minutes on a busy token, so the engine
//! samples its cumulative (`sample`, at most once per `sample_gap_slots`) into `Config::px`; two
//! samples give the exact TWAP between them however far apart they are.

use crate::errors::{m, SpotError};
use crate::state::*;
use anchor_lang::prelude::*;
use levpad_math::{mul_div, Q64};

pub const HOOK_PROGRAM: Pubkey = levpad_hook::ID;
pub const OBS_SEED: &[u8] = b"obs";
/// Ring layout (levpad-hook ring.rs): 0..8 magic, 8..40 header key, 40..44 head, 44..48 len,
/// 48..52 cap, 52..64 pad and last quote reserve, then 40-byte observations (slot u64, cum u128,
/// price u128).
const RING_MAGIC: [u8; 8] = *b"LVOBSRNG";
const RING_HEADER: usize = 64;
const OBS_LEN: usize = 40;

#[event]
pub struct PriceSampled {
    pub slot: u64,
    pub cum: u128,
    /// TWAP since the previous sample (0 for the first).
    pub twap: u128,
}

/// The platform token's hook ring. None before a token is set.
pub fn ring_key(cfg: &Config) -> Option<Pubkey> {
    (cfg.token_mint != Pubkey::default()).then(|| Pubkey::find_program_address(&[OBS_SEED, cfg.token_mint.as_ref()], &HOOK_PROGRAM).0)
}

/// The ring's cumulative price at `slot` (its newest observation extended at its price).
fn ring_cum(d: &[u8], slot: u64) -> Option<u128> {
    if d.len() < RING_HEADER || d[..8] != RING_MAGIC {
        return None;
    }
    let rd = |o: usize, n: usize| -> &[u8] { &d[o..o + n] };
    let head = u32::from_le_bytes(rd(40, 4).try_into().ok()?) as usize;
    let len = u32::from_le_bytes(rd(44, 4).try_into().ok()?);
    let cap = u32::from_le_bytes(rd(48, 4).try_into().ok()?) as usize;
    if len == 0 || head >= cap || d.len() < RING_HEADER + cap * OBS_LEN {
        return None;
    }
    let o = RING_HEADER + head * OBS_LEN;
    let s = u64::from_le_bytes(rd(o, 8).try_into().ok()?);
    let cum = u128::from_le_bytes(rd(o + 8, 16).try_into().ok()?);
    let price = u128::from_le_bytes(rd(o + 24, 16).try_into().ok()?);
    // the price set in this slot holds for 0 slots yet: a same-slot dump counts nothing
    Some(cum.wrapping_add(price.wrapping_mul(slot.saturating_sub(s) as u128)))
}

fn twap(a: &PxSample, b: &PxSample) -> Option<u128> {
    let dt = b.slot.checked_sub(a.slot).filter(|d| *d > 0)?;
    let d = b.cum.wrapping_sub(a.cum);
    // a difference this large means the cumulative went backwards (a restarted ring): no price
    (d < 1u128 << 127).then(|| d / dt as u128)
}

/// Sample `i` back from the newest (0 = newest).
fn back(c: &Config, i: usize) -> Option<&PxSample> {
    (i < c.px.len as usize).then(|| &c.px.samples[(c.px.head as usize + PX_SAMPLES - i) % PX_SAMPLES])
}

/// Record a sample from `ring` when it is the platform token's hook ring and the last sample is
/// at least `sample_gap_slots` old. Returns whether one was taken; never fails on a wrong ring.
pub fn sample(cfg: &mut Config, ring: &AccountInfo, slot: u64) -> Result<bool> {
    let Some(want) = ring_key(cfg) else { return Ok(false) };
    if ring.key() != want || *ring.owner != HOOK_PROGRAM {
        return Ok(false);
    }
    let Some(cum) = ring_cum(&ring.try_borrow_data()?, slot) else { return Ok(false) };
    let prev = back(cfg, 0).copied();
    if let Some(p) = prev {
        if slot < p.slot.saturating_add(cfg.params.price.sample_gap_slots.max(1) as u64) {
            return Ok(false);
        }
    }
    let s = PxSample { slot, cum };
    let x = &mut cfg.px;
    let head = if x.len == 0 { 0 } else { (x.head as usize + 1) % PX_SAMPLES };
    x.samples[head] = s;
    x.head = head as u8;
    x.len = (x.len as usize + 1).min(PX_SAMPLES) as u8;
    let tw = prev.as_ref().and_then(|p| twap(p, &s)).unwrap_or(0);
    emit!(PriceSampled { slot, cum, twap: tw });
    Ok(true)
}

/// max(long TWAP, recent TWAP) at `slot`. None when the newest sample is older than
/// `max_age_slots`, or no sample is at least `long_window_slots` older than the newest.
pub fn price(c: &Config, slot: u64) -> Option<u128> {
    let p = &c.params.price;
    let newest = back(c, 0)?;
    if slot > newest.slot.saturating_add(p.max_age_slots as u64) {
        return None;
    }
    let recent = twap(back(c, 1)?, newest)?;
    let long_from = (1..c.px.len as usize).filter_map(|i| back(c, i)).find(|s| s.slot.saturating_add(p.long_window_slots as u64) <= newest.slot)?;
    let long = twap(long_from, newest)?;
    Some(long.max(recent)).filter(|p| *p > 0)
}

/// 24 hours of slots at the 0.212 s mainnet slot time measured 2026-10-23 (was 216,000: 24 h at
/// 0.4 s, 12.7 h at 0.212 s).
pub const DAY_SLOTS: u64 = 407_500;

/// The TWAP from the oldest sample at most `window` slots before the newest to the newest: the
/// longest window the sample book holds up to `window` (32 samples at the default 1,416-slot gap
/// span about 44,000 slots, 2.6 h at 0.212 s; a 24 h window needs a gap of at least 13,150
/// slots, which also makes the anti-farming price's freshness `max_age_slots` at least that).
/// None without a fresh newest sample or a second sample.
pub fn twap_back(c: &Config, slot: u64, window: u64) -> Option<u128> {
    let newest = back(c, 0)?;
    if slot > newest.slot.saturating_add(c.params.price.max_age_slots as u64) {
        return None;
    }
    let from = (1..c.px.len as usize).filter_map(|i| back(c, i)).take_while(|s| newest.slot.saturating_sub(s.slot) <= window).last()?;
    twap(from, newest).filter(|p| *p > 0)
}

/// Atoms `value` lamports buys at `price` (Q64.64 lamports per atom), saturating.
pub fn tokens_for(value: u64, price: u128) -> Result<u64> {
    require!(price > 0, SpotError::Math);
    Ok(match mul_div(value as u128, Q64, price) {
        Ok(v) => v.min(u64::MAX as u128) as u64,
        Err(_) => u64::MAX,
    })
}

/// Lamports `atoms` is worth at `price`, rounded up (so converting back never undercounts what
/// was used).
pub fn value_of(atoms: u64, price: u128) -> Result<u64> {
    let v = m(mul_div(atoms as u128, price, Q64))?;
    let exact = m(mul_div(v, Q64, price))? == atoms as u128;
    let v = if exact { v } else { v + 1 };
    u64::try_from(v).map_err(|_| error!(SpotError::Math))
}

impl PriceParams {
    pub fn validate(&self) -> Result<()> {
        self.validate_with(PX_SAMPLES)
    }

    /// The bounds for a ring of `samples` samples (the platform token's, or a quote's book).
    pub fn validate_with(&self, samples: usize) -> Result<()> {
        require!((HARD_MIN_SAMPLE_GAP..=HARD_MAX_SAMPLE_GAP).contains(&self.sample_gap_slots), SpotError::BadParam);
        // the long window must fit in the sample ring at the gap
        require!(self.long_window_slots >= self.sample_gap_slots, SpotError::BadParam);
        require!((self.long_window_slots as u64) <= self.sample_gap_slots as u64 * (samples as u64 - 2), SpotError::BadParam);
        require!(self.max_age_slots >= self.sample_gap_slots && self.max_age_slots <= HARD_MAX_PX_AGE, SpotError::BadParam);
        Ok(())
    }
}
