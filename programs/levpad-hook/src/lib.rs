//! levpad-hook: the price oracle for coins launched on Meteora DBC.
//!
//! Curve phase: DBC creates the coin's Token-2022 mint with this program as its transfer hook.
//! Every swap moves the base token in or out of the pool's `base_vault`, Token-2022 calls
//! `execute`, and the hook writes DBC's post-swap `sqrt_price` (as a Q64.64 price) and quote
//! reserve into the observation ring the perp engine reads. The hook never returns an error: a
//! failing hook would freeze the curve, so any input it does not recognise records nothing.
//!
//! Graduation: the swap that completes the curve still calls the hook, then DBC revokes it for
//! good and migrates the liquidity into DAMM v2. `bind_damm` ties the coin to the DAMM v2 pool
//! that DBC's migration created, and `poke_damm` (permissionless) keeps writing the same ring
//! from that pool, so the engine's TWAP runs across graduation without a gap.
//!
//! Fair open (SPEC.md, "Fair open"): a coin launched while `GateConfig` exists, on a DBC config
//! the router counts, gets the v2 meta list and a gate account, and the hook then refuses curve
//! swaps of it that break the anti-bundle rules (`gate::enforce`). That is the one path on which
//! `execute` may fail; every other coin (v1 meta list) still never fails.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::system_instruction;
use spl_discriminator::SplDiscriminate;
use spl_tlv_account_resolution::{account::ExtraAccountMeta, state::ExtraAccountMetaList};
use spl_transfer_hook_interface::instruction::ExecuteInstruction;

pub mod gate;
pub mod pump;
pub mod ring;
pub mod venue;

use pump::{PumpFeed, PumpLink, PHASE_AMM, PHASE_CURVE, PUMP_FEED_SEED};
use venue::{damm, dbc, DAMM_V2_PROGRAM, DBC_POOL_AUTHORITY, TOKEN_2022, WSOL};

declare_id!("3ri134ez6K1QPUV4XMhAKeJbfhBDW55bQCoGJZL6cmxc");

// Shown by Solana Explorer, Solscan and other terminals (owner 2026-10-19). Left out of CPI builds
// so programs that depend on this crate do not embed a second copy.
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Antidote",
    project_url: "https://github.com/antidotefinance",
    contacts: "link:https://github.com/antidotefinance/antidote-programs/security/advisories/new",
    policy: "https://github.com/antidotefinance/antidote-programs/blob/main/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/antidotefinance/antidote-programs"
}

/// The spot engine (tilt-spot). Ring headers still name a PDA ["market", base_mint] under it, a
/// label carried over from levpad; tilt-spot creates no such account and never reads it.
pub const ENGINE_PROGRAM: Pubkey = pubkey!("5iWcJpM7vGksXq7LxusmmfpPLZNmmWxjNC8aRaFJtoE8");
pub const META_SEED: &[u8] = b"extra-account-metas";
pub const OBS_SEED: &[u8] = b"obs";
pub const DAMM_LINK_SEED: &[u8] = b"damm";
pub const MARKET_SEED: &[u8] = b"market";
/// Two extra accounts: the DBC pool (readonly) and the ring (writable).
pub const META_COUNT: usize = 2;
/// The gated (v2) list adds the instructions sysvar (readonly), the gate (writable), tilt-spot's
/// config (readonly) and the hook's GateConfig (readonly), in that order.
pub const META_COUNT_V2: usize = 6;

#[error_code]
pub enum HookError {
    #[msg("not a DBC transfer-hook pool for this mint")]
    NotDbcPool,
    #[msg("mint is not a Token-2022 mint")]
    NotToken2022,
    #[msg("the DBC pool has not migrated yet")]
    NotMigrated,
    #[msg("not a DAMM v2 config created for DBC's migrations")]
    NotDbcMigrationConfig,
    #[msg("not the DAMM v2 pool DBC's migration creates for this coin")]
    NotMigrationPool,
    #[msg("ring account is not this coin's ring")]
    BadRing,
    #[msg("DAMM v2 pool price or reserve out of range")]
    BadDammPrice,
    #[msg("the mint's transfer hook is not levpad-hook")]
    NotOurHook,
    #[msg("already poked in this slot")]
    PokedThisSlot,
    #[msg("the curve is not live")]
    CurveNotLive,
    #[msg("not this mint's pump.fun bonding curve")]
    NotPumpCurve,
    #[msg("pump.fun coin is not quoted in SOL")]
    PumpNotSol,
    #[msg("not a SPL Token or Token-2022 mint")]
    NotMint,
    #[msg("the ring for this mint already exists")]
    RingExists,
    #[msg("the pump.fun curve is complete: bind its PumpSwap pool")]
    PumpCurveComplete,
    #[msg("the pump.fun curve is not complete")]
    PumpCurveLive,
    #[msg("not the canonical PumpSwap pool for this mint")]
    NotPumpPool,
    #[msg("not the pool's vault")]
    BadPumpVault,
    #[msg("pump.fun venue price or reserve out of range")]
    BadPumpPrice,
    #[msg("a pump poke must be a top-level instruction")]
    PokeNotTopLevel,
    #[msg("a pump poke shares its transaction only with compute budget, levpad-hook and the engine")]
    PokeShared,
    #[msg("wrong pump feed phase for this instruction")]
    WrongPumpPhase,
    #[msg("fair open: curve swaps of this coin go through tilt-spot's trade_dbc only")]
    NotRouter,
    #[msg("fair open: in the creation slot only the pool creator can buy")]
    CreationSlotCreatorOnly,
    #[msg("fair open: one trade_dbc buy of this coin per transaction")]
    OneBuyPerTx,
    #[msg("fair open: this slot's ramp cap is used up")]
    RampCap,
    #[msg("fair open: not this coin's gate account")]
    BadGate,
    #[msg("fair open: not the instructions sysvar")]
    BadInstructionsSysvar,
    #[msg("fair open: base_bps must be 1..=10000 and ramp_slots 0..=300")]
    BadGateParams,
    #[msg("fair open: not the gate config's admin")]
    NotGateAdmin,
    #[msg("fair open: not tilt-spot's config")]
    BadSpotConfig,
    #[msg("a curve transfer must carry the coin's meta list (extra-account-metas)")]
    MissingMetaList,
    #[msg("fair open: the creator's creation-slot buys are over the creator cap")]
    CreatorCap,
    #[msg("fair open: not the hook's GateConfig")]
    BadGateConfig,
}

#[event]
pub struct DammBound {
    pub mint: Pubkey,
    pub damm_pool: Pubkey,
    pub damm_config: Pubkey,
}

#[account]
#[derive(InitSpace)]
pub struct DammLink {
    pub mint: Pubkey,
    pub damm_pool: Pubkey,
    pub ring: Pubkey,
    pub bound_slot: u64,
    pub bump: u8,
}

