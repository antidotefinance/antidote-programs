use anchor_lang::prelude::*;

pub const BPS: u64 = 10_000;

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const AUTHORITY_SEED: &[u8] = b"authority";
pub const TRADER_SEED: &[u8] = b"trader";
pub const POSITION_SEED: &[u8] = b"pos";
/// Loss receipts (2026-10-23): ["receipt", owner, seq (u64 LE)], its NFT mint ["rmint", receipt]
/// and the owner's token account of it ["rtok", receipt].
pub const RECEIPT_SEED: &[u8] = b"receipt";
pub const RECEIPT_MINT_SEED: &[u8] = b"rmint";
pub const RECEIPT_TOKEN_SEED: &[u8] = b"rtok";

/// Launch configs whose coins count (their partner fee goes to levfee).
pub const DBC_CONFIGS: usize = 8;
/// Grant buckets per wallet; a ninth grant joins the newest bucket.
pub const BUCKETS: usize = 8;
/// Price samples kept (anti-farming TWAP, `price.rs`).
pub const PX_SAMPLES: usize = 32;
/// Mint budget day ring (`budget.rs`).
pub const BUDGET_DAYS: usize = 16;

/// 0.2% (Antidote launch economics, owner 2026-10-23; was 0.5%).
pub const DEFAULT_ROUTER_FEE_BPS: u16 = 20;
pub const HARD_MAX_ROUTER_FEE_BPS: u16 = 200;
/// The fee-capped grant's share (inert since the loss receipts of 2026-10-23; kept in `Params`
/// so the pinned layout does not move).
pub const DEFAULT_GRANT_BPS: u16 = 5_000;
pub const DEFAULT_GRANT_WEIGHT_BPS: u16 = 5_000;
pub const DEFAULT_LOCK_SECS: u32 = 86_400;
pub const DEFAULT_VEST_SECS: u32 = 604_800;
pub const HARD_MAX_LOCK: u32 = 2 * 365 * 86_400;
pub const HARD_MAX_TIMELOCK: u32 = 30 * 86_400;
/// Slot-denominated defaults assume the mainnet slot time measured 2026-10-23
/// (getRecentPerformanceSamples): 0.212 s, about 4.7 slots a second, 407,547 a day. They were
/// tuned for 0.4 s before. The admin retunes them with `set_params` if slot times move again.
/// A sample every 1,416 slots (about 5 min).
pub const DEFAULT_SAMPLE_GAP_SLOTS: u32 = 1_416;
/// The long TWAP: 34,000 slots (about 2 h).
pub const DEFAULT_LONG_WINDOW_SLOTS: u32 = 34_000;
/// The newest sample at most 4,245 slots old (about 15 min).
pub const DEFAULT_MAX_PX_AGE_SLOTS: u32 = 4_245;
pub const HARD_MIN_SAMPLE_GAP: u32 = 10;
/// About 25 h at 0.212 s (was 216,000: 24 h at 0.4 s, 12.7 h at 0.212 s).
pub const HARD_MAX_SAMPLE_GAP: u32 = 432_000;
pub const HARD_MAX_PX_AGE: u32 = 432_000;
pub const DEFAULT_BUDGET_K_BPS: u32 = 10_000;
pub const DEFAULT_BUDGET_WINDOW_DAYS: u16 = 7;
/// 10M TILT at 6 decimals.
pub const DEFAULT_BUDGET_SEED_ATOMS: u64 = 10_000_000_000_000;
pub const HARD_MAX_BUDGET_K_BPS: u32 = 100_000;
pub const HARD_MAX_BUDGET_WINDOW_DAYS: u16 = 15;

/// The anti-farming price: max(long TWAP, recent TWAP) of the platform token from samples of its
/// hook ring (`price.rs`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct PriceParams {
    /// A new sample needs this many slots since the last one.
    pub sample_gap_slots: u32,
    /// The long TWAP runs from the newest sample back to one at least this many slots older.
    pub long_window_slots: u32,
    /// The newest sample must be at most this many slots old for the price to count.
    pub max_age_slots: u32,
}

/// Mint tracks burn: over any trailing `window_days` UTC days, grants take at most
/// `k_bps` / 10,000 of the platform token levfee burned in them, plus a lifetime seed.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct MintBudget {
    pub k_bps: u32,
    pub window_days: u16,
    pub seed_atoms: u64,
}

