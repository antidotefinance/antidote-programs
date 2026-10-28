//! levfee: levpad's launch-fee router (owner decision 2026-10-17).
//!
//! levpad's launch-side income is (a) the DBC partner share of curve trading fees on every launch
//! (plus the partner's share of a completed curve's surplus) and (b) the trading fees of the
//! permanently locked DAMM v2 LP that DBC hands the partner when a coin graduates. This program's
//! PDA ["fee"] is that partner: new launch configs name it as `fee_claimer` (and leftover
//! receiver), so it signs the claims and holds every graduated position NFT.
//!
//! Anyone may crank it, and the rules are fixed in the code:
//!
//! - **Claims** (`claim_dbc`, `claim_dbc_surplus`, `claim_damm`) pull the SOL income into the PDA's
//!   WSOL account and book all of it to the burn side (owner 2026-10-23: no stakers; it was 50%
//!   burn, 50% stakers). Any other WSOL that lands in the account (a donation) is booked the same
//!   way. `pay_stakers` is gone; a staker side left from before is moved to the burn side the
//!   next time income is booked.
//! - **Buy and burn** (`buy_burn_dbc` on the curve, `buy_burn_damm` after graduation): the burn
//!   side buys the platform token on market and burns everything the PDA holds of it. Each buy is
//!   at most `MAX_BUY_LAMPORTS`, at most one per `MIN_SLOTS_BETWEEN_BUYS`, refused when the
//!   venue's spot is more than `MAX_SPOT_PREMIUM_BPS` above the hook ring's TWAP over the last
//!   `TWAP_WINDOW_SLOTS` (refused, too, while the ring's history is shorter than that), and reverted
//!   when the tokens received price the buy more than `MAX_EXEC_PREMIUM_BPS` above that TWAP
//!   (fee and impact included). So a sandwich can only extract a few percent of one capped buy.
//!
//! - **Curve fees into the LP** (Antidote launch economics, 2026-10-23). A launch config whose
//!   coins migrate into a compounding DAMM v2 pool (DBC `migration_fee_option` Customizable and
//!   `migrated_collect_fee_mode` Compounding, read from the config itself) is *LP-bound*: its
//!   partner curve fees are not income. After the coin graduates, `fee_to_lp` claims them (at most
//!   `LP_MAX_CLAIM_LAMPORTS` per call), buys the coin with about half on its DAMM v2 pool (the same
//!   TWAP guards as a buyback), adds both sides to the fee PDA's position in that pool and locks
//!   the added liquidity permanently, in one instruction. While the curve runs they stay
//!   unclaimed in the DBC pool; if the curve is still running `LP_GRACE_SLOTS` after the pool
//!   opened, `claim_dbc` takes them for the buyback.
//!
//! - **Quotes other than SOL** (multi-quote launches, 2026-10-23). A coin quoted in USDC or TSLAx
//!   pays its router fee and its curve fees in that quote, into the fee PDA's associated account
//!   of it. `claim_dbc`, `claim_dbc_surplus` and `fee_to_lp` take such a coin's quote (and its
//!   `QuoteFee` ["quote", mint] as remaining account 0, `init_quote` creates both). The quote
//!   never reaches the 50/50 split as such: `swap_quote_to_sol` (anyone) sells it to the caller
//!   for WSOL at the quote's price from the engine's `QuoteBook` (max of its long and recent TWAPs,
//!   less `QUOTE_SALE_DISCOUNT_BPS` for the caller's trouble), at most `MAX_QUOTE_SALE_LAMPORTS`
//!   per call and one call per `MIN_SLOTS_BETWEEN_QUOTE_SALES` per quote; the WSOL is booked as
//!   income and runs the SOL path unchanged. Buy-and-burn stays SOL. Only quotes the engine allows
//!   have a price, so only those sell. The discount, the per-call cap and the spacing are
//!   `QuoteSaleParams` (owner 2026-10-23, were constants).
//!
//! - **The platform token's own fee** (owner decisions 2026-10-23, "Antidote's 2% fee"). Antidote's
//!   pool charges 2% on its curve and after graduation; Meteora keeps 20% (0.4% of volume) and the
//!   fee PDA gets 1.6% as the curve's partner fee (`claim_dbc`) and, after graduation, as the fees
//!   of its permanently locked position in the OnlyB (SOL-only fee) DAMM v2 pool (`claim_damm`).
//!   Income claimed from the platform token's own pools splits `FeeParams::treasury_bps` (default
//!   half) to the treasury side and the rest to the burn side; every other income stays 100% burn.
//!   The treasury side is WSOL held in the fee PDA's account and counted in `pending_treasury`;
//!   `pay_treasury` (anyone) unwraps it to `FeeParams::treasury` once the admin has set one, and
//!   until then nothing is sent anywhere. The platform token is never LP-bound: `fee_to_lp`
//!   refuses it.
//! - **Buy-and-burn threshold** (same decisions). A buyback runs only once the burn side holds at
//!   least `FeeParams::burn_threshold_lamports` (default 3 SOL) and spends at most
//!   `FeeParams::max_buy_lamports` (default 3 SOL) per call, with the same TWAP and price guards.
//!
//! **Admin (2026-10-23).** One small account, `Params` ["params"], holds the quote-sale
//! parameters and an admin (set once by `init_params`, signed by this program's upgrade
//! authority; handed over with `set_admin` + `accept_admin`). `set_params` changes them within
//! hard caps (`HARD_MAX_*`), at once while the timelock is 0, else `apply_params` (anyone) after
//! it. Nothing else has an admin: there is no way to move the position NFTs, the WSOL or the
//! platform tokens anywhere else, and no IDL account (built with `no-idl`). The team takes
//! nothing. Burns reduce the token's supply, which frees levcap room for later loss receipts
//! (levcap's limit is supply plus owed since 2026-10-23).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::system_instruction;
use anchor_lang::solana_program::hash::hashv;
use anchor_spl::associated_token::{self, get_associated_token_address_with_program_id, AssociatedToken};
use anchor_spl::token::Token;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{self, Mint, TokenInterface};
use tilt_spot::state::{QuoteBook, QuoteEntry};
use levpad_hook::venue::{self, dbc, DAMM_V2_PROGRAM, DBC_POOL_AUTHORITY, DBC_PROGRAM, WSOL};
use levpad_math::oracle::{Obs, Ring};
use levpad_math::{mul_div, price_from_sqrt_q64, Q64};

declare_id!("6MBSd18Ce5pEJ28qXktoxxtJEvopukTBEBCVknvTkhbM");

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

pub const STATE_SEED: &[u8] = b"state";
pub const FEE_SEED: &[u8] = b"fee";
/// ["unwrap"]: `pay_treasury`'s temporary WSOL account (was `pay_stakers`'; it lives only inside
/// the instruction).
pub const UNWRAP_SEED: &[u8] = b"unwrap";
/// ["params"]: the quote-sale parameters and the admin (2026-10-23).
pub const PARAMS_SEED: &[u8] = b"params";

/// The burn side's share of every lamport of income, in bps: all of it (owner 2026-10-23: no
/// stakers; was 5,000).
pub const BURN_SHARE_BPS: u64 = 10_000;
/// Defaults of `FeeParams` (owner decisions 2026-10-23): a buyback runs once the burn side holds 3
/// SOL and spends up to 3 SOL; half of the platform token's own pool fees go to the treasury.
pub const DEFAULT_BURN_THRESHOLD_LAMPORTS: u64 = 3_000_000_000;
pub const DEFAULT_MAX_BUY_LAMPORTS: u64 = 3_000_000_000;
pub const DEFAULT_TREASURY_BPS: u16 = 5_000;
/// Hard caps of `FeeParams`: the treasury never takes more than half of the platform token's pool
/// fees (the owner's split; the admin may lower it), the threshold at most 100 SOL, a single
/// buyback 0.01 to 25 SOL.
pub const HARD_MAX_TREASURY_BPS: u16 = 5_000;
pub const HARD_MAX_BURN_THRESHOLD_LAMPORTS: u64 = 100_000_000_000;
pub const HARD_MIN_MAX_BUY_LAMPORTS: u64 = 10_000_000;
pub const HARD_MAX_MAX_BUY_LAMPORTS: u64 = 25_000_000_000;
/// Slot-denominated values assume the mainnet slot time measured 2026-10-23
/// (getRecentPerformanceSamples): 0.212 s, about 4.7 slots a second, 407,547 a day (they assumed
/// 0.4 s before). The ones that are `FeeParams` / `QuoteSaleParams` are admin-retunable.
pub const SLOTS_PER_DAY: u64 = 407_547;
/// Default slots between two buybacks (`FeeParams::min_buy_gap_slots`, about a minute; was 150).
pub const MIN_SLOTS_BETWEEN_BUYS: u64 = 283;
pub const HARD_MAX_MIN_BUY_GAP_SLOTS: u32 = 432_000;
/// TWAP window over the hook ring, 1,416 slots (about five minutes; was 750). Always the whole window: a buy is
/// refused (`ShortHistory`) while the ring's oldest observation is younger than this (red team F4).
/// The ring holds 240 observations, at most one per slot, so on a token that trades or is poked
/// in nearly every slot it covers less than the window and buybacks wait for a quieter stretch
/// (after graduation the ring is written only by `poke_damm`, which the keeper sends every few
/// slots, so it covers several windows). levfee is immutable, so it refuses rather than shorten the
/// window to whatever history happens to be there.
pub const TWAP_WINDOW_SLOTS: u64 = 1_416;
/// `fee_to_lp`: refuse its coin buy when the venue's spot is more than this above the coin's TWAP.
pub const MAX_SPOT_PREMIUM_BPS: u64 = 200;
/// `fee_to_lp`: revert its coin buy when the average price (lamports in over tokens out, fee and
/// impact included) is more than this above the TWAP.
pub const MAX_EXEC_PREMIUM_BPS: u64 = 500;
/// The buyback's own price guard is `FeeParams::guard_bps` (owner 2026-10-23: off by default, so
/// the threshold buy executes in full every time); at most this.
pub const HARD_MAX_GUARD_BPS: u16 = 5_000;
/// The TWAP checkpoint ["twap"]: a copy of the ring's cumulative price, taken at least
/// `TWAP_WINDOW_SLOTS` apart. A busy token's ring (one observation per slot with a swap, 240 at
/// most) can span fewer than `TWAP_WINDOW_SLOTS`; the cumulative itself never resets, so a
/// checkpoint at least that old still gives the exact TWAP over a window at least that long.
pub const CHECKPOINT_SEED: &[u8] = b"twap";
/// A checkpoint older than this is not used: the window would lag the price too much.
pub const MAX_CHECKPOINT_AGE_SLOTS: u64 = 4 * TWAP_WINDOW_SLOTS;

pub const DAMM_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
pub const DAMM_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");
/// DBC's `__event_authority` PDA.
pub const DBC_EVENT_AUTHORITY: Pubkey = pubkey!("8Ks12pbrD6PXxfty1hVQiE9sc289zgU1zHkvXhrSdriF");

/// DBC `ConfigWithTransferHook` / `PoolConfig` byte offsets (with the discriminator; DBC release
/// 0.2.2 state/config.rs, read back in tests/tests/antidote_lp.rs).
pub mod dbc_config {
    pub const FEE_CLAIMER: usize = 40;
    /// 6 = `MigrationFeeOption::Customizable`.
    pub const MIGRATION_FEE_OPTION: usize = 243;
    /// 2 = `MigratedCollectFeeMode::Compounding`.
    pub const MIGRATED_COLLECT_FEE_MODE: usize = 360;
    pub const MIGRATED_POOL_FEE_BPS: usize = 362;
    pub const MIN_LEN: usize = 1048;
    pub const CUSTOMIZABLE: u8 = 6;
    pub const COMPOUNDING: u8 = 2;
}
/// DBC pool: the partner's unclaimed quote fee.
pub const DBC_PARTNER_QUOTE_FEE: usize = 272;
/// DBC pool: the slot it opened (`activation_point`; launch configs activate by slot).
pub const DBC_ACTIVATION_POINT: usize = 296;
/// DAMM v2 `Pool` byte offsets (cp-amm 0.2.5): collect fee mode (2 = compounding) and the
/// reserves a compounding pool tracks.
pub mod damm_pool {
    pub const COLLECT_FEE_MODE: usize = 484;
    pub const TOKEN_A_AMOUNT: usize = 680;
    pub const TOKEN_B_AMOUNT: usize = 688;
    pub const COMPOUNDING: u8 = 2;
}

/// Default grace (`FeeParams::lp_grace_slots`): an LP-bound coin whose curve is still running this
/// many slots after its pool opened (about 7 days at 0.212 s; was 7 x 216,000, 7 days at 0.4 s
/// and 3.7 days at 0.212 s) gives up its curve fees (and its flip fees) to the buyback
/// (`claim_dbc`, `sweep_flip`; all to the burn side). A coin that graduates later sends whatever
/// is still unclaimed to its LP. The admin may set 1 to 60 days of slots.
pub const LP_GRACE_SLOTS: u64 = 7 * SLOTS_PER_DAY;
pub const HARD_MIN_LP_GRACE_SLOTS: u32 = SLOTS_PER_DAY as u32;
pub const HARD_MAX_LP_GRACE_SLOTS: u32 = 60 * SLOTS_PER_DAY as u32;
/// ["flip", mint]: the owner of a coin's flip-fee accrual (tilt-spot's flip fee, owner
/// 2026-10-23), its associated account of the coin's quote. `fee_to_lp` adds it to the coin's
/// LP; `sweep_flip` sends it to the burn side where the curve fees would go there too.
pub const FLIP_SEED: &[u8] = b"flip";
/// Largest claim one `fee_to_lp` puts into the LP (about half of it is swapped).
pub const LP_MAX_CLAIM_LAMPORTS: u64 = 2_000_000_000;

