// Nebula DEX — Transfer Position NFT
//
// Transfers an LP position NFT to a new owner.
// Only allowed when nft_position_record.transferable == true.
// Owner signs the transfer; new_owner's ATA is created if needed.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer as TokenTransfer};

use crate::state::NftPositionRecord;
use crate::error::NftError;

pub fn handler(ctx: Context<TransferPosition>) -> Result<()> {
    let record = &mut ctx.accounts.nft_position_record;

    require!(record.transferable, NftError::NftNotTransferable);
    require!(
        record.owner == ctx.accounts.owner.key(),
        NftError::NotPositionOwner
    );

    let new_owner = ctx.accounts.new_owner.key();

    // Transfer the NFT token to new owner's ATA
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TokenTransfer {
                from: ctx.accounts.owner_token_account.to_account_info(),
                to: ctx.accounts.new_owner_token_account.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        1,
    )?;

    // Update ownership record
    let old_owner = record.owner;
    record.owner = new_owner;

    emit!(PositionTransferred {
        nft_mint: ctx.accounts.nft_mint.key(),
        pool_id: record.pool_id,
        from: old_owner,
        to: new_owner,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct TransferPosition<'info> {
    /// Current position owner — signs the transfer
    #[account(mut)]
    pub owner: Signer<'info>,

    /// The NFT mint
    pub nft_mint: Account<'info, Mint>,

    /// Owner's token account — holds the NFT
    #[account(
        mut,
        associated_token::mint = nft_mint,
        associated_token::authority = owner,
    )]
    pub owner_token_account: Account<'info, TokenAccount>,

    /// New owner's wallet (pubkey only — must be a valid account)
    /// CHECK: pubkey used to derive new_owner_token_account ATA
    pub new_owner: AccountInfo<'info>,

    /// New owner's ATA — created if it doesn't exist
    #[account(
        init_if_needed,
        payer = owner,
        associated_token::mint = nft_mint,
        associated_token::authority = new_owner,
    )]
    pub new_owner_token_account: Account<'info, TokenAccount>,

    /// Position record — ownership updated in place
    #[account(
        mut,
        seeds = [b"nft_position", nft_mint.key().as_ref()],
        bump = nft_position_record.bump,
    )]
    pub nft_position_record: Account<'info, NftPositionRecord>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct PositionTransferred {
    pub nft_mint: Pubkey,
    pub pool_id: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
}