/// Everything the admin can change, all at once and behind `timelock_secs`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Params {
    /// Taken on every routed buy (from the SOL in) and sell (from the SOL out), to levfee.
    pub router_fee_bps: u16,
    /// Inert since 2026-10-23 (the fee-capped grant; loss receipts use `ReceiptParams`).
    pub grant_bps: u16,
    /// Inert since 2026-10-23 (staking is gone).
    pub grant_weight_bps: u16,
    /// Inert since 2026-10-23 (grant buckets; receipts redeem after `receipt::delay_for`).
    pub lock_secs: u32,
    pub vest_secs: u32,
    /// Delay before a `set_params` takes effect (0 = at once).
    pub timelock_secs: u32,
    /// The platform token's TWAP (receipts price Antidote at max(spot, this TWAP)).
    pub price: PriceParams,
    /// Inert since 2026-10-23 (the k-of-burns budget; levcap's supply + owed limit and the
    /// issuance bucket replace it). Still validated within its bounds.
    pub budget: MintBudget,
    /// DBC launch configs whose coins count. Unused slots are the default key.
    pub dbc_configs: [Pubkey; DBC_CONFIGS],
}

impl Params {
    pub const fn default_with(dbc_configs: [Pubkey; DBC_CONFIGS]) -> Params {
        Params {
            router_fee_bps: DEFAULT_ROUTER_FEE_BPS,
            grant_bps: DEFAULT_GRANT_BPS,
            grant_weight_bps: DEFAULT_GRANT_WEIGHT_BPS,
            lock_secs: DEFAULT_LOCK_SECS,
            vest_secs: DEFAULT_VEST_SECS,
            timelock_secs: 0,
            price: PriceParams {
                sample_gap_slots: DEFAULT_SAMPLE_GAP_SLOTS,
                long_window_slots: DEFAULT_LONG_WINDOW_SLOTS,
                max_age_slots: DEFAULT_MAX_PX_AGE_SLOTS,
            },
            budget: MintBudget { k_bps: DEFAULT_BUDGET_K_BPS, window_days: DEFAULT_BUDGET_WINDOW_DAYS, seed_atoms: DEFAULT_BUDGET_SEED_ATOMS },
            dbc_configs,
        }
    }
}

/// Grant parameters added after launch (2026-10-23, Antidote launch economics). They live at the
/// end of `Config`, after every field the clients, levfee and levpad-hook read at fixed offsets
/// (`paused` at byte 72, `params.dbc_configs` at 120, `token_mint` at 684), so none of those move;
/// `grow_config` brings a `Config` created before them up to size. Set with `set_params_ext`,
/// behind the same timelock as `Params`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct ParamsExt {
    /// Share of the DBC partner fee that counts toward a wallet's eligible fees (the grant cap), in
    /// bps; the router fee always counts in full. 10,000 (the default) keeps the cap at router fee
    /// + partner fee; 0 counts the router fee only (the treasury's share), for when the partner fee
    /// goes to the coin's locked LP rather than to the platform.
    pub partner_fee_count_bps: u16,
    /// Share of the router fee paid to the referrer a routed trade names (its WSOL account, in the
    /// same trade), in bps of the router fee; at most `HARD_MAX_REFERRAL_BPS`. The share paid is
    /// not counted toward the wallet's eligible fees. Taken from `reserved` (2026-10-23), so
    /// nothing moved; a config whose `ext` existed before reads 0 (no referral) until set.
    pub referral_bps: u16,
    /// The router fee on the platform token's own trades, in bps (owner 2026-10-23: 0, so a
    /// routed Antidote trade costs what a direct one does; its pool's own 2% fee is the platform's
    /// revenue). At most `HARD_MAX_ROUTER_FEE_BPS`. Taken from `reserved` (2026-10-23): a config
    /// whose `ext` existed before reads 0. Every other coin pays `params.router_fee_bps`.
    pub platform_router_fee_bps: u16,
    /// The flip fee (owner 2026-10-23): a routed sell of a pad coin (never the platform token)
    /// within `flip_window_secs` of the position's size-weighted entry pays `flip_fee_bps` of the
    /// sell's proceeds of routed atoms, falling linearly to 0 at the window's end, to the coin's
    /// LP side (levfee's ["flip", mint] account of the quote, which `fee_to_lp` adds to the coin's
    /// locked LP after graduation). At most `HARD_MAX_FLIP_FEE_BPS` and `HARD_MAX_FLIP_WINDOW_SECS`;
    /// 0 turns it off. Taken from `reserved` (2026-10-23): a config whose `ext` existed before
    /// reads 0 (off) until set.
    pub flip_fee_bps: u16,
    pub flip_window_secs: u32,
    /// Room for later grant parameters; must be zero.
    pub reserved: [u8; 20],
}

pub const DEFAULT_PARTNER_FEE_COUNT_BPS: u16 = 10_000;
/// A quarter of the router fee (0.05% of the trade at the 0.2% router fee).
pub const DEFAULT_REFERRAL_BPS: u16 = 2_500;
/// At most half the router fee.
pub const HARD_MAX_REFERRAL_BPS: u16 = 5_000;
/// The flip fee: 1% within 30 s of entry (owner 2026-10-23), at most 3% and 600 s.
pub const DEFAULT_FLIP_FEE_BPS: u16 = 100;
pub const HARD_MAX_FLIP_FEE_BPS: u16 = 300;
pub const DEFAULT_FLIP_WINDOW_SECS: u32 = 30;
pub const HARD_MAX_FLIP_WINDOW_SECS: u32 = 600;