/// ["quote", mint]: a non-SOL quote's book here (`init_quote`).
pub const QUOTE_FEE_SEED: &[u8] = b"quote";
/// Defaults of `QuoteSaleParams` (were constants until 2026-10-23). `swap_quote_to_sol` pays the
/// caller this much below the quote's price (0.5%), for the swap it makes elsewhere.
pub const QUOTE_SALE_DISCOUNT_BPS: u64 = 50;
/// Largest single quote sale, in lamports paid.
pub const MAX_QUOTE_SALE_LAMPORTS: u64 = 5_000_000_000;
/// Slots between two sales of one quote (about a minute at 0.212 s; was 150).
pub const MIN_SLOTS_BETWEEN_QUOTE_SALES: u64 = 283;
/// Hard caps of `QuoteSaleParams` and the timelock.
pub const HARD_MAX_QUOTE_SALE_DISCOUNT_BPS: u16 = 300;
pub const HARD_MAX_QUOTE_SALE_LAMPORTS: u64 = 50_000_000_000;
pub const HARD_MIN_QUOTE_SALE_SLOTS: u32 = 10;
pub const HARD_MAX_QUOTE_SALE_SLOTS: u32 = 432_000;
pub const HARD_MAX_TIMELOCK_SECS: u32 = 30 * 86_400;

/// What `swap_quote_to_sol` pays and how often (admin-set within the hard caps).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct QuoteSaleParams {
    /// Below the quote's price, in bps (at most `HARD_MAX_QUOTE_SALE_DISCOUNT_BPS`).
    pub discount_bps: u16,
    /// Largest single sale, lamports paid (1 to `HARD_MAX_QUOTE_SALE_LAMPORTS`).
    pub max_lamports: u64,
    /// Slots between two sales of one quote (`HARD_MIN_QUOTE_SALE_SLOTS` to `HARD_MAX_QUOTE_SALE_SLOTS`).
    pub min_slots: u32,
    /// Delay before a `set_params` applies (at most `HARD_MAX_TIMELOCK_SECS`; 0 = at once).
    pub timelock_secs: u32,
}

impl QuoteSaleParams {
    pub const DEFAULT: QuoteSaleParams =
        QuoteSaleParams { discount_bps: QUOTE_SALE_DISCOUNT_BPS as u16, max_lamports: MAX_QUOTE_SALE_LAMPORTS, min_slots: MIN_SLOTS_BETWEEN_QUOTE_SALES as u32, timelock_secs: 0 };

    pub fn validate(&self) -> Result<()> {
        require!(self.discount_bps <= HARD_MAX_QUOTE_SALE_DISCOUNT_BPS, FeeError::BadParam);
        require!(self.max_lamports > 0 && self.max_lamports <= HARD_MAX_QUOTE_SALE_LAMPORTS, FeeError::BadParam);
        require!((HARD_MIN_QUOTE_SALE_SLOTS..=HARD_MAX_QUOTE_SALE_SLOTS).contains(&self.min_slots), FeeError::BadParam);
        require!(self.timelock_secs <= HARD_MAX_TIMELOCK_SECS, FeeError::BadParam);
        Ok(())
    }
}

/// The platform token's own fee split and the buyback's trigger (owner decisions 2026-10-23;
/// admin-set within the hard caps, behind the same timelock as `QuoteSaleParams`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct FeeParams {
    /// Where `pay_treasury` sends the treasury side (SOL). Default (all zero) = not set: the
    /// treasury side accumulates in the fee PDA's WSOL account and nothing is sent anywhere.
    pub treasury: Pubkey,
    /// Share of income from the platform token's own pools (curve partner fee, graduated LP fees)
    /// booked to the treasury side, bps (at most `HARD_MAX_TREASURY_BPS`); the rest to the burn
    /// side. Other income is 100% burn.
    pub treasury_bps: u16,
    /// A buyback runs only while the burn side holds at least this much (0 to
    /// `HARD_MAX_BURN_THRESHOLD_LAMPORTS`; 0 = any time).
    pub burn_threshold_lamports: u64,
    /// Largest single buyback (`HARD_MIN_MAX_BUY_LAMPORTS` to `HARD_MAX_MAX_BUY_LAMPORTS`).
    pub max_buy_lamports: u64,
    /// The buyback's price guard, bps over the ring TWAP: spot at most that far above it, and the
    /// price paid (fee and impact included) too. 0 = off (owner 2026-10-23, the default): the buy
    /// executes in full whatever the price, no TWAP is read, and nothing but the threshold, the
    /// cap and the gap can stop it. Off, a caller can sandwich the buy inside its own
    /// transaction: set `buyback_caller` (SPEC.md "Buyback").
    pub guard_bps: u16,
    /// Who may call `buy_burn_dbc` / `buy_burn_damm` (it signs as `caller`); default (all zero) =
    /// anyone.
    pub buyback_caller: Pubkey,
    /// Slots between two buybacks (1 to `HARD_MAX_MIN_BUY_GAP_SLOTS`).
    pub min_buy_gap_slots: u32,
    /// An LP-bound coin's grace (`HARD_MIN_LP_GRACE_SLOTS` to `HARD_MAX_LP_GRACE_SLOTS`).
    pub lp_grace_slots: u32,
}

impl FeeParams {
    pub const DEFAULT: FeeParams = FeeParams {
        treasury: Pubkey::new_from_array([0; 32]),
        treasury_bps: DEFAULT_TREASURY_BPS,
        burn_threshold_lamports: DEFAULT_BURN_THRESHOLD_LAMPORTS,
        max_buy_lamports: DEFAULT_MAX_BUY_LAMPORTS,
        guard_bps: 0,
        buyback_caller: Pubkey::new_from_array([0; 32]),
        min_buy_gap_slots: MIN_SLOTS_BETWEEN_BUYS as u32,
        lp_grace_slots: LP_GRACE_SLOTS as u32,
    };

    pub fn validate(&self) -> Result<()> {
        require!(self.treasury_bps <= HARD_MAX_TREASURY_BPS, FeeError::BadParam);
        require!(self.burn_threshold_lamports <= HARD_MAX_BURN_THRESHOLD_LAMPORTS, FeeError::BadParam);
        require!((HARD_MIN_MAX_BUY_LAMPORTS..=HARD_MAX_MAX_BUY_LAMPORTS).contains(&self.max_buy_lamports), FeeError::BadParam);
        require!(self.guard_bps <= HARD_MAX_GUARD_BPS, FeeError::BadParam);
        require!((1..=HARD_MAX_MIN_BUY_GAP_SLOTS).contains(&self.min_buy_gap_slots), FeeError::BadParam);
        require!((HARD_MIN_LP_GRACE_SLOTS..=HARD_MAX_LP_GRACE_SLOTS).contains(&self.lp_grace_slots), FeeError::BadParam);
        // the treasury is a destination outside this program's own accounts
        let own = [fee_authority().0, state_address().0, unwrap_address().0, Pubkey::find_program_address(&[PARAMS_SEED], &crate::ID).0, crate::ID];
        require!(!own.contains(&self.treasury), FeeError::BadParam);
        Ok(())
    }
}

/// ["params"]: levfee's only admin-held state (2026-10-23).
#[account]
#[derive(InitSpace)]
pub struct Params {
    pub admin: Pubkey,
    /// `set_admin` names it; it takes over with `accept_admin`.
    pub pending_admin: Pubkey,
    pub bump: u8,
    pub quote_sale: QuoteSaleParams,
    /// A `set_params` waiting out the timelock; eta 0 = none.
    pub pending: QuoteSaleParams,
    pub pending_eta: i64,
    /// The platform token's fee split and the buyback trigger (2026-10-23), and a `set_fee_params`
    /// waiting out the timelock (eta 0 = none).
    pub fee: FeeParams,
    pub pending_fee: FeeParams,
    pub pending_fee_eta: i64,
    pub reserved: [u8; 32],
}

#[event]
pub struct ParamsSet {
    pub eta: i64,
    pub params: QuoteSaleParams,
}

#[event]
pub struct FeeParamsSet {
    pub eta: i64,
    pub params: FeeParams,
}

/// `pay_treasury`: the treasury side sent to the treasury, in SOL.
#[event]
pub struct TreasuryPaid {
    pub treasury: Pubkey,
    pub lamports: u64,
    pub total: u64,
}

/// Income sources (`Claimed::source`).
pub const SRC_DBC_FEE: u8 = 0;
pub const SRC_DBC_SURPLUS: u8 = 1;
pub const SRC_DAMM_LP: u8 = 2;
pub const SRC_OTHER: u8 = 3;

pub fn state_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[STATE_SEED], &crate::ID)
}
pub fn fee_authority() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[FEE_SEED], &crate::ID)
}
pub fn unwrap_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[UNWRAP_SEED], &crate::ID)
}

/// The one state account, PDA ["state"]. Every field but the counters is fixed at `initialize`.
#[account]
#[derive(InitSpace)]
pub struct FeeState {
    /// The platform token (the engine's levcap token when `initialize` ran).
    pub platform_mint: Pubkey,
    /// The fee PDA's WSOL account (classic Token ATA) and platform-token account (Token-2022 ATA).
    pub wsol_ata: Pubkey,
    pub burn_ata: Pubkey,
    pub bump: u8,
    pub fee_bump: u8,
    pub unwrap_bump: u8,
    /// WSOL in `wsol_ata` waiting for a buyback, and waiting for `pay_treasury` (the treasury side,
    /// 2026-10-23: the old `pending_stakers` bytes; `init_params` moved a staker side left from
    /// before to the burn side).
    pub pending_burn: u64,
    pub pending_treasury: u64,
    /// Lifetime income by source, lamports.
    pub income_dbc: u64,
    pub income_surplus: u64,
    pub income_damm: u64,
    pub income_other: u64,
    /// Lifetime lamports swapped for the platform token, atoms burned, lamports paid to the
    /// treasury (the old `to_stakers` and `staker_payments` bytes, zeroed by `init_params`).
    pub burn_spent: u64,
    pub burned: u64,
    pub to_treasury: u64,
    pub buys: u64,
    pub treasury_payments: u64,
    pub last_buy_slot: u64,
}

/// Two cumulative-price checkpoints of the platform token's ring (the newer is `newest`).
#[account]
#[derive(InitSpace)]
pub struct TwapCheckpoint {
    pub slots: [u64; 2],
    pub cums: [u128; 2],
    pub newest: u8,
    pub bump: u8,
}

#[event]
pub struct Checkpointed {
    pub slot: u64,
    pub cum: u128,
}

impl FeeState {
    pub fn pending(&self) -> u64 {
        self.pending_burn.saturating_add(self.pending_treasury)
    }
}

#[event]
pub struct Claimed {
    pub source: u8,
    pub amount: u64,
    pub to_burn: u64,
    pub to_treasury: u64,
}

#[event]
pub struct BoughtAndBurned {
    pub venue: u8,
    pub lamports_in: u64,
    pub tokens_out: u64,
    /// Everything the fee PDA held of the platform token, burned (tokens_out plus any sent to it).
    pub burned: u64,
    pub twap_q64: u128,
    pub spot_q64: u128,
    pub burned_total: u64,
}

/// A coin's flip fees (tilt-spot) left its ["flip", mint] accrual: into its LP (`fee_to_lp`) or
/// to the burn side (`sweep_flip`).
#[event]
pub struct FlipSwept {
    pub mint: Pubkey,
    pub amount: u64,
    pub to_lp: bool,
}

/// `fee_to_lp`: a graduated coin's curve fees went into its locked LP.
#[event]
pub struct LpAdded {
    pub mint: Pubkey,
    pub damm_pool: Pubkey,
    /// Partner curve fees claimed from DBC (lamports).
    pub claimed: u64,
    /// Lamports swapped for the coin, and the coin atoms that came out.
    pub swapped: u64,
    pub bought: u64,
    /// What went into the pool, and the liquidity added and permanently locked.
    pub sol_added: u64,
    pub tokens_added: u64,
    pub liquidity: u128,
    /// Lamports of the claim left over (rounding), booked to the burn side.
    pub leftover: u64,
}

/// A non-SOL quote's account here: what was claimed into the fee PDA's account of it, the part
/// that may only fund the buyback, and what `swap_quote_to_sol` sold.
#[account]
#[derive(InitSpace)]
pub struct QuoteFee {
    pub mint: Pubkey,
    pub bump: u8,
    /// Atoms of the quote waiting to be sold whose SOL goes to the burn side only.
    pub burn_only: u64,
    /// Lifetime atoms claimed from DBC into the account (router fees arrive by plain transfer).
    pub claimed: u64,
    pub sold: u64,
    pub lamports_in: u64,
    pub sales: u64,
    pub last_sale_slot: u64,
}

#[event]
pub struct QuoteSold {
    pub mint: Pubkey,
    pub buyer: Pubkey,
    pub atoms: u64,
    pub lamports: u64,
    /// The engine's price (Q64.64 lamports per atom) before the discount.
    pub price: u128,
    pub to_burn_only: u64,
}

#[error_code]
pub enum FeeError {
    #[msg("the engine's platform token is not a levcap token")]
    NotLevcapToken,
    #[msg("wrong account")]
    WrongAccount,
    #[msg("nothing to do")]
    Nothing,
    #[msg("buy over the per-call cap or over what the burn side holds")]
    OverCap,
    #[msg("too soon after the last buyback")]
    TooSoon,
    #[msg("the venue is not live for this phase (curve still running, or migrated)")]
    WrongPhase,
    #[msg("the observation ring has too little history for a TWAP")]
    ShortHistory,
    #[msg("spot is too far above the TWAP")]
    SpotAboveTwap,
    #[msg("the buy's price is too far above the TWAP")]
    PriceAboveTwap,
    #[msg("math overflow")]
    Math,
    #[msg("this config's curve fees go to the coin's LP: fee_to_lp after graduation, claim_dbc only after the grace period")]
    LpBound,
    #[msg("not an LP-bound config, or the coin has not graduated into its compounding pool")]
    NotLpBound,
    #[msg("the engine does not allow this quote, or it has no fresh price")]
    NoQuotePrice,
    #[msg("signer is not the admin")]
    NotAdmin,
    #[msg("a parameter is outside its hard cap")]
    BadParam,
    #[msg("no change is pending, or its time has not come")]
    NotPending,
    #[msg("the burn side has not reached the buyback threshold")]
    BelowThreshold,
    #[msg("no treasury address is set: the treasury side stays here")]
    NoTreasury,
    #[msg("only the admin-set buyback caller may buy back")]
    NotCaller,
    #[msg("nothing in the coin's flip-fee account, or its fees go to its LP (fee_to_lp)")]
    NoFlip,
}

