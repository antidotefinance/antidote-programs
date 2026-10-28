//! Loss receipts (owner decisions 2026-10-23; SPEC.md "Loss receipts").
//!
//! A routed sell that realizes a loss L on a pad coin (never the platform token) earns
//! V = share x L x h x d lamports of value: `share` of the loss, h = min(1, held / H) with held
//! counted from the position's size-weighted entry, d = min(1, room_left / room_ref) with
//! room_left = levcap's lifetime cap - (supply + owed). Coins quoted in another mint value L in
//! SOL at the quote's min TWAP. V buys A = V / max(spot, TWAP) atoms of the platform token, at
//! most what the issuance bucket holds (it refills continuously at `rate_bps_per_day` of
//! room_left per day, up to a day's worth; a bigger receipt is scaled down, the scale recorded).
//! A is reserved at levcap at once (owed), so every receipt is honored.
//!
//! The proof is a soulbound Token-2022 NFT in the loser's wallet: NonTransferable,
//! PermanentDelegate and MintCloseAuthority (both the ["authority"] PDA, so redemption can burn
//! it and close the mint), MetadataPointer to itself and TokenMetadata with the numbers as
//! additional fields. `redeem_receipt` (anyone from `redeemable_from`: the delay `delay_for` gave
//! it, 15 min to 48 h; the keeper redeems it then) burns it, closes the mint,
//! the token account (when its close authority is ours) and the receipt, all rent to the owner,
//! and mints A through levcap to the owner's associated account.
//!
//! The sell books the receipt (and reserves A) with its NFT's rent prefunded by the seller;
//! `issue_receipt` (the owner, signing with the sell, or anyone: the keeper within seconds)
//! makes the NFT from that prefund. Measured: the NFT alone is about 200k compute units (eleven
//! metadata fields at 7k to 18k each), so a sell plus the NFT does not fit 400k. Without a platform-token price
//! (or a quote price) at the close, the receipt waits with its V (or L in quote atoms) and
//! `issue_receipt` prices it when one exists.

use crate::errors::SpotError;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_lang::system_program;
use anchor_spl::token_2022::spl_token_2022::{
    self,
    extension::ExtensionType,
    instruction::AuthorityType,
    state::{Account as T22Account, Mint as T22Mint},
};
use anchor_spl::token_2022_extensions::spl_token_metadata_interface::{self as tmi, state::Field};
use levpad_math::{mul_div, Q64};
use crate::state::BPS;

pub const PPM: u64 = 1_000_000;
pub const RECEIPT_SYMBOL: &str = "PROOF";
pub const REDEEM_TEXT: &str = "burned when redeemed (by you or automatically, from redeemable_from); mints the antidote to this wallet";

#[event]
pub struct ReceiptBooked {
    pub owner: Pubkey,
    pub receipt: Pubkey,
    pub coin: Pubkey,
    pub quote: Pubkey,
    pub status: u8,
    pub loss: u64,
    pub loss_sol: u64,
    pub share_bps: u16,
    pub h_ppm: u32,
    pub d_ppm: u32,
    pub rate_ppm: u32,
    pub value_sol: u64,
    pub amount: u64,
    pub price: u128,
    pub closed_at: i64,
    pub delay_secs: u32,
    pub in_lane: bool,
    pub redeemable_from: i64,
    pub auto_redeems_at: i64,
}

#[event]
pub struct ReceiptIssued {
    pub owner: Pubkey,
    pub receipt: Pubkey,
    pub number: u64,
    pub receipt_mint: Pubkey,
    pub amount: u64,
    pub value_sol: u64,
    pub rate_ppm: u32,
    pub redeemable_from: i64,
    pub auto_redeems_at: i64,
}

#[event]
pub struct ReceiptRedeemed {
    pub owner: Pubkey,
    pub receipt: Pubkey,
    pub number: u64,
    pub amount: u64,
    pub by: Pubkey,
    /// Lamports of rent returned to the owner (receipt, NFT mint, token account).
    pub rent_returned: u64,
}

#[event]
pub struct ReceiptParamsSet {
    pub eta: i64,
    pub params: ReceiptParams,
}