impl ParamsExt {
    pub const DEFAULT: ParamsExt = ParamsExt {
        partner_fee_count_bps: DEFAULT_PARTNER_FEE_COUNT_BPS,
        referral_bps: DEFAULT_REFERRAL_BPS,
        platform_router_fee_bps: 0,
        flip_fee_bps: DEFAULT_FLIP_FEE_BPS,
        flip_window_secs: DEFAULT_FLIP_WINDOW_SECS,
        reserved: [0; 20],
    };

    /// The flip fee rate (bps) on a sell `held` seconds after the entry: `flip_fee_bps` x
    /// (window - held) / window, 0 from the window's end (and when off).
    pub fn flip_bps(&self, held: i64) -> u64 {
        let w = self.flip_window_secs as i64;
        if self.flip_fee_bps == 0 || w == 0 || held >= w {
            return 0;
        }
        let left = (w - held.max(0)) as u64;
        self.flip_fee_bps as u64 * left / w as u64
    }
}

impl Default for ParamsExt {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One sample of the platform token's hook ring: its wrapping cumulative price (Q64.64 lamports
/// per token atom, times slots) at `slot`. Two samples give the exact TWAP between them.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct PxSample {
    pub slot: u64,
    pub cum: u128,
}

/// The platform token's price samples: index of the newest, how many are valid.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct PxBook {
    pub head: u8,
    pub len: u8,
    pub samples: [PxSample; PX_SAMPLES],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct BudgetDay {
    /// UTC day index (unix time / 86,400).
    pub day: u32,
    /// levfee's burned counter the first time this day was seen.
    pub burned_start: u64,
    /// Atoms granted from burn room on this day.
    pub granted: u64,
}

/// The mint budget's running state.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct MintBook {
    /// The highest levfee `FeeState::burned` read so far (atoms).
    pub burned_seen: u64,
    /// Seed atoms granted, lifetime.
    pub seed_used: u64,
    pub day_head: u8,
    pub day_len: u8,
    pub days: [BudgetDay; BUDGET_DAYS],
}

/// Lifetime counters, for analytics. Lamports unless named.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct Totals {
    pub buys: u64,
    pub sells: u64,
    /// SOL into the venues on buys, out of them on sells (before the router fee).
    pub sol_in: u64,
    pub sol_out: u64,
    pub router_fees: u64,
    /// The DBC partner fee (levfee's) booked on routed curve trades.
    pub partner_fees: u64,
    /// Realized losses on tracked tokens, before the cap.
    pub losses: u64,
    /// Receipt value V (lamports), lifetime.
    pub granted_value: u64,
    /// Platform-token atoms reserved on receipts, and minted at redemption.
    pub granted_atoms: u64,
    pub minted_atoms: u64,
    /// SOL credited to stakers (stopped 2026-10-23).
    pub funded: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// `set_admin` names it; it takes over with `accept_admin`.
    pub pending_admin: Pubkey,
    pub paused: bool,
    pub bump: u8,
    pub vault_bump: u8,
    pub authority_bump: u8,
    pub params: Params,
    /// A `set_params` waiting out the timelock; eta 0 = none.
    pub pending_eta: i64,
    /// (Boxed fields keep the SBF stack frame of `Config`'s decoder under 4 KB; borsh lays them
    /// out inline, as if unboxed.)
    pub pending: Box<Params>,
    /// The platform token (levcap-registered, minter = ["authority"]); default until set.
    pub token_mint: Pubkey,
    pub levcap_cap: Pubkey,
    /// The authority's associated Token-2022 account of the platform token.
    pub stake_vault: Pubkey,
    /// Atoms still mintable under levcap's lifetime cap after every grant made so far.
    pub mint_room: u64,
    /// Atoms granted and not yet minted.
    pub committed: u64,
    /// Grant value waiting for a price or for budget room (lamports).
    pub pending_value: u64,
    // staking
    pub total_weight: u64,
    /// Q64.64 lamports per unit of weight, lifetime.
    pub reward_per_weight: u128,
    /// Lamports credited to stakers and not yet claimed.
    pub staker_owed: u64,
    pub totals: Totals,
    pub px: Box<PxBook>,
    pub mint: Box<MintBook>,
    // ---- appended 2026-10-23: nothing above moves (`grow_config` sizes an older account) ----
    pub ext: ParamsExt,
    /// A `set_params_ext` waiting out the timelock; eta 0 = none.
    pub pending_ext: ParamsExt,
    pub pending_ext_eta: i64,
    // ---- appended 2026-10-23 (router round): nothing above moves ----
    /// Router fee paid to referrers, lifetime (lamports; part of `totals.router_fees`).
    pub referral_paid: u64,
    // ---- appended 2026-10-23 (multi-quote): nothing above moves ----
    /// The quote mints other than SOL that routed coins may be quoted in (stablecoins, tokenized
    /// stocks), with where their SOL price comes from. An empty slot has the default mint. Set
    /// one slot at a time with `set_quote` behind `params.timelock_secs`.
    pub quotes: Box<QuoteList>,
    /// A `set_quote` waiting out the timelock (slot, entry); eta 0 = none.
    pub pending_quote: QuoteEntry,
    pub pending_quote_slot: u8,
    pub pending_quote_eta: i64,
    // ---- appended 2026-10-23 (loss receipts): nothing above moves ----
    /// How a realized loss becomes a receipt (`receipt.rs`). Set with `set_receipt_params`
    /// behind `params.timelock_secs`.
    pub receipts: ReceiptParams,
    pub pending_receipts: ReceiptParams,
    pub pending_receipts_eta: i64,
    /// Receipt counters and the issuance bucket.
    pub receipt_book: ReceiptBook,
}

