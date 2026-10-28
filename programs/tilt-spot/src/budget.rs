//! Mint budget: mint tracks burn (levpad owner decision 2026-10-20, carried over).
//!
//! levcap's own daily ceiling is fixed and knows no price, so the real limit on how fast grants
//! create the platform token lives here. Over any trailing `window_days` UTC days, grants take at
//! most `k_bps` / 10,000 of the platform token levfee burned in those days. Before buybacks have
//! burned anything, grants draw on a lifetime `seed_atoms` allowance instead; burn room is always
//! used first.
//!
//! **Burn source.** levfee's `FeeState::burned`, its lifetime counter of burned atoms, read from
//! the raw account. The counter only grows, so a stale read only shrinks the room.
//!
//! **Days.** The first time a UTC day is seen it records the counter as its start. The burn in a
//! window is the counter now minus the start of the oldest recorded day inside the window.

use crate::errors::SpotError;
use crate::state::*;
use anchor_lang::prelude::*;

pub const LEVFEE_PROGRAM: Pubkey = pubkey!("6MBSd18Ce5pEJ28qXktoxxtJEvopukTBEBCVknvTkhbM");
/// `FeeState::burned`: discriminator 8, three pubkeys 96, three bumps 3, pending_burn,
/// pending_stakers, four income counters and burn_spent (7 x 8).
const BURNED_OFFSET: usize = 8 + 96 + 3 + 7 * 8;

#[event]
pub struct BurnsSeen {
    pub burned: u64,
    pub day: u32,
}

/// A grant was cut by the budget (the rest waits as pending value).
#[event]
pub struct GrantBudgeted {
    pub wanted: u64,
    pub granted: u64,
    pub from_seed: u64,
}

pub fn levfee_state() -> Pubkey {
    Pubkey::find_program_address(&[b"state"], &LEVFEE_PROGRAM).0
}

/// levfee's fee PDA ["fee"]; the router fee goes to its WSOL account.
pub fn levfee_fee_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"fee"], &LEVFEE_PROGRAM).0
}

/// levfee's ["flip", mint] PDA: the owner of a coin's flip-fee accrual (its associated account of
/// the coin's quote), which levfee's `fee_to_lp` adds to the coin's locked LP.
pub fn levfee_flip_authority(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"flip", mint.as_ref()], &LEVFEE_PROGRAM).0
}

/// The flip-fee accrual of `mint` in `quote` (under `quote_program`).
pub fn flip_account(mint: &Pubkey, quote: &Pubkey, quote_program: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address_with_program_id(&levfee_flip_authority(mint), quote, quote_program)
}

pub fn day_of(now: i64) -> u32 {
    (now.max(0) / 86_400) as u32
}

/// Read levfee's burned counter when `ai` is its state account. Returns whether it was.
pub fn refresh(cfg: &mut Config, ai: &AccountInfo, now: i64) -> Result<bool> {
    if ai.key() != levfee_state() || *ai.owner != LEVFEE_PROGRAM {
        return Ok(false);
    }
    let d = ai.try_borrow_data()?;
    let Some(b) = d.get(BURNED_OFFSET..BURNED_OFFSET + 8) else { return Ok(false) };
    let burned = u64::from_le_bytes(b.try_into().unwrap());
    let x = &mut cfg.mint;
    x.burned_seen = x.burned_seen.max(burned);
    let day = day_of(now);
    touch(x, day);
    emit!(BurnsSeen { burned: x.burned_seen, day });
    Ok(true)
}

/// Day `i` back from the newest (0 = newest).
fn back(x: &MintBook, i: usize) -> Option<&BudgetDay> {
    (i < x.day_len as usize).then(|| &x.days[(x.day_head as usize + BUDGET_DAYS - i) % BUDGET_DAYS])
}

/// Start `day` in the ring if it is newer than the newest day held.
fn touch(x: &mut MintBook, day: u32) {
    if back(x, 0).is_some_and(|d| d.day >= day) {
        return;
    }
    let head = if x.day_len == 0 { 0 } else { (x.day_head as usize + 1) % BUDGET_DAYS };
    x.days[head] = BudgetDay { day, burned_start: x.burned_seen, granted: 0 };
    x.day_head = head as u8;
    x.day_len = (x.day_len as usize + 1).min(BUDGET_DAYS) as u8;
}

/// Room on `day`: (burn side, seed side), atoms.
pub fn room(x: &MintBook, b: &MintBudget, day: u32) -> (u64, u64) {
    let start = (day + 1).saturating_sub(b.window_days.max(1) as u32);
    let mut oldest = None;
    let mut granted = 0u128;
    for i in 0..x.day_len as usize {
        let Some(d) = back(x, i) else { break };
        if d.day < start {
            break;
        }
        if d.day <= day {
            oldest = Some(d.burned_start);
            granted += d.granted as u128;
        }
    }
    let burned = x.burned_seen.saturating_sub(oldest.unwrap_or(x.burned_seen)) as u128;
    let cap = burned * b.k_bps as u128 / 10_000;
    let burn = cap.saturating_sub(granted).min(u64::MAX as u128) as u64;
    (burn, b.seed_atoms.saturating_sub(x.seed_used))
}

/// Take up to `amount` from the budget at `now`: the burn side first, then the seed. Returns what
/// was taken.
pub fn take(cfg: &mut Config, amount: u64, now: i64) -> u64 {
    if amount == 0 {
        return 0;
    }
    let b = cfg.params.budget;
    let x = &mut cfg.mint;
    let day = day_of(now);
    touch(x, day);
    let (burn, seed) = room(x, &b, day);
    let a = amount.min(burn.saturating_add(seed));
    let from_burn = a.min(burn);
    let from_seed = a - from_burn;
    let head = x.day_head as usize;
    x.days[head].granted = x.days[head].granted.saturating_add(from_burn);
    x.seed_used = x.seed_used.saturating_add(from_seed);
    if a < amount || from_seed > 0 {
        emit!(GrantBudgeted { wanted: amount, granted: a, from_seed });
    }
    a
}

impl MintBudget {
    pub fn validate(&self) -> Result<()> {
        require!(self.k_bps <= HARD_MAX_BUDGET_K_BPS, SpotError::BadParam);
        require!((1..=HARD_MAX_BUDGET_WINDOW_DAYS).contains(&self.window_days), SpotError::BadParam);
        Ok(())
    }
}
