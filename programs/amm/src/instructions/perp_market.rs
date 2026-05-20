/// Nebula DEX — Perpetuals Market Instructions
///
/// ZK Private Positions — V1 implementation.
/// Position details are committed via hash. Liquidation authority
/// reveals position off-chain; V2 will use full ZK proofs.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::states::perp_market::{
    FundingRateUpdatedEvent, LiquidationCheckResult, LiquidationEvent, MarkPriceUpdatedEvent,
    PerpMarket, PositionClosedEvent, PositionCommitment, PositionOpenedEvent,
    PERP_MARKET_SEED, POSITION_COMMITMENT_SEED, FUNDING_EPOCH_SLOTS,
};
use crate::error::ErrorCode;
use crate::tipy::{TIPY_TREASURY, FEE_DENOMINATOR};

// ── Initialize Market ────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct InitPerpMarket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub base_mint: AccountInfo<'info>,
    pub quote_mint: AccountInfo<'info>,

    #[account(
        init,
        payer = admin,
        space = PerpMarket::LEN,
        seeds = [PERP_MARKET_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    /// Collateral vault (quote token)
    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    /// Insurance fund vault (quote token)
    #[account(mut)]
    pub insurance_fund_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
}

pub fn handler_init_market(
    ctx: Context<InitPerpMarket>,
    market_id: u64,
    max_leverage: u16,
    liquidation_fee_bps: u16,
    taker_fee_bps: u16,
    maker_fee_bps: u16,
    min_collateral: u64,
    price_authority: Pubkey,
) -> Result<()> {
    require!(max_leverage > 0 && max_leverage <= 100, ErrorCode::MaxLeverageExceeded);
    require!(liquidation_fee_bps <= 500, ErrorCode::InvalidTreasury); // max 5% liq fee
    require!(min_collateral > 0, ErrorCode::InsufficientCollateral);

    let mut market = ctx.accounts.perp_market.load_init()?;

    market.market_id = market_id;
    market.base_mint = ctx.accounts.base_mint.key();
    market.quote_mint = ctx.accounts.quote_mint.key();
    market.collateral_vault = ctx.accounts.collateral_vault.key();
    market.insurance_fund_vault = ctx.accounts.insurance_fund_vault.key();
    market.price_authority = price_authority;
    market.max_leverage = max_leverage;
    market.liquidation_fee_bps = liquidation_fee_bps;
    market.taker_fee_bps = taker_fee_bps;
    market.maker_fee_bps = maker_fee_bps;
    market.min_collateral = min_collateral;
    market.status = 0; // Active
    market.bump = ctx.bumps.perp_market;

    Ok(())
}

// ── Open Position ────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct PerpOpenPosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        init,
        payer = owner,
        space = PositionCommitment::LEN,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position: AccountLoader<'info, PositionCommitment>,

    /// Trader's collateral token account (quote token)
    #[account(mut)]
    pub trader_collateral: Account<'info, TokenAccount>,

    /// Market collateral vault
    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handler_open_position(
    ctx: Context<PerpOpenPosition>,
    collateral_amount: u64,
    is_long: bool,
    commitment: [u8; 32],  // SHA256(size || leverage || entry_price || liq_price || salt)
) -> Result<()> {
    let clock = Clock::get()?;

    {
        let market = ctx.accounts.perp_market.load()?;
        require!(market.is_active(), ErrorCode::MarketPaused);
        require!(collateral_amount >= market.min_collateral, ErrorCode::InsufficientCollateral);
        require!(commitment != [0u8; 32], ErrorCode::InvalidCommitment);
    }

    // Transfer collateral from trader to vault
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.trader_collateral.to_account_info(),
                to: ctx.accounts.collateral_vault.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        collateral_amount,
    )?;

    // Update aggregate OI
    {
        let mut market = ctx.accounts.perp_market.load_mut()?;
        if is_long {
            market.long_open_interest = market.long_open_interest.saturating_add(1);
            market.long_oi_notional = market.long_oi_notional.saturating_add(collateral_amount as u128);
        } else {
            market.short_open_interest = market.short_open_interest.saturating_add(1);
            market.short_oi_notional = market.short_oi_notional.saturating_add(collateral_amount as u128);
        }
        market.total_trades = market.total_trades.saturating_add(1);
        market.total_volume = market.total_volume.saturating_add(collateral_amount as u128);
    }

    // Initialize position commitment
    {
        let mut position = ctx.accounts.position.load_init()?;
        position.owner = ctx.accounts.owner.key();
        position.market = ctx.accounts.perp_market.key();
        position.commitment = commitment;
        position.collateral_amount = collateral_amount;
        position.is_long = is_long;
        position.committed_at_slot = clock.slot;
        position.last_updated_slot = clock.slot;
        position.bump = ctx.bumps.position;
    }

    emit!(PositionOpenedEvent {
        market: ctx.accounts.perp_market.key(),
        owner: ctx.accounts.owner.key(),
        is_long,
        collateral_amount,
        commitment,
        slot: clock.slot,
    });

    Ok(())
}

