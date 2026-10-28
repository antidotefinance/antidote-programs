//! Tilt spot engine (working name; the product is Antidote).
//!
//! A swap router for coins launched on the pad: Meteora DBC while the coin is on its curve, then
//! the DAMM v2 pool it graduates into (DBC revokes the coin's transfer hook at graduation, so only
//! a router sees both venues). Each routed trade books the wallet's position in the coin (atoms,
//! what they cost with fees, and their size-weighted entry time). A sell that realizes a loss on
//! routed atoms of a pad coin earns a proof-of-loss receipt (`receipt.rs`, owner 2026-10-23):
//! V = share x L x h x d of value, redeemed after a delay of 15 min to 48 h for V / max(spot, TWAP) of the
//! platform token, reserved at levcap when the receipt is made and minted at redemption. There is
//! no staking and no fee cap since 2026-10-23. See SPEC.md "Loss receipts".

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hashv;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke;
use anchor_lang::system_program;
use anchor_spl::token;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{self, TokenInterface};
use levpad_hook::venue::{self, damm, DAMM_V2_PROGRAM, DBC_POOL_AUTHORITY, DBC_PROGRAM, WSOL};
use levpad_math::mul_div;

pub mod attest;
pub mod budget;
pub mod errors;
pub mod price;
pub mod quote;
pub mod receipt;
pub mod svg;
pub mod state;

use errors::{m, SpotError};
use state::*;

declare_id!("5iWcJpM7vGksXq7LxusmmfpPLZNmmWxjNC8aRaFJtoE8");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Antidote",
    project_url: "https://github.com/antidotefinance",
    contacts: "link:https://github.com/antidotefinance/antidote-programs/security/advisories/new",
    policy: "https://github.com/antidotefinance/antidote-programs/blob/main/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/antidotefinance/antidote-programs"
}

pub const TOKEN_2022: Pubkey = anchor_spl::token_2022::ID;
pub const DBC_EVENT_AUTHORITY: Pubkey = pubkey!("8Ks12pbrD6PXxfty1hVQiE9sc289zgU1zHkvXhrSdriF");
pub const DAMM_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
pub const DAMM_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");
/// DBC `PoolState::config` and `partner_quote_fee` (with the discriminator; DBC release 0.2.2).
pub const DBC_POOL_CONFIG: usize = 72;
/// DBC `PoolConfig::quote_mint` (with the discriminator; the first field).
pub const DBC_CONFIG_QUOTE_MINT: usize = 8;
pub const DBC_PARTNER_QUOTE_FEE: usize = 272;

pub const VENUE_DBC: u8 = 0;
pub const VENUE_DAMM: u8 = 1;

/// A routed trade.
#[event]
pub struct Traded {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub venue: u8,
    pub buy: bool,
    /// SOL into the venue (buy) or out of it (sell), before the router fee.
    pub sol: u64,
    pub tokens: u64,
    pub router_fee: u64,
    pub partner_fee: u64,
    /// Realized loss on the routed atoms sold (0 on buys and on gains).
    pub loss: u64,
    /// The loss receipt's value V in lamports (0 without a receipt, or while a quote coin's
    /// loss waits for a quote price).
    pub grant_value: u64,
}

/// A routed trade named a referrer: `amount` lamports of its router fee went to the referrer's
/// WSOL account (`referrer` is that account's owner). Separate from `Traded` so `Traded` keeps
/// its layout.
/// The platform token was set (`from` is the default key the first time) or hot-swapped.
#[event]
pub struct PlatformTokenSet {
    pub from: Pubkey,
    pub to: Pubkey,
}

#[event]
pub struct Referred {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub referrer: Pubkey,
    pub amount: u64,
}

/// A routed sell within the flip window paid `amount` (atoms of the coin's quote) at `bps` to the
/// coin's flip-fee accrual (levfee ["flip", mint]'s account of the quote; its LP side).
#[event]
pub struct FlipFee {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub quote: Pubkey,
    pub amount: u64,
    pub bps: u16,
    pub held_secs: i64,
}

/// A receipt waiting for a price was dropped by its owner (`drop_receipt`): its rent came back and
/// it will never redeem.
#[event]
pub struct ReceiptDropped {
    pub owner: Pubkey,
    pub receipt: Pubkey,
    pub value_sol: u64,
}

/// A loss on a coin quoted in another mint than SOL earned `grant_quote` atoms of the quote:
/// valued at `price` (Q64.64 lamports per atom) into `lamports` of grant value, or, when
/// `lamports` is 0, put in the wallet's `QuotePending` to wait for a price.
#[event]
pub struct QuoteGranted {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub quote_mint: Pubkey,
    pub grant_quote: u64,
    pub lamports: u64,
    pub price: u128,
}

#[event]
pub struct QuoteSet {
    pub eta: i64,
    pub slot: u8,
    pub entry: QuoteEntry,
}

/// Grant value became platform tokens in a bucket (not emitted since 2026-10-23; kept for
/// decoders of older transactions).
#[event]
pub struct Granted {
    pub owner: Pubkey,
    pub atoms: u64,
    pub value: u64,
    pub price: u128,
    pub unlock_at: i64,
    pub vest_secs: u32,
}

#[event]
pub struct ParamsSet {
    pub eta: i64,
    pub params: Params,
}

#[event]
pub struct ParamsExtSet {
    pub eta: i64,
    pub ext: ParamsExt,
}

fn disc(name: &str) -> [u8; 8] {
    hashv(&[format!("global:{name}").as_bytes()]).to_bytes()[..8].try_into().unwrap()
}

/// A token account's (mint, owner, amount), Token or Token-2022.
fn token_account(a: &AccountInfo) -> Result<(Pubkey, Pubkey, u64)> {
    require!(*a.owner == token::ID || *a.owner == TOKEN_2022, SpotError::WrongAccount);
    let d = a.try_borrow_data()?;
    require!(d.len() >= 165, SpotError::WrongAccount);
    Ok((venue::key_at(&d, 0), venue::key_at(&d, 32), venue::u64_at(&d, 64)))
}

fn amount(a: &AccountInfo) -> Result<u64> {
    Ok(token_account(a)?.2)
}

fn mul_div_u64(a: u64, b: u64, c: u64) -> Result<u64> {
    if c == 0 {
        return Ok(0);
    }
    u64::try_from(m(mul_div(a as u128, b as u128, c as u128))?).map_err(|_| error!(SpotError::Math))
}

fn init_trader(t: &mut Trader, owner: Pubkey, bump: u8) {
    if t.owner == Pubkey::default() {
        t.owner = owner;
        t.bump = bump;
    }
}

/// Sample the platform token's price ring when it is passed (levfee's state, once read for the
/// mint budget, is ignored since 2026-10-23).
fn refresh(cfg: &mut Config, px_ring: Option<&AccountInfo>, slot: u64) -> Result<()> {
    if let Some(r) = px_ring {
        price::sample(cfg, r, slot)?;
    }
    Ok(())
}

// ---------- the router ----------

/// What a routed swap moved, measured on the wallet's accounts.
struct Fill {
    sol: u64,
    tokens: u64,
    partner_fee: u64,
}

/// The accounts every routed trade shares. `user_wsol`, `fee_wsol` and `wsol_mint` hold the
/// coin's quote: WSOL for SOL coins, else the quote mint of the coin's DBC config (multi-quote).
struct Book<'a, 'info> {
    owner: &'a Signer<'info>,
    config: &'a mut Box<Account<'info, Config>>,
    position: &'a mut Box<Account<'info, Position>>,
    trader: &'a mut Box<Account<'info, Trader>>,
    user_wsol: &'a AccountInfo<'info>,
    fee_wsol: &'a AccountInfo<'info>,
    wsol_mint: &'a AccountInfo<'info>,
    /// The quote's token program (SPL Token for WSOL, SPL Token or Token-2022 otherwise).
    token_program: &'a AccountInfo<'info>,
    /// The referrer's quote account and its owner, when the trade names one (not the wallet itself).
    referrer: Option<(&'a AccountInfo<'info>, Pubkey)>,
    quote: Pubkey,
    quote_decimals: u8,
    /// A coin quoted in another mint than SOL: its quote's book and the wallet's pending account.
    q: Option<QuoteAccs<'a, 'info>>,
    /// The loss receipt's accounts, when the trade passes them (sells).
    rc: Option<ReceiptAccs<'a, 'info>>,
    /// The coin's flip-fee accrual, when the trade passes it (a sell inside the flip window).
    flip: Option<&'a AccountInfo<'info>>,
    px_ring: Option<&'a AccountInfo<'info>>,
    token_2022: AccountInfo<'info>,
    system: AccountInfo<'info>,
}

/// A sell's loss-receipt accounts, after the referrer (if any): the receipt ["receipt", owner,
/// seq] (writable), levcap's Cap (writable), the platform mint (its supply), the ["authority"]
/// PDA (levcap's minter) and levcap. The NFT is made by `issue_receipt` afterwards (measured:
/// a sell plus the NFT needs about 530k compute units and more than the 32 KB heap).
struct ReceiptAccs<'a, 'info> {
    receipt: &'a AccountInfo<'info>,
    cap: &'a AccountInfo<'info>,
    platform_mint: &'a AccountInfo<'info>,
    authority: &'a AccountInfo<'info>,
    levcap_program: &'a AccountInfo<'info>,
}

/// The coin's flip-fee accrual, the referrer and the receipt accounts from `at` on, each optional,
/// in that order: the accrual's and the receipt's addresses tell them apart (a referrer is a
/// token account, never either of them).
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn tail_accs<'a, 'info>(cfg: &Config, r: &'a [AccountInfo<'info>], at: usize, owner: &Pubkey, mint: &Pubkey, quote: &Pubkey, quote_program: &Pubkey, seq: u64) -> Result<(Option<&'a AccountInfo<'info>>, Option<(&'a AccountInfo<'info>, Pubkey)>, Option<ReceiptAccs<'a, 'info>>)> {
    let rkey = receipt::receipt_address(owner, seq).0;
    let (flip, at) = match r.get(at) {
        Some(acc) if acc.key() != rkey && acc.key() == budget::flip_account(mint, quote, quote_program) => {
            require!(acc.is_writable, SpotError::WrongAccount);
            (Some(acc), at + 1)
        }
        _ => (None, at),
    };
    let (referrer, rc_at) = match r.get(at) {
        Some(acc) if acc.key() == rkey => (None, at),
        Some(_) => (referrer_at(r, at, owner, quote, quote_program)?, at + 1),
        None => (None, at),
    };
    let Some(rc) = r.get(rc_at) else { return Ok((flip, referrer, None)) };
    require_keys_eq!(rc.key(), rkey, SpotError::WrongAccount);
    require!(r.len() >= rc_at + 5, SpotError::WrongAccount);
    let (cap, pm, auth, lp) = (&r[rc_at + 1], &r[rc_at + 2], &r[rc_at + 3], &r[rc_at + 4]);
    require!(cfg.token_mint != Pubkey::default(), SpotError::NoToken);
    require_keys_eq!(cap.key(), cfg.levcap_cap, SpotError::WrongAccount);
    require_keys_eq!(pm.key(), cfg.token_mint, SpotError::WrongAccount);
    require_keys_eq!(auth.key(), Pubkey::create_program_address(&[AUTHORITY_SEED, &[cfg.authority_bump]], &crate::ID).map_err(|_| SpotError::WrongAccount)?, SpotError::WrongAccount);
    require_keys_eq!(lp.key(), levcap::ID, SpotError::WrongAccount);
    require!(rc.is_writable && cap.is_writable, SpotError::WrongAccount);
    Ok((flip, referrer, Some(ReceiptAccs { receipt: rc, cap, platform_mint: pm, authority: auth, levcap_program: lp })))
}

/// A non-SOL coin's extra accounts: the quote's `QuoteBook` and the wallet's `QuotePending`
/// (checked for the account layout's sake; unused since 2026-10-23: a loss waiting for a quote
/// price waits in its receipt).
#[allow(dead_code)]
struct QuoteAccs<'a, 'info> {
    book: &'a AccountInfo<'info>,
    pending: &'a AccountInfo<'info>,
    system_program: AccountInfo<'info>,
}

