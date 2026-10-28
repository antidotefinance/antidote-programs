//! Fair open v1 (SPEC.md, "Fair open"): the anti-bundle rules the hook enforces on a gated coin's
//! DBC curve. A coin is gated when its meta list is v2 (`[dbc_pool, ring, instructions sysvar,
//! gate (writable), tilt-spot config, GateConfig]`, in that order); `initialize_extra_account_meta_list`
//! writes v2 when `GateConfig` exists and the pool's DBC config is one tilt-spot's router counts,
//! and v1 (`[dbc_pool, ring]`) otherwise.
//!
//! The gate is a raw fixed-size account at ["gate", mint] (little-endian):
//!
//! | bytes      | field                                                              |
//! | ---------- | ------------------------------------------------------------------ |
//! | 0..8       | magic `LVGATE01`, or `LVGATE02` when the base is in hundredths      |
//! | 8..40      | mint                                                               |
//! | 40..72     | creator (the DBC pool's creator)                                   |
//! | 72..80     | created_slot (the slot the meta list was written)                  |
//! | 80..88     | supply (the pool's `base_reserve` at that moment)                   |
//! | 88..90     | base (u16, copied from GateConfig): whole bps under magic          |
//! |            | `LVGATE01`, hundredths of a bps under `LVGATE02` (2026-10-23)       |
//! | 90..92     | ramp_slots (u16, copied from GateConfig)                           |
//! | 92         | bump                                                               |
//! | 93         | ledger entries used (0..=32)                                       |
//! | 94..96     | creator_cap_bps (u16, copied from GateConfig)                      |
//! | 96..104    | cur_slot: the slot `cur_out` counts                                |
//! | 104..112   | cur_out: net atoms out of the vault in `cur_slot` (ramp slots only)|
//! | 112..160   | the creator's entry (wallet, slot, atoms: summed creation-slot buys)|
//! | 160..1696  | 32 ledger entries (wallet 32, slot u64, atoms u64): the largest ramp buys|

use anchor_lang::prelude::*;

use crate::HookError;

pub const GATE_SEED: &[u8] = b"gate";
pub const GATE_CONFIG_SEED: &[u8] = b"gate_config";

pub const DEFAULT_BASE_BPS: u16 = 25;
/// 16 slots, about 3.4 s at the 0.212 s mainnet slots measured 2026-10-23 (owner 2026-10-23; was
/// 8, about 3.2 s at 0.4 s). The cap doubles every slot, so at the default base (25 bps) it passes
/// the whole supply at slot 10 (about 2.1 s): later ramp slots cap nothing.
pub const DEFAULT_RAMP_SLOTS: u16 = 16;
pub const MAX_BASE_BPS: u16 = 10_000;
/// About 64 s at 0.212 s slots (was 150: 60 s at 0.4 s).
pub const MAX_RAMP_SLOTS: u16 = 300;
/// The creator's creation-slot buys, in bps of the supply (owner 2026-10-22: 2%).
pub const DEFAULT_CREATOR_CAP_BPS: u16 = 200;
pub const MAX_CREATOR_CAP_BPS: u16 = 10_000;
/// GateConfig's address, `find_program_address(["gate_config"], levpad-hook)` (unit test below).
pub const GATE_CONFIG: Pubkey = pubkey!("GVGLqrfWzT3g5F78oBuZPxSddFp8eSzyou4ASuKp5n4o");
/// GateConfig's Anchor discriminator and the raw offsets the transfer path reads.
pub const GATE_CONFIG_DISC: [u8; 8] = [161, 200, 10, 61, 185, 120, 92, 21];
pub const GC_ROUTER_REQUIRED: usize = 8 + 32 + 2 + 2 + 1;
pub const GC_LEN: usize = GC_ROUTER_REQUIRED + 1 + 2;
/// GateConfig's raw tail after the Anchor struct (2026-10-23; a GateConfig created earlier is
/// `GC_LEN` long and has none until `set_gate_ramp` grows it, which reads as 0):
/// `base_hundredths` (u16): slot 1's cap in hundredths of a bps of the supply; when non-zero it
/// overrides `base_bps` and new coins get an `LVGATE02` gate. Then 14 reserved bytes.
pub const GC_BASE_HUNDREDTHS: usize = GC_LEN;
pub const GC_TAIL_LEN: usize = 16;
pub const GC_LEN_V2: usize = GC_LEN + GC_TAIL_LEN;
/// The default the deployment scripts set (2026-10-23): 0.23 bps. The cap stays under the supply
/// through slot 16 for any base up to 0.30 bps (0.30 x 2^15 = 98.3%), but a curve sells only part
/// of its supply (the standard config 75.9%, the production test curve 78.6%; the rest is the
/// LP), so a cap above that binds nothing. 0.23 x 2^15 = 75.37% is the largest base that still
/// refuses a whole-curve buy in slot 16 on both; slot 17 has no cap. The program's own default
/// stays `base_bps` 25 with no override.
pub const RECOMMENDED_BASE_HUNDREDTHS: u16 = 23;