fn disc(name: &str) -> [u8; 8] {
    hashv(&[format!("global:{name}").as_bytes()]).to_bytes()[..8].try_into().unwrap()
}

/// A token account's amount (Token or Token-2022, offset 64), after checking its owner program.
fn token_amount(ai: &AccountInfo) -> Result<u64> {
    require!(*ai.owner == anchor_spl::token::ID || *ai.owner == anchor_spl::token_2022::ID, FeeError::WrongAccount);
    let d = ai.try_borrow_data()?;
    require!(d.len() >= 165, FeeError::WrongAccount);
    Ok(u64::from_le_bytes(d[64..72].try_into().unwrap()))
}

/// The fee PDA's receiving account for a coin's quote: the state's WSOL account for SOL, else
/// the fee PDA's associated account of the quote under its program (which owns the mint).
fn check_receiver(s: &FeeState, recv: &AccountInfo, quote: &AccountInfo, quote_program: &Pubkey) -> Result<()> {
    if quote.key() == WSOL {
        require_keys_eq!(*quote_program, anchor_spl::token::ID, FeeError::WrongAccount);
        require_keys_eq!(recv.key(), s.wsol_ata, FeeError::WrongAccount);
    } else {
        require_keys_eq!(*quote.owner, *quote_program, FeeError::WrongAccount);
        require_keys_eq!(recv.key(), get_associated_token_address_with_program_id(&fee_authority().0, &quote.key(), quote_program), FeeError::WrongAccount);
    }
    Ok(())
}

/// Book `amount` atoms of a non-SOL quote claimed into the fee PDA's account in the quote's
/// `QuoteFee` (remaining account 0); `burn_only` when its SOL may only fund the buyback.
fn credit_quote(r: &[AccountInfo], quote: &Pubkey, amount: u64, burn_only: bool) -> Result<()> {
    let ai = r.first().ok_or(FeeError::WrongAccount)?;
    require_keys_eq!(ai.key(), Pubkey::find_program_address(&[QUOTE_FEE_SEED, quote.as_ref()], &crate::ID).0, FeeError::WrongAccount);
    require!(*ai.owner == crate::ID && ai.is_writable, FeeError::WrongAccount);
    let mut q = QuoteFee::try_deserialize(&mut &ai.try_borrow_data()?[..])?;
    q.claimed = q.claimed.saturating_add(amount);
    if burn_only {
        q.burn_only = q.burn_only.saturating_add(amount);
    }
    let mut d = ai.try_borrow_mut_data()?;
    q.try_serialize(&mut &mut d[..])
}

/// The engine's entry for an allowed quote, read in place from its Config.
fn engine_quote(cfg: &AccountInfo, mint: &Pubkey) -> Result<QuoteEntry> {
    use tilt_spot::state::{config_offsets, QUOTES};
    require_keys_eq!(*cfg.owner, tilt_spot::ID, FeeError::WrongAccount);
    let d = cfg.try_borrow_data()?;
    let n = QuoteEntry::INIT_SPACE;
    require!(d.len() >= config_offsets::QUOTES + QUOTES * n, FeeError::NoQuotePrice);
    for i in 0..QUOTES {
        let o = config_offsets::QUOTES + i * n;
        let e = QuoteEntry::deserialize(&mut &d[o..o + n])?;
        if e.mint == *mint && *mint != Pubkey::default() {
            return Ok(e);
        }
    }
    err!(FeeError::NoQuotePrice)
}

/// Book `amount` of new income: `treasury_bps` of it (floor) to the treasury side, the rest to the
/// burn side. Only the platform token's own pool fees pass a non-zero share; every other income
/// is 100% burn (`BURN_SHARE_BPS`, owner 2026-10-23).
fn book(s: &mut FeeState, source: u8, amount: u64, treasury_bps: u16) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let to_treasury = (amount as u128 * treasury_bps.min(HARD_MAX_TREASURY_BPS) as u128 / 10_000) as u64;
    let to_burn = amount - to_treasury;
    debug_assert!(BURN_SHARE_BPS == 10_000);
    s.pending_burn = s.pending_burn.checked_add(to_burn).ok_or(FeeError::Math)?;
    s.pending_treasury = s.pending_treasury.checked_add(to_treasury).ok_or(FeeError::Math)?;
    let src = match source {
        SRC_DBC_FEE => &mut s.income_dbc,
        SRC_DBC_SURPLUS => &mut s.income_surplus,
        SRC_DAMM_LP => &mut s.income_damm,
        _ => &mut s.income_other,
    };
    *src = src.checked_add(amount).ok_or(FeeError::Math)?;
    emit!(Claimed { source, amount, to_burn, to_treasury });
    Ok(())
}

/// Book `amount` of new income to the burn side only (an LP-bound coin's curve fees after the
/// grace period, and `fee_to_lp`'s rounding leftover). Since 2026-10-23 `book` does the same.
fn book_burn(s: &mut FeeState, source: u8, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    s.pending_burn = s.pending_burn.checked_add(amount).ok_or(FeeError::Math)?;
    let src = if source == SRC_DBC_FEE { &mut s.income_dbc } else { &mut s.income_other };
    *src = src.checked_add(amount).ok_or(FeeError::Math)?;
    emit!(Claimed { source, amount, to_burn: amount, to_treasury: 0 });
    Ok(())
}

/// After a claim: `got` lamports came from `source`; anything else unaccounted in the WSOL account
/// (a donation) is booked as `SRC_OTHER` (all burn).
fn absorb(s: &mut FeeState, wsol: &AccountInfo, source: u8, got: u64) -> Result<()> {
    absorb_as(s, wsol, source, got, false, 0)
}

/// `absorb` with `got` booked burn-only, or split `treasury_bps` to the treasury side (the
/// platform token's own pool fees).
fn absorb_as(s: &mut FeeState, wsol: &AccountInfo, source: u8, got: u64, burn_only: bool, treasury_bps: u16) -> Result<()> {
    let bal = token_amount(wsol)?;
    let unaccounted = bal.saturating_sub(s.pending());
    let got = got.min(unaccounted);
    if burn_only {
        book_burn(s, source, got)?;
    } else {
        book(s, source, got, treasury_bps)?;
    }
    book(s, SRC_OTHER, unaccounted - got, 0)
}

/// A DBC launch config whose partner is the fee PDA and whose coins migrate into a compounding
/// DAMM v2 pool: its curve fees belong to each coin's LP (see the module docs). Read from the
/// config, which nobody can change after `create_config`.
pub fn lp_bound(owner: &Pubkey, d: &[u8], fee_pda: &Pubkey) -> bool {
    use dbc_config as c;
    *owner == DBC_PROGRAM
        && d.len() >= c::MIN_LEN
        && d[c::FEE_CLAIMER..c::FEE_CLAIMER + 32] == fee_pda.to_bytes()
        && d[c::MIGRATION_FEE_OPTION] == c::CUSTOMIZABLE
        && d[c::MIGRATED_COLLECT_FEE_MODE] == c::COMPOUNDING
}

/// Lamports to swap for the coin out of `got` so what is left pairs with what comes out at the
/// pool's ratio afterwards (largest x with (got - x) x a' >= dy x b', a constant-product pool with
/// reserves `a` (coin) and `b` (SOL) and `fee_bps` taken from the input). The leftover is the
/// rounding and the protocol's cut of the fee.
pub fn lp_swap_amount(got: u64, a: u64, b: u64, fee_bps: u64) -> u64 {
    let (a, b, got) = (a as u128, b as u128, got as u128);
    let ok = |x: u128| -> bool {
        let xn = x * (10_000 - fee_bps.min(10_000) as u128) / 10_000;
        let dy = if b + xn == 0 { 0 } else { a * xn / (b + xn) };
        let lhs = (got - x).saturating_mul(a.saturating_sub(dy));
        let rhs = dy.saturating_mul(b + x);
        lhs >= rhs
    };
    let (mut lo, mut hi) = (0u128, got);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo as u64
}

pub fn flip_authority(mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[FLIP_SEED, mint.as_ref()], &crate::ID)
}

/// Move everything in `mint`'s flip-fee accrual (found among `r` with its authority PDA) into the
/// fee PDA's account `to` of the quote. Returns the atoms moved (0 without the accounts, or when
/// the accrual does not exist or is empty).
fn sweep_flip_into<'info>(r: &[AccountInfo<'info>], mint: &Pubkey, quote: &AccountInfo<'info>, qprog: &AccountInfo<'info>, to: &AccountInfo<'info>) -> Result<u64> {
    let (auth, bump) = flip_authority(mint);
    let acc_key = get_associated_token_address_with_program_id(&auth, &quote.key(), &qprog.key());
    let (Some(ai), Some(acc)) = (r.iter().find(|x| x.key() == auth), r.iter().find(|x| x.key() == acc_key)) else { return Ok(0) };
    if *acc.owner != qprog.key() || acc.data_len() < 165 {
        return Ok(0);
    }
    let amount = token_amount(acc)?;
    if amount == 0 {
        return Ok(0);
    }
    require!(acc.is_writable, FeeError::WrongAccount);
    let decimals = {
        let d = quote.try_borrow_data()?;
        require!(d.len() >= 82, FeeError::WrongAccount);
        d[44]
    };
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            qprog.clone(),
            token_interface::TransferChecked { from: acc.clone(), mint: quote.clone(), to: to.clone(), authority: ai.clone() },
            &[&[FLIP_SEED, mint.as_ref(), &[bump]]],
        ),
        amount,
        decimals,
    )?;
    Ok(amount)
}

/// The ring's cumulative price at `now`: its newest observation extended at its price (a price set
/// in this slot has held for 0 slots). None for an empty ring.
fn ring_cum_now(d: &[u8], now: u64) -> Option<u128> {
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let (head, len) = (u32_at(40) as usize, u32_at(44));
    if len == 0 || head >= levpad_hook::ring::CAP {
        return None;
    }
    let o = levpad_hook::ring::HEADER + head * levpad_hook::ring::OBS_LEN;
    let slot = u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let cum = u128::from_le_bytes(d[o + 8..o + 24].try_into().unwrap());
    let price = u128::from_le_bytes(d[o + 24..o + 40].try_into().unwrap());
    Some(cum.wrapping_add(price.wrapping_mul(now.saturating_sub(slot) as u128)))
}

/// The ring checked as the platform token's.
fn check_ring(ring: &AccountInfo, mint: &Pubkey) -> Result<()> {
    require_keys_eq!(ring.key(), levpad_hook::ring_address(mint).0, FeeError::WrongAccount);
    require_keys_eq!(*ring.owner, levpad_hook::ID, FeeError::WrongAccount);
    require!(levpad_hook::ring::is_ring(&ring.try_borrow_data()?), FeeError::WrongAccount);
    Ok(())
}

/// TWAP from the ring when it spans `TWAP_WINDOW_SLOTS`, else from the checkpoint (when passed):
/// the newest checkpoint `TWAP_WINDOW_SLOTS` to `MAX_CHECKPOINT_AGE_SLOTS` old, to the ring's
/// cumulative now. Either way the window is at least `TWAP_WINDOW_SLOTS` (red team F4).
fn buy_twap(ring: &AccountInfo, checkpoint: Option<&AccountInfo>, mint: &Pubkey, now: u64) -> Result<u128> {
    let short = match ring_twap(ring, mint, now) {
        Ok(t) => return Ok(t),
        Err(e) => e,
    };
    let Some(cp) = checkpoint else { return Err(short) };
    require_keys_eq!(cp.key(), Pubkey::find_program_address(&[CHECKPOINT_SEED], &crate::ID).0, FeeError::WrongAccount);
    // not created yet (the keeper's first `checkpoint` makes it): no window, as without one
    if *cp.owner != crate::ID {
        return Err(short);
    }
    let c = TwapCheckpoint::try_deserialize(&mut &cp.try_borrow_data()?[..])?;
    let cum = ring_cum_now(&ring.try_borrow_data()?, now).ok_or(FeeError::ShortHistory)?;
    for i in [c.newest as usize, 1 - c.newest as usize] {
        let age = now.saturating_sub(c.slots[i]);
        if c.slots[i] == 0 || !(TWAP_WINDOW_SLOTS..=MAX_CHECKPOINT_AGE_SLOTS).contains(&age) {
            continue;
        }
        let d = cum.wrapping_sub(c.cums[i]);
        // the cumulative went backwards (a restarted ring): no price from this checkpoint
        if d >= 1u128 << 127 {
            continue;
        }
        let twap = d / age as u128;
        require!(twap > 0, FeeError::ShortHistory);
        return Ok(twap);
    }
    Err(error!(FeeError::ShortHistory))
}

/// TWAP of the hook ring over exactly `TWAP_WINDOW_SLOTS`, Q64.64 lamports per base atom.
/// `ShortHistory` while the ring does not reach back that far.
fn ring_twap(ring: &AccountInfo, mint: &Pubkey, now: u64) -> Result<u128> {
    require_keys_eq!(ring.key(), levpad_hook::ring_address(mint).0, FeeError::WrongAccount);
    require_keys_eq!(*ring.owner, levpad_hook::ID, FeeError::WrongAccount);
    let d = ring.try_borrow_data()?;
    require!(levpad_hook::ring::is_ring(&d), FeeError::WrongAccount);
    let cap = levpad_hook::ring::CAP;
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let mut obs: Vec<Obs> = (0..cap)
        .map(|i| {
            let o = levpad_hook::ring::HEADER + i * levpad_hook::ring::OBS_LEN;
            Obs {
                slot: u64::from_le_bytes(d[o..o + 8].try_into().unwrap()),
                cum: u128::from_le_bytes(d[o + 8..o + 24].try_into().unwrap()),
                price: u128::from_le_bytes(d[o + 24..o + 40].try_into().unwrap()),
            }
        })
        .collect();
    let (mut head, mut len) = (u32_at(40), u32_at(44));
    require!(len > 0 && (head as usize) < cap && (len as usize) <= cap, FeeError::ShortHistory);
    let oldest = if (len as usize) < cap { obs[0].slot } else { obs[(head as usize + 1) % cap].slot };
    require!(now.saturating_sub(oldest) >= TWAP_WINDOW_SLOTS, FeeError::ShortHistory);
    let r = Ring { obs: &mut obs, head: &mut head, len: &mut len };
    let twap = r.twap(now, TWAP_WINDOW_SLOTS).map_err(|_| error!(FeeError::ShortHistory))?;
    require!(twap > 0, FeeError::ShortHistory);
    Ok(twap)
}

