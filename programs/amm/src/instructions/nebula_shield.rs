// Nebula DEX — Nebula Shield
//
// Adapted from Theo (@xxen_bot) contribution for nebula-dex-fork module structure.
//
// Anti-sandwich, anti-MEV, anti-Jared.
// Three-layer defense baked into every swap:
//  Layer 1: Pre-swap arb sweep (protocol captures spread, price corrected)
//  Layer 2: Same-slot manipulation detection
//  Layer 3: Post-swap JIT window lock

use anchor_lang::prelude::*;
use crate::states::{PoolState, ArbConfig};
use crate::states::oracle::ObservationState;
use crate::libraries::tick_math;
use crate::error::ErrorCode;
use crate::libraries::fixed_point_64::Q64;

// ── Layer 1: Pre-Swap Arb Sweep ──────────────────────────────────────────────

/// Run pre-swap arb check. Returns (swept, a_to_b, profit_amount).
/// Called inline from swap before swap math runs.
pub fn pre_swap_arb_check(
    pool: &mut PoolState,
    arb_config: &mut ArbConfig,
    oracle: &ObservationState,
    current_slot: u64,
    current_timestamp: u32,
) -> Result<(bool, bool, u64)> {
    if !arb_config.enabled {
        return Ok((false, false, 0));
    }
    if current_slot < arb_config.last_sweep_slot + arb_config.cooldown_slots {
        return Ok((false, false, 0));
    }

    // Require at least 3 initialized observations
    const MIN_OBSERVATIONS: u16 = 3;
    if oracle.observation_index < MIN_OBSERVATIONS {
        msg!("Nebula Shield: insufficient observations, skipping sweep");
        return Ok((false, false, 0));
    }

    // Get TWAP reference tick
    let twap_tick = match observe_twap(oracle, arb_config.twap_window_seconds, current_timestamp) {
        Some(t) => t,
        None => return Ok((false, false, 0)),
    };

    let twap_sqrt = match tick_math::get_sqrt_price_at_tick(twap_tick) {
        Ok(p) => p,
        Err(_) => return Ok((false, false, 0)),
    };

    let spot = pool.sqrt_price_x64;

    let (spread_bps, a_to_b) = if spot > twap_sqrt {
        let spread = (spot - twap_sqrt)
            .saturating_mul(10_000)
            .checked_div(twap_sqrt)
            .unwrap_or(0);
        (spread, true)
    } else {
        let spread = (twap_sqrt - spot)
            .saturating_mul(10_000)
            .checked_div(twap_sqrt)
            .unwrap_or(0);
        (spread, false)
    };

    if spread_bps < arb_config.min_spread_bps as u128 {
        return Ok((false, false, 0));
    }

    // Close 80% of the spread gap
    let target_sqrt = if a_to_b {
        spot.saturating_sub((spot - twap_sqrt) * 4 / 5)
    } else {
        spot.saturating_add((twap_sqrt - spot) * 4 / 5)
    };

    let price_delta = if a_to_b {
        spot.saturating_sub(target_sqrt)
    } else {
        target_sqrt.saturating_sub(spot)
    };

    let raw_amount = (pool.liquidity as u128)
        .saturating_mul(price_delta)
        .checked_div(Q64)
        .unwrap_or(0) as u64;

    let sweep_amount = raw_amount
        .max(arb_config.min_sweep_amount)
        .min(arb_config.max_sweep_amount);

    if sweep_amount == 0 {
        return Ok((false, false, 0));
    }

    let (amount_in, amount_out) = compute_inline_arb(pool.sqrt_price_x64, pool.liquidity, sweep_amount, a_to_b);
    let profit = amount_out.saturating_sub(amount_in);

    if profit == 0 {
        return Ok((false, false, 0));
    }

    // Apply price correction
    let price_impact = (sweep_amount as u128)
        .saturating_mul(Q64)
        .checked_div(pool.liquidity.max(1))
        .unwrap_or(0);

    if a_to_b {
        pool.sqrt_price_x64 = pool.sqrt_price_x64.saturating_sub(price_impact);
        arb_config.total_profit_captured_b = arb_config
            .total_profit_captured_b
            .saturating_add(profit);
    } else {
        pool.sqrt_price_x64 = pool.sqrt_price_x64.saturating_add(price_impact);
        arb_config.total_profit_captured_a = arb_config
            .total_profit_captured_a
            .saturating_add(profit);
    }

    if let Ok(new_tick) = tick_math::get_tick_at_sqrt_price(pool.sqrt_price_x64) {
        pool.tick_current = new_tick;
    }

    arb_config.last_sweep_slot = current_slot;

    let treasury_share = (profit as u128)
        .saturating_mul(arb_config.treasury_share_bps as u128)
        / 10_000;

    if a_to_b {
        pool.protocol_fees_token_1 = pool.protocol_fees_token_1.saturating_add(treasury_share as u64);
    } else {
        pool.protocol_fees_token_0 = pool.protocol_fees_token_0.saturating_add(treasury_share as u64);
    }

    emit!(NebulaShieldSweep {
        pool: arb_config.pool,
        spread_bps: spread_bps as u16,
        sweep_amount,
        profit,
        a_to_b,
        slot: current_slot,
    });

    Ok((true, a_to_b, profit))
}

