/// Nebula DEX — Perpetuals Market Structure
///
/// ZK Private Positions design:
/// Individual position details (size, leverage, entry price, liquidation price)
/// are NOT stored in plaintext on-chain. Instead, a Poseidon/SHA256 commitment
/// hash is stored. This prevents bots from reading liquidation clusters and
/// front-running cascade attacks.
///
/// What is PUBLIC (needed for protocol math):
///   - collateral_amount: solvency checks require knowing how much is at risk
///   - is_long: needed for aggregate OI accounting
///
/// What is PRIVATE (inside commitment hash):
///   - position_size: notional size
///   - leverage: multiplier
///   - entry_price: mark price at open
///   - liquidation_price: computed from entry + leverage + collateral
///   - salt: random nonce preventing commitment grinding
///
/// Full ZK proof verification for liquidations is a V2 feature.
/// V1 uses the commitment as an integrity check; liquidation authority
/// verifies off-chain and submits the plaintext + proof.

use anchor_lang::prelude::*;

// ── Seeds ─────────────────────────────────────────────────────────────────────

pub const PERP_MARKET_SEED: &[u8] = b"perp_market";
pub const POSITION_COMMITMENT_SEED: &[u8] = b"position";

// ── Funding rate constants ────────────────────────────────────────────────────

/// Base funding rate per epoch (8-hour funding, in Q64.64 bps)
/// 0.01% per 8h = 3 bps/day
pub const BASE_FUNDING_RATE_BPS: u64 = 1; // 0.01% per funding epoch
pub const FUNDING_EPOCH_SLOTS: u64 = 72_000; // ~8 hours at 400ms/slot

// ── PerpMarket ────────────────────────────────────────────────────────────────

/// One account per perpetual trading pair.
/// Seeds: [b"perp_market", base_mint.as_ref(), quote_mint.as_ref()]
#[account(zero_copy(unsafe))]
#[repr(C, packed)]
#[derive(Default, Debug)]
pub struct PerpMarket {
    /// Unique market identifier
    pub market_id: u64,
    /// Base asset mint (e.g. XNT)
    pub base_mint: Pubkey,
    /// Quote asset mint (e.g. USDC)
    pub quote_mint: Pubkey,
    /// Collateral vault holding trader deposits
    pub collateral_vault: Pubkey,
    /// Insurance fund vault
    pub insurance_fund_vault: Pubkey,
    /// Authority allowed to update mark price
    pub price_authority: Pubkey,

    /// Current mark price as Q64.64 fixed point
    pub mark_price_x64: u128,
    /// Oracle index price (smoothed reference) as Q64.64
    pub index_price_x64: u128,

    /// Current funding rate as signed Q64.64
    /// Positive = longs pay shorts. Negative = shorts pay longs.
    pub funding_rate_x64: i128,
    /// Slot of last funding rate update
    pub last_funding_slot: u64,
    /// Cumulative funding (for position P&L settlement)
    pub cumulative_funding_x64: i128,

    /// Aggregate long open interest (number of long positions)
    pub long_open_interest: u64,
    /// Aggregate short open interest (number of short positions)
    pub short_open_interest: u64,
    /// Total notional value of all long positions (in quote token lamports)
    pub long_oi_notional: u128,
    /// Total notional value of all short positions
    pub short_oi_notional: u128,

    /// Insurance fund balance (in quote token lamports)
    pub insurance_fund_balance: u64,

    /// Maximum allowed leverage (e.g. 20 = 20x)
    pub max_leverage: u16,
    /// Fee charged on liquidations (in bps)
    pub liquidation_fee_bps: u16,
    /// Taker fee (in bps)
    pub taker_fee_bps: u16,
    /// Maker fee rebate (in bps, can be 0)
    pub maker_fee_bps: u16,
    /// Minimum collateral to open a position (in quote token lamports)
    pub min_collateral: u64,

    /// Lifetime trade count
    pub total_trades: u64,
    /// Lifetime volume (in quote token lamports)
    pub total_volume: u128,

    /// Market status: 0 = Active, 1 = Paused, 2 = Emergency (liquidations only)
    pub status: u8,

    /// PDA bump
    pub bump: u8,

    /// Reserved for future use
    pub padding: [u8; 6],
}

impl PerpMarket {
    /// Space: discriminator(8) + market_id(8) + 5*Pubkey(160) +
    ///        mark/index prices(32) + funding(24+8+16) + OI(16+16+32+32) +
    ///        insurance(8) + fees(8) + min_collateral(8) +
    ///        stats(8+16) + status(1) + bump(1) + padding(6)
    pub const LEN: usize = 8 + 8 + 160 + 32 + 48 + 96 + 8 + 8 + 8 + 24 + 2 + 8;

    pub fn is_active(&self) -> bool {
        self.status == 0
    }

    pub fn is_emergency(&self) -> bool {
        self.status == 2
    }

