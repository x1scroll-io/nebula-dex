// Nebula DEX — Protocol Arb Sweep
// Adapted from Theo (@xxen_bot) contribution for nebula-dex-fork module structure.
//
// First-crack arbitrage: protocol captures price imbalance before any external bot.
// Lives inside the CLMM program — no mempool exposure, no front-running possible.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::states::{PoolState, ArbConfig};
use crate::states::oracle::ObservationState;
use crate::libraries::{tick_math, fixed_point_64::Q64};
use crate::error::ErrorCode;
use crate::tipy::{TIPY_TREASURY, FEE_DENOMINATOR};

// ── Accounts ────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct ArbSweep<'info> {
    /// Protocol arb authority — only this signer can call
    #[account(mut)]
    pub arb_authority: Signer<'info>,

    /// Pool being arbed (zero_copy → AccountLoader)
    #[account(mut)]
    pub pool: AccountLoader<'info, PoolState>,

    /// Arb config for this pool
    #[account(
        mut,
        seeds = [b"arb_config", pool.key().as_ref()],
        bump = arb_config.bump,
        constraint = arb_config.pool == pool.key(),
        constraint = arb_config.arb_authority == arb_authority.key() @ ErrorCode::NotSender,
        constraint = arb_config.enabled @ ErrorCode::ArbDisabled,
        constraint = arb_config.treasury == treasury_vault.key() @ ErrorCode::InvalidTreasury,
    )]
    pub arb_config: Account<'info, ArbConfig>,

    /// TWAP oracle for reference price (zero_copy → AccountLoader)
    #[account(mut)]
    pub oracle: AccountLoader<'info, ObservationState>,

    /// Pool's token 0 vault
    #[account(mut)]
    pub token_vault_0: Account<'info, TokenAccount>,

    /// Pool's token 1 vault
    #[account(mut)]
    pub token_vault_1: Account<'info, TokenAccount>,

    /// Treasury vault — arb profit destination (TiPy token account)
    #[account(mut)]
    pub treasury_vault: Account<'info, TokenAccount>,

    /// Pool authority PDA (signs vault transfers)
    /// CHECK: PDA validated by seeds
    #[account(
        seeds = [b"pool_authority", pool.key().as_ref()],
        bump,
    )]
    pub pool_authority: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

// ── Handler ─────────────────────────────────────────────────────────────────