pub fn engine_market(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[MARKET_SEED, mint.as_ref()], &ENGINE_PROGRAM).0
}

pub fn ring_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[OBS_SEED, mint.as_ref()], &crate::ID)
}

pub fn meta_list_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[META_SEED, mint.as_ref()], &crate::ID)
}

pub fn damm_link_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[DAMM_LINK_SEED, mint.as_ref()], &crate::ID)
}

/// The DAMM v2 pool PDA for a config and a pair (cp-amm `pool` seeds).
pub fn damm_pool_address(config: &Pubkey, a: &Pubkey, b: &Pubkey) -> Pubkey {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    Pubkey::find_program_address(&[damm::POOL_PREFIX, config.as_ref(), hi.as_ref(), lo.as_ref()], &DAMM_V2_PROGRAM).0
}

pub fn gate_address(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[gate::GATE_SEED, mint.as_ref()], &crate::ID)
}

pub fn gate_config_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[gate::GATE_CONFIG_SEED], &crate::ID)
}

pub fn extra_metas(dbc_pool: &Pubkey, ring: &Pubkey) -> Result<[ExtraAccountMeta; META_COUNT]> {
    Ok([ExtraAccountMeta::new_with_pubkey(dbc_pool, false, false)?, ExtraAccountMeta::new_with_pubkey(ring, false, true)?])
}

/// tilt-spot's Config ["config"].
pub fn spot_config_address() -> Pubkey {
    Pubkey::find_program_address(&[gate::SPOT_CONFIG_SEED], &ENGINE_PROGRAM).0
}

/// The gated list: v1's two, then the instructions sysvar (readonly), the gate (writable) and
/// tilt-spot's config (readonly) and GateConfig (readonly). The order is fixed: the hook reads
/// them at 7 to 10.
pub fn extra_metas_v2(dbc_pool: &Pubkey, ring: &Pubkey, gate: &Pubkey, spot_config: &Pubkey, gate_config: &Pubkey) -> Result<[ExtraAccountMeta; META_COUNT_V2]> {
    Ok([
        ExtraAccountMeta::new_with_pubkey(dbc_pool, false, false)?,
        ExtraAccountMeta::new_with_pubkey(ring, false, true)?,
        ExtraAccountMeta::new_with_pubkey(&anchor_lang::solana_program::sysvar::instructions::ID, false, false)?,
        ExtraAccountMeta::new_with_pubkey(gate, false, true)?,
        ExtraAccountMeta::new_with_pubkey(spot_config, false, false)?,
        ExtraAccountMeta::new_with_pubkey(gate_config, false, false)?,
    ])
}

/// How many extra accounts a meta list names (`ExtraAccountMetaList` TLV: 8 discriminator, 4
/// length, 4 count, then 35 bytes each), or 0 when it is not one of this program's lists.
pub fn meta_list_count(owner: &Pubkey, d: &[u8]) -> u32 {
    if *owner != crate::ID || d.len() < 16 {
        return 0;
    }
    u32::from_le_bytes([d[12], d[13], d[14], d[15]])
}

/// Token-2022 sets `transferring` on the source account's TransferHookAccount extension for the
/// length of the hook call. A direct call to `execute` (not from inside a transfer) sees it unset.
fn is_transferring(d: &[u8]) -> bool {
    const BASE: usize = 165;
    const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
    const TRANSFER_HOOK_ACCOUNT: u16 = 15;
    if d.len() <= BASE || d[BASE] != ACCOUNT_TYPE_ACCOUNT {
        return false;
    }
    let mut i = BASE + 1;
    while i + 4 <= d.len() {
        let t = u16::from_le_bytes([d[i], d[i + 1]]);
        let l = u16::from_le_bytes([d[i + 2], d[i + 3]]) as usize;
        if t == 0 {
            return false;
        }
        let v = i + 4;
        if v + l > d.len() {
            return false;
        }
        if t == TRANSFER_HOOK_ACCOUNT {
            return l >= 1 && d[v] == 1;
        }
        i = v + l;
    }
    false
}

/// The hook body. Records the post-swap price into the ring, and quietly records nothing on
/// anything it does not expect. On a gated coin (v2 meta list) it also enforces the fair-open
/// rules on curve swaps and fails the transfer when one is broken (`gate::enforce`).
/// Accounts: 0 source, 1 mint, 2 destination, 3 authority, 4 meta list, 5 DBC pool, 6 ring, and
/// on a gated coin 7 instructions sysvar, 8 gate, 9 tilt-spot's config, 10 GateConfig.
fn on_transfer(accs: &[AccountInfo], amount: u64) -> Result<()> {
    if accs.len() < 7 {
        return unlisted_transfer(accs);
    }
    let (source, mint, dest, pool, ring_ai) = (&accs[0], &accs[1], &accs[2], &accs[5], &accs[6]);
    // only Token-2022 can set `transferring` on an account it owns (audit H5)
    if *source.owner != TOKEN_2022 || *mint.owner != TOKEN_2022 {
        return Ok(());
    }
    let (slot, price, quote_reserve, is_out, pool_config) = {
        let Ok(d) = pool.try_borrow_data() else { return Ok(()) };
        if !venue::is_dbc_pool(pool.owner, &d, mint.key) {
            return Ok(());
        }
        // only while the curve runs (the completing swap still has progress 0 at transfer time)
        if d[dbc::IS_MIGRATED] != 0 || d[dbc::MIGRATION_PROGRESS] != 0 {
            return Ok(());
        }
        let vault = &d[dbc::BASE_VAULT..dbc::BASE_VAULT + 32];
        let is_out = source.key.as_ref() == vault;
        if !is_out && dest.key.as_ref() != vault {
            return Ok(());
        }
        // a price the ring cannot take records nothing (as before) but does not skip the gate
        let price = levpad_math::price_from_sqrt_q64(venue::u128_at(&d, dbc::SQRT_PRICE)).ok().filter(|p| *p > 0);
        let Ok(clock) = Clock::get() else { return Ok(()) };
        (clock.slot, price, venue::u64_at(&d, dbc::QUOTE_RESERVE), is_out, venue::key_at(&d, dbc::CONFIG))
    };
    {
        let Ok(s) = source.try_borrow_data() else { return Ok(()) };
        if !is_transferring(&s) {
            return Ok(());
        }
        // inside a real transfer of this mint (the source is this mint's account and Token-2022
        // has it marked): the accounts are the ones Token-2022 resolved from the coin's meta list
        if s[..32] == mint.key.to_bytes() {
            fair_open(accs, mint.key, &pool_config, is_out, slot, amount)?;
        }
    }
    let Some(price) = price else { return Ok(()) };
    if !ring_ai.is_writable || *ring_ai.owner != crate::ID {
        return Ok(());
    }
    let Ok(mut d) = ring_ai.try_borrow_mut_data() else { return Ok(()) };
    if !ring::is_ring(&d) {
        return Ok(());
    }
    match Pubkey::create_program_address(&[OBS_SEED, mint.key.as_ref(), &[d[ring::BUMP_AT]]], &crate::ID) {
        Ok(k) if k == *ring_ai.key => {}
        _ => return Ok(()),
    }
    let _ = ring::record(&mut d, slot, price, quote_reserve);
    Ok(())
}

