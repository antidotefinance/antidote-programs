//! levcap: the capped mint authority for the platform token (owner decision 2026-10-17; limit
//! recycles burns since 2026-10-23).
//!
//! A mint's authority is handed to this program's PDA ["mint_auth", mint] and the mint is
//! registered once, with values that never change again:
//!
//! - `lifetime_cap`: the most atoms that may exist plus be owed at any time: `supply + owed <=
//!   lifetime_cap` always holds (owner 2026-10-23, was "minted ever"). Burned tokens leave the
//!   supply, so they free room for later mints.
//! - `minter`: the one key that may call `reserve` and `mint_to` (the engine's ["authority"] PDA,
//!   which signs through CPI when it issues and redeems loss receipts).
//! - `daily_ceiling`: the most atoms that may be newly committed (reserved, or minted without a
//!   reservation) in one UTC day (unix time / 86400). Minting what was reserved earlier never
//!   counts again, so a reservation is always honored.
//! - `mint`: the mint itself (no freeze authority allowed, and on Token-2022 only the extensions
//!   in `ALLOWED_EXTENSIONS`: metadata pointer, token metadata and transfer hook).
//!
//! `reserve(amount)` books `amount` as owed (the engine does it when it issues a receipt), within
//! the lifetime room `lifetime_cap - supply - owed` and today's ceiling. `mint_to(amount)` mints
//! from what is owed first; anything above it is a fresh commitment checked like a reservation.
//!
//! There is no admin, no setter, no way to hand the mint authority back, and no IDL account
//! (built with `no-idl`). Deployed with its upgrade authority set to none, these rules hold for
//! as long as the chain runs.
//!
//! Registration must sit in the same transaction as the `SetAuthority` that hands the mint
//! authority to the PDA: between two transactions anyone could register the mint with a minter
//! of their own (the program cannot know who handed the authority over).

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, MintTo, TokenAccount, TokenInterface};

declare_id!("Gq9eEG2mnAefVGtcgnmZeutUitKvZoxErxL27RALJ7U");

// Shown by Solana Explorer, Solscan and other terminals (owner 2026-10-19). Left out of CPI builds
// so programs that depend on this crate do not embed a second copy.
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Antidote",
    project_url: "https://github.com/antidotefinance",
    contacts: "link:https://github.com/antidotefinance/antidote-cap/security/advisories/new",
    policy: "https://github.com/antidotefinance/antidote-cap/blob/main/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/antidotefinance/antidote-cap"
}

pub const CAP_SEED: &[u8] = b"cap";
pub const MINT_AUTH_SEED: &[u8] = b"mint_auth";
pub const DAY_SECS: i64 = 86_400;

/// Token-2022 mint extensions `register` accepts (red team F5), by their on-chain type number:
/// TransferHook (14), MetadataPointer (18), TokenMetadata (19). These are what a Meteora DBC
/// transfer-hook launch puts on its mint. Everything else is refused, including every extension
/// that lets some key take, freeze, charge or rescale holders' tokens or close the mint
/// (MintCloseAuthority, PermanentDelegate, TransferFeeConfig, ConfidentialTransfer*,
/// DefaultAccountState, NonTransferable, InterestBearingConfig, ScaledUiAmount, Pausable) and any
/// type this program does not know.
pub const ALLOWED_EXTENSIONS: [u16; 3] = [14, 18, 19];

