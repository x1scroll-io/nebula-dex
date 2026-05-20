// Nebula Shield — State accounts
//
// Three PDAs:
//  - ShieldConfig: per-pool shield + arb sweep configuration
//  - JitGuardState: per-pool JIT liquidity lock tracking
//  - GlobalShieldState: program-wide cumulative stats

use anchor_lang::prelude::*;

// ── ShieldConfig ──────────────────────────────────────────────────────────────

/// Per-pool Nebula Shield configuration.
/// Seeds: [b"shield_config", pool.key().as_ref()]
#[account]
#[derive(Default)]
pub struct ShieldConfig {
    /// Pool this config belongs to
    pub pool: Pubkey,
    /// Authority allowed to call arb_sweep and update_shield_config
    pub arb_authority: Pubkey,
    /// Treasury token account (TiPy) receiving arb sweep profits
    pub treasury: Pubkey,
    /// Minimum spread in bps before an arb sweep is triggered
    pub min_spread_bps: u16,
    /// Share of arb profit routed to treasury (in bps, e.g. 2000 = 20%)
    pub treasury_share_bps: u16,
    /// TWAP window in seconds for reference price
    pub twap_window_seconds: u32,
    /// Minimum sweep amount (in base token lamports)
    pub min_sweep_amount: u64,
    /// Maximum sweep amount cap
    pub max_sweep_amount: u64,
    /// Last slot an arb sweep was executed (cooldown enforcement)
    pub last_sweep_slot: u64,
    /// Minimum slots between allowed sweeps
    pub cooldown_slots: u64,
    /// Minimum bps deviation before same-slot manipulation is flagged
    pub manipulation_threshold_bps: u16,
    /// Minimum lock slots for JIT protection (anti back-run window)
    pub min_jit_lock_slots: u16,
    /// Cumulative token A profit captured by protocol
    pub total_profit_captured_a: u64,
    /// Cumulative token B profit captured by protocol
    pub total_profit_captured_b: u64,
    /// Whether the shield is active for this pool
    pub enabled: bool,
    /// PDA bump
    pub bump: u8,
}

impl ShieldConfig {
    /// discriminator(8) + 3*Pubkey(96) + 4*u16(8) + u32(4) + 6*u64(48) + bool(1) + u8(1) = 166
    pub const LEN: usize = 8 + 96 + 8 + 4 + 48 + 1 + 1;
}

// ── JitGuardState ─────────────────────────────────────────────────────────────

/// Per-pool JIT liquidity guard.
/// Tracks add/remove liquidity slots to detect sandwich back-runs.
/// Seeds: [b"jit_guard_state", pool.key().as_ref(), position_nft_mint.key().as_ref()]
#[account]
#[derive(Default)]
pub struct JitGuardState {
    /// Pool the guarded position belongs to
    pub pool: Pubkey,
    /// NFT mint identifying the specific position
    pub position_nft_mint: Pubkey,
    /// Minimum slots between add_liquidity and remove_liquidity
    pub min_lock_slots: u16,
    /// Slot of the most recent add_liquidity call
    pub last_add_slot: u64,
    /// Total liquidity additions recorded
    pub add_count: u64,
    /// Total liquidity removals recorded
    pub remove_count: u64,
    /// Number of times a removal was blocked as a suspected JIT attack
    pub jit_blocks: u64,
    /// PDA bump
    pub bump: u8,
}

impl JitGuardState {
    /// Absolute ceiling — nobody can lock forever
    pub const MAX_LOCK_SLOTS: u16 = 100;
    /// Default lock slots when caller passes 0
    pub const DEFAULT_MIN_LOCK_SLOTS: u16 = 5;

    /// discriminator(8) + 2*Pubkey(64) + u16(2) + 4*u64(32) + u8(1) = 107
    pub const LEN: usize = 8 + 64 + 2 + 32 + 1;

    /// Returns (can_remove, slots_remaining) at the given slot.
    pub fn can_remove(&self, current_slot: u64) -> (bool, u64) {
        let unlock_slot = self.last_add_slot + self.min_lock_slots as u64;
        if current_slot >= unlock_slot {
            (true, 0)
        } else {
            (false, unlock_slot - current_slot)
        }
    }

    pub fn record_add(&mut self, slot: u64) {
        self.last_add_slot = slot;
        self.add_count = self.add_count.saturating_add(1);
    }

    pub fn record_remove(&mut self, _slot: u64, blocked: bool) {
        self.remove_count = self.remove_count.saturating_add(1);
        if blocked {
            self.jit_blocks = self.jit_blocks.saturating_add(1);
        }
    }
}

// ── GlobalShieldState ─────────────────────────────────────────────────────────

/// Program-wide cumulative Nebula Shield statistics.
/// Seeds: [b"global_shield"]
#[account]
#[derive(Default)]
pub struct GlobalShieldState {
    /// Program admin — can update configs
    pub admin: Pubkey,
    /// Total pools with shield enabled
    pub pools_protected: u64,
    /// Total JIT attacks blocked across all pools
    pub total_jit_blocks: u64,
    /// Total arb sweeps executed across all pools
    pub total_arb_sweeps: u64,
    /// Total lamports swept to treasury
    pub total_treasury_routed: u64,
    /// PDA bump
    pub bump: u8,
}

impl GlobalShieldState {
    /// discriminator(8) + Pubkey(32) + 4*u64(32) + u8(1) = 73
    pub const LEN: usize = 8 + 32 + 32 + 1;
}
