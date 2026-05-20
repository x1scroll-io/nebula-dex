/// Nebula DEX — Perp Shield State
///
/// PerpShield: per-market circuit-breaker + cascade liquidation guard.
///
/// Cascade risk: when too many leveraged positions liquidate simultaneously,
/// the resulting OI unwind creates a death spiral — each liquidation pushes
/// price further, triggering more liquidations. This module detects and
/// interrupts that feedback loop.
///
/// Circuit breaker model:
///   - Monitors: OI imbalance, liquidation rate, price velocity
///   - Trigger: any threshold breach arms the breaker
///   - Effect: pauses new position opens; liquidations still proceed
///   - Reset: authority or auto-reset after cooldown + metrics normalize

use anchor_lang::prelude::*;

// ── Seeds ─────────────────────────────────────────────────────────────────────

pub const PERP_SHIELD_SEED: &[u8] = b"perp_shield";
pub const CASCADE_ALERT_SEED: &[u8] = b"cascade_alert";

// ── Thresholds ────────────────────────────────────────────────────────────────

/// OI imbalance bps above which cascade risk is elevated
pub const CASCADE_OI_IMBALANCE_BPS: u16 = 7_000; // 70%

/// Liquidations-per-epoch that triggers the circuit breaker
pub const CASCADE_LIQ_RATE_THRESHOLD: u16 = 50;

/// Price velocity bps/slot above which breaker is armed
pub const CASCADE_PRICE_VELOCITY_BPS: u16 = 500; // 5% per slot (extreme)

/// Slots after trigger before auto-reset is attempted
pub const CIRCUIT_BREAKER_COOLDOWN_SLOTS: u64 = 1_800; // ~12 minutes at 400ms

// ── ShieldAction ──────────────────────────────────────────────────────────────

/// Action recorded when the shield responds to a cascade threat.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShieldAction {
    /// No action — position is within safe parameters
    None = 0,
    /// Shield flagged elevated risk but did not block
    Flagged = 1,
    /// Circuit breaker armed — position open blocked
    CircuitBreakerArmed = 2,
    /// Liquidation allowed to proceed under guard
    LiquidationAllowed = 3,
    /// Liquidation deferred — circuit breaker active, cooldown enforced
    LiquidationDeferred = 4,
    /// Circuit breaker manually reset by authority
    Reset = 5,
    /// Circuit breaker auto-reset after cooldown expired
    AutoReset = 6,
}

impl Default for ShieldAction {
    fn default() -> Self {
        ShieldAction::None
    }
}

// ── PerpShield ────────────────────────────────────────────────────────────────

/// One PDA per perp market. Tracks cascade risk and circuit-breaker state.
///
/// Seeds: [b"perp_shield", perp_market.as_ref()]
#[account(zero_copy(unsafe))]
#[repr(C, packed)]
#[derive(Default, Debug)]
pub struct PerpShield {
    /// PerpMarket this shield belongs to
    pub market: Pubkey,

    /// Authority allowed to manually trigger or reset the circuit breaker
    pub authority: Pubkey,

    // ── Circuit Breaker State ──────────────────────────────────────────────

    /// Whether the circuit breaker is currently active (1 = active, 0 = inactive)
    pub circuit_breaker_active: u8,

    /// Slot when the circuit breaker was last triggered
    pub triggered_at_slot: u64,

    /// Slot when the circuit breaker was last reset
    pub reset_at_slot: u64,

    /// Number of times the circuit breaker has been triggered (lifetime)
    pub trigger_count: u64,

    /// Last ShieldAction taken (u8 repr of ShieldAction enum)
    pub last_action: u8,

    // ── Cascade Detection Metrics ──────────────────────────────────────────

    /// OI imbalance bps at last check (cached from PerpMarket)
    pub last_oi_imbalance_bps: u16,

    /// Liquidations counted in current epoch
    pub liquidations_this_epoch: u16,

    /// Slot when the current liquidation epoch started
    pub epoch_start_slot: u64,

    /// Number of slots in a liquidation counting epoch
    pub epoch_slots: u64,

