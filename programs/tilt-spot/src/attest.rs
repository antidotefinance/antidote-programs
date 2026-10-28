//! Attested loss receipts (owner 2026-10-26: "wherever you trade you get receipts for losses ...
//! regardless if it goes through our router"; SPEC.md "Attested receipts").
//!
//! On the curve every trade goes through the router (levpad-hook's fair open enforces it), so
//! routed receipts cover it. After graduation DBC revokes the coin's transfer hook, and a trade in
//! the coin's DAMM v2 pool that skips the router (Jupiter, a terminal, a direct swap) is invisible
//! to this program. For those sells an off-chain attester (the indexer reads every swap in the
//! pool, from any program, and keeps each wallet's cost across venues) signs
//! `book_attested_receipt` with the sell's numbers and a hash committing to the swap transactions
//! they come from. The program checks the coin is a counted pad coin that graduated, refuses a
//! sell signature it has seen before, and applies the routed receipt's rules unchanged (share,
//! minimum loss and value, the hold ramp, d, the issuance bucket, the per-receipt limit, the
//! delay schedule and the fast lane), plus a per-day attested budget and a pause of its own.
//!
//! The attester fronts the receipt's rent (and its NFT prefund, as a seller does); redemption
//! sends that rent back to the ["attest"] account rather than to the owner (anyone sweeps it to the
//! attester), so an attested receipt never pays its owner rent it did not front. The owner's NFT
//! token account (about 0.002 SOL, created by `issue_receipt` in the owner's wallet) stays the
//! owner's, as for any receipt issued by someone else.
//!
//! Trust: the attester is trusted for the numbers; the program bounds what a wrong or stolen
//! attester key can do (the per-day budget in atoms and receipts, the maximum sell age, the
//! issuance bucket and per-receipt limit every receipt already obeys, the pause, rotation and
//! `attester = default` to switch it off), and every attested receipt carries its evidence hash so
//! anyone can recompute the loss from the transactions (docs/redteam/attested-receipts-2026-10-26.md).

use crate::errors::SpotError;
use crate::receipt;
use crate::state::*;
use anchor_lang::prelude::*;
use levpad_hook::venue::{self, damm, DAMM_V2_PROGRAM, WSOL};

/// ["attest"]: the attester, its limits, today's use and the rent pool of attested receipts.
pub const ATTEST_SEED: &[u8] = b"attest";
/// ["attested", sig[0..32], sig[32..64]]: one per attested sell transaction, forever.
pub const ATTESTED_SEED: &[u8] = b"attested";

/// At most this many attested receipts a UTC day (the setting is the admin's, within it).
pub const HARD_MAX_ATTEST_DAY_RECEIPTS: u32 = 100_000;
/// At most the 1B supply (6 decimals) a day; the issuance bucket bounds it further.
pub const HARD_MAX_ATTEST_DAY_ATOMS: u64 = 1_000_000_000_000_000;
/// A sell may be attested from 1 minute to 30 days after it closed (the setting is the admin's).
pub const HARD_MIN_ATTEST_AGE_SECS: u32 = 60;
pub const HARD_MAX_ATTEST_AGE_SECS: u32 = 30 * 86_400;

/// After the `Receipt` struct, an attested receipt's account holds its evidence: the 32-byte
/// hash, the sell's 64-byte signature and the number of swaps (u16 LE) the loss came from.
pub const TAIL_EVIDENCE: usize = 0;
pub const TAIL_SIG: usize = 32;
pub const TAIL_SWAPS: usize = 96;
pub const TAIL_LEN: usize = 98;

/// The attester's limits (admin, at once: a compromised attester is answered within a block).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct AttestLimits {
    /// Platform-token atoms attested receipts may reserve per UTC day (0 = none).
    pub day_atoms: u64,
    /// Attested receipts per UTC day (0 = none).
    pub day_receipts: u32,
    /// The oldest sell (now - closed_at, seconds) that may still be attested.
    pub max_age_secs: u32,
}