impl ReceiptParams {
    pub fn validate(&self) -> Result<()> {
        require!(self.share_bps <= HARD_MAX_RECEIPT_SHARE_BPS, SpotError::BadParam);
        require!(self.hold_secs <= HARD_MAX_HOLD_SECS, SpotError::BadParam);
        require!((HARD_MIN_ROOM_REF_ATOMS..=HARD_MAX_ROOM_REF_ATOMS).contains(&self.room_ref_atoms), SpotError::BadParam);
        require!(self.min_delay_secs >= HARD_MIN_DELAY_SECS && self.max_delay_secs <= HARD_MAX_DELAY_SECS && self.min_delay_secs <= self.max_delay_secs, SpotError::BadParam);
        require!(self.delay_a_bps <= HARD_MAX_DELAY_A_BPS, SpotError::BadParam);
        require!(self.i_double_atoms == 0 || self.i_double_atoms >= 1_000_000, SpotError::BadParam);
        require!(self.rate_bps_per_day > 0 && self.rate_bps_per_day <= HARD_MAX_RATE_BPS_PER_DAY, SpotError::BadParam);
        require!(self.min_value_lamports <= HARD_MAX_MIN_VALUE_LAMPORTS, SpotError::BadParam);
        require!(self.reserved == [0; 14] && self.i_free_atoms <= HARD_MAX_I_FREE_ATOMS, SpotError::BadParam);
        require!(self.lane_lamports <= HARD_MAX_LANE_LAMPORTS, SpotError::BadParam);
        require!((HARD_MIN_LANE_WINDOW_SECS..=HARD_MAX_LANE_WINDOW_SECS).contains(&self.lane_window_secs), SpotError::BadParam);
        require!(self.min_loss_lamports <= HARD_MAX_MIN_LOSS_LAMPORTS && self.max_receipt_bucket_bps as u64 <= BPS, SpotError::BadParam);
        let n = self.uri.iter().position(|b| *b == 0).unwrap_or(RECEIPT_URI_LEN);
        require!(self.uri[n..].iter().all(|b| *b == 0) && core::str::from_utf8(&self.uri[..n]).is_ok(), SpotError::BadParam);
        Ok(())
    }

    pub fn uri_str(&self) -> &str {
        let n = self.uri.iter().position(|b| *b == 0).unwrap_or(RECEIPT_URI_LEN);
        core::str::from_utf8(&self.uri[..n]).unwrap_or("")
    }
}

/// Counted loss for the fast lane: L in SOL after the hold ramp (L x h). None while a quote
/// coin's loss waits for a quote price (then the era delay applies).
pub fn counted_loss(r: &Receipt) -> Option<u64> {
    if r.loss_sol == 0 && r.loss > 0 {
        return None;
    }
    Some((r.loss_sol as u128 * r.h_ppm as u128 / PPM as u128) as u64)
}

/// The wallet's counted loss already in its lane window at `now` (0 once the window has ended).
pub fn lane_used(t: &Trader, window: u32, now: i64) -> u64 {
    if now >= t.lane_start.saturating_add(window as i64) {
        0
    } else {
        t.lane_loss
    }
}

/// The price the delay's market cap uses (the redeem-delay sim, 2026-10-23): min(spot, TWAP over
/// up to 24 h) of the platform token, the TWAP from `price::twap_back` (the longest window the
/// sample book holds, up to 24 h). Spot alone, or the TWAP alone, when only one exists.
pub fn mcap_price(cfg: &Config, ring: Option<&AccountInfo>, slot: u64) -> Option<u128> {
    let twap = crate::price::twap_back(cfg, slot, crate::price::DAY_SLOTS);
    let spot = ring.and_then(|r| ring_spot(cfg, r, slot));
    match (spot, twap) {
        (Some(s), Some(t)) => Some(s.min(t)),
        (a, b) => a.or(b),
    }
}

/// The exponent's market-cap term: a x log2(mcap / mcap_ref), Q64.64, mcap = `mcap_price` x the
/// platform token's supply (circulating: all of it outside nobody's hands but burned). 0 without a
/// reference or a price; a market cap under the reference gives a negative term (the delay's
/// floor then holds).
pub fn mcap_term(p: &ReceiptParams, twap: Option<u128>, supply: u64) -> i128 {
    let (Some(px), true) = (twap, p.mcap_ref_lamports > 0) else { return 0 };
    let Ok(mcap) = mul_div(px, supply as u128, Q64) else { return 0 };
    let Ok(ratio) = mul_div(mcap, Q64, p.mcap_ref_lamports as u128) else { return i128::MAX / 4 };
    match levpad_math::expo::log2_q64(ratio) {
        Some(l) => l / BPS as i128 * p.delay_a_bps as i128 + (l % BPS as i128) * p.delay_a_bps as i128 / BPS as i128,
        None => i128::MIN / 4,
    }
}

/// The exponent's issuance term: max(0, issued - i_free) / i_double, Q64.64 (owner 2026-10-23:
/// the first i_free issued wait the floor). 0 when i_double is 0.
pub fn issued_term(p: &ReceiptParams, issued: u64) -> i128 {
    if p.i_double_atoms == 0 {
        return 0;
    }
    let issued = issued.saturating_sub(p.i_free_atoms);
    (mul_div(issued as u128, Q64, p.i_double_atoms as u128).unwrap_or(u128::MAX >> 2)).min(u128::MAX >> 2) as i128
}