/// Who may buy back: anyone, or the admin-set `buyback_caller` signing as `caller`.
fn check_caller(p: &FeeParams, caller: Option<&Signer>) -> Result<()> {
    if p.buyback_caller != Pubkey::default() {
        require!(caller.map(|c| c.key()) == Some(p.buyback_caller), FeeError::NotCaller);
    }
    Ok(())
}

/// The buyback's TWAP: read only while the guard is on (`guard_bps` > 0); 0 when off.
fn guard_twap(p: &FeeParams, ring: &AccountInfo, checkpoint: Option<&AccountInfo>, mint: &Pubkey, slot: u64) -> Result<u128> {
    if p.guard_bps == 0 {
        return Ok(0);
    }
    buy_twap(ring, checkpoint, mint, slot)
}

/// The pre-buy checks shared by both venues; returns the guard's minimum output (1 with the guard
/// off). A buyback runs only while the burn side holds at least `p.burn_threshold_lamports`,
/// spends at most `p.max_buy_lamports`, and `p.min_buy_gap_slots` after the last one; with the
/// guard on, only while spot is at most `guard_bps` over the TWAP.
fn pre_buy(s: &FeeState, p: &FeeParams, amount_in: u64, spot: u128, twap: u128, slot: u64) -> Result<u64> {
    require!(amount_in > 0, FeeError::Nothing);
    require!(s.pending_burn >= p.burn_threshold_lamports, FeeError::BelowThreshold);
    require!(amount_in <= p.max_buy_lamports && amount_in <= s.pending_burn, FeeError::OverCap);
    require!(s.buys == 0 || slot >= s.last_buy_slot.saturating_add(p.min_buy_gap_slots.max(1) as u64), FeeError::TooSoon);
    if p.guard_bps == 0 {
        return Ok(1);
    }
    let g = p.guard_bps.min(HARD_MAX_GUARD_BPS) as u128;
    // spot <= twap x (1 + guard)
    require!(spot.checked_mul(10_000).ok_or(FeeError::Math)? <= twap.checked_mul(10_000 + g).ok_or(FeeError::Math)?, FeeError::SpotAboveTwap);
    let limit = mul_div(twap, 10_000 + g, 10_000).map_err(|_| error!(FeeError::Math))?;
    // tokens out must be at least amount_in / limit (prices are Q64.64 lamports per atom)
    let min_out = mul_div(amount_in as u128, Q64, limit).map_err(|_| error!(FeeError::Math))?;
    Ok(u64::try_from(min_out).map_err(|_| FeeError::Math)?.max(1))
}

#[program]
pub mod levfee {
    use super::*;

