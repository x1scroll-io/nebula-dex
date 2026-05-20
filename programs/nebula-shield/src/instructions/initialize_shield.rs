// Nebula Shield — Initialize Shield Config
//
// Sets up ShieldConfig + GlobalShieldState for a pool.
// Called once per pool by the protocol admin.

use anchor_lang::prelude::*;

use crate::state::{GlobalShieldState, ShieldConfig};
use crate::error::ShieldError;

/// TiPy treasury address — hardcoded to prevent admin rerouting fees
pub const TIPY_TREASURY: Pubkey = pubkey!("TiPy76viRMRTcKsZMfNp9enh2cCfaUXg3LPdjtpmBDu");

/// Config parameters for initializing a pool's shield
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ShieldInitConfig {
    pub arb_authority: Pubkey,
    pub min_spread_bps: u16,
    pub treasury_share_bps: u16,
    pub twap_window_seconds: u32,
    pub min_sweep_amount: u64,
    pub max_sweep_amount: u64,
    pub cooldown_slots: u64,
    pub manipulation_threshold_bps: u16,
    pub min_jit_lock_slots: u16,
}

pub fn handler(ctx: Context<InitializeShield>, config: ShieldInitConfig) -> Result<()> {
    // Validate params
    require!(
        config.treasury_share_bps <= 10_000,
        ShieldError::InvalidShareBps
    );
    require!(config.min_spread_bps > 0, ShieldError::ZeroSpreadBps);
    require!(
        config.min_sweep_amount < config.max_sweep_amount,
        ShieldError::InvalidSweepRange
    );
    require!(
        config.twap_window_seconds >= 15,
        ShieldError::TwapWindowTooShort
    );
    require!(
        config.min_jit_lock_slots <= crate::state::JitGuardState::MAX_LOCK_SLOTS,
        ShieldError::LockSlotsExceedMax
    );

    // Populate ShieldConfig
    let shield = &mut ctx.accounts.shield_config;
    shield.pool = ctx.accounts.pool.key();
    shield.arb_authority = config.arb_authority;
    shield.treasury = TIPY_TREASURY;
    shield.min_spread_bps = config.min_spread_bps;
    shield.treasury_share_bps = config.treasury_share_bps;
    shield.twap_window_seconds = config.twap_window_seconds;
    shield.min_sweep_amount = config.min_sweep_amount;
    shield.max_sweep_amount = config.max_sweep_amount;
    shield.last_sweep_slot = 0;
    shield.cooldown_slots = config.cooldown_slots;
    shield.manipulation_threshold_bps = config.manipulation_threshold_bps;
    shield.min_jit_lock_slots = if config.min_jit_lock_slots == 0 {
        crate::state::JitGuardState::DEFAULT_MIN_LOCK_SLOTS
    } else {
        config.min_jit_lock_slots
    };
    shield.total_profit_captured_a = 0;
    shield.total_profit_captured_b = 0;
    shield.enabled = true;
    shield.bump = ctx.bumps.shield_config;

    // Update global stats
    let global = &mut ctx.accounts.global_shield;
    global.pools_protected = global.pools_protected.saturating_add(1);

    emit!(ShieldInitialized {
        pool: ctx.accounts.pool.key(),
        arb_authority: config.arb_authority,
        min_spread_bps: config.min_spread_bps,
        enabled: true,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct InitializeShield<'info> {
    /// Protocol admin — must match global_shield.admin
    #[account(
        mut,
        constraint = admin.key() == global_shield.admin @ ShieldError::NotAdmin,
    )]
    pub admin: Signer<'info>,

    /// CHECK: Pool pubkey — used as seed. Caller supplies the pool's address.
    pub pool: AccountInfo<'info>,

    /// Shield config PDA for this pool
    #[account(
        init,
        payer = admin,
        space = ShieldConfig::LEN,
        seeds = [b"shield_config", pool.key().as_ref()],
        bump,
    )]
    pub shield_config: Account<'info, ShieldConfig>,

    /// Global program stats
    #[account(
        mut,
        seeds = [b"global_shield"],
        bump = global_shield.bump,
    )]
    pub global_shield: Account<'info, GlobalShieldState>,

    pub system_program: Program<'info, System>,
}

#[event]
pub struct ShieldInitialized {
    pub pool: Pubkey,
    pub arb_authority: Pubkey,
    pub min_spread_bps: u16,
    pub enabled: bool,
}