/// min x 2^exponent, clamped to [min, max] (exponent Q64.64).
pub fn curve_delay(p: &ReceiptParams, exponent: i128) -> u32 {
    let (lo, hi) = (p.min_delay_secs, p.max_delay_secs.max(p.min_delay_secs));
    if exponent <= 0 {
        return lo;
    }
    match levpad_math::expo::exp2_q64(exponent) {
        Some(f) => u32::try_from(mul_div(lo as u128, f, Q64).unwrap_or(u128::MAX)).unwrap_or(u32::MAX).clamp(lo, hi),
        None => hi,
    }
}

/// How long after the close `r` redeems (owner 2026-10-23: the computed delay is the unlock for
/// everyone; the keeper redeems it then), and whether through the fast lane:
/// `min_delay` when the lane is on and at least half of the receipt's counted loss fits in what is
/// left of the wallet's lane; else min_delay x 2^(a x log2(mcap_twap / mcap_ref) + max(0, issued -
/// i_free) / i_double), clamped to [min_delay, max_delay]. `issued` is cumulative receipts issued before
/// this one (monotonic, not net of redemptions); `twap` the platform token's TWAP; `supply` its
/// mint supply.
pub fn delay_for(r: &Receipt, cfg: &Config, trader: &Trader, now: i64, twap: Option<u128>, supply: u64) -> (u32, bool) {
    let p = &cfg.receipts;
    if p.lane_enabled {
        if let Some(c) = counted_loss(r).filter(|c| *c > 0) {
            let left = p.lane_lamports.saturating_sub(lane_used(trader, p.lane_window_secs, now));
            if (c.min(left) as u128) * 2 >= c as u128 {
                return (p.min_delay_secs, true);
            }
        }
    }
    let issued = cfg.receipt_book.atoms_reserved.saturating_sub(r.amount);
    let e = mcap_term(p, twap, supply).saturating_add(issued_term(p, issued));
    (curve_delay(p, e), false)
}

/// Book the receipt's counted loss in the wallet's lane window (a new window starts at the first
/// loss after the last one ended).
pub fn lane_record(t: &mut Trader, window: u32, counted: u64, now: i64) {
    if now >= t.lane_start.saturating_add(window as i64) {
        t.lane_start = now;
        t.lane_loss = 0;
    }
    t.lane_loss = t.lane_loss.saturating_add(counted);
}

pub fn receipt_address(owner: &Pubkey, seq: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[RECEIPT_SEED, owner.as_ref(), &seq.to_le_bytes()], &crate::ID)
}
pub fn receipt_mint_address(receipt: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[RECEIPT_MINT_SEED, receipt.as_ref()], &crate::ID)
}
pub fn receipt_token_address(receipt: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[RECEIPT_TOKEN_SEED, receipt.as_ref()], &crate::ID)
}

// ---------- the numbers ----------

/// h in ppm: held seconds over H, at most 1.
pub fn h_ppm(held: i64, hold_secs: u32) -> u32 {
    if hold_secs == 0 {
        return PPM as u32;
    }
    let held = held.max(0) as u128;
    (held * PPM as u128 / hold_secs as u128).min(PPM as u128) as u32
}

/// d in ppm: room left over room_ref, at most 1.
pub fn d_ppm(room_left: u64, room_ref: u64) -> u32 {
    if room_ref == 0 {
        return PPM as u32;
    }
    (room_left as u128 * PPM as u128 / room_ref as u128).min(PPM as u128) as u32
}

/// V = L x share x h x d (floor).
pub fn value(loss: u64, share_bps: u16, h: u32, d: u32) -> u64 {
    let v = loss as u128 * share_bps as u128 * h as u128 * d as u128 / (BPS as u128 * PPM as u128 * PPM as u128);
    v.min(u64::MAX as u128) as u64
}

/// The most one receipt may take: `max_receipt_bucket_bps` of a day's issuance (rate x room
/// left), or no limit (0 or 10,000).
pub fn receipt_limit(p: &ReceiptParams, room_left: u64) -> u64 {
    let b = p.max_receipt_bucket_bps as u128;
    if b == 0 || b >= BPS as u128 {
        return u64::MAX;
    }
    let day = room_left as u128 * p.rate_bps_per_day as u128 / BPS as u128;
    (day * b / BPS as u128).min(u64::MAX as u128) as u64
}

/// The bucket after refilling to `now`: + rate x room_left x elapsed / 1 day, at most a day's
/// worth (rate x room_left). It starts full.
pub fn refill(book: &ReceiptBook, rate_bps: u16, room_left: u64, now: i64) -> u64 {
    let day = room_left as u128 * rate_bps as u128 / BPS as u128;
    if book.bucket_ts == 0 {
        return day.min(u64::MAX as u128) as u64;
    }
    let elapsed = (now - book.bucket_ts).max(0) as u128;
    let add = day * elapsed / 86_400;
    (book.bucket_atoms as u128 + add).min(day).min(u64::MAX as u128) as u64
}

