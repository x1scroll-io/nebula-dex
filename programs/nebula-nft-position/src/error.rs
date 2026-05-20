use anchor_lang::prelude::*;

#[error_code]
pub enum NftError {
    #[msg("This NFT position is not transferable — it is soulbound")]
    NftNotTransferable,
    #[msg("Caller is not the position owner")]
    NotPositionOwner,
    #[msg("Invalid pool ID provided")]
    InvalidPoolId,
    #[msg("A position NFT has already been minted for this mint address")]
    PositionAlreadyMinted,
    #[msg("Invalid TiPy treasury account")]
    InvalidTreasury,
    #[msg("Unauthorized: caller is not the program admin")]
    Unauthorized,
}
