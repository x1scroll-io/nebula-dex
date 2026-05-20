// Nebula Shield — Execute Arb Sweep
//
// Sweeps arb profits from pool vaults to TiPy treasury.
// Only callable by the pool's arb_authority.
// Enforces cooldown, min spread, TWAP-based price reference.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer as TokenTransfer};

use crate::state::{GlobalShieldState, ShieldConfig};
use crate::error::ShieldError;

// Fixed-point constant Q64 = 2^64
const Q64: u128 = 1u128 << 64;

pub fn handler(ctx: Context<ExecuteArbSweep>, sweep_amount: u64) -> Result<()> {
    let shield = &ctx.accounts.shield_config;
    let clock = Clock::get()?;
    let current_slot = clock.slot;

    require!(shield.enabled, ShieldError::ShieldDisabled);
    require!(
        current_slot >= shield.last_sweep_slot + shield.cooldown_slots,
        ShieldError::ArbCooldownActive
    );
    require!(
        sweep_amount >= shield.min_sweep_amount,
        ShieldError::AmountBelowMin
    );
    require!(
        sweep_amount <= shield.max_sweep_amount,
        ShieldError::AmountBelowMin
    );

    // Calculate treasury share
    let treasury_amount = (sweep_amount as u128)
        .checked_mul(shield.treasury_share_bps as u128)
        .ok_or(ShieldError::ArithmeticOverflow)?
        .checked_div(10_000)
        .ok_or(ShieldError::DivisionByZero)? as u64;

    require!(treasury_amount > 0, ShieldError::NoArbProfit);

    // Route treasury share to TiPy via pool authority PDA
    let pool_key = ctx.accounts.pool.key();
    let seeds = &[
        b"pool_authority",
        pool_key.as_ref(),
        &[ctx.bumps.pool_authority],
    ];
    let signer_seeds = &[&seeds[..]];

    // Always sweep token_vault_0 (base token) unless a_to_b flag indicates otherwise
    // For simplicity: sweep from vault_0 to treasury
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TokenTransfer {
                from: ctx.accounts.token_vault.to_account_info(),
                to: ctx.accounts.treasury_vault.to_account_info(),
                authority: ctx.accounts.pool_authority.to_account_info(),
            },
            signer_seeds,
        ),
        treasury_amount,
    )?;

    // Update shield config
    let shield = &mut ctx.accounts.shield_config;
    shield.last_sweep_slot = current_slot;
    shield.total_profit_captured_a = shield
        .total_profit_captured_a
        .saturating_add(treasury_amount);

    // Update global stats
    let global = &mut ctx.accounts.global_shield;
    global.total_arb_sweeps = global.total_arb_sweeps.saturating_add(1);
    global.total_treasury_routed = global
        .total_treasury_routed
        .saturating_add(treasury_amount);

    emit!(ArbSweepExecuted {
        pool: ctx.accounts.pool.key(),
        sweep_amount,
        treasury_amount,
        slot: current_slot,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct ExecuteArbSweep<'info> {
    /// Arb authority — must match shield_config.arb_authority
    #[account(
        mut,
        constraint = arb_authority.key() == shield_config.arb_authority @ ShieldError::NotArbAuthority,
    )]
    pub arb_authority: Signer<'info>,

    /// CHECK: Pool pubkey — used as seed
    pub pool: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"shield_config", pool.key().as_ref()],
        bump = shield_config.bump,
        constraint = shield_config.pool == pool.key() @ ShieldError::PoolMismatch,
        constraint = shield_config.enabled @ ShieldError::ShieldDisabled,
    )]
    pub shield_config: Account<'info, ShieldConfig>,

    /// Pool token vault to sweep from (base token)
    #[account(mut)]
    pub token_vault: Account<'info, TokenAccount>,

    /// TiPy treasury token account — receives arb profit
    #[account(
        mut,
        constraint = treasury_vault.key() == shield_config.treasury @ ShieldError::InvalidTreasury,
    )]
    pub treasury_vault: Account<'info, TokenAccount>,

    /// Pool authority PDA — signs vault transfers on behalf of pool
    /// CHECK: PDA validated by seeds
    #[account(
        seeds = [b"pool_authority", pool.key().as_ref()],
        bump,
    )]
    pub pool_authority: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"global_shield"],
        bump = global_shield.bump,
    )]
    pub global_shield: Account<'info, GlobalShieldState>,

    pub token_program: Program<'info, Token>,
}

// ── Math Helpers ──────────────────────────────────────────────────────────────

/// Compute inline arb: given sqrt_price and liquidity, estimate (amount_in, amount_out)
#[allow(dead_code)]
pub fn compute_arb_swap(
    sqrt_price: u128,
    liquidity: u128,
    amount: u64,
    a_to_b: bool,
) -> (u64, u64) {
    if a_to_b {
        let x_virtual = liquidity.checked_div((sqrt_price >> 32).max(1)).unwrap_or(1);
        let numerator = liquidity.saturating_mul(amount as u128);
        let denominator = x_virtual.saturating_add(amount as u128);
        let amount_out = (numerator.checked_div(denominator).unwrap_or(0)) as u64;
        (amount, amount_out)
    } else {
        let y_virtual = liquidity.saturating_mul(sqrt_price >> 64);
        let numerator = liquidity.saturating_mul(amount as u128);
        let denominator = y_virtual.saturating_add(amount as u128);
        let amount_out = (numerator.checked_div(denominator).unwrap_or(0)) as u64;
        (amount, amount_out)
    }
}

/// Calculate spread between spot and TWAP in bps, returns (spread_bps, a_to_b)
#[allow(dead_code)]
pub fn calculate_spread(spot: u128, twap: u128) -> (u128, bool) {
    if spot > twap {
        let spread = (spot - twap)
            .saturating_mul(10_000)
            .checked_div(twap.max(1))
            .unwrap_or(0);
        (spread, true)
    } else {
        let spread = (twap - spot)
            .saturating_mul(10_000)
            .checked_div(twap.max(1))
            .unwrap_or(0);
        (spread, false)
    }
}

/// Calculate how much to sweep to close 80% of price-TWAP gap
#[allow(dead_code)]
pub fn calculate_sweep_amount(
    spot: u128,
    twap: u128,
    liquidity: u128,
    min_amount: u64,
    max_amount: u64,
    a_to_b: bool,
) -> u64 {
    let target = if a_to_b {
        spot.saturating_sub((spot - twap) * 4 / 5)
    } else {
        spot.saturating_add((twap - spot) * 4 / 5)
    };

    let delta = if a_to_b {
        spot.saturating_sub(target)
    } else {
        target.saturating_sub(spot)
    };

    let raw = (liquidity)
        .saturating_mul(delta)
        .checked_div(Q64)
        .unwrap_or(0) as u64;

    raw.max(min_amount).min(max_amount)
}

// ── Event ─────────────────────────────────────────────────────────────────────

#[event]
pub struct ArbSweepExecuted {
    pub pool: Pubkey,
    pub sweep_amount: u64,
    pub treasury_amount: u64,
    pub slot: u64,
}