/// Token-2022 treats the meta list as optional: when the caller leaves it out of the transfer's
/// accounts, it calls `execute` with the four base accounts only, and none of the coin's extra
/// accounts (red team F1, docs/redteam/fair-open-2026-10-22.md). The hook cannot tell a gated
/// coin from a v1 coin without them, so a real transfer of this mint into or out of an account
/// owned by DBC's pool authority (a DBC base vault: curve swaps, fee claims) is refused; every
/// client and program on the pad passes the list. Anything else (wallet transfers, DAMM v2
/// after graduation, where DBC has revoked the hook anyway, direct calls) passes as before.
fn unlisted_transfer(accs: &[AccountInfo]) -> Result<()> {
    if accs.len() < 4 {
        return Ok(());
    }
    let (source, mint, dest) = (&accs[0], &accs[1], &accs[2]);
    if *source.owner != TOKEN_2022 || *mint.owner != TOKEN_2022 {
        return Ok(());
    }
    let owner_of = |ai: &AccountInfo| -> Option<Pubkey> {
        let d = ai.try_borrow_data().ok()?;
        (*ai.owner == TOKEN_2022 && d.len() >= 64).then(|| venue::key_at(&d, 32))
    };
    {
        let Ok(s) = source.try_borrow_data() else { return Ok(()) };
        if s.len() < 64 || s[..32] != mint.key.to_bytes() || !is_transferring(&s) {
            return Ok(());
        }
    }
    let touches_dbc = owner_of(source) == Some(DBC_POOL_AUTHORITY) || owner_of(dest) == Some(DBC_POOL_AUTHORITY);
    require!(!touches_dbc, HookError::MissingMetaList);
    Ok(())
}

/// Fair open for one curve transfer into or out of the vault. A v1 coin (two-entry list) returns
/// at once; a gated coin must carry the real instructions sysvar, its own gate and tilt-spot's
/// config (spoofs fail).
fn fair_open(accs: &[AccountInfo], mint: &Pubkey, pool_config: &Pubkey, is_out: bool, slot: u64, amount: u64) -> Result<()> {
    let list = &accs[4];
    let n = {
        let Ok(d) = list.try_borrow_data() else { return Ok(()) };
        meta_list_count(list.owner, &d)
    };
    if n as usize != META_COUNT_V2 {
        return Ok(());
    }
    require!(accs.len() >= 11, HookError::BadGate);
    let (ixs, gate_ai, spot, gcfg, dest) = (&accs[7], &accs[8], &accs[9], &accs[10], &accs[2]);
    require_keys_eq!(*ixs.key, anchor_lang::solana_program::sysvar::instructions::ID, HookError::BadInstructionsSysvar);
    // tilt-spot's one Config: owned by tilt-spot with its discriminator (it is created only at
    // ["config"]); the meta list Token-2022 resolved names that address in any case
    // the meta list names tilt-spot's config by address; an account there that is no longer a
    // readable Config (closed, reassigned, rewritten by an upgrade) means the router is gone:
    // treat it as closed rather than freeze the curve (red team F2)
    require_keys_eq!(*spot.key, gate::SPOT_CONFIG, HookError::BadSpotConfig);
    let closed = match spot.try_borrow_data() {
        Ok(d) if *spot.owner == ENGINE_PROGRAM && d.len() > 8 && d[..8] == gate::SPOT_CONFIG_DISC => gate::router_closed(&d, pool_config.as_ref()),
        _ => true,
    };
    // the gate admin's escape hatch (red team F3): router_required false opens every gated curve
    require_keys_eq!(*gcfg.key, gate::GATE_CONFIG, HookError::BadGateConfig);
    let closed = closed || !gate::router_required(gcfg.owner, &gcfg.try_borrow_data()?);
    require!(*gate_ai.owner == crate::ID && gate_ai.is_writable, HookError::BadGate);
    let mut g = gate_ai.try_borrow_mut_data()?;
    require!(gate::is_gate(&g, mint), HookError::BadGate);
    let dest_owner = {
        let d = dest.try_borrow_data()?;
        require!(d.len() >= 64, HookError::BadGate);
        let mut o = [0u8; 32];
        o.copy_from_slice(&d[32..64]);
        o
    };
    let ix_data = ixs.try_borrow_data()?;
    gate::enforce(&mut g, &ix_data, mint, closed, is_out, &dest_owner, slot, amount)
}

/// The program id in a Token-2022 mint's TransferHook extension (type 14), if set. The mint is
/// 82 base bytes padded to 165, then the account type (1 = Mint), then TLV entries.
pub fn mint_hook_program(d: &[u8]) -> Option<Pubkey> {
    const BASE: usize = 165;
    const TRANSFER_HOOK: u16 = 14;
    if d.len() <= BASE || d[BASE] != 1 {
        return None;
    }
    let mut i = BASE + 1;
    while i + 4 <= d.len() {
        let t = u16::from_le_bytes([d[i], d[i + 1]]);
        let l = u16::from_le_bytes([d[i + 2], d[i + 3]]) as usize;
        let v = i + 4;
        if t == 0 || v + l > d.len() {
            return None;
        }
        if t == TRANSFER_HOOK && l >= 64 {
            let k = Pubkey::new_from_array(d[v + 32..v + 64].try_into().unwrap());
            return (k != Pubkey::default()).then_some(k);
        }
        i = v + l;
    }
    None
}

/// Largest move one poke may record, per slot since the newest observation (audit H4): after
/// graduation DAMM swaps do not reach the hook, so a pump, poke and dump in one slot would
/// otherwise leave the pumped price standing in the ring. 1% a slot is about 4.7% a second at the
/// 0.212 s mainnet slots measured 2026-10-23 (was 2% a slot: 5% a second at 0.4 s).
pub const MAX_POKE_STEP_BPS_PER_SLOT: u128 = 100;

/// Clamp a poked price to the step allowed since `last`, or None for a second poke in the same
/// slot (the first poke in a slot wins, so a same-slot pump cannot overwrite an honest poke).
pub fn clamp_poke(last: Option<levpad_math::oracle::Obs>, slot: u64, price: u128) -> Option<u128> {
    let Some(last) = last else { return Some(price) };
    if slot <= last.slot {
        return None;
    }
    let dt = (slot - last.slot) as u128;
    let step = last.price.saturating_mul((MAX_POKE_STEP_BPS_PER_SLOT * dt).min(1_000_000)) / 10_000;
    Some(price.clamp(last.price.saturating_sub(step), last.price.saturating_add(step)))
}