/// The quote of a routed coin: SOL, or an allowed quote with its two accounts at `at` in the
/// remaining accounts. Returns the quote accounts (None for SOL) and where the referrer goes.
fn quote_accs<'a, 'info>(cfg: &Config, r: &'a [AccountInfo<'info>], at: usize, owner: &Pubkey, quote: &Pubkey, system_program: AccountInfo<'info>) -> Result<(Option<QuoteAccs<'a, 'info>>, usize)> {
    if *quote == WSOL {
        return Ok((None, at));
    }
    require!(cfg.quote(quote).is_some(), SpotError::QuoteNotAllowed);
    require!(r.len() >= at + 2, SpotError::WrongAccount);
    let (book, pending) = (&r[at], &r[at + 1]);
    require_keys_eq!(book.key(), Pubkey::find_program_address(&[QUOTE_BOOK_SEED, quote.as_ref()], &crate::ID).0, SpotError::WrongAccount);
    require_keys_eq!(*book.owner, crate::ID, SpotError::WrongAccount);
    require_keys_eq!(pending.key(), Pubkey::find_program_address(&[QUOTE_PENDING_SEED, owner.as_ref(), quote.as_ref()], &crate::ID).0, SpotError::WrongAccount);
    require!(book.is_writable && pending.is_writable, SpotError::WrongAccount);
    Ok((Some(QuoteAccs { book, pending, system_program }), at + 2))
}

/// The quote mint's decimals (SPL Token or Token-2022 mint, byte 44).
fn mint_decimals(ai: &AccountInfo) -> Result<u8> {
    let d = ai.try_borrow_data()?;
    require!(d.len() >= 82, SpotError::WrongAccount);
    Ok(d[44])
}

/// The optional referrer: the remaining account at `at` (after the fair-open and quote accounts,
/// if any), a token account of the coin's quote (WSOL for SOL coins) under the quote's program.
/// Its owner is the referrer. A wallet naming itself (its account's owner is the signer) gets
/// nothing: the trade goes on as if none was named.
fn referrer_at<'a, 'info>(r: &'a [AccountInfo<'info>], at: usize, owner: &Pubkey, quote: &Pubkey, quote_program: &Pubkey) -> Result<Option<(&'a AccountInfo<'info>, Pubkey)>> {
    let Some(acc) = r.get(at) else { return Ok(None) };
    let (m, o, _) = token_account(acc)?;
    require!(*acc.owner == *quote_program && m == *quote && acc.is_writable, SpotError::WrongAccount);
    Ok(if o == *owner { None } else { Some((acc, o)) })
}

/// Check the wallet's accounts and the coin, and start its position.
fn open_book(b: &mut Book, mint: &Pubkey, user_token: &AccountInfo, position_bump: u8, trader_bump: u8) -> Result<()> {
    require!(!b.config.paused, SpotError::Paused);
    // the platform token trades through the router too (2026-10-23); `book` grants it nothing
    let owner = b.owner.key();
    let qp = b.token_program.key();
    require_keys_eq!(b.wsol_mint.key(), b.quote, SpotError::WrongAccount);
    require_keys_eq!(*b.wsol_mint.owner, qp, SpotError::WrongAccount);
    let (wm, wo, _) = token_account(b.user_wsol)?;
    require!(wm == b.quote && wo == owner && *b.user_wsol.owner == qp, SpotError::WrongAccount);
    let (tm, to, _) = token_account(user_token)?;
    require!(tm == *mint && to == owner && *user_token.owner == TOKEN_2022, SpotError::WrongAccount);
    let fee_ata = anchor_spl::associated_token::get_associated_token_address_with_program_id(&budget::levfee_fee_authority(), &b.quote, &qp);
    require_keys_eq!(b.fee_wsol.key(), fee_ata, SpotError::WrongAccount);
    let p = &mut b.position;
    if p.owner == Pubkey::default() {
        p.owner = owner;
        p.mint = *mint;
        p.bump = position_bump;
    }
    init_trader(b.trader, owner, trader_bump);
    Ok(())
}

/// Router fee in WSOL from the wallet to levfee's WSOL account.
/// The router fee on a routed curve buy that spent `spent` lamports in the swap: grossed up so it
/// is `bps` of the total paid, `ceil(spent x bps / (10,000 - bps))`, never above `max_fee` (the
/// fee on the whole `amount_in`, which the wallet set aside).
pub fn buy_fee(spent: u64, bps: u64, max_fee: u64) -> Result<u64> {
    if bps == 0 {
        return Ok(0);
    }
    let den = (BPS - bps) as u128;
    let fee = (spent as u128 * bps as u128).div_ceil(den);
    Ok(u64::try_from(fee).unwrap_or(u64::MAX).min(max_fee))
}

/// The router fee rate for `mint`: `ext.platform_router_fee_bps` on the platform token (0 by
/// default, owner 2026-10-23), `params.router_fee_bps` on every other coin.
fn router_fee_bps(c: &Config, mint: &Pubkey) -> u64 {
    if *mint == c.token_mint {
        c.ext.platform_router_fee_bps as u64
    } else {
        c.params.router_fee_bps as u64
    }
}

/// The router fee in WSOL from the wallet: `referral_bps` of it to the referrer's WSOL account
/// when the trade names one, the rest to levfee's. Returns the referral share paid.
fn pay_router_fee<'info>(b: &Book<'_, 'info>, fee: u64) -> Result<u64> {
    if fee == 0 {
        return Ok(0);
    }
    let referral = match b.referrer {
        Some(_) => mul_div_u64(fee, b.config.ext.referral_bps as u64, BPS)?,
        None => 0,
    };
    send_wsol(b, b.fee_wsol, fee - referral)?;
    if let Some((acc, _)) = b.referrer {
        send_wsol(b, acc, referral)?;
    }
    Ok(referral)
}

fn send_wsol<'info>(b: &Book<'_, 'info>, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    token_interface::transfer_checked(
        CpiContext::new(
            b.token_program.clone(),
            token_interface::TransferChecked { from: b.user_wsol.clone(), mint: b.wsol_mint.clone(), to: to.clone(), authority: b.owner.to_account_info() },
        ),
        amount,
        b.quote_decimals,
    )
}

fn load_book(ai: &AccountInfo) -> Result<QuoteBook> {
    require_keys_eq!(*ai.owner, crate::ID, SpotError::WrongAccount);
    QuoteBook::try_deserialize(&mut &ai.try_borrow_data()?[..])
}

fn store<T: AccountSerialize>(ai: &AccountInfo, v: &T) -> Result<()> {
    let mut d = ai.try_borrow_mut_data()?;
    v.try_serialize(&mut &mut d[..])
}

/// Book a routed trade. A sell that realizes a loss on routed atoms of a pad coin books a loss
/// receipt when the trade passes its accounts (`receipt.rs`). `referral` is the part of
/// `router_fee` paid to a referrer. The platform token is booked like any coin but its losses
/// earn nothing.
#[allow(clippy::too_many_arguments)]
fn book(b: &mut Book, mint: Pubkey, venue: u8, buy: bool, f: &Fill, router_fee: u64, referral: u64, now: i64, slot: u64) -> Result<()> {
    let fees = router_fee.saturating_add(f.partner_fee);
    let platform = mint == b.config.token_mint;
    let p = &mut b.position;
    p.fees_paid = p.fees_paid.saturating_add(fees);
    p.trades = p.trades.saturating_add(1);
    let mut loss = 0u64;
    let mut flip: Option<(u64, u16, i64)> = None;
    if buy {
        // the hold clock: the size-weighted entry of the atoms held through the router
        let (old, add) = (p.tokens as i128, f.tokens as i128);
        p.entry_ts = if old == 0 { now } else { ((old * p.entry_ts as i128 + add * now as i128) / (old + add)) as i64 };
        p.tokens = p.tokens.saturating_add(f.tokens);
        p.cost = p.cost.saturating_add(f.sol).saturating_add(router_fee);
        p.bought = p.bought.saturating_add(f.sol).saturating_add(router_fee);
    } else {
        let net = f.sol - router_fee;
        p.sold = p.sold.saturating_add(net);
        // only atoms bought through the router count, at their average cost
        let tracked = f.tokens.min(p.tokens);
        if tracked > 0 {
            let cost_part = mul_div_u64(p.cost, tracked, p.tokens)?;
            let proceeds_part = mul_div_u64(net, tracked, f.tokens)?;
            // the flip fee: on the proceeds of routed atoms sold inside the window (never the
            // platform token); not part of the loss, so a receipt never pays it back
            let held = now - p.entry_ts;
            let fbps = if platform { 0 } else { b.config.ext.flip_bps(held) };
            if fbps > 0 {
                let fee = mul_div_u64(proceeds_part, fbps, BPS)?;
                if fee > 0 {
                    p.fees_paid = p.fees_paid.saturating_add(fee);
                    flip = Some((fee, fbps as u16, held));
                }
            }
            let p = &mut b.position;
            p.tokens -= tracked;
            p.cost -= cost_part;
            loss = cost_part.saturating_sub(proceeds_part);
            p.losses = p.losses.saturating_add(loss);
        }
    }
    if let Some((fee, bps, held)) = flip {
        let acc = b.flip.ok_or(SpotError::FlipAccount)?;
        send_wsol(b, acc, fee)?;
        emit!(FlipFee { owner: b.owner.key(), mint, quote: b.quote, amount: fee, bps, held_secs: held });
    }
    // a coin quoted in another mint: its loss in SOL at the quote's min TWAP (None without one)
    let mut qb = match &b.q {
        Some(q) => Some(load_book(q.book)?),
        None => None,
    };
    let loss_sol = match &qb {
        None => Some(loss),
        Some(book) => {
            let e = *b.config.quote(&b.quote).ok_or(SpotError::QuoteNotAllowed)?;
            match (e.flags & QUOTE_GRANTS != 0, loss > 0) {
                (true, true) => quote::price(book, &e, slot).map(|px| quote::lamports_for(loss, px)).transpose()?,
                _ => None,
            }
        }
    };
    let (value_sol, value_quote) = if !buy && loss > 0 && !platform { book_receipt(b, mint, loss, loss_sol, now, slot)? } else { (0, 0) };
    b.position.granted_value = b.position.granted_value.saturating_add(value_sol);
    b.config.totals.granted_value = b.config.totals.granted_value.saturating_add(value_sol);
    if let (Some(q), Some(qb)) = (&b.q, qb.as_mut()) {
        // a quote coin's trade totals are the quote's (Config's are lamports)
        let t = &mut qb.totals;
        if buy {
            t.buys += 1;
            t.quote_in = t.quote_in.saturating_add(f.sol);
        } else {
            t.sells += 1;
            t.quote_out = t.quote_out.saturating_add(f.sol);
        }
        t.router_fees = t.router_fees.saturating_add(router_fee);
        t.partner_fees = t.partner_fees.saturating_add(f.partner_fee);
        t.referral_paid = t.referral_paid.saturating_add(referral);
        t.losses = t.losses.saturating_add(loss);
        t.grant_quote = t.grant_quote.saturating_add(value_quote);
        if value_sol > 0 {
            t.valued_quote = t.valued_quote.saturating_add(value_quote);
            t.valued_lamports = t.valued_lamports.saturating_add(value_sol);
        } else {
            t.pending_quote = t.pending_quote.saturating_add(value_quote);
        }
        store(q.book, qb)?;
    } else {
        let c = &mut b.config.totals;
        if buy {
            c.buys += 1;
            c.sol_in = c.sol_in.saturating_add(f.sol);
        } else {
            c.sells += 1;
            c.sol_out = c.sol_out.saturating_add(f.sol);
        }
        c.router_fees = c.router_fees.saturating_add(router_fee);
        c.partner_fees = c.partner_fees.saturating_add(f.partner_fee);
        c.losses = c.losses.saturating_add(loss);
        b.config.referral_paid = b.config.referral_paid.saturating_add(referral);
    }
    emit!(Traded { owner: b.owner.key(), mint, venue, buy, sol: f.sol, tokens: f.tokens, router_fee, partner_fee: f.partner_fee, loss, grant_value: value_sol });
    if let (Some((_, referrer)), true) = (b.referrer, referral > 0) {
        emit!(Referred { owner: b.owner.key(), mint, referrer, amount: referral });
    }
    Ok(())
}

