// Nebula Shield — Standalone MEV/JIT/Arb Protection Program
//
// Three-layer defense extracted from the Nebula AMM into a standalone deployable program:
//   Layer 1: Pre-swap arb sweep — protocol captures spread before any external bot
//   Layer 2: Same-slot manipulation detection
//   Layer 3: Post-swap JIT lock window (anti back-run)
//
// Can be called via CPI from the Nebula AMM or any integrating program.
// All arb profits route to TiPy treasury: TiPy76viRMRTcKsZMfNp9enh2cCfaUXg3LPdjtpmBDu
//
// Program ID: replace with deployed address — this is a placeholder.

pub mod cpi_helpers;
pub mod error;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;
use instructions::*;
use state::*;

// Placeholder ID — Anchor example pubkey, replace at deploy.
declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

#[program]
pub mod nebula_shield {
    use super::*;

    /// Initialize the global shield state (one-time program setup).
    /// Must be called before any pool shields can be created.
    pub fn initialize_global(ctx: Context<InitializeGlobal>) -> Result<()> {
        let global = &mut ctx.accounts.global_shield;
        global.admin = ctx.accounts.admin.key();
        global.pools_protected = 0;
        global.total_jit_blocks = 0;
        global.total_arb_sweeps = 0;
        global.total_treasury_routed = 0;
        global.bump = ctx.bumps.global_shield;
        Ok(())
    }

    /// Initialize Nebula Shield for a pool.
    /// Creates ShieldConfig PDA. Admin only.
    pub fn initialize_shield(ctx: Context<InitializeShield>, config: ShieldInitConfig) -> Result<()> {
        instructions::initialize_shield::handler(ctx, config)
    }

    /// Initialize JIT guard for a new LP position.
    pub fn init_jit_guard(ctx: Context<InitJitGuard>, min_lock_slots: u16) -> Result<()> {
        instructions::check_jit::handler_init_guard(ctx, min_lock_slots)
    }

    /// Check JIT protection before a liquidity removal or swap.
    /// Blocks and returns error if within the lock window.
    pub fn check_jit_protection(
        ctx: Context<CheckJitProtection>,
        swap_amount: u64,
        min_amount_out: u64,
    ) -> Result<()> {
        instructions::check_jit::handler_check_jit(ctx, swap_amount, min_amount_out)
    }

    /// Record that liquidity was added — resets the JIT lock window.
    pub fn record_jit_add(ctx: Context<RecordJitAdd>) -> Result<()> {
        instructions::check_jit::handler_record_add(ctx)
    }

    /// Execute an arb sweep: route treasury share of sweep_amount to TiPy.
    pub fn execute_arb_sweep(ctx: Context<ExecuteArbSweep>, sweep_amount: u64) -> Result<()> {
        instructions::arb_sweep::handler(ctx, sweep_amount)
    }

    /// Update shield configuration for a pool. arb_authority only.
    pub fn update_shield_config(
        ctx: Context<UpdateShieldConfig>,
        new_config: ShieldConfigUpdate,
    ) -> Result<()> {
        instructions::update_config::handler(ctx, new_config)
    }

    /// Get current shield status — emits ShieldStatusEvent for UI consumers.
    pub fn get_shield_status(ctx: Context<GetShieldStatus>) -> Result<()> {
        let shield = &ctx.accounts.shield_config;
        let global = &ctx.accounts.global_shield;

        emit!(ShieldStatusEvent {
            pool: shield.pool,
            enabled: shield.enabled,
            min_spread_bps: shield.min_spread_bps,
            cooldown_slots: shield.cooldown_slots,
            last_sweep_slot: shield.last_sweep_slot,
            total_profit_a: shield.total_profit_captured_a,
            total_profit_b: shield.total_profit_captured_b,
            pools_protected: global.pools_protected,
            total_jit_blocks: global.total_jit_blocks,
            total_arb_sweeps: global.total_arb_sweeps,
        });

        let shield_enabled = shield.enabled as u8;
        anchor_lang::solana_program::program::set_return_data(&[shield_enabled]);

        Ok(())
    }
}

// ── One-time global init ──────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeGlobal<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = GlobalShieldState::LEN,
        seeds = [b"global_shield"],
        bump,
    )]
    pub global_shield: Account<'info, GlobalShieldState>,

    pub system_program: Program<'info, System>,
}

// ── Get Shield Status ─────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct GetShieldStatus<'info> {
    /// CHECK: Pool pubkey — seed only
    pub pool: AccountInfo<'info>,

    #[account(
        seeds = [b"shield_config", pool.key().as_ref()],
        bump = shield_config.bump,
    )]
    pub shield_config: Account<'info, ShieldConfig>,

    #[account(
        seeds = [b"global_shield"],
        bump = global_shield.bump,
    )]
    pub global_shield: Account<'info, GlobalShieldState>,
}

// ── Events ────────────────────────────────────────────────────────────────────

#[event]
pub struct ShieldStatusEvent {
    pub pool: Pubkey,
    pub enabled: bool,
    pub min_spread_bps: u16,
    pub cooldown_slots: u64,
    pub last_sweep_slot: u64,
    pub total_profit_a: u64,
    pub total_profit_b: u64,
    pub pools_protected: u64,
    pub total_jit_blocks: u64,
    pub total_arb_sweeps: u64,
}
