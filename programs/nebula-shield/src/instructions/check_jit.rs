// Nebula Shield — JIT Protection Check
//
// Called by the AMM (or directly) before swap or liquidity removal.
// Records liquidity add/remove events and blocks JIT back-runs.
// Returns guard result via set_return_data for CPI callers.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;

use crate::state::{JitGuardState, ShieldConfig};
use crate::error::ShieldError;

/// Initialize a JIT guard for a new position (called on first add_liquidity)
pub fn handler_init_guard(
    ctx: Context<InitJitGuard>,
    min_lock_slots: u16,
) -> Result<()> {
    let shield = &ctx.accounts.shield_config;
    require!(shield.enabled, ShieldError::ShieldDisabled);

    let effective_lock = if min_lock_slots == 0 {
        shield.min_jit_lock_slots
    } else {
        min_lock_slots.min(JitGuardState::MAX_LOCK_SLOTS)
    };

    let guard = &mut ctx.accounts.jit_guard;
    guard.pool = ctx.accounts.pool.key();
    guard.position_nft_mint = ctx.accounts.position_nft_mint.key();
    guard.min_lock_slots = effective_lock;
    guard.last_add_slot = Clock::get()?.slot;
    guard.add_count = 1;
    guard.remove_count = 0;
    guard.jit_blocks = 0;
    guard.bump = ctx.bumps.jit_guard;

    Ok(())
}

/// Check whether a liquidity removal is a JIT attack.
/// Blocks removal if within the lock window; emits event.
/// Return data: [1] = allowed, [0] = blocked.
pub fn handler_check_jit(
    ctx: Context<CheckJitProtection>,
    _swap_amount: u64,
    _min_amount_out: u64,
) -> Result<()> {
    let guard = &mut ctx.accounts.jit_guard;
    let current_slot = Clock::get()?.slot;
    let (can_remove, slots_remaining) = guard.can_remove(current_slot);

    if !can_remove {
        guard.record_remove(current_slot, true);

        // Signal to global stats
        let global = &mut ctx.accounts.global_shield;
        global.total_jit_blocks = global.total_jit_blocks.saturating_add(1);

        msg!(
            "Nebula Shield JIT: removal blocked — {} slots remaining",
            slots_remaining
        );

        emit!(JitBlocked {
            pool: guard.pool,
            position_nft_mint: guard.position_nft_mint,
            slots_remaining,
            current_slot,
        });

        // Return data = 0 (blocked)
        set_return_data(&[0u8]);
        return err!(ShieldError::JitBackRunDetected);
    }

    guard.record_remove(current_slot, false);

    emit!(JitAllowed {
        pool: guard.pool,
        position_nft_mint: guard.position_nft_mint,
        current_slot,
    });

    // Return data = 1 (allowed)
    set_return_data(&[1u8]);
    Ok(())
}

/// Record that liquidity was added — resets the JIT lock window.
pub fn handler_record_add(ctx: Context<RecordJitAdd>) -> Result<()> {
    let guard = &mut ctx.accounts.jit_guard;
    guard.record_add(Clock::get()?.slot);
    Ok(())
}

// ── Account Structs ───────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitJitGuard<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    /// CHECK: Pool pubkey — seed only
    pub pool: AccountInfo<'info>,

    /// CHECK: NFT mint identifying this position
    pub position_nft_mint: AccountInfo<'info>,

    #[account(
        seeds = [b"shield_config", pool.key().as_ref()],
        bump = shield_config.bump,
        constraint = shield_config.pool == pool.key() @ ShieldError::PoolMismatch,
    )]
    pub shield_config: Account<'info, ShieldConfig>,

    #[account(
        init,
        payer = owner,
        space = JitGuardState::LEN,
        seeds = [b"jit_guard_state", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump,
    )]
    pub jit_guard: Account<'info, JitGuardState>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CheckJitProtection<'info> {
    pub caller: Signer<'info>,

    /// CHECK: Pool pubkey — seed only
    pub pool: AccountInfo<'info>,

    /// CHECK: NFT mint — seed only
    pub position_nft_mint: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"jit_guard_state", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump = jit_guard.bump,
    )]
    pub jit_guard: Account<'info, JitGuardState>,

    #[account(
        mut,
        seeds = [b"global_shield"],
        bump = global_shield.bump,
    )]
    pub global_shield: Account<'info, GlobalShieldState>,
}

#[derive(Accounts)]
pub struct RecordJitAdd<'info> {
    pub owner: Signer<'info>,

    /// CHECK: Pool pubkey — seed only
    pub pool: AccountInfo<'info>,

    /// CHECK: NFT mint — seed only
    pub position_nft_mint: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"jit_guard_state", pool.key().as_ref(), position_nft_mint.key().as_ref()],
        bump = jit_guard.bump,
    )]
    pub jit_guard: Account<'info, JitGuardState>,
}

// ── Events ────────────────────────────────────────────────────────────────────

#[event]
pub struct JitBlocked {
    pub pool: Pubkey,
    pub position_nft_mint: Pubkey,
    pub slots_remaining: u64,
    pub current_slot: u64,
}

#[event]
pub struct JitAllowed {
    pub pool: Pubkey,
    pub position_nft_mint: Pubkey,
    pub current_slot: u64,
}

// Re-export for use in check_jit.rs accounts
use crate::state::GlobalShieldState;