    /// Last mark price snapshot (Q64.64) for velocity calculation
    pub last_mark_price_x64: u128,

    /// Slot of last mark price snapshot
    pub last_price_slot: u64,

    /// Computed price velocity in bps/slot (cached)
    pub price_velocity_bps: u16,

    // ── Thresholds (per-market overrides) ─────────────────────────────────

    /// OI imbalance bps threshold (0 = use default CASCADE_OI_IMBALANCE_BPS)
    pub oi_imbalance_threshold_bps: u16,

    /// Liquidation rate threshold per epoch (0 = use default)
    pub liq_rate_threshold: u16,

    /// Price velocity threshold bps/slot (0 = use default)
    pub price_velocity_threshold_bps: u16,

    // ── Lifecycle ─────────────────────────────────────────────────────────

    /// Whether this shield has been initialized (1 = yes)
    pub initialized: u8,

    /// PDA bump
    pub bump: u8,

    /// Reserved for future use
    pub padding: [u8; 4],
}

impl PerpShield {
    /// Space: discriminator(8) + 2*Pubkey(64) + flags/counts(~80) + padding(4)
    pub const LEN: usize = 8 + 64 + 1 + 8 + 8 + 8 + 1 + 2 + 2 + 8 + 8 + 16 + 8 + 2 + 2 + 2 + 2 + 1 + 1 + 4;

    /// Is the circuit breaker currently active?
    pub fn is_breaker_active(&self) -> bool {
        self.circuit_breaker_active == 1
    }

    /// Has the cooldown elapsed since the circuit breaker was triggered?
    pub fn cooldown_elapsed(&self, current_slot: u64) -> bool {
        let cooldown = if self.epoch_slots > 0 {
            self.epoch_slots * 3 // 3 epochs as default cooldown
        } else {
            CIRCUIT_BREAKER_COOLDOWN_SLOTS
        };
        current_slot >= self.triggered_at_slot + cooldown
    }

    /// Effective OI imbalance threshold
    pub fn oi_threshold(&self) -> u16 {
        if self.oi_imbalance_threshold_bps == 0 {
            CASCADE_OI_IMBALANCE_BPS
        } else {
            self.oi_imbalance_threshold_bps
        }
    }

    /// Effective liquidation rate threshold
    pub fn liq_threshold(&self) -> u16 {
        if self.liq_rate_threshold == 0 {
            CASCADE_LIQ_RATE_THRESHOLD
        } else {
            self.liq_rate_threshold
        }
    }

    /// Effective price velocity threshold
    pub fn velocity_threshold(&self) -> u16 {
        if self.price_velocity_threshold_bps == 0 {
            CASCADE_PRICE_VELOCITY_BPS
        } else {
            self.price_velocity_threshold_bps
        }
    }

    /// Record that a new liquidation epoch has started
    pub fn reset_epoch(&mut self, current_slot: u64) {
        self.epoch_start_slot = current_slot;
        self.liquidations_this_epoch = 0;
    }

    /// Increment liquidation counter; auto-resets epoch if expired
    pub fn record_liquidation(&mut self, current_slot: u64) {
        let epoch_len = if self.epoch_slots > 0 { self.epoch_slots } else { 1_800 };
        if current_slot >= self.epoch_start_slot + epoch_len {
            self.reset_epoch(current_slot);
        }
        self.liquidations_this_epoch = self.liquidations_this_epoch.saturating_add(1);
    }

    /// Update price velocity estimate from a new mark price observation
    pub fn update_price_velocity(&mut self, new_price_x64: u128, current_slot: u64) {
        if self.last_price_slot == 0 || self.last_mark_price_x64 == 0 {
            self.last_mark_price_x64 = new_price_x64;
            self.last_price_slot = current_slot;
            self.price_velocity_bps = 0;
            return;
        }
        let slots_elapsed = current_slot.saturating_sub(self.last_price_slot);
        if slots_elapsed == 0 {
            return;
        }
        let old = self.last_mark_price_x64;
        let velocity_bps = if new_price_x64 > old {
            (new_price_x64 - old)
                .saturating_mul(10_000)
                .checked_div(old.max(1))
                .unwrap_or(0)
                .checked_div(slots_elapsed as u128)
                .unwrap_or(0)
        } else {
            (old - new_price_x64)
                .saturating_mul(10_000)
                .checked_div(old.max(1))
                .unwrap_or(0)
                .checked_div(slots_elapsed as u128)
                .unwrap_or(0)
        };
        self.price_velocity_bps = velocity_bps.min(u16::MAX as u128) as u16;
        self.last_mark_price_x64 = new_price_x64;
        self.last_price_slot = current_slot;
    }

