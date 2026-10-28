//! antidote-ideas: suggestions and votes for Antidote, on chain (owner 2026-10-25).
//!
//! Anyone holding at least `min_balance` of the platform token ($ANTI) suggests an idea (a title
//! of at most 80 bytes and a body of at most 600 bytes of UTF-8, both stored in the Idea account)
//! and votes on ideas, once per wallet per idea: the vote is a PDA ["vote", idea, voter] whose
//! existence is the vote. When an Open idea reaches `threshold` votes (10 by default) it moves to
//! InReview in the same instruction and `ThresholdReached` is emitted. The admin then moves it to
//! Building and Live, or to Rejected with a reason, and may close spam (rent back to the author).
//!
//! Forks (owner 2026-10-26): a holder may fork an idea with a better version (`suggest_fork`). A
//! fork is an Idea of its own (own id, title, body, votes and threshold flip) whose `parent` names
//! the idea it forks; clients show forks under the parent like a comment thread. One level only: a
//! fork cannot be forked. Any idea that still exists can be forked, Rejected ones included (a fork
//! is how a rejected idea comes back fixed); a closed idea's account is gone, so it cannot be.
//!
//! Curated ideas (owner 2026-10-26): the admin seeds the experiments Antidote already runs as
//! Ideas (`seed_idea`, `curated` set, in the status that matches them) so they take the same votes.
//! A curated idea also takes votes while Building or Live (a support count); every other idea
//! takes votes while Open or InReview only.
//!
//! The holding check reads a token account of the platform mint owned by the signer (SPL Token or
//! Token-2022) at the moment of the suggestion or vote. It is not sybil resistant: the same tokens
//! can be moved to another wallet and vote again. SPEC.md "Ideas" has the caveat and the stronger
//! options that were left out.
//!
//! This program is separate from the launch programs (tilt-spot, levpad-hook, levcap, levfee): it
//! reads nothing of theirs and none of them reads it.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount};

declare_id!("6a4WDHiWcpGV1WB8QjEoT3bxRGCHo2HnvFgiF8NW4bqz");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Antidote Ideas",
    project_url: "https://github.com/antidotefinance",
    contacts: "link:https://github.com/antidotefinance/antidote-programs/security/advisories/new",
    policy: "https://github.com/antidotefinance/antidote-programs/blob/main/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/antidotefinance/antidote-programs"
}

pub const CONFIG_SEED: &[u8] = b"config";
pub const IDEA_SEED: &[u8] = b"idea";
pub const VOTE_SEED: &[u8] = b"vote";

pub const TITLE_MAX: usize = 80;
pub const BODY_MAX: usize = 600;
pub const NOTE_MAX: usize = 200;
pub const DEFAULT_THRESHOLD: u32 = 10;
/// The highest threshold `set_config` accepts (a sanity bound, not a policy).
pub const THRESHOLD_MAX: u32 = 1_000_000;

#[program]
pub mod antidote_ideas {
    use super::*;

    /// Creates the Config. Only the program's upgrade authority may call it (no front-running the
    /// admin). `min_balance` is in atoms of `anti_mint` (1 $ANTI = 10^decimals); `threshold` votes
    /// move an idea to InReview.
    pub fn initialize(ctx: Context<Initialize>, min_balance: u64, threshold: u32) -> Result<()> {
        check_params(min_balance, threshold)?;
        let c = &mut ctx.accounts.config;
        c.admin = ctx.accounts.admin.key();
        c.pending_admin = Pubkey::default();
        c.anti_mint = ctx.accounts.anti_mint.key();
        c.min_balance = min_balance;
        c.threshold = threshold;
        c.next_id = 0;
        c.paused = false;
        c.bump = ctx.bumps.config;
        c.ideas = 0;
        c.votes = 0;
        c.reserved = [0; 64];
        emit!(ConfigSet { admin: c.admin, pending_admin: c.pending_admin, anti_mint: c.anti_mint, min_balance, threshold, paused: false });
        Ok(())
    }

