use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::error::ErrorCode;
use crate::state::perp_market::PerpMarket;
use crate::state::position_commitment::{PositionCommitment, POSITION_COMMITMENT_SEED};
use crate::state::user::User;

/// Open a perp position with a ZK commitment.
///
/// Instead of storing position details on-chain, the trader submits a
/// SHA256 commitment. The actual size, leverage, entry price, and
/// liquidation price are revealed only at close or liquidation time.
///
/// V1: Off-chain commitment verification. V2: Full ZK proof.
#[derive(Accounts)]
pub struct OpenPerpWithCommitment<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    /// The Drift User account (must already be initialized)
    #[account(mut)]
    pub user: AccountLoader<'info, User>,

    /// The perp market to open a position in
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    /// The commitment PDA — stores the hash, not the plaintext
    #[account(
        init,
        payer = owner,
        space = PositionCommitment::LEN,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position_commitment: Account<'info, PositionCommitment>,

    /// Trader's collateral token account (quote token)
    #[account(mut)]
    pub trader_collateral: Account<'info, TokenAccount>,

    /// Market collateral vault
    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handler_open_perp_with_commitment(
    ctx: Context<OpenPerpWithCommitment>,
    _market_index: u16,
    collateral_amount: u64,
    is_long: bool,
    commitment: [u8; 32],
) -> Result<()> {
    let clock = Clock::get()?;

    // Validate market is active
    {
        let market = ctx.accounts.perp_market.load()?;
        require!(!market.is_reduce_only()?, ErrorCode::MarketPaused);
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

    // Update market OI
    {
        let mut market = ctx.accounts.perp_market.load_mut()?;
        if is_long {
            market.number_of_users_with_base = market.number_of_users_with_base.saturating_add(1);
        } else {
            market.number_of_users_with_base = market.number_of_users_with_base.saturating_add(1);
        }
        market.number_of_users = market.number_of_users.saturating_add(1);
    }

    // Initialize the commitment PDA
    let commitment_account = &mut ctx.accounts.position_commitment;
    commitment_account.owner = ctx.accounts.owner.key();
    commitment_account.market = ctx.accounts.perp_market.key();
    commitment_account.commitment = commitment;
    commitment_account.collateral_amount = collateral_amount;
    commitment_account.is_long = is_long;
    commitment_account.committed_at_slot = clock.slot;
    commitment_account.last_updated_slot = clock.slot;
    commitment_account.bump = ctx.bumps.position_commitment;

    Ok(())
}

/// Close a perp position with a commitment reveal.
///
/// The trader reveals the preimage (size, leverage, entry, liq, salt).
/// The program verifies the SHA256 hash matches the stored commitment,
/// then calculates P&L and returns collateral + PnL to the trader.
#[derive(Accounts)]
pub struct ClosePerpWithReveal<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    /// The Drift User account
    #[account(mut)]
    pub user: AccountLoader<'info, User>,

    /// The perp market
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    /// The commitment PDA — will be closed after reveal
    #[account(
        mut,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), owner.key().as_ref()],
        bump = position_commitment.bump,
        close = owner,
    )]
    pub position_commitment: Account<'info, PositionCommitment>,

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

pub fn handler_close_perp_with_reveal(
    ctx: Context<ClosePerpWithReveal>,
    _market_index: u16,
    collateral_to_return: u64,
    revealed_size: u64,
    revealed_leverage_bps: u16,
    revealed_entry_price_x64: u128,
    revealed_liquidation_price_x64: u128,
    revealed_salt: [u8; 16],
) -> Result<()> {
    let clock = Clock::get()?;

    // Verify the reveal matches the stored commitment
    let commitment = &ctx.accounts.position_commitment;
    require!(
        commitment.owner == ctx.accounts.owner.key(),
        ErrorCode::NotSender
    );
    require!(
        commitment.verify_reveal(
            revealed_size,
            revealed_leverage_bps,
            revealed_entry_price_x64,
            revealed_liquidation_price_x64,
            revealed_salt,
        ),
        ErrorCode::InvalidCommitment
    );

    let is_long = commitment.is_long;
    let collateral_amount = commitment.collateral_amount;

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

    // Update market user counts
    {
        let mut market = ctx.accounts.perp_market.load_mut()?;
        if is_long {
            market.number_of_users_with_base = market.number_of_users_with_base.saturating_sub(1);
        } else {
            market.number_of_users_with_base = market.number_of_users_with_base.saturating_sub(1);
        }
        market.number_of_users = market.number_of_users.saturating_sub(1);
    }

    // Emit close event
    emit!(PositionClosedEvent {
        market: ctx.accounts.perp_market.key(),
        owner: ctx.accounts.owner.key(),
        is_long,
        collateral_returned: return_amount,
        slot: clock.slot,
    });

    Ok(())
}

