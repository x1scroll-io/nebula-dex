// SLIPPAGE PROTECTION — CONFIRMED PRESENT
// swap_v2 enforces slippage via `other_amount_threshold`:
//   - exact-input (is_base_input=true):  output must be >= other_amount_threshold → TooLittleOutputReceived
//   - exact-output (is_base_input=false): input must be <= other_amount_threshold → TooMuchInputPaid
// See: pub fn swap_v2(..., other_amount_threshold: u64, ...) → require_gte! checks at lines ~441
use crate::error::ErrorCode;
use crate::libraries::tick_math;
use crate::swap::{swap_internal, SwapInternalResult};
use crate::util::*;
use crate::{states::*, util};
use anchor_lang::{prelude::*, solana_program};
use anchor_spl::memo::Memo;
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, Token2022, TokenAccount};
use std::collections::VecDeque;

/// Memo msg for swap
pub const SWAP_MEMO_MSG: &'static [u8] = b"raydium_swap";
#[derive(Accounts)]
pub struct SwapSingleV2<'info> {
    /// The user performing the swap
    pub payer: Signer<'info>,

    /// The factory state to read protocol fees
    #[account(address = pool_state.load()?.amm_config)]
    pub amm_config: Box<Account<'info, AmmConfig>>,

    /// The program account of the pool in which the swap will be performed
    #[account(mut)]
    pub pool_state: AccountLoader<'info, PoolState>,

    /// The user token account for input token
    #[account(mut)]
    pub input_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The user token account for output token
    #[account(mut)]
    pub output_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for input token
    #[account(mut)]
    pub input_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for output token
    #[account(mut)]
    pub output_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The program account for the most recent oracle observation
    #[account(mut, address = pool_state.load()?.observation_key)]
    pub observation_state: AccountLoader<'info, ObservationState>,

    /// SPL program for token transfers
    pub token_program: Program<'info, Token>,

    /// SPL program 2022 for token transfers
    pub token_program_2022: Program<'info, Token2022>,

    /// Memo program
    pub memo_program: Program<'info, Memo>,

    /// The mint of token vault 0
    #[account(
        address = input_vault.mint
    )]
    pub input_vault_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The mint of token vault 1
    #[account(
        address = output_vault.mint
    )]
    pub output_vault_mint: Box<InterfaceAccount<'info, Mint>>,
    // remaining accounts
    // tickarray_bitmap_extension: must add account if need
    // tick_array_account_1
    // tick_array_account_2
    // tick_array_account_...
}

