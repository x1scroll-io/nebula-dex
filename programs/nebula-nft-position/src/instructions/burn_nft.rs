// Nebula DEX — Burn Position NFT
//
// Burns the NFT and closes the NftPositionRecord when a position is closed.
// Only the position owner can burn.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount};

use crate::state::NftPositionRecord;
use crate::error::NftError;

pub fn handler(ctx: Context<BurnPositionNft>) -> Result<()> {
    let record = &ctx.accounts.nft_position_record;
    require!(
        record.owner == ctx.accounts.owner.key(),
        NftError::NotPositionOwner
    );

    // Burn the 1 NFT token
    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.nft_mint.to_account_info(),
                from: ctx.accounts.owner_token_account.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        1,
    )?;

    // NftPositionRecord is closed by Anchor (close = owner) — lamports returned

    emit!(PositionNftBurned {
        nft_mint: ctx.accounts.nft_mint.key(),
        pool_id: record.pool_id,
        owner: ctx.accounts.owner.key(),
    });

    Ok(())
}

#[derive(Accounts)]
pub struct BurnPositionNft<'info> {
    /// Position owner — must match record.owner
    #[account(mut)]
    pub owner: Signer<'info>,

    /// The NFT mint
    #[account(mut)]
    pub nft_mint: Account<'info, Mint>,

    /// Owner's token account holding the NFT
    #[account(
        mut,
        associated_token::mint = nft_mint,
        associated_token::authority = owner,
    )]
    pub owner_token_account: Account<'info, TokenAccount>,

    /// Position record — closed on burn, lamports → owner
    #[account(
        mut,
        seeds = [b"nft_position", nft_mint.key().as_ref()],
        bump = nft_position_record.bump,
        close = owner,
    )]
    pub nft_position_record: Account<'info, NftPositionRecord>,

    pub token_program: Program<'info, Token>,
}

#[event]
pub struct PositionNftBurned {
    pub nft_mint: Pubkey,
    pub pool_id: Pubkey,
    pub owner: Pubkey,
}