    /// Net OI skew: positive = more longs, negative = more shorts
    /// Used by Perp Shield cascade detection and funding rate calculation
    pub fn oi_skew(&self) -> i128 {
        self.long_open_interest as i128 - self.short_open_interest as i128
    }

    /// OI imbalance ratio in bps (0-10000)
    /// 0 = perfectly balanced, 10000 = all one side
    pub fn oi_imbalance_bps(&self) -> u16 {
        let total = self.long_open_interest + self.short_open_interest;
        if total == 0 {
            return 0;
        }
        let skew = self.long_open_interest.abs_diff(self.short_open_interest);
        ((skew as u128 * 10_000) / total as u128).min(10_000) as u16
    }

    /// Calculate funding rate based on OI imbalance
    /// funding_rate = skew_ratio * BASE_FUNDING_RATE_BPS / 10000
    pub fn calculate_funding_rate(&self) -> i128 {
        let total = (self.long_open_interest + self.short_open_interest) as i128;
        if total == 0 {
            return 0;
        }
        let skew = self.long_open_interest as i128 - self.short_open_interest as i128;
        // Normalize to bps then scale by base rate
        let rate_bps = skew * BASE_FUNDING_RATE_BPS as i128 / total;
        rate_bps
    }
}

// ── PositionCommitment ────────────────────────────────────────────────────────

/// One account per trader per market. Stores a commitment hash of the position.
///
/// PRIVATE (inside commitment hash, never stored plaintext):
///   - position_size_lamports: u64
///   - leverage_bps: u16 (e.g. 2000 = 20x)
///   - entry_price_x64: u128
///   - liquidation_price_x64: u128
///   - salt: [u8; 16]  (random nonce)
///
/// PUBLIC (stored on-chain, needed for protocol):
///   - collateral_amount: solvency verification
///   - is_long: OI accounting
///
/// Seeds: [b"position", market.as_ref(), owner.as_ref()]
#[account(zero_copy(unsafe))]
#[repr(C, packed)]
#[derive(Default, Debug)]
pub struct PositionCommitment {
    /// Trader's wallet
    pub owner: Pubkey,
    /// PerpMarket this position belongs to
    pub market: Pubkey,

    /// Commitment hash of private position details
    /// SHA256(position_size || leverage_bps || entry_price_x64 || liquidation_price_x64 || salt)
    /// V2: Replace with Poseidon hash for ZK proof compatibility
    pub commitment: [u8; 32],

    /// Collateral deposited (in quote token lamports) — PUBLIC
    /// Required for solvency checks without revealing position size
    pub collateral_amount: u64,

    /// Direction of the position — PUBLIC
    /// Required for aggregate OI accounting
    pub is_long: bool,

    /// Slot when position was opened
    pub committed_at_slot: u64,
    /// Slot of last update (e.g. adding collateral, partial close)
    pub last_updated_slot: u64,

    /// Cumulative funding at time of last settlement
    pub last_cumulative_funding_x64: i128,

    /// PDA bump
    pub bump: u8,

    /// Reserved
    pub padding: [u8; 6],
}

impl PositionCommitment {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 8 + 1 + 8 + 8 + 16 + 1 + 6;
}

// ── LiquidationCheckResult ─────────────────────────────────────────────────────

/// Result of a liquidation check — returned by the liquidation hook.
/// This is the interface Perp Shield plugs into.
///
/// V1: liquidation authority verifies position off-chain, submits commitment reveal
/// V2: Full ZK proof — position proven underwater without revealing details
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub enum LiquidationCheckResult {
    /// Position is healthy — no action needed
    Healthy,
    /// Position is underwater — eligible for liquidation
    Underwater {
        /// Remaining collateral after losses (may be 0)
        collateral_remaining: u64,
        /// Estimated loss (for insurance fund accounting)
        estimated_loss: u64,
    },
    /// Position has no remaining collateral — insurance fund covers gap
    Insolvent {
        /// Shortfall to be covered by insurance fund
        insurance_shortfall: u64,
    },
}

// ── Events ────────────────────────────────────────────────────────────────────

#[event]
pub struct PositionOpenedEvent {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub is_long: bool,
    pub collateral_amount: u64,
    pub commitment: [u8; 32],
    pub slot: u64,
}

#[event]
pub struct PositionClosedEvent {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub is_long: bool,
    pub collateral_returned: u64,
    pub slot: u64,
}

#[event]
pub struct LiquidationEvent {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub collateral_seized: u64,
    pub insurance_shortfall: u64,
    pub slot: u64,
}

#[event]
pub struct FundingRateUpdatedEvent {
    pub market: Pubkey,
    pub funding_rate_x64: i128,
    pub long_oi: u64,
    pub short_oi: u64,
    pub slot: u64,
}

#[event]
pub struct MarkPriceUpdatedEvent {
    pub market: Pubkey,
    pub mark_price_x64: u128,
    pub index_price_x64: u128,
    pub slot: u64,
}
