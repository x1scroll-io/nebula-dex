// Nebula Shield — Update Shield Config
//
// Admin instruction to update pool-level shield thresholds and parameters.
// Only the pool's arb_authority can update its own config.

use anchor_lang::prelude::*;

use crate::state::ShieldConfig;
use crate::error::ShieldError;

/// Fields that can be updated. None = leave unchanged.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ShieldConfigUpdate {
    pub min_spread_bps: Option<u16>,
    pub treasury_share_bps: Option<u16>,
    pub twap_window_seconds: Option<u32>,
    pub min_sweep_amount: Option<u64>,
    pub max_sweep_amount: Option<u64>,
    pub cooldown_slots: Option<u64>,
    pub manipulation_threshold_bps: Option<u16>,
    pub min_jit_lock_slots: Option<u16>,
    pub enabled: Option<bool>,
}

pub fn handler(ctx: Context<UpdateShieldConfig>, new_config: ShieldConfigUpdate) -> Result<()> {
    let shield = &mut ctx.accounts.shield_config;

    if let Some(min_spread_bps) = new_config.min_spread_bps {
        require!(min_spread_bps > 0, ShieldError::ZeroSpreadBps);
        shield.min_spread_bps = min_spread_bps;
    }

    if let Some(treasury_share_bps) = new_config.treasury_share_bps {
        require!(
            treasury_share_bps <= 10_000,
            ShieldError::InvalidShareBps
        );
        shield.treasury_share_bps = treasury_share_bps;
    }

    if let Some(twap_window_seconds) = new_config.twap_window_seconds {
        require!(
            twap_window_seconds >= 15,
            ShieldError::TwapWindowTooShort
        );
        shield.twap_window_seconds = twap_window_seconds;
    }

    if let Some(min_sweep_amount) = new_config.min_sweep_amount {
        shield.min_sweep_amount = min_sweep_amount;
    }

    if let Some(max_sweep_amount) = new_config.max_sweep_amount {
        shield.max_sweep_amount = max_sweep_amount;
    }

    // Re-validate range if both were updated
    require!(
        shield.min_sweep_amount < shield.max_sweep_amount,
        ShieldError::InvalidSweepRange
    );

    if let Some(cooldown_slots) = new_config.cooldown_slots {
        shield.cooldown_slots = cooldown_slots;
    }

    if let Some(manipulation_threshold_bps) = new_config.manipulation_threshold_bps {
        shield.manipulation_threshold_bps = manipulation_threshold_bps;
    }

    if let Some(min_jit_lock_slots) = new_config.min_jit_lock_slots {
        require!(
            min_jit_lock_slots <= crate::state::JitGuardState::MAX_LOCK_SLOTS,
            ShieldError::LockSlotsExceedMax
        );
        shield.min_jit_lock_slots = min_jit_lock_slots;
    }

    if let Some(enabled) = new_config.enabled {
        shield.enabled = enabled;
    }

    emit!(ShieldConfigUpdated {
        pool: shield.pool,
        enabled: shield.enabled,
        min_spread_bps: shield.min_spread_bps,
        cooldown_slots: shield.cooldown_slots,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct UpdateShieldConfig<'info> {
    /// Arb authority — must match shield_config.arb_authority
    #[account(
        constraint = arb_authority.key() == shield_config.arb_authority @ ShieldError::NotArbAuthority,
    )]
    pub arb_authority: Signer<'info>,

    /// CHECK: Pool pubkey — seed only
    pub pool: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"shield_config", pool.key().as_ref()],
        bump = shield_config.bump,
        constraint = shield_config.pool == pool.key() @ ShieldError::PoolMismatch,
    )]
    pub shield_config: Account<'info, ShieldConfig>,
}

#[event]
pub struct ShieldConfigUpdated {
    pub pool: Pubkey,
    pub enabled: bool,
    pub min_spread_bps: u16,
    pub cooldown_slots: u64,
}
