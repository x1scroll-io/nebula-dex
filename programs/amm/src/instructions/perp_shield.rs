/// Nebula DEX — Perp Shield Instructions
///
/// Four instructions:
///   1. initialize_shield    — creates the PerpShield PDA for a market
///   2. perp_shield_liquidation_guard — checks cascade metrics before allowing liquidation
///   3. trigger_circuit_breaker — manually arm the circuit breaker (authority)
///   4. reset_circuit_breaker  — reset the circuit breaker (authority or auto after cooldown)

use anchor_lang::prelude::*;

use crate::states::perp_market::PerpMarket;
use crate::states::perp_shield::{
    CascadeAlert, PerpShield, ShieldAction,
    CASCADE_ALERT_SEED, PERP_SHIELD_SEED,
    CircuitBreakerResetEvent, CircuitBreakerTriggeredEvent,
    LiquidationGuardEvent, PerpShieldInitializedEvent,
};
use crate::error::ErrorCode;

// ── 1. Initialize Shield ──────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeShield<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    /// The perp market this shield guards.
    /// Validates that a valid PerpMarket PDA exists at this address.
    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        init,
        payer = authority,
        space = PerpShield::LEN,
        seeds = [PERP_SHIELD_SEED, perp_market.key().as_ref()],
        bump,
    )]
    pub perp_shield: AccountLoader<'info, PerpShield>,

    pub system_program: Program<'info, System>,
}

pub fn initialize_shield(
    ctx: Context<InitializeShield>,
    epoch_slots: u64,
    oi_imbalance_threshold_bps: u16,
    liq_rate_threshold: u16,
    price_velocity_threshold_bps: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let shield_key = ctx.accounts.perp_shield.key();
    let market_key = ctx.accounts.perp_market.key();

    let mut shield = ctx.accounts.perp_shield.load_init()?;

    shield.market = market_key;
    shield.authority = ctx.accounts.authority.key();
    shield.circuit_breaker_active = 0;
    shield.triggered_at_slot = 0;
    shield.reset_at_slot = 0;
    shield.trigger_count = 0;
    shield.last_action = ShieldAction::None as u8;
    shield.last_oi_imbalance_bps = 0;
    shield.liquidations_this_epoch = 0;
    shield.epoch_start_slot = clock.slot;
    shield.epoch_slots = if epoch_slots > 0 { epoch_slots } else { 1_800 };
    shield.last_mark_price_x64 = 0;
    shield.last_price_slot = 0;
    shield.price_velocity_bps = 0;
    shield.oi_imbalance_threshold_bps = oi_imbalance_threshold_bps;
    shield.liq_rate_threshold = liq_rate_threshold;
    shield.price_velocity_threshold_bps = price_velocity_threshold_bps;
    shield.initialized = 1;
    shield.bump = ctx.bumps.perp_shield;
    shield.padding = [0u8; 4];

    emit!(PerpShieldInitializedEvent {
        market: market_key,
        shield: shield_key,
        authority: ctx.accounts.authority.key(),
        slot: clock.slot,
    });

    let epoch_slots_val = shield.epoch_slots;
    msg!(
        "PerpShield initialized: market={} shield={} epoch_slots={}",
        market_key,
        shield_key,
        epoch_slots_val,
    );

    Ok(())
}

// ── 2. Perp Shield Liquidation Guard ─────────────────────────────────────────

/// Called by the liquidation authority before executing a liquidation.
///
/// Checks:
///   - Shield is initialized
///   - Circuit breaker state
///   - Auto-reset if cooldown elapsed
///   - Update cascade metrics from current market state
///   - Trip breaker if thresholds exceeded
///   - Allow or block the liquidation accordingly
///
/// If the circuit breaker is active, returns CircuitBreakerActive error
/// UNLESS `force_liquidate` is true (emergency override for deeply insolvent positions).
#[derive(Accounts)]
pub struct PerpShieldLiquidationGuard<'info> {
    /// Liquidation authority (must be the price_authority of the market, or the shield authority)
    pub liquidation_authority: Signer<'info>,

    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        mut,
        seeds = [PERP_SHIELD_SEED, perp_market.key().as_ref()],
        bump = perp_shield.load()?.bump,
    )]
    pub perp_shield: AccountLoader<'info, PerpShield>,
}