/// Performs a single exact input/output swap
/// if is_base_input = true, return value is the max_amount_out, otherwise is min_amount_in
pub fn exact_internal_v2<'c: 'info, 'info>(
    ctx: &mut SwapSingleV2<'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<u64> {
    // invoke_memo_instruction(SWAP_MEMO_MSG, ctx.memo_program.to_account_info())?;

    let block_timestamp = solana_program::clock::Clock::get()?.unix_timestamp as u64;

    let mut swap_result: SwapInternalResult;
    let zero_for_one;
    let swap_price_before;

    let input_balance_before = ctx.input_token_account.amount;
    let output_balance_before = ctx.output_token_account.amount;

    // calculate specified amount because the amount includes transfer_fee as input and without transfer_fee as output
    let (amount_calculate_specified, transfer_fee) = if is_base_input {
        let transfer_fee = util::get_transfer_fee(ctx.input_vault_mint.clone(), amount_specified)?;
        (amount_specified - transfer_fee, transfer_fee)
    } else {
        let transfer_fee =
            util::get_transfer_inverse_fee(ctx.output_vault_mint.clone(), amount_specified)?;
        (amount_specified + transfer_fee, transfer_fee)
    };

    {
        let pool_state = &mut ctx.pool_state.load_mut()?;
        zero_for_one = ctx.input_vault.mint == pool_state.token_mint_0;

        // Nebula Shield (Layer 3) reuses pool.open_time as a slot tracker
        // for back-run detection. The original timestamp gate is disabled
        // upstream (see PoolState.open_time comment); the value here is a
        // slot, which is always smaller than the unix timestamp so the
        // require_gt assertion remains satisfied.
        require_gt!(block_timestamp, pool_state.padding1[0]);

        require!(
            if zero_for_one {
                ctx.input_vault.key() == pool_state.token_vault_0
                    && ctx.output_vault.key() == pool_state.token_vault_1
            } else {
                ctx.input_vault.key() == pool_state.token_vault_1
                    && ctx.output_vault.key() == pool_state.token_vault_0
            },
            ErrorCode::InvalidInputPoolVault
        );

        let mut tickarray_bitmap_extension = None;
        let mut arb_config_account_info: Option<&AccountInfo<'info>> = None;
        let tick_array_states = &mut VecDeque::new();

        let pool_key = ctx.pool_state.key();
        let tick_array_bitmap_extension_key = TickArrayBitmapExtension::key(pool_key);
        let (arb_config_pda, _) = Pubkey::find_program_address(
            &[b"arb_config", pool_key.as_ref()],
            &crate::ID,
        );

        for account_info in remaining_accounts.into_iter() {
            if account_info.key().eq(&tick_array_bitmap_extension_key) {
                tickarray_bitmap_extension = Some(account_info);
                continue;
            }
            if account_info.key().eq(&arb_config_pda) {
                arb_config_account_info = Some(account_info);
                continue;
            }
            if account_info.data_len() != TickArrayState::LEN {
                break;
            }
            tick_array_states.push_back(AccountLoad::load_data_mut(account_info)?);
        }

        // ── Nebula Shield: Layer 1 pre-swap arb sweep ──
        // Best-effort: never blocks swap. Mutations to pool.sqrt_price happen
        // before swap_price_before is captured below so the post-swap
        // monotonicity check remains valid.
        {
            let observation = ctx.observation_state.load()?;
            crate::instructions::nebula_shield::try_pre_swap_shield(
                pool_state,
                &*observation,
                arb_config_account_info,
                pool_key,
                block_timestamp as u32,
            );
        }

        // ── Nebula Shield: Layer 2 honeypot manipulation tax ──
        // Runs only for is_base_input swaps: detects same-slot manipulation
        // against a short TWAP, routes a configurable share of the input
        // into pool.protocol_fees (the same field collect_protocol_fee drains
        // to the TiPy treasury), and reduces the amount handed to
        // swap_internal so the attacker pays full input for a much smaller
        // effective swap.
        let manipulation_tax_amount: u64 = if is_base_input {
            let observation = ctx.observation_state.load()?;
            crate::instructions::nebula_shield::try_apply_manipulation_tax(
                pool_state,
                &*observation,
                arb_config_account_info,
                pool_key,
                block_timestamp as u32,
                amount_calculate_specified,
                zero_for_one,
            )
        } else {
            0
        };
        let amount_for_swap = amount_calculate_specified.saturating_sub(manipulation_tax_amount);

        // Capture sqrt price AFTER any shield correction so the post-swap
        // monotonicity check (require_gte! below) is consistent with swap
        // direction.
        swap_price_before = pool_state.sqrt_price_x64;

        swap_result = swap_internal(
            &ctx.amm_config,
            pool_state,
            tick_array_states,
            &mut ctx.observation_state.load_mut()?,
            tickarray_bitmap_extension,
            amount_for_swap,
            if sqrt_price_limit_x64 == 0 {
                if zero_for_one {
                    tick_math::MIN_SQRT_PRICE_X64 + 1
                } else {
                    tick_math::MAX_SQRT_PRICE_X64 - 1
                }
            } else {
                sqrt_price_limit_x64
            },
            zero_for_one,
            is_base_input,
            oracle::block_timestamp(),
        )?;

        // Re-attribute the manipulation tax to the input side of swap_result
        // so the downstream transfer accounting and the require_eq! invariant
        // on the user's deposit observe the full amount the user committed.
        // The vault receives the full deposit; the residual sits in
        // pool.protocol_fees on the input-token side.
        if manipulation_tax_amount > 0 {
            if zero_for_one {
                swap_result.amount_0 = swap_result
                    .amount_0
                    .saturating_add(manipulation_tax_amount);
            } else {
                swap_result.amount_1 = swap_result
                    .amount_1
                    .saturating_add(manipulation_tax_amount);
            }
        }

        #[cfg(feature = "enable-log")]
        msg!(
            "exact_swap_internal, is_base_input:{}, amount_0: {}, amount_1: {}",
            is_base_input,
            swap_result.amount_0,
            swap_result.amount_1
        );
        require!(
            swap_result.amount_0 != 0 && swap_result.amount_1 != 0,
            ErrorCode::TooSmallInputOrOutputAmount
        );

        // ── Nebula Shield: Layer 3 — record this swap's slot on the pool
        // so any same-slot liquidity add gets flagged as a suspected back-run.
        if let Ok(clock) = Clock::get() {
            crate::instructions::nebula_shield::record_swap_slot(&mut **pool_state, clock.slot);
        }
    }
    let (token_account_0, token_account_1, vault_0, vault_1, vault_0_mint, vault_1_mint) =
        if zero_for_one {
            (
                ctx.input_token_account.clone(),
                ctx.output_token_account.clone(),
                ctx.input_vault.clone(),
                ctx.output_vault.clone(),
                ctx.input_vault_mint.clone(),
                ctx.output_vault_mint.clone(),
            )
        } else {
            (
                ctx.output_token_account.clone(),
                ctx.input_token_account.clone(),
                ctx.output_vault.clone(),
                ctx.input_vault.clone(),
                ctx.output_vault_mint.clone(),
                ctx.input_vault_mint.clone(),
            )
        };

    let amount_0_without_fee;
    let amount_1_without_fee;
    let transfer_fee_0;
    let transfer_fee_1;
    let transfer_amount_0;
    let transfer_amount_1;
    if zero_for_one {
        transfer_fee_0 = if is_base_input && swap_result.amount_0 == amount_calculate_specified {
            transfer_fee
        } else {
            util::get_transfer_inverse_fee(vault_0_mint.clone(), swap_result.amount_0)?
        };
        transfer_fee_1 = util::get_transfer_fee(vault_1_mint.clone(), swap_result.amount_1)?;

        amount_0_without_fee = swap_result.amount_0;
        amount_1_without_fee = swap_result
            .amount_1
            .checked_sub(transfer_fee_1)
            .ok_or(ErrorCode::CalculateOverflow)?;
        (transfer_amount_0, transfer_amount_1) = (
            swap_result
                .amount_0
                .checked_add(transfer_fee_0)
                .ok_or(ErrorCode::CalculateOverflow)?,
            swap_result.amount_1,
        );
    } else {
        transfer_fee_0 = util::get_transfer_fee(vault_0_mint.clone(), swap_result.amount_0)?;
        transfer_fee_1 = if is_base_input && swap_result.amount_1 == amount_calculate_specified {
            transfer_fee
        } else {
            util::get_transfer_inverse_fee(vault_1_mint.clone(), swap_result.amount_1)?
        };

        amount_0_without_fee = swap_result
            .amount_0
            .checked_sub(transfer_fee_0)
            .ok_or(ErrorCode::CalculateOverflow)?;
        amount_1_without_fee = swap_result.amount_1;
        (transfer_amount_0, transfer_amount_1) = (
            swap_result.amount_0,
            swap_result
                .amount_1
                .checked_add(transfer_fee_1)
                .ok_or(ErrorCode::CalculateOverflow)?,
        );
    }
    #[cfg(feature = "enable-log")]
    msg!(
        "amount_0:{}, transfer_fee_0:{}, amount_1:{}, transfer_fee_1:{}",
        swap_result.amount_0,
        transfer_fee_0,
        swap_result.amount_1,
        transfer_fee_1
    );

    emit!(SwapEvent {
        pool_state: ctx.pool_state.key(),
        sender: ctx.payer.key(),
        token_account_0: token_account_0.key(),
        token_account_1: token_account_1.key(),
        amount_0: amount_0_without_fee,
        transfer_fee_0,
        amount_1: amount_1_without_fee,
        transfer_fee_1,
        zero_for_one,
        sqrt_price_x64: swap_result.sqrt_price_x64,
        liquidity: swap_result.liquidity,
        tick: swap_result.tick,
        trade_fee_0: swap_result.trade_fee_0,
        trade_fee_1: swap_result.trade_fee_1,
    });

    if zero_for_one {
        //  x -> y, deposit x token from user to pool vault.
        transfer_from_user_to_pool_vault(
            &ctx.payer,
            &token_account_0.to_account_info(),
            &vault_0.to_account_info(),
            Some(vault_0_mint),
            &ctx.token_program,
            Some(ctx.token_program_2022.to_account_info()),
            transfer_amount_0,
        )?;
        // x -> y，transfer y token from pool vault to user.
        transfer_from_pool_vault_to_user(
            &ctx.pool_state,
            &vault_1.to_account_info(),
            &token_account_1.to_account_info(),
            Some(vault_1_mint),
            &ctx.token_program,
            Some(ctx.token_program_2022.to_account_info()),
            transfer_amount_1,
        )?;
    } else {
        transfer_from_user_to_pool_vault(
            &ctx.payer,
            &token_account_1.to_account_info(),
            &vault_1.to_account_info(),
            Some(vault_1_mint),
            &ctx.token_program,
            Some(ctx.token_program_2022.to_account_info()),
            transfer_amount_1,
        )?;
        transfer_from_pool_vault_to_user(
            &ctx.pool_state,
            &vault_0.to_account_info(),
            &token_account_0.to_account_info(),
            Some(vault_0_mint),
            &ctx.token_program,
            Some(ctx.token_program_2022.to_account_info()),
            transfer_amount_0,
        )?;
    }
    ctx.output_token_account.reload()?;
    ctx.input_token_account.reload()?;

    if zero_for_one {
        require_gte!(swap_price_before, swap_result.sqrt_price_x64);
    } else {
        require_gte!(swap_result.sqrt_price_x64, swap_price_before);
    }
    if sqrt_price_limit_x64 == 0 {
        // Does't allow partial filled without specified limit_price.
        if is_base_input {
            if zero_for_one {
                require_eq!(amount_specified, transfer_amount_0);
            } else {
                require_eq!(amount_specified, transfer_amount_1);
            }
        } else {
            if zero_for_one {
                require_eq!(amount_calculate_specified, transfer_amount_1);
            } else {
                require_eq!(amount_calculate_specified, transfer_amount_0);
            }
        }
    }

    if is_base_input {
        ctx.output_token_account
            .amount
            .checked_sub(output_balance_before)
            .ok_or(ErrorCode::CalculateOverflow.into())
    } else {
        input_balance_before
            .checked_sub(ctx.input_token_account.amount)
            .ok_or(ErrorCode::CalculateOverflow.into())
    }
}

pub fn swap_v2<'a, 'b, 'c: 'info, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, SwapSingleV2<'info>>,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<()> {
    let amount_result = exact_internal_v2(
        ctx.accounts,
        ctx.remaining_accounts,
        amount,
        sqrt_price_limit_x64,
        is_base_input,
    )?;
    if is_base_input {
        require_gte!(
            amount_result,
            other_amount_threshold,
            ErrorCode::TooLittleOutputReceived
        );
    } else {
        require_gte!(
            other_amount_threshold,
            amount_result,
            ErrorCode::TooMuchInputPaid
        );
    }

    Ok(())
}