/// Byte offsets of `Config` (with the discriminator) that other programs and clients read raw.
/// `tests/tests/antidote_fees.rs` pins them against the real layout.
pub mod config_offsets {
    use anchor_lang::Space;
    pub const PAUSED: usize = 72;
    pub const DBC_CONFIGS: usize = 120;
    pub const TOKEN_MINT: usize = 684;
    /// `ext` starts right after the fields that existed before 2026-10-23.
    pub const EXT: usize = 2_032;
    /// The size of a `Config` account created before `ext` (what `grow_config` grows from).
    pub const LEN_BEFORE_EXT: usize = EXT;
    /// `referral_paid` (router round, 2026-10-23).
    pub const REFERRAL_PAID: usize = EXT + 2 * super::ParamsExt::INIT_SPACE + 8;
    /// The size of a `Config` that has `ext` but not `referral_paid`.
    pub const LEN_BEFORE_REFERRAL: usize = REFERRAL_PAID;
    /// `quotes` (multi-quote, 2026-10-23), then `pending_quote`, its slot and eta, the last fields.
    pub const QUOTES: usize = REFERRAL_PAID + 8;
    /// The size of a `Config` that has `referral_paid` but not `quotes` (2,112).
    pub const LEN_BEFORE_QUOTES: usize = QUOTES;
    /// `receipts` (loss receipts, 2026-10-23), then `pending_receipts`, its eta and `receipt_book`.
    pub const RECEIPTS: usize = QUOTES + super::QuoteList::INIT_SPACE + super::QuoteEntry::INIT_SPACE + 1 + 8;
    /// The size of a `Config` that has `quotes` but not `receipts` (3,264).
    pub const LEN_BEFORE_RECEIPTS: usize = RECEIPTS;
    pub const LEN: usize = 8 + super::Config::INIT_SPACE;
    const _: () = assert!(RECEIPTS + 2 * super::ReceiptParams::INIT_SPACE + 8 + super::ReceiptBook::INIT_SPACE == LEN);
}

// ---------- loss receipts (2026-10-23, SPEC.md "Loss receipts") ----------