/// The mint's extension types are all in `ALLOWED_EXTENSIONS`. A classic Token mint (82 bytes)
/// has none. Token-2022 extended mints: 82 base bytes, zero padding to 165, the account type
/// (1 = Mint) at 165, then TLV entries (u16 type, u16 length, value) until type 0 or the end.
pub fn extensions_allowed(d: &[u8]) -> bool {
    const BASE: usize = 82;
    const ACCOUNT_LEN: usize = 165;
    if d.len() == BASE {
        return true;
    }
    if d.len() <= ACCOUNT_LEN || d[ACCOUNT_LEN] != 1 || d[BASE..ACCOUNT_LEN].iter().any(|b| *b != 0) {
        return false;
    }
    let mut i = ACCOUNT_LEN + 1;
    while i + 4 <= d.len() {
        let t = u16::from_le_bytes([d[i], d[i + 1]]);
        let l = u16::from_le_bytes([d[i + 2], d[i + 3]]) as usize;
        if t == 0 {
            break;
        }
        if !ALLOWED_EXTENSIONS.contains(&t) {
            return false;
        }
        i += 4 + l;
        if i > d.len() {
            return false;
        }
    }
    // a type with no room for its length is malformed (Token-2022 refuses it too)
    !(i + 2 <= d.len() && i + 4 > d.len() && (d[i] != 0 || d[i + 1] != 0))
}

/// One per registered mint, PDA ["cap", mint]. Every field but the counters is fixed at
/// registration.
#[account]
#[derive(InitSpace)]
pub struct Cap {
    pub mint: Pubkey,
    pub minter: Pubkey,
    /// `supply + owed` never exceeds it.
    pub lifetime_cap: u64,
    /// New commitments (reservations and unreserved mints) per UTC day.
    pub daily_ceiling: u64,
    /// The mint's supply at registration (minted before levcap held the authority).
    pub registered_supply: u64,
    /// `registered_supply` plus everything `mint_to` has minted, lifetime (analytics only since
    /// 2026-10-23: the limit is `supply + owed`).
    pub minted_total: u64,
    /// UTC day index (unix time / 86400) `committed_today` counts.
    pub day: i64,
    /// Atoms newly committed on `day` (was `minted_today`; same bytes).
    pub committed_today: u64,
    pub bump: u8,
    pub auth_bump: u8,
    // ---- appended 2026-10-23 (`migrate_cap` grows an older account) ----
    /// Atoms reserved for the minter and not minted yet.
    pub owed: u64,
    /// Lifetime atoms reserved.
    pub reserved_total: u64,
}

/// The size of a `Cap` created before `owed` (what `migrate_cap` grows from).
pub const CAP_LEN_V1: usize = 8 + 32 + 32 + 8 * 6 + 2;

impl Cap {
    /// Atoms that could be newly committed right now with the mint at `supply`: the lifetime room
    /// (`lifetime_cap - supply - owed`) and today's room, whichever is less. Today's count resets
    /// only when the UTC day moves forward past `day` (red team F6): a clock that steps back
    /// keeps counting against the newest day seen.
    pub fn room(&self, supply: u64, now: i64) -> u64 {
        self.lifetime_room(supply).min(self.daily_room(now))
    }

    /// `lifetime_cap - supply - owed` (burns free room).
    pub fn lifetime_room(&self, supply: u64) -> u64 {
        self.lifetime_cap.saturating_sub(supply).saturating_sub(self.owed)
    }

    pub fn daily_room(&self, now: i64) -> u64 {
        let today = if now.div_euclid(DAY_SECS) > self.day { 0 } else { self.committed_today };
        self.daily_ceiling.saturating_sub(today)
    }

    /// Book `amount` as newly committed now, within both rooms.
    fn commit(&mut self, supply: u64, amount: u64, now: i64) -> Result<()> {
        let total = supply.checked_add(self.owed).and_then(|x| x.checked_add(amount)).ok_or(CapError::Math)?;
        require!(total <= self.lifetime_cap, CapError::OverLifetimeCap);
        // a new UTC day only when it moves forward (red team F6): a backwards clock step keeps
        // counting against the newest day seen, so it can never reopen a day's ceiling
        let day = now.div_euclid(DAY_SECS).max(self.day);
        if day > self.day {
            self.day = day;
            self.committed_today = 0;
        }
        let today = self.committed_today.checked_add(amount).ok_or(CapError::Math)?;
        require!(today <= self.daily_ceiling, CapError::OverDailyCeiling);
        self.committed_today = today;
        Ok(())
    }
}

pub fn cap_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CAP_SEED, mint.as_ref()], &crate::ID)
}