/// The loss receipt for a sell that realized `loss` (atoms of the quote; `loss_sol` its value in
/// lamports when known). Returns (V in lamports, V in atoms of the quote); (0, 0) without one.
fn book_receipt(b: &mut Book, mint: Pubkey, loss: u64, loss_sol: Option<u64>, now: i64, slot: u64) -> Result<(u64, u64)> {
    let Some(ra) = b.rc.as_ref() else { return Ok((0, 0)) };
    let rp = b.config.receipts;
    if rp.share_bps == 0 {
        return Ok((0, 0));
    }
    let cap = receipt::load_cap(ra.cap)?;
    let supply = receipt::mint_supply(ra.platform_mint)?;
    let h = receipt::h_ppm(now - b.position.entry_ts, rp.hold_secs);
    let d = receipt::d_ppm(receipt::room_left(&cap, supply), rp.room_ref_atoms);
    let value_quote = receipt::value(loss, rp.share_bps, h, d);
    let value_sol = loss_sol.map(|l| receipt::value(l, rp.share_bps, h, d));
    match value_sol {
        Some(v) if v == 0 || v < rp.min_value_lamports => return Ok((0, 0)),
        None if value_quote == 0 => return Ok((0, 0)),
        _ => {}
    }
    // a counted loss (L in SOL x h) under the minimum makes no receipt and charges no rent float
    // (owner 2026-10-23); a quote coin's loss without a quote price yet is not checked
    if let Some(l) = loss_sol {
        if (l as u128 * h as u128 / receipt::PPM as u128) < rp.min_loss_lamports as u128 {
            return Ok((0, 0));
        }
    }
    let owner = b.owner.key();
    let seq = b.trader.receipt_seq;
    let (rkey, rbump) = receipt::receipt_address(&owner, seq);
    let price = match value_sol {
        Some(_) => receipt::antidote_price(b.config, b.px_ring, slot),
        None => None,
    };
    let mut r = Receipt {
        owner,
        seq,
        number: 0,
        bump: rbump,
        status: RECEIPT_PENDING_PRICE,
        close_auth: false,
        coin: mint,
        quote: b.quote,
        loss,
        loss_sol: loss_sol.unwrap_or(0),
        share_bps: rp.share_bps,
        h_ppm: h,
        d_ppm: d,
        rate_ppm: receipt::PPM as u32,
        value_sol: value_sol.unwrap_or(0),
        amount: 0,
        amount_wanted: 0,
        price: price.unwrap_or(0),
        closed_at: now,
        delay_secs: 0,
        in_lane: false,
        redeemable_from: 0,
        auto_redeems_at: 0,
        issued_at: 0,
        attested: false,
        reserved: [0; 26],
    };
    let bump = b.config.authority_bump;
    if let (Some(v), Some(px)) = (value_sol, price) {
        let (amount, wanted, rate, bucket_after) = receipt::size(b.config, v, px, &cap, supply, now)?;
        // the bucket (or levcap's room) empty right now: the receipt waits for a price like one
        // booked without a price (`issue_receipt` sizes it once the bucket refills; red team
        // 2026-10-23, R2: it used to be dropped, so whoever emptied the bucket took every loss's
        // receipt until the refill)
        if amount > 0 {
            r.amount = amount;
            r.amount_wanted = wanted;
            r.rate_ppm = rate;
            r.status = RECEIPT_PENDING_NFT;
            receipt::reserve(ra.levcap_program, ra.authority, ra.cap, ra.platform_mint, &b.token_2022, amount, bump)?;
            receipt::take(b.config, &r, bucket_after, now);
        } else {
            r.price = 0;
        }
    }
    // the redeem delay (fast lane, or the curve on market cap and issuance: `receipt::delay_for`),
    // then the lane books this loss
    let mpx = receipt::mcap_price(b.config, b.px_ring, slot);
    let (delay, lane) = receipt::delay_for(&r, b.config, b.trader, now, mpx, supply);
    r.delay_secs = delay;
    r.in_lane = lane;
    r.redeemable_from = now.saturating_add(delay as i64);
    r.auto_redeems_at = r.redeemable_from;
    if let Some(c) = receipt::counted_loss(&r) {
        receipt::lane_record(b.trader, rp.lane_window_secs, c, now);
    }
    // the receipt account: the seller pays its rent and, in advance, its NFT's (made by
    // `issue_receipt` right after: the NFT does not fit a sell's compute or heap budget)
    let owner_ai = b.owner.to_account_info();
    let funder = receipt::Funder::Signer(&owner_ai);
    let space = 8 + Receipt::INIT_SPACE;
    let lamports = Rent::get()?.minimum_balance(space) + receipt::nft_rent_bound(rp.uri_str(), false)?;
    receipt::make_account(ra.receipt, &funder, &b.system, space, lamports, &crate::ID, &[RECEIPT_SEED, owner.as_ref(), &seq.to_le_bytes(), &[rbump]])?;
    b.trader.receipt_seq = seq + 1;
    b.config.receipt_book.pending += 1;
    receipt::store_receipt(ra.receipt, &r)?;
    let book = &mut b.config.receipt_book;
    book.value_lamports = book.value_lamports.saturating_add(r.value_sol);
    b.trader.granted_atoms = b.trader.granted_atoms.saturating_add(r.amount);
    emit!(receipt::ReceiptBooked {
        owner,
        receipt: rkey,
        coin: mint,
        quote: r.quote,
        status: r.status,
        loss,
        loss_sol: r.loss_sol,
        share_bps: r.share_bps,
        h_ppm: h,
        d_ppm: d,
        rate_ppm: r.rate_ppm,
        value_sol: r.value_sol,
        amount: r.amount,
        price: r.price,
        closed_at: now,
        delay_secs: r.delay_secs,
        in_lane: r.in_lane,
        redeemable_from: r.redeemable_from,
        auto_redeems_at: r.auto_redeems_at,
    });
    Ok((r.value_sol, value_quote))
}

impl ParamsExt {
    pub fn validate(&self) -> Result<()> {
        require!(
            self.partner_fee_count_bps as u64 <= BPS
                && self.referral_bps <= HARD_MAX_REFERRAL_BPS
                && self.platform_router_fee_bps <= HARD_MAX_ROUTER_FEE_BPS
                && self.flip_fee_bps <= HARD_MAX_FLIP_FEE_BPS
                && self.flip_window_secs <= HARD_MAX_FLIP_WINDOW_SECS
                && self.reserved == [0; 20],
            SpotError::BadParam
        );
        Ok(())
    }
}

impl Params {
    pub fn validate(&self) -> Result<()> {
        let b = BPS as u16;
        require!(self.router_fee_bps <= HARD_MAX_ROUTER_FEE_BPS, SpotError::BadParam);
        require!(self.grant_bps <= b && self.grant_weight_bps <= b, SpotError::BadParam);
        require!(self.lock_secs <= HARD_MAX_LOCK && self.vest_secs <= HARD_MAX_LOCK, SpotError::BadParam);
        require!(self.timelock_secs <= HARD_MAX_TIMELOCK, SpotError::BadParam);
        self.price.validate()?;
        self.budget.validate()
    }
}

#[program]
pub mod tilt_spot {
    use super::*;

    /// The program's upgrade authority, once.
    pub fn initialize(ctx: Context<Initialize>, params: Params) -> Result<()> {
        params.validate()?;
        let c = &mut ctx.accounts.config;
        c.admin = ctx.accounts.admin.key();
        c.bump = ctx.bumps.config;
        c.vault_bump = ctx.bumps.vault;
        c.authority_bump = Pubkey::find_program_address(&[AUTHORITY_SEED], &crate::ID).1;
        c.params = params;
        c.ext = ParamsExt::DEFAULT;
        c.receipts = ReceiptParams::DEFAULT;
        Ok(())
    }

    /// Anyone (the payer funds the rent): grow a `Config` created before `ParamsExt` was appended
    /// to its current size and set `ext` to its defaults. Nothing else moves. Run it right after
    /// the program upgrade that adds `ext`; until then the other instructions cannot load the
    /// config. Refused once the account is already full size.
    pub fn grow_config(ctx: Context<GrowConfig>) -> Result<()> {
        let cfg = ctx.accounts.config.to_account_info();
        let new_len = 8 + Config::INIT_SPACE;
        let old_len = {
            let d = cfg.try_borrow_data()?;
            require!(d.len() >= 8 && d[..8] == *Config::DISCRIMINATOR, SpotError::WrongAccount);
            require!(d.len() < new_len, SpotError::NotPending);
            d.len()
        };
        let need = Rent::get()?.minimum_balance(new_len).saturating_sub(cfg.lamports());
        if need > 0 {
            system_program::transfer(
                CpiContext::new(ctx.accounts.system_program.to_account_info(), system_program::Transfer { from: ctx.accounts.payer.to_account_info(), to: cfg.clone() }),
                need,
            )?;
        }
        cfg.realloc(new_len, true)?;
        // defaults only for the fields the account did not have: a config that already holds
        // `ext` (grown for a later append) keeps its values; new bytes are zero otherwise
        let mut d = cfg.try_borrow_mut_data()?;
        let at = state::config_offsets::EXT;
        if old_len <= at {
            let ext = ParamsExt::DEFAULT.try_to_vec()?;
            d[at..at + ext.len()].copy_from_slice(&ext);
        }
        let at = state::config_offsets::RECEIPTS;
        if old_len <= at {
            let rp = ReceiptParams::DEFAULT.try_to_vec()?;
            d[at..at + rp.len()].copy_from_slice(&rp);
        }
        Ok(())
    }