impl AttestLimits {
    pub fn validate(&self) -> Result<()> {
        require!(self.day_atoms <= HARD_MAX_ATTEST_DAY_ATOMS, SpotError::BadParam);
        require!(self.day_receipts <= HARD_MAX_ATTEST_DAY_RECEIPTS, SpotError::BadParam);
        require!((HARD_MIN_ATTEST_AGE_SECS..=HARD_MAX_ATTEST_AGE_SECS).contains(&self.max_age_secs), SpotError::BadParam);
        Ok(())
    }
}

#[account]
#[derive(InitSpace, Debug)]
pub struct Attest {
    /// The only key that may sign `book_attested_receipt`; the default key switches it off.
    pub attester: Pubkey,
    /// Stops `book_attested_receipt` (admin or attester sets it, only the admin clears it).
    pub paused: bool,
    pub bump: u8,
    pub limits: AttestLimits,
    /// The UTC day (unix / 86,400) `day_atoms` and `day_receipts` count.
    pub day: i64,
    pub day_atoms: u64,
    pub day_receipts: u32,
    /// Lifetime: attested receipts, their atoms, value V and loss L (lamports).
    pub receipts: u64,
    pub atoms: u64,
    pub value_lamports: u64,
    pub loss_lamports: u64,
    /// Lamports the attester fronted for receipts and their NFTs (they come back here at redemption).
    pub rent_fronted: u64,
    pub reserved: [u8; 64],
}

/// ["attested", sig[0..32], sig[32..64]]: the sell transaction `sig` was attested (once, ever).
#[account]
#[derive(InitSpace, Debug)]
pub struct Attestation {
    pub owner: Pubkey,
    pub coin: Pubkey,
    pub receipt: Pubkey,
    pub evidence: [u8; 32],
    pub closed_at: i64,
    pub booked_at: i64,
    pub bump: u8,
}

/// One unrouted sell at a loss, as the attester computed it (all amounts lamports or atoms).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttestedSell {
    /// The sell transaction's signature (64 bytes).
    pub sig: [u8; 64],
    /// Cost of the atoms sold at the wallet's average cost across every venue (fees in).
    pub cost_lamports: u64,
    /// SOL the sell returned (net of the venue's fees).
    pub proceeds_lamports: u64,
    pub atoms_sold: u64,
    /// The sell's block time (unix seconds).
    pub closed_at: i64,
    /// Seconds from the size-weighted entry of the atoms held to the sell (the hold ramp's input).
    pub held_secs: i64,
    /// How many swap transactions (buys, sells and the sell itself) the numbers come from.
    pub swaps: u16,
    /// sha256 of those transactions' signatures (64 bytes each) in the order applied, the sell last.
    pub evidence: [u8; 32],
}

#[event]
pub struct AttestedReceipt {
    pub owner: Pubkey,
    pub receipt: Pubkey,
    pub coin: Pubkey,
    pub attester: Pubkey,
    pub sig: [u8; 64],
    pub evidence: [u8; 32],
    pub swaps: u16,
    pub cost_lamports: u64,
    pub proceeds_lamports: u64,
    pub atoms_sold: u64,
    pub held_secs: i64,
    pub closed_at: i64,
    /// Routed atoms the sell took off the wallet's routed position (so a later routed sell
    /// cannot count their cost again).
    pub position_atoms: u64,
    pub position_cost: u64,
}

#[event]
pub struct AttestSet {
    pub attester: Pubkey,
    pub paused: bool,
    pub limits: AttestLimits,
}

pub fn attest_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[ATTEST_SEED], &crate::ID)
}

pub fn attestation_address(sig: &[u8; 64]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[ATTESTED_SEED, &sig[..32], &sig[32..]], &crate::ID)
}