fn create_pda<'info>(
    payer: &AccountInfo<'info>,
    target: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    space: usize,
    seeds: &[&[u8]],
) -> Result<()> {
    let lamports = Rent::get()?.minimum_balance(space).saturating_sub(target.lamports());
    if target.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(payer.key, target.key, lamports, space as u64, &crate::ID),
            &[payer.clone(), target.clone(), system_program.clone()],
            &[seeds],
        )?;
    } else {
        // pre-funded address: top up, allocate, assign
        if lamports > 0 {
            anchor_lang::solana_program::program::invoke(
                &system_instruction::transfer(payer.key, target.key, lamports),
                &[payer.clone(), target.clone(), system_program.clone()],
            )?;
        }
        invoke_signed(&system_instruction::allocate(target.key, space as u64), &[target.clone(), system_program.clone()], &[seeds])?;
        invoke_signed(&system_instruction::assign(target.key, &crate::ID), &[target.clone(), system_program.clone()], &[seeds])?;
    }
    Ok(())
}

#[program]
pub mod levpad_hook {
    use super::*;

    /// Part of the launch transaction, right after DBC's pool init: writes the coin's
    /// ExtraAccountMetaList (DBC pool readonly, ring writable) and creates the ring with the
    /// engine market as its header. Permissionless and idempotent: the contents follow from the
    /// mint and on-chain state, so whoever calls it writes the same thing.
    ///
    /// Fair open: when `gate_config` exists and the pool's DBC config is one of tilt-spot's
    /// counted `dbc_configs`, the list is v2 (adds the instructions sysvar and the gate) and the
    /// gate is created here, recording this slot as the creation slot. Otherwise (the platform
    /// token, whose config the router does not count; any coin before `init_gate_config`) the list
    /// stays v1. The caller cannot choose: `gate_config`, `spot_config` and `gate` are fixed PDAs.
    pub fn initialize_extra_account_meta_list(ctx: Context<InitializeExtraAccountMetaList>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        require_keys_eq!(*ctx.accounts.mint.owner, TOKEN_2022, HookError::NotToken2022);
        {
            // the mint must use this program as its hook (audit H5): no ring for other hooks' coins
            let d = ctx.accounts.mint.try_borrow_data()?;
            require!(mint_hook_program(&d) == Some(crate::ID), HookError::NotOurHook);
        }
        let (pool_config, creator, supply) = {
            let d = ctx.accounts.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(ctx.accounts.dbc_pool.owner, &d, &mint), HookError::NotDbcPool);
            (venue::key_at(&d, dbc::CONFIG), venue::key_at(&d, dbc::CREATOR), venue::u64_at(&d, dbc::BASE_RESERVE))
        };
        let payer = ctx.accounts.payer.to_account_info();
        let sys = ctx.accounts.system_program.to_account_info();

        let ring_ai = ctx.accounts.ring.to_account_info();
        if *ring_ai.owner != crate::ID {
            let bump = ctx.bumps.ring;
            create_pda(&payer, &ring_ai, &sys, ring::LEN, &[OBS_SEED, mint.as_ref(), &[bump]])?;
            ring::init(&mut ring_ai.try_borrow_mut_data()?, &engine_market(&mint).to_bytes(), bump);
        }

