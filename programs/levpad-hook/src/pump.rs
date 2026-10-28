//! pump.fun coins as listed markets (owner decision 2026-10-19): the hook keeps the same
//! observation ring for a coin it did not launch, written only by a permissionless crank that
//! samples the coin's pump.fun bonding curve and, after the migration, its canonical PumpSwap pool.
//!
//! The engine reads the ring exactly like a launched coin's (`OracleKind::Ring`, `obs_owner` =
//! this program, `["obs", mint]`), and `PumpLink` at `["damm", mint]` is what the engine's
//! `mark_graduated` looks for, so pump markets need no engine change. Listing stays with the
//! engine admin (`create_market` + `set_obs`); this program only keeps prices.
//!
//! A sampled ring can be poked between an attacker's own buy and sell. Defences (SPEC.md,
//! "pump.fun listed markets"):
//! - pokes must be top-level instructions in a transaction that calls only the compute-budget
//!   program, this program and the engine (`gate`), so nobody can buy, poke and sell in one
//!   transaction, directly or through a router that CPIs into pump;
//! - the first poke in a slot wins and each poke moves the price at most
//!   `MAX_POKE_STEP_BPS_PER_SLOT` per slot since the newest observation (`clamp_poke`), so a
//!   Jito bundle (buy, poke, sell as separate transactions) buys at most one clamped step;
//! - the recorded quote reserve, which sizes the engine's net cap, may rise at most the same step
//!   per slot (`clamp_reserve`), so a same-slot buy cannot inflate the cap (audit M6).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};

use crate::{HookError, MAX_POKE_STEP_BPS_PER_SLOT};

pub const PUMP_FEED_SEED: &[u8] = b"pump";
pub const COMPUTE_BUDGET: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
/// The base `clamp_reserve` grows from when the recorded reserve is smaller (an early curve holds
/// almost no real SOL).
pub const RESERVE_STEP_FLOOR: u64 = 1_000_000_000;

pub const PHASE_CURVE: u8 = 0;
pub const PHASE_AMM: u8 = 1;

/// One per listed pump coin, at ["pump", mint]: where its prices come from.
#[account]
#[derive(InitSpace)]
pub struct PumpFeed {
    pub mint: Pubkey,
    /// ["bonding-curve", mint] under the pump program, checked at init.
    pub bonding_curve: Pubkey,
    /// The canonical PumpSwap pool, set by `bind_pump_amm` (default before).
    pub pool: Pubkey,
    pub ring: Pubkey,
    /// `PHASE_CURVE` or `PHASE_AMM`.
    pub phase: u8,
    pub created_slot: u64,
    pub bound_slot: u64,
    pub bump: u8,
}

/// Written by `bind_pump_amm` at ["damm", mint]: the engine's `mark_graduated` reads bytes 8..40
/// (the mint) of whatever this program owns at that address, so a migrated pump coin graduates in
/// the engine exactly like a DBC coin (shorts after `short_min_age_secs`). A different account type
/// from `DammLink`, so `poke_damm` and the keeper's DAMM crank never see it.
#[account]
#[derive(InitSpace)]
pub struct PumpLink {
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub ring: Pubkey,
    pub bound_slot: u64,
    pub bump: u8,
}

#[event]
pub struct PumpBound {
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub seed_price: u128,
}

/// The program ids of every top-level instruction in the transaction, read straight from the
/// instructions sysvar (u16 count, u16 offsets, then per instruction u16 n_accounts,
/// n x (u8 flags, pubkey), program id, u16 data length, data).
pub fn tx_programs(d: &[u8]) -> Option<Vec<Pubkey>> {
    let rd16 = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let n = rd16(0)?;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let at = rd16(2 + 2 * i)?;
        let accs = rd16(at)?;
        let p = at + 2 + accs * 33;
        out.push(Pubkey::new_from_array(d.get(p..p + 32)?.try_into().ok()?));
    }
    Some(out)
}

