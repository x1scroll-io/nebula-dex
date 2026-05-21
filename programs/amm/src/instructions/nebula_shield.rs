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

/// Detect same-slot manipulation. Returns the tax bps (>0) to apply when
/// manipulation is detected, or 0 when not. Honeypot behavior: never blocks
/// the swap — the protocol lets it execute but taxes it via the returned bps.
/// When manipulation is detected, the detections counter on the supplied
/// arb_config is incremented.
pub fn detect_same_slot_manipulation(
    pool: &PoolState,
    oracle: &ObservationState,
    arb_config: &mut ArbConfig,
    current_timestamp: u32,
    manipulation_threshold_bps: u16,
) -> u16 {
    let recent_twap = match observe_twap(oracle, 5, current_timestamp) {
        Some(t) => t,
        None => return 0,
    };

    let twap_sqrt = match tick_math::get_sqrt_price_at_tick(recent_twap) {
        Ok(p) => p,
        Err(_) => return 0,
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

    if deviation_bps > manipulation_threshold_bps as u128 {
        arb_config.manipulation_detections =
            arb_config.manipulation_detections.saturating_add(1);
        arb_config.manipulation_tax_bps
    } else {
        0
    }
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

/// Proper tick-based TWAP from ObservationState.
/// Returns average tick over the window using cumulative tick deltas.
const MIN_OBSERVATIONS_FOR_TWAP: u16 = 2;

fn observe_twap(oracle: &ObservationState, window_seconds: u32, current_timestamp: u32) -> Option<i32> {
    if oracle.observation_index == 0 {
        return None;
    }

    // Require at least 2 observations for a valid TWAP
    if oracle.observation_index < MIN_OBSERVATIONS_FOR_TWAP {
        return None;
    }

    let idx = oracle.observation_index as usize;
    let latest = &oracle.observations[idx.saturating_sub(1) % crate::states::oracle::OBSERVATION_NUM];
    if latest.block_timestamp == 0 {
        return None;
    }

    let elapsed = current_timestamp.saturating_sub(latest.block_timestamp);

    // When elapsed == 0 or data is stale, return None — insufficient data for TWAP
    if elapsed == 0 || elapsed > window_seconds * 2 {
        return None;
    }

    // Proper tick-based TWAP: find observation from window_seconds ago
    let target_timestamp = current_timestamp.saturating_sub(window_seconds);
    let older_idx = find_observation_at_or_before(oracle, target_timestamp)?;
    let older = &oracle.observations[older_idx];

    let time_delta = latest.block_timestamp.saturating_sub(older.block_timestamp) as i64;
    if time_delta == 0 {
        return None;
    }

    // Tick TWAP = (tick_cumulative_latest - tick_cumulative_older) / time_delta
    let tick_cumulative_delta = latest.tick_cumulative.wrapping_sub(older.tick_cumulative);
    let avg_tick = (tick_cumulative_delta / time_delta) as i32;

    Some(avg_tick.clamp(-443636, 443636))
}

fn find_observation_at_or_before(oracle: &ObservationState, target_timestamp: u32) -> Option<usize> {
    let current_idx = oracle.observation_index as usize;
    let obs_num = crate::states::oracle::OBSERVATION_NUM;

    for i in 0..obs_num {
        let idx = (current_idx + obs_num - i) % obs_num;
        if oracle.observations[idx].block_timestamp != 0
            && oracle.observations[idx].block_timestamp <= target_timestamp
        {
            return Some(idx);
        }
    }
    None
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

// ── Swap-Path Integration Wrapper ────────────────────────────────────────────
//
// Best-effort hook called from swap_v2.rs before swap math runs. If an
// ArbConfig PDA for this pool is supplied in remaining_accounts, Layer 1
// (pre-swap arb sweep) executes. Otherwise the hook is a no-op. All errors
// are swallowed and logged so the shield never blocks a swap in V1.

use core::cell::RefMut;

pub fn try_pre_swap_shield<'info>(
    pool_state: &mut RefMut<PoolState>,
    observation: &ObservationState,
    arb_config_ai: Option<&'info AccountInfo<'info>>,
    pool_key: Pubkey,
    block_timestamp: u32,
) {
    // Layer 1: pre-swap arb sweep — only if ArbConfig PDA is supplied
    let arb_ai = match arb_config_ai {
        Some(ai) => ai,
        None => return,
    };

    let (expected_pda, _) = Pubkey::find_program_address(
        &[b"arb_config", pool_key.as_ref()],
        &crate::ID,
    );
    if arb_ai.key() != expected_pda {
        msg!("Nebula Shield: arb_config PDA mismatch, skipping sweep");
        return;
    }
    if arb_ai.owner != &crate::ID {
        msg!("Nebula Shield: arb_config owner mismatch, skipping sweep");
        return;
    }

    let mut arb_config: Account<ArbConfig> = match Account::try_from(arb_ai) {
        Ok(a) => a,
        Err(_) => {
            msg!("Nebula Shield: arb_config deserialize failed, skipping sweep");
            return;
        }
    };

    let clock = match Clock::get() {
        Ok(c) => c,
        Err(_) => return,
    };

    let result = pre_swap_arb_check(
        &mut **pool_state,
        &mut arb_config,
        observation,
        clock.slot,
        block_timestamp,
    );

    match result {
        Ok((swept, a_to_b, profit)) => {
            if swept {
                msg!(
                    "Nebula Shield: pre-swap sweep executed a_to_b={} profit={}",
                    a_to_b,
                    profit
                );
            }
        }
        Err(e) => {
            msg!("Nebula Shield: pre-swap sweep error code={:?}", e);
            return;
        }
    }

    if let Err(e) = arb_config.exit(&crate::ID) {
        msg!("Nebula Shield: arb_config exit error code={:?}", e);
    }
}

// ── Honeypot: Manipulation Tax Application ───────────────────────────────────
//
// Best-effort hook called from swap_v2.rs after try_pre_swap_shield and before
// swap_internal. If an ArbConfig PDA is supplied and same-slot manipulation is
// detected, a tax is charged on the swap input: routed into the pool's
// protocol_fees on the input-token side (same path collect_protocol_fee drains
// to the TiPy treasury). Returns the tax amount that must be removed from the
// amount handed to swap_internal. Never blocks the swap.

pub fn try_apply_manipulation_tax<'info>(
    pool_state: &mut RefMut<PoolState>,
    observation: &ObservationState,
    arb_config_ai: Option<&'info AccountInfo<'info>>,
    pool_key: Pubkey,
    block_timestamp: u32,
    swap_amount: u64,
    zero_for_one: bool,
) -> u64 {
    let arb_ai = match arb_config_ai {
        Some(ai) => ai,
        None => return 0,
    };

    let (expected_pda, _) =
        Pubkey::find_program_address(&[b"arb_config", pool_key.as_ref()], &crate::ID);
    if arb_ai.key() != expected_pda || arb_ai.owner != &crate::ID {
        return 0;
    }

    let mut arb_config: Account<ArbConfig> = match Account::try_from(arb_ai) {
        Ok(a) => a,
        Err(_) => return 0,
    };

    // Manipulation deviation threshold — read from ArbConfig (admin-configurable).
    // Falls back to 500 bps (5%) for older PDAs that predate the field.
    let threshold_bps = arb_config.effective_manipulation_threshold_bps();
    let tax_bps = detect_same_slot_manipulation(
        &*pool_state,
        observation,
        &mut arb_config,
        block_timestamp,
        threshold_bps,
    );

    if tax_bps == 0 || tax_bps > 10_000 {
        let _ = arb_config.exit(&crate::ID);
        return 0;
    }

    let tax_amount = ((swap_amount as u128) * (tax_bps as u128) / 10_000) as u64;
    // Refuse to tax if it would consume the entire input or be zero — keeps
    // swap_internal viable so the swap can still settle and the tax sticks.
    if tax_amount == 0 || tax_amount >= swap_amount {
        let _ = arb_config.exit(&crate::ID);
        return 0;
    }

    if zero_for_one {
        pool_state.protocol_fees_token_0 = pool_state
            .protocol_fees_token_0
            .saturating_add(tax_amount);
        arb_config.manipulation_tax_collected_a = arb_config
            .manipulation_tax_collected_a
            .saturating_add(tax_amount);
    } else {
        pool_state.protocol_fees_token_1 = pool_state
            .protocol_fees_token_1
            .saturating_add(tax_amount);
        arb_config.manipulation_tax_collected_b = arb_config
            .manipulation_tax_collected_b
            .saturating_add(tax_amount);
    }

    let slot = Clock::get().map(|c| c.slot).unwrap_or(0);
    emit!(ManipulationTaxEvent {
        pool: arb_config.pool,
        swap_amount,
        tax_amount,
        tax_bps,
        slot,
        a_to_b: zero_for_one,
    });

    if let Err(e) = arb_config.exit(&crate::ID) {
        msg!("Nebula Shield: arb_config exit error code={:?}", e);
    }

    tax_amount
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

#[event]
pub struct ManipulationTaxEvent {
    pub pool: Pubkey,
    pub swap_amount: u64,
    pub tax_amount: u64,
    pub tax_bps: u16,
    pub slot: u64,
    pub a_to_b: bool,
}