pub fn handler(ctx: Context<ArbSweep>) -> Result<()> {
    let arb_config = &mut ctx.accounts.arb_config;
    let pool_loader = &ctx.accounts.pool;
    let oracle = ctx.accounts.oracle.load()?;

    let clock = Clock::get()?;
    let current_slot = clock.slot;
    let current_timestamp = clock.unix_timestamp as u32;

    // Cooldown check
    require!(
        current_slot >= arb_config.last_sweep_slot + arb_config.cooldown_slots,
        ErrorCode::ArbCooldownActive
    );

    // Minimum observations check
    const MIN_OBSERVATIONS: u16 = 3;
    require!(
        oracle.observation_index >= MIN_OBSERVATIONS,
        ErrorCode::InsufficientOracleData
    );

    // Read pool snapshot for math (immutable borrow first)
    let (spot_sqrt_price, pool_liquidity) = {
        let pool = pool_loader.load()?;
        (pool.sqrt_price_x64, pool.liquidity)
    };

    // Get TWAP tick
    let twap_tick = get_twap_tick(&oracle, arb_config.twap_window_seconds, current_timestamp)
        .ok_or(ErrorCode::InsufficientOracleData)?;

    let twap_sqrt_price = tick_math::get_sqrt_price_at_tick(twap_tick)?;

    // Calculate spread
    let (spread_bps, a_to_b) = calculate_spread(spot_sqrt_price, twap_sqrt_price);

    require!(
        spread_bps >= arb_config.min_spread_bps as u128,
        ErrorCode::SpreadTooSmall
    );

    // Calculate sweep amount
    let sweep_amount = calculate_sweep_amount(
        spot_sqrt_price,
        twap_sqrt_price,
        pool_liquidity,
        arb_config.min_sweep_amount,
        arb_config.max_sweep_amount,
        a_to_b,
    )?;

    // Execute internal swap approximation
    let (amount_in, amount_out) = compute_arb_swap(
        spot_sqrt_price,
        pool_liquidity,
        sweep_amount,
        a_to_b,
    )?;

    let profit = amount_out.saturating_sub(amount_in);
    require!(profit > 0, ErrorCode::NoArbProfit);

    // Split profit: treasury share
    let treasury_amount = (profit as u128)
        .checked_mul(arb_config.treasury_share_bps as u128)
        .ok_or(ErrorCode::ArithmeticOverflow)?
        .checked_div(FEE_DENOMINATOR as u128)
        .ok_or(ErrorCode::DivisionByZero)? as u64;

    // Transfer treasury share
    if treasury_amount > 0 {
        let pool_key = pool_loader.key();
        let seeds = &[
            b"pool_authority",
            pool_key.as_ref(),
            &[ctx.bumps.pool_authority],
        ];
        let signer_seeds = &[&seeds[..]];

        if a_to_b {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_1.to_account_info(),
                        to: ctx.accounts.treasury_vault.to_account_info(),
                        authority: ctx.accounts.pool_authority.to_account_info(),
                    },
                    signer_seeds,
                ),
                treasury_amount,
            )?;
            arb_config.total_profit_captured_b = arb_config
                .total_profit_captured_b
                .saturating_add(treasury_amount);
        } else {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_0.to_account_info(),
                        to: ctx.accounts.treasury_vault.to_account_info(),
                        authority: ctx.accounts.pool_authority.to_account_info(),
                    },
                    signer_seeds,
                ),
                treasury_amount,
            )?;
            arb_config.total_profit_captured_a = arb_config
                .total_profit_captured_a
                .saturating_add(treasury_amount);
        }
    }

    // Apply price correction to pool (mutable borrow here)
    {
        let mut pool = pool_loader.load_mut()?;
        apply_arb_to_pool(&mut pool, sweep_amount, a_to_b)?;
    }

    // Update arb config
    arb_config.last_sweep_slot = current_slot;

    emit!(ArbSweepEvent {
        pool: pool_loader.key(),
        spread_bps: spread_bps as u16,
        sweep_amount,
        amount_in,
        amount_out,
        treasury_amount,
        a_to_b,
        slot: current_slot,
    });

    Ok(())
}

// ── Initialize Arb Config ────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitArbConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub pool: AccountLoader<'info, PoolState>,

    #[account(
        init,
        payer = admin,
        space = ArbConfig::LEN,
        seeds = [b"arb_config", pool.key().as_ref()],
        bump,
    )]
    pub arb_config: Account<'info, ArbConfig>,

    pub system_program: Program<'info, System>,
}

pub fn handler_init(
    ctx: Context<InitArbConfig>,
    arb_authority: Pubkey,
    treasury: Pubkey,
    min_spread_bps: u16,
    treasury_share_bps: u16,
    twap_window_seconds: u32,
    min_sweep_amount: u64,
    max_sweep_amount: u64,
    cooldown_slots: u64,
) -> Result<()> {
    let arb_config = &mut ctx.accounts.arb_config;
    let pool_key = ctx.accounts.pool.key();

    require!(treasury_share_bps <= 10_000, ErrorCode::ArithmeticOverflow);
    require!(min_spread_bps > 0, ErrorCode::ZeroLiquidity);
    require!(min_sweep_amount < max_sweep_amount, ErrorCode::AmountBelowMin);
    require!(twap_window_seconds >= 15, ErrorCode::InsufficientOracleData);

    arb_config.pool = pool_key;
    arb_config.arb_authority = arb_authority;
    arb_config.treasury = treasury;
    arb_config.min_spread_bps = min_spread_bps;
    arb_config.treasury_share_bps = treasury_share_bps;
    arb_config.twap_window_seconds = twap_window_seconds;
    arb_config.min_sweep_amount = min_sweep_amount;
    arb_config.max_sweep_amount = max_sweep_amount;
    arb_config.last_sweep_slot = 0;
    arb_config.cooldown_slots = cooldown_slots;
    arb_config.total_profit_captured_a = 0;
    arb_config.total_profit_captured_b = 0;
    arb_config.enabled = true;
    arb_config.bump = ctx.bumps.arb_config;

    Ok(())
}