        let list_ai = ctx.accounts.extra_account_meta_list.to_account_info();
        if *list_ai.owner != crate::ID {
            let bump = ctx.bumps.extra_account_meta_list;
            let params = gate_params(&ctx.accounts.gate_config, &ctx.accounts.spot_config, &pool_config)?;
            if let Some((base, hundredths, ramp_slots, creator_cap_bps)) = params {
                let gate_ai = ctx.accounts.gate.to_account_info();
                let gbump = ctx.bumps.gate;
                if *gate_ai.owner != crate::ID {
                    create_pda(&payer, &gate_ai, &sys, gate::LEN, &[gate::GATE_SEED, mint.as_ref(), &[gbump]])?;
                }
                gate::init(&mut gate_ai.try_borrow_mut_data()?, &mint, &creator, Clock::get()?.slot, supply, base, hundredths, ramp_slots, creator_cap_bps, gbump);
                let space = ExtraAccountMetaList::size_of(META_COUNT_V2)?;
                create_pda(&payer, &list_ai, &sys, space, &[META_SEED, mint.as_ref(), &[bump]])?;
                let metas = extra_metas_v2(&ctx.accounts.dbc_pool.key(), &ring_ai.key(), &gate_ai.key(), &ctx.accounts.spot_config.key(), &ctx.accounts.gate_config.key())?;
                ExtraAccountMetaList::init::<ExecuteInstruction>(&mut list_ai.try_borrow_mut_data()?, &metas)?;
            } else {
                let space = ExtraAccountMetaList::size_of(META_COUNT)?;
                create_pda(&payer, &list_ai, &sys, space, &[META_SEED, mint.as_ref(), &[bump]])?;
                let metas = extra_metas(&ctx.accounts.dbc_pool.key(), &ring_ai.key())?;
                ExtraAccountMetaList::init::<ExecuteInstruction>(&mut list_ai.try_borrow_mut_data()?, &metas)?;
            }
        }
        Ok(())
    }

    /// Token-2022's transfer hook `Execute`. Never fails for a v1 coin; on a gated coin it fails a
    /// curve swap that breaks the fair-open rules. See `on_transfer`.
    #[instruction(discriminator = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE)]
    pub fn execute<'info>(ctx: Context<'_, '_, 'info, 'info, Execute>, _amount: u64) -> Result<()> {
        on_transfer(ctx.remaining_accounts, _amount)
    }

    /// Fair open: creates `GateConfig` (64 bytes with its tail) with the default ramp (25 bps, no
    /// hundredths override, 16 slots). Signed by the
    /// hook's upgrade authority, which becomes the gate admin. From then on new coins on counted
    /// DBC configs launch gated.
    pub fn init_gate_config(ctx: Context<InitGateConfig>) -> Result<()> {
        let c = &mut ctx.accounts.gate_config;
        c.admin = ctx.accounts.admin.key();
        c.base_bps = gate::DEFAULT_BASE_BPS;
        c.ramp_slots = gate::DEFAULT_RAMP_SLOTS;
        c.bump = ctx.bumps.gate_config;
        c.router_required = true;
        c.creator_cap_bps = gate::DEFAULT_CREATOR_CAP_BPS;
        Ok(())
    }

    /// Gate admin: the ramp parameters copied into coins launched from now on (`base_bps`
    /// 1..=10,000; `ramp_slots` 0..=300, 0 = no ramp). Coins already launched keep theirs. A
    /// non-zero `base_hundredths` (set_gate_ramp) still overrides `base_bps`.
    pub fn set_gate_params(ctx: Context<SetGateParams>, base_bps: u16, ramp_slots: u16) -> Result<()> {
        gate::check_params(base_bps, ramp_slots)?;
        let c = &mut ctx.accounts.gate_config;
        c.base_bps = base_bps;
        c.ramp_slots = ramp_slots;
        Ok(())
    }

    /// Gate admin: the ramp base in hundredths of a bps (`base_hundredths`, 0..=65,535; non-zero
    /// overrides `base_bps`, 0 goes back to it) and `ramp_slots` (0..=300), for coins launched from
    /// now on. Grows a GateConfig created before the field (48 bytes) to 64, the admin paying the
    /// rent. Any base up to 0.30 bps (30) keeps the cap under the supply through slot 16; the
    /// deployments set 0.23 bps (23), which also refuses a whole-curve buy in slot 16 (SPEC
    /// "Fair open").
    pub fn set_gate_ramp(ctx: Context<SetGateRamp>, base_hundredths: u16, ramp_slots: u16) -> Result<()> {
        require!(ramp_slots <= gate::MAX_RAMP_SLOTS, HookError::BadGateParams);
        ctx.accounts.gate_config.ramp_slots = ramp_slots;
        let ai = ctx.accounts.gate_config.to_account_info();
        if ai.data_len() < gate::GC_LEN_V2 {
            let need = Rent::get()?.minimum_balance(gate::GC_LEN_V2).saturating_sub(ai.lamports());
            if need > 0 {
                invoke_signed(
                    &system_instruction::transfer(ctx.accounts.admin.key, ai.key, need),
                    &[ctx.accounts.admin.to_account_info(), ai.clone(), ctx.accounts.system_program.to_account_info()],
                    &[],
                )?;
            }
            ai.realloc(gate::GC_LEN_V2, true)?;
        }
        ai.try_borrow_mut_data()?[gate::GC_BASE_HUNDREDTHS..gate::GC_BASE_HUNDREDTHS + 2].copy_from_slice(&base_hundredths.to_le_bytes());
        Ok(())
    }

    /// Gate admin: the creator's creation-slot cap (bps of the supply, 0..=10,000) copied into
    /// coins launched from now on.
    pub fn set_creator_cap(ctx: Context<SetGateParams>, creator_cap_bps: u16) -> Result<()> {
        require!(creator_cap_bps <= gate::MAX_CREATOR_CAP_BPS, HookError::BadGateParams);
        ctx.accounts.gate_config.creator_cap_bps = creator_cap_bps;
        Ok(())
    }

    /// Gate admin, the escape hatch (red team F3): false lets any route trade every gated coin on
    /// its curve at once (router-only and one-buy rules off; creation slot, creator cap and ramp
    /// stay), for when tilt-spot breaks or goes away; true restores the router rule.
    pub fn set_router_required(ctx: Context<SetGateParams>, router_required: bool) -> Result<()> {
        ctx.accounts.gate_config.router_required = router_required;
        Ok(())
    }

    /// Ties the coin to its graduated DAMM v2 pool, once, after DBC has migrated. The pool must
    /// be the PDA of a DAMM v2 config whose `pool_creator_authority` is DBC's pool authority, for
    /// the pair (base mint, its quote: WSOL, or as token B any other quote DBC migrated). Only DBC can create pools under those configs, and DBC
    /// migrates a curve once, so nobody can bind a pool of their own (see SPEC.md, graduation).
    pub fn bind_damm(ctx: Context<BindDamm>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        // the curve's final price: DBC freezes `sqrt_price` at migration and opens the DAMM v2 pool
        // at exactly that square root (tests/tests/redteam_levcap_levfee.rs checks it on the devnet
        // builds), so it is the pool's opening price and nobody can move it any more
        let curve_final = {
            let d = ctx.accounts.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(ctx.accounts.dbc_pool.owner, &d, &mint), HookError::NotDbcPool);
            require!(d[dbc::IS_MIGRATED] == 1, HookError::NotMigrated);
            levpad_math::price_from_sqrt_q64(venue::u128_at(&d, dbc::SQRT_PRICE)).map_err(|_| error!(HookError::BadDammPrice))?
        };
        let cfg = &ctx.accounts.damm_config;
        {
            let d = cfg.try_borrow_data()?;
            let o = damm::CONFIG_POOL_CREATOR_AUTHORITY;
            require!(
                *cfg.owner == DAMM_V2_PROGRAM
                    && d.len() == damm::CONFIG_LEN
                    && d[..8] == damm::CONFIG_DISC
                    && d[o..o + 32] == DBC_POOL_AUTHORITY.to_bytes(),
                HookError::NotDbcMigrationConfig
            );
        }
        let pool = &ctx.accounts.damm_pool;
        {
            let d = pool.try_borrow_data()?;
            require!(
                *pool.owner == DAMM_V2_PROGRAM && d.len() >= damm::POOL_LEN && d[..8] == damm::POOL_DISC,
                HookError::NotMigrationPool
            );
            let (a, b) = (venue::key_at(&d, damm::TOKEN_A_MINT), venue::key_at(&d, damm::TOKEN_B_MINT));
            // the coin's quote (multi-quote, 2026-10-23): SOL either way round as before; any other
            // quote only as DBC migrates it, the coin as token A. A DBC pool whose *quote* is this
            // coin migrates into (other base, coin) with the coin as B, so it can never be bound
            // here: a mint is the base of exactly one DBC pool, its own.
            let quote = if a == mint { b } else { a };
            require!((a == mint && b == quote) || (quote == WSOL && b == mint), HookError::NotMigrationPool);
            require_keys_eq!(pool.key(), damm_pool_address(&cfg.key(), &mint, &quote), HookError::NotMigrationPool);
            require_keys_eq!(venue::key_at(&d, damm::CREATOR), DBC_POOL_AUTHORITY, HookError::NotMigrationPool);
        }
        {
            let d = ctx.accounts.ring.try_borrow_data()?;
            require!(ring::is_ring(&d), HookError::BadRing);
        }
        let link = &mut ctx.accounts.link;
        link.mint = mint;
        link.damm_pool = pool.key();
        link.ring = ctx.accounts.ring.key();
        link.bound_slot = Clock::get()?.slot;
        link.bump = ctx.bumps.link;
        // the venue switched: restart the ring (econ red team E6), so no ring TWAP mixes the curve's
        // prices with the pool's and fills wait for a full window of the pool. The seed is the
        // lower of the curve's final price and the pool's live price (red team F2, levcap-levfee):
        // the live price alone lets whoever binds pump the pool in the same transaction and leave
        // the pumped price standing until the next poke (pokes then walk down at most
        // MAX_POKE_STEP_BPS_PER_SLOT per slot), inflating every TWAP that reads the ring; the frozen
        // curve price alone would hide a real drain at graduation (E6) behind the same clamp. The
        // lower of the two can be pushed only down, by selling into the pool, which helps no
        // reader: shorts are gated after graduation, and levfee's buyback guard refuses buys while
        // spot is above the TWAP. The cumulative carries on across the restart (`ring::restart`),
        // so the engine's anti-farming samples stay exact across graduation.
        {
            let q = {
                let d = pool.try_borrow_data()?;
                venue::damm_quote(&d, &mint).map_err(|_| error!(HookError::BadDammPrice))?
            };
            let seed = q.price.min(curve_final);
            require!(seed > 0, HookError::BadDammPrice);
            let ring_ai = ctx.accounts.ring.to_account_info();
            let mut d = ring_ai.try_borrow_mut_data()?;
            ring::restart(&mut d, &engine_market(&mint).to_bytes(), Clock::get()?.slot, seed, q.quote_reserve).map_err(|_| error!(HookError::BadDammPrice))?;
        }
        emit!(DammBound { mint, damm_pool: pool.key(), damm_config: cfg.key() });
        Ok(())
    }

    /// Permissionless crank: record the bound DAMM v2 pool's price and quote reserve.
    pub fn poke_damm(ctx: Context<PokeDamm>) -> Result<()> {
        let mint = ctx.accounts.link.mint;
        let q = {
            let pool = &ctx.accounts.damm_pool;
            require_keys_eq!(*pool.owner, DAMM_V2_PROGRAM, HookError::NotMigrationPool);
            let d = pool.try_borrow_data()?;
            require!(d.len() >= damm::POOL_LEN && d[..8] == damm::POOL_DISC, HookError::NotMigrationPool);
            venue::damm_quote(&d, &mint).map_err(|_| error!(HookError::BadDammPrice))?
        };
        require!(q.price > 0, HookError::BadDammPrice);
        let mut d = ctx.accounts.ring.try_borrow_mut_data()?;
        require!(ring::is_ring(&d), HookError::BadRing);
        let slot = Clock::get()?.slot;
        // the first poke in a slot wins; a later one in the same slot is a no-op rather than an
        // error, so several keeper transactions in one slot can each carry the poke the engine
        // requires without all but one failing (QA Q9)
        let Some(price) = clamp_poke(ring::newest(&d), slot, q.price) else {
            return Ok(());
        };
        ring::record(&mut d, slot, price, q.quote_reserve).map_err(|_| error!(HookError::BadDammPrice))?;
        Ok(())
    }

    /// Permissionless crank for the curve phase: record the DBC pool's current price and quote
    /// reserve into the ring (the engine requires a fresh observation for every fill, close and
    /// liquidation). The curve's state is the truth (every swap moves it and records), so no clamp
    /// is needed; a second poke in a slot only rewrites the same price.
    pub fn poke_dbc(ctx: Context<PokeDbc>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let (price, quote_reserve) = {
            let pool = &ctx.accounts.dbc_pool;
            let d = pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(pool.owner, &d, &mint), HookError::NotDbcPool);
            require!(d[dbc::IS_MIGRATED] == 0 && d[dbc::MIGRATION_PROGRESS] == 0, HookError::CurveNotLive);
            let price = levpad_math::price_from_sqrt_q64(venue::u128_at(&d, dbc::SQRT_PRICE)).map_err(|_| error!(HookError::BadDammPrice))?;
            require!(price > 0, HookError::BadDammPrice);
            (price, venue::u64_at(&d, dbc::QUOTE_RESERVE))
        };
        let mut d = ctx.accounts.ring.try_borrow_mut_data()?;
        require!(ring::is_ring(&d), HookError::BadRing);
        let slot = Clock::get()?.slot;
        ring::record(&mut d, slot, price, quote_reserve).map_err(|_| error!(HookError::BadDammPrice))?;
        Ok(())
    }

    /// pump.fun listed markets: creates the ring at ["obs", mint] (header: the engine market
    /// ["market", mint], as for a launched coin) and the coin's `PumpFeed`, for a SOL-quoted coin
    /// with a pump.fun bonding curve. Permissionless: the contents follow from the mint and the
    /// verified curve, the ring alone lists nothing (only the engine admin's `create_market` +
    /// `set_obs` does), and letting anyone create it lets the keeper or the lister warm the ring
    /// before the admin acts. The ring starts empty; the first `poke_pump` (or `bind_pump_amm` for
    /// a coin that has already migrated) writes it.
    pub fn init_pump_ring(ctx: Context<InitPumpRing>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        {
            let m = &ctx.accounts.mint;
            let d = m.try_borrow_data()?;
            // an initialized mint (82-byte base state, `is_initialized` at 45) of either token program
            require!((*m.owner == venue::pump::TOKEN || *m.owner == TOKEN_2022) && d.len() >= 82 && d[45] == 1, HookError::NotMint);
        }
        let curve = &ctx.accounts.bonding_curve;
        let (want, _) = Pubkey::find_program_address(&[venue::pump::CURVE_SEED, mint.as_ref()], &venue::pump::PUMP_PROGRAM);
        require_keys_eq!(curve.key(), want, HookError::NotPumpCurve);
        require_keys_eq!(*curve.owner, venue::pump::PUMP_PROGRAM, HookError::NotPumpCurve);
        {
            let d = curve.try_borrow_data()?;
            require!(d.len() >= venue::pump::CURVE_MIN_LEN && d[..8] == venue::pump::CURVE_DISC, HookError::NotPumpCurve);
            require!(venue::pump_curve_sol_quoted(&d), HookError::PumpNotSol);
        }
        let ring_ai = ctx.accounts.ring.to_account_info();
        require!(*ring_ai.owner != crate::ID, HookError::RingExists);
        let bump = ctx.bumps.ring;
        create_pda(&ctx.accounts.payer.to_account_info(), &ring_ai, &ctx.accounts.system_program.to_account_info(), ring::LEN, &[OBS_SEED, mint.as_ref(), &[bump]])?;
        ring::init(&mut ring_ai.try_borrow_mut_data()?, &engine_market(&mint).to_bytes(), bump);
        let f = &mut ctx.accounts.feed;
        f.mint = mint;
        f.bonding_curve = curve.key();
        f.pool = Pubkey::default();
        f.ring = ring_ai.key();
        f.phase = PHASE_CURVE;
        f.created_slot = Clock::get()?.slot;
        f.bound_slot = 0;
        f.bump = ctx.bumps.feed;
        Ok(())
    }

    /// Permissionless crank for a pump coin: sample the bonding curve (before the migration) or
    /// the bound PumpSwap pool (after) into the ring. Top-level only, in a transaction that calls
    /// nothing but compute budget, this program and the engine (`pump::gate`); the first poke in a
    /// slot wins (later ones are no-ops, as in `poke_damm`); the price moves at most
    /// `MAX_POKE_STEP_BPS_PER_SLOT` per slot and the recorded reserve rises at most as fast.
    /// Accounts: the curve (phase 0) or the pool (phase 1) as `venue`; in phase 1 the pool's base
    /// and quote vaults, in phase 0 any two accounts (the keeper passes the curve twice).
    pub fn poke_pump(ctx: Context<PokePump>) -> Result<()> {
        pump::gate(&ctx.accounts.instructions)?;
        let f = &ctx.accounts.feed;
        let q = read_pump(f, &ctx.accounts.venue, &ctx.accounts.base_vault, &ctx.accounts.quote_vault)?;
        let mut d = ctx.accounts.ring.try_borrow_mut_data()?;
        require!(ring::is_ring(&d), HookError::BadRing);
        let slot = Clock::get()?.slot;
        let newest = ring::newest(&d);
        let Some(price) = clamp_poke(newest, slot, q.price) else {
            return Ok(());
        };
        let reserve = pump::clamp_reserve(newest.map(|o| (o.slot, venue::u64_at(&d, 56))), slot, q.quote_reserve);
        ring::record(&mut d, slot, price, reserve).map_err(|_| error!(HookError::BadPumpPrice))?;
        Ok(())
    }

    /// Permissionless, once per coin, after pump.fun has completed the curve and migrated it: binds
    /// the canonical PumpSwap pool (index 0, created by pump's ["pool-authority", mint], quoted in
    /// WSOL, at its PDA; only pump's migration can create it) and restarts the ring there, as
    /// `bind_damm` does for DBC coins (econ E6: no TWAP mixes the two venues). The seed is the
    /// lower of the ring's last curve price and the pool's live price (red team F2: whoever binds
    /// cannot leave a pumped seed standing; the lower can only be pushed down, which shorts cannot
    /// use yet). Also writes `PumpLink` at ["damm", mint] for the engine's `mark_graduated`.
    /// Same transaction rule as `poke_pump`.
    pub fn bind_pump_amm(ctx: Context<BindPumpAmm>) -> Result<()> {
        pump::gate(&ctx.accounts.instructions)?;
        let mint = ctx.accounts.feed.mint;
        require!(ctx.accounts.feed.phase == PHASE_CURVE, HookError::WrongPumpPhase);
        {
            let c = &ctx.accounts.bonding_curve;
            require_keys_eq!(c.key(), ctx.accounts.feed.bonding_curve, HookError::NotPumpCurve);
            require_keys_eq!(*c.owner, venue::pump::PUMP_PROGRAM, HookError::NotPumpCurve);
            let d = c.try_borrow_data()?;
            require!(venue::pump_curve_complete(&d), HookError::PumpCurveLive);
        }
        let pool = &ctx.accounts.pool;
        let (auth, _) = Pubkey::find_program_address(&[venue::pump::POOL_AUTHORITY_SEED, mint.as_ref()], &venue::pump::PUMP_PROGRAM);
        let (want, _) = Pubkey::find_program_address(
            &[venue::pump::POOL_SEED, &0u16.to_le_bytes(), auth.as_ref(), mint.as_ref(), WSOL.as_ref()],
            &venue::pump::PUMP_AMM_PROGRAM,
        );
        require_keys_eq!(pool.key(), want, HookError::NotPumpPool);
        require_keys_eq!(*pool.owner, venue::pump::PUMP_AMM_PROGRAM, HookError::NotPumpPool);
        {
            let d = pool.try_borrow_data()?;
            require!(d.len() >= venue::pump::POOL_MIN_LEN && d[..8] == venue::pump::POOL_DISC, HookError::NotPumpPool);
            require!(
                venue::key_at(&d, venue::pump::POOL_BASE_MINT) == mint
                    && venue::key_at(&d, venue::pump::POOL_QUOTE_MINT) == WSOL
                    && venue::key_at(&d, venue::pump::POOL_CREATOR) == auth,
                HookError::NotPumpPool
            );
        }
        let f = &mut ctx.accounts.feed;
        f.pool = pool.key();
        f.phase = PHASE_AMM;
        let slot = Clock::get()?.slot;
        f.bound_slot = slot;
        let q = read_pump(f, pool, &ctx.accounts.base_vault, &ctx.accounts.quote_vault)?;
        let ring_ai = ctx.accounts.ring.to_account_info();
        let mut d = ring_ai.try_borrow_mut_data()?;
        require!(ring::is_ring(&d), HookError::BadRing);
        let seed = match ring::newest(&d) {
            Some(o) => o.price.min(q.price),
            None => q.price,
        };
        ring::restart(&mut d, &engine_market(&mint).to_bytes(), slot, seed, q.quote_reserve).map_err(|_| error!(HookError::BadPumpPrice))?;
        let l = &mut ctx.accounts.link;
        l.mint = mint;
        l.pool = pool.key();
        l.ring = ring_ai.key();
        l.bound_slot = slot;
        l.bump = ctx.bumps.link;
        emit!(pump::PumpBound { mint, pool: pool.key(), seed_price: seed });
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeExtraAccountMetaList<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: created here; PDA ["extra-account-metas", mint]
    #[account(mut, seeds = [META_SEED, mint.key().as_ref()], bump)]
    pub extra_account_meta_list: UncheckedAccount<'info>,
    /// CHECK: owner checked (Token-2022); the DBC pool must name it as its base mint
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked in the handler (owner DBC, TransferHookPool, base_mint)
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: created here; PDA ["obs", mint]
    #[account(mut, seeds = [OBS_SEED, mint.key().as_ref()], bump)]
    pub ring: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: ["gate_config"]; it may not exist yet (then no coin is gated)
    #[account(seeds = [gate::GATE_CONFIG_SEED], bump)]
    pub gate_config: UncheckedAccount<'info>,
    /// CHECK: tilt-spot's Config ["config"], read for its counted DBC configs
    #[account(seeds = [gate::SPOT_CONFIG_SEED], bump, seeds::program = ENGINE_PROGRAM)]
    pub spot_config: UncheckedAccount<'info>,
    /// CHECK: created here for a gated coin; PDA ["gate", mint]
    #[account(mut, seeds = [gate::GATE_SEED, mint.key().as_ref()], bump)]
    pub gate: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct InitGateConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + gate::GateConfig::INIT_SPACE + gate::GC_TAIL_LEN, seeds = [gate::GATE_CONFIG_SEED], bump)]
    pub gate_config: Account<'info, gate::GateConfig>,
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ HookError::NotGateAdmin,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetGateParams<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [gate::GATE_CONFIG_SEED], bump = gate_config.bump, has_one = admin @ HookError::NotGateAdmin)]
    pub gate_config: Account<'info, gate::GateConfig>,
}