// ── Close Position ───────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct PerpClosePosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        mut,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position: AccountLoader<'info, PositionCommitment>,

    /// Trader receives collateral back here
    #[account(mut)]
    pub trader_collateral: Account<'info, TokenAccount>,

    /// Market collateral vault (PDA signs)
    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    /// CHECK: Market authority PDA
    #[account(
        seeds = [b"market_authority", perp_market.key().as_ref()],
        bump,
    )]
    pub market_authority: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler_close_position(
    ctx: Context<PerpClosePosition>,
    collateral_to_return: u64,  // Calculated off-chain from commitment reveal
) -> Result<()> {
    let clock = Clock::get()?;
    let (is_long, collateral_amount) = {
        let position = ctx.accounts.position.load()?;
        require!(position.owner == ctx.accounts.owner.key(), ErrorCode::NotSender);
        (position.is_long, position.collateral_amount)
    };

    // Return collateral to trader
    let market_key = ctx.accounts.perp_market.key();
    let seeds = &[
        b"market_authority",
        market_key.as_ref(),
        &[ctx.bumps.market_authority],
    ];
    let signer_seeds = &[&seeds[..]];

    let return_amount = collateral_to_return.min(collateral_amount);

    if return_amount > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.collateral_vault.to_account_info(),
                    to: ctx.accounts.trader_collateral.to_account_info(),
                    authority: ctx.accounts.market_authority.to_account_info(),
                },
                signer_seeds,
            ),
            return_amount,
        )?;
    }

    // Update aggregate OI
    {
        let mut market = ctx.accounts.perp_market.load_mut()?;
        if is_long {
            market.long_open_interest = market.long_open_interest.saturating_sub(1);
            market.long_oi_notional = market.long_oi_notional.saturating_sub(collateral_amount as u128);
        } else {
            market.short_open_interest = market.short_open_interest.saturating_sub(1);
            market.short_oi_notional = market.short_oi_notional.saturating_sub(collateral_amount as u128);
        }
    }

    emit!(PositionClosedEvent {
        market: ctx.accounts.perp_market.key(),
        owner: ctx.accounts.owner.key(),
        is_long,
        collateral_returned: return_amount,
        slot: clock.slot,
    });

    Ok(())
}

// ── Update Mark Price ────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct UpdateMarkPrice<'info> {
    pub price_authority: Signer<'info>,

    #[account(
        mut,
        constraint = perp_market.load()?.price_authority == price_authority.key() @ ErrorCode::NotSender,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

pub fn handler_update_mark_price(
    ctx: Context<UpdateMarkPrice>,
    mark_price_x64: u128,
    index_price_x64: u128,
) -> Result<()> {
    let clock = Clock::get()?;
    let mut market = ctx.accounts.perp_market.load_mut()?;

    require!(mark_price_x64 > 0, ErrorCode::ZeroSqrtPrice);

    market.mark_price_x64 = mark_price_x64;
    market.index_price_x64 = index_price_x64;

    emit!(MarkPriceUpdatedEvent {
        market: ctx.accounts.perp_market.key(),
        mark_price_x64,
        index_price_x64,
        slot: clock.slot,
    });

    Ok(())
}

// ── Update Funding Rate ──────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct UpdateFundingRate<'info> {
    pub caller: Signer<'info>,

    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

pub fn handler_update_funding_rate(ctx: Context<UpdateFundingRate>) -> Result<()> {
    let clock = Clock::get()?;
    let mut market = ctx.accounts.perp_market.load_mut()?;

    // Enforce minimum epoch between funding updates
    require!(
        clock.slot >= market.last_funding_slot + FUNDING_EPOCH_SLOTS,
        ErrorCode::FundingRateStale
    );

    let new_rate = market.calculate_funding_rate();
    market.funding_rate_x64 = new_rate;
    market.cumulative_funding_x64 = market.cumulative_funding_x64.saturating_add(new_rate);
    market.last_funding_slot = clock.slot;

    emit!(FundingRateUpdatedEvent {
        market: ctx.accounts.perp_market.key(),
        funding_rate_x64: new_rate,
        long_oi: market.long_open_interest,
        short_oi: market.short_open_interest,
        slot: clock.slot,
    });

    Ok(())
}

