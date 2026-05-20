// Nebula DEX — NFT Position Program
//
// Tokenizes concentrated liquidity LP positions as NFTs.
// Each position in the Nebula AMM gets a unique NFT the user holds in their wallet.
// NFTs are soulbound by default; admin can unlock transferability for position trading.
//
// Program ID: replace with deployed address — this is a placeholder.

pub mod error;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;
use instructions::*;

// Placeholder ID — 43 chars, valid 32-byte base58 pubkey. Replace at deploy.
declare_id!("NebNFT1111111111111111111111111111111111111");

#[program]
pub mod nebula_nft_position {
    use super::*;

    /// Mint an NFT representing an LP position.
    /// Charges 0.001 XNT to TiPy treasury. Position is soulbound by default.
    pub fn mint_position_nft(
        ctx: Context<MintPositionNft>,
        pool_id: Pubkey,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<()> {
        instructions::mint_nft::handler(ctx, pool_id, tick_lower, tick_upper)
    }

    /// Burn the position NFT when the LP position is closed.
    /// Returns rent lamports to the owner. Position record PDA is closed.
    pub fn burn_position_nft(ctx: Context<BurnPositionNft>) -> Result<()> {
        instructions::burn_nft::handler(ctx)
    }

    /// Admin-only: toggle whether this position NFT can be transferred.
    /// Enables/disables position trading for a specific NFT.
    pub fn set_transferable(ctx: Context<SetTransferable>, transferable: bool) -> Result<()> {
        instructions::set_transferable::handler(ctx, transferable)
    }

    /// Transfer a position NFT to a new owner.
    /// Fails if the NFT is soulbound (transferable == false).
    pub fn transfer_position(ctx: Context<TransferPosition>) -> Result<()> {
        instructions::transfer_position::handler(ctx)
    }
}