    /// Anyone, once: creates the state, funds the fee PDA's float (a token account's rent, kept
    /// from when `pay_stakers` used it, plus its own minimum) and its WSOL and platform-token accounts. The
    /// platform token is read from the engine's config, which must already hold its levcap token
    /// (`set_platform_token`); after a hot-swap `sync_platform_token` follows it.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        {
            // the engine config's platform mint, read in place (fixed offset from the engine's own
            // layout; deserializing the whole Config would grow this binary). The engine only
            // accepts a levcap token there (`set_platform_token`), once.
            use tilt_spot::state::{Config, Params};
            let cfg = &ctx.accounts.engine_config;
            require_keys_eq!(*cfg.owner, tilt_spot::ID, FeeError::WrongAccount);
            let d = cfg.try_borrow_data()?;
            require!(d.len() >= 8 + Config::INIT_SPACE && d[..8] == *Config::DISCRIMINATOR, FeeError::WrongAccount);
            // admin, pending admin, paused + three bumps, params, pending eta, pending params
            let mint_at = 8 + 32 + 32 + 4 + Params::INIT_SPACE + 8 + Params::INIT_SPACE;
            require!(d[mint_at..mint_at + 32] == ctx.accounts.platform_mint.key().to_bytes(), FeeError::NotLevcapToken);
        }
        let fee = ctx.accounts.fee_authority.key();
        let wsol_ata = get_associated_token_address_with_program_id(&fee, &WSOL, &anchor_spl::token::ID);
        let burn_ata = get_associated_token_address_with_program_id(&fee, &ctx.accounts.platform_mint.key(), &anchor_spl::token_2022::ID);
        require_keys_eq!(ctx.accounts.wsol_ata.key(), wsol_ata, FeeError::WrongAccount);
        require_keys_eq!(ctx.accounts.burn_ata.key(), burn_ata, FeeError::WrongAccount);
        for (ata, mint, tp) in [
            (&ctx.accounts.wsol_ata, ctx.accounts.wsol_mint.to_account_info(), ctx.accounts.token_program.to_account_info()),
            (&ctx.accounts.burn_ata, ctx.accounts.platform_mint.to_account_info(), ctx.accounts.token_2022_program.to_account_info()),
        ] {
            associated_token::create_idempotent(CpiContext::new(
                ctx.accounts.associated_token_program.to_account_info(),
                associated_token::Create {
                    payer: ctx.accounts.payer.to_account_info(),
                    associated_token: ata.to_account_info(),
                    authority: ctx.accounts.fee_authority.to_account_info(),
                    mint,
                    system_program: ctx.accounts.system_program.to_account_info(),
                    token_program: tp,
                },
            ))?;
        }
        let rent = Rent::get()?;
        let float = rent.minimum_balance(165) + rent.minimum_balance(0);
        let have = ctx.accounts.fee_authority.lamports();
        if have < float {
            anchor_lang::system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    anchor_lang::system_program::Transfer { from: ctx.accounts.payer.to_account_info(), to: ctx.accounts.fee_authority.to_account_info() },
                ),
                float - have,
            )?;
        }
        let s = &mut ctx.accounts.state;
        s.platform_mint = ctx.accounts.platform_mint.key();
        s.wsol_ata = wsol_ata;
        s.burn_ata = burn_ata;
        s.bump = ctx.bumps.state;
        s.fee_bump = ctx.bumps.fee_authority;
        s.unwrap_bump = unwrap_address().1;
        Ok(())
    }

    /// Anyone: claim the partner's share of a DBC pool's curve fees (SOL) into the fee PDA and
    /// book it. The pool's config must name the fee PDA as `fee_claimer` (DBC checks). The
    /// platform token's own curve fee splits `params.fee.treasury_bps` to the treasury side; any
    /// other coin's goes to the burn side.
    ///
    /// An LP-bound config's fees (module docs) are claimed here only while the coin's curve is
    /// still running `LP_GRACE_SLOTS` after its pool opened, and then go to the burn side only.
    pub fn claim_dbc<'info>(ctx: Context<'_, '_, '_, 'info, ClaimDbc<'info>>) -> Result<()> {
        let a = &ctx.accounts;
        let quote = a.quote_mint.key();
        let qprog = a.token_program.key();
        check_receiver(&a.state, &a.wsol_ata, &a.quote_mint, &qprog)?;
        let lp = lp_bound(a.dbc_config.owner, &a.dbc_config.try_borrow_data()?, &a.fee_authority.key());
        if lp {
            let d = a.dbc_pool.try_borrow_data()?;
            require!(*a.dbc_pool.owner == DBC_PROGRAM && d.len() >= dbc::LEN && d[dbc::CONFIG..dbc::CONFIG + 32] == a.dbc_config.key().to_bytes(), FeeError::WrongAccount);
            let opened = venue::u64_at(&d, DBC_ACTIVATION_POINT);
            let running = d[dbc::IS_MIGRATED] == 0 && d[dbc::MIGRATION_PROGRESS] == 0;
            require!(running && Clock::get()?.slot >= opened.saturating_add(a.params.fee.lp_grace_slots as u64), FeeError::LpBound);
        }
        let before = token_amount(&a.wsol_ata)?;
        let mut data = disc("claim_trading_fee2").to_vec();
        data.extend_from_slice(&0u64.to_le_bytes()); // max base: curve fees are quote only
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes()); // TransferHookAccountsInfo { slices: [] }
        let ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.dbc_pool.key(), false),
                AccountMeta::new(a.burn_ata.key(), false), // base receiver: never paid (max base 0)
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.base_vault.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new_readonly(a.base_mint.key(), false),
                AccountMeta::new_readonly(quote, false),
                AccountMeta::new_readonly(a.fee_authority.key(), true),
                AccountMeta::new_readonly(a.token_base_program.key(), false),
                AccountMeta::new_readonly(qprog, false),
                AccountMeta::new_readonly(DBC_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DBC_PROGRAM, false),
            ],
            data,
        };
        let infos = [
            a.dbc_pool_authority.to_account_info(),
            a.dbc_config.to_account_info(),
            a.dbc_pool.to_account_info(),
            a.burn_ata.to_account_info(),
            a.wsol_ata.to_account_info(),
            a.base_vault.to_account_info(),
            a.quote_vault.to_account_info(),
            a.base_mint.to_account_info(),
            a.quote_mint.to_account_info(),
            a.fee_authority.to_account_info(),
            a.token_base_program.to_account_info(),
            a.token_program.to_account_info(),
            a.dbc_event_authority.to_account_info(),
            a.dbc_program.to_account_info(),
        ];
        invoke_signed(&ix, &infos, &[&[FEE_SEED, &[a.state.fee_bump]]])?;
        let got = token_amount(&a.wsol_ata)?.saturating_sub(before);
        if quote != WSOL {
            return credit_quote(ctx.remaining_accounts, &quote, got, lp);
        }
        // the platform token's own curve fee splits with the treasury (owner 2026-10-23)
        let treasury_bps = if !lp && a.base_mint.key() == a.state.platform_mint { a.params.fee.treasury_bps } else { 0 };
        let wsol = a.wsol_ata.to_account_info();
        absorb_as(&mut ctx.accounts.state, &wsol, SRC_DBC_FEE, got, lp, treasury_bps)
    }

    /// Anyone, after an LP-bound coin (module docs) graduated into its compounding DAMM v2 pool:
    /// claim up to `max_claim` (at most `LP_MAX_CLAIM_LAMPORTS`) of its partner curve fees, buy
    /// the coin with about half on that pool (refused when spot is more than
    /// `MAX_SPOT_PREMIUM_BPS` over the coin's ring TWAP, reverted when the buy's price is more than
    /// `MAX_EXEC_PREMIUM_BPS` over it), add both sides (plus any of the coin the fee PDA already
    /// holds) to the fee PDA's position in the pool, and lock the added liquidity permanently.
    /// The SOL the deposit could not use is booked to the burn side. The keeper calls it until the
    /// pool's partner fee is empty; the fee PDA's coin account must exist.
    pub fn fee_to_lp<'info>(ctx: Context<'_, '_, '_, 'info, FeeToLp<'info>>, max_claim: u64) -> Result<()> {
        let a = &ctx.accounts;
        let mint = a.base_mint.key();
        let fee = a.fee_authority.key();
        let slot = Clock::get()?.slot;
        // the coin's quote (`wsol_mint`): WSOL, or another quote claimed into the fee PDA's account of it
        let quote = a.wsol_mint.key();
        let qprog = a.token_program.key();
        check_receiver(&a.state, &a.wsol_ata, &a.wsol_mint, &qprog)?;
        // never the platform token: its fees are income (half to the treasury), not LP (2026-10-23)
        require!(mint != a.state.platform_mint, FeeError::NotLpBound);
        // the coin: an LP-bound config's pool, migrated, bound by levpad-hook to this DAMM v2 pool
        let fee_bps = {
            let d = a.dbc_config.try_borrow_data()?;
            require!(lp_bound(a.dbc_config.owner, &d, &fee), FeeError::NotLpBound);
            require!(d[8..40] == quote.to_bytes(), FeeError::WrongAccount);
            u16::from_le_bytes(d[dbc_config::MIGRATED_POOL_FEE_BPS..dbc_config::MIGRATED_POOL_FEE_BPS + 2].try_into().unwrap()) as u64
        };
        {
            let d = a.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(a.dbc_pool.owner, &d, &mint), FeeError::WrongAccount);
            require!(d[dbc::CONFIG..dbc::CONFIG + 32] == a.dbc_config.key().to_bytes(), FeeError::WrongAccount);
            require!(d[dbc::IS_MIGRATED] == 1, FeeError::WrongPhase);
        }
        {
            require_keys_eq!(a.damm_link.key(), levpad_hook::damm_link_address(&mint).0, FeeError::WrongAccount);
            require_keys_eq!(*a.damm_link.owner, levpad_hook::ID, FeeError::WrongAccount);
            let d = a.damm_link.try_borrow_data()?;
            let link = levpad_hook::DammLink::try_deserialize(&mut &d[..])?;
            require!(link.mint == mint && link.damm_pool == a.damm_pool.key() && link.ring == a.ring.key(), FeeError::WrongAccount);
        }
        let read_pool = |p: &AccountInfo| -> Result<(u128, u128, u64, u64)> {
            require_keys_eq!(*p.owner, DAMM_V2_PROGRAM, FeeError::WrongAccount);
            let d = p.try_borrow_data()?;
            require!(d.len() >= venue::damm::POOL_LEN && d[..8] == venue::damm::POOL_DISC, FeeError::WrongAccount);
            require!(venue::key_at(&d, venue::damm::TOKEN_A_MINT) == mint && venue::key_at(&d, venue::damm::TOKEN_B_MINT) == quote, FeeError::WrongAccount);
            require!(d[damm_pool::COLLECT_FEE_MODE] == damm_pool::COMPOUNDING, FeeError::NotLpBound);
            let spot = price_from_sqrt_q64(venue::u128_at(&d, venue::damm::SQRT_PRICE)).map_err(|_| error!(FeeError::Math))?;
            Ok((spot, venue::u128_at(&d, venue::damm::LIQUIDITY), venue::u64_at(&d, damm_pool::TOKEN_A_AMOUNT), venue::u64_at(&d, damm_pool::TOKEN_B_AMOUNT)))
        };
        require_keys_eq!(a.coin_ata.key(), get_associated_token_address_with_program_id(&fee, &mint, &anchor_spl::token_2022::ID), FeeError::WrongAccount);
        let seeds: &[&[u8]] = &[FEE_SEED, &[a.state.fee_bump]];
        let (w0, c0) = (token_amount(&a.wsol_ata)?, token_amount(&a.coin_ata)?);

        // 1. the coin's flip fees (remaining accounts: its ["flip", mint] PDA and that PDA's account
        // of the quote, when passed), then claim
        let flipped = sweep_flip_into(ctx.remaining_accounts, &mint, &a.wsol_mint.to_account_info(), &a.token_program.to_account_info(), &a.wsol_ata.to_account_info())?;
        let cap = max_claim.min(LP_MAX_CLAIM_LAMPORTS);
        require!(cap > 0, FeeError::Nothing);
        let partner_fee = venue::u64_at(&a.dbc_pool.try_borrow_data()?, DBC_PARTNER_QUOTE_FEE);
        if partner_fee > 0 {
        let mut data = disc("claim_trading_fee2").to_vec();
        data.extend_from_slice(&0u64.to_le_bytes()); // max base: curve fees are quote only
        data.extend_from_slice(&cap.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes()); // TransferHookAccountsInfo { slices: [] }
        let ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.dbc_pool.key(), false),
                AccountMeta::new(a.coin_ata.key(), false), // base receiver: never paid (max base 0)
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.dbc_base_vault.key(), false),
                AccountMeta::new(a.dbc_quote_vault.key(), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(quote, false),
                AccountMeta::new_readonly(fee, true),
                AccountMeta::new_readonly(anchor_spl::token_2022::ID, false),
                AccountMeta::new_readonly(qprog, false),
                AccountMeta::new_readonly(DBC_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DBC_PROGRAM, false),
            ],
            data,
        };
        invoke_signed(
            &ix,
            &[
                a.dbc_pool_authority.to_account_info(),
                a.dbc_config.to_account_info(),
                a.dbc_pool.to_account_info(),
                a.coin_ata.to_account_info(),
                a.wsol_ata.to_account_info(),
                a.dbc_base_vault.to_account_info(),
                a.dbc_quote_vault.to_account_info(),
                a.base_mint.to_account_info(),
                a.wsol_mint.to_account_info(),
                a.fee_authority.to_account_info(),
                a.token_2022_program.to_account_info(),
                a.token_program.to_account_info(),
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
            &[seeds],
        )?;
        }
        let claimed = token_amount(&a.wsol_ata)?.saturating_sub(w0);
        require!(claimed > 0, FeeError::Nothing);
        if flipped > 0 {
            emit!(FlipSwept { mint, amount: flipped, to_lp: true });
        }

        // 2. buy about half, guarded by the coin's ring TWAP like a buyback
        let (spot, _, ra, rb) = read_pool(a.damm_pool.as_ref())?;
        let twap = ring_twap(&a.ring, &mint, slot)?;
        require!(spot.checked_mul(10_000).ok_or(FeeError::Math)? <= twap.checked_mul(10_000 + MAX_SPOT_PREMIUM_BPS as u128).ok_or(FeeError::Math)?, FeeError::SpotAboveTwap);
        let swapped = lp_swap_amount(claimed, ra, rb, fee_bps);
        let mut bought = 0u64;
        if swapped > 0 {
            let limit = mul_div(twap, 10_000 + MAX_EXEC_PREMIUM_BPS as u128, 10_000).map_err(|_| error!(FeeError::Math))?;
            let min_out = u64::try_from(mul_div(swapped as u128, Q64, limit).map_err(|_| error!(FeeError::Math))?).map_err(|_| FeeError::Math)?.max(1);
            let mut data = disc("swap").to_vec();
            data.extend_from_slice(&swapped.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            let ix = Instruction {
                program_id: DAMM_V2_PROGRAM,
                accounts: vec![
                    AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                    AccountMeta::new(a.damm_pool.key(), false),
                    AccountMeta::new(a.wsol_ata.key(), false),
                    AccountMeta::new(a.coin_ata.key(), false),
                    AccountMeta::new(a.token_a_vault.key(), false),
                    AccountMeta::new(a.token_b_vault.key(), false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new_readonly(quote, false),
                    AccountMeta::new_readonly(fee, true),
                    AccountMeta::new_readonly(anchor_spl::token_2022::ID, false),
                    AccountMeta::new_readonly(qprog, false),
                    AccountMeta::new_readonly(DAMM_V2_PROGRAM, false), // no referral
                    AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                    AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
                ],
                data,
            };
            invoke_signed(
                &ix,
                &[
                    a.damm_pool_authority.to_account_info(),
                    a.damm_pool.to_account_info(),
                    a.wsol_ata.to_account_info(),
                    a.coin_ata.to_account_info(),
                    a.token_a_vault.to_account_info(),
                    a.token_b_vault.to_account_info(),
                    a.base_mint.to_account_info(),
                    a.wsol_mint.to_account_info(),
                    a.fee_authority.to_account_info(),
                    a.token_2022_program.to_account_info(),
                    a.token_program.to_account_info(),
                    a.damm_event_authority.to_account_info(),
                    a.damm_program.to_account_info(),
                ],
                &[seeds],
            )?;
            bought = token_amount(&a.coin_ata)?.saturating_sub(c0);
            let need = mul_div(swapped as u128, Q64, limit).map_err(|_| error!(FeeError::Math))?;
            require!(bought as u128 >= need.max(1), FeeError::PriceAboveTwap);
        }

        // 3. add both sides to the fee PDA's position at the pool's ratio, 4. lock it for good
        let tokens = token_amount(&a.coin_ata)?;
        let sol = token_amount(&a.wsol_ata)?.saturating_sub(w0);
        let (_, l, ra, rb) = read_pool(a.damm_pool.as_ref())?;
        require!(ra > 0 && rb > 0, FeeError::Math);
        let la = mul_div(tokens as u128, l, ra as u128).map_err(|_| error!(FeeError::Math))?;
        let lb = mul_div(sol as u128, l, rb as u128).map_err(|_| error!(FeeError::Math))?;
        let liquidity = la.min(lb);
        require!(liquidity > 0, FeeError::Nothing);
        let mut data = disc("add_liquidity").to_vec();
        data.extend_from_slice(&liquidity.to_le_bytes());
        data.extend_from_slice(&tokens.to_le_bytes());
        data.extend_from_slice(&sol.to_le_bytes());
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new(a.damm_pool.key(), false),
                AccountMeta::new(a.position.key(), false),
                AccountMeta::new(a.coin_ata.key(), false),
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.token_a_vault.key(), false),
                AccountMeta::new(a.token_b_vault.key(), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(quote, false),
                AccountMeta::new_readonly(a.position_nft_account.key(), false),
                AccountMeta::new_readonly(fee, true),
                AccountMeta::new_readonly(anchor_spl::token_2022::ID, false),
                AccountMeta::new_readonly(qprog, false),
                AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
            ],
            data,
        };
        invoke_signed(
            &ix,
            &[
                a.damm_pool.to_account_info(),
                a.position.to_account_info(),
                a.coin_ata.to_account_info(),
                a.wsol_ata.to_account_info(),
                a.token_a_vault.to_account_info(),
                a.token_b_vault.to_account_info(),
                a.base_mint.to_account_info(),
                a.wsol_mint.to_account_info(),
                a.position_nft_account.to_account_info(),
                a.fee_authority.to_account_info(),
                a.token_2022_program.to_account_info(),
                a.token_program.to_account_info(),
                a.damm_event_authority.to_account_info(),
                a.damm_program.to_account_info(),
            ],
            &[seeds],
        )?;
        let mut data = disc("permanent_lock_position").to_vec();
        data.extend_from_slice(&liquidity.to_le_bytes());
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new(a.damm_pool.key(), false),
                AccountMeta::new(a.position.key(), false),
                AccountMeta::new_readonly(a.position_nft_account.key(), false),
                AccountMeta::new_readonly(fee, true),
                AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
            ],
            data,
        };
        invoke_signed(
            &ix,
            &[
                a.damm_pool.to_account_info(),
                a.position.to_account_info(),
                a.position_nft_account.to_account_info(),
                a.fee_authority.to_account_info(),
                a.damm_event_authority.to_account_info(),
                a.damm_program.to_account_info(),
            ],
            &[seeds],
        )?;

        // the claim never dips into the pending sides; what it could not use goes to the burn side
        let w1 = token_amount(&a.wsol_ata)?;
        require!(w1 >= w0, FeeError::OverCap);
        let tokens_added = tokens.saturating_sub(token_amount(&a.coin_ata)?);
        let leftover = w1 - w0;
        let sol_added = claimed - swapped - leftover;
        let damm = a.damm_pool.key();
        if quote == WSOL {
            book_burn(&mut ctx.accounts.state, SRC_DBC_FEE, leftover)?;
        } else {
            credit_quote(ctx.remaining_accounts, &quote, leftover, true)?;
        }
        emit!(LpAdded { mint, damm_pool: damm, claimed, swapped, bought, sol_added, tokens_added, liquidity, leftover });
        Ok(())
    }

    /// Anyone: send a coin's flip fees (its ["flip", mint] accrual) to the burn side where its curve
    /// fees go there too: any time for a config that is not LP-bound, and for an LP-bound one while
    /// its curve is still running `params.fee.lp_grace_slots` after its pool opened (as
    /// `claim_dbc`). A graduated LP-bound coin's flip fees go to its LP with `fee_to_lp`. A quote
    /// coin's are credited to its `QuoteFee` (remaining account 0) as burn-only.
    pub fn sweep_flip<'info>(ctx: Context<'_, '_, '_, 'info, SweepFlip<'info>>) -> Result<()> {
        let a = &ctx.accounts;
        let mint = a.base_mint.key();
        let quote = a.quote_mint.key();
        let qprog = a.token_program.key();
        check_receiver(&a.state, &a.wsol_ata, &a.quote_mint, &qprog)?;
        {
            let d = a.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(a.dbc_pool.owner, &d, &mint), FeeError::WrongAccount);
            require!(d[dbc::CONFIG..dbc::CONFIG + 32] == a.dbc_config.key().to_bytes(), FeeError::WrongAccount);
            if lp_bound(a.dbc_config.owner, &a.dbc_config.try_borrow_data()?, &a.fee_authority.key()) {
                let opened = venue::u64_at(&d, DBC_ACTIVATION_POINT);
                let running = d[dbc::IS_MIGRATED] == 0 && d[dbc::MIGRATION_PROGRESS] == 0;
                require!(running && Clock::get()?.slot >= opened.saturating_add(a.params.fee.lp_grace_slots as u64), FeeError::LpBound);
            }
        }
        let accs = [a.flip_authority.to_account_info(), a.flip_account.to_account_info()];
        let got = sweep_flip_into(&accs, &mint, &a.quote_mint.to_account_info(), &a.token_program.to_account_info(), &a.wsol_ata.to_account_info())?;
        require!(got > 0, FeeError::NoFlip);
        emit!(FlipSwept { mint, amount: got, to_lp: false });
        if quote != WSOL {
            return credit_quote(ctx.remaining_accounts, &quote, got, true);
        }
        book_burn(&mut ctx.accounts.state, SRC_OTHER, got)
    }

    /// Anyone: withdraw the partner's share of a completed curve's surplus (SOL) and split it.
    pub fn claim_dbc_surplus<'info>(ctx: Context<'_, '_, '_, 'info, ClaimDbcSurplus<'info>>) -> Result<()> {
        let a = &ctx.accounts;
        let quote = a.quote_mint.key();
        let qprog = a.token_program.key();
        check_receiver(&a.state, &a.wsol_ata, &a.quote_mint, &qprog)?;
        let before = token_amount(&a.wsol_ata)?;
        let ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.dbc_pool.key(), false),
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new_readonly(quote, false),
                AccountMeta::new_readonly(a.fee_authority.key(), true),
                AccountMeta::new_readonly(qprog, false),
                AccountMeta::new_readonly(DBC_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DBC_PROGRAM, false),
            ],
            data: disc("partner_withdraw_surplus").to_vec(),
        };
        let infos = [
            a.dbc_pool_authority.to_account_info(),
            a.dbc_config.to_account_info(),
            a.dbc_pool.to_account_info(),
            a.wsol_ata.to_account_info(),
            a.quote_vault.to_account_info(),
            a.quote_mint.to_account_info(),
            a.fee_authority.to_account_info(),
            a.token_program.to_account_info(),
            a.dbc_event_authority.to_account_info(),
            a.dbc_program.to_account_info(),
        ];
        invoke_signed(&ix, &infos, &[&[FEE_SEED, &[a.state.fee_bump]]])?;
        let got = token_amount(&a.wsol_ata)?.saturating_sub(before);
        if quote != WSOL {
            return credit_quote(ctx.remaining_accounts, &quote, got, false);
        }
        let wsol = a.wsol_ata.to_account_info();
        absorb(&mut ctx.accounts.state, &wsol, SRC_DBC_SURPLUS, got)
    }

    /// Anyone: claim a graduated pool's LP fees (SOL) from a permanently locked position whose NFT
    /// the fee PDA holds, and book them (the platform token's pool: `params.fee.treasury_bps` to
    /// the treasury side, the rest burn; any other: burn). The liquidity can never be withdrawn.
    pub fn claim_damm(ctx: Context<ClaimDamm>) -> Result<()> {
        let a = &ctx.accounts;
        require_keys_eq!(a.token_b_mint.key(), WSOL, FeeError::WrongAccount);
        let before = token_amount(&a.wsol_ata)?;
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.damm_pool.key(), false),
                AccountMeta::new(a.position.key(), false),
                AccountMeta::new(a.burn_ata.key(), false), // token A receiver: OnlyB pools pay no A fees
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.token_a_vault.key(), false),
                AccountMeta::new(a.token_b_vault.key(), false),
                AccountMeta::new_readonly(a.token_a_mint.key(), false),
                AccountMeta::new_readonly(WSOL, false),
                AccountMeta::new_readonly(a.position_nft_account.key(), false),
                AccountMeta::new_readonly(a.fee_authority.key(), true),
                AccountMeta::new_readonly(a.token_a_program.key(), false),
                AccountMeta::new_readonly(anchor_spl::token::ID, false),
                AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
            ],
            data: disc("claim_position_fee").to_vec(),
        };
        let infos = [
            a.damm_pool_authority.to_account_info(),
            a.damm_pool.to_account_info(),
            a.position.to_account_info(),
            a.burn_ata.to_account_info(),
            a.wsol_ata.to_account_info(),
            a.token_a_vault.to_account_info(),
            a.token_b_vault.to_account_info(),
            a.token_a_mint.to_account_info(),
            a.token_b_mint.to_account_info(),
            a.position_nft_account.to_account_info(),
            a.fee_authority.to_account_info(),
            a.token_a_program.to_account_info(),
            a.token_program.to_account_info(),
            a.damm_event_authority.to_account_info(),
            a.damm_program.to_account_info(),
        ];
        invoke_signed(&ix, &infos, &[&[FEE_SEED, &[a.state.fee_bump]]])?;
        let got = token_amount(&a.wsol_ata)?.saturating_sub(before);
        // the platform token's graduated pool (OnlyB, 2%): its LP fees split with the treasury
        let treasury_bps = if a.token_a_mint.key() == a.state.platform_mint { a.params.fee.treasury_bps } else { 0 };
        let wsol = a.wsol_ata.to_account_info();
        absorb_as(&mut ctx.accounts.state, &wsol, SRC_DAMM_LP, got, false, treasury_bps)
    }

    /// Anyone: split WSOL that reached the fee PDA's account by other means (a donation).
    pub fn absorb_donations(ctx: Context<Absorb>) -> Result<()> {
        let wsol = ctx.accounts.wsol_ata.to_account_info();
        let s = &mut ctx.accounts.state;
        let before = s.pending();
        absorb(s, &wsol, SRC_OTHER, 0)?;
        require!(s.pending() > before, FeeError::Nothing);
        Ok(())
    }

    /// Once, signed by this program's upgrade authority: create `Params` with the default
    /// quote-sale parameters and that key as admin.
    pub fn init_params(ctx: Context<InitParams>) -> Result<()> {
        {
            let pd = &ctx.accounts.program_data;
            require_keys_eq!(*pd.owner, anchor_lang::solana_program::bpf_loader_upgradeable::ID, FeeError::NotAdmin);
            let d = pd.try_borrow_data()?;
            require!(d.len() >= 45 && d[0..4] == [3, 0, 0, 0] && d[12] == 1 && d[13..45] == ctx.accounts.admin.key().to_bytes(), FeeError::NotAdmin);
        }
        let p = &mut ctx.accounts.params;
        p.admin = ctx.accounts.admin.key();
        p.bump = ctx.bumps.params;
        p.quote_sale = QuoteSaleParams::DEFAULT;
        p.pending = QuoteSaleParams::DEFAULT;
        p.fee = FeeParams::DEFAULT;
        p.pending_fee = FeeParams::DEFAULT;
        let now = Clock::get()?.unix_timestamp;
        emit!(ParamsSet { eta: now, params: p.quote_sale });
        emit!(FeeParamsSet { eta: now, params: p.fee });
        // the treasury side reuses the staker side's bytes: a staker side left from before
        // 2026-10-23 goes to the burn side, and the staker counters start over as treasury ones
        let s = &mut ctx.accounts.state;
        s.pending_burn = s.pending_burn.checked_add(s.pending_treasury).ok_or(FeeError::Math)?;
        s.pending_treasury = 0;
        s.to_treasury = 0;
        s.treasury_payments = 0;
        Ok(())
    }

    /// Admin: the platform token's fee split, the treasury address and the buyback trigger,
    /// within the hard caps; at once while `quote_sale.timelock_secs` is 0, else after it
    /// (`apply_fee_params`; `cancel_params` drops it).
    pub fn set_fee_params(ctx: Context<AdminParams>, params: FeeParams) -> Result<()> {
        params.validate()?;
        let p = &mut ctx.accounts.params;
        let now = Clock::get()?.unix_timestamp;
        if p.quote_sale.timelock_secs == 0 {
            p.fee = params;
            p.pending_fee_eta = 0;
            emit!(FeeParamsSet { eta: now, params });
        } else {
            p.pending_fee = params;
            p.pending_fee_eta = now + p.quote_sale.timelock_secs as i64;
            emit!(FeeParamsSet { eta: p.pending_fee_eta, params });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending fee parameters.
    pub fn apply_fee_params(ctx: Context<ApplyParams>) -> Result<()> {
        let p = &mut ctx.accounts.params;
        require!(p.pending_fee_eta != 0 && Clock::get()?.unix_timestamp >= p.pending_fee_eta, FeeError::NotPending);
        p.fee = p.pending_fee;
        p.pending_fee_eta = 0;
        Ok(())
    }

    /// Anyone, once `params.fee.treasury` is set: send the whole treasury side to it in SOL (the
    /// WSOL is unwrapped through the ["unwrap"] PDA; the fee PDA fronts that account's rent and
    /// gets it back in the same instruction). Until a treasury is set it stays here.
    pub fn pay_treasury(ctx: Context<PayTreasury>) -> Result<()> {
        let treasury = ctx.accounts.params.fee.treasury;
        require!(treasury != Pubkey::default(), FeeError::NoTreasury);
        require_keys_eq!(ctx.accounts.treasury.key(), treasury, FeeError::WrongAccount);
        let amount = ctx.accounts.state.pending_treasury;
        require!(amount > 0, FeeError::Nothing);
        let fee_seeds: &[&[u8]] = &[FEE_SEED, &[ctx.accounts.state.fee_bump]];
        let unwrap_seeds: &[&[u8]] = &[UNWRAP_SEED, &[ctx.accounts.state.unwrap_bump]];
        let fee = ctx.accounts.fee_authority.to_account_info();
        let tmp = ctx.accounts.unwrap_account.to_account_info();
        let tp = ctx.accounts.token_program.to_account_info();
        let sys = ctx.accounts.system_program.to_account_info();
        let rent = Rent::get()?.minimum_balance(165);
        let have = tmp.lamports();
        if have == 0 {
            invoke_signed(&system_instruction::create_account(fee.key, tmp.key, rent, 165, &anchor_spl::token::ID), &[fee.clone(), tmp.clone(), sys.clone()], &[fee_seeds, unwrap_seeds])?;
        } else {
            // somebody sent lamports to the address (create_account would refuse it for good):
            // top up, allocate and assign instead; the close below returns them all
            if have < rent {
                invoke_signed(&system_instruction::transfer(fee.key, tmp.key, rent - have), &[fee.clone(), tmp.clone(), sys.clone()], &[fee_seeds])?;
            }
            invoke_signed(&system_instruction::allocate(tmp.key, 165), &[tmp.clone(), sys.clone()], &[unwrap_seeds])?;
            invoke_signed(&system_instruction::assign(tmp.key, &anchor_spl::token::ID), &[tmp.clone(), sys.clone()], &[unwrap_seeds])?;
        }
        anchor_spl::token::initialize_account3(CpiContext::new(
            tp.clone(),
            anchor_spl::token::InitializeAccount3 { account: tmp.clone(), mint: ctx.accounts.wsol_mint.to_account_info(), authority: fee.clone() },
        ))?;
        anchor_spl::token::transfer(
            CpiContext::new_with_signer(tp.clone(), anchor_spl::token::Transfer { from: ctx.accounts.wsol_ata.to_account_info(), to: tmp.clone(), authority: fee.clone() }, &[fee_seeds]),
            amount,
        )?;
        anchor_spl::token::close_account(CpiContext::new_with_signer(tp, anchor_spl::token::CloseAccount { account: tmp, destination: fee.clone(), authority: fee.clone() }, &[fee_seeds]))?;
        invoke_signed(&system_instruction::transfer(fee.key, &treasury, amount), &[fee, ctx.accounts.treasury.to_account_info(), sys], &[fee_seeds])?;
        let s = &mut ctx.accounts.state;
        s.pending_treasury = 0;
        s.to_treasury = s.to_treasury.checked_add(amount).ok_or(FeeError::Math)?;
        s.treasury_payments += 1;
        emit!(TreasuryPaid { treasury, lamports: amount, total: s.to_treasury });
        Ok(())
    }

    /// Admin: new quote-sale parameters within the hard caps, at once when the current timelock
    /// is 0, else after it (`apply_params`; `cancel_params` drops it).
    pub fn set_params(ctx: Context<AdminParams>, params: QuoteSaleParams) -> Result<()> {
        params.validate()?;
        let p = &mut ctx.accounts.params;
        let now = Clock::get()?.unix_timestamp;
        if p.quote_sale.timelock_secs == 0 {
            p.quote_sale = params;
            p.pending_eta = 0;
            emit!(ParamsSet { eta: now, params });
        } else {
            p.pending = params;
            p.pending_eta = now + p.quote_sale.timelock_secs as i64;
            emit!(ParamsSet { eta: p.pending_eta, params });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending parameters.
    pub fn apply_params(ctx: Context<ApplyParams>) -> Result<()> {
        let p = &mut ctx.accounts.params;
        require!(p.pending_eta != 0 && Clock::get()?.unix_timestamp >= p.pending_eta, FeeError::NotPending);
        p.quote_sale = p.pending;
        p.pending_eta = 0;
        Ok(())
    }

    pub fn cancel_params(ctx: Context<AdminParams>) -> Result<()> {
        ctx.accounts.params.pending_eta = 0;
        ctx.accounts.params.pending_fee_eta = 0;
        Ok(())
    }

    pub fn set_admin(ctx: Context<AdminParams>, admin: Pubkey) -> Result<()> {
        ctx.accounts.params.pending_admin = admin;
        Ok(())
    }

    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let p = &mut ctx.accounts.params;
        p.admin = p.pending_admin;
        p.pending_admin = Pubkey::default();
        Ok(())
    }

    /// Anyone (the payer funds the account rent): follow the engine's platform token after a
    /// hot-swap (`tilt_spot::set_platform_token`, owner 2026-10-28). Reads the engine config's mint
    /// in place, as `initialize` does, records it and creates the fee PDA's account of it, where
    /// buybacks of the new token land to be burned, and restarts the TWAP checkpoint when the mint
    /// changed. Run the old token's fee claims before the swap (red team V2-F1).
    pub fn sync_platform_token(ctx: Context<SyncPlatformToken>) -> Result<()> {
        {
            use tilt_spot::state::{Config, Params};
            let cfg = &ctx.accounts.engine_config;
            require_keys_eq!(*cfg.owner, tilt_spot::ID, FeeError::WrongAccount);
            let d = cfg.try_borrow_data()?;
            require!(d.len() >= 8 + Config::INIT_SPACE && d[..8] == *Config::DISCRIMINATOR, FeeError::WrongAccount);
            let mint_at = 8 + 32 + 32 + 4 + Params::INIT_SPACE + 8 + Params::INIT_SPACE;
            require!(d[mint_at..mint_at + 32] == ctx.accounts.platform_mint.key().to_bytes(), FeeError::NotLevcapToken);
        }
        let fee = ctx.accounts.fee_authority.key();
        let burn_ata = get_associated_token_address_with_program_id(&fee, &ctx.accounts.platform_mint.key(), &anchor_spl::token_2022::ID);
        require_keys_eq!(ctx.accounts.burn_ata.key(), burn_ata, FeeError::WrongAccount);
        associated_token::create_idempotent(CpiContext::new(
            ctx.accounts.associated_token_program.to_account_info(),
            associated_token::Create {
                payer: ctx.accounts.payer.to_account_info(),
                associated_token: ctx.accounts.burn_ata.to_account_info(),
                authority: ctx.accounts.fee_authority.to_account_info(),
                mint: ctx.accounts.platform_mint.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                token_program: ctx.accounts.token_2022_program.to_account_info(),
            },
        ))?;
        let s = &mut ctx.accounts.state;
        if s.platform_mint != ctx.accounts.platform_mint.key() {
            // the TWAP checkpoint held the previous token's ring cumulative (red team V2-F2): restart it
            let c = &mut ctx.accounts.checkpoint;
            c.slots = [0, 0];
            c.cums = [0, 0];
            c.newest = 0;
        }
        ctx.accounts.checkpoint.bump = ctx.bumps.checkpoint;
        s.platform_mint = ctx.accounts.platform_mint.key();
        s.burn_ata = burn_ata;
        Ok(())
    }

    /// Anyone (the payer funds the rent): a quote's `QuoteFee` and the fee PDA's associated account
    /// of it, for a quote the engine allows.
    pub fn init_quote(ctx: Context<InitQuote>) -> Result<()> {
        let mint = ctx.accounts.quote_mint.key();
        require!(mint != WSOL, FeeError::WrongAccount);
        engine_quote(&ctx.accounts.engine_config, &mint)?;
        require_keys_eq!(*ctx.accounts.quote_mint.owner, ctx.accounts.quote_token_program.key(), FeeError::WrongAccount);
        associated_token::create_idempotent(CpiContext::new(
            ctx.accounts.associated_token_program.to_account_info(),
            associated_token::Create {
                payer: ctx.accounts.payer.to_account_info(),
                associated_token: ctx.accounts.fee_quote_ata.to_account_info(),
                authority: ctx.accounts.fee_authority.to_account_info(),
                mint: ctx.accounts.quote_mint.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                token_program: ctx.accounts.quote_token_program.to_account_info(),
            },
        ))?;
        let q = &mut ctx.accounts.quote_fee;
        q.mint = mint;
        q.bump = ctx.bumps.quote_fee;
        Ok(())
    }

    /// Anyone: buy `atoms` of a quote the fee PDA holds for WSOL at the engine's price for it (the
    /// max of its long and recent TWAPs, `QuoteBook`) less `params.quote_sale.discount_bps`,
    /// rounded up, at most `max_lamports` (the caller's limit) and `quote_sale.max_lamports`; one
    /// sale per quote per `quote_sale.min_slots`. The WSOL is income (all to the burn side). The
    /// caller then sells the quote wherever it likes.
    pub fn swap_quote_to_sol(ctx: Context<SwapQuoteToSol>, atoms: u64, max_lamports: u64) -> Result<()> {
        let a = &ctx.accounts;
        let mint = a.quote_mint.key();
        let qprog = a.quote_token_program.key();
        let slot = Clock::get()?.slot;
        require!(mint != WSOL, FeeError::WrongAccount);
        check_receiver(&a.state, &a.fee_quote_ata, &a.quote_mint, &qprog)?;
        let e = engine_quote(&a.engine_config, &mint)?;
        let price = {
            require_keys_eq!(*a.quote_book.owner, tilt_spot::ID, FeeError::WrongAccount);
            let b = QuoteBook::try_deserialize(&mut &a.quote_book.try_borrow_data()?[..])?;
            require_keys_eq!(b.mint, mint, FeeError::WrongAccount);
            tilt_spot::quote::price_high(&b, &e, slot).ok_or(FeeError::NoQuotePrice)?
        };
        let qs = a.params.quote_sale;
        let q = &a.quote_fee;
        require!(q.sales == 0 || slot >= q.last_sale_slot.saturating_add(qs.min_slots as u64), FeeError::TooSoon);
        require!(atoms > 0 && atoms <= token_amount(&a.fee_quote_ata)?, FeeError::Nothing);
        let worth = mul_div(atoms as u128, price, Q64).map_err(|_| error!(FeeError::Math))?;
        let pay = (worth * (10_000 - qs.discount_bps as u128)).div_ceil(10_000);
        let pay = u64::try_from(pay).map_err(|_| FeeError::Math)?;
        require!(pay > 0, FeeError::Nothing);
        require!(pay <= qs.max_lamports && pay <= max_lamports, FeeError::OverCap);
        let decimals = {
            let d = a.quote_mint.try_borrow_data()?;
            require!(d.len() >= 82, FeeError::WrongAccount);
            d[44]
        };
        anchor_spl::token::transfer(
            CpiContext::new(
                a.token_program.to_account_info(),
                anchor_spl::token::Transfer { from: a.buyer_wsol.to_account_info(), to: a.wsol_ata.to_account_info(), authority: a.buyer.to_account_info() },
            ),
            pay,
        )?;
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                a.quote_token_program.to_account_info(),
                token_interface::TransferChecked {
                    from: a.fee_quote_ata.to_account_info(),
                    mint: a.quote_mint.to_account_info(),
                    to: a.buyer_quote.to_account_info(),
                    authority: a.fee_authority.to_account_info(),
                },
                &[&[FEE_SEED, &[a.state.fee_bump]]],
            ),
            atoms,
            decimals,
        )?;
        let buyer = a.buyer.key();
        let q = &mut ctx.accounts.quote_fee;
        let burn_atoms = q.burn_only.min(atoms);
        let to_burn = u64::try_from(mul_div(pay as u128, burn_atoms as u128, atoms as u128).map_err(|_| error!(FeeError::Math))?).map_err(|_| FeeError::Math)?;
        q.burn_only -= burn_atoms;
        q.sold = q.sold.saturating_add(atoms);
        q.lamports_in = q.lamports_in.saturating_add(pay);
        q.sales += 1;
        q.last_sale_slot = slot;
        let s = &mut ctx.accounts.state;
        book_burn(s, SRC_OTHER, to_burn)?;
        book(s, SRC_OTHER, pay - to_burn, 0)?;
        emit!(QuoteSold { mint, buyer, atoms, lamports: pay, price, to_burn_only: to_burn });
        Ok(())
    }

    /// Anyone: copy the platform token's ring cumulative into the checkpoint, at most once per
    /// `TWAP_WINDOW_SLOTS` (the older of the two is replaced). Creates it on first use (the
    /// caller pays its rent).
    pub fn checkpoint(ctx: Context<Checkpoint>) -> Result<()> {
        let mint = ctx.accounts.state.platform_mint;
        check_ring(&ctx.accounts.ring, &mint)?;
        let now = Clock::get()?.slot;
        let cum = ring_cum_now(&ctx.accounts.ring.try_borrow_data()?, now).ok_or(FeeError::ShortHistory)?;
        let c = &mut ctx.accounts.checkpoint;
        c.bump = ctx.bumps.checkpoint;
        let n = c.newest as usize;
        let i = if c.slots[n] == 0 {
            n
        } else {
            require!(now >= c.slots[n].saturating_add(TWAP_WINDOW_SLOTS), FeeError::TooSoon);
            1 - n
        };
        c.slots[i] = now;
        c.cums[i] = cum;
        c.newest = i as u8;
        emit!(Checkpointed { slot: now, cum });
        Ok(())
    }

    /// Anyone, while the platform token is on its DBC curve: buy it with up to `amount_in` of the
    /// burn side (DBC `swap2_with_transfer_hook`) and burn all the fee PDA holds of it.
    pub fn buy_burn_dbc(ctx: Context<BuyBurnDbc>, amount_in: u64, min_out: u64) -> Result<()> {
        let a = &ctx.accounts;
        let mint = a.state.platform_mint;
        let slot = Clock::get()?.slot;
        let spot = {
            let d = a.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(a.dbc_pool.owner, &d, &mint), FeeError::WrongAccount);
            require!(d[dbc::IS_MIGRATED] == 0 && d[dbc::MIGRATION_PROGRESS] == 0, FeeError::WrongPhase);
            price_from_sqrt_q64(venue::u128_at(&d, dbc::SQRT_PRICE)).map_err(|_| error!(FeeError::Math))?
        };
        check_caller(&a.params.fee, a.caller.as_ref())?;
        let twap = guard_twap(&a.params.fee, &a.ring, a.twap_checkpoint.as_ref().map(|x| x.as_ref()), &mint, slot)?;
        let guard_min = pre_buy(&a.state, &a.params.fee, amount_in, spot, twap, slot)?;
        let (w0, b0) = (token_amount(&a.wsol_ata)?, token_amount(&a.burn_ata)?);
        let mut data = disc("swap2_with_transfer_hook").to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&guard_min.max(min_out).to_le_bytes());
        data.push(0); // ExactIn
        data.extend_from_slice(&1u32.to_le_bytes()); // one slice: TransferHookBase, 4 accounts
        data.extend_from_slice(&[0, 4]);
        let ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.dbc_pool.key(), false),
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.burn_ata.key(), false),
                AccountMeta::new(a.base_vault.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new(mint, false),
                AccountMeta::new_readonly(WSOL, false),
                AccountMeta::new_readonly(a.fee_authority.key(), true),
                AccountMeta::new_readonly(anchor_spl::token_2022::ID, false),
                AccountMeta::new_readonly(anchor_spl::token::ID, false),
                AccountMeta::new_readonly(DBC_PROGRAM, false), // no referral
                AccountMeta::new_readonly(DBC_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DBC_PROGRAM, false),
                AccountMeta::new_readonly(a.meta_list.key(), false),
                AccountMeta::new_readonly(levpad_hook::ID, false),
                AccountMeta::new_readonly(a.dbc_pool.key(), false),
                AccountMeta::new(a.ring.key(), false),
            ],
            data,
        };
        let infos = [
            a.dbc_pool_authority.to_account_info(),
            a.dbc_config.to_account_info(),
            a.dbc_pool.to_account_info(),
            a.wsol_ata.to_account_info(),
            a.burn_ata.to_account_info(),
            a.base_vault.to_account_info(),
            a.quote_vault.to_account_info(),
            a.platform_mint.to_account_info(),
            a.wsol_mint.to_account_info(),
            a.fee_authority.to_account_info(),
            a.token_2022_program.to_account_info(),
            a.token_program.to_account_info(),
            a.dbc_event_authority.to_account_info(),
            a.dbc_program.to_account_info(),
            a.meta_list.to_account_info(),
            a.hook_program.to_account_info(),
            a.ring.to_account_info(),
        ];
        invoke_signed(&ix, &infos, &[&[FEE_SEED, &[a.state.fee_bump]]])?;
        let g = ctx.accounts.params.fee.guard_bps;
        settle_buy(&mut ctx.accounts.state, &ctx.accounts.wsol_ata, &ctx.accounts.burn_ata, &ctx.accounts.platform_mint, &ctx.accounts.fee_authority, &ctx.accounts.token_2022_program, (w0, b0), amount_in, guard_min, twap, spot, slot, 0, g)
    }

    /// Anyone, after the platform token graduated: buy it on its DAMM v2 pool (the one levpad-hook
    /// bound to its ring) with up to `amount_in` of the burn side and burn all the fee PDA holds.
    pub fn buy_burn_damm(ctx: Context<BuyBurnDamm>, amount_in: u64, min_out: u64) -> Result<()> {
        let a = &ctx.accounts;
        let mint = a.state.platform_mint;
        let slot = Clock::get()?.slot;
        {
            require_keys_eq!(a.damm_link.key(), levpad_hook::damm_link_address(&mint).0, FeeError::WrongAccount);
            require_keys_eq!(*a.damm_link.owner, levpad_hook::ID, FeeError::WrongAccount);
            let d = a.damm_link.try_borrow_data()?;
            let link = levpad_hook::DammLink::try_deserialize(&mut &d[..])?;
            require!(link.mint == mint && link.damm_pool == a.damm_pool.key() && link.ring == a.ring.key(), FeeError::WrongAccount);
        }
        let (spot, a_is_platform) = {
            require_keys_eq!(*a.damm_pool.owner, DAMM_V2_PROGRAM, FeeError::WrongAccount);
            let d = a.damm_pool.try_borrow_data()?;
            require!(d.len() >= venue::damm::POOL_LEN && d[..8] == venue::damm::POOL_DISC, FeeError::WrongAccount);
            let (ka, kb) = (venue::key_at(&d, venue::damm::TOKEN_A_MINT), venue::key_at(&d, venue::damm::TOKEN_B_MINT));
            require!((ka == mint && kb == WSOL) || (ka == WSOL && kb == mint), FeeError::WrongAccount);
            let q = venue::damm_quote(&d, &mint).map_err(|_| error!(FeeError::Math))?;
            (q.price, ka == mint)
        };
        check_caller(&a.params.fee, a.caller.as_ref())?;
        let twap = guard_twap(&a.params.fee, &a.ring, a.twap_checkpoint.as_ref().map(|x| x.as_ref()), &mint, slot)?;
        let guard_min = pre_buy(&a.state, &a.params.fee, amount_in, spot, twap, slot)?;
        let (w0, b0) = (token_amount(&a.wsol_ata)?, token_amount(&a.burn_ata)?);
        let mut data = disc("swap").to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&guard_min.max(min_out).to_le_bytes());
        let (mint_a, mint_b, prog_a, prog_b) = if a_is_platform {
            (mint, WSOL, anchor_spl::token_2022::ID, anchor_spl::token::ID)
        } else {
            (WSOL, mint, anchor_spl::token::ID, anchor_spl::token_2022::ID)
        };
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                AccountMeta::new(a.damm_pool.key(), false),
                AccountMeta::new(a.wsol_ata.key(), false),
                AccountMeta::new(a.burn_ata.key(), false),
                AccountMeta::new(a.token_a_vault.key(), false),
                AccountMeta::new(a.token_b_vault.key(), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new_readonly(a.fee_authority.key(), true),
                AccountMeta::new_readonly(prog_a, false),
                AccountMeta::new_readonly(prog_b, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false), // no referral
                AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
            ],
            data,
        };
        let infos = [
            a.damm_pool_authority.to_account_info(),
            a.damm_pool.to_account_info(),
            a.wsol_ata.to_account_info(),
            a.burn_ata.to_account_info(),
            a.token_a_vault.to_account_info(),
            a.token_b_vault.to_account_info(),
            a.platform_mint.to_account_info(),
            a.wsol_mint.to_account_info(),
            a.fee_authority.to_account_info(),
            a.token_2022_program.to_account_info(),
            a.token_program.to_account_info(),
            a.damm_event_authority.to_account_info(),
            a.damm_program.to_account_info(),
        ];
        invoke_signed(&ix, &infos, &[&[FEE_SEED, &[a.state.fee_bump]]])?;
        let g = ctx.accounts.params.fee.guard_bps;
        settle_buy(&mut ctx.accounts.state, &ctx.accounts.wsol_ata, &ctx.accounts.burn_ata, &ctx.accounts.platform_mint, &ctx.accounts.fee_authority, &ctx.accounts.token_2022_program, (w0, b0), amount_in, guard_min, twap, spot, slot, 1, g)
    }
}