#[derive(Accounts)]
pub struct SetGateRamp<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(mut, seeds = [gate::GATE_CONFIG_SEED], bump = gate_config.bump, has_one = admin @ HookError::NotGateAdmin)]
    pub gate_config: Account<'info, gate::GateConfig>,
    pub system_program: Program<'info, System>,
}

/// The ramp parameters for a new coin, or None for a v1 coin: `GateConfig` must exist and the
/// pool's DBC config must be one tilt-spot's router counts (a gated coin can trade on its curve
/// only through the router, so a coin the router refuses must never be gated).
fn gate_params(gate_config: &AccountInfo, spot_config: &AccountInfo, pool_config: &Pubkey) -> Result<Option<(u16, bool, u16, u16)>> {
    if *gate_config.owner != crate::ID || *spot_config.owner != ENGINE_PROGRAM || *pool_config == Pubkey::default() {
        return Ok(None);
    }
    let counted = gate::spot_counts(&spot_config.try_borrow_data()?, pool_config.as_ref());
    if !counted {
        return Ok(None);
    }
    let d = gate_config.try_borrow_data()?;
    let c = gate::GateConfig::try_deserialize(&mut &d[..])?;
    let (base, hundredths) = gate::effective_base(c.base_bps, gate::base_hundredths(&d));
    Ok(Some((base, hundredths, c.ramp_slots, c.creator_cap_bps)))
}