pub fn mint_auth_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[MINT_AUTH_SEED, mint.as_ref()], &crate::ID)
}

#[event]
pub struct Registered {
    pub mint: Pubkey,
    pub minter: Pubkey,
    pub lifetime_cap: u64,
    pub daily_ceiling: u64,
    pub registered_supply: u64,
}

#[event]
pub struct Reserved {
    pub mint: Pubkey,
    pub amount: u64,
    pub owed: u64,
    pub supply: u64,
    pub day: i64,
    pub committed_today: u64,
}

#[event]
pub struct Minted {
    pub mint: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
    pub minted_total: u64,
    pub day: i64,
    pub committed_today: u64,
    /// Of `amount`, the part that was owed (reserved earlier).
    pub from_owed: u64,
}

#[error_code]
pub enum CapError {
    #[msg("the mint's mint authority is not levcap's PDA [\"mint_auth\", mint]")]
    AuthorityNotHandedOver,
    #[msg("the mint has a freeze authority")]
    FreezeAuthority,
    #[msg("the mint's supply is already over the lifetime cap")]
    SupplyOverCap,
    #[msg("bad registration value")]
    BadParam,
    #[msg("over the lifetime cap")]
    OverLifetimeCap,
    #[msg("over today's ceiling")]
    OverDailyCeiling,
    #[msg("amount must be positive")]
    ZeroAmount,
    #[msg("math overflow")]
    Math,
    #[msg("the mint has a Token-2022 extension levcap does not allow")]
    ExtensionNotAllowed,
    #[msg("the cap account is already current")]
    AlreadyCurrent,
}

#[program]
pub mod levcap {
    use super::*;

    /// Once per mint (the Cap PDA can only be created once). Anyone may call it; it checks that
    /// the authority was already handed to levcap's PDA, so put it in the handover transaction.
    pub fn register(ctx: Context<Register>, lifetime_cap: u64, minter: Pubkey, daily_ceiling: u64) -> Result<()> {
        let mint = &ctx.accounts.mint;
        let auth = ctx.accounts.mint_auth.key();
        require!(mint.mint_authority == Some(auth).into(), CapError::AuthorityNotHandedOver);
        require!(mint.freeze_authority.is_none(), CapError::FreezeAuthority);
        require!(extensions_allowed(&mint.to_account_info().try_borrow_data()?), CapError::ExtensionNotAllowed);
        require!(lifetime_cap > 0 && daily_ceiling > 0 && minter != Pubkey::default(), CapError::BadParam);
        require!(mint.supply <= lifetime_cap, CapError::SupplyOverCap);
        let now = Clock::get()?.unix_timestamp;
        let cap = &mut ctx.accounts.cap;
        cap.mint = mint.key();
        cap.minter = minter;
        cap.lifetime_cap = lifetime_cap;
        cap.daily_ceiling = daily_ceiling;
        cap.registered_supply = mint.supply;
        cap.minted_total = mint.supply;
        cap.day = now.div_euclid(DAY_SECS);
        cap.committed_today = 0;
        cap.owed = 0;
        cap.reserved_total = 0;
        cap.bump = ctx.bumps.cap;
        cap.auth_bump = ctx.bumps.mint_auth;
        emit!(Registered { mint: cap.mint, minter, lifetime_cap, daily_ceiling, registered_supply: mint.supply });
        Ok(())
    }