/// After a buyback swap: check what came out against the TWAP guard, burn everything the fee PDA
/// holds of the platform token, and book it.
#[allow(clippy::too_many_arguments)]
fn settle_buy<'info>(
    s: &mut Account<'info, FeeState>,
    wsol_ata: &UncheckedAccount<'info>,
    burn_ata: &UncheckedAccount<'info>,
    mint: &InterfaceAccount<'info, Mint>,
    fee: &UncheckedAccount<'info>,
    t22: &Program<'info, Token2022>,
    (w0, b0): (u64, u64),
    amount_in: u64,
    guard_min: u64,
    twap: u128,
    spot: u128,
    slot: u64,
    venue_id: u8,
    guard_bps: u16,
) -> Result<()> {
    let (w1, b1) = (token_amount(wsol_ata)?, token_amount(burn_ata)?);
    let spent = w0.saturating_sub(w1);
    let out = b1.saturating_sub(b0);
    require!(spent > 0 && spent <= amount_in && spent <= s.pending_burn, FeeError::OverCap);
    // with the guard on: the price actually paid (fee and impact included) within guard_bps of
    // the TWAP
    let need = if guard_bps == 0 {
        1
    } else {
        let limit = mul_div(twap, 10_000 + guard_bps.min(HARD_MAX_GUARD_BPS) as u128, 10_000).map_err(|_| error!(FeeError::Math))?;
        mul_div(spent as u128, Q64, limit).map_err(|_| error!(FeeError::Math))?
    };
    require!(out >= guard_min && out as u128 >= need.max(1), FeeError::PriceAboveTwap);
    token_interface::burn(
        CpiContext::new_with_signer(
            t22.to_account_info(),
            token_interface::Burn { mint: mint.to_account_info(), from: burn_ata.to_account_info(), authority: fee.to_account_info() },
            &[&[FEE_SEED, &[s.fee_bump]]],
        ),
        b1,
    )?;
    s.pending_burn -= spent;
    s.burn_spent = s.burn_spent.checked_add(spent).ok_or(FeeError::Math)?;
    s.burned = s.burned.checked_add(b1).ok_or(FeeError::Math)?;
    s.buys += 1;
    s.last_buy_slot = slot;
    emit!(BoughtAndBurned { venue: venue_id, lamports_in: spent, tokens_out: out, burned: b1, twap_q64: twap, spot_q64: spot, burned_total: s.burned });
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + FeeState::INIT_SPACE, seeds = [STATE_SEED], bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], a plain system account: the DBC fee claimer and LP owner
    #[account(mut, seeds = [FEE_SEED], bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the engine's ["config"] (address, owner and discriminator checked; read in place)
    #[account(seeds = [tilt_spot::state::CONFIG_SEED], bump, seeds::program = tilt_spot::ID)]
    pub engine_config: UncheckedAccount<'info>,
    #[account(mint::token_program = token_2022_program)]
    pub platform_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the WSOL mint
    #[account(address = WSOL)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA, created here (address checked)
    #[account(mut)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's platform-token ATA, created here (address checked)
    #[account(mut)]
    pub burn_ata: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub token_2022_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SyncPlatformToken<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], owner of the new platform-token account
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the engine's ["config"] (address, owner and discriminator checked; read in place)
    #[account(seeds = [tilt_spot::state::CONFIG_SEED], bump, seeds::program = tilt_spot::ID)]
    pub engine_config: UncheckedAccount<'info>,
    #[account(mint::token_program = token_2022_program)]
    pub platform_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the fee PDA's account of the new platform token, created here (address checked)
    #[account(mut)]
    pub burn_ata: UncheckedAccount<'info>,
    /// The buyback's TWAP checkpoint, restarted when the platform token changes (made if missing)
    #[account(init_if_needed, payer = payer, space = 8 + TwapCheckpoint::INIT_SPACE, seeds = [CHECKPOINT_SEED], bump)]
    pub checkpoint: Account<'info, TwapCheckpoint>,
    pub token_2022_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimDbc<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], signs the claim
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's account of the coin's quote (its WSOL ATA for SOL coins; checked)
    #[account(mut)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's platform-token ATA, passed as DBC's base receiver (max base 0)
    #[account(mut, address = state.burn_ata)]
    pub burn_ata: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DBC checks it (its fee_claimer must be the fee PDA)
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the pool's quote mint (DBC checks it; WSOL, or a quote with a `QuoteFee`)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the base mint's token program (DBC checks it)
    pub token_base_program: UncheckedAccount<'info>,
    /// The quote's token program.
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: DBC's event authority
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: DBC
    #[account(address = DBC_PROGRAM)]
    pub dbc_program: UncheckedAccount<'info>,
    /// The platform token's fee split (2026-10-23).
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct FeeToLp<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"]: claims, swaps, owns the position
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's account of the coin's quote (its WSOL ATA for SOL coins; checked)
    #[account(mut)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's Token-2022 ATA of the coin (address checked; must exist)
    #[account(mut)]
    pub coin_ata: UncheckedAccount<'info>,
    /// CHECK: the coin (DBC and DAMM v2 check it against their pools)
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's quote mint (WSOL, or another quote; checked against the config and pool)
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: LP-bound (checked) and the pool's (checked)
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC pool (owner, layout, mint, config, migrated: checked)
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub dbc_base_vault: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub dbc_quote_vault: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's DammLink ["damm", mint] (checked)
    pub damm_link: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's ring for the coin (checked against the link and by the TWAP reader)
    pub ring: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: the coin's bound compounding pool (checked against the link, layout and mode)
    #[account(mut)]
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    /// CHECK: a position in the pool whose NFT the fee PDA holds (DAMM v2 checks both)
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    pub position_nft_account: UncheckedAccount<'info>,
    /// The quote's token program.
    pub token_program: Interface<'info, TokenInterface>,
    pub token_2022_program: Program<'info, Token2022>,
    /// CHECK: DBC's event authority
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: DBC
    #[account(address = DBC_PROGRAM)]
    pub dbc_program: UncheckedAccount<'info>,
    /// CHECK: DAMM v2's event authority
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2
    #[account(address = DAMM_V2_PROGRAM)]
    pub damm_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SweepFlip<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
    /// CHECK: PDA ["fee"] (the LP-bound check reads the config's fee claimer)
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's account of the coin's quote (checked)
    #[account(mut)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the coin
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's quote mint (checked with the receiver)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC config (its pool's; checked)
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC pool (owner, layout, mint, config: checked)
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: PDA ["flip", mint]
    #[account(seeds = [FLIP_SEED, base_mint.key().as_ref()], bump)]
    pub flip_authority: UncheckedAccount<'info>,
    /// CHECK: the PDA's associated account of the quote (address checked in the handler)
    #[account(mut)]
    pub flip_account: UncheckedAccount<'info>,
    /// The quote's token program.
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimDbcSurplus<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"]
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's account of the coin's quote (its WSOL ATA for SOL coins; checked)
    #[account(mut)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote mint (DBC checks it; WSOL, or a quote with a `QuoteFee`)
    pub quote_mint: UncheckedAccount<'info>,
    /// The quote's token program.
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: DBC's event authority
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: DBC
    #[account(address = DBC_PROGRAM)]
    pub dbc_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ClaimDamm<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], the position NFT's owner, signs the claim
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA
    #[account(mut, address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's platform-token ATA (token A receiver; OnlyB pools pay none)
    #[account(mut, address = state.burn_ata)]
    pub burn_ata: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it (has_one pool)
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    pub token_a_mint: UncheckedAccount<'info>,
    /// CHECK: WSOL (graduated pools quote in SOL as token B)
    #[account(address = WSOL)]
    pub token_b_mint: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it is the position's NFT account and its owner signed
    pub position_nft_account: UncheckedAccount<'info>,
    /// CHECK: token A's program (DAMM v2 checks it)
    pub token_a_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    /// CHECK: DAMM v2's event authority
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2
    #[account(address = DAMM_V2_PROGRAM)]
    pub damm_program: UncheckedAccount<'info>,
    /// The platform token's fee split (2026-10-23).
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct PayTreasury<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
    /// CHECK: PDA ["fee"], fronts the unwrap account's rent and pays the treasury
    #[account(mut, seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA
    #[account(mut, address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: PDA ["unwrap"], a WSOL account that lives only inside this instruction
    #[account(mut, seeds = [UNWRAP_SEED], bump = state.unwrap_bump)]
    pub unwrap_account: UncheckedAccount<'info>,
    /// CHECK: WSOL
    #[account(address = WSOL)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: `params.fee.treasury` (checked in the instruction)
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitParams<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Params::INIT_SPACE, seeds = [PARAMS_SEED], bump)]
    pub params: Box<Account<'info, Params>>,
    /// levfee's state: a staker side left from before 2026-10-23 moves to the burn side here.
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: this program's ProgramData (address checked); its upgrade authority is read raw
    /// (tag 3, slot, Some, key at 13..45) to keep bincode out of this binary
    #[account(seeds = [crate::ID.as_ref()], bump, seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID)]
    pub program_data: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminParams<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [PARAMS_SEED], bump = params.bump, has_one = admin @ FeeError::NotAdmin)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct ApplyParams<'info> {
    #[account(mut, seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    #[account(address = params.pending_admin @ FeeError::NotAdmin)]
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct InitQuote<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], the associated account's owner
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the engine's ["config"] (its quote list read in place)
    #[account(seeds = [tilt_spot::state::CONFIG_SEED], bump, seeds::program = tilt_spot::ID)]
    pub engine_config: UncheckedAccount<'info>,
    /// CHECK: an allowed quote mint (checked against the engine's list and its program)
    pub quote_mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + QuoteFee::INIT_SPACE, seeds = [QUOTE_FEE_SEED, quote_mint.key().as_ref()], bump)]
    pub quote_fee: Box<Account<'info, QuoteFee>>,
    /// CHECK: the fee PDA's associated account of the quote (the ATA program checks the address)
    #[account(mut)]
    pub fee_quote_ata: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SwapQuoteToSol<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], sends the quote
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [QUOTE_FEE_SEED, quote_mint.key().as_ref()], bump = quote_fee.bump)]
    pub quote_fee: Box<Account<'info, QuoteFee>>,
    /// CHECK: the quote mint (checked against its program and the engine's list)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's associated account of the quote (checked)
    #[account(mut)]
    pub fee_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA (receives the payment)
    #[account(mut, address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
    pub buyer: Signer<'info>,
    /// CHECK: the buyer's WSOL account (Token checks it)
    #[account(mut)]
    pub buyer_wsol: UncheckedAccount<'info>,
    /// CHECK: the buyer's account of the quote (the token program checks it)
    #[account(mut)]
    pub buyer_quote: UncheckedAccount<'info>,
    /// CHECK: the engine's ["config"] (its quote list read in place)
    #[account(seeds = [tilt_spot::state::CONFIG_SEED], bump, seeds::program = tilt_spot::ID)]
    pub engine_config: UncheckedAccount<'info>,
    /// CHECK: the engine's `QuoteBook` ["quote", mint] (address and owner checked)
    #[account(seeds = [tilt_spot::state::QUOTE_BOOK_SEED, quote_mint.key().as_ref()], bump, seeds::program = tilt_spot::ID)]
    pub quote_book: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
    /// The quote-sale parameters (2026-10-23).
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
}