/// Every account arrives in `remaining_accounts` so that account validation can never fail.
#[derive(Accounts)]
pub struct Execute {}

#[derive(Accounts)]
pub struct BindDamm<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the DBC pool must name it as its base mint
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked in the handler
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: checked in the handler
    pub damm_config: UncheckedAccount<'info>,
    /// CHECK: checked in the handler
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: this coin's ring (reset at the binding)
    #[account(mut, seeds = [OBS_SEED, mint.key().as_ref()], bump, owner = crate::ID)]
    pub ring: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + DammLink::INIT_SPACE, seeds = [DAMM_LINK_SEED, mint.key().as_ref()], bump)]
    pub link: Account<'info, DammLink>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PokeDbc<'info> {
    /// CHECK: the DBC pool must name it as its base mint
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked in the handler (DBC-owned TransferHookPool for this mint, curve live)
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: this coin's ring
    #[account(mut, seeds = [OBS_SEED, mint.key().as_ref()], bump, owner = crate::ID)]
    pub ring: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct PokeDamm<'info> {
    #[account(seeds = [DAMM_LINK_SEED, link.mint.as_ref()], bump = link.bump, has_one = damm_pool, has_one = ring)]
    pub link: Account<'info, DammLink>,
    /// CHECK: matched to link.damm_pool; owner and layout checked in the handler
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: matched to link.ring
    #[account(mut, owner = crate::ID)]
    pub ring: UncheckedAccount<'info>,
}