    /// Admin: change any of the settings. `new_admin` is proposed and takes over only when it signs
    /// `accept_admin`. Passing `new_mint` switches the token the holding check reads.
    pub fn set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
        let c = &mut ctx.accounts.config;
        if let Some(m) = &ctx.accounts.new_mint {
            c.anti_mint = m.key();
        }
        if let Some(v) = args.min_balance {
            c.min_balance = v;
        }
        if let Some(v) = args.threshold {
            c.threshold = v;
        }
        check_params(c.min_balance, c.threshold)?;
        if let Some(v) = args.paused {
            c.paused = v;
        }
        if let Some(a) = args.new_admin {
            c.pending_admin = a;
        }
        emit!(ConfigSet { admin: c.admin, pending_admin: c.pending_admin, anti_mint: c.anti_mint, min_balance: c.min_balance, threshold: c.threshold, paused: c.paused });
        Ok(())
    }

    /// The proposed admin takes over.
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require_keys_neq!(c.pending_admin, Pubkey::default(), IdeasError::NotPendingAdmin);
        require_keys_eq!(c.pending_admin, ctx.accounts.new_admin.key(), IdeasError::NotPendingAdmin);
        c.admin = c.pending_admin;
        c.pending_admin = Pubkey::default();
        emit!(ConfigSet { admin: c.admin, pending_admin: c.pending_admin, anti_mint: c.anti_mint, min_balance: c.min_balance, threshold: c.threshold, paused: c.paused });
        Ok(())
    }

    /// A holder suggests an idea; it gets the next id and the author pays its rent (sized to the
    /// text, plus room for the admin's note).
    pub fn suggest(ctx: Context<Suggest>, title: String, body: String) -> Result<()> {
        check_text(&title, &body)?;
        let now = Clock::get()?.unix_timestamp;
        let c = &mut ctx.accounts.config;
        require!(!c.paused, IdeasError::Paused);
        let id = c.next_id;
        c.next_id = id.checked_add(1).ok_or(IdeasError::Overflow)?;
        c.ideas = c.ideas.saturating_add(1);
        let i = &mut ctx.accounts.idea;
        i.id = id;
        i.author = ctx.accounts.author.key();
        i.created_at = now;
        i.votes = 0;
        i.status = IdeaStatus::Open;
        i.bump = ctx.bumps.idea;
        i.status_at = now;
        i.title = title;
        i.body = body;
        i.note = String::new();
        i.parent = None;
        i.curated = false;
        i.forks = 0;
        emit!(IdeaSuggested {
            idea: i.key(),
            id,
            author: i.author,
            title: i.title.clone(),
            body: i.body.clone(),
            created_at: now,
            balance: ctx.accounts.author_token.amount,
        });
        Ok(())
    }

    /// A holder forks an idea: a new Idea (next id, the author pays its rent) whose `parent` is
    /// `parent_id`. The parent must exist and must not be a fork itself (one level). Its status does
    /// not matter, Rejected included. Votes and the threshold flip work on a fork exactly as on any
    /// idea.
    pub fn suggest_fork(ctx: Context<SuggestFork>, parent_id: u64, title: String, body: String) -> Result<()> {
        check_text(&title, &body)?;
        let now = Clock::get()?.unix_timestamp;
        let c = &mut ctx.accounts.config;
        require!(!c.paused, IdeasError::Paused);
        let p = &mut ctx.accounts.parent;
        require!(p.id == parent_id, IdeasError::WrongIdea);
        require!(p.parent.is_none(), IdeasError::ForkOfFork);
        p.forks = p.forks.saturating_add(1);
        let id = c.next_id;
        c.next_id = id.checked_add(1).ok_or(IdeasError::Overflow)?;
        c.ideas = c.ideas.saturating_add(1);
        let i = &mut ctx.accounts.idea;
        i.id = id;
        i.author = ctx.accounts.author.key();
        i.created_at = now;
        i.votes = 0;
        i.status = IdeaStatus::Open;
        i.bump = ctx.bumps.idea;
        i.status_at = now;
        i.title = title;
        i.body = body;
        i.note = String::new();
        i.parent = Some(parent_id);
        i.curated = false;
        i.forks = 0;
        emit!(IdeaForked {
            idea: i.key(),
            id,
            parent: p.key(),
            parent_id,
            author: i.author,
            title: i.title.clone(),
            body: i.body.clone(),
            created_at: now,
            balance: ctx.accounts.author_token.amount,
        });
        Ok(())
    }

    /// Admin: seed a curated idea (an experiment Antidote already runs) in the status that matches
    /// it, so holders can vote on it. The admin is its author and pays its rent; no holding check.
    /// Any status but Rejected; `note` as in `set_status` (at most 200 bytes). A curated idea takes
    /// votes while Open, InReview, Building or Live; seeded Open, it flips at the threshold as usual.
    pub fn seed_idea(ctx: Context<SeedIdea>, title: String, body: String, status: IdeaStatus, note: Option<String>) -> Result<()> {
        check_text(&title, &body)?;
        require!(status != IdeaStatus::Rejected, IdeasError::BadTransition);
        let note = note.unwrap_or_default();
        require!(note.len() <= NOTE_MAX, IdeasError::NoteTooLong);
        let now = Clock::get()?.unix_timestamp;
        let c = &mut ctx.accounts.config;
        let id = c.next_id;
        c.next_id = id.checked_add(1).ok_or(IdeasError::Overflow)?;
        c.ideas = c.ideas.saturating_add(1);
        let i = &mut ctx.accounts.idea;
        i.id = id;
        i.author = ctx.accounts.admin.key();
        i.created_at = now;
        i.votes = 0;
        i.status = status;
        i.bump = ctx.bumps.idea;
        i.status_at = now;
        i.title = title;
        i.body = body;
        i.note = note;
        i.parent = None;
        i.curated = true;
        i.forks = 0;
        emit!(IdeaSeeded { idea: i.key(), id, author: i.author, title: i.title.clone(), body: i.body.clone(), status, note: i.note.clone(), created_at: now });
        Ok(())
    }

    /// Anyone (the payer pays the rent difference): grows an Idea created before forks existed to
    /// the current size, so its note keeps its full 200 bytes of room. A no-op when already grown.
    /// Only devnet has such ideas; mainnet starts with the current layout.
    pub fn grow_idea(ctx: Context<GrowIdea>) -> Result<()> {
        let info = ctx.accounts.idea.to_account_info();
        let need = Idea::space(&ctx.accounts.idea.title, &ctx.accounts.idea.body);
        let have = info.data_len();
        if have >= need {
            return Ok(());
        }
        let rent = Rent::get()?.minimum_balance(need).saturating_sub(info.lamports());
        if rent > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(ctx.accounts.system_program.to_account_info(), anchor_lang::system_program::Transfer { from: ctx.accounts.payer.to_account_info(), to: info.clone() }),
                rent,
            )?;
        }
        info.realloc(need, true)?;
        emit!(IdeaGrown { idea: info.key(), id: ctx.accounts.idea.id, from: have as u32, to: need as u32 });
        Ok(())
    }

    /// A holder votes for an idea, once: the vote PDA is created here and the voter pays its rent.
    /// Votes are taken while Open or InReview, and on curated ideas also while Building or Live.
    /// The vote that reaches the threshold moves an Open idea to InReview.
    pub fn vote(ctx: Context<Vote>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = &mut ctx.accounts.config;
        require!(!c.paused, IdeasError::Paused);
        let i = &mut ctx.accounts.idea;
        let open = matches!(i.status, IdeaStatus::Open | IdeaStatus::InReview) || (i.curated && matches!(i.status, IdeaStatus::Building | IdeaStatus::Live));
        require!(open, IdeasError::VotingClosed);
        i.votes = i.votes.checked_add(1).ok_or(IdeasError::Overflow)?;
        c.votes = c.votes.saturating_add(1);
        let v = &mut ctx.accounts.vote;
        v.idea = i.key();
        v.voter = ctx.accounts.voter.key();
        v.voted_at = now;
        v.bump = ctx.bumps.vote;
        let mut reached = false;
        if i.status == IdeaStatus::Open && i.votes >= c.threshold {
            i.status = IdeaStatus::InReview;
            i.status_at = now;
            reached = true;
        }
        emit!(Voted { idea: i.key(), id: i.id, voter: v.voter, votes: i.votes, status: i.status, balance: ctx.accounts.voter_token.amount, at: now });
        if reached {
            emit!(ThresholdReached { idea: i.key(), id: i.id, votes: i.votes, threshold: c.threshold, at: now });
        }
        Ok(())
    }

    /// The voter takes the vote back: the vote PDA is closed (rent to the voter) and the count goes
    /// down, but the status never moves back (an idea in review stays in review). Works while
    /// paused, and after the admin closed the idea (then only the vote is closed).
    pub fn unvote(ctx: Context<Unvote>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let info = ctx.accounts.idea.to_account_info();
        let live = info.owner == &crate::ID && info.data_len() > 0;
        let (id, votes, status) = if live {
            let mut i = {
                let d = info.try_borrow_data()?;
                Idea::try_deserialize(&mut &d[..])?
            };
            i.votes = i.votes.saturating_sub(1);
            let mut d = info.try_borrow_mut_data()?;
            let mut w: &mut [u8] = &mut d[..];
            i.try_serialize(&mut w)?;
            (i.id, i.votes, Some(i.status))
        } else {
            (u64::MAX, 0, None)
        };
        if live {
            let c = &mut ctx.accounts.config;
            c.votes = c.votes.saturating_sub(1);
        }
        emit!(Unvoted { idea: info.key(), id, voter: ctx.accounts.voter.key(), votes, status, idea_closed: !live, at: now });
        Ok(())
    }

    /// Admin: InReview -> Building -> Live, or Open / InReview / Building -> Rejected with a reason.
    /// The same status again only updates the note (a link once Live, for example). `note` is at
    /// most 200 bytes; None keeps the current one.
    pub fn set_status(ctx: Context<AdminIdea>, status: IdeaStatus, note: Option<String>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let i = &mut ctx.accounts.idea;
        let from = i.status;
        let ok = from == status
            || matches!(
                (from, status),
                (IdeaStatus::InReview, IdeaStatus::Building)
                    | (IdeaStatus::Building, IdeaStatus::Live)
                    | (IdeaStatus::Open, IdeaStatus::Rejected)
                    | (IdeaStatus::InReview, IdeaStatus::Rejected)
                    | (IdeaStatus::Building, IdeaStatus::Rejected)
            );
        require!(ok, IdeasError::BadTransition);
        if let Some(n) = note {
            require!(n.len() <= NOTE_MAX, IdeasError::NoteTooLong);
            i.note = n;
        }
        if status == IdeaStatus::Rejected {
            require!(!i.note.trim().is_empty(), IdeasError::ReasonRequired);
        }
        if from != status {
            i.status = status;
            i.status_at = now;
        }
        emit!(StatusSet { idea: i.key(), id: i.id, from, to: status, note: i.note.clone(), at: now });
        Ok(())
    }

    /// Admin, for spam: closes the idea and returns its rent to the author. Votes on it stay until
    /// their voters `unvote` (rent back to them).
    pub fn close_idea(ctx: Context<CloseIdea>) -> Result<()> {
        let i = &ctx.accounts.idea;
        let c = &mut ctx.accounts.config;
        c.ideas = c.ideas.saturating_sub(1);
        c.votes = c.votes.saturating_sub(i.votes as u64);
        emit!(IdeaClosed { idea: i.key(), id: i.id, author: i.author, votes: i.votes, status: i.status, at: Clock::get()?.unix_timestamp });
        Ok(())
    }
}