/// The platform token's spot: its hook ring's newest price (Q64.64 lamports per atom) set before
/// `slot`. An observation from this very slot is skipped for the one before it (red team
/// 2026-10-23, R5: a dump in the same transaction as a sell set the delay's market cap): moving
/// it now takes a trade in an earlier slot, which anyone may arbitrage.
pub fn ring_spot(cfg: &Config, ring: &AccountInfo, slot: u64) -> Option<u128> {
    let want = crate::price::ring_key(cfg)?;
    if ring.key() != want || *ring.owner != crate::price::HOOK_PROGRAM {
        return None;
    }
    let d = ring.try_borrow_data().ok()?;
    if d.len() < 64 || d[..8] != *b"LVOBSRNG" {
        return None;
    }
    let head = u32::from_le_bytes(d[40..44].try_into().ok()?) as usize;
    let len = u32::from_le_bytes(d[44..48].try_into().ok()?);
    let cap = u32::from_le_bytes(d[48..52].try_into().ok()?) as usize;
    if len == 0 || head >= cap || d.len() < 64 + cap * 40 {
        return None;
    }
    let obs_slot = |i: usize| -> Option<u64> { Some(u64::from_le_bytes(d[64 + i * 40..64 + i * 40 + 8].try_into().ok()?)) };
    let i = if obs_slot(head)? < slot {
        head
    } else if len >= 2 {
        (head + cap - 1) % cap
    } else {
        return None;
    };
    if obs_slot(i)? >= slot {
        return None;
    }
    let o = 64 + i * 40 + 24;
    Some(u128::from_le_bytes(d[o..o + 16].try_into().ok()?)).filter(|p| *p > 0)
}

/// max(spot, TWAP) of the platform token: pumping it only shrinks receipts. None without a TWAP.
pub fn antidote_price(cfg: &Config, ring: Option<&AccountInfo>, slot: u64) -> Option<u128> {
    let twap = crate::price::price(cfg, slot)?;
    let spot = ring.and_then(|r| ring_spot(cfg, r, slot)).unwrap_or(0);
    Some(twap.max(spot))
}

/// The supply of a mint account (base layout, byte 36).
pub fn mint_supply(ai: &AccountInfo) -> Result<u64> {
    let d = ai.try_borrow_data()?;
    require!(d.len() >= 82, SpotError::WrongAccount);
    Ok(u64::from_le_bytes(d[36..44].try_into().unwrap()))
}

/// levcap's room for new receipts: cap - supply - owed.
pub fn room_left(cap: &levcap::Cap, supply: u64) -> u64 {
    cap.lifetime_room(supply)
}

/// Price a receipt whose V is known: A wanted, then cut to levcap's rooms and the bucket.
/// Returns (amount, wanted, rate_ppm, bucket after).
pub fn size(cfg: &Config, value_sol: u64, price: u128, cap: &levcap::Cap, supply: u64, now: i64) -> Result<(u64, u64, u32, u64)> {
    let wanted = crate::price::tokens_for(value_sol, price)?;
    let left = room_left(cap, supply);
    let bucket = refill(&cfg.receipt_book, cfg.receipts.rate_bps_per_day, left, now);
    let amount = wanted.min(left).min(cap.daily_room(now)).min(bucket).min(receipt_limit(&cfg.receipts, left));
    let rate = if wanted == 0 { PPM as u32 } else { (amount as u128 * PPM as u128 / wanted as u128) as u32 };
    Ok((amount, wanted, rate, bucket - amount))
}

/// Book the bucket draw and the counters for a receipt reserved now.
pub fn take(cfg: &mut Config, r: &Receipt, bucket_after: u64, now: i64) {
    let b = &mut cfg.receipt_book;
    b.bucket_atoms = bucket_after;
    b.bucket_ts = now;
    b.owed = b.owed.saturating_add(r.amount);
    b.atoms_reserved = b.atoms_reserved.saturating_add(r.amount);
    if r.amount < r.amount_wanted {
        b.scaled += 1;
        b.atoms_cut = b.atoms_cut.saturating_add(r.amount_wanted - r.amount);
    }
    cfg.totals.granted_atoms = cfg.totals.granted_atoms.saturating_add(r.amount);
}

// ---------- accounts ----------

/// Who pays for a new account: a signer, by system transfer. (Lamports a program-owned account
/// holds cannot fund a CPI-created account directly: the caller would be unbalanced at the CPI.
/// `issue_receipt` and `redeem_receipt` let the payer fund, then pay it back from the receipt
/// at the end, with `reimburse`.)
pub enum Funder<'a, 'info> {
    Signer(&'a AccountInfo<'info>),
}

