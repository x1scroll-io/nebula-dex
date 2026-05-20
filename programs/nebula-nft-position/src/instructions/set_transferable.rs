// Nebula DEX — Set Position Transferability
//
// Admin-only instruction to toggle whether a position NFT can be traded.
// Default is soulbound (false). Enabling transfer unlocks position trading.

use anchor_lang::prelude::*;

use crate::state::NftPositionRecord;
use crate::error::NftError;

/// Nebula protocol admin — replace with actual admin pubkey at deploy
/// Uses TiPy treasury as placeholder; governance will manage this.
pub const NEBULA_ADMIN: Pubkey = pubkey!("TiPy76viRMRTcKsZMfNp9enh2cCfaUXg3LPdjtpmBDu");

pub fn handler(ctx: Context<SetTransferable>, transferable: bool) -> Result<()> {
    ctx.accounts.nft_position_record.transferable = transferable;

    emit!(TransferabilityChanged {
        nft_mint: ctx.accounts.nft_position_record.nft_mint,
        transferable,
        admin: ctx.accounts.admin.key(),
    });

    Ok(())
}

#[derive(Accounts)]
pub struct SetTransferable<'info> {
    /// Protocol admin — must match NEBULA_ADMIN
    #[account(
        constraint = admin.key() == NEBULA_ADMIN @ NftError::Unauthorized,
    )]
    pub admin: Signer<'info>,

    /// Position record to update
    #[account(mut)]
    pub nft_position_record: Account<'info, NftPositionRecord>,
}

#[event]
pub struct TransferabilityChanged {
    pub nft_mint: Pubkey,
    pub transferable: bool,
    pub admin: Pubkey,
}