/// The coin is a pad coin (its DBC pool's config is counted; never the platform token) that
/// graduated: DBC marks the pool migrated and levpad-hook bound its DAMM v2 pool, which pairs it
/// with WSOL (attested receipts are for SOL coins).
pub fn check_graduated(cfg: &Config, mint: &Pubkey, dbc_pool: &AccountInfo, damm_link: &AccountInfo, damm_pool: &AccountInfo) -> Result<()> {
    require!(*mint != cfg.token_mint, SpotError::NotPadCoin);
    {
        let d = dbc_pool.try_borrow_data()?;
        require!(venue::is_dbc_pool(dbc_pool.owner, &d, mint), SpotError::WrongAccount);
        let cfg_key = venue::key_at(&d, venue::dbc::CONFIG);
        require!(cfg_key != Pubkey::default() && cfg.params.dbc_configs.contains(&cfg_key), SpotError::NotPadCoin);
        require!(d[venue::dbc::IS_MIGRATED] == 1, SpotError::NotGraduated);
    }
    require_keys_eq!(damm_link.key(), levpad_hook::damm_link_address(mint).0, SpotError::NotGraduated);
    require!(*damm_link.owner == levpad_hook::ID && !damm_link.data_is_empty(), SpotError::NotGraduated);
    let link = levpad_hook::DammLink::try_deserialize(&mut &damm_link.try_borrow_data()?[..])?;
    require!(link.mint == *mint && link.damm_pool == damm_pool.key(), SpotError::NotGraduated);
    require_keys_eq!(*damm_pool.owner, DAMM_V2_PROGRAM, SpotError::WrongAccount);
    let p = damm_pool.try_borrow_data()?;
    require!(p.len() >= damm::POOL_LEN && p[..8] == damm::POOL_DISC, SpotError::WrongAccount);
    let (ka, kb) = (venue::key_at(&p, damm::TOKEN_A_MINT), venue::key_at(&p, damm::TOKEN_B_MINT));
    require!((ka == *mint && kb == WSOL) || (ka == WSOL && kb == *mint), SpotError::QuoteNotAllowed);
    Ok(())
}

/// Roll the per-day counters to `now`'s UTC day.
pub fn roll_day(a: &mut Attest, now: i64) {
    let day = now.div_euclid(86_400);
    if a.day != day {
        a.day = day;
        a.day_atoms = 0;
        a.day_receipts = 0;
    }
}

/// Take the routed atoms an unrouted sell removed from the wallet off its routed position, at the
/// position's average cost (what a routed sell of them would have done), so they cannot count
/// again. Returns (atoms, cost) taken. A position that does not exist takes nothing.
pub fn consume_position(pos_ai: &AccountInfo, owner: &Pubkey, mint: &Pubkey, atoms_sold: u64) -> Result<(u64, u64)> {
    let want = Pubkey::find_program_address(&[POSITION_SEED, owner.as_ref(), mint.as_ref()], &crate::ID).0;
    require_keys_eq!(pos_ai.key(), want, SpotError::WrongAccount);
    if pos_ai.data_is_empty() || *pos_ai.owner != crate::ID {
        return Ok((0, 0));
    }
    let mut p = Position::try_deserialize(&mut &pos_ai.try_borrow_data()?[..])?;
    let tracked = atoms_sold.min(p.tokens);
    if tracked == 0 {
        return Ok((0, 0));
    }
    let cost = u64::try_from(crate::errors::m(levpad_math::mul_div(p.cost as u128, tracked as u128, p.tokens as u128))?).map_err(|_| error!(SpotError::Math))?;
    p.tokens -= tracked;
    p.cost -= cost;
    let mut d = pos_ai.try_borrow_mut_data()?;
    p.try_serialize(&mut &mut d[..])?;
    Ok((tracked, cost))
}

/// Write an attested receipt's evidence after the `Receipt` struct.
pub fn write_tail(ai: &AccountInfo, sell: &AttestedSell) -> Result<()> {
    let at = 8 + Receipt::INIT_SPACE;
    let mut d = ai.try_borrow_mut_data()?;
    require!(d.len() >= at + TAIL_LEN, SpotError::Math);
    d[at + TAIL_EVIDENCE..at + TAIL_EVIDENCE + 32].copy_from_slice(&sell.evidence);
    d[at + TAIL_SIG..at + TAIL_SIG + 64].copy_from_slice(&sell.sig);
    d[at + TAIL_SWAPS..at + TAIL_SWAPS + 2].copy_from_slice(&sell.swaps.to_le_bytes());
    Ok(())
}