    /// The minter reserves `amount` atoms: they are owed from now on and count against the
    /// lifetime room (`supply + owed <= lifetime_cap`) and today's ceiling; a later `mint_to`
    /// mints them without counting again.
    pub fn reserve(ctx: Context<Reserve>, amount: u64) -> Result<()> {
        require!(amount > 0, CapError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let supply = ctx.accounts.mint.supply;
        let cap = &mut ctx.accounts.cap;
        cap.commit(supply, amount, now)?;
        cap.owed = cap.owed.checked_add(amount).ok_or(CapError::Math)?;
        cap.reserved_total = cap.reserved_total.saturating_add(amount);
        emit!(Reserved { mint: cap.mint, amount, owed: cap.owed, supply, day: cap.day, committed_today: cap.committed_today });
        Ok(())
    }

    /// The minter mints `amount` into `destination`: from what it is owed first, the rest as a
    /// fresh commitment within the lifetime room and today's ceiling.
    pub fn mint_to(ctx: Context<MintCapped>, amount: u64) -> Result<()> {
        require!(amount > 0, CapError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let supply = ctx.accounts.mint.supply;
        let cap = &mut ctx.accounts.cap;
        let from_owed = amount.min(cap.owed);
        cap.owed -= from_owed;
        let fresh = amount - from_owed;
        if fresh > 0 {
            // the owed part is about to join the supply: count it there
            cap.commit(supply.checked_add(from_owed).ok_or(CapError::Math)?, fresh, now)?;
        }
        let total = cap.minted_total.saturating_add(amount);
        cap.minted_total = total;
        let (day, today) = (cap.day, cap.committed_today);
        let mint_key = cap.mint;
        let seeds: &[&[u8]] = &[MINT_AUTH_SEED, mint_key.as_ref(), &[cap.auth_bump]];
        token_interface::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.destination.to_account_info(),
                    authority: ctx.accounts.mint_auth.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        emit!(Minted { mint: mint_key, to: ctx.accounts.destination.key(), amount, minted_total: total, day, committed_today: today, from_owed });
        Ok(())
    }

    /// Anyone (the payer funds the rent): grow a `Cap` registered before `owed` existed to the
    /// current size, with nothing owed. Nothing else changes. Refused on a current account.
    pub fn migrate_cap(ctx: Context<MigrateCap>) -> Result<()> {
        let ai = ctx.accounts.cap.to_account_info();
        let new_len = 8 + Cap::INIT_SPACE;
        {
            let d = ai.try_borrow_data()?;
            require!(d.len() >= 8 && d[..8] == *Cap::DISCRIMINATOR, CapError::BadParam);
            require!(d.len() == CAP_LEN_V1, CapError::AlreadyCurrent);
            require!(d[8..40] == ctx.accounts.mint.key().to_bytes(), CapError::BadParam);
        }
        let need = Rent::get()?.minimum_balance(new_len).saturating_sub(ai.lamports());
        if need > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(ctx.accounts.system_program.to_account_info(), anchor_lang::system_program::Transfer { from: ctx.accounts.payer.to_account_info(), to: ai.clone() }),
                need,
            )?;
        }
        ai.realloc(new_len, true)?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Register<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: PDA ["mint_auth", mint]; the mint's authority must already be it
    #[account(seeds = [MINT_AUTH_SEED, mint.key().as_ref()], bump)]
    pub mint_auth: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + Cap::INIT_SPACE, seeds = [CAP_SEED, mint.key().as_ref()], bump)]
    pub cap: Account<'info, Cap>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Reserve<'info> {
    pub minter: Signer<'info>,
    #[account(mut, seeds = [CAP_SEED, mint.key().as_ref()], bump = cap.bump, has_one = minter, has_one = mint)]
    pub cap: Account<'info, Cap>,
    /// The mint, for its supply now.
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct MigrateCap<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the ["cap", mint] PDA (seeds, owner, discriminator and mint checked); read raw
    /// because an older account is too short to load
    #[account(mut, seeds = [CAP_SEED, mint.key().as_ref()], bump, owner = crate::ID)]
    pub cap: UncheckedAccount<'info>,
    /// CHECK: the registered mint (compared to the cap's)
    pub mint: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MintCapped<'info> {
    pub minter: Signer<'info>,
    #[account(mut, seeds = [CAP_SEED, mint.key().as_ref()], bump = cap.bump, has_one = minter, has_one = mint)]
    pub cap: Account<'info, Cap>,
    #[account(mut, mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: PDA ["mint_auth", mint], signs the mint
    #[account(seeds = [MINT_AUTH_SEED, mint.key().as_ref()], bump = cap.auth_bump)]
    pub mint_auth: UncheckedAccount<'info>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}