pub const MAGIC: [u8; 8] = *b"LVGATE01";
/// A gate whose base (bytes 88..90) is in hundredths of a bps (GateConfig's `base_hundredths`).
pub const MAGIC_V2: [u8; 8] = *b"LVGATE02";
pub const MINT: usize = 8;
pub const CREATOR: usize = 40;
pub const CREATED_SLOT: usize = 72;
pub const SUPPLY: usize = 80;
pub const BASE_BPS: usize = 88;
pub const RAMP_SLOTS: usize = 90;
pub const BUMP: usize = 92;
pub const COUNT: usize = 93;
pub const CREATOR_CAP_BPS: usize = 94;
pub const CUR_SLOT: usize = 96;
pub const CUR_OUT: usize = 104;
pub const CREATOR_ENTRY: usize = 112;
pub const LEDGER: usize = 160;
pub const ENTRY_LEN: usize = 48;
pub const LEDGER_MAX: usize = 32;
pub const LEN: usize = LEDGER + LEDGER_MAX * ENTRY_LEN;

/// tilt-spot's `trade_dbc` (Anchor discriminator sha256("global:trade_dbc")[..8]; the suite checks
/// it against tilt-spot's own) and the position of `base_mint` in its accounts (IDL order).
pub const TRADE_DBC_DISC: [u8; 8] = [130, 103, 100, 153, 244, 135, 58, 149];
pub const TRADE_DBC_MINT_INDEX: usize = 12;

/// tilt-spot's Config ["config"]: `params.dbc_configs` (8 keys) at this offset (8 disc, admin,
/// pending_admin, 4 flag bytes, then Params: 3 x u16, 3 x u32, PriceParams 3 x u32, MintBudget
/// u32 u16 u64). The suite checks it against a serialized Config.
pub const SPOT_CONFIG_SEED: &[u8] = b"config";
pub const SPOT_DBC_CONFIGS: usize = 120;
pub const SPOT_DBC_CONFIGS_LEN: usize = 8;
/// The Config's address, `find_program_address(["config"], tilt-spot)` (unit test below).
pub const SPOT_CONFIG: Pubkey = pubkey!("8MFhRjab1XSkFSGZuFL9XEYNCwb3SLz72Vz83Mmtyia4");
/// `paused` (bool) in the same Config (8 disc, admin, pending_admin).
pub const SPOT_PAUSED: usize = 72;
/// Anchor's discriminator of tilt-spot's `Config` (sha256("account:Config")[..8]).
pub const SPOT_CONFIG_DISC: [u8; 8] = [155, 12, 170, 224, 30, 250, 204, 130];

/// Whether tilt-spot's config `d` counts the DBC config `pool_config`.
pub fn spot_counts(d: &[u8], pool_config: &[u8]) -> bool {
    let end = SPOT_DBC_CONFIGS + 32 * SPOT_DBC_CONFIGS_LEN;
    d.len() >= end && d[SPOT_DBC_CONFIGS..end].chunks_exact(32).any(|k| k == pool_config)
}

/// The router is not available to this coin: tilt-spot is paused, or no longer counts the coin's
/// DBC config. Then any route may trade the coin (selling is never frozen); the creation-slot and
/// ramp rules still apply.
pub fn router_closed(d: &[u8], pool_config: &[u8]) -> bool {
    d.len() <= SPOT_PAUSED || d[SPOT_PAUSED] != 0 || !spot_counts(d, pool_config)
}

/// Admin-set ramp parameters, copied into each gate when its coin launches.
#[account]
#[derive(InitSpace)]
pub struct GateConfig {
    /// The hook's upgrade authority when `init_gate_config` ran.
    pub admin: Pubkey,
    /// Slot 1's cap in bps of the curve's supply (1..=10,000; default 25).
    pub base_bps: u16,
    /// Slots the cap doubles over (0..=300; 0 turns the ramp off; default 16).
    pub ramp_slots: u16,
    pub bump: u8,
    /// False lets any route trade every gated coin (the router-only and one-buy rules off; the
    /// creation slot, creator cap and ramp stay): the escape hatch if tilt-spot ever breaks
    /// (red team F3). Default true.
    pub router_required: bool,
    /// The creator's creation-slot buys, in bps of the supply (0..=10,000; default 200),
    /// copied into each gate at launch.
    pub creator_cap_bps: u16,
}