/// Refuse unless this instruction is top-level and the transaction calls nothing but the
/// compute-budget program and this program: no swap on pump or PumpSwap, and no
/// other program that could CPI into them, can share the transaction with a pump poke or bind.
pub fn gate(ixs: &AccountInfo) -> Result<()> {
    require!(get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT, HookError::PokeNotTopLevel);
    require_keys_eq!(*ixs.key, anchor_lang::solana_program::sysvar::instructions::ID, HookError::PokeShared);
    let d = ixs.try_borrow_data()?;
    let progs = tx_programs(&d).ok_or(error!(HookError::PokeShared))?;
    for p in progs {
        require!(p == COMPUTE_BUDGET || p == crate::ID, HookError::PokeShared);
    }
    Ok(())
}

/// The quote reserve to record: what the venue holds now, but at most the last recorded one (or
/// `RESERVE_STEP_FLOOR` if larger) grown by `MAX_POKE_STEP_BPS_PER_SLOT` per slot since it was
/// recorded. A drop is recorded at once (it only shrinks the engine's cap).
pub fn clamp_reserve(last: Option<(u64, u64)>, slot: u64, observed: u64) -> u64 {
    let Some((last_slot, last_reserve)) = last else { return observed };
    let dt = slot.saturating_sub(last_slot) as u128;
    let base = last_reserve.max(RESERVE_STEP_FLOOR) as u128;
    let cap = base + base * (MAX_POKE_STEP_BPS_PER_SLOT * dt).min(1_000_000) / 10_000;
    (observed as u128).min(cap) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sysvar(progs: &[(Pubkey, usize)]) -> Vec<u8> {
        // same layout as solana_program::sysvar::instructions::serialize_instructions
        let mut d = vec![];
        d.extend_from_slice(&(progs.len() as u16).to_le_bytes());
        let head = d.len();
        d.resize(head + 2 * progs.len(), 0);
        for (i, (p, accs)) in progs.iter().enumerate() {
            let at = d.len() as u16;
            d[head + 2 * i..head + 2 * i + 2].copy_from_slice(&at.to_le_bytes());
            d.extend_from_slice(&(*accs as u16).to_le_bytes());
            for k in 0..*accs {
                d.push(1);
                d.extend_from_slice(&[k as u8; 32]);
            }
            d.extend_from_slice(p.as_ref());
            d.extend_from_slice(&3u16.to_le_bytes());
            d.extend_from_slice(&[9, 9, 9]);
        }
        d.extend_from_slice(&0u16.to_le_bytes());
        d
    }

    #[test]
    fn reads_program_ids() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let d = sysvar(&[(COMPUTE_BUDGET, 0), (a, 3), (b, 17)]);
        assert_eq!(tx_programs(&d).unwrap(), vec![COMPUTE_BUDGET, a, b]);
        assert!(tx_programs(&d[..d.len() - 40]).is_none());
    }

    #[test]
    fn reserve_rises_slowly_and_falls_at_once() {
        let s = 1_000_000_000u64;
        assert_eq!(clamp_reserve(None, 5, 77 * s), 77 * s);
        // 1% per slot (MAX_POKE_STEP_BPS_PER_SLOT, retuned from 2% for 0.212 s mainnet slots in
        // af9fbe3) of the larger of the last reserve and 1 SOL
        assert_eq!(MAX_POKE_STEP_BPS_PER_SLOT, 100);
        assert_eq!(clamp_reserve(Some((10, 50 * s)), 11, 80 * s), 50 * s + s / 2);
        assert_eq!(clamp_reserve(Some((10, 50 * s)), 15, 80 * s), 52 * s + s / 2);
        assert_eq!(clamp_reserve(Some((10, 0)), 11, 80 * s), s + s / 100);
        assert_eq!(clamp_reserve(Some((10, 50 * s)), 11, 3 * s), 3 * s);
        // the rise saturates at 100x the base (10,000 slots and more)
        assert_eq!(clamp_reserve(Some((10, 50 * s)), 1_000_000, u64::MAX), 50 * s * 101);
        // same slot: no rise at all
        assert_eq!(clamp_reserve(Some((10, 50 * s)), 10, 80 * s), 50 * s);
    }
}
