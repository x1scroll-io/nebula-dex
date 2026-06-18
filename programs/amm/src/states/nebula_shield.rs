/// Nebula Shield state accounts
///
/// ArbConfig: per-pool protocol arb sweep configuration
/// JitGuard: per-position JIT liquidity protection

use anchor_lang::prelude::*;

// ── ArbConfig ─────────────────────────────────────────────────────────────────

/// Configuration for the protocol arb sweep on a given pool.
/// One PDA per pool, seeds = [b"arb_config", pool.key().as_ref()]
#[account]
#[derive(Default)]
pub struct ArbConfig {
    /// The pool this config belongs to
    pub pool: Pubkey,
    /// Authority allowed to call arb_sweep (protocol hot wallet)
    pub arb_authority: Pubkey,
    /// Token account that receives the treasury share of arb profit (TiPy)
    pub treasury: Pubkey,
    /// Minimum spread in bps before a sweep is triggered
    pub min_spread_bps: u16,
    /// Share of arb profit routed to treasury (in bps, e.g. 2000 = 20%)
    pub treasury_share_bps: u16,
    /// TWAP window in seconds for reference price
    pub twap_window_seconds: u32,
    /// Minimum sweep amount (in base token lamports)
    pub min_sweep_amount: u64,
    /// Maximum sweep amount cap
    pub max_sweep_amount: u64,
    /// Last slot a sweep was executed (cooldown enforcement)
    pub last_sweep_slot: u64,
    /// Slots between allowed sweeps
    pub cooldown_slots: u64,
    /// Cumulative token A profit captured by protocol
    pub total_profit_captured_a: u64,
    /// Cumulative token B profit captured by protocol
    pub total_profit_captured_b: u64,
    /// Whether the arb sweep is enabled for this pool
    pub enabled: bool,
    /// PDA bump
    pub bump: u8,
    /// Tax rate applied when same-slot manipulation is detected (bps, default 9000 = 90%)
    pub manipulation_tax_bps: u16,
    /// Cumulative manipulation tax captured in token A
    pub manipulation_tax_collected_a: u64,
    /// Cumulative manipulation tax captured in token B
    pub manipulation_tax_collected_b: u64,
    /// Count of times manipulation was detected and taxed
    pub manipulation_detections: u64,
    /// Deviation threshold (in bps) above which same-slot price movement is
    /// classified as manipulation. Configurable by admin. Default: 500 (5%).
    /// Stored as 0 = use default (500) for backwards compatibility with older PDAs.
    pub manipulation_threshold_bps: u16,
}

impl ArbConfig {
    /// Default manipulation deviation threshold: 5% sqrt-price drift vs 5s TWAP
    pub const DEFAULT_MANIPULATION_THRESHOLD_BPS: u16 = 500;

    /// Returns the effective manipulation threshold, falling back to the default
    /// when the stored value is 0 (unset / pre-upgrade PDA).
    pub fn effective_manipulation_threshold_bps(&self) -> u16 {
        if self.manipulation_threshold_bps == 0 {
            Self::DEFAULT_MANIPULATION_THRESHOLD_BPS
        } else {
            self.manipulation_threshold_bps
        }
    }

    /// Space: discriminator(8) + 3*Pubkey(96) + u16+u16(4) + u32(4) + 6*u64(48) + bool(1) + u8(1)
    ///        + manipulation: u16(2) + 3*u64(24) + manipulation_threshold_bps: u16(2)
    pub const LEN: usize = 8 + 96 + 4 + 4 + 48 + 1 + 1 + 2 + 24 + 2;
}

// ── JitGuard ──────────────────────────────────────────────────────────────────

/// Per-position JIT liquidity guard.
/// Seeds = [b"jit_guard", pool.key().as_ref(), nft_mint.key().as_ref()]
#[account]
#[derive(Default)]
pub struct JitGuard {
    /// Pool the guarded position belongs to
    pub pool: Pubkey,
    /// NFT mint identifying the position
    pub position_nft_mint: Pubkey,
    /// Minimum slots that must elapse between add_liquidity and remove_liquidity
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

impl JitGuard {
    /// Absolute max lock slots (safety ceiling so nobody can lock forever)
    pub const MAX_LOCK_SLOTS: u16 = 100;
    /// Default lock if caller passes 0
    pub const DEFAULT_MIN_LOCK_SLOTS: u16 = 5;

    /// Space: discriminator(8) + 2*Pubkey(64) + u16(2) + 4*u64(32) + u8(1)
    pub const LEN: usize = 8 + 64 + 2 + 32 + 1;

    /// Check whether a removal is allowed at the given slot.
    /// Returns (can_remove, slots_remaining).
    pub fn can_remove(&self, current_slot: u64) -> (bool, u64) {
        let unlock_slot = self.last_add_slot + self.min_lock_slots as u64;
        if current_slot >= unlock_slot {
            (true, 0)
        } else {
            (false, unlock_slot - current_slot)
        }
    }

    /// Record that liquidity was added at `slot`.
    pub fn record_add(&mut self, slot: u64) {
        self.last_add_slot = slot;
        self.add_count = self.add_count.saturating_add(1);
    }

    /// Record a removal attempt. If `blocked` is true, increment jit_blocks.
    pub fn record_remove(&mut self, _slot: u64, blocked: bool) {
        self.remove_count = self.remove_count.saturating_add(1);
        if blocked {
            self.jit_blocks = self.jit_blocks.saturating_add(1);
        }
    }
}