impl<'a, 'info> Funder<'a, 'info> {
    pub fn pay(&self, to: &AccountInfo<'info>, sys: &AccountInfo<'info>, lamports: u64) -> Result<()> {
        if lamports == 0 {
            return Ok(());
        }
        match self {
            Funder::Signer(from) => system_program::transfer(CpiContext::new(sys.clone(), system_program::Transfer { from: (*from).clone(), to: to.clone() }), lamports),
        }
    }
}

/// Move `lamports` from `from` (owned by this program) to `to`, after the last CPI of an
/// instruction.
pub fn reimburse(from: &AccountInfo, to: &AccountInfo, lamports: u64) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }
    let have = from.lamports();
    require!(have >= lamports, SpotError::Math);
    **from.try_borrow_mut_lamports()? = have - lamports;
    **to.try_borrow_mut_lamports()? = to.lamports().checked_add(lamports).ok_or(SpotError::Math)?;
    Ok(())
}

/// Make `ai` (a PDA of this program, `seeds` with bump) an account of `space` bytes owned by
/// `owner` holding `lamports`, whatever it held before (lamports sent to the address first would
/// make `create_account` fail for good).
#[allow(clippy::too_many_arguments)]
pub fn make_account<'info>(ai: &AccountInfo<'info>, funder: &Funder<'_, 'info>, sys: &AccountInfo<'info>, space: usize, lamports: u64, owner: &Pubkey, seeds: &[&[u8]]) -> Result<()> {
    let have = ai.lamports();
    funder.pay(ai, sys, lamports.saturating_sub(have))?;
    if space > 0 {
        system_program::allocate(CpiContext::new_with_signer(sys.clone(), system_program::Allocate { account_to_allocate: ai.clone() }, &[seeds]), space as u64)?;
    }
    system_program::assign(CpiContext::new_with_signer(sys.clone(), system_program::Assign { account_to_assign: ai.clone() }, &[seeds]), owner)
}

pub const MINT_EXTENSIONS: [ExtensionType; 4] = [ExtensionType::NonTransferable, ExtensionType::PermanentDelegate, ExtensionType::MintCloseAuthority, ExtensionType::MetadataPointer];
pub const ACCOUNT_EXTENSIONS: [ExtensionType; 2] = [ExtensionType::ImmutableOwner, ExtensionType::NonTransferableAccount];

pub fn mint_len() -> usize {
    ExtensionType::try_calculate_account_len::<T22Mint>(&MINT_EXTENSIONS).unwrap()
}
pub fn token_len() -> usize {
    ExtensionType::try_calculate_account_len::<T22Account>(&ACCOUNT_EXTENSIONS).unwrap()
}

// ---------- metadata ----------

/// Decimal digits of `v` appended to `out` (no `format!`: its machinery costs compute).
fn push_u128(out: &mut String, mut v: u128) {
    let mut buf = [0u8; 40];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.push_str(core::str::from_utf8(&buf[i..]).unwrap());
}

fn push_i64(out: &mut String, v: i64) {
    if v < 0 {
        out.push('-');
    }
    push_u128(out, v.unsigned_abs() as u128);
}

/// `v` with `dec` decimals, trailing zeros kept ("1.500000000").
pub fn fixed(v: u128, dec: u32) -> String {
    let mut out = String::with_capacity(48);
    push_fixed(&mut out, v, dec);
    out
}

fn push_fixed(out: &mut String, v: u128, dec: u32) {
    let p = 10u128.pow(dec);
    push_u128(out, v / p);
    if dec > 0 {
        out.push('.');
        let frac = v % p;
        let mut digits = [b'0'; 40];
        let (mut f, mut i) = (frac, dec as usize);
        while i > 0 {
            i -= 1;
            digits[i] = b'0' + (f % 10) as u8;
            f /= 10;
        }
        out.push_str(core::str::from_utf8(&digits[..dec as usize]).unwrap());
    }
}

/// A unix time as "YYYY-MM-DD HH:MM UTC" (owner 2026-10-23: readable dates in the metadata),
/// proleptic Gregorian (H. Hinnant's days-to-civil).
pub fn utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let mut o = String::with_capacity(20);
    let two = |o: &mut String, v: i64| {
        o.push((b'0' + (v / 10) as u8) as char);
        o.push((b'0' + (v % 10) as u8) as char);
    };
    push_i64(&mut o, y);
    o.push('-');
    two(&mut o, m);
    o.push('-');
    two(&mut o, d);
    o.push(' ');
    two(&mut o, secs / 3_600);
    o.push(':');
    two(&mut o, secs % 3_600 / 60);
    o.push_str(" UTC");
    o
}

/// ppm as a percent with two decimals ("60.00%").
pub fn pct(ppm: u64) -> String {
    let mut out = String::with_capacity(8);
    push_fixed(&mut out, (ppm / 100) as u128, 2);
    out.push('%');
    out
}