/// 60% of the loss (owner 2026-10-23), also the hard cap.
pub const DEFAULT_RECEIPT_SHARE_BPS: u16 = 6_000;
pub const HARD_MAX_RECEIPT_SHARE_BPS: u16 = 6_000;
/// Full credit after 120 s held (h ramps linearly from 0).
pub const DEFAULT_HOLD_SECS: u32 = 120;
pub const HARD_MAX_HOLD_SECS: u32 = 7 * 86_400;
/// d = min(1, room_left / room_ref): full while 400M of levcap's room is left.
pub const DEFAULT_ROOM_REF_ATOMS: u64 = 400_000_000_000_000;
pub const HARD_MIN_ROOM_REF_ATOMS: u64 = 1_000_000;
pub const HARD_MAX_ROOM_REF_ATOMS: u64 = 1_000_000_000_000_000;
/// The redeem delay (owner 2026-10-23): delay = min x 2^(a x log2(mcap_twap / mcap_ref) +
/// max(0, issued - i_free) / i_double), clamped to [min, max]; the hard floor is 15 min (owner
/// 2026-10-26, was 1 h) and the hard ceiling 48 h.
pub const DEFAULT_MIN_DELAY_SECS: u32 = 900;
pub const HARD_MIN_DELAY_SECS: u32 = 900;
pub const DEFAULT_MAX_DELAY_SECS: u32 = 172_800;
pub const HARD_MAX_DELAY_SECS: u32 = 172_800;
/// a = 1 until the sim picks it (bps of the log2 of the market cap ratio).
pub const DEFAULT_DELAY_A_BPS: u32 = 10_000;
pub const HARD_MAX_DELAY_A_BPS: u32 = 100_000;
/// I_double = 30M tokens (owner 2026-10-23: 3% of the 1B supply; was 100M): past the free amount
/// the delay doubles per I_double issued.
pub const DEFAULT_I_DOUBLE_ATOMS: u64 = 30_000_000_000_000;
/// I_free = 30M tokens (owner 2026-10-23: "the first 3% of total supply unlocks at 1 hour"; the
/// floor is 15 min since 2026-10-26): the issuance term is 0 until this much has been issued.
pub const DEFAULT_I_FREE_ATOMS: u64 = 30_000_000_000_000;
pub const HARD_MAX_I_FREE_ATOMS: u64 = 1_000_000_000_000_000;
/// Issuance bucket: refills at 2% of levcap's room left per day (loss-share sim, 2026-10-23).
pub const DEFAULT_RATE_BPS_PER_DAY: u16 = 200;
pub const HARD_MAX_RATE_BPS_PER_DAY: u16 = 10_000;
pub const HARD_MAX_MIN_VALUE_LAMPORTS: u64 = 1_000_000_000;
/// The per-wallet fast lane (docs/research/redeem-delay-schedule.md): each wallet's first 0.5 SOL
/// of counted loss per 24 h window redeems after the minimum delay.
pub const DEFAULT_LANE_LAMPORTS: u64 = 500_000_000;
pub const HARD_MAX_LANE_LAMPORTS: u64 = 100_000_000_000;
pub const DEFAULT_LANE_WINDOW_SECS: u32 = 86_400;
pub const HARD_MIN_LANE_WINDOW_SECS: u32 = 3_600;
pub const HARD_MAX_LANE_WINDOW_SECS: u32 = 7 * 86_400;
/// Counted losses (L in SOL x h) under 0.01 SOL make no receipt and charge no rent float (owner
/// 2026-10-23); at most 1 SOL.
pub const DEFAULT_MIN_LOSS_LAMPORTS: u64 = 10_000_000;
pub const HARD_MAX_MIN_LOSS_LAMPORTS: u64 = 1_000_000_000;

pub const RECEIPT_URI_LEN: usize = 96;

/// Admin-set, timelocked like `Params`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct ReceiptParams {
    /// Share of the realized loss, bps (at most `HARD_MAX_RECEIPT_SHARE_BPS`; 0 turns receipts off).
    pub share_bps: u16,
    /// H: h = min(1, held / H), held from the position's size-weighted entry (0 = h is always 1).
    pub hold_secs: u32,
    /// room_ref: d = min(1, room_left / room_ref), room_left = levcap's cap - supply - owed.
    pub room_ref_atoms: u64,
    /// The redeem delay's floor (at least 15 min) and ceiling (at most 48 h): see `receipt::delay_for`.
    pub min_delay_secs: u32,
    pub max_delay_secs: u32,
    /// The issuance bucket refills at this share of room_left per day, continuously, up to one
    /// day's worth; a receipt bigger than the bucket is issued for what it holds.
    pub rate_bps_per_day: u16,
    /// Losses whose value is under this (lamports) get no receipt (0 = every loss).
    pub min_value_lamports: u64,
    /// The fast lane: when on, a wallet's first `lane_lamports` of counted loss (L in SOL x h) in a
    /// window of `lane_window_secs` (it restarts at the first loss after the window ends) redeems
    /// after `min_delay_secs`. A receipt with at least half its counted loss inside the lane gets
    /// the lane delay (no split: a second receipt would cost a second NFT).
    pub lane_enabled: bool,
    pub lane_lamports: u64,
    pub lane_window_secs: u32,
    /// a in bps: the delay's exponent grows by a x log2(mcap_twap / mcap_ref).
    pub delay_a_bps: u32,
    /// The market cap the delay is measured against (lamports; set when the platform token
    /// launches; 0 leaves that term out).
    pub mcap_ref_lamports: u64,
    /// The delay doubles for every `i_double_atoms` of receipts issued (cumulative,
    /// `receipt_book.atoms_reserved`; 0 leaves that term out).
    pub i_double_atoms: u64,
    /// The receipt NFT's metadata uri base (UTF-8, zero padded; empty by default): each NFT's uri is
    /// this base followed by its mint in base58 (`receipt::receipt_uri`; empty stays empty).
    pub uri: [u8; RECEIPT_URI_LEN],
    /// Counted losses (L in SOL x h, lamports) under this make no receipt and charge no rent float
    /// (0 = every loss; at most `HARD_MAX_MIN_LOSS_LAMPORTS`). A quote coin's loss waiting for a
    /// quote price is not checked (its rent float is paid at the sell). Taken from `reserved`
    /// (2026-10-23).
    pub min_loss_lamports: u64,
    /// The largest share of a day's issuance (`rate_bps_per_day` x room left) one receipt may take,
    /// bps (red team 2026-10-23, R2: one large receipt took the whole bucket). 0 or 10,000 = no
    /// limit. Taken from `reserved` (2026-10-23).
    pub max_receipt_bucket_bps: u16,
    /// Receipts issued (cumulative, as `i_double_atoms`) before the issuance term starts: up to
    /// this the delay's issuance term is 0, after it the delay doubles every `i_double_atoms`
    /// (owner 2026-10-23; 0 = the term starts at once, as before). Taken from `reserved`
    /// (2026-10-23).
    pub i_free_atoms: u64,
    /// Must be zero.
    pub reserved: [u8; 14],
}