    /// Admin: new `ParamsExt` (the grant cap's base), at once when the timelock is 0, else after it
    /// (`apply_params_ext`; `cancel_params` drops it too).
    pub fn set_params_ext(ctx: Context<Admin>, ext: ParamsExt) -> Result<()> {
        ext.validate()?;
        let c = &mut ctx.accounts.config;
        let now = Clock::get()?.unix_timestamp;
        if c.params.timelock_secs == 0 {
            c.ext = ext;
            c.pending_ext_eta = 0;
            emit!(ParamsExtSet { eta: now, ext });
        } else {
            c.pending_ext = ext;
            c.pending_ext_eta = now + c.params.timelock_secs as i64;
            emit!(ParamsExtSet { eta: c.pending_ext_eta, ext });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending `ParamsExt`.
    pub fn apply_params_ext(ctx: Context<ConfigOnly>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(c.pending_ext_eta != 0 && Clock::get()?.unix_timestamp >= c.pending_ext_eta, SpotError::NotPending);
        c.ext = c.pending_ext;
        c.pending_ext_eta = 0;
        Ok(())
    }

    /// Admin: new parameters, at once when the current timelock is 0, else after it.
    pub fn set_params(ctx: Context<Admin>, params: Params) -> Result<()> {
        params.validate()?;
        let c = &mut ctx.accounts.config;
        let now = Clock::get()?.unix_timestamp;
        if c.params.timelock_secs == 0 {
            c.params = params;
            c.pending_eta = 0;
            emit!(ParamsSet { eta: now, params });
        } else {
            *c.pending = params;
            c.pending_eta = now + c.params.timelock_secs as i64;
            emit!(ParamsSet { eta: c.pending_eta, params });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending parameters.
    pub fn apply_params(ctx: Context<ConfigOnly>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(c.pending_eta != 0 && Clock::get()?.unix_timestamp >= c.pending_eta, SpotError::NotPending);
        c.params = *c.pending;
        c.pending_eta = 0;
        Ok(())
    }

    pub fn cancel_params(ctx: Context<Admin>) -> Result<()> {
        ctx.accounts.config.pending_eta = 0;
        ctx.accounts.config.pending_ext_eta = 0;
        ctx.accounts.config.pending_quote_eta = 0;
        ctx.accounts.config.pending_receipts_eta = 0;
        Ok(())
    }

    /// Admin: new loss-receipt parameters within the hard caps, at once when the timelock is 0,
    /// else after it (`apply_receipt_params`; `cancel_params` drops it).
    pub fn set_receipt_params(ctx: Context<Admin>, receipts: ReceiptParams) -> Result<()> {
        receipts.validate()?;
        let c = &mut ctx.accounts.config;
        let now = Clock::get()?.unix_timestamp;
        if c.params.timelock_secs == 0 {
            c.receipts = receipts;
            c.pending_receipts_eta = 0;
            emit!(receipt::ReceiptParamsSet { eta: now, params: receipts });
        } else {
            c.pending_receipts = receipts;
            c.pending_receipts_eta = now + c.params.timelock_secs as i64;
            emit!(receipt::ReceiptParamsSet { eta: c.pending_receipts_eta, params: receipts });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending receipt parameters.
    pub fn apply_receipt_params(ctx: Context<ConfigOnly>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(c.pending_receipts_eta != 0 && Clock::get()?.unix_timestamp >= c.pending_receipts_eta, SpotError::NotPending);
        c.receipts = c.pending_receipts;
        c.pending_receipts_eta = 0;
        Ok(())
    }

    /// Admin: put `entry` in quote slot `slot` (an empty entry clears it), at once when the
    /// timelock is 0, else after it (`apply_quote`; `cancel_params` drops it). A mint may sit in
    /// one slot only. Clearing a slot stops routed trades of its coins: take their configs out of
    /// `dbc_configs` first, which opens their curves to every route (levpad-hook).
    pub fn set_quote(ctx: Context<Admin>, slot: u8, entry: QuoteEntry) -> Result<()> {
        entry.validate()?;
        require!((slot as usize) < QUOTES, SpotError::BadParam);
        let c = &mut ctx.accounts.config;
        require!(entry.mint == Pubkey::default() || c.quotes.entries.iter().enumerate().all(|(i, q)| i == slot as usize || q.mint != entry.mint), SpotError::BadParam);
        let now = Clock::get()?.unix_timestamp;
        if c.params.timelock_secs == 0 {
            c.quotes.entries[slot as usize] = entry;
            c.pending_quote_eta = 0;
            emit!(QuoteSet { eta: now, slot, entry });
        } else {
            c.pending_quote = entry;
            c.pending_quote_slot = slot;
            c.pending_quote_eta = now + c.params.timelock_secs as i64;
            emit!(QuoteSet { eta: c.pending_quote_eta, slot, entry });
        }
        Ok(())
    }

    /// Anyone, after the timelock: apply the pending `set_quote`.
    pub fn apply_quote(ctx: Context<ConfigOnly>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(c.pending_quote_eta != 0 && Clock::get()?.unix_timestamp >= c.pending_quote_eta, SpotError::NotPending);
        let (slot, entry) = (c.pending_quote_slot as usize, c.pending_quote);
        require!(entry.mint == Pubkey::default() || c.quotes.entries.iter().enumerate().all(|(i, q)| i == slot || q.mint != entry.mint), SpotError::BadParam);
        c.quotes.entries[slot] = entry;
        c.pending_quote_eta = 0;
        Ok(())
    }

    /// Admin, at once: stop valuing losses on a quote's coins in SOL (an issuer pause, a broken
    /// price pool). Their receipts wait for a price (`issue_receipt`) until a `set_quote` turns it
    /// back on.
    pub fn disable_quote_grants(ctx: Context<Admin>, slot: u8) -> Result<()> {
        require!((slot as usize) < QUOTES, SpotError::BadParam);
        ctx.accounts.config.quotes.entries[slot as usize].flags &= !QUOTE_GRANTS;
        Ok(())
    }

    /// Anyone (the payer funds the rent): create an allowed quote's `QuoteBook`.
    pub fn init_quote_book(ctx: Context<InitQuoteBook>) -> Result<()> {
        let mint = ctx.accounts.quote_mint.key();
        require!(ctx.accounts.config.quote(&mint).is_some(), SpotError::QuoteNotAllowed);
        let b = &mut ctx.accounts.book;
        b.mint = mint;
        b.bump = ctx.bumps.book;
        Ok(())
    }

    /// Anyone: sample a quote's SOL price from its leg pools (leg 1 only for two-leg quotes).
    /// Fails when no sample was due.
    pub fn sample_quote(ctx: Context<SampleQuote>) -> Result<()> {
        let a = ctx.accounts;
        let e = *a.config.quote(&a.book.mint).ok_or(SpotError::QuoteNotAllowed)?;
        let spot = quote::spot(&e, &a.leg0, a.leg1.as_ref().map(|x| x.as_ref()))?;
        let clock = Clock::get()?;
        require!(quote::sample(&mut a.book, &e, spot, clock.slot, clock.unix_timestamp)?, SpotError::NotPending);
        Ok(())
    }

    pub fn set_admin(ctx: Context<Admin>, admin: Pubkey) -> Result<()> {
        ctx.accounts.config.pending_admin = admin;
        Ok(())
    }

    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        c.admin = c.pending_admin;
        c.pending_admin = Pubkey::default();
        Ok(())
    }

    /// Admin, at once: stop or resume routed trading. Receipts go on issuing and redeeming.
    pub fn set_paused(ctx: Context<Admin>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    /// Admin: the platform token, a levcap-registered Token-2022 mint whose minter is this
    /// program's ["authority"] PDA. Remaining account 0, optional: the token's DBC pool; its price
    /// then x the supply sets the redeem delay's `mcap_ref_lamports` (the launch market cap). Creates nothing: the authority's associated Token-2022
    /// account (the stake vault until 2026-10-23, inert now) must exist.
    ///
    /// Hot-swap (owner 2026-10-28): the admin may set it again to another levcap token, starting it
    /// fresh: refused while any receipt is owed or waiting (`ReceiptsOpen`; pause and let the keeper
    /// redeem first), the new token must have 6 decimals and its DBC pool must be passed; the price
    /// book and the issuance counters restart. levfee follows with `sync_platform_token` (send it in
    /// the same transaction) and antidote-ideas with `set_config(new_mint)`.
    pub fn set_platform_token<'info>(ctx: Context<'_, '_, '_, 'info, SetPlatformToken<'info>>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        let swap_from = c.token_mint;
        let swapping = swap_from != Pubkey::default() && swap_from != ctx.accounts.token_mint.key();
        if swapping {
            // a swap starts the new token fresh (owner 2026-10-28; red team v2 H1, M1, L1, L2): nothing
            // owed or waiting on receipts crosses tokens, the new token has the same 6 decimals, and its
            // DBC pool is passed so the delay's market-cap reference is the new token's own
            let b = &c.receipt_book;
            require!(b.owed == 0 && b.pending == 0, SpotError::ReceiptsOpen);
            require!(!ctx.remaining_accounts.is_empty(), SpotError::WrongAccount);
            let d = ctx.accounts.token_mint.try_borrow_data()?;
            require!(d.len() >= 82 && d[44] == 6, SpotError::WrongAccount);
        }
        let cap = &ctx.accounts.levcap_cap;
        let mint = &ctx.accounts.token_mint;
        require_keys_eq!(cap.minter, ctx.accounts.authority.key(), SpotError::NotOurMinter);
        require_keys_eq!(cap.mint, mint.key(), SpotError::NotOurMinter);
        {
            // Token-2022 mint: COption tag + mint authority at 0..36
            let d = mint.try_borrow_data()?;
            let auth = levcap::mint_auth_address(&mint.key()).0;
            require!(d.len() >= 82 && d[0..4] == [1, 0, 0, 0] && d[4..36] == auth.to_bytes(), SpotError::NotOurMinter);
        }
        let vault = &ctx.accounts.stake_vault;
        let want = anchor_spl::associated_token::get_associated_token_address_with_program_id(ctx.accounts.authority.key, &mint.key(), &TOKEN_2022);
        require_keys_eq!(vault.key(), want, SpotError::WrongAccount);
        require!(*vault.owner == TOKEN_2022 && vault.data_len() >= 165, SpotError::WrongAccount);
        c.mint_room = cap.lifetime_cap.saturating_sub(cap.minted_total);
        c.levcap_cap = cap.key();
        c.stake_vault = vault.key();
        c.token_mint = mint.key();
        *c.px = PxBook::default();
        if swapping {
            // the new token's issuance starts from zero: its first receipts wait the shortest time and
            // its bucket starts full
            c.receipt_book.atoms_reserved = 0;
            c.receipt_book.bucket_atoms = 0;
            c.receipt_book.bucket_ts = 0;
        }
        // the launch market cap, from the token's DBC pool when passed
        if let Some(pool) = ctx.remaining_accounts.first() {
            let d = pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(pool.owner, &d, &mint.key()), SpotError::WrongAccount);
            let sqrt = venue::u128_at(&d, venue::dbc::SQRT_PRICE);
            let px = m(levpad_math::price_from_sqrt_q64(sqrt))?;
            let supply = receipt::mint_supply(mint)?;
            let mcap = m(mul_div(px, supply as u128, levpad_math::Q64))?;
            c.receipts.mcap_ref_lamports = u64::try_from(mcap).unwrap_or(u64::MAX);
        }
        emit!(PlatformTokenSet { from: swap_from, to: c.token_mint });
        Ok(())
    }

    /// Buy (`buy`, `amount_in` lamports of WSOL, the router fee taken first) or sell (`amount_in`
    /// atoms, the router fee taken from the SOL out) a coin on its DBC curve. `min_out` is DBC's:
    /// atoms out, or lamports out before the router fee.
    ///
    /// Fair open: a gated coin (levpad-hook meta list v2) also takes, as remaining accounts, the
    /// instructions sysvar, the coin's gate ["gate", mint] under levpad-hook (writable) and the
    /// hook's GateConfig ["gate_config"]; DBC gets them after the ring in the meta list's order
    /// (sysvar, gate, this program's config, GateConfig), a hook slice of 8 accounts instead of 4.
    /// A v1 coin takes none.
    ///
    /// Referral: an optional writable WSOL account (SPL Token) after those (remaining account 0 on
    /// a v1 coin, 3 on a gated one). Its owner is the referrer and gets `ext.referral_bps` of the
    /// router fee in the same trade (`Referred`); a wallet naming itself gets nothing.
    ///
    /// The platform token routes too (its config need not be counted); it earns no grants.
    ///
    /// Multi-quote: a coin whose DBC config quotes another mint than WSOL (an allowed quote in
    /// `Config::quotes`) trades in that mint: `quote_mint` is it, `token_program` its program,
    /// `user_wsol` and `fee_wsol` the wallet's and levfee's (the fee PDA's associated) accounts of
    /// it, and two more remaining accounts come right after the fair-open ones: the quote's
    /// `QuoteBook` ["quote", mint] and the wallet's `QuotePending` ["qpend", owner, mint] (both
    /// writable; the second may not exist yet). The referrer, if any, follows them and holds the
    /// quote. Amounts (`amount_in`, `min_out`, the position) are in atoms of the quote.
    ///
    /// Buys swap in DBC's partial-fill mode: `amount_in` less the router fee on all of it goes
    /// in, the fee is charged on what the swap spent (`buy_fee`), and what the curve could not
    /// take (the buy that completes the curve) stays in the wallet's WSOL.
    pub fn trade_dbc<'info>(ctx: Context<'_, '_, '_, 'info, TradeDbc<'info>>, buy: bool, amount_in: u64, min_out: u64) -> Result<()> {
        require!(amount_in > 0, SpotError::BadParam);
        let a = &ctx.accounts;
        let mint = a.base_mint.key();
        {
            let d = a.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(a.dbc_pool.owner, &d, &mint), SpotError::WrongAccount);
            let cfg_key = venue::key_at(&d, DBC_POOL_CONFIG);
            require_keys_eq!(cfg_key, a.dbc_config.key(), SpotError::WrongAccount);
            let counted = cfg_key != Pubkey::default() && a.config.params.dbc_configs.contains(&cfg_key);
            require!(counted || mint == a.config.token_mint, SpotError::NotPadCoin);
        }
        // the coin's quote: its DBC config's quote mint (WSOL, or an allowed quote)
        let quote = {
            require_keys_eq!(*a.dbc_config.owner, DBC_PROGRAM, SpotError::WrongAccount);
            let d = a.dbc_config.try_borrow_data()?;
            require!(d.len() >= DBC_CONFIG_QUOTE_MINT + 32, SpotError::WrongAccount);
            venue::key_at(&d, DBC_CONFIG_QUOTE_MINT)
        };
        require_keys_eq!(a.quote_mint.key(), quote, SpotError::WrongAccount);
        // the coin's own hook accounts: another meta list would make Token-2022 skip the ring
        require_keys_eq!(a.meta_list.key(), levpad_hook::meta_list_address(&mint).0, SpotError::WrongAccount);
        require_keys_eq!(a.ring.key(), levpad_hook::ring_address(&mint).0, SpotError::WrongAccount);
        // fair open: a gated coin's hook needs two more accounts (Token-2022 would refuse anyway)
        let gated = levpad_hook::meta_list_count(a.meta_list.owner, &a.meta_list.try_borrow_data()?) as usize == levpad_hook::META_COUNT_V2;
        let gate_accs: &[AccountInfo<'info>] = if gated {
            let r = ctx.remaining_accounts;
            require!(r.len() >= 3, SpotError::WrongAccount);
            require_keys_eq!(r[0].key(), anchor_lang::solana_program::sysvar::instructions::ID, SpotError::WrongAccount);
            require_keys_eq!(r[1].key(), levpad_hook::gate_address(&mint).0, SpotError::WrongAccount);
            require_keys_eq!(r[2].key(), levpad_hook::gate::GATE_CONFIG, SpotError::WrongAccount);
            &r[..3]
        } else {
            &[]
        };
        let qprog = a.token_program.key();
        let (q, ref_at) = quote_accs(&a.config, ctx.remaining_accounts, gate_accs.len(), &a.owner.key(), &quote, a.system_program.to_account_info())?;
        let (flip, referrer, rc) = tail_accs(&a.config, ctx.remaining_accounts, ref_at, &a.owner.key(), &mint, &quote, &qprog, a.trader.receipt_seq)?;
        let quote_decimals = mint_decimals(&a.quote_mint)?;
        let (clock, bumps) = (Clock::get()?, (ctx.bumps.position, ctx.bumps.trader));
        let a = ctx.accounts;
        let config_ai = a.config.to_account_info();
        let user_token = a.user_token.to_account_info();
        let (user_wsol, fee_wsol, wsol_mint, token_program) = (a.user_wsol.to_account_info(), a.fee_wsol.to_account_info(), a.quote_mint.to_account_info(), a.token_program.to_account_info());
        let mut b = Book {
            owner: &a.owner,
            config: &mut a.config,
            position: &mut a.position,
            trader: &mut a.trader,
            user_wsol: &user_wsol,
            fee_wsol: &fee_wsol,
            wsol_mint: &wsol_mint,
            token_program: &token_program,
            referrer,
            quote,
            quote_decimals,
            q,
            rc,
            flip,
            px_ring: a.px_ring.as_ref().map(|x| x.as_ref()),
            token_2022: a.token_2022_program.to_account_info(),
            system: a.system_program.to_account_info(),
        };
        open_book(&mut b, &mint, &user_token, bumps.0, bumps.1)?;
        refresh(b.config, b.px_ring, clock.slot)?;
        let fee_bps = router_fee_bps(b.config, &mint);
        // a buy swaps all but the largest fee it can owe, in DBC's partial-fill mode (the buy that
        // completes the curve takes only what the curve has left); the fee is charged after, on
        // what was spent, and the unspent WSOL stays with the wallet
        let max_fee = if buy { mul_div_u64(amount_in, fee_bps, BPS)? } else { 0 };
        let swap_in = amount_in - max_fee;
        let partner = |ai: &AccountInfo| -> Result<u64> { Ok(venue::u64_at(&ai.try_borrow_data()?, DBC_PARTNER_QUOTE_FEE)) };
        let (w0, t0, p0) = (amount(b.user_wsol)?, amount(&user_token)?, partner(a.dbc_pool.as_ref())?);
        let (input, output) = if buy { (b.user_wsol.key(), user_token.key()) } else { (user_token.key(), b.user_wsol.key()) };
        let mut data = disc("swap2_with_transfer_hook").to_vec();
        data.extend_from_slice(&swap_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.push(if buy { 1 } else { 0 }); // PartialFill for buys, ExactIn for sells
        // one slice: TransferHookBase, 4 accounts (v1) or 8 (gated: + sysvar, gate, this config,
        // the hook's GateConfig, in the meta list's order)
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[0, if gated { 8 } else { 4 }]);
        let mut ix = Instruction {
            program_id: DBC_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.dbc_pool.key(), false),
                AccountMeta::new(input, false),
                AccountMeta::new(output, false),
                AccountMeta::new(a.base_vault.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new(mint, false),
                AccountMeta::new_readonly(quote, false),
                AccountMeta::new_readonly(b.owner.key(), true),
                AccountMeta::new_readonly(TOKEN_2022, false),
                AccountMeta::new_readonly(qprog, false),
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
        if gated {
            ix.accounts.push(AccountMeta::new_readonly(gate_accs[0].key(), false));
            ix.accounts.push(AccountMeta::new(gate_accs[1].key(), false));
            ix.accounts.push(AccountMeta::new_readonly(config_ai.key(), false));
            ix.accounts.push(AccountMeta::new_readonly(gate_accs[2].key(), false));
        }
        let mut infos = vec![
            a.dbc_pool_authority.to_account_info(),
            a.dbc_config.to_account_info(),
            a.dbc_pool.to_account_info(),
            b.user_wsol.clone(),
            user_token.clone(),
            a.base_vault.to_account_info(),
            a.quote_vault.to_account_info(),
            a.base_mint.to_account_info(),
            a.quote_mint.to_account_info(),
            b.owner.to_account_info(),
            a.token_2022_program.to_account_info(),
            a.token_program.to_account_info(),
            a.dbc_event_authority.to_account_info(),
            a.dbc_program.to_account_info(),
            a.meta_list.to_account_info(),
            a.hook_program.to_account_info(),
            a.ring.to_account_info(),
        ];
        infos.extend_from_slice(gate_accs);
        if gated {
            infos.push(config_ai.clone());
        }
        invoke(&ix, &infos)?;
        let (w1, t1, p1) = (amount(b.user_wsol)?, amount(&user_token)?, partner(a.dbc_pool.as_ref())?);
        let partner_fee = p1.saturating_sub(p0);
        let f = if buy {
            Fill { sol: w0.checked_sub(w1).ok_or(SpotError::Math)?, tokens: t1.checked_sub(t0).ok_or(SpotError::Math)?, partner_fee }
        } else {
            Fill { sol: w1.checked_sub(w0).ok_or(SpotError::Math)?, tokens: t0.checked_sub(t1).ok_or(SpotError::Math)?, partner_fee }
        };
        require!(f.sol > 0 && f.tokens > 0, SpotError::NoFill);
        let router_fee = if buy { buy_fee(f.sol, fee_bps, max_fee)? } else { mul_div_u64(f.sol, fee_bps, BPS)? };
        let referral = pay_router_fee(&b, router_fee)?;
        book(&mut b, mint, VENUE_DBC, buy, &f, router_fee, referral, clock.unix_timestamp, clock.slot)
    }

    /// Buy or sell a graduated coin on the DAMM v2 pool levpad-hook bound to it. As `trade_dbc`;
    /// DAMM v2's LP fee is not counted toward the cap (only the router fee is). The optional
    /// referrer WSOL account is remaining account 0. A coin quoted in another mint takes its
    /// `QuoteBook` and `QuotePending` as remaining accounts 0 and 1 (then the referrer).
    pub fn trade_damm<'info>(ctx: Context<'_, '_, '_, 'info, TradeDamm<'info>>, buy: bool, amount_in: u64, min_out: u64) -> Result<()> {
        require!(amount_in > 0, SpotError::BadParam);
        let a = &ctx.accounts;
        let mint = a.base_mint.key();
        let base_is_a = {
            require_keys_eq!(a.damm_link.key(), levpad_hook::damm_link_address(&mint).0, SpotError::WrongAccount);
            require_keys_eq!(*a.damm_link.owner, levpad_hook::ID, SpotError::WrongAccount);
            let d = a.damm_link.try_borrow_data()?;
            let link = levpad_hook::DammLink::try_deserialize(&mut &d[..])?;
            require!(link.mint == mint && link.damm_pool == a.damm_pool.key(), SpotError::NotPadCoin);
            // the coin graduated from a counted config: its DBC pool outlives the migration
            let d = a.dbc_pool.try_borrow_data()?;
            require!(venue::is_dbc_pool(a.dbc_pool.owner, &d, &mint), SpotError::WrongAccount);
            let cfg_key = venue::key_at(&d, DBC_POOL_CONFIG);
            let counted = cfg_key != Pubkey::default() && a.config.params.dbc_configs.contains(&cfg_key);
            require!(counted || mint == a.config.token_mint, SpotError::NotPadCoin);
            require_keys_eq!(*a.damm_pool.owner, DAMM_V2_PROGRAM, SpotError::WrongAccount);
            let p = a.damm_pool.try_borrow_data()?;
            require!(p.len() >= damm::POOL_LEN && p[..8] == damm::POOL_DISC, SpotError::WrongAccount);
            let (ka, kb) = (venue::key_at(&p, damm::TOKEN_A_MINT), venue::key_at(&p, damm::TOKEN_B_MINT));
            // SOL coins either way round (as before); other quotes as DBC migrates them: coin = A
            let quote = if ka == mint { kb } else { ka };
            require!((ka == mint && kb == quote) || (quote == WSOL && kb == mint), SpotError::WrongAccount);
            (ka == mint, quote)
        };
        let (base_is_a, quote) = base_is_a;
        require_keys_eq!(a.quote_mint.key(), quote, SpotError::WrongAccount);
        let qprog = a.token_program.key();
        let (q, ref_at) = quote_accs(&a.config, ctx.remaining_accounts, 0, &a.owner.key(), &quote, a.system_program.to_account_info())?;
        let (flip, referrer, rc) = tail_accs(&a.config, ctx.remaining_accounts, ref_at, &a.owner.key(), &mint, &quote, &qprog, a.trader.receipt_seq)?;
        let quote_decimals = mint_decimals(&a.quote_mint)?;
        let (clock, bumps) = (Clock::get()?, (ctx.bumps.position, ctx.bumps.trader));
        let a = ctx.accounts;
        let user_token = a.user_token.to_account_info();
        let (user_wsol, fee_wsol, wsol_mint, token_program) = (a.user_wsol.to_account_info(), a.fee_wsol.to_account_info(), a.quote_mint.to_account_info(), a.token_program.to_account_info());
        let mut b = Book {
            owner: &a.owner,
            config: &mut a.config,
            position: &mut a.position,
            trader: &mut a.trader,
            user_wsol: &user_wsol,
            fee_wsol: &fee_wsol,
            wsol_mint: &wsol_mint,
            token_program: &token_program,
            referrer,
            quote,
            quote_decimals,
            q,
            rc,
            flip,
            px_ring: a.px_ring.as_ref().map(|x| x.as_ref()),
            token_2022: a.token_2022_program.to_account_info(),
            system: a.system_program.to_account_info(),
        };
        open_book(&mut b, &mint, &user_token, bumps.0, bumps.1)?;
        refresh(b.config, b.px_ring, clock.slot)?;
        let fee_bps = router_fee_bps(b.config, &mint);
        let (mut router_fee, mut referral) = (0, 0);
        let swap_in = if buy {
            router_fee = mul_div_u64(amount_in, fee_bps, BPS)?;
            referral = pay_router_fee(&b, router_fee)?;
            amount_in - router_fee
        } else {
            amount_in
        };
        let (w0, t0) = (amount(b.user_wsol)?, amount(&user_token)?);
        let (input, output) = if buy { (b.user_wsol.key(), user_token.key()) } else { (user_token.key(), b.user_wsol.key()) };
        let (mint_a, mint_b, prog_a, prog_b) = if base_is_a { (mint, quote, TOKEN_2022, qprog) } else { (quote, mint, qprog, TOKEN_2022) };
        let mut data = disc("swap").to_vec();
        data.extend_from_slice(&swap_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        let ix = Instruction {
            program_id: DAMM_V2_PROGRAM,
            accounts: vec![
                AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                AccountMeta::new(a.damm_pool.key(), false),
                AccountMeta::new(input, false),
                AccountMeta::new(output, false),
                AccountMeta::new(a.token_a_vault.key(), false),
                AccountMeta::new(a.token_b_vault.key(), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new_readonly(b.owner.key(), true),
                AccountMeta::new_readonly(prog_a, false),
                AccountMeta::new_readonly(prog_b, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false), // no referral
                AccountMeta::new_readonly(DAMM_EVENT_AUTHORITY, false),
                AccountMeta::new_readonly(DAMM_V2_PROGRAM, false),
            ],
            data,
        };
        invoke(
            &ix,
            &[
                a.damm_pool_authority.to_account_info(),
                a.damm_pool.to_account_info(),
                b.user_wsol.clone(),
                user_token.clone(),
                a.token_a_vault.to_account_info(),
                a.token_b_vault.to_account_info(),
                a.base_mint.to_account_info(),
                a.quote_mint.to_account_info(),
                b.owner.to_account_info(),
                a.token_2022_program.to_account_info(),
                a.token_program.to_account_info(),
                a.damm_event_authority.to_account_info(),
                a.damm_program.to_account_info(),
            ],
        )?;
        let (w1, t1) = (amount(b.user_wsol)?, amount(&user_token)?);
        let f = if buy {
            Fill { sol: w0.checked_sub(w1).ok_or(SpotError::Math)?, tokens: t1.checked_sub(t0).ok_or(SpotError::Math)?, partner_fee: 0 }
        } else {
            Fill { sol: w1.checked_sub(w0).ok_or(SpotError::Math)?, tokens: t0.checked_sub(t1).ok_or(SpotError::Math)?, partner_fee: 0 }
        };
        require!(f.sol > 0 && f.tokens > 0, SpotError::NoFill);
        if !buy {
            router_fee = mul_div_u64(f.sol, fee_bps, BPS)?;
            referral = pay_router_fee(&b, router_fee)?;
        }
        book(&mut b, mint, VENUE_DAMM, buy, &f, router_fee, referral, clock.unix_timestamp, clock.slot)
    }

    /// The owner closes its book on a coin and takes the rent back. The atoms tracked and the
    /// eligible fees on it are forgotten: routed atoms it still holds count as untracked.
    pub fn close_position(_ctx: Context<ClosePosition>) -> Result<()> {
        Ok(())
    }

    /// Anyone: sample the platform token's price ring (fails when no sample was due).
    pub fn sample_price(ctx: Context<SamplePrice>) -> Result<()> {
        let slot = Clock::get()?.slot;
        require!(price::sample(&mut ctx.accounts.config, &ctx.accounts.ring, slot)?, SpotError::NotPending);
        Ok(())
    }

    /// Anyone: make a booked receipt's NFT from the rent the seller prefunded (the app sends it
    /// signed by the owner with the sell; the keeper sends it within seconds otherwise); first
    /// price it when it waits for a price (a quote coin's loss at the quote's min TWAP, then
    /// Antidote at max(spot, TWAP)), reserving its amount at levcap through the issuance bucket.
    /// When the owner signs, the token account's close authority becomes the ["authority"] PDA
    /// and redemption closes it; otherwise redemption leaves it, empty, for the owner to close.
    /// Remaining account 0 (optional, 2026-10-26): the receipt's coin mint, whose Token-2022
    /// metadata symbol becomes the NFT's `ticker` field and the picture's ticker (without it the
    /// picture prints the mint's first and last four characters).
    pub fn issue_receipt<'info>(ctx: Context<'_, '_, '_, 'info, IssueReceipt<'info>>) -> Result<()> {
        let clock = Clock::get()?;
        let coin_ai = ctx.remaining_accounts.first().cloned();
        let now = clock.unix_timestamp;
        let a = ctx.accounts;
        refresh(&mut a.config, a.px_ring.as_ref().map(|x| x.as_ref()), clock.slot)?;
        let mut r: Receipt = (**a.receipt).clone();
        require!(r.status != RECEIPT_ISSUED, SpotError::NotPending);
        let bump = a.config.authority_bump;
        let t22 = a.token_2022_program.to_account_info();
        let cap_ai = a.levcap_cap.to_account_info();
        let mint_ai = a.token_mint.to_account_info();
        if r.status == RECEIPT_PENDING_PRICE {
            if r.loss_sol == 0 {
                // a quote coin's loss, valued in SOL now
                let e = *a.config.quote(&r.quote).ok_or(SpotError::QuoteNotAllowed)?;
                require!(e.flags & QUOTE_GRANTS != 0, SpotError::NoQuotePrice);
                let bk = a.quote_book.as_ref().ok_or(SpotError::NoQuotePrice)?;
                require_keys_eq!(bk.key(), Pubkey::find_program_address(&[QUOTE_BOOK_SEED, r.quote.as_ref()], &crate::ID).0, SpotError::WrongAccount);
                let book = load_book(bk)?;
                let px = quote::price(&book, &e, clock.slot).ok_or(SpotError::NoQuotePrice)?;
                r.loss_sol = quote::lamports_for(r.loss, px)?;
                r.value_sol = receipt::value(r.loss_sol, r.share_bps, r.h_ppm, r.d_ppm);
                require!(r.value_sol > 0, SpotError::NoQuotePrice);
                a.config.totals.granted_value = a.config.totals.granted_value.saturating_add(r.value_sol);
                a.config.receipt_book.value_lamports = a.config.receipt_book.value_lamports.saturating_add(r.value_sol);
            }
            let px = receipt::antidote_price(&a.config, a.px_ring.as_ref().map(|x| x.as_ref()), clock.slot).ok_or(SpotError::NoPrice)?;
            let cap = receipt::load_cap(&cap_ai)?;
            let supply = receipt::mint_supply(&mint_ai)?;
            let (amount, wanted, rate, bucket_after) = receipt::size(&a.config, r.value_sol, px, &cap, supply, now)?;
            require!(amount > 0, SpotError::NoMintRoom);
            r.price = px;
            r.amount = amount;
            r.amount_wanted = wanted;
            r.rate_ppm = rate;
            receipt::reserve(&a.levcap_program.to_account_info(), &a.authority.to_account_info(), &cap_ai, &mint_ai, &t22, amount, bump)?;
            receipt::take(&mut a.config, &r, bucket_after, now);
            a.trader.granted_atoms = a.trader.granted_atoms.saturating_add(amount);
            r.status = RECEIPT_PENDING_NFT;
        }
        let number = a.config.receipt_book.issued + 1;
        let decimals = mint_decimals(&mint_ai)?;
        let rkey = a.receipt.key();
        let receipt_ai = a.receipt.to_account_info();
        let (mi, ti, owner, auth, sys) = (a.receipt_mint.to_account_info(), a.receipt_token.to_account_info(), a.owner.to_account_info(), a.authority.to_account_info(), a.system_program.to_account_info());
        let accs = receipt::NftAccs { receipt: rkey, mint: &mi, token: &ti, owner: &owner, authority: &auth, token_2022: &t22, system: &sys };
        let uri = a.config.receipts.uri_str().to_string();
        // the on-chain details and picture (2026-10-26): the coin's and the platform token's
        // tickers from their Token-2022 metadata, an attested receipt's evidence hash
        let ticker = match &coin_ai {
            Some(c) if c.key() == r.coin && *c.owner == TOKEN_2022 => svg::t22_symbol(&c.try_borrow_data()?).and_then(svg::clean_ticker),
            Some(_) => return err!(SpotError::WrongAccount),
            None => None,
        };
        let platform_ticker = svg::t22_symbol(&mint_ai.try_borrow_data()?).and_then(svg::clean_ticker);
        let evidence = match r.attested {
            true => {
                let d = receipt_ai.try_borrow_data()?;
                let at = 8 + Receipt::INIT_SPACE + attest::TAIL_EVIDENCE;
                d.get(at..at + 32).map(|e| <[u8; 32]>::try_from(e).unwrap())
            }
            false => None,
        };
        let extra = receipt::Extra { ticker: ticker.as_deref(), platform_ticker: platform_ticker.as_deref(), evidence };
        let payer = a.payer.to_account_info();
        let before = payer.lamports();
        r.close_auth = receipt::make_nft(&r, number, &uri, decimals, &extra, &accs, &receipt::Funder::Signer(&payer), bump, (ctx.bumps.receipt_mint, ctx.bumps.receipt_token))?;
        // the payer fronted the NFT's rent: the seller's prefund in the receipt pays it back
        let spent = before.saturating_sub(payer.lamports());
        let floor = Rent::get()?.minimum_balance(receipt_ai.data_len());
        receipt::reimburse(&receipt_ai, &payer, spent.min(receipt_ai.lamports().saturating_sub(floor)))?;
        r.number = number;
        r.status = RECEIPT_ISSUED;
        r.issued_at = now;
        let book = &mut a.config.receipt_book;
        book.issued = number;
        book.pending = book.pending.saturating_sub(1);
        emit!(receipt::ReceiptIssued { owner: r.owner, receipt: rkey, number, receipt_mint: mi.key(), amount: r.amount, value_sol: r.value_sol, rate_ppm: r.rate_ppm, redeemable_from: r.redeemable_from, auto_redeems_at: r.auto_redeems_at });
        **a.receipt = r;
        Ok(())
    }

    /// The owner: drop a receipt that still waits for a price (a quote coin's loss without a quote
    /// price, or a loss booked while the issuance bucket was empty) and take its rent float back.
    /// Nothing was reserved for it, so nothing is owed; it never redeems.
    pub fn drop_receipt(ctx: Context<DropReceipt>) -> Result<()> {
        let r = &ctx.accounts.receipt;
        require!(r.status == RECEIPT_PENDING_PRICE && r.amount == 0, SpotError::NotDroppable);
        let book = &mut ctx.accounts.config.receipt_book;
        book.pending = book.pending.saturating_sub(1);
        book.dropped = book.dropped.saturating_add(1);
        book.value_dropped = book.value_dropped.saturating_add(r.value_sol);
        emit!(ReceiptDropped { owner: r.owner, receipt: r.key(), value_sol: r.value_sol });
        Ok(())
    }

    /// Anyone (the owner, or the keeper, which does it then) from `redeemable_from` (the delay
    /// `receipt::delay_for` gave it, 15 min to 48 h): burn the receipt
    /// NFT (permanent delegate), close its mint and, when its close authority is ours, the token
    /// account, mint the receipt's amount through levcap (from what is owed) to the owner's
    /// associated account (created from the receipt's own rent when missing), and close the
    /// receipt. Every lamport of rent goes to the owner.
    pub fn redeem_receipt<'info>(ctx: Context<'_, '_, '_, 'info, RedeemReceipt<'info>>) -> Result<()> {
        use anchor_spl::token_2022::spl_token_2022::instruction as ti;
        let now = Clock::get()?.unix_timestamp;
        // an attested receipt's rent was the attester's: it goes back to the ["attest"] account
        // (remaining account 0), never to the owner (2026-10-26, `attest.rs`)
        let pool = match ctx.accounts.receipt.attested {
            true => {
                let p = ctx.remaining_accounts.first().ok_or(SpotError::WrongAccount)?;
                require_keys_eq!(p.key(), attest::attest_address().0, SpotError::WrongAccount);
                require!(p.is_writable && *p.owner == crate::ID, SpotError::WrongAccount);
                Some(p.clone())
            }
            false => None,
        };
        let a = ctx.accounts;
        let r: Receipt = (**a.receipt).clone();
        require!(r.status == RECEIPT_ISSUED, SpotError::NotPending);
        require!(now >= r.redeemable_from, SpotError::ReceiptLocked);
        let rkey = a.receipt.key();
        require_keys_eq!(a.receipt_mint.key(), receipt::receipt_mint_address(&rkey).0, SpotError::WrongAccount);
        require_keys_eq!(a.receipt_token.key(), receipt::receipt_token_address(&rkey).0, SpotError::WrongAccount);
        let owner_before = a.owner.lamports();
        let bump = a.config.authority_bump;
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[bump]];
        let t22 = a.token_2022_program.to_account_info();
        let (mi, tk, auth, owner) = (a.receipt_mint.to_account_info(), a.receipt_token.to_account_info(), a.authority.to_account_info(), a.owner.to_account_info());
        let rent_to = pool.clone().unwrap_or_else(|| owner.clone());
        // the token: burned by the permanent delegate (the owner may have burned it already)
        if *tk.owner == TOKEN_2022 && amount(&tk)? > 0 {
            invoke_signed_ix(&ti::burn(t22.key, tk.key, mi.key, auth.key, &[], 1)?, &[tk.clone(), mi.clone(), auth.clone()], seeds)?;
        }
        if *tk.owner == TOKEN_2022 && r.close_auth {
            invoke_signed_ix(&ti::close_account(t22.key, tk.key, rent_to.key, auth.key, &[])?, &[tk.clone(), rent_to.clone(), auth.clone()], seeds)?;
        }
        if *mi.owner == TOKEN_2022 {
            invoke_signed_ix(&ti::close_account(t22.key, mi.key, rent_to.key, auth.key, &[])?, &[mi.clone(), rent_to.clone(), auth.clone()], seeds)?;
        }
        // the owner's associated account of the platform token: the payer creates it and the
        // receipt's rent pays it back (the owner's own lamports, in the owner's own account)
        let mut ata_rent = 0u64;
        let dest = a.destination.to_account_info();
        let pm = a.token_mint.to_account_info();
        require_keys_eq!(dest.key(), anchor_spl::associated_token::get_associated_token_address_with_program_id(owner.key, pm.key, &TOKEN_2022), SpotError::WrongAccount);
        if dest.data_is_empty() {
            let len = {
                use anchor_spl::token_2022::spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
                let d = pm.try_borrow_data()?;
                let st = StateWithExtensions::<anchor_spl::token_2022::spl_token_2022::state::Mint>::unpack(&d)?;
                let mut exts = ExtensionType::get_required_init_account_extensions(&st.get_extension_types()?);
                exts.push(ExtensionType::ImmutableOwner);
                ExtensionType::try_calculate_account_len::<anchor_spl::token_2022::spl_token_2022::state::Account>(&exts)?
            };
            ata_rent = Rent::get()?.minimum_balance(len).saturating_sub(dest.lamports());
            anchor_spl::associated_token::create_idempotent(CpiContext::new(
                a.associated_token_program.to_account_info(),
                anchor_spl::associated_token::Create {
                    payer: a.payer.to_account_info(),
                    associated_token: dest.clone(),
                    authority: owner.clone(),
                    mint: pm.clone(),
                    system_program: a.system_program.to_account_info(),
                    token_program: t22.clone(),
                },
            ))?;
        }
        if r.amount > 0 {
            levcap::cpi::mint_to(
                CpiContext::new_with_signer(
                    a.levcap_program.to_account_info(),
                    levcap::cpi::accounts::MintCapped {
                        minter: auth.clone(),
                        cap: a.levcap_cap.to_account_info(),
                        mint: pm.clone(),
                        mint_auth: a.mint_auth.to_account_info(),
                        destination: dest.clone(),
                        token_program: t22.clone(),
                    },
                    &[seeds],
                ),
                r.amount,
            )?;
        }
        let c = &mut a.config;
        c.receipt_book.redeemed += 1;
        c.receipt_book.owed = c.receipt_book.owed.saturating_sub(r.amount);
        c.receipt_book.atoms_redeemed = c.receipt_book.atoms_redeemed.saturating_add(r.amount);
        c.totals.minted_atoms = c.totals.minted_atoms.saturating_add(r.amount);
        a.trader.claimed_atoms = a.trader.claimed_atoms.saturating_add(r.amount);
        let receipt_ai = a.receipt.to_account_info();
        let payer = a.payer.to_account_info();
        receipt::reimburse(&receipt_ai, &payer, ata_rent.min(receipt_ai.lamports()))?;
        if let Some(p) = &pool {
            receipt::reimburse(&receipt_ai, p, receipt_ai.lamports())?;
        }
        // the receipt's own lamports go to the owner when Anchor closes it (`close = owner`)
        let rent_returned = owner.lamports().saturating_sub(owner_before) + a.receipt.to_account_info().lamports();
        emit!(receipt::ReceiptRedeemed { owner: r.owner, receipt: rkey, number: r.number, amount: r.amount, by: a.payer.key(), rent_returned });
        Ok(())
    }

    // ---------- attested receipts (owner 2026-10-26, `attest.rs`) ----------

    /// Admin, once: create the ["attest"] account with its attester (the default key leaves it
    /// off) and limits.
    pub fn init_attest(ctx: Context<InitAttest>, attester: Pubkey, limits: attest::AttestLimits) -> Result<()> {
        limits.validate()?;
        let at = &mut ctx.accounts.attest;
        at.attester = attester;
        at.limits = limits;
        at.bump = ctx.bumps.attest;
        emit!(attest::AttestSet { attester, paused: at.paused, limits });
        Ok(())
    }

    /// Admin, at once: rotate the attester (the default key switches attested receipts off).
    pub fn set_attester(ctx: Context<AttestAdmin>, attester: Pubkey) -> Result<()> {
        let at = &mut ctx.accounts.attest;
        at.attester = attester;
        emit!(attest::AttestSet { attester, paused: at.paused, limits: at.limits });
        Ok(())
    }

    /// Admin, at once: the attested per-day budget and the oldest sell that may be attested.
    pub fn set_attest_limits(ctx: Context<AttestAdmin>, limits: attest::AttestLimits) -> Result<()> {
        limits.validate()?;
        let at = &mut ctx.accounts.attest;
        at.limits = limits;
        emit!(attest::AttestSet { attester: at.attester, paused: at.paused, limits });
        Ok(())
    }

    /// The admin or the attester: pause attested receipts; only the admin resumes them.
    pub fn set_attest_paused(ctx: Context<AttestPause>, paused: bool) -> Result<()> {
        let (who, admin) = (ctx.accounts.signer.key(), ctx.accounts.config.admin);
        let at = &mut ctx.accounts.attest;
        require!(who == admin || (paused && who == at.attester && at.attester != Pubkey::default()), SpotError::NotAdmin);
        at.paused = paused;
        emit!(attest::AttestSet { attester: at.attester, paused, limits: at.limits });
        Ok(())
    }

    /// Anyone: send the ["attest"] account's lamports above its rent (attested receipts' rent come
    /// back from redemption) to the current attester.
    pub fn sweep_attest_rent(ctx: Context<SweepAttest>) -> Result<()> {
        let ai = ctx.accounts.attest.to_account_info();
        let floor = Rent::get()?.minimum_balance(ai.data_len());
        let surplus = ai.lamports().saturating_sub(floor);
        receipt::reimburse(&ai, &ctx.accounts.attester.to_account_info(), surplus)
    }

    /// The attester: book a receipt for `owner`'s unrouted sell of a graduated pad coin in its DAMM
    /// v2 pool (Jupiter, a terminal, a direct swap), with the routed receipt's rules unchanged and
    /// the attested day budget on top. The sell's signature can be attested once
    /// (["attested", sig]). The routed atoms the sell took leave the owner's routed position at its
    /// average cost. The attester fronts the receipt's rent and NFT prefund; they come back to the
    /// ["attest"] account at redemption. Then `issue_receipt` and `redeem_receipt` as for any
    /// receipt (redemption passes the ["attest"] account as remaining account 0).
    pub fn book_attested_receipt<'info>(ctx: Context<'_, '_, '_, 'info, BookAttested<'info>>, sell: attest::AttestedSell) -> Result<()> {
        let clock = Clock::get()?;
        let now = clock.unix_timestamp;
        let a = ctx.accounts;
        require!(!a.attest.paused, SpotError::AttestPaused);
        let mint = a.coin.key();
        let owner = a.owner.key();
        attest::check_graduated(&a.config, &mint, &a.dbc_pool, &a.damm_link, &a.damm_pool)?;
        require!(sell.closed_at <= now && now - sell.closed_at <= a.attest.limits.max_age_secs as i64, SpotError::AttestStale);
        require!(sell.atoms_sold > 0 && sell.held_secs >= 0 && sell.swaps > 0 && sell.cost_lamports > sell.proceeds_lamports, SpotError::BadParam);
        attest::roll_day(&mut a.attest, now);
        init_trader(&mut a.trader, owner, ctx.bumps.trader);
        refresh(&mut a.config, a.px_ring.as_ref().map(|x| x.as_ref()), clock.slot)?;
        let (pos_atoms, pos_cost) = attest::consume_position(&a.position, &owner, &mint, sell.atoms_sold)?;
        let cap_ai = a.levcap_cap.to_account_info();
        let mint_ai = a.token_mint.to_account_info();
        let cap = receipt::load_cap(&cap_ai)?;
        let supply = receipt::mint_supply(&mint_ai)?;
        let ring = a.px_ring.as_ref().map(|x| x.as_ref());
        let price = receipt::antidote_price(&a.config, ring, clock.slot);
        let mpx = receipt::mcap_price(&a.config, ring, clock.slot);
        let seq = a.trader.receipt_seq;
        let (rkey, rbump) = receipt::receipt_address(&owner, seq);
        require_keys_eq!(a.receipt.key(), rkey, SpotError::WrongAccount);
        let (r, v) = attest::size_attested(&mut a.config, &mut a.attest, &a.trader, &sell, owner, mint, seq, rbump, &cap, supply, price, mpx, now)?;
        let bump = a.config.authority_bump;
        let t22 = a.token_2022_program.to_account_info();
        receipt::reserve(&a.levcap_program.to_account_info(), &a.authority.to_account_info(), &cap_ai, &mint_ai, &t22, r.amount, bump)?;
        if let Some(c) = receipt::counted_loss(&r) {
            receipt::lane_record(&mut a.trader, a.config.receipts.lane_window_secs, c, now);
        }
        // the receipt account with its evidence after the struct; the attester fronts its rent and
        // its NFT's (paid back to the ["attest"] account at redemption)
        let payer = a.attester.to_account_info();
        let sys = a.system_program.to_account_info();
        let space = 8 + Receipt::INIT_SPACE + attest::TAIL_LEN;
        let lamports = Rent::get()?.minimum_balance(space) + receipt::nft_rent_bound(a.config.receipts.uri_str(), true)?;
        let receipt_ai = a.receipt.to_account_info();
        receipt::make_account(&receipt_ai, &receipt::Funder::Signer(&payer), &sys, space, lamports, &crate::ID, &[RECEIPT_SEED, owner.as_ref(), &seq.to_le_bytes(), &[rbump]])?;
        receipt::store_receipt(&receipt_ai, &r)?;
        attest::write_tail(&receipt_ai, &sell)?;
        a.trader.receipt_seq = seq + 1;
        a.trader.granted_atoms = a.trader.granted_atoms.saturating_add(r.amount);
        let book = &mut a.config.receipt_book;
        book.pending += 1;
        book.value_lamports = book.value_lamports.saturating_add(v);
        a.config.totals.granted_value = a.config.totals.granted_value.saturating_add(v);
        a.attest.rent_fronted = a.attest.rent_fronted.saturating_add(lamports);
        let rec = &mut a.record;
        rec.owner = owner;
        rec.coin = mint;
        rec.receipt = rkey;
        rec.evidence = sell.evidence;
        rec.closed_at = sell.closed_at;
        rec.booked_at = now;
        rec.bump = ctx.bumps.record;
        emit!(receipt::ReceiptBooked {
            owner,
            receipt: rkey,
            coin: mint,
            quote: r.quote,
            status: r.status,
            loss: r.loss,
            loss_sol: r.loss_sol,
            share_bps: r.share_bps,
            h_ppm: r.h_ppm,
            d_ppm: r.d_ppm,
            rate_ppm: r.rate_ppm,
            value_sol: r.value_sol,
            amount: r.amount,
            price: r.price,
            closed_at: r.closed_at,
            delay_secs: r.delay_secs,
            in_lane: r.in_lane,
            redeemable_from: r.redeemable_from,
            auto_redeems_at: r.auto_redeems_at,
        });
        emit!(attest::AttestedReceipt {
            owner,
            receipt: rkey,
            coin: mint,
            attester: payer.key(),
            sig: sell.sig,
            evidence: sell.evidence,
            swaps: sell.swaps,
            cost_lamports: sell.cost_lamports,
            proceeds_lamports: sell.proceeds_lamports,
            atoms_sold: sell.atoms_sold,
            held_secs: sell.held_secs,
            closed_at: sell.closed_at,
            position_atoms: pos_atoms,
            position_cost: pos_cost,
        });
        Ok(())
    }

}

