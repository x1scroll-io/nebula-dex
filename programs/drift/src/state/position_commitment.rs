use anchor_lang::prelude::*;

/// A ZK commitment for a private perp position.
///
/// Position details (size, leverage, entry_price, liq_price) are NOT stored
/// on-chain. Only a SHA256 hash of those values is stored. The trader reveals
/// the preimage at close/liquidation time.
///
/// This is a separate PDA from Drift's native PerpPosition — it overlays
/// commitment semantics on top of the existing position without modifying
/// any Drift structs or breaking account sizes.
///
/// V1: Commitment verification is off-chain (liquidation authority verifies
/// the reveal). V2: Full ZK proof verification on-chain.
#[account]
#[derive(Default)]
pub struct PositionCommitment {
    /// The owner of this position
    pub owner: Pubkey,
    /// The perp market this position belongs to
    pub market: Pubkey,
    /// SHA256(size || leverage_bps || entry_price_x64 || liq_price_x64 || salt)
    pub commitment: [u8; 32],
    /// Collateral amount deposited (quote token precision)
    pub collateral_amount: u64,
    /// Whether this is a long position
    pub is_long: bool,
    /// Slot when the position was opened
    pub committed_at_slot: u64,
    /// Slot when the position was last updated
    pub last_updated_slot: u64,
    /// Bump seed for PDA
    pub bump: u8,
    /// Padding for alignment
    pub padding: [u8; 7],
}

impl PositionCommitment {
    pub const LEN: usize = 8 + // discriminator
        32 + // owner
        32 + // market
        32 + // commitment
        8 +  // collateral_amount
        1 +  // is_long
        8 +  // committed_at_slot
        8 +  // last_updated_slot
        1 +  // bump
        7;   // padding

    /// Verify that a revealed preimage matches the stored commitment.
    /// Reconstructs SHA256(revealed_size || revealed_leverage || revealed_entry || revealed_liq || revealed_salt)
    /// and compares against self.commitment.
    pub fn verify_reveal(
        &self,
        revealed_size: u64,
        revealed_leverage_bps: u16,
        revealed_entry_price_x64: u128,
        revealed_liquidation_price_x64: u128,
        revealed_salt: [u8; 16],
    ) -> bool {
        let mut preimage = Vec::with_capacity(8 + 2 + 16 + 16 + 16);
        preimage.extend_from_slice(&revealed_size.to_le_bytes());
        preimage.extend_from_slice(&revealed_leverage_bps.to_le_bytes());
        preimage.extend_from_slice(&revealed_entry_price_x64.to_le_bytes());
        preimage.extend_from_slice(&revealed_liquidation_price_x64.to_le_bytes());
        preimage.extend_from_slice(&revealed_salt);

        let hash = solana_program::hash::hash(&preimage);
        hash.to_bytes() == self.commitment
    }
}

/// PDA seed for PositionCommitment accounts
pub const POSITION_COMMITMENT_SEED: &[u8] = b"position_commitment";