impl ReceiptParams {
    pub const DEFAULT: ReceiptParams = ReceiptParams {
        share_bps: DEFAULT_RECEIPT_SHARE_BPS,
        hold_secs: DEFAULT_HOLD_SECS,
        room_ref_atoms: DEFAULT_ROOM_REF_ATOMS,
        min_delay_secs: DEFAULT_MIN_DELAY_SECS,
        max_delay_secs: DEFAULT_MAX_DELAY_SECS,
        rate_bps_per_day: DEFAULT_RATE_BPS_PER_DAY,
        min_value_lamports: 0,
        lane_enabled: true,
        lane_lamports: DEFAULT_LANE_LAMPORTS,
        lane_window_secs: DEFAULT_LANE_WINDOW_SECS,
        delay_a_bps: DEFAULT_DELAY_A_BPS,
        mcap_ref_lamports: 0,
        i_double_atoms: DEFAULT_I_DOUBLE_ATOMS,
        uri: [0; RECEIPT_URI_LEN],
        min_loss_lamports: DEFAULT_MIN_LOSS_LAMPORTS,
        max_receipt_bucket_bps: 0,
        i_free_atoms: DEFAULT_I_FREE_ATOMS,
        reserved: [0; 14],
    };
}

impl Default for ReceiptParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Receipt counters (atoms of the platform token unless named) and the issuance bucket.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct ReceiptBook {
    /// Receipts with an NFT, lifetime; the newest one's number (#1 first).
    pub issued: u64,
    pub redeemed: u64,
    /// Receipts booked and waiting for `issue_receipt` (NFT or price).
    pub pending: u64,
    /// Antidote owed on receipts not redeemed yet (levcap's `owed` from this program).
    pub owed: u64,
    pub value_lamports: u64,
    pub atoms_reserved: u64,
    pub atoms_redeemed: u64,
    /// Receipts the bucket scaled down, and the atoms it cut.
    pub scaled: u64,
    pub atoms_cut: u64,
    pub bucket_atoms: u64,
    /// When the bucket was last refilled (0 = never: it starts full).
    pub bucket_ts: i64,
    /// Receipts waiting for a price that their owner dropped (`drop_receipt`), and their value
    /// (lamports). Taken from `reserved` (2026-10-23).
    pub dropped: u64,
    pub value_dropped: u64,
    pub reserved: [u8; 16],
}

pub const RECEIPT_PENDING_PRICE: u8 = 0;
pub const RECEIPT_PENDING_NFT: u8 = 1;
pub const RECEIPT_ISSUED: u8 = 2;

/// ["receipt", owner, seq]: one realized loss's proof, with the canonical numbers (the NFT's metadata
/// repeats them). Closed at redemption; `attested`: booked by the attester (`attest.rs`).
#[account]
#[derive(InitSpace, Debug)]
pub struct Receipt {
    pub owner: Pubkey,
    /// The owner's `Trader::receipt_seq` when it was booked.
    pub seq: u64,
    /// "#n": the deployment-wide number, given when the NFT is issued (0 before).
    pub number: u64,
    pub bump: u8,
    /// `RECEIPT_PENDING_PRICE`, `RECEIPT_PENDING_NFT` or `RECEIPT_ISSUED`.
    pub status: u8,
    /// The token account's close authority is the ["authority"] PDA (set when the owner signed
    /// the issue, i.e. in the sell itself), so redemption closes it too.
    pub close_auth: bool,
    pub coin: Pubkey,
    pub quote: Pubkey,
    /// L: realized loss, in atoms of the quote (lamports for SOL coins).
    pub loss: u64,
    /// L in lamports (at the quote's min TWAP for quote coins; 0 until valued).
    pub loss_sol: u64,
    pub share_bps: u16,
    /// h and d at the close, parts per million.
    pub h_ppm: u32,
    pub d_ppm: u32,
    /// The bucket's scale on A (1,000,000 = none).
    pub rate_ppm: u32,
    /// V = share x L x h x d, lamports.
    pub value_sol: u64,
    /// A: platform-token atoms it redeems for (reserved at levcap when priced).
    pub amount: u64,
    /// A before the bucket scaled it.
    pub amount_wanted: u64,
    /// max(spot, TWAP) of the platform token, Q64.64 lamports per atom.
    pub price: u128,
    pub closed_at: i64,
    /// The delay `delay_for` gave it, and whether through the fast lane.
    pub delay_secs: u32,
    pub in_lane: bool,
    /// From here anyone may redeem it (the owner or the keeper); `auto_redeems_at` is the same.
    pub redeemable_from: i64,
    pub auto_redeems_at: i64,
    pub issued_at: i64,
    pub attested: bool,
    pub reserved: [u8; 26],
}

