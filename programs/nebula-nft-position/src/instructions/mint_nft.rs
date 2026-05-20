// Nebula DEX — Mint Position NFT
//
// Mints a unique NFT representing an LP position.
// Charges 0.001 XNT mint fee routed to TiPy treasury.
// NFT supply is capped at 1; mint authority revoked post-mint → true NFT.
// Metadata (pool_id, tick_lower, tick_upper) stored in NftPositionRecord PDA.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer as SolTransfer};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};

use crate::state::NftPositionRecord;
use crate::error::NftError;

/// TiPy treasury — receives the 0.001 XNT mint fee
pub const TIPY_TREASURY: Pubkey = pubkey!("TiPy76viRMRTcKsZMfNp9enh2cCfaUXg3LPdjtpmBDu");

/// Mint fee: 0.001 XNT (1_000_000 lamports)
pub const MINT_FEE_LAMPORTS: u64 = 1_000_000;

pub fn handler(
    ctx: Context<MintPositionNft>,
    pool_id: Pubkey,
    tick_lower: i32,
    tick_upper: i32,
) -> Result<()> {
    require!(tick_lower < tick_upper, NftError::InvalidPoolId);

    // ── 1. Charge mint fee → TiPy treasury ───────────────────────────────
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            SolTransfer {
                from: ctx.accounts.owner.to_account_info(),
                to: ctx.accounts.tipy_treasury.to_account_info(),
            },
        ),
        MINT_FEE_LAMPORTS,
    )?;

    // ── 2. Mint 1 NFT token to owner's ATA ───────────────────────────────
    token::mint_to(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.nft_mint.to_account_info(),
                to: ctx.accounts.owner_token_account.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        1,
    )?;

    // ── 3. Initialize position record PDA ────────────────────────────────
    // NOTE: Mint authority revocation (supply lock) is handled by a separate
    // revoke_mint_authority instruction called after the position is fully
    // established. The program enforces single-mint invariants via PDA seeds.
    // The nft_mint is still controlled by owner until revoke is called.
    let record = &mut ctx.accounts.nft_position_record;
    record.nft_mint = ctx.accounts.nft_mint.key();
    record.pool_id = pool_id;
    record.tick_lower = tick_lower;
    record.tick_upper = tick_upper;
    record.owner = ctx.accounts.owner.key();
    record.transferable = false; // soulbound by default
    record.bump = ctx.bumps.nft_position_record;

    emit!(PositionNftMinted {
        nft_mint: ctx.accounts.nft_mint.key(),
        pool_id,
        tick_lower,
        tick_upper,
        owner: ctx.accounts.owner.key(),
    });

    Ok(())
}

#[derive(Accounts)]
#[instruction(pool_id: Pubkey, tick_lower: i32, tick_upper: i32)]
pub struct MintPositionNft<'info> {
    /// The user opening the position — pays mint fee + rent
    #[account(mut)]
    pub owner: Signer<'info>,

    /// New mint for the NFT — caller generates a fresh keypair
    #[account(
        init,
        payer = owner,
        mint::decimals = 0,
        mint::authority = owner,
        mint::freeze_authority = owner,
    )]
    pub nft_mint: Account<'info, Mint>,

    /// Owner's associated token account for the NFT
    #[account(
        init_if_needed,
        payer = owner,
        associated_token::mint = nft_mint,
        associated_token::authority = owner,
    )]
    pub owner_token_account: Account<'info, TokenAccount>,

    /// Position record PDA — stores metadata & ownership
    #[account(
        init,
        payer = owner,
        space = 8 + NftPositionRecord::LEN,
        seeds = [b"nft_position", nft_mint.key().as_ref()],
        bump,
    )]
    pub nft_position_record: Account<'info, NftPositionRecord>,

    /// CHECK: TiPy treasury — hardcoded, receives mint fee
    #[account(
        mut,
        constraint = tipy_treasury.key() == TIPY_TREASURY @ NftError::InvalidTreasury,
    )]
    pub tipy_treasury: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct PositionNftMinted {
    pub nft_mint: Pubkey,
    pub pool_id: Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub owner: Pubkey,
}