/// The feed's venue, verified: the stored bonding curve in phase 0 (its live reserves; a complete
/// curve still holding reserves is frozen at its final price, an emptied one asks for the bind),
/// the stored pool and its own two vaults in phase 1.
fn read_pump(f: &PumpFeed, venue_ai: &AccountInfo, base_vault: &AccountInfo, quote_vault: &AccountInfo) -> Result<venue::PumpQuote> {
    if f.phase == PHASE_CURVE {
        require_keys_eq!(venue_ai.key(), f.bonding_curve, HookError::NotPumpCurve);
        require_keys_eq!(*venue_ai.owner, venue::pump::PUMP_PROGRAM, HookError::NotPumpCurve);
        let d = venue_ai.try_borrow_data()?;
        return match venue::pump_curve_quote(&d) {
            Some(q) => Ok(q),
            None if venue::pump_curve_complete(&d) => err!(HookError::PumpCurveComplete),
            None => err!(HookError::BadPumpPrice),
        };
    }
    require_keys_eq!(venue_ai.key(), f.pool, HookError::NotPumpPool);
    require_keys_eq!(*venue_ai.owner, venue::pump::PUMP_AMM_PROGRAM, HookError::NotPumpPool);
    let d = venue_ai.try_borrow_data()?;
    require!(d.len() >= venue::pump::POOL_MIN_LEN && d[..8] == venue::pump::POOL_DISC, HookError::NotPumpPool);
    require!(base_vault.key() == venue::key_at(&d, venue::pump::POOL_BASE_VAULT), HookError::BadPumpVault);
    require!(quote_vault.key() == venue::key_at(&d, venue::pump::POOL_QUOTE_VAULT), HookError::BadPumpVault);
    let (bm, base) = venue::token_account(base_vault.owner, &base_vault.try_borrow_data()?).ok_or(error!(HookError::BadPumpVault))?;
    let (qm, quote) = venue::token_account(quote_vault.owner, &quote_vault.try_borrow_data()?).ok_or(error!(HookError::BadPumpVault))?;
    require!(bm == f.mint && qm == WSOL, HookError::BadPumpVault);
    venue::pump_pool_quote(&d, base, quote).ok_or(error!(HookError::BadPumpPrice))
}

#[derive(Accounts)]
pub struct InitPumpRing<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: an initialized SPL Token or Token-2022 mint (checked in the handler)
    pub mint: UncheckedAccount<'info>,
    /// CHECK: ["bonding-curve", mint] under the pump program, owner and layout checked
    pub bonding_curve: UncheckedAccount<'info>,
    /// CHECK: created here; PDA ["obs", mint]
    #[account(mut, seeds = [OBS_SEED, mint.key().as_ref()], bump)]
    pub ring: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + PumpFeed::INIT_SPACE, seeds = [PUMP_FEED_SEED, mint.key().as_ref()], bump)]
    pub feed: Account<'info, PumpFeed>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PokePump<'info> {
    #[account(seeds = [PUMP_FEED_SEED, feed.mint.as_ref()], bump = feed.bump, has_one = ring)]
    pub feed: Account<'info, PumpFeed>,
    /// CHECK: matched to feed.ring
    #[account(mut, owner = crate::ID)]
    pub ring: UncheckedAccount<'info>,
    /// CHECK: the feed's bonding curve (phase 0) or pool (phase 1), checked in the handler
    pub venue: UncheckedAccount<'info>,
    /// CHECK: phase 1: the pool's base vault (checked); phase 0: ignored
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: phase 1: the pool's quote vault (checked); phase 0: ignored
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the instructions sysvar (address checked in `pump::gate`)
    pub instructions: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct BindPumpAmm<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [PUMP_FEED_SEED, feed.mint.as_ref()], bump = feed.bump, has_one = ring)]
    pub feed: Account<'info, PumpFeed>,
    /// CHECK: matched to feed.bonding_curve, must be complete
    pub bonding_curve: UncheckedAccount<'info>,
    /// CHECK: the canonical PumpSwap pool, checked in the handler
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the pool's base vault (checked)
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault (checked)
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: matched to feed.ring (restarted at the binding)
    #[account(mut, owner = crate::ID)]
    pub ring: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + PumpLink::INIT_SPACE, seeds = [DAMM_LINK_SEED, feed.mint.as_ref()], bump)]
    pub link: Account<'info, PumpLink>,
    /// CHECK: the instructions sysvar (address checked in `pump::gate`)
    pub instructions: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}