// ---------- multi-quote (2026-10-23, SPEC.md "Multi-quote launches") ----------

/// Quote slots in `Config::quotes`.
pub const QUOTES: usize = 8;
/// Samples kept per quote (`QuoteBook`).
pub const QPX_SAMPLES: usize = 32;
pub const QUOTE_BOOK_SEED: &[u8] = b"quote";
pub const QUOTE_PENDING_SEED: &[u8] = b"qpend";

/// How a price leg's pool is read (`quote.rs`).
pub const LEG_NONE: u8 = 0;
/// Meteora DAMM v2 `Pool` (sqrt price of token B per token A).
pub const LEG_DAMM_V2: u8 = 1;
/// Raydium CLMM `PoolState` (sqrt price of token 1 per token 0).
pub const LEG_RAYDIUM_CLMM: u8 = 2;
/// Orca Whirlpool (sqrt price of token B per token A).
pub const LEG_ORCA_WHIRLPOOL: u8 = 3;

/// `QuoteEntry::flags`: losses on this quote's coins are valued in SOL and granted. Off, the
/// grant value waits in the wallet's `QuotePending` (nothing is lost). `disable_quote_grants`
/// clears it at once; setting it goes through the timelock.
pub const QUOTE_GRANTS: u8 = 1;

/// One pool on the way from the quote to SOL.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct PriceLeg {
    pub pool: Pubkey,
    pub kind: u8,
}

/// An allowed quote mint and its SOL price source: leg 0 pairs the quote with SOL, or with a
/// middle mint that leg 1 pairs with SOL (TSLAx/USDC then USDC/SOL). The price is a TWAP of
/// samples of those pools' spot prices (`sample_quote`), valued at min(long, recent) for grants.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct QuoteEntry {
    /// The quote mint (SPL Token or Token-2022); default = empty slot.
    pub mint: Pubkey,
    pub flags: u8,
    pub legs: [PriceLeg; 2],
    /// A new sample needs this many slots since the last one.
    pub sample_gap_slots: u32,
    /// The long TWAP reaches back at least this far from the newest sample.
    pub long_window_slots: u32,
    /// The newest sample must be at most this old for a price to exist.
    pub max_age_slots: u32,
    /// Red team R7 (2026-10-23): the most a new sample may differ from the previous recorded one,
    /// in bps of it per second elapsed between them (`Clock::unix_timestamp`), as a ratio both
    /// ways (up to x (1 + s), down to / (1 + s)). 0 = `DEFAULT_QUOTE_STEP_BPS_PER_SEC`; at most
    /// `HARD_MAX_QUOTE_STEP_BPS_PER_SEC`. Taken from `reserved` (2026-10-23): `QuoteEntry` and
    /// `Config` keep their sizes and offsets.
    pub max_step_bps_per_sec: u16,
    /// Must be zero.
    pub reserved: [u8; 14],
}

/// `QuoteEntry::max_step_bps_per_sec` when it is 0 (SPEC.md "Multi-quote launches", R7): 1 bps a
/// second, 3% per 5-minute sample. A real 20% step reaches the book in 7 samples up, 8 down;
/// one atomic sample moves the 2 h long TWAP by at most 3% x gap / window (about 0.12%).
pub const DEFAULT_QUOTE_STEP_BPS_PER_SEC: u16 = 1;
/// Hard cap on `max_step_bps_per_sec`: 1% a second (300% per 5-minute sample, the clamp is then
/// practically off; for an owner who has to let a real crash through at once).
pub const HARD_MAX_QUOTE_STEP_BPS_PER_SEC: u16 = 100;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct QuoteList {
    pub entries: [QuoteEntry; QUOTES],
}

impl anchor_lang::Space for Box<QuoteList> {
    const INIT_SPACE: usize = QuoteList::INIT_SPACE;
}