fn check_params(min_balance: u64, threshold: u32) -> Result<()> {
    require!(min_balance > 0, IdeasError::BadMinBalance);
    require!(threshold > 0 && threshold <= THRESHOLD_MAX, IdeasError::BadThreshold);
    Ok(())
}

fn check_text(title: &str, body: &str) -> Result<()> {
    // String arguments are already valid UTF-8: borsh refuses anything else before this runs
    require!(!title.trim().is_empty(), IdeasError::TitleEmpty);
    require!(title.len() <= TITLE_MAX, IdeasError::TitleTooLong);
    require!(body.len() <= BODY_MAX, IdeasError::BodyTooLong);
    Ok(())
}

// ---------- accounts ----------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Proposed by `set_config`, takes over on `accept_admin`; default when none.
    pub pending_admin: Pubkey,
    /// The token the holding check reads ($ANTI; on devnet the devnet platform token).
    pub anti_mint: Pubkey,
    /// Atoms of `anti_mint` a wallet must hold to suggest or vote.
    pub min_balance: u64,
    /// Votes that move an Open idea to InReview.
    pub threshold: u32,
    /// The id the next idea gets (ids start at 0 and are never reused).
    pub next_id: u64,
    pub paused: bool,
    pub bump: u8,
    /// Ideas not closed, and votes on them (informational counters).
    pub ideas: u64,
    pub votes: u64,
    pub reserved: [u8; 64],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum IdeaStatus {
    Open,
    InReview,
    Building,
    Live,
    Rejected,
}