fn invoke_signed_ix(ix: &Instruction, infos: &[AccountInfo], seeds: &[&[u8]]) -> Result<()> {
    anchor_lang::solana_program::program::invoke_signed(ix, infos, &[seeds])?;
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = admin, space = 8 + Vault::INIT_SPACE, seeds = [VAULT_SEED], bump)]
    pub vault: Account<'info, Vault>,
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ SpotError::NotAdmin,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Admin<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SpotError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    #[account(address = config.pending_admin @ SpotError::NotAdmin)]
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
}

#[derive(Accounts)]
pub struct GrowConfig<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the ["config"] PDA (seeds, owner and discriminator checked); read raw because a
    /// config from before `ParamsExt` is too short to load
    #[account(mut, seeds = [CONFIG_SEED], bump, owner = crate::ID)]
    pub config: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ConfigOnly<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
}

#[derive(Accounts)]
pub struct SetPlatformToken<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SpotError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: PDA, levcap's minter and the stake vault's owner
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: a Token-2022 mint whose authority is levcap's PDA (checked in the handler)
    #[account(owner = TOKEN_2022)]
    pub token_mint: UncheckedAccount<'info>,
    #[account(seeds = [levcap::CAP_SEED, token_mint.key().as_ref()], bump = levcap_cap.bump, seeds::program = levcap::ID)]
    pub levcap_cap: Box<Account<'info, levcap::Cap>>,
    /// CHECK: the authority's associated Token-2022 account of the mint (checked in the handler)
    pub stake_vault: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct TradeDbc<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(init_if_needed, payer = owner, space = 8 + Position::INIT_SPACE, seeds = [POSITION_SEED, owner.key().as_ref(), base_mint.key().as_ref()], bump)]
    pub position: Box<Account<'info, Position>>,
    #[account(init_if_needed, payer = owner, space = 8 + Trader::INIT_SPACE, seeds = [TRADER_SEED, owner.key().as_ref()], bump)]
    pub trader: Box<Account<'info, Trader>>,
    /// CHECK: the owner's account of the coin's quote (WSOL for SOL coins; checked)
    #[account(mut)]
    pub user_wsol: UncheckedAccount<'info>,
    /// CHECK: the owner's Token-2022 account of the coin (checked)
    #[account(mut)]
    pub user_token: UncheckedAccount<'info>,
    /// CHECK: levfee's account of the coin's quote: the fee PDA's associated account (checked)
    #[account(mut)]
    pub fee_wsol: UncheckedAccount<'info>,
    /// CHECK: DBC's pool authority
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: the pool's config, which must be counted (checked)
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC pool (checked)
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: DBC checks
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: DBC checks
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the coin (DBC checks it against the pool)
    #[account(mut)]
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's quote: WSOL, or the allowed quote its DBC config names (checked)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's hook meta list (Token-2022 checks)
    pub meta_list: UncheckedAccount<'info>,
    /// CHECK: levpad-hook
    #[account(address = levpad_hook::ID)]
    pub hook_program: UncheckedAccount<'info>,
    /// CHECK: the coin's hook ring (Token-2022 checks)
    #[account(mut)]
    pub ring: UncheckedAccount<'info>,
    /// The quote's token program (SPL Token for SOL coins; checked against the quote mint).
    pub token_program: Interface<'info, TokenInterface>,
    pub token_2022_program: Program<'info, Token2022>,
    /// CHECK: DBC's event authority
    #[account(address = DBC_EVENT_AUTHORITY)]
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: DBC
    #[account(address = DBC_PROGRAM)]
    pub dbc_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: the platform token's hook ring, sampled when passed
    pub px_ring: Option<UncheckedAccount<'info>>,
    /// CHECK: levfee's state, read for the mint budget when passed
    pub fee_state: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct TradeDamm<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(init_if_needed, payer = owner, space = 8 + Position::INIT_SPACE, seeds = [POSITION_SEED, owner.key().as_ref(), base_mint.key().as_ref()], bump)]
    pub position: Box<Account<'info, Position>>,
    #[account(init_if_needed, payer = owner, space = 8 + Trader::INIT_SPACE, seeds = [TRADER_SEED, owner.key().as_ref()], bump)]
    pub trader: Box<Account<'info, Trader>>,
    /// CHECK: the owner's account of the coin's quote (WSOL for SOL coins; checked)
    #[account(mut)]
    pub user_wsol: UncheckedAccount<'info>,
    /// CHECK: the owner's Token-2022 account of the coin (checked)
    #[account(mut)]
    pub user_token: UncheckedAccount<'info>,
    /// CHECK: levfee's account of the coin's quote: the fee PDA's associated account (checked)
    #[account(mut)]
    pub fee_wsol: UncheckedAccount<'info>,
    /// CHECK: DAMM v2's pool authority
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: the pool levpad-hook bound to the coin (checked)
    #[account(mut)]
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    /// CHECK: the coin
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the coin's quote: the pool's other mint, WSOL or an allowed quote (checked)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's DammLink for the coin (checked)
    pub damm_link: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC pool, migrated (checked: its config must be counted)
    pub dbc_pool: UncheckedAccount<'info>,
    /// The quote's token program (SPL Token for SOL coins; checked against the quote mint).
    pub token_program: Interface<'info, TokenInterface>,
    pub token_2022_program: Program<'info, Token2022>,
    /// CHECK: DAMM v2's event authority
    #[account(address = DAMM_EVENT_AUTHORITY)]
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2
    #[account(address = DAMM_V2_PROGRAM)]
    pub damm_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: the platform token's hook ring, sampled when passed
    pub px_ring: Option<UncheckedAccount<'info>>,
    /// CHECK: levfee's state, read for the mint budget when passed
    pub fee_state: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct InitQuoteBook<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: an allowed quote mint (checked)
    pub quote_mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + QuoteBook::INIT_SPACE, seeds = [QUOTE_BOOK_SEED, quote_mint.key().as_ref()], bump)]
    pub book: Box<Account<'info, QuoteBook>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SampleQuote<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [QUOTE_BOOK_SEED, book.mint.as_ref()], bump = book.bump)]
    pub book: Box<Account<'info, QuoteBook>>,
    /// CHECK: the quote's leg 0 pool (checked against the entry, read raw)
    pub leg0: UncheckedAccount<'info>,
    /// CHECK: the quote's leg 1 pool, for two-leg quotes (checked against the entry, read raw)
    pub leg1: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, close = owner, has_one = owner, seeds = [POSITION_SEED, owner.key().as_ref(), position.mint.as_ref()], bump = position.bump)]
    pub position: Box<Account<'info, Position>>,
}

