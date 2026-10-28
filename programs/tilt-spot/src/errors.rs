use anchor_lang::prelude::*;

#[error_code]
pub enum SpotError {
    #[msg("Signer is not the admin")]
    NotAdmin,
    #[msg("A parameter is outside its hard cap")]
    BadParam,
    #[msg("Trading through the router is paused")]
    Paused,
    #[msg("Math overflow")]
    Math,
    #[msg("An account is not the one this needs")]
    WrongAccount,
    #[msg("The coin was not launched on a counted config")]
    NotPadCoin,
    #[msg("The platform token does not trade through the router (unused since 2026-10-23: it does)")]
    PlatformToken,
    #[msg("The coin is not in the phase this venue needs")]
    WrongPhase,
    #[msg("The swap moved nothing")]
    NoFill,
    #[msg("Platform token not set")]
    NoToken,
    #[msg("Platform token already set")]
    TokenExists,
    #[msg("levcap's minter or mint authority is not this program's")]
    NotOurMinter,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("levcap has no room to mint today")]
    NoMintRoom,
    #[msg("Nobody stakes (unused since 2026-10-23)")]
    NoStakers,
    #[msg("No change is pending, or its time has not come")]
    NotPending,
    #[msg("The coin's quote mint is not an allowed quote")]
    QuoteNotAllowed,
    #[msg("The quote's price leg pool has no usable price")]
    NoQuotePrice,
    #[msg("The platform token has no price yet")]
    NoPrice,
    #[msg("The receipt is still locked")]
    ReceiptLocked,
    #[msg("Only the receipt's owner may redeem it now (unused since the delay curve: anyone redeems from redeemable_from)")]
    OwnerOnly,
    #[msg("This sell owes the flip fee: pass the coin's flip-fee account (levfee [\"flip\", mint]'s account of the quote, created first)")]
    FlipAccount,
    #[msg("Only a receipt waiting for a price can be dropped, by its owner")]
    NotDroppable,
    #[msg("Only the attester may book attested receipts (none is set, or another key signed)")]
    NotAttester,
    #[msg("Attested receipts are paused")]
    AttestPaused,
    #[msg("The sell is too old to attest, or in the future")]
    AttestStale,
    #[msg("Today's attested receipt budget is spent")]
    AttestBudget,
    #[msg("The coin has not graduated (its DBC pool is not migrated or its DAMM v2 pool is not bound)")]
    NotGraduated,
    #[msg("The loss is under the receipt minimum, or receipts are off")]
    BelowMinimum,
    #[msg("Receipts are owed or waiting: a platform-token swap waits until they are redeemed")]
    ReceiptsOpen,
}

pub fn m<T>(r: std::result::Result<T, levpad_math::MathError>) -> Result<T> {
    r.map_err(|_| error!(SpotError::Math))
}