#[account]
pub struct Idea {
    pub id: u64,
    pub author: Pubkey,
    pub created_at: i64,
    pub votes: u32,
    pub status: IdeaStatus,
    pub bump: u8,
    /// When the status last changed (created_at while Open).
    pub status_at: i64,
    /// At most 80 bytes.
    pub title: String,
    /// At most 600 bytes of UTF-8.
    pub body: String,
    /// The admin's reason (Rejected) or link, at most 200 bytes; empty until set.
    pub note: String,
    // The fields below were appended on 2026-10-26 after the strings. An Idea created before then
    // reads them from the zeroed note room as None / false / 0 (`grow_idea` adds their bytes).
    /// The id of the idea this one forks; None for a top-level idea.
    pub parent: Option<u64>,
    /// Seeded by the admin (`seed_idea`): an experiment Antidote runs; takes votes while Building
    /// or Live too.
    pub curated: bool,
    /// Forks suggested on this idea (closed ones included).
    pub forks: u32,
}

impl Idea {
    /// Fixed part (with the discriminator and the three string length prefixes).
    pub const FIXED: usize = 8 + 8 + 32 + 8 + 4 + 1 + 1 + 8 + 4 + 4 + 4;
    /// The fields after the strings: parent (Option<u64>, 9 at most), curated, forks.
    pub const TAIL: usize = 9 + 1 + 4;
    /// The account is sized to the text plus room for the longest note.
    pub fn space(title: &str, body: &str) -> usize {
        Self::FIXED + title.len() + body.len() + NOTE_MAX + Self::TAIL
    }
}