#[derive(Accounts)]
pub struct SamplePrice<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: the platform token's hook ring (checked by `sample`)
    pub ring: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct IssueReceipt<'info> {
    /// Fronts the NFT's rent (paid back from the receipt's prefund in the same instruction).
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [RECEIPT_SEED, receipt.owner.as_ref(), &receipt.seq.to_le_bytes()], bump = receipt.bump)]
    pub receipt: Box<Account<'info, Receipt>>,
    #[account(mut, seeds = [TRADER_SEED, receipt.owner.as_ref()], bump = trader.bump)]
    pub trader: Box<Account<'info, Trader>>,
    /// CHECK: the receipt's NFT mint ["rmint", receipt], created here
    #[account(mut, seeds = [RECEIPT_MINT_SEED, receipt.key().as_ref()], bump)]
    pub receipt_mint: UncheckedAccount<'info>,
    /// CHECK: the owner's token account of it ["rtok", receipt], created here
    #[account(mut, seeds = [RECEIPT_TOKEN_SEED, receipt.key().as_ref()], bump)]
    pub receipt_token: UncheckedAccount<'info>,
    /// CHECK: the receipt's owner
    #[account(address = receipt.owner)]
    pub owner: UncheckedAccount<'info>,
    /// CHECK: PDA, levcap's minter and the NFT's authorities
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: levcap's Cap of the platform token (reserved when the receipt is priced here)
    #[account(mut, address = config.levcap_cap)]
    pub levcap_cap: UncheckedAccount<'info>,
    /// CHECK: the platform token (its supply)
    #[account(address = config.token_mint)]
    pub token_mint: UncheckedAccount<'info>,
    pub token_2022_program: Program<'info, Token2022>,
    pub levcap_program: Program<'info, levcap::program::Levcap>,
    pub system_program: Program<'info, System>,
    /// CHECK: the platform token's hook ring, sampled when passed (needed to price a receipt)
    pub px_ring: Option<UncheckedAccount<'info>>,
    /// CHECK: the quote's `QuoteBook` (checked), to value a quote coin's loss
    pub quote_book: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct DropReceipt<'info> {
    #[account(mut, address = receipt.owner @ SpotError::NotDroppable)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, close = owner, seeds = [RECEIPT_SEED, receipt.owner.as_ref(), &receipt.seq.to_le_bytes()], bump = receipt.bump)]
    pub receipt: Box<Account<'info, Receipt>>,
}