#[derive(Accounts)]
pub struct Absorb<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: the fee PDA's WSOL ATA
    #[account(address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct BuyBurnDbc<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], the swapper
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA (pays)
    #[account(mut, address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's platform-token ATA (receives, then burned)
    #[account(mut, address = state.burn_ata)]
    pub burn_ata: UncheckedAccount<'info>,
    #[account(mut, address = state.platform_mint)]
    pub platform_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: WSOL
    #[account(address = WSOL)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DBC checks it against the pool
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: the platform token's DBC pool (checked: owner, layout, base mint, not migrated)
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: DBC checks it
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's meta list for the mint (Token-2022 checks it)
    pub meta_list: UncheckedAccount<'info>,
    /// CHECK: levpad-hook
    #[account(address = levpad_hook::ID)]
    pub hook_program: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's ring for the mint (address and layout checked)
    #[account(mut)]
    pub ring: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub token_2022_program: Program<'info, Token2022>,
    /// CHECK: DBC's event authority
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: DBC
    #[account(address = DBC_PROGRAM)]
    pub dbc_program: UncheckedAccount<'info>,
    /// The buyback trigger (threshold, largest buy; 2026-10-23).
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
    /// CHECK: the TWAP checkpoint ["twap"] (checked), used when the ring spans too few slots
    pub twap_checkpoint: Option<UncheckedAccount<'info>>,
    /// The admin-set `FeeParams::buyback_caller`, signing (needed only when one is set).
    pub caller: Option<Signer<'info>>,
}

