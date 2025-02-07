use std::collections::VecDeque;
use std::ops::Deref;

use crate::error::ErrorCode;
use crate::libraries::tick_math;
use crate::swap::swap_internal;
use crate::util::*;
use crate::{states::*, util};
use anchor_lang::prelude::*;
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, Token2022, TokenAccount};

/// Memo msg for swap
pub const SWAP_MEMO_MSG: &'static [u8] = b"raydium_swap";

#[event_cpi]
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

    /// CHECK:
    #[account(
        address = spl_memo::id()
    )]
    pub memo_program: UncheckedAccount<'info>,

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
    // tickarray_bitmap_extension: must add account if need regardless the sequence
    // tick_array_account_1
    // tick_array_account_2
    // tick_array_account_...
}

/// Performs a single exact input/output swap
/// if is_base_input = true, return vaule is the max_amount_out, otherwise is min_amount_in
pub fn exact_internal_v2<'c: 'info, 'info>(
    ctx: Context<'_, '_, 'c, 'info, SwapSingleV2>,
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<u64> {
    // invoke_memo_instruction(SWAP_MEMO_MSG, ctx.memo_program.to_account_info())?;

    let block_timestamp = solana_program::clock::Clock::get()?.unix_timestamp as u64;

    let amount_0;
    let amount_1;
    let zero_for_one;
    let swap_price_before;

    let input_balance_before = ctx.accounts.input_token_account.amount;
    let output_balance_before = ctx.accounts.output_token_account.amount;

    // calculate specified amount because the amount includes thransfer_fee as input and without thransfer_fee as output
    let amount_calculate_specified = if is_base_input {
        let transfer_fee =
            util::get_transfer_fee(ctx.accounts.input_vault_mint.clone(), amount_specified)
                .unwrap();
        amount_specified - transfer_fee
    } else {
        let transfer_fee = util::get_transfer_inverse_fee(
            ctx.accounts.output_vault_mint.clone(),
            amount_specified,
        )
        .unwrap();
        amount_specified + transfer_fee
    };

    {
        swap_price_before = ctx.accounts.pool_state.load()?.sqrt_price_x64;
        let pool_state = &mut ctx.accounts.pool_state.load_mut()?;
        zero_for_one = ctx.accounts.input_vault.mint == pool_state.token_mint_0;

        require_gt!(block_timestamp, pool_state.open_time);

        require!(
            if zero_for_one {
                ctx.accounts.input_vault.key() == pool_state.token_vault_0
                    && ctx.accounts.output_vault.key() == pool_state.token_vault_1
            } else {
                ctx.accounts.input_vault.key() == pool_state.token_vault_1
                    && ctx.accounts.output_vault.key() == pool_state.token_vault_0
            },
            ErrorCode::InvalidInputPoolVault
        );

        let mut tickarray_bitmap_extension = None;
        let tick_array_states = &mut VecDeque::new();

        let tick_array_bitmap_extension_key = TickArrayBitmapExtension::key(pool_state.key());
        for account_info in ctx.remaining_accounts.into_iter() {
            if account_info.key().eq(&tick_array_bitmap_extension_key) {
                tickarray_bitmap_extension = Some(
                    *(AccountLoader::<TickArrayBitmapExtension>::try_from(account_info)?
                        .load()?
                        .deref()),
                );
                continue;
            }
            tick_array_states.push_back(AccountLoad::load_data_mut(account_info)?);
        }

        (amount_0, amount_1) = swap_internal(
            &ctx.accounts.amm_config,
            pool_state,
            tick_array_states,
            &mut ctx.accounts.observation_state.load_mut()?,
            &tickarray_bitmap_extension,
            amount_calculate_specified,
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

        #[cfg(feature = "enable-log")]
        msg!(
            "exact_swap_internal, is_base_input:{}, amount_0: {}, amount_1: {}",
            is_base_input,
            amount_0,
            amount_1
        );
        require!(
            amount_0 != 0 && amount_1 != 0,
            ErrorCode::TooSmallInputOrOutputAmount
        );
    }
    let (token_account_0, token_account_1, vault_0, vault_1, vault_0_mint, vault_1_mint) =
        if zero_for_one {
            (
                ctx.accounts.input_token_account.clone(),
                ctx.accounts.output_token_account.clone(),
                ctx.accounts.input_vault.clone(),
                ctx.accounts.output_vault.clone(),
                ctx.accounts.input_vault_mint.clone(),
                ctx.accounts.output_vault_mint.clone(),
            )
        } else {
            (
                ctx.accounts.output_token_account.clone(),
                ctx.accounts.input_token_account.clone(),
                ctx.accounts.output_vault.clone(),
                ctx.accounts.input_vault.clone(),
                ctx.accounts.output_vault_mint.clone(),
                ctx.accounts.input_vault_mint.clone(),
            )
        };

    // user or pool real amount delta without tranfer fee
    let amount_0_without_fee;
    let amount_1_without_fee;
    // the transfer fee amount charged by withheld_amount
    let transfer_fee_0;
    let transfer_fee_1;
    // transfer amount
    let transfer_amount_0;
    let transfer_amount_1;
    if zero_for_one {
        transfer_fee_0 = util::get_transfer_inverse_fee(vault_0_mint.clone(), amount_0).unwrap();
        transfer_fee_1 = util::get_transfer_fee(vault_1_mint.clone(), amount_1).unwrap();

        amount_0_without_fee = amount_0;
        amount_1_without_fee = amount_1.checked_sub(transfer_fee_1).unwrap();
        (transfer_amount_0, transfer_amount_1) = (amount_0 + transfer_fee_0, amount_1);
        #[cfg(feature = "enable-log")]
        msg!(
            "amount_0:{}, transfer_fee_0:{}, amount_1:{}, transfer_fee_1:{}",
            amount_0,
            transfer_fee_0,
            amount_1,
            transfer_fee_1
        );
        //  x -> y, deposit x token from user to pool vault.
        transfer_from_user_to_pool_vault(
            &ctx.accounts.payer,
            &token_account_0.to_account_info(),
            &vault_0.to_account_info(),
            Some(vault_0_mint),
            &ctx.accounts.token_program,
            Some(ctx.accounts.token_program_2022.to_account_info()),
            transfer_amount_0,
        )?;
        if vault_1.amount <= transfer_amount_1 {
            // freeze pool, disable all instructions
            ctx.accounts.pool_state.load_mut()?.set_status(255);
        }
        // x -> y，transfer y token from pool vault to user.
        transfer_from_pool_vault_to_user(
            &ctx.accounts.pool_state,
            &vault_1.to_account_info(),
            &token_account_1.to_account_info(),
            Some(vault_1_mint),
            &ctx.accounts.token_program,
            Some(ctx.accounts.token_program_2022.to_account_info()),
            transfer_amount_1,
        )?;
    } else {
        transfer_fee_0 = util::get_transfer_fee(vault_0_mint.clone(), amount_0).unwrap();
        transfer_fee_1 = util::get_transfer_inverse_fee(vault_1_mint.clone(), amount_1).unwrap();

        amount_0_without_fee = amount_0.checked_sub(transfer_fee_0).unwrap();
        amount_1_without_fee = amount_1;
        (transfer_amount_0, transfer_amount_1) = (amount_0, amount_1 + transfer_fee_1);
        #[cfg(feature = "enable-log")]
        msg!(
            "amount_0:{}, transfer_fee_0:{}, amount_1:{}, transfer_fee_1:{}",
            amount_0,
            transfer_fee_0,
            amount_1,
            transfer_fee_1
        );
        transfer_from_user_to_pool_vault(
            &ctx.accounts.payer,
            &token_account_1.to_account_info(),
            &vault_1.to_account_info(),
            Some(vault_1_mint),
            &ctx.accounts.token_program,
            Some(ctx.accounts.token_program_2022.to_account_info()),
            transfer_amount_1,
        )?;
        if vault_0.amount <= transfer_amount_0 {
            // freeze pool, disable all instructions
            ctx.accounts.pool_state.load_mut()?.set_status(255);
        }
        transfer_from_pool_vault_to_user(
            &ctx.accounts.pool_state,
            &vault_0.to_account_info(),
            &token_account_0.to_account_info(),
            Some(vault_0_mint),
            &ctx.accounts.token_program,
            Some(ctx.accounts.token_program_2022.to_account_info()),
            transfer_amount_0,
        )?;
    }
    ctx.accounts.output_token_account.reload()?;
    ctx.accounts.input_token_account.reload()?;

    let pool_state = ctx.accounts.pool_state.load()?;
    emit_cpi!(SwapEvent {
        pool_state: pool_state.key(),
        sender: ctx.accounts.payer.key(),
        token_account_0: token_account_0.key(),
        token_account_1: token_account_1.key(),
        amount_0: amount_0_without_fee,
        transfer_fee_0,
        amount_1: amount_1_without_fee,
        transfer_fee_1,
        zero_for_one,
        sqrt_price_x64: pool_state.sqrt_price_x64,
        liquidity: pool_state.liquidity,
        tick: pool_state.tick_current
    });
    if zero_for_one {
        require_gt!(swap_price_before, pool_state.sqrt_price_x64);
    } else {
        require_gt!(pool_state.sqrt_price_x64, swap_price_before);
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
                require_eq!(amount_specified, transfer_amount_1);
            } else {
                require_eq!(amount_specified, transfer_amount_0);
            }
        }
    }

    if is_base_input {
        Ok(ctx
            .accounts
            .output_token_account
            .amount
            .checked_sub(output_balance_before)
            .unwrap())
    } else {
        Ok(input_balance_before
            .checked_sub(ctx.accounts.input_token_account.amount)
            .unwrap())
    }
}

pub fn swap_v2<'c: 'info, 'info>(
    ctx: Context<'_, '_, 'c, 'info, SwapSingleV2<'info>>,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<()> {
    let amount_result = exact_internal_v2(ctx, amount, sqrt_price_limit_x64, is_base_input)?;
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