#[derive(Accounts)]
pub struct RedeemReceipt<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, close = owner, seeds = [RECEIPT_SEED, receipt.owner.as_ref(), &receipt.seq.to_le_bytes()], bump = receipt.bump)]
    pub receipt: Box<Account<'info, Receipt>>,
    #[account(mut, seeds = [TRADER_SEED, receipt.owner.as_ref()], bump = trader.bump)]
    pub trader: Box<Account<'info, Trader>>,
    /// CHECK: the receipt's owner, who gets the tokens and every lamport of rent
    #[account(mut, address = receipt.owner)]
    pub owner: UncheckedAccount<'info>,
    /// CHECK: ["rmint", receipt] (checked)
    #[account(mut)]
    pub receipt_mint: UncheckedAccount<'info>,
    /// CHECK: ["rtok", receipt] (checked)
    #[account(mut)]
    pub receipt_token: UncheckedAccount<'info>,
    /// CHECK: PDA, levcap's minter and the NFT's authorities
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: levcap's Cap (levcap checks it)
    #[account(mut, address = config.levcap_cap)]
    pub levcap_cap: UncheckedAccount<'info>,
    /// CHECK: the platform token
    #[account(mut, address = config.token_mint)]
    pub token_mint: UncheckedAccount<'info>,
    /// CHECK: levcap's ["mint_auth", mint] PDA; levcap checks it
    pub mint_auth: UncheckedAccount<'info>,
    /// CHECK: the owner's associated Token-2022 account of the platform token (checked; created
    /// when missing)
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    pub token_2022_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, anchor_spl::associated_token::AssociatedToken>,
    pub levcap_program: Program<'info, levcap::program::Levcap>,
    pub system_program: Program<'info, System>,
}