// ── Toggle ───────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct ToggleArb<'info> {
    pub arb_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"arb_config", pool.key().as_ref()],
        bump = arb_config.bump,
        constraint = arb_config.arb_authority == arb_authority.key() @ ErrorCode::NotSender,
    )]
    pub arb_config: Account<'info, ArbConfig>,

    pub pool: AccountLoader<'info, PoolState>,
}

pub fn handler_toggle(ctx: Context<ToggleArb>, enabled: bool) -> Result<()> {
    ctx.accounts.arb_config.enabled = enabled;
    Ok(())
}

// ── Math Helpers ─────────────────────────────────────────────────────────────

fn get_twap_tick(oracle: &ObservationState, window_seconds: u32, current_timestamp: u32) -> Option<i32> {
    if oracle.observation_index == 0 {
        return None;
    }
    let idx = oracle.observation_index as usize;
    let latest = &oracle.observations[(idx.saturating_sub(1)) % crate::states::oracle::OBSERVATION_NUM];
    if latest.block_timestamp == 0 {
        return None;
    }
    let elapsed = current_timestamp.saturating_sub(latest.block_timestamp);
    if elapsed == 0 {
        return None;
    }
    let avg_tick = (latest.tick_cumulative / elapsed.max(1) as i64) as i32;
    Some(avg_tick.clamp(-443636, 443636))
}

fn calculate_spread(spot: u128, twap: u128) -> (u128, bool) {
    if spot > twap {
        let spread = (spot - twap)
            .saturating_mul(10_000)
            .checked_div(twap)
            .unwrap_or(0);
        (spread, true)
    } else {
        let spread = (twap - spot)
            .saturating_mul(10_000)
            .checked_div(twap)
            .unwrap_or(0);
        (spread, false)
    }
}

fn calculate_sweep_amount(
    spot: u128,
    twap: u128,
    liquidity: u128,
    min_amount: u64,
    max_amount: u64,
    a_to_b: bool,
) -> Result<u64> {
    let target_sqrt = if a_to_b {
        spot.saturating_sub((spot - twap) * 4 / 5)
    } else {
        spot.saturating_add((twap - spot) * 4 / 5)
    };

    let price_delta = if a_to_b {
        spot.saturating_sub(target_sqrt)
    } else {
        target_sqrt.saturating_sub(spot)
    };

    let raw_amount = (liquidity)
        .saturating_mul(price_delta)
        .checked_div(Q64)
        .unwrap_or(0) as u64;

    let clamped = raw_amount.max(min_amount).min(max_amount);
    Ok(clamped)
}

fn compute_arb_swap(
    sqrt_price: u128,
    liquidity: u128,
    amount: u64,
    a_to_b: bool,
) -> Result<(u64, u64)> {
    if a_to_b {
        let x_virtual = liquidity.checked_div((sqrt_price >> 32).max(1)).unwrap_or(1);
        let numerator = liquidity * amount as u128;
        let denominator = x_virtual + amount as u128;
        let amount_out = (numerator / denominator) as u64;
        Ok((amount, amount_out))
    } else {
        let y_virtual = liquidity.saturating_mul(sqrt_price >> 64);
        let numerator = liquidity * amount as u128;
        let denominator = y_virtual + amount as u128;
        let amount_out = (numerator / denominator) as u64;
        Ok((amount, amount_out))
    }
}

fn apply_arb_to_pool(pool: &mut PoolState, amount: u64, a_to_b: bool) -> Result<()> {
    let price_impact = (amount as u128)
        .saturating_mul(Q64)
        .checked_div(pool.liquidity.max(1))
        .unwrap_or(0);

    if a_to_b {
        pool.sqrt_price_x64 = pool.sqrt_price_x64.saturating_sub(price_impact);
    } else {
        pool.sqrt_price_x64 = pool.sqrt_price_x64.saturating_add(price_impact);
    }

    pool.tick_current = tick_math::get_tick_at_sqrt_price(pool.sqrt_price_x64)?;
    Ok(())
}

// ── Event ─────────────────────────────────────────────────────────────────────

#[event]
pub struct ArbSweepEvent {
    pub pool: Pubkey,
    pub spread_bps: u16,
    pub sweep_amount: u64,
    pub amount_in: u64,
    pub amount_out: u64,
    pub treasury_amount: u64,
    pub a_to_b: bool,
    pub slot: u64,
}