#[derive(Accounts)]
pub struct Checkpoint<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    #[account(init_if_needed, payer = payer, space = 8 + TwapCheckpoint::INIT_SPACE, seeds = [CHECKPOINT_SEED], bump)]
    pub checkpoint: Account<'info, TwapCheckpoint>,
    /// CHECK: levpad-hook's ring for the platform mint (checked)
    pub ring: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BuyBurnDamm<'info> {
    #[account(mut, seeds = [STATE_SEED], bump = state.bump)]
    pub state: Box<Account<'info, FeeState>>,
    /// CHECK: PDA ["fee"], the swapper
    #[account(seeds = [FEE_SEED], bump = state.fee_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's WSOL ATA (pays)
    #[account(mut, address = state.wsol_ata)]
    pub wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the fee PDA's platform-token ATA (receives, then burned)
    #[account(mut, address = state.burn_ata)]
    pub burn_ata: UncheckedAccount<'info>,
    #[account(mut, address = state.platform_mint)]
    pub platform_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: WSOL
    #[account(address = WSOL)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: the graduated pool levpad-hook bound for the mint (checked against the link)
    #[account(mut)]
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's DammLink ["damm", mint] (checked)
    pub damm_link: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's ring for the mint (checked)
    pub ring: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub token_2022_program: Program<'info, Token2022>,
    /// CHECK: DAMM v2's event authority
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2
    #[account(address = DAMM_V2_PROGRAM)]
    pub damm_program: UncheckedAccount<'info>,
    /// The buyback trigger (threshold, largest buy; 2026-10-23).
    #[account(seeds = [PARAMS_SEED], bump = params.bump)]
    pub params: Box<Account<'info, Params>>,
    /// CHECK: the TWAP checkpoint ["twap"] (checked), used when the ring spans too few slots
    pub twap_checkpoint: Option<UncheckedAccount<'info>>,
    /// The admin-set `FeeParams::buyback_caller`, signing (needed only when one is set).
    pub caller: Option<Signer<'info>>,
}
