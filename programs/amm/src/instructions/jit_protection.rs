// Nebula DEX — JIT Liquidity Protection Instructions
// Adapted from Theo (@xxen_bot) contribution for nebula-dex-fork module structure.
// Initializes and checks the JIT guard on every add/remove liquidity.

use anchor_lang::prelude::*;
use crate::states::{PoolState, PersonalPositionState, JitGuard};
use crate::error::ErrorCode;

/// Initialize JIT guard for a position (called when first adding liquidity)
#[inline(never)]
pub fn handler_init(ctx: Context<InitJitGuard>, min_lock_slots: u16) -> Result<()> {
    require!(
        min_lock_slots <= JitGuard::MAX_LOCK_SLOTS,
        ErrorCode::InvalidTickIndex // reusing closest available error
    );

    let guard = &mut ctx.accounts.jit_guard;
    guard.pool = ctx.accounts.pool.key();
    guard.position_nft_mint = ctx.accounts.position_nft_mint.key();
    guard.min_lock_slots = if min_lock_slots == 0 {
        JitGuard::DEFAULT_MIN_LOCK_SLOTS
    } else {
        min_lock_slots
    };
    guard.last_add_slot = Clock::get()?.slot;
    guard.add_count = 1;
    guard.remove_count = 0;
    guard.jit_blocks = 0;
    guard.bump = ctx.bumps.jit_guard;

    Ok(())
}

/// Validate that a liquidity removal is not a JIT attack
#[inline(never)]
pub fn handler_check_remove(ctx: Context<CheckJitGuard>) -> Result<()> {
    let guard = &mut ctx.accounts.jit_guard;
    let current_slot = Clock::get()?.slot;
    let (can_remove, slots_remaining) = guard.can_remove(current_slot);

    if !can_remove {
        guard.record_remove(current_slot, true);
        msg!(
            "JIT protection: removal blocked. {} slots remaining until unlock.",
            slots_remaining
        );
        return err!(ErrorCode::JitBackRunDetected);
    }

    guard.record_remove(current_slot, false);
    Ok(())
}

/// Record that liquidity was added (update last_add_slot)
#[inline(never)]
pub fn handler_record_add(ctx: Context<RecordJitAdd>) -> Result<()> {
    let guard = &mut ctx.accounts.jit_guard;
    guard.record_add(Clock::get()?.slot);
    Ok(())
}

#[derive(Accounts)]
pub struct InitJitGuard<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    /// Pool (zero_copy) — AccountLoader required
    pub pool: AccountLoader<'info, PoolState>,
    /// CHECK: NFT mint pubkey identifying the position
    pub position_nft_mint: AccountInfo<'info>,
    #[account(
        init,
        payer = owner,
        space = JitGuard::LEN,
        seeds = [b"jit_guard", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump
    )]
    pub jit_guard: Account<'info, JitGuard>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CheckJitGuard<'info> {
    pub owner: Signer<'info>,
    pub pool: AccountLoader<'info, PoolState>,
    /// CHECK: NFT mint pubkey identifying the position
    pub position_nft_mint: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"jit_guard", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump = jit_guard.bump
    )]
    pub jit_guard: Account<'info, JitGuard>,
}

#[derive(Accounts)]
pub struct RecordJitAdd<'info> {
    pub owner: Signer<'info>,
    pub pool: AccountLoader<'info, PoolState>,
    /// CHECK: NFT mint pubkey identifying the position
    pub position_nft_mint: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"jit_guard", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump = jit_guard.bump
    )]
    pub jit_guard: Account<'info, JitGuard>,
}