pub fn perp_shield_liquidation_guard(
    ctx: Context<PerpShieldLiquidationGuard>,
    force_liquidate: bool,
) -> Result<ShieldAction> {
    let clock = Clock::get()?;
    let current_slot = clock.slot;

    let market_key;
    let mark_price_x64;
    let oi_imbalance_bps;
    let price_authority;

    {
        let market = ctx.accounts.perp_market.load()?;
        require!(market.is_active() || market.is_emergency(), ErrorCode::MarketPaused);
        market_key = ctx.accounts.perp_market.key();
        mark_price_x64 = market.mark_price_x64;
        oi_imbalance_bps = market.oi_imbalance_bps();
        price_authority = market.price_authority;
    }

    let mut shield = ctx.accounts.perp_shield.load_mut()?;

    require!(shield.initialized == 1, ErrorCode::ShieldNotInitialized);

    // Validate caller is shield authority or market price authority
    let caller = ctx.accounts.liquidation_authority.key();
    require!(
        caller == shield.authority || caller == price_authority,
        ErrorCode::NotApproved
    );

    // ── Auto-reset check ───────────────────────────────────────────────────
    if shield.is_breaker_active() && shield.cooldown_elapsed(current_slot) {
        // Metrics must also normalize before auto-reset
        let still_elevated = oi_imbalance_bps >= shield.oi_threshold()
            || shield.liquidations_this_epoch >= shield.liq_threshold()
            || shield.price_velocity_bps >= shield.velocity_threshold();

        if !still_elevated {
            shield.circuit_breaker_active = 0;
            shield.reset_at_slot = current_slot;
            shield.last_action = ShieldAction::AutoReset as u8;

            emit!(CircuitBreakerResetEvent {
                market: market_key,
                shield: ctx.accounts.perp_shield.key(),
                trigger_count: shield.trigger_count,
                by_authority: false,
                slot: current_slot,
            });
        }
    }

    // ── Update metrics ─────────────────────────────────────────────────────
    shield.last_oi_imbalance_bps = oi_imbalance_bps;
    shield.update_price_velocity(mark_price_x64, current_slot);
    shield.record_liquidation(current_slot);

    // ── Trip check ─────────────────────────────────────────────────────────
    if !shield.is_breaker_active() && shield.should_trip(oi_imbalance_bps) {
        shield.circuit_breaker_active = 1;
        shield.triggered_at_slot = current_slot;
        shield.trigger_count = shield.trigger_count.saturating_add(1);

        let mut flags: u8 = 0;
        if oi_imbalance_bps >= shield.oi_threshold() {
            flags |= CascadeAlert::FLAG_OI;
        }
        if shield.liquidations_this_epoch >= shield.liq_threshold() {
            flags |= CascadeAlert::FLAG_LIQ_RATE;
        }
        if shield.price_velocity_bps >= shield.velocity_threshold() {
            flags |= CascadeAlert::FLAG_VELOCITY;
        }

        shield.last_action = ShieldAction::CircuitBreakerArmed as u8;

        emit!(CircuitBreakerTriggeredEvent {
            market: market_key,
            shield: ctx.accounts.perp_shield.key(),
            trigger_count: shield.trigger_count,
            oi_imbalance_bps,
            liquidations_in_epoch: shield.liquidations_this_epoch,
            price_velocity_bps: shield.price_velocity_bps,
            trigger_flags: flags,
            slot: current_slot,
        });
    }

    // ── Decision ───────────────────────────────────────────────────────────
    let action = if shield.is_breaker_active() && !force_liquidate {
        shield.last_action = ShieldAction::LiquidationDeferred as u8;

        emit!(LiquidationGuardEvent {
            market: market_key,
            owner: caller,
            action: ShieldAction::LiquidationDeferred as u8,
            oi_imbalance_bps,
            liquidations_in_epoch: shield.liquidations_this_epoch,
            slot: current_slot,
        });

        return Err(ErrorCode::CircuitBreakerActive.into());
    } else {
        shield.last_action = ShieldAction::LiquidationAllowed as u8;

        emit!(LiquidationGuardEvent {
            market: market_key,
            owner: caller,
            action: ShieldAction::LiquidationAllowed as u8,
            oi_imbalance_bps,
            liquidations_in_epoch: shield.liquidations_this_epoch,
            slot: current_slot,
        });

        ShieldAction::LiquidationAllowed
    };

    Ok(action)
}

// ── 3. Trigger Circuit Breaker ────────────────────────────────────────────────

/// Manually arm the circuit breaker. Authority-only.
/// Used when off-chain monitors detect cascade risk before thresholds are hit on-chain.
///
/// `alert_index` must equal the current `perp_shield.trigger_count + 1`.
/// The client should read the shield account to get `trigger_count` before calling.
#[derive(Accounts)]
#[instruction(reason_flags: u8, alert_index: u64)]
pub struct TriggerCircuitBreaker<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        mut,
        seeds = [PERP_SHIELD_SEED, perp_market.key().as_ref()],
        bump = perp_shield.load()?.bump,
        has_one = authority @ ErrorCode::NotApproved,
    )]
    pub perp_shield: AccountLoader<'info, PerpShield>,

    /// New CascadeAlert record for this trigger event.
    /// Seeds: [CASCADE_ALERT_SEED, perp_shield.key(), alert_index.to_le_bytes()]
    #[account(
        init,
        payer = authority,
        space = CascadeAlert::LEN,
        seeds = [
            CASCADE_ALERT_SEED,
            perp_shield.key().as_ref(),
            &alert_index.to_le_bytes(),
        ],
        bump,
    )]
    pub cascade_alert: AccountLoader<'info, CascadeAlert>,

    pub system_program: Program<'info, System>,
}