// ---------- attested receipts (owner 2026-10-26, `attest.rs`) ----------

#[derive(Accounts)]
pub struct InitAttest<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SpotError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = admin, space = 8 + attest::Attest::INIT_SPACE, seeds = [attest::ATTEST_SEED], bump)]
    pub attest: Box<Account<'info, attest::Attest>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AttestAdmin<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SpotError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [attest::ATTEST_SEED], bump = attest.bump)]
    pub attest: Box<Account<'info, attest::Attest>>,
}

#[derive(Accounts)]
pub struct AttestPause<'info> {
    /// The admin, or the attester (pausing only)
    pub signer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [attest::ATTEST_SEED], bump = attest.bump)]
    pub attest: Box<Account<'info, attest::Attest>>,
}

#[derive(Accounts)]
pub struct SweepAttest<'info> {
    #[account(mut, seeds = [attest::ATTEST_SEED], bump = attest.bump)]
    pub attest: Box<Account<'info, attest::Attest>>,
    /// CHECK: the current attester (gets the surplus)
    #[account(mut, address = attest.attester @ SpotError::NotAttester)]
    pub attester: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(sell: attest::AttestedSell)]
pub struct BookAttested<'info> {
    /// The attester: signs and fronts the rent (receipt, its NFT prefund, the attestation record,
    /// the wallet's Trader when it has none)
    #[account(mut, address = attest.attester @ SpotError::NotAttester, constraint = attest.attester != Pubkey::default() @ SpotError::NotAttester)]
    pub attester: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [attest::ATTEST_SEED], bump = attest.bump)]
    pub attest: Box<Account<'info, attest::Attest>>,
    /// The sell's signature, once ever
    #[account(init, payer = attester, space = 8 + attest::Attestation::INIT_SPACE, seeds = [attest::ATTESTED_SEED, &sell.sig[..32], &sell.sig[32..]], bump)]
    pub record: Box<Account<'info, attest::Attestation>>,
    /// CHECK: the wallet that sold (the receipt's owner)
    pub owner: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = attester, space = 8 + Trader::INIT_SPACE, seeds = [TRADER_SEED, owner.key().as_ref()], bump)]
    pub trader: Box<Account<'info, Trader>>,
    /// CHECK: the wallet's routed position in the coin ["pos", owner, coin] (checked; may not exist)
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the receipt ["receipt", owner, trader.receipt_seq] (checked), created here
    #[account(mut)]
    pub receipt: UncheckedAccount<'info>,
    /// CHECK: the coin (a pad coin that graduated; checked with its pools)
    pub coin: UncheckedAccount<'info>,
    /// CHECK: the coin's DBC pool: counted config, migrated (checked)
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: levpad-hook's DammLink for the coin (checked)
    pub damm_link: UncheckedAccount<'info>,
    /// CHECK: the DAMM v2 pool the link binds, paired with WSOL (checked)
    pub damm_pool: UncheckedAccount<'info>,
    /// CHECK: PDA, levcap's minter
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: levcap's Cap of the platform token
    #[account(mut, address = config.levcap_cap)]
    pub levcap_cap: UncheckedAccount<'info>,
    /// CHECK: the platform token (its supply)
    #[account(address = config.token_mint)]
    pub token_mint: UncheckedAccount<'info>,
    pub token_2022_program: Program<'info, Token2022>,
    pub levcap_program: Program<'info, levcap::program::Levcap>,
    pub system_program: Program<'info, System>,
    /// CHECK: the platform token's hook ring (the receipt's price; sampled when passed)
    pub px_ring: Option<UncheckedAccount<'info>>,
}