impl Config {
    /// The entry for `mint`, if it is an allowed quote.
    pub fn quote(&self, mint: &Pubkey) -> Option<&QuoteEntry> {
        if *mint == Pubkey::default() {
            return None;
        }
        self.quotes.entries.iter().find(|q| q.mint == *mint)
    }
}

/// Lifetime counters of one quote's routed coins, in atoms of the quote unless named.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct QuoteTotals {
    pub buys: u64,
    pub sells: u64,
    pub quote_in: u64,
    pub quote_out: u64,
    pub router_fees: u64,
    pub partner_fees: u64,
    pub referral_paid: u64,
    pub losses: u64,
    /// Grant value earned (after the fee cap and `grant_bps`), valued in SOL so far, and the
    /// lamports it was valued at.
    pub grant_quote: u64,
    pub valued_quote: u64,
    pub valued_lamports: u64,
    /// Grant value waiting for a price in wallets' `QuotePending` accounts.
    pub pending_quote: u64,
}

/// ["quote", mint]: a quote's SOL price samples and its routed totals. Anyone creates it once the
/// mint is allowed (`init_quote_book`); anyone samples it (`sample_quote`).
#[account]
#[derive(InitSpace)]
pub struct QuoteBook {
    pub mint: Pubkey,
    pub bump: u8,
    /// The leg pools the samples were taken from; samples from other pools are dropped.
    pub legs: [Pubkey; 2],
    /// Spot (Q64.64 lamports per quote atom) at the newest sample: it holds until the next one.
    pub last_price: u128,
    pub head: u8,
    pub len: u8,
    /// Cumulative of the sampled spot prices (Q64.64 lamports per atom, times slots).
    pub samples: [PxSample; QPX_SAMPLES],
    pub totals: QuoteTotals,
    /// Unix time of the newest sample (R7 clamp, 2026-10-23; taken from `reserved`, so the
    /// account keeps its size). 0 on a book sampled before the clamp existed.
    pub last_ts: i64,
    pub reserved: [u8; 56],
}

/// ["qpend", owner, quote mint]: grant value in atoms of the quote that waits for a SOL price
/// (none fresh at the sell, or the quote's grants disabled). `convert_quote` (anyone) values it.
#[account]
#[derive(InitSpace)]
pub struct QuotePending {
    pub owner: Pubkey,
    pub quote_mint: Pubkey,
    pub bump: u8,
    pub pending: u64,
    pub valued_quote: u64,
    pub valued_lamports: u64,
}

/// Held stakers' SOL (inert since 2026-10-23; still created by `initialize`).
#[account]
#[derive(InitSpace)]
pub struct Vault {}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct Bucket {
    pub amount: u64,
    pub claimed: u64,
    /// 0 = empty.
    pub unlock_at: i64,
    pub vest_secs: u32,
}

/// A wallet. Since 2026-10-23 only `owner`, `bump` and the receipt counters are live; the
/// staking and grant-bucket fields stay (inert) so accounts made before still load.
#[account]
#[derive(InitSpace)]
pub struct Trader {
    pub owner: Pubkey,
    pub bump: u8,
    /// Counted loss (lamports, L x h) in the current fast-lane window (was `staked`).
    pub lane_loss: u64,
    /// When the current fast-lane window began (unix secs; was `weight`).
    pub lane_start: i64,
    pub reward_debt: u128,
    pub rewards_owed: u64,
    pub pending_value: u64,
    pub buckets: [Bucket; BUCKETS],
    /// Platform-token atoms on this wallet's receipts, booked and redeemed (lifetime).
    pub granted_atoms: u64,
    pub claimed_atoms: u64,
    /// The next receipt's seq (["receipt", owner, seq]); was `rewards_claimed`.
    pub receipt_seq: u64,
}

/// A wallet's book in one coin, from routed trades only.
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub bump: u8,
    /// Atoms bought through the router and not yet sold through it.
    pub tokens: u64,
    /// What those atoms cost, fees included (lamports).
    pub cost: u64,
    /// When `tokens` were bought, size-weighted (unix seconds; the hold ramp h counts from it).
    /// Was `eligible` (the fee cap) until 2026-10-23.
    pub entry_ts: i64,
    pub fees_paid: u64,
    pub bought: u64,
    pub sold: u64,
    pub losses: u64,
    pub granted_value: u64,
    pub trades: u32,
}

// `Config`'s boxed fields count their contents (borsh writes a box inline).
impl anchor_lang::Space for Box<Params> {
    const INIT_SPACE: usize = Params::INIT_SPACE;
}
impl anchor_lang::Space for Box<PxBook> {
    const INIT_SPACE: usize = PxBook::INIT_SPACE;
}
impl anchor_lang::Space for Box<MintBook> {
    const INIT_SPACE: usize = MintBook::INIT_SPACE;
}