/// What the NFT's metadata needs beyond the receipt (2026-10-26, on-chain details and picture):
/// the coin's and the platform token's tickers (from their Token-2022 metadata, cleaned by
/// `svg::clean_ticker`; None when they have none) and an attested receipt's evidence hash.
#[derive(Default, Clone, Copy)]
pub struct Extra<'a> {
    pub ticker: Option<&'a str>,
    pub platform_ticker: Option<&'a str>,
    pub evidence: Option<[u8; 32]>,
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut o = String::with_capacity(2 * b.len());
    for x in b {
        o.push(H[(x >> 4) as usize] as char);
        o.push(H[(x & 15) as usize] as char);
    }
    o
}

/// The receipt's name, symbol, uri and additional fields, the picture (`image`) last.
pub fn metadata(r: &Receipt, number: u64, uri: &str, decimals: u8, x: &Extra) -> (String, String, String, Vec<(String, String)>) {
    // SOL per whole token, 12 decimals: lamports per atom (Q64) x 10^decimals x 1,000
    let pico = mul_div(r.price, 10u128.pow(decimals as u32) * 1_000, Q64).unwrap_or(0);
    let sol = |v: u128, dec: u32| {
        let mut o = fixed(v, dec);
        o.push_str(" SOL");
        o
    };
    let mut counted = String::with_capacity(48);
    counted.push_str("share ");
    counted.push_str(&pct(r.share_bps as u64 * 100));
    counted.push_str(" x h ");
    counted.push_str(&pct(r.h_ppm as u64));
    counted.push_str(" x d ");
    counted.push_str(&pct(r.d_ppm as u64));
    let int = |v: i64| {
        let mut o = String::with_capacity(20);
        push_i64(&mut o, v);
        o
    };
    let coin = r.coin.to_string();
    let redeemable_utc = utc(r.redeemable_from);
    // the picture's text: "$FERN" (or the mint's first and last four), the loss, what is owed
    let mut coin_txt = String::with_capacity(12);
    match x.ticker {
        Some(t) => {
            coin_txt.push('$');
            coin_txt.push_str(t);
        }
        None => {
            coin_txt.push_str(&coin[..4]);
            coin_txt.push_str("..");
            coin_txt.push_str(&coin[coin.len() - 4..]);
        }
    }
    let mut owed = crate::svg::grouped(r.amount, decimals);
    match x.platform_ticker {
        Some(t) => {
            owed.push_str(" $");
            owed.push_str(t);
        }
        None => owed.push_str(" tokens"),
    }
    let loss_txt = crate::svg::sol_trim(r.loss_sol);
    let image = crate::svg::data_uri(&crate::svg::Ticket { coin: &coin_txt, loss: &loss_txt, owed: &owed, number, redeemable_utc: &redeemable_utc });
    // shortest first: each Token-2022 update_field rewrites the whole metadata, so long values
    // added last cost the least (the picture last of all)
    let mut fields = Vec::with_capacity(22);
    fields.push(("booked".to_string(), if r.attested { "attested" } else { "routed" }.to_string()));
    fields.push(("share".to_string(), pct(r.share_bps as u64 * 100)));
    fields.push(("rate_scale".to_string(), pct(r.rate_ppm as u64)));
    if let Some(t) = x.ticker {
        fields.push(("ticker".to_string(), t.to_string()));
    }
    fields.push(("number".to_string(), int(number as i64)));
    fields.push(("closed_at".to_string(), int(r.closed_at)));
    fields.push(("redeemable_from".to_string(), int(r.redeemable_from)));
    fields.push(("auto_redeems_at".to_string(), int(r.auto_redeems_at)));
    fields.push(("loss_lamports".to_string(), int(r.loss_sol as i64)));
    fields.push(("closed_at_utc".to_string(), utc(r.closed_at)));
    fields.push(("redeemable_from_utc".to_string(), redeemable_utc));
    fields.push(("antidote_amount".to_string(), fixed(r.amount as u128, decimals as u32)));
    fields.push(("loss".to_string(), sol(r.loss_sol as u128, 9)));
    fields.push(("value_sol".to_string(), sol(r.value_sol as u128, 9)));
    fields.push(("antidote_price".to_string(), sol(pico, 12)));
    fields.push(("counted".to_string(), counted));
    fields.push(("coin".to_string(), coin));
    fields.push(("quote".to_string(), r.quote.to_string()));
    if let Some(e) = x.evidence {
        fields.push(("evidence".to_string(), hex(&e)));
    }
    fields.push(("redeem".to_string(), REDEEM_TEXT.to_string()));
    fields.push(("image".to_string(), image));
    let mut name = String::with_capacity(48);
    name.push_str("Antidote proof of loss #");
    push_u128(&mut name, number as u128);
    (name, RECEIPT_SYMBOL.to_string(), uri.to_string(), fields)
}

