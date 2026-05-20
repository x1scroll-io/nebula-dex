use anchor_lang::prelude::*;

/// Per-position NFT record PDA.
/// Seeds: [b"nft_position", nft_mint.key().as_ref()]
#[account]
#[derive(Default)]
pub struct NftPositionRecord {
    /// The NFT mint address for this position
    pub nft_mint: Pubkey,
    /// Pool this position belongs to
    pub pool_id: Pubkey,
    /// Lower tick bound of the concentrated liquidity range
    pub tick_lower: i32,
    /// Upper tick bound of the concentrated liquidity range
    pub tick_upper: i32,
    /// Current owner of the position (wallet that holds the NFT)
    pub owner: Pubkey,
    /// Whether this NFT can be transferred (soulbound by default)
    pub transferable: bool,
    /// PDA bump
    pub bump: u8,
}

impl NftPositionRecord {
    /// discriminator(8) + Pubkey*3(96) + i32*2(8) + bool(1) + u8(1) = 114
    pub const LEN: usize = 96 + 8 + 1 + 1;
}