/// The receipt for an attested sell, sized like a routed one; (V, A) are on the receipt. Fails
/// (the attester retries or drops it) where a routed sell would book no receipt or wait for a
/// price: share 0, value or counted loss under the minimum, no price, an empty bucket or room,
/// or the attested day budget spent.
#[allow(clippy::too_many_arguments)]
pub fn size_attested(cfg: &mut Config, at: &mut Attest, trader: &Trader, sell: &AttestedSell, owner: Pubkey, coin: Pubkey, seq: u64, rbump: u8, cap: &levcap::Cap, supply: u64, price: Option<u128>, mcap_px: Option<u128>, now: i64) -> Result<(Receipt, u64)> {
    let rp = cfg.receipts;
    require!(rp.share_bps > 0, SpotError::BelowMinimum);
    let loss = sell.cost_lamports - sell.proceeds_lamports;
    let h = receipt::h_ppm(sell.held_secs, rp.hold_secs);
    let d = receipt::d_ppm(receipt::room_left(cap, supply), rp.room_ref_atoms);
    let v = receipt::value(loss, rp.share_bps, h, d);
    require!(v > 0 && v >= rp.min_value_lamports, SpotError::BelowMinimum);
    require!(loss as u128 * h as u128 / receipt::PPM as u128 >= rp.min_loss_lamports as u128, SpotError::BelowMinimum);
    let px = price.ok_or(SpotError::NoPrice)?;
    let (amount, wanted, _, bucket_after) = receipt::size(cfg, v, px, cap, supply, now)?;
    require!(amount > 0, SpotError::NoMintRoom);
    // the attested day budget: a receipt bigger than a whole day's budget is scaled to it (as the
    // bucket scales), one that does not fit what is left today waits for tomorrow
    let amount_final = amount.min(at.limits.day_atoms);
    require!(amount_final > 0 && at.day_receipts < at.limits.day_receipts, SpotError::AttestBudget);
    require!(at.day_atoms.saturating_add(amount_final) <= at.limits.day_atoms, SpotError::AttestBudget);
    let rate = (amount_final as u128 * receipt::PPM as u128 / wanted.max(1) as u128) as u32;
    let mut r = Receipt {
        owner,
        seq,
        number: 0,
        bump: rbump,
        status: RECEIPT_PENDING_NFT,
        close_auth: false,
        coin,
        quote: WSOL,
        loss,
        loss_sol: loss,
        share_bps: rp.share_bps,
        h_ppm: h,
        d_ppm: d,
        rate_ppm: rate,
        value_sol: v,
        amount: amount_final,
        amount_wanted: wanted,
        price: px,
        closed_at: sell.closed_at,
        delay_secs: 0,
        in_lane: false,
        redeemable_from: 0,
        auto_redeems_at: 0,
        issued_at: 0,
        attested: true,
        reserved: [0; 26],
    };
    receipt::take(cfg, &r, bucket_after + (amount - amount_final), now);
    // the delay from now (the booking), so its 15 min floor always runs after the receipt exists
    let (delay, lane) = receipt::delay_for(&r, cfg, trader, now, mcap_px, supply);
    r.delay_secs = delay;
    r.in_lane = lane;
    r.redeemable_from = now.saturating_add(delay as i64);
    r.auto_redeems_at = r.redeemable_from;
    at.day_atoms += amount_final;
    at.day_receipts += 1;
    at.receipts += 1;
    at.atoms = at.atoms.saturating_add(amount_final);
    at.value_lamports = at.value_lamports.saturating_add(v);
    at.loss_lamports = at.loss_lamports.saturating_add(loss);
    Ok((r, v))
}