/// The TokenMetadata extension's TLV size (type, length, then borsh: update authority, mint,
/// name, symbol, uri, the additional fields), without building it.
pub fn metadata_tlv_len(name: &str, symbol: &str, uri: &str, fields: &[(String, String)]) -> usize {
    4 + 32 + 32 + 4 + name.len() + 4 + symbol.len() + 4 + uri.len() + 4 + fields.iter().map(|(k, v)| 8 + k.len() + v.len()).sum::<usize>()
}

/// The longest each metadata value can print: a base58 key 44, a u64 with 9 decimals and " SOL"
/// 25, "share 60.00% x h 100.00% x d 100.00%" 36, a percent 7, a u64 amount with up to 9
/// decimals 21, a price with 12 decimals and " SOL" at most 60, an i64 20, a UTC date 20 (years
/// up to 9999; 29 for any i64), "attested" 8, a ticker 10, the picture `svg::uri_len_bound`; the
/// name 24 + 20. An attested receipt adds `evidence` (64 hex digits).
const MAX_VALUE_LENS: [usize; 20] = [8, 7, 7, crate::svg::MAX_TICKER, 20, 20, 20, 20, 20, 29, 29, 21, 25, 25, 60, 36, 44, 44, REDEEM_TEXT.len(), crate::svg::uri_len_bound()];
const FIELD_KEYS: [&str; 20] = [
    "booked",
    "share",
    "rate_scale",
    "ticker",
    "number",
    "closed_at",
    "redeemable_from",
    "auto_redeems_at",
    "loss_lamports",
    "closed_at_utc",
    "redeemable_from_utc",
    "antidote_amount",
    "loss",
    "value_sol",
    "antidote_price",
    "counted",
    "coin",
    "quote",
    "redeem",
    "image",
];
const EVIDENCE_FIELD: usize = 8 + "evidence".len() + 64;

/// The NFT's uri: the `uri` param is a base and the receipt's mint (base58) is appended, so each
/// receipt points at its own metadata JSON (`<base><mint>`); an empty param leaves the uri empty
/// (2026-10-25, receipt cards).
pub fn receipt_uri(base: &str, mint: &Pubkey) -> String {
    if base.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(base.len() + 44);
    s.push_str(base);
    s.push_str(&mint.to_string());
    s
}

/// The longest `receipt_uri` can be for a base: the base and a base58 key (at most 44).
pub fn receipt_uri_len_bound(base: &str) -> usize {
    if base.is_empty() {
        0
    } else {
        base.len() + 44
    }
}

/// Rent for the mint (with its metadata at the largest these numbers can print) and the token
/// account: what a booked receipt holds in advance for its NFT. Whatever is not used goes back to
/// the owner with the receipt's own rent.
pub fn nft_rent_bound(uri: &str, attested: bool) -> Result<u64> {
    let fields: usize = FIELD_KEYS.iter().zip(MAX_VALUE_LENS.iter()).map(|(k, v)| 8 + k.len() + v).sum::<usize>() + if attested { EVIDENCE_FIELD } else { 0 };
    let md = 4 + 32 + 32 + 4 + 44 + 4 + RECEIPT_SYMBOL.len() + 4 + receipt_uri_len_bound(uri) + 4 + fields;
    let rent = Rent::get()?;
    Ok(rent.minimum_balance(mint_len() + md) + rent.minimum_balance(token_len()))
}

/// The accounts that make a receipt's NFT.
pub struct NftAccs<'a, 'info> {
    pub receipt: Pubkey,
    pub mint: &'a AccountInfo<'info>,
    pub token: &'a AccountInfo<'info>,
    /// The receipt's owner; a signer when it signs this transaction (the sell).
    pub owner: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub token_2022: &'a AccountInfo<'info>,
    pub system: &'a AccountInfo<'info>,
}