pub fn trigger_circuit_breaker(
    ctx: Context<TriggerCircuitBreaker>,
    reason_flags: u8,
    alert_index: u64,
) -> Result<()> {
    let clock = Clock::get()?;
    let current_slot = clock.slot;

    let market_key;
    let oi_imbalance_bps;

    {
        let market = ctx.accounts.perp_market.load()?;
        market_key = ctx.accounts.perp_market.key();
        oi_imbalance_bps = market.oi_imbalance_bps();
    }

    let shield_key = ctx.accounts.perp_shield.key();

    let trigger_index;
    let liquidations_in_epoch;
    let price_velocity_bps;

    {
        let mut shield = ctx.accounts.perp_shield.load_mut()?;
        require!(shield.initialized == 1, ErrorCode::ShieldNotInitialized);

        shield.circuit_breaker_active = 1;
        shield.triggered_at_slot = current_slot;
        shield.trigger_count = shield.trigger_count.saturating_add(1);
        shield.last_action = ShieldAction::CircuitBreakerArmed as u8;

        trigger_index = shield.trigger_count;
        liquidations_in_epoch = shield.liquidations_this_epoch;
        price_velocity_bps = shield.price_velocity_bps;
    }

    // Validate that the caller passed the correct alert_index
    require!(alert_index == trigger_index, ErrorCode::NotApproved);

    // Initialize the CascadeAlert record
    let alert_bump = ctx.bumps.cascade_alert;
    let mut alert = ctx.accounts.cascade_alert.load_init()?;
    alert.shield = shield_key;
    alert.market = market_key;
    alert.alert_index = trigger_index;
    alert.triggered_at_slot = current_slot;
    alert.reset_at_slot = 0;
    alert.oi_imbalance_bps = oi_imbalance_bps;
    alert.liquidations_in_epoch = liquidations_in_epoch;
    alert.price_velocity_bps = price_velocity_bps;
    alert.trigger_flags = reason_flags;
    alert.resolution = 0;
    alert.bump = alert_bump;
    alert.padding = [0u8; 5];

    emit!(CircuitBreakerTriggeredEvent {
        market: market_key,
        shield: shield_key,
        trigger_count: trigger_index,
        oi_imbalance_bps,
        liquidations_in_epoch,
        price_velocity_bps,
        trigger_flags: reason_flags,
        slot: current_slot,
    });

    msg!(
        "PerpShield: circuit breaker TRIGGERED manually. market={} trigger={}",
        market_key,
        trigger_index,
    );

    Ok(())
}

// ── 4. Reset Circuit Breaker ─────────────────────────────────────────────────

/// Reset (disarm) the circuit breaker. Authority-only.
/// Cooldown must have elapsed unless `force_reset` is true (emergency).
#[derive(Accounts)]
pub struct ResetCircuitBreaker<'info> {
    pub authority: Signer<'info>,

    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        mut,
        seeds = [PERP_SHIELD_SEED, perp_market.key().as_ref()],
        bump = perp_shield.load()?.bump,
        has_one = authority @ ErrorCode::NotApproved,
    )]
    pub perp_shield: AccountLoader<'info, PerpShield>,
}

pub fn reset_circuit_breaker(
    ctx: Context<ResetCircuitBreaker>,
    force_reset: bool,
) -> Result<()> {
    let clock = Clock::get()?;
    let current_slot = clock.slot;

    let mut shield = ctx.accounts.perp_shield.load_mut()?;
    require!(shield.initialized == 1, ErrorCode::ShieldNotInitialized);

    // Breaker must be active to reset
    if !shield.is_breaker_active() {
        msg!("PerpShield: circuit breaker already inactive, nothing to reset.");
        return Ok(());
    }

    // Unless forced, require cooldown to have elapsed
    if !force_reset {
        require!(
            shield.cooldown_elapsed(current_slot),
            ErrorCode::CircuitBreakerActive // reuse: "still in cooldown"
        );
    }

    shield.circuit_breaker_active = 0;
    shield.reset_at_slot = current_slot;
    shield.last_action = ShieldAction::Reset as u8;
    // Reset epoch counter so liquidation rate doesn't immediately re-trip
    shield.reset_epoch(current_slot);

    let market_key = shield.market;
    let trigger_count = shield.trigger_count;

    emit!(CircuitBreakerResetEvent {
        market: market_key,
        shield: ctx.accounts.perp_shield.key(),
        trigger_count,
        by_authority: true,
        slot: current_slot,
    });

    msg!(
        "PerpShield: circuit breaker RESET by authority. market={} force={}",
        market_key,
        force_reset,
    );

    Ok(())
}