/// GateConfig's `router_required`, read raw: true unless the account is this program's GateConfig
/// and says false.
pub fn router_required(owner: &Pubkey, d: &[u8]) -> bool {
    !(*owner == crate::ID && d.len() >= GC_LEN && d[..8] == GATE_CONFIG_DISC && d[GC_ROUTER_REQUIRED] == 0)
}

pub fn check_params(base_bps: u16, ramp_slots: u16) -> Result<()> {
    require!((1..=MAX_BASE_BPS).contains(&base_bps) && ramp_slots <= MAX_RAMP_SLOTS, HookError::BadGateParams);
    Ok(())
}

/// GateConfig's `base_hundredths` (0 when the account predates the tail).
pub fn base_hundredths(d: &[u8]) -> u16 {
    if d.len() >= GC_BASE_HUNDREDTHS + 2 {
        u16_at(d, GC_BASE_HUNDREDTHS)
    } else {
        0
    }
}

/// The ramp base a new gate gets: (base, in hundredths of a bps) from GateConfig's `base_bps` and
/// `base_hundredths` (the latter wins when non-zero).
pub fn effective_base(base_bps: u16, base_hundredths: u16) -> (u16, bool) {
    if base_hundredths != 0 {
        (base_hundredths, true)
    } else {
        (base_bps, false)
    }
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn put_u64(d: &mut [u8], o: usize, v: u64) {
    d[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

#[allow(clippy::too_many_arguments)]
pub fn init(d: &mut [u8], mint: &Pubkey, creator: &Pubkey, created_slot: u64, supply: u64, base: u16, hundredths: bool, ramp_slots: u16, creator_cap_bps: u16, bump: u8) {
    d[..LEN].fill(0);
    d[..8].copy_from_slice(if hundredths { &MAGIC_V2 } else { &MAGIC });
    d[MINT..MINT + 32].copy_from_slice(mint.as_ref());
    d[CREATOR..CREATOR + 32].copy_from_slice(creator.as_ref());
    put_u64(d, CREATED_SLOT, created_slot);
    put_u64(d, SUPPLY, supply);
    d[BASE_BPS..BASE_BPS + 2].copy_from_slice(&base.to_le_bytes());
    d[RAMP_SLOTS..RAMP_SLOTS + 2].copy_from_slice(&ramp_slots.to_le_bytes());
    d[CREATOR_CAP_BPS..CREATOR_CAP_BPS + 2].copy_from_slice(&creator_cap_bps.to_le_bytes());
    d[BUMP] = bump;
}

pub fn is_gate(d: &[u8], mint: &Pubkey) -> bool {
    d.len() == LEN && (d[..8] == MAGIC || d[..8] == MAGIC_V2) && d[MINT..MINT + 32] == mint.to_bytes()
}

/// The denominator of a gate's base: 10,000 (whole bps, `LVGATE01`) or 1,000,000 (hundredths of
/// a bps, `LVGATE02`).
pub fn base_denom(g: &[u8]) -> u128 {
    if g[..8] == MAGIC_V2 {
        1_000_000
    } else {
        10_000
    }
}

/// Slot `k`'s cap (k >= 1): `supply x base_bps x 2^(k-1) / 10,000`, or None (no cap) once
/// `k > ramp_slots`.
pub fn ramp_cap(supply: u64, base_bps: u16, ramp_slots: u16, k: u64) -> Option<u64> {
    ramp_cap_in(supply, base_bps, 10_000, ramp_slots, k)
}

/// Slot `k`'s cap with the base in units of 1 / `denom` of the supply (10,000: bps; 1,000,000:
/// hundredths of a bps): `supply x base x 2^(k-1) / denom`, or None once `k > ramp_slots`.
pub fn ramp_cap_in(supply: u64, base: u16, denom: u128, ramp_slots: u16, k: u64) -> Option<u64> {
    if k == 0 || k > ramp_slots as u64 {
        return None;
    }
    // from 2^40 on the cap is above any u64 supply (base >= 1, denom <= 1,000,000 < 2^20), and
    // supply x base x 2^39 < 2^64 x 2^16 x 2^39 fits a u128
    if k - 1 >= 40 {
        return Some(u64::MAX);
    }
    let cap = ((supply as u128 * base as u128) << (k - 1)) / denom;
    Some(u64::try_from(cap).unwrap_or(u64::MAX))
}

/// What the instructions sysvar says about this transaction for `mint`: whether the top-level
/// instruction now executing is tilt-spot's `trade_dbc` of this mint, and how many top-level
/// `trade_dbc` buys of this mint the transaction holds. None on a malformed sysvar. Layout:
/// u16 count, u16 offsets, then per instruction u16 n_accounts, n x (u8 flags, pubkey),
/// program id, u16 data length, data; the last two bytes are the current index.
pub fn scan(d: &[u8], mint: &Pubkey) -> Option<(bool, u32)> {
    let rd16 = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let n = rd16(0)?;
    let current = rd16(d.len().checked_sub(2)?)?;
    let (mut here, mut buys) = (false, 0u32);
    for i in 0..n {
        let at = rd16(2 + 2 * i)?;
        let accs = rd16(at)?;
        let p = at + 2 + accs * 33;
        let prog = d.get(p..p + 32)?;
        let len = rd16(p + 32)?;
        let data = d.get(p + 34..p + 34 + len)?;
        let is_trade = prog == crate::ENGINE_PROGRAM.as_ref()
            && len >= 9
            && data[..8] == TRADE_DBC_DISC
            && accs > TRADE_DBC_MINT_INDEX
            && d.get(at + 2 + TRADE_DBC_MINT_INDEX * 33 + 1..at + 2 + TRADE_DBC_MINT_INDEX * 33 + 33)? == mint.as_ref();
        if is_trade && data[8] == 1 {
            buys += 1;
        }
        if i == current {
            here = is_trade;
        }
    }
    Some((here, buys))
}

/// The rules for one transfer into (`is_out` false) or out of the vault of a gated coin. `ixs` is
/// the instructions sysvar, `g` the gate, `dest_owner` the owner of the destination token account,
/// `router_closed` from tilt-spot's config (then the router-only and one-buy rules are off).
#[allow(clippy::too_many_arguments)]
pub fn enforce(g: &mut [u8], ixs: &[u8], mint: &Pubkey, router_closed: bool, is_out: bool, dest_owner: &[u8], slot: u64, amount: u64) -> Result<()> {
    if !router_closed {
        let (here, buys) = scan(ixs, mint).ok_or(error!(HookError::BadInstructionsSysvar))?;
        require!(here, HookError::NotRouter);
        require!(buys <= 1, HookError::OneBuyPerTx);
    }
    if !is_out {
        // sells are never capped; one in the slot being counted frees room (net outflow)
        if u64_at(g, CUR_SLOT) == slot {
            put_u64(g, CUR_OUT, u64_at(g, CUR_OUT).saturating_sub(amount));
        }
        return Ok(());
    }
    let created = u64_at(g, CREATED_SLOT);
    if slot <= created {
        require!(dest_owner == &g[CREATOR..CREATOR + 32], HookError::CreationSlotCreatorOnly);
        let e = CREATOR_ENTRY;
        let atoms = u64_at(g, e + 40).saturating_add(amount);
        // the creator's creation-slot buys, summed, at most creator_cap_bps of the supply
        let cap = (u64_at(g, SUPPLY) as u128 * u16_at(g, CREATOR_CAP_BPS) as u128 / 10_000) as u64;
        require!(atoms <= cap, HookError::CreatorCap);
        g.copy_within(CREATOR..CREATOR + 32, e);
        put_u64(g, e + 32, created);
        put_u64(g, e + 40, atoms);
        return Ok(());
    }
    let k = slot - created;
    let Some(cap) = ramp_cap_in(u64_at(g, SUPPLY), u16_at(g, BASE_BPS), base_denom(g), u16_at(g, RAMP_SLOTS), k) else {
        return Ok(());
    };
    if u64_at(g, CUR_SLOT) != slot {
        put_u64(g, CUR_SLOT, slot);
        put_u64(g, CUR_OUT, 0);
    }
    let out = u64_at(g, CUR_OUT).saturating_add(amount);
    require!(out <= cap, HookError::RampCap);
    put_u64(g, CUR_OUT, out);
    record(g, dest_owner, slot, amount);
    Ok(())
}

/// The opening ledger keeps the 32 largest ramp buys (red team F4): append while there is room,
/// then a buy larger than the smallest entry replaces it (of equal smallest entries, the latest
/// goes, so equal sizes keep the earlier buys); a buy no larger than the smallest is not kept.
pub fn record(g: &mut [u8], wallet: &[u8], slot: u64, amount: u64) {
    let n = g[COUNT] as usize;
    let at = if n < LEDGER_MAX {
        g[COUNT] = (n + 1) as u8;
        n
    } else {
        let mut min_i = 0;
        let mut min = u64::MAX;
        for i in 0..LEDGER_MAX {
            let a = u64_at(g, LEDGER + i * ENTRY_LEN + 40);
            if a <= min {
                min = a;
                min_i = i;
            }
        }
        if amount <= min {
            return;
        }
        min_i
    };
    let e = LEDGER + at * ENTRY_LEN;
    g[e..e + 32].copy_from_slice(&wallet[..32]);
    put_u64(g, e + 32, slot);
    put_u64(g, e + 40, amount);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_config_address_is_the_pda() {
        assert_eq!(GATE_CONFIG, Pubkey::find_program_address(&[GATE_CONFIG_SEED], &crate::ID).0);
        assert_eq!(GC_LEN, 8 + GateConfig::INIT_SPACE);
        assert_eq!(&GATE_CONFIG_DISC[..], <GateConfig as anchor_lang::Discriminator>::DISCRIMINATOR);
    }

    #[test]
    fn ledger_keeps_the_largest() {
        let mut g = vec![0u8; LEN];
        let w = |i: u8| [i; 32];
        for i in 0..LEDGER_MAX as u8 {
            record(&mut g, &w(i), 1, 10 + (i as u64 % 3));
        }
        assert_eq!(g[COUNT] as usize, LEDGER_MAX);
        record(&mut g, &w(200), 2, 10); // not larger than the smallest: dropped
        assert!((0..LEDGER_MAX).all(|i| g[LEDGER + i * ENTRY_LEN] != 200));
        record(&mut g, &w(201), 2, 1_000); // replaces the latest of the smallest (wallet 30)
        let at = |i: usize| (g[LEDGER + i * ENTRY_LEN], u64_at(&g, LEDGER + i * ENTRY_LEN + 40));
        assert_eq!(at(30), (201, 1_000));
        assert_eq!(at(0), (0, 10));
    }

    #[test]
    fn spot_config_address_is_the_pda() {
        assert_eq!(SPOT_CONFIG, Pubkey::find_program_address(&[SPOT_CONFIG_SEED], &crate::ENGINE_PROGRAM).0);
    }

    #[test]
    fn ramp_doubles_then_opens() {
        let s = 1_000_000_000u64;
        assert_eq!(ramp_cap(s, 25, 8, 0), None);
        assert_eq!(ramp_cap(s, 25, 8, 1), Some(2_500_000));
        assert_eq!(ramp_cap(s, 25, 8, 2), Some(5_000_000));
        assert_eq!(ramp_cap(s, 25, 8, 8), Some(320_000_000));
        assert_eq!(ramp_cap(s, 25, 8, 9), None);
        assert_eq!(ramp_cap(s, 25, 0, 1), None);
        assert_eq!(ramp_cap(u64::MAX, 10_000, 150, 150), Some(u64::MAX));
        assert_eq!(ramp_cap(u64::MAX, 10_000, 150, 2), Some(u64::MAX));
    }

    #[test]
    fn ramp_in_hundredths_binds_through_16() {
        let s = 1_000_000_000_000_000u64; // 1B tokens of 6 decimals
        let b = RECOMMENDED_BASE_HUNDREDTHS;
        assert_eq!(ramp_cap_in(s, b, 1_000_000, 16, 1), Some(23_000_000_000)); // 0.0023%
        assert_eq!(ramp_cap_in(s, b, 1_000_000, 16, 16), Some(753_664_000_000_000)); // 75.3664%
        assert_eq!(ramp_cap_in(s, b, 1_000_000, 16, 17), None);
        assert!(ramp_cap_in(s, 30, 1_000_000, 16, 16).unwrap() < s);
        assert!(ramp_cap_in(s, 31, 1_000_000, 16, 16).unwrap() > s);
        // whole bps through ramp_cap_in agree with ramp_cap
        for k in 0..=20 {
            assert_eq!(ramp_cap_in(s, 25, 10_000, 16, k), ramp_cap(s, 25, 16, k));
        }
        assert_eq!(ramp_cap_in(u64::MAX, u16::MAX, 1_000_000, 300, 40), Some(u64::MAX));
        assert_eq!(ramp_cap_in(u64::MAX, u16::MAX, 1_000_000, 300, 41), Some(u64::MAX));
        assert_eq!(effective_base(1, 0), (1, false));
        assert_eq!(effective_base(1, 30), (30, true));
        assert_eq!(base_hundredths(&[0u8; GC_LEN]), 0);
    }
}