/// Create the receipt's mint (soulbound, burnable and closable by the ["authority"] PDA, metadata
/// on itself), the owner's token account of it, and mint the one token. When the owner signs,
/// the token account's close authority becomes the PDA too (so redemption closes it). Returns
/// whether it did that.
/// `bumps` are the mint's and the token account's (their addresses checked by the caller).
#[allow(clippy::too_many_arguments)]
pub fn make_nft<'info>(r: &Receipt, number: u64, uri: &str, decimals: u8, x: &Extra, a: &NftAccs<'_, 'info>, funder: &Funder<'_, 'info>, authority_bump: u8, bumps: (u8, u8)) -> Result<bool> {
    use spl_token_2022::instruction as ti;
    let t22 = a.token_2022.key;
    let auth = a.authority.key;
    let (rmint, mint_bump) = (a.mint.key(), bumps.0);
    let (rtok, tok_bump) = (a.token.key(), bumps.1);
    require_keys_eq!(a.owner.key(), r.owner, SpotError::WrongAccount);
    let full_uri = receipt_uri(uri, &rmint);
    let (name, symbol, uri, fields) = metadata(r, number, &full_uri, decimals, x);
    let rent = Rent::get()?;
    let mlen = mint_len();
    let mint_rent = rent.minimum_balance(mlen + metadata_tlv_len(&name, &symbol, &uri, &fields));
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &[authority_bump]];
    let rs = a.receipt;
    make_account(a.mint, funder, a.system, mlen, mint_rent, t22, &[RECEIPT_MINT_SEED, rs.as_ref(), &[mint_bump]])?;
    let mi = || vec![a.mint.clone()];
    invoke(&ti::initialize_non_transferable_mint(t22, &rmint)?, &mi())?;
    invoke(&ti::initialize_permanent_delegate(t22, &rmint, auth)?, &mi())?;
    invoke(&ti::initialize_mint_close_authority(t22, &rmint, Some(auth))?, &mi())?;
    invoke(&spl_token_2022::extension::metadata_pointer::instruction::initialize(t22, &rmint, Some(*auth), Some(rmint))?, &mi())?;
    invoke(&ti::initialize_mint2(t22, &rmint, auth, None, 0)?, &mi())?;
    let md_infos = [a.mint.clone(), a.authority.clone(), a.mint.clone(), a.authority.clone()];
    invoke_signed(&tmi::instruction::initialize(t22, &rmint, auth, &rmint, auth, name, symbol, uri), &md_infos, &[auth_seeds])?;
    // one instruction, its data rewritten per field (2026-10-26: twenty fields and the picture
    // overran the 32 KB bump heap when each update_field built its own instruction): the
    // discriminator, then borsh Field::Key(k) (tag 3) and the value
    let mut ix = tmi::instruction::update_field(t22, &rmint, auth, Field::Key(String::new()), String::new());
    let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
    let most = fields.iter().map(|(k, v)| k.len() + v.len()).max().unwrap_or(0);
    ix.data = Vec::with_capacity(8 + 1 + 4 + 4 + most);
    for (k, v) in fields.iter() {
        ix.data.clear();
        ix.data.extend_from_slice(&disc);
        ix.data.push(3);
        ix.data.extend_from_slice(&(k.len() as u32).to_le_bytes());
        ix.data.extend_from_slice(k.as_bytes());
        ix.data.extend_from_slice(&(v.len() as u32).to_le_bytes());
        ix.data.extend_from_slice(v.as_bytes());
        invoke_signed(&ix, &[a.mint.clone(), a.authority.clone()], &[auth_seeds])?;
    }
    // the owner's token account, at a PDA so nobody else can make it first
    let tlen = token_len();
    make_account(a.token, funder, a.system, tlen, rent.minimum_balance(tlen), t22, &[RECEIPT_TOKEN_SEED, rs.as_ref(), &[tok_bump]])?;
    invoke(&ti::initialize_immutable_owner(t22, &rtok)?, &[a.token.clone()])?;
    invoke(&ti::initialize_account3(t22, &rtok, &rmint, a.owner.key)?, &[a.token.clone(), a.mint.clone()])?;
    invoke_signed(&ti::mint_to(t22, &rmint, &rtok, auth, &[], 1)?, &[a.mint.clone(), a.token.clone(), a.authority.clone()], &[auth_seeds])?;
    // one token, ever
    invoke_signed(&ti::set_authority(t22, &rmint, None, AuthorityType::MintTokens, auth, &[])?, &[a.mint.clone(), a.authority.clone()], &[auth_seeds])?;
    if a.owner.is_signer {
        invoke(&ti::set_authority(t22, &rtok, Some(auth), AuthorityType::CloseAccount, a.owner.key, &[])?, &[a.token.clone(), a.owner.clone()])?;
        return Ok(true);
    }
    Ok(false)
}

/// Reserve `amount` at levcap, signed by the ["authority"] PDA (levcap's minter).
pub fn reserve<'info>(levcap_program: &AccountInfo<'info>, authority: &AccountInfo<'info>, cap: &AccountInfo<'info>, mint: &AccountInfo<'info>, token_2022: &AccountInfo<'info>, amount: u64, authority_bump: u8) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    levcap::cpi::reserve(
        CpiContext::new_with_signer(
            levcap_program.clone(),
            levcap::cpi::accounts::Reserve { minter: authority.clone(), cap: cap.clone(), mint: mint.clone(), token_program: token_2022.clone() },
            &[&[AUTHORITY_SEED, &[authority_bump]]],
        ),
        amount,
    )
}

pub fn load_cap(ai: &AccountInfo) -> Result<levcap::Cap> {
    require_keys_eq!(*ai.owner, levcap::ID, SpotError::WrongAccount);
    levcap::Cap::try_deserialize(&mut &ai.try_borrow_data()?[..])
}

pub fn store_receipt(ai: &AccountInfo, r: &Receipt) -> Result<()> {
    let mut d = ai.try_borrow_mut_data()?;
    r.try_serialize(&mut &mut d[..])
}