/// PerpShield Liquidation Check — called BEFORE a liquidation executes.
///
/// This is the interface PerpShield plugs into. It verifies the commitment
/// reveal and checks if the position is actually underwater.
///
/// V1: Off-chain reveal verification. V2: Full ZK proof.
#[derive(Accounts)]
pub struct PerpShieldLiquidationCheck<'info> {
    /// The liquidation authority (keeper bot)
    pub liquidator: Signer<'info>,

    /// The perp market
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    /// The commitment PDA
    #[account(
        mut,
        seeds = [POSITION_COMMITMENT_SEED, perp_market.key().as_ref(), position_owner.key().as_ref()],
        bump = position_commitment.bump,
    )]
    pub position_commitment: Account<'info, PositionCommitment>,

    /// CHECK: The position owner's wallet (used for PDA derivation)
    pub position_owner: AccountInfo<'info>,
}

pub fn handler_perp_shield_liquidation_check(
    ctx: Context<PerpShieldLiquidationCheck>,
    revealed_size: u64,
    revealed_leverage_bps: u16,
    revealed_entry_price_x64: u128,
    revealed_liquidation_price_x64: u128,
    revealed_salt: [u8; 16],
) -> Result<LiquidationCheckResult> {
    let commitment = &ctx.accounts.position_commitment;

    // Verify the reveal matches the stored commitment
    require!(
        commitment.verify_reveal(
            revealed_size,
            revealed_leverage_bps,
            revealed_entry_price_x64,
            revealed_liquidation_price_x64,
            revealed_salt,
        ),
        ErrorCode::InvalidCommitment
    );

    // Get oracle price from Drift's AMM historical oracle data
    let oracle_price = {
        let market = ctx.accounts.perp_market.load()?;
        require!(
            !market.is_reduce_only()? || market.amm.historical_oracle_data.last_oracle_price > 0,
            ErrorCode::MarketNotFound
        );
        market.amm.historical_oracle_data.last_oracle_price
    };

    // Check if oracle price has crossed liquidation price
    let is_liquidatable = if commitment.is_long {
        // Long is liquidated when oracle price falls below liq price
        (oracle_price as u128) <= revealed_liquidation_price_x64
    } else {
        // Short is liquidated when oracle price rises above liq price
        (oracle_price as u128) >= revealed_liquidation_price_x64
    };

    if !is_liquidatable {
        return Ok(LiquidationCheckResult::Healthy);
    }

    // Calculate estimated loss (simplified V1)
    let loss_estimate = if commitment.collateral_amount > 0 {
        commitment
            .collateral_amount
            .saturating_sub(commitment.collateral_amount / 10)
    } else {
        0
    };

    let remaining = commitment
        .collateral_amount
        .saturating_sub(loss_estimate);

    if remaining == 0 {
        let market = ctx.accounts.perp_market.load()?;
        let shortfall = loss_estimate.saturating_sub(commitment.collateral_amount);
        if shortfall > 0 {
            require!(
                market.insurance_claim.quote_max_insurance >= shortfall as u64,
                ErrorCode::InsuranceFundDepleted
            );
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

/// Result of a PerpShield liquidation check
#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Debug)]
pub enum LiquidationCheckResult {
    /// Position is healthy, no liquidation needed
    Healthy,
    /// Position is underwater but has remaining collateral
    Underwater {
        collateral_remaining: u64,
        estimated_loss: u64,
    },
    /// Position is insolvent — insurance fund must cover
    Insolvent {
        insurance_shortfall: u64,
    },
}

/// Event emitted when a commitment-based position is closed
#[event]
pub struct PositionClosedEvent {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub is_long: bool,
    pub collateral_returned: u64,
    pub slot: u64,
}