#[account]
#[derive(InitSpace)]
pub struct VoteRecord {
    pub idea: Pubkey,
    pub voter: Pubkey,
    pub voted_at: i64,
    pub bump: u8,
}

pub fn config_address() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &crate::ID).0
}
pub fn idea_address(id: u64) -> Pubkey {
    Pubkey::find_program_address(&[IDEA_SEED, &id.to_le_bytes()], &crate::ID).0
}
pub fn vote_address(idea: &Pubkey, voter: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[VOTE_SEED, idea.as_ref(), voter.as_ref()], &crate::ID).0
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct ConfigArgs {
    pub min_balance: Option<u64>,
    pub threshold: Option<u32>,
    pub paused: Option<bool>,
    pub new_admin: Option<Pubkey>,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    pub anti_mint: InterfaceAccount<'info, Mint>,
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ IdeasError::NotAdmin,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ IdeasError::NotAdmin)]
    pub config: Account<'info, Config>,
    pub new_mint: Option<InterfaceAccount<'info, Mint>>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub new_admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
#[instruction(title: String, body: String)]
pub struct Suggest<'info> {
    #[account(mut)]
    pub author: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = author,
        space = Idea::space(&title, &body),
        seeds = [IDEA_SEED, &config.next_id.to_le_bytes()],
        bump,
    )]
    pub idea: Account<'info, Idea>,
    /// The author's account of the platform token (SPL or Token-2022).
    #[account(
        constraint = author_token.mint == config.anti_mint @ IdeasError::WrongMint,
        constraint = author_token.owner == author.key() @ IdeasError::NotTokenOwner,
        constraint = author_token.amount >= config.min_balance @ IdeasError::BelowMinimum,
    )]
    pub author_token: InterfaceAccount<'info, TokenAccount>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(parent_id: u64, title: String, body: String)]