// ── Layer 2: Same-Slot Manipulation Detection ─────────────────────────────────

pub fn detect_same_slot_manipulation(
    pool: &PoolState,
    oracle: &ObservationState,
    current_timestamp: u32,
    manipulation_threshold_bps: u16,
) -> bool {
    let recent_twap = match observe_twap(oracle, 5, current_timestamp) {
        Some(t) => t,
        None => return false,
    };

    let twap_sqrt = match tick_math::get_sqrt_price_at_tick(recent_twap) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let spot = pool.sqrt_price_x64;
    let deviation_bps = if spot > twap_sqrt {
        (spot - twap_sqrt)
            .saturating_mul(10_000)
            .checked_div(twap_sqrt)
            .unwrap_or(0)
    } else {
        (twap_sqrt - spot)
            .saturating_mul(10_000)
            .checked_div(twap_sqrt)
            .unwrap_or(0)
    };

    deviation_bps > manipulation_threshold_bps as u128
}

// ── Layer 3: Post-Swap Back-Run Lock ─────────────────────────────────────────

/// Record swap slot on pool for post-swap JIT detection
pub fn record_swap_slot(pool: &mut PoolState, current_slot: u64) {
    pool.open_time = current_slot; // reuse open_time as slot tracker for JIT guard
}

/// Check if a liquidity operation is a suspected back-run
pub fn is_suspected_backrun(pool: &PoolState, current_slot: u64) -> bool {
    pool.open_time == current_slot
}

// ── TWAP Helper ───────────────────────────────────────────────────────────────

/// Simple TWAP tick approximation from ObservationState.
/// Returns the tick_cumulative delta over the window as an average tick.
fn observe_twap(oracle: &ObservationState, window_seconds: u32, current_timestamp: u32) -> Option<i32> {
    if oracle.observation_index == 0 {
        return None;
    }
    let idx = oracle.observation_index as usize;
    let latest = &oracle.observations[idx.saturating_sub(1) % crate::states::oracle::OBSERVATION_NUM];
    if latest.block_timestamp == 0 {
        return None;
    }
    let elapsed = current_timestamp.saturating_sub(latest.block_timestamp);
    if elapsed == 0 || elapsed > window_seconds * 2 {
        // Use current observation tick directly if very fresh
        let avg_tick = if elapsed == 0 {
            (latest.tick_cumulative) as i32
        } else {
            (latest.tick_cumulative / elapsed as i64) as i32
        };
        return Some(avg_tick.clamp(-443636, 443636));
    }
    let avg_tick = (latest.tick_cumulative / elapsed.max(1) as i64) as i32;
    Some(avg_tick.clamp(-443636, 443636))
}

// ── Inline Arb Math ───────────────────────────────────────────────────────────

fn compute_inline_arb(sqrt_price: u128, liquidity: u128, amount: u64, a_to_b: bool) -> (u64, u64) {
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

// ── Event ─────────────────────────────────────────────────────────────────────

#[event]
pub struct NebulaShieldSweep {
    pub pool: Pubkey,
    pub spread_bps: u16,
    pub sweep_amount: u64,
    pub profit: u64,
    pub a_to_b: bool,
    pub slot: u64,
}