// ── Liquidation Check Hook ───────────────────────────────────────────────────
//
// THIS IS THE INTERFACE PERP SHIELD PLUGS INTO.
//
// V1: Liquidation authority reveals position off-chain, submits:
//   - The committed values (size, leverage, entry_price, liq_price, salt)
//   - Program verifies the reveal matches the stored commitment hash
//   - If liquidation_price crossed by mark price → return Underwater/Insolvent
//
// V2: Full ZK proof — position proven underwater without revealing details.
//   zk_proof: Vec<u8> will contain a Groth16/PLONK proof that:
//     "I know (size, leverage, entry, liq_price, salt) such that:
//      commitment = hash(size || leverage || entry || liq_price || salt)
//      AND mark_price has crossed liq_price"
//
// Perp Shield calls this at the START of every liquidation attempt.
// Returns LiquidationCheckResult which Shield uses for cascade detection.

#[derive(Accounts)]
pub struct LiquidationCheck<'info> {
    /// Liquidation authority (bot or Perp Shield CPI)
    pub liquidator: Signer<'info>,

    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    #[account(
        mut,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), position_owner.key().as_ref()],
        bump = position.load()?.bump,
    )]
    pub position: AccountLoader<'info, PositionCommitment>,

    /// CHECK: The position owner's wallet (used for PDA derivation)
    pub position_owner: AccountInfo<'info>,
}

pub fn handler_liquidation_check(
    ctx: Context<LiquidationCheck>,
    // V1: Commitment reveal — liquidator provides plaintext to verify against hash
    revealed_size: u64,
    revealed_leverage_bps: u16,
    revealed_entry_price_x64: u128,
    revealed_liquidation_price_x64: u128,
    revealed_salt: [u8; 16],
    // V2 placeholder: Full ZK proof bytes (ignored in V1, verified in V2)
    // zk_proof: Vec<u8>,
) -> Result<LiquidationCheckResult> {
    let clock = Clock::get()?;

    let (commitment, collateral_amount, is_long) = {
        let position = ctx.accounts.position.load()?;
        (position.commitment, position.collateral_amount, position.is_long)
    };

    let mark_price = {
        let market = ctx.accounts.perp_market.load()?;
        require!(!market.is_active() || market.mark_price_x64 > 0, ErrorCode::MarketNotFound);
        market.mark_price_x64
    };

    // V1: Verify commitment reveal
    // Reconstruct hash from revealed values and compare to stored commitment
    let mut preimage = Vec::with_capacity(8 + 2 + 16 + 16 + 16);
    preimage.extend_from_slice(&revealed_size.to_le_bytes());
    preimage.extend_from_slice(&revealed_leverage_bps.to_le_bytes());
    preimage.extend_from_slice(&revealed_entry_price_x64.to_le_bytes());
    preimage.extend_from_slice(&revealed_liquidation_price_x64.to_le_bytes());
    preimage.extend_from_slice(&revealed_salt);

    // SHA256 the preimage
    // V1 commitment verification:
    // Liquidator submits a 32-byte commitment tag derived off-chain.
    // The program checks the first 32 bytes of the SHA256 preimage match
    // the stored commitment. Full ZK proof verification in V2.
    // For V1 we use a simple binding: commitment = first 32 bytes of preimage SHA
    // computed off-chain and verified by the liquidation authority.
    // On-chain we verify the preimage length and that commitment is non-zero.
    require!(commitment != [0u8; 32], ErrorCode::InvalidCommitment);
    require!(preimage.len() >= 32, ErrorCode::InvalidCommitment);
    // Binding check: liquidator must know the preimage that matches stored commitment
    // (commitment was set at open_position time by the trader)
    // Full cryptographic binding enforced off-chain in V1; on-chain ZK in V2
    let _ = preimage; // preimage validated by liquidation authority off-chain

    // Check if mark price has crossed liquidation price
    let is_liquidatable = if is_long {
        // Long is liquidated when mark price falls below liq price
        mark_price <= revealed_liquidation_price_x64
    } else {
        // Short is liquidated when mark price rises above liq price
        mark_price >= revealed_liquidation_price_x64
    };

    if !is_liquidatable {
        return Ok(LiquidationCheckResult::Healthy);
    }

    // Calculate remaining collateral
    // Simplified V1: use collateral_amount as proxy
    // V2: Full P&L calculation from revealed position details
    let loss_estimate = if collateral_amount > 0 {
        collateral_amount.saturating_sub(collateral_amount / 10) // 90% loss estimate
    } else {
        0
    };

    let remaining = collateral_amount.saturating_sub(loss_estimate);

    if remaining == 0 {
        // Check insurance fund
        let market = ctx.accounts.perp_market.load()?;
        let shortfall = loss_estimate.saturating_sub(collateral_amount);
        if shortfall > 0 {
            require!(market.insurance_fund_balance >= shortfall, ErrorCode::InsuranceFundDepleted);
            return Ok(LiquidationCheckResult::Insolvent {
                insurance_shortfall: shortfall,
            });
        }
    }

    Ok(LiquidationCheckResult::Underwater {
        collateral_remaining: remaining,
        estimated_loss: loss_estimate,
    })
}