pub struct SuggestFork<'info> {
    #[account(mut)]
    pub author: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// The idea forked (must exist; a closed idea's account is gone).
    #[account(mut, seeds = [IDEA_SEED, &parent_id.to_le_bytes()], bump = parent.bump)]
    pub parent: Account<'info, Idea>,
    #[account(
        init,
        payer = author,
        space = Idea::space(&title, &body),
        seeds = [IDEA_SEED, &config.next_id.to_le_bytes()],
        bump,
    )]
    pub idea: Account<'info, Idea>,
    #[account(
        constraint = author_token.mint == config.anti_mint @ IdeasError::WrongMint,
        constraint = author_token.owner == author.key() @ IdeasError::NotTokenOwner,
        constraint = author_token.amount >= config.min_balance @ IdeasError::BelowMinimum,
    )]
    pub author_token: InterfaceAccount<'info, TokenAccount>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(title: String, body: String)]
pub struct SeedIdea<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ IdeasError::NotAdmin)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = admin,
        space = Idea::space(&title, &body),
        seeds = [IDEA_SEED, &config.next_id.to_le_bytes()],
        bump,
    )]
    pub idea: Account<'info, Idea>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct GrowIdea<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [IDEA_SEED, &idea.id.to_le_bytes()], bump = idea.bump)]
    pub idea: Account<'info, Idea>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Vote<'info> {
    #[account(mut)]
    pub voter: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [IDEA_SEED, &idea.id.to_le_bytes()], bump = idea.bump)]
    pub idea: Account<'info, Idea>,
    /// One per [idea, voter]: a second vote fails because this account exists.
    #[account(init, payer = voter, space = 8 + VoteRecord::INIT_SPACE, seeds = [VOTE_SEED, idea.key().as_ref(), voter.key().as_ref()], bump)]
    pub vote: Account<'info, VoteRecord>,
    #[account(
        constraint = voter_token.mint == config.anti_mint @ IdeasError::WrongMint,
        constraint = voter_token.owner == voter.key() @ IdeasError::NotTokenOwner,
        constraint = voter_token.amount >= config.min_balance @ IdeasError::BelowMinimum,
    )]
    pub voter_token: InterfaceAccount<'info, TokenAccount>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unvote<'info> {
    #[account(mut)]
    pub voter: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: the idea the vote names; read and written only when it is still an Idea of this
    /// program (it may have been closed by the admin).
    #[account(mut, address = vote.idea @ IdeasError::WrongIdea)]
    pub idea: UncheckedAccount<'info>,
    #[account(
        mut,
        close = voter,
        seeds = [VOTE_SEED, vote.idea.as_ref(), voter.key().as_ref()],
        bump = vote.bump,
        has_one = voter @ IdeasError::NotVoter,
    )]
    pub vote: Account<'info, VoteRecord>,
}

#[derive(Accounts)]
pub struct AdminIdea<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ IdeasError::NotAdmin)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [IDEA_SEED, &idea.id.to_le_bytes()], bump = idea.bump)]
    pub idea: Account<'info, Idea>,
}