    /// Evaluate all cascade signals and return true if circuit breaker should trip
    pub fn should_trip(
        &self,
        oi_imbalance_bps: u16,
    ) -> bool {
        if oi_imbalance_bps >= self.oi_threshold() {
            return true;
        }
        if self.liquidations_this_epoch >= self.liq_threshold() {
            return true;
        }
        if self.price_velocity_bps >= self.velocity_threshold() {
            return true;
        }
        false
    }
}

// ── CascadeAlert ─────────────────────────────────────────────────────────────

/// Immutable on-chain record of each circuit-breaker trigger event.
/// One PDA per trigger event, keyed by shield + trigger_count.
///
/// Seeds: [b"cascade_alert", perp_shield.as_ref(), trigger_count_le_bytes]
#[account(zero_copy(unsafe))]
#[repr(C, packed)]
#[derive(Default, Debug)]
pub struct CascadeAlert {
    /// PerpShield that generated this alert
    pub shield: Pubkey,

    /// PerpMarket at time of alert
    pub market: Pubkey,

    /// Sequential trigger number (matches PerpShield.trigger_count at time of event)
    pub alert_index: u64,

    /// Slot the circuit breaker was triggered
    pub triggered_at_slot: u64,

    /// Slot the circuit breaker was reset (0 if still active)
    pub reset_at_slot: u64,

    /// OI imbalance bps at trigger time
    pub oi_imbalance_bps: u16,

    /// Liquidations in epoch at trigger time
    pub liquidations_in_epoch: u16,

    /// Price velocity bps/slot at trigger time
    pub price_velocity_bps: u16,

    /// Which threshold(s) were breached (bit flags: 0x1=OI, 0x2=LiqRate, 0x4=Velocity)
    pub trigger_flags: u8,

    /// Whether this alert was resolved by authority (1) or auto-reset (2)
    pub resolution: u8,

    /// PDA bump
    pub bump: u8,

    /// Reserved
    pub padding: [u8; 5],
}

impl CascadeAlert {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 8 + 8 + 2 + 2 + 2 + 1 + 1 + 1 + 5;

    /// Trigger flag: OI imbalance threshold breached
    pub const FLAG_OI: u8 = 0x01;
    /// Trigger flag: liquidation rate threshold breached
    pub const FLAG_LIQ_RATE: u8 = 0x02;
    /// Trigger flag: price velocity threshold breached
    pub const FLAG_VELOCITY: u8 = 0x04;

    pub fn is_resolved(&self) -> bool {
        self.reset_at_slot > 0
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

#[event]
pub struct PerpShieldInitializedEvent {
    pub market: Pubkey,
    pub shield: Pubkey,
    pub authority: Pubkey,
    pub slot: u64,
}

#[event]
pub struct CircuitBreakerTriggeredEvent {
    pub market: Pubkey,
    pub shield: Pubkey,
    pub trigger_count: u64,
    pub oi_imbalance_bps: u16,
    pub liquidations_in_epoch: u16,
    pub price_velocity_bps: u16,
    pub trigger_flags: u8,
    pub slot: u64,
}

#[event]
pub struct CircuitBreakerResetEvent {
    pub market: Pubkey,
    pub shield: Pubkey,
    pub trigger_count: u64,
    pub by_authority: bool,
    pub slot: u64,
}

#[event]
pub struct LiquidationGuardEvent {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub action: u8,
    pub oi_imbalance_bps: u16,
    pub liquidations_in_epoch: u16,
    pub slot: u64,
}