#[derive(Accounts)]
pub struct CloseIdea<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ IdeasError::NotAdmin)]
    pub config: Account<'info, Config>,
    #[account(mut, close = author, seeds = [IDEA_SEED, &idea.id.to_le_bytes()], bump = idea.bump, has_one = author @ IdeasError::NotAuthor)]
    pub idea: Account<'info, Idea>,
    /// CHECK: receives the rent; must be the idea's author (has_one above).
    #[account(mut)]
    pub author: UncheckedAccount<'info>,
}

// ---------- events ----------

#[event]
pub struct ConfigSet {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub anti_mint: Pubkey,
    pub min_balance: u64,
    pub threshold: u32,
    pub paused: bool,
}

#[event]
pub struct IdeaSuggested {
    pub idea: Pubkey,
    pub id: u64,
    pub author: Pubkey,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    /// The author's platform-token atoms at the time (what the check saw).
    pub balance: u64,
}

#[event]
pub struct IdeaForked {
    pub idea: Pubkey,
    pub id: u64,
    /// The forked idea's account and id.
    pub parent: Pubkey,
    pub parent_id: u64,
    pub author: Pubkey,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub balance: u64,
}

#[event]
pub struct IdeaSeeded {
    pub idea: Pubkey,
    pub id: u64,
    pub author: Pubkey,
    pub title: String,
    pub body: String,
    pub status: IdeaStatus,
    pub note: String,
    pub created_at: i64,
}

#[event]
pub struct IdeaGrown {
    pub idea: Pubkey,
    pub id: u64,
    pub from: u32,
    pub to: u32,
}

#[event]
pub struct Voted {
    pub idea: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub votes: u32,
    pub status: IdeaStatus,
    pub balance: u64,
    pub at: i64,
}

#[event]
pub struct ThresholdReached {
    pub idea: Pubkey,
    pub id: u64,
    pub votes: u32,
    pub threshold: u32,
    pub at: i64,
}

#[event]
pub struct Unvoted {
    pub idea: Pubkey,
    /// u64::MAX when the idea was already closed.
    pub id: u64,
    pub voter: Pubkey,
    pub votes: u32,
    pub status: Option<IdeaStatus>,
    pub idea_closed: bool,
    pub at: i64,
}

#[event]
pub struct StatusSet {
    pub idea: Pubkey,
    pub id: u64,
    pub from: IdeaStatus,
    pub to: IdeaStatus,
    pub note: String,
    pub at: i64,
}

#[event]
pub struct IdeaClosed {
    pub idea: Pubkey,
    pub id: u64,
    pub author: Pubkey,
    pub votes: u32,
    pub status: IdeaStatus,
    pub at: i64,
}

#[error_code]
pub enum IdeasError {
    #[msg("Only the admin may do this")]
    NotAdmin,
    #[msg("Suggestions and votes are paused")]
    Paused,
    #[msg("The title is empty")]
    TitleEmpty,
    #[msg("The title is over 80 bytes")]
    TitleTooLong,
    #[msg("The body is over 600 bytes")]
    BodyTooLong,
    #[msg("The note is over 200 bytes")]
    NoteTooLong,
    #[msg("Rejecting needs a reason")]
    ReasonRequired,
    #[msg("The token account is not of the platform token")]
    WrongMint,
    #[msg("The token account is not the signer's")]
    NotTokenOwner,
    #[msg("Below the minimum platform-token balance")]
    BelowMinimum,
    #[msg("Voting is closed for this idea")]
    VotingClosed,
    #[msg("That status change is not allowed")]
    BadTransition,
    #[msg("The threshold must be 1 to 1,000,000")]
    BadThreshold,
    #[msg("The minimum balance must be above 0")]
    BadMinBalance,
    #[msg("Overflow")]
    Overflow,
    #[msg("Not the proposed admin")]
    NotPendingAdmin,
    #[msg("The idea is not the one the vote names")]
    WrongIdea,
    #[msg("Not the voter")]
    NotVoter,
    #[msg("Not the idea's author")]
    NotAuthor,
    #[msg("A fork cannot be forked (one level)")]
    ForkOfFork,
}
