use crate::error::ErrorCode;
use crate::libraries::{
    big_num::U128, fixed_point_64, full_math::MulDiv, liquidity_math, swap_math, tick_math,
};
use crate::states::*;
use crate::util::*;
use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};
use std::cell::RefMut;
use std::collections::VecDeque;
#[cfg(feature = "enable-log")]
use std::convert::identity;
use std::ops::{Deref, Neg};

#[event_cpi]
#[derive(Accounts)]
pub struct SwapSingle<'info> {
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
    pub input_token_account: Box<Account<'info, TokenAccount>>,

    /// The user token account for output token
    #[account(mut)]
    pub output_token_account: Box<Account<'info, TokenAccount>>,

    /// The vault token account for input token
    #[account(mut)]
    pub input_vault: Box<Account<'info, TokenAccount>>,

    /// The vault token account for output token
    #[account(mut)]
    pub output_vault: Box<Account<'info, TokenAccount>>,

    /// The program account for the most recent oracle observation
    #[account(mut, address = pool_state.load()?.observation_key)]
    pub observation_state: AccountLoader<'info, ObservationState>,

    /// SPL program for token transfers
    pub token_program: Program<'info, Token>,

    #[account(mut, constraint = tick_array.load()?.pool_id == pool_state.key())]
    pub tick_array: AccountLoader<'info, TickArrayState>,
}

pub struct SwapAccounts<'b, 'info> {
    /// The user performing the swap
    pub signer: Signer<'info>,

    /// The user token account for input token
    pub input_token_account: Box<Account<'info, TokenAccount>>,

    /// The user token account for output token
    pub output_token_account: Box<Account<'info, TokenAccount>>,

    /// The vault token account for input token
    pub input_vault: Box<Account<'info, TokenAccount>>,

    /// The vault token account for output token
    pub output_vault: Box<Account<'info, TokenAccount>>,

    /// SPL program for token transfers
    pub token_program: Program<'info, Token>,

    /// The factory state to read protocol fees
    pub amm_config: &'b Box<Account<'info, AmmConfig>>,

    /// The program account of the pool in which the swap will be performed
    pub pool_state: &'b mut AccountLoader<'info, PoolState>,

    /// The tick_array account of current or next initialized
    pub tick_array_state: &'b mut AccountLoader<'info, TickArrayState>,

    /// The program account for the oracle observation
    pub observation_state: &'b mut AccountLoader<'info, ObservationState>,
}

// the top level state of the swap, the results of which are recorded in storage at the end
#[derive(Debug)]
pub struct SwapState {
    // the amount remaining to be swapped in/out of the input/output asset
    pub amount_specified_remaining: u64,
    // the amount already swapped out/in of the output/input asset
    pub amount_calculated: u64,
    // current sqrt(price)
    pub sqrt_price_x64: u128,
    // the tick associated with the current price
    pub tick: i32,
    // the global fee growth of the input token
    pub fee_growth_global_x64: u128,
    // the global fee of the input token
    pub fee_amount: u64,
    // amount of input token paid as protocol fee
    pub protocol_fee: u64,
    // amount of input token paid as fund fee
    pub fund_fee: u64,
    // the current liquidity in range
    pub liquidity: u128,
}

#[derive(Default)]
struct StepComputations {
    // the price at the beginning of the step
    sqrt_price_start_x64: u128,
    // the next tick to swap to from the current tick in the swap direction
    tick_next: i32,
    // whether tick_next is initialized or not
    initialized: bool,
    // sqrt(price) for the next tick (1/0)
    sqrt_price_next_x64: u128,
    // how much is being swapped in in this step
    amount_in: u64,
    // how much is being swapped out
    amount_out: u64,
    // how much fee is being paid in
    fee_amount: u64,
}

pub fn swap_internal<'b, 'info>(
    amm_config: &AmmConfig,
    pool_state: &mut RefMut<PoolState>,
    tick_array_states: &mut VecDeque<RefMut<TickArrayState>>,
    observation_state: &mut RefMut<ObservationState>,
    tickarray_bitmap_extension: &Option<TickArrayBitmapExtension>,
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    zero_for_one: bool,
    is_base_input: bool,
    block_timestamp: u32,
) -> Result<(u64, u64)> {
    require!(amount_specified != 0, ErrorCode::ZeroAmountSpecified);
    if !pool_state.get_status_by_bit(PoolStatusBitIndex::Swap) {
        return err!(ErrorCode::NotApproved);
    }
    require!(
        if zero_for_one {
            sqrt_price_limit_x64 < pool_state.sqrt_price_x64
                && sqrt_price_limit_x64 > tick_math::MIN_SQRT_PRICE_X64
        } else {
            sqrt_price_limit_x64 > pool_state.sqrt_price_x64
                && sqrt_price_limit_x64 < tick_math::MAX_SQRT_PRICE_X64
        },
        ErrorCode::SqrtPriceLimitOverflow
    );

    let liquidity_start = pool_state.liquidity;

    let updated_reward_infos = pool_state.update_reward_infos(block_timestamp as u64)?;

    let mut state = SwapState {
        amount_specified_remaining: amount_specified,
        amount_calculated: 0,
        sqrt_price_x64: pool_state.sqrt_price_x64,
        tick: pool_state.tick_current,
        fee_growth_global_x64: if zero_for_one {
            pool_state.fee_growth_global_0_x64
        } else {
            pool_state.fee_growth_global_1_x64
        },
        fee_amount: 0,
        protocol_fee: 0,
        fund_fee: 0,
        liquidity: liquidity_start,
    };

    // check observation account is owned by the pool
    require_keys_eq!(observation_state.pool_id, pool_state.key());

    let (mut is_match_pool_current_tick_array, first_vaild_tick_array_start_index) =
        pool_state.get_first_initialized_tick_array(&tickarray_bitmap_extension, zero_for_one)?;
    let mut current_vaild_tick_array_start_index = first_vaild_tick_array_start_index;

    let mut tick_array_current = tick_array_states.pop_front().unwrap();
    // find the first active tick array account
    for _ in 0..tick_array_states.len() {
        if tick_array_current.start_tick_index == current_vaild_tick_array_start_index {
            break;
        }
        tick_array_current = tick_array_states
            .pop_front()
            .ok_or(ErrorCode::NotEnoughTickArrayAccount)?;
    }
    // check the first tick_array account is owned by the pool
    require_keys_eq!(tick_array_current.pool_id, pool_state.key());
    // check first tick array account is correct
    require_eq!(
        tick_array_current.start_tick_index,
        current_vaild_tick_array_start_index,
        ErrorCode::InvalidFirstTickArrayAccount
    );

    // continue swapping as long as we haven't used the entire input/output and haven't
    // reached the price limit
    while state.amount_specified_remaining != 0 && state.sqrt_price_x64 != sqrt_price_limit_x64 {
        #[cfg(feature = "enable-log")]
        msg!(
            "while begin, is_base_input:{},fee_growth_global_x32:{}, state_sqrt_price_x64:{}, state_tick:{},state_liquidity:{},state.protocol_fee:{}, protocol_fee_rate:{}",
            is_base_input,
            state.fee_growth_global_x64,
            state.sqrt_price_x64,
            state.tick,
            state.liquidity,
            state.protocol_fee,
            amm_config.protocol_fee_rate
        );
        // Save these three pieces of information for PriceChangeEvent
        // let tick_before = state.tick;
        // let sqrt_price_x64_before = state.sqrt_price_x64;
        // let liquidity_before = state.liquidity;

        let mut step = StepComputations::default();
        step.sqrt_price_start_x64 = state.sqrt_price_x64;

        let mut next_initialized_tick = if let Some(tick_state) = tick_array_current
            .next_initialized_tick(state.tick, pool_state.tick_spacing, zero_for_one)?
        {
            Box::new(*tick_state)
        } else {
            if !is_match_pool_current_tick_array {
                is_match_pool_current_tick_array = true;
                Box::new(*tick_array_current.first_initialized_tick(zero_for_one)?)
            } else {
                Box::new(TickState::default())
            }
        };
        #[cfg(feature = "enable-log")]
        msg!(
            "next_initialized_tick, status:{}, tick_index:{}, tick_array_current:{}",
            next_initialized_tick.is_initialized(),
            identity(next_initialized_tick.tick),
            tick_array_current.key().to_string(),
        );
        if !next_initialized_tick.is_initialized() {
            let next_initialized_tickarray_index = pool_state
                .next_initialized_tick_array_start_index(
                    &tickarray_bitmap_extension,
                    current_vaild_tick_array_start_index,
                    zero_for_one,
                )?;
            if next_initialized_tickarray_index.is_none() {
                return err!(ErrorCode::LiquidityInsufficient);
            }

            while tick_array_current.start_tick_index != next_initialized_tickarray_index.unwrap() {
                tick_array_current = tick_array_states
                    .pop_front()
                    .ok_or(ErrorCode::NotEnoughTickArrayAccount)?;
                // check the tick_array account is owned by the pool
                require_keys_eq!(tick_array_current.pool_id, pool_state.key());
            }
            current_vaild_tick_array_start_index = next_initialized_tickarray_index.unwrap();

            let first_initialized_tick = tick_array_current.first_initialized_tick(zero_for_one)?;
            next_initialized_tick = Box::new(*first_initialized_tick);
        }
        step.tick_next = next_initialized_tick.tick;
        step.initialized = next_initialized_tick.is_initialized();

        if step.tick_next < tick_math::MIN_TICK {
            step.tick_next = tick_math::MIN_TICK;
        } else if step.tick_next > tick_math::MAX_TICK {
            step.tick_next = tick_math::MAX_TICK;
        }
        step.sqrt_price_next_x64 = tick_math::get_sqrt_price_at_tick(step.tick_next)?;

        let target_price = if (zero_for_one && step.sqrt_price_next_x64 < sqrt_price_limit_x64)
            || (!zero_for_one && step.sqrt_price_next_x64 > sqrt_price_limit_x64)
        {
            sqrt_price_limit_x64
        } else {
            step.sqrt_price_next_x64
        };

        if zero_for_one {
            require_gte!(state.tick, step.tick_next);
            require_gte!(step.sqrt_price_start_x64, step.sqrt_price_next_x64);
            require_gte!(step.sqrt_price_start_x64, target_price);
        } else {
            require_gt!(step.tick_next, state.tick);
            require_gte!(step.sqrt_price_next_x64, step.sqrt_price_start_x64);
            require_gte!(target_price, step.sqrt_price_start_x64);
        }
        #[cfg(feature = "enable-log")]
        msg!(
            "sqrt_price_current_x64:{}, sqrt_price_target:{}, liquidity:{}, amount_remaining:{}",
            step.sqrt_price_start_x64,
            target_price,
            state.liquidity,
            state.amount_specified_remaining
        );
        let swap_step = swap_math::compute_swap_step(
            step.sqrt_price_start_x64,
            target_price,
            state.liquidity,
            state.amount_specified_remaining,
            amm_config.trade_fee_rate,
            is_base_input,
            zero_for_one,
            block_timestamp,
        )?;
        #[cfg(feature = "enable-log")]
        msg!("{:#?}", swap_step);
        if zero_for_one {
            require_gte!(swap_step.sqrt_price_next_x64, target_price);
        } else {
            require_gte!(target_price, swap_step.sqrt_price_next_x64);
        }
        state.sqrt_price_x64 = swap_step.sqrt_price_next_x64;
        step.amount_in = swap_step.amount_in;
        step.amount_out = swap_step.amount_out;
        step.fee_amount = swap_step.fee_amount;

        if is_base_input {
            state.amount_specified_remaining = state
                .amount_specified_remaining
                .checked_sub(step.amount_in + step.fee_amount)
                .unwrap();
            state.amount_calculated = state
                .amount_calculated
                .checked_add(step.amount_out)
                .unwrap();
        } else {
            state.amount_specified_remaining = state
                .amount_specified_remaining
                .checked_sub(step.amount_out)
                .unwrap();

            let step_amount_calculate = step
                .amount_in
                .checked_add(step.fee_amount)
                .ok_or(ErrorCode::CalculateOverflow)?;
            state.amount_calculated = state
                .amount_calculated
                .checked_add(step_amount_calculate)
                .ok_or(ErrorCode::CalculateOverflow)?;
        }

        let step_fee_amount = step.fee_amount;
        // if the protocol fee is on, calculate how much is owed, decrement fee_amount, and increment protocol_fee
        if amm_config.protocol_fee_rate > 0 {
            let delta = U128::from(step_fee_amount)
                .checked_mul(amm_config.protocol_fee_rate.into())
                .unwrap()
                .checked_div(FEE_RATE_DENOMINATOR_VALUE.into())
                .unwrap()
                .as_u64();
            step.fee_amount = step.fee_amount.checked_sub(delta).unwrap();
            state.protocol_fee = state.protocol_fee.checked_add(delta).unwrap();
        }
        // if the fund fee is on, calculate how much is owed, decrement fee_amount, and increment fund_fee
        if amm_config.fund_fee_rate > 0 {
            let delta = U128::from(step_fee_amount)
                .checked_mul(amm_config.fund_fee_rate.into())
                .unwrap()
                .checked_div(FEE_RATE_DENOMINATOR_VALUE.into())
                .unwrap()
                .as_u64();
            step.fee_amount = step.fee_amount.checked_sub(delta).unwrap();
            state.fund_fee = state.fund_fee.checked_add(delta).unwrap();
        }

        // update global fee tracker
        if state.liquidity > 0 {
            let fee_growth_global_x64_delta = U128::from(step.fee_amount)
                .mul_div_floor(U128::from(fixed_point_64::Q64), U128::from(state.liquidity))
                .unwrap()
                .as_u128();

            state.fee_growth_global_x64 = state
                .fee_growth_global_x64
                .checked_add(fee_growth_global_x64_delta)
                .unwrap();
            state.fee_amount = state.fee_amount.checked_add(step.fee_amount).unwrap();
            #[cfg(feature = "enable-log")]
            msg!(
                "fee_growth_global_x64_delta:{}, state.fee_growth_global_x64:{}, state.liquidity:{}, step.fee_amount:{}, state.fee_amount:{}",
                fee_growth_global_x64_delta,
                state.fee_growth_global_x64, state.liquidity, step.fee_amount, state.fee_amount
            );
        }
        // shift tick if we reached the next price
        if state.sqrt_price_x64 == step.sqrt_price_next_x64 {
            // if the tick is initialized, run the tick transition
            if step.initialized {
                #[cfg(feature = "enable-log")]
                msg!("loading next tick {}", step.tick_next);

                let mut liquidity_net = next_initialized_tick.cross(
                    if zero_for_one {
                        state.fee_growth_global_x64
                    } else {
                        pool_state.fee_growth_global_0_x64
                    },
                    if zero_for_one {
                        pool_state.fee_growth_global_1_x64
                    } else {
                        state.fee_growth_global_x64
                    },
                    &updated_reward_infos,
                );
                // update tick_state to tick_array account
                tick_array_current.update_tick_state(
                    next_initialized_tick.tick,
                    pool_state.tick_spacing.into(),
                    *next_initialized_tick,
                )?;

                if zero_for_one {
                    liquidity_net = liquidity_net.neg();
                }
                state.liquidity = liquidity_math::add_delta(state.liquidity, liquidity_net)?;
            }

            state.tick = if zero_for_one {
                step.tick_next - 1
            } else {
                step.tick_next
            };
        } else if state.sqrt_price_x64 != step.sqrt_price_start_x64 {
            // recompute unless we're on a lower tick boundary (i.e. already transitioned ticks), and haven't moved
            // if only a small amount of quantity is traded, the input may be consumed by fees, resulting in no price change. If state.sqrt_price_x64, i.e., the latest price in the pool, is used to recalculate the tick, some errors may occur.
            // for example, if zero_for_one, and the price falls exactly on an initialized tick t after the first trade, then at this point, pool.sqrtPriceX64 = get_sqrt_price_at_tick(t), while pool.tick = t-1. if the input quantity of the
            // second trade is very small and the pool price does not change after the transaction, if the tick is recalculated, pool.tick will be equal to t, which is incorrect.
            state.tick = tick_math::get_tick_at_sqrt_price(state.sqrt_price_x64)?;
        }

        #[cfg(feature = "enable-log")]
        msg!(
            "end, is_base_input:{},step_amount_in:{}, step_amount_out:{}, step_fee_amount:{},fee_growth_global_x32:{}, state_sqrt_price_x64:{}, state_tick:{}, state_liquidity:{},state.protocol_fee:{}, protocol_fee_rate:{}, state.fund_fee:{}, fund_fee_rate:{}",
            is_base_input,
            step.amount_in,
            step.amount_out,
            step.fee_amount,
            state.fee_growth_global_x64,
            state.sqrt_price_x64,
            state.tick,
            state.liquidity,
            state.protocol_fee,
            amm_config.protocol_fee_rate,
            state.fund_fee,
            amm_config.fund_fee_rate,
        );
        // emit!(PriceChangeEvent {
        //     pool_state: pool_state.key(),
        //     tick_before,
        //     tick_after: state.tick,
        //     sqrt_price_x64_before,
        //     sqrt_price_x64_after: state.sqrt_price_x64,
        //     liquidity_before,
        //     liquidity_after: state.liquidity,
        //     zero_for_one,
        // });
    }
    // update tick
    if state.tick != pool_state.tick_current {
        // update the previous tick to the observation
        observation_state.update(block_timestamp, pool_state.tick_current);
        pool_state.tick_current = state.tick;
    }
    pool_state.sqrt_price_x64 = state.sqrt_price_x64;

    if liquidity_start != state.liquidity {
        pool_state.liquidity = state.liquidity;
    }

    let (amount_0, amount_1) = if zero_for_one == is_base_input {
        (
            amount_specified
                .checked_sub(state.amount_specified_remaining)
                .unwrap(),
            state.amount_calculated,
        )
    } else {
        (
            state.amount_calculated,
            amount_specified
                .checked_sub(state.amount_specified_remaining)
                .unwrap(),
        )
    };

    if zero_for_one {
        pool_state.fee_growth_global_0_x64 = state.fee_growth_global_x64;
        pool_state.total_fees_token_0 = pool_state
            .total_fees_token_0
            .checked_add(state.fee_amount)
            .unwrap();

        if state.protocol_fee > 0 {
            pool_state.protocol_fees_token_0 = pool_state
                .protocol_fees_token_0
                .checked_add(state.protocol_fee)
                .unwrap();
        }
        if state.fund_fee > 0 {
            pool_state.fund_fees_token_0 = pool_state
                .fund_fees_token_0
                .checked_add(state.fund_fee)
                .unwrap();
        }
        pool_state.swap_in_amount_token_0 = pool_state
            .swap_in_amount_token_0
            .checked_add(u128::from(amount_0))
            .unwrap();
        pool_state.swap_out_amount_token_1 = pool_state
            .swap_out_amount_token_1
            .checked_add(u128::from(amount_1))
            .unwrap();
    } else {
        pool_state.fee_growth_global_1_x64 = state.fee_growth_global_x64;
        pool_state.total_fees_token_1 = pool_state
            .total_fees_token_1
            .checked_add(state.fee_amount)
            .unwrap();

        if state.protocol_fee > 0 {
            pool_state.protocol_fees_token_1 = pool_state
                .protocol_fees_token_1
                .checked_add(state.protocol_fee)
                .unwrap();
        }
        if state.fund_fee > 0 {
            pool_state.fund_fees_token_1 = pool_state
                .fund_fees_token_1
                .checked_add(state.fund_fee)
                .unwrap();
        }
        pool_state.swap_in_amount_token_1 = pool_state
            .swap_in_amount_token_1
            .checked_add(u128::from(amount_1))
            .unwrap();
        pool_state.swap_out_amount_token_0 = pool_state
            .swap_out_amount_token_0
            .checked_add(u128::from(amount_0))
            .unwrap();
    }

    Ok((amount_0, amount_1))
}

/// Performs a single exact input/output swap
/// if is_base_input = true, return vaule is the max_amount_out, otherwise is min_amount_in
pub fn exact_internal<'b, 'c: 'info, 'info>(
    // ctx: &mut SwapAccounts<'b, 'info>,
    ctx: Context<'_, 'b, 'c, 'info, SwapSingle>,
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<u64> {
    let block_timestamp = solana_program::clock::Clock::get()?.unix_timestamp as u64;

    let amount_0;
    let amount_1;
    let zero_for_one;
    let swap_price_before;

    let input_balance_before = ctx.accounts.input_vault.amount;
    let output_balance_before = ctx.accounts.output_vault.amount;

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
        tick_array_states.push_back(ctx.accounts.tick_array.load_mut()?);

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
            amount_specified,
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
    let (token_account_0, token_account_1, vault_0, vault_1) = if zero_for_one {
        (
            ctx.accounts.input_token_account.clone(),
            ctx.accounts.output_token_account.clone(),
            ctx.accounts.input_vault.clone(),
            ctx.accounts.output_vault.clone(),
        )
    } else {
        (
            ctx.accounts.output_token_account.clone(),
            ctx.accounts.input_token_account.clone(),
            ctx.accounts.output_vault.clone(),
            ctx.accounts.input_vault.clone(),
        )
    };

    if zero_for_one {
        //  x -> y, deposit x token from user to pool vault.
        transfer_from_user_to_pool_vault(
            &ctx.accounts.payer,
            &token_account_0.to_account_info(),
            &vault_0.to_account_info(),
            None,
            &ctx.accounts.token_program,
            None,
            amount_0,
        )?;
        if vault_1.amount <= amount_1 {
            // freeze pool, disable all instructions
            ctx.accounts.pool_state.load_mut()?.set_status(255);
        }
        // x -> y，transfer y token from pool vault to user.
        transfer_from_pool_vault_to_user(
            &ctx.accounts.pool_state,
            &vault_1.to_account_info(),
            &token_account_1.to_account_info(),
            None,
            &ctx.accounts.token_program,
            None,
            amount_1,
        )?;
    } else {
        transfer_from_user_to_pool_vault(
            &ctx.accounts.payer,
            &token_account_1.to_account_info(),
            &vault_1.to_account_info(),
            None,
            &ctx.accounts.token_program,
            None,
            amount_1,
        )?;
        if vault_0.amount <= amount_0 {
            // freeze pool, disable all instructions
            ctx.accounts.pool_state.load_mut()?.set_status(255);
        }
        transfer_from_pool_vault_to_user(
            &ctx.accounts.pool_state,
            &vault_0.to_account_info(),
            &token_account_0.to_account_info(),
            None,
            &ctx.accounts.token_program,
            None,
            amount_0,
        )?;
    }
    ctx.accounts.output_vault.reload()?;
    ctx.accounts.input_vault.reload()?;

    let pool_state = ctx.accounts.pool_state.load()?;

    emit_cpi!(SwapEvent {
        pool_state: pool_state.key(),
        sender: ctx.accounts.payer.key(),
        token_account_0: token_account_0.key(),
        token_account_1: token_account_1.key(),
        amount_0,
        transfer_fee_0: 0,
        amount_1,
        transfer_fee_1: 0,
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
                require_eq!(amount_specified, amount_0);
            } else {
                require_eq!(amount_specified, amount_1);
            }
        } else {
            if zero_for_one {
                require_eq!(amount_specified, amount_1);
            } else {
                require_eq!(amount_specified, amount_0);
            }
        }
    }

    if is_base_input {
        Ok(output_balance_before
            .checked_sub(ctx.accounts.output_vault.amount)
            .unwrap())
    } else {
        Ok(ctx
            .accounts
            .input_vault
            .amount
            .checked_sub(input_balance_before)
            .unwrap())
    }
}

pub fn swap<'a, 'b, 'c: 'info, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, SwapSingle<'info>>,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<()> {
    let amount = exact_internal(ctx, amount, sqrt_price_limit_x64, is_base_input)?;
    if is_base_input {
        require!(
            amount >= other_amount_threshold,
            ErrorCode::TooLittleOutputReceived
        );
    } else {
        require!(
            amount <= other_amount_threshold,
            ErrorCode::TooMuchInputPaid
        );
    }

    Ok(())
}

#[cfg(test)]
mod swap_test {
    use liquidity_math::get_delta_amounts_signed;
    use tick_array_bitmap_extension_test::{
        build_tick_array_bitmap_extension_info, BuildExtensionAccountInfo,
    };

    use super::*;
    use crate::states::pool_test::build_pool;
    use crate::states::tick_array_test::{
        build_tick, build_tick_array_with_tick_states, TickArrayInfo,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::vec;

    pub fn get_tick_array_states_mut(
        deque_tick_array_states: &VecDeque<RefCell<TickArrayState>>,
    ) -> RefCell<VecDeque<RefMut<TickArrayState>>> {
        let mut tick_array_states = VecDeque::new();

        for tick_array_state in deque_tick_array_states {
            tick_array_states.push_back(tick_array_state.borrow_mut());
        }
        RefCell::new(tick_array_states)
    }

    fn build_swap_param<'info>(
        tick_current: i32,
        tick_spacing: u16,
        sqrt_price_x64: u128,
        liquidity: u128,
        tick_array_infos: Vec<TickArrayInfo>,
    ) -> (
        AmmConfig,
        RefCell<PoolState>,
        VecDeque<RefCell<TickArrayState>>,
        RefCell<ObservationState>,
    ) {
        let amm_config = AmmConfig {
            trade_fee_rate: 1000,
            tick_spacing,
            ..Default::default()
        };
        let pool_state = build_pool(tick_current, tick_spacing, sqrt_price_x64, liquidity);

        let observation_state = RefCell::new(ObservationState::default());
        observation_state.borrow_mut().pool_id = pool_state.borrow().key();

        let mut tick_array_states: VecDeque<RefCell<TickArrayState>> = VecDeque::new();
        for tick_array_info in tick_array_infos {
            tick_array_states.push_back(build_tick_array_with_tick_states(
                pool_state.borrow().key(),
                tick_array_info.start_tick_index,
                tick_spacing,
                tick_array_info.ticks,
            ));
            pool_state
                .borrow_mut()
                .flip_tick_array_bit(None, tick_array_info.start_tick_index)
                .unwrap();
        }

        (amm_config, pool_state, tick_array_states, observation_state)
    }

    pub struct OpenPositionParam {
        pub amount_0: u64,
        pub amount_1: u64,
        // pub liquidity: u128,
        pub tick_lower: i32,
        pub tick_upper: i32,
    }

    fn setup_swap_test<'info>(
        start_tick: i32,
        tick_spacing: u16,
        position_params: Vec<OpenPositionParam>,
        zero_for_one: bool,
    ) -> (
        AmmConfig,
        RefCell<PoolState>,
        VecDeque<RefCell<TickArrayState>>,
        RefCell<ObservationState>,
        TickArrayBitmapExtension,
        u64,
        u64,
    ) {
        let amm_config = AmmConfig {
            trade_fee_rate: 1000,
            tick_spacing,
            ..Default::default()
        };

        let pool_state_refcel = build_pool(
            start_tick,
            tick_spacing,
            tick_math::get_sqrt_price_at_tick(start_tick).unwrap(),
            0,
        );

        let observation_state = RefCell::new(ObservationState::default());

        let param = &mut BuildExtensionAccountInfo::default();
        param.key = Pubkey::find_program_address(
            &[
                POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(),
                pool_state_refcel.borrow().key().as_ref(),
            ],
            &crate::id(),
        )
        .0;
        let bitmap_extension = build_tick_array_bitmap_extension_info(param);
        let mut tick_array_states: VecDeque<RefCell<TickArrayState>> = VecDeque::new();
        let mut sum_amount_0: u64 = 0;
        let mut sum_amount_1: u64 = 0;
        {
            let mut pool_state = pool_state_refcel.borrow_mut();
            observation_state.borrow_mut().pool_id = pool_state.key();

            let mut tick_array_map = HashMap::new();

            for position_param in position_params {
                let liquidity = liquidity_math::get_liquidity_from_amounts(
                    pool_state.sqrt_price_x64,
                    tick_math::get_sqrt_price_at_tick(position_param.tick_lower).unwrap(),
                    tick_math::get_sqrt_price_at_tick(position_param.tick_upper).unwrap(),
                    position_param.amount_0,
                    position_param.amount_1,
                );

                let (amount_0, amount_1) = get_delta_amounts_signed(
                    start_tick,
                    tick_math::get_sqrt_price_at_tick(start_tick).unwrap(),
                    position_param.tick_lower,
                    position_param.tick_upper,
                    liquidity as i128,
                )
                .unwrap();
                sum_amount_0 += amount_0;
                sum_amount_1 += amount_1;
                let tick_array_lower_start_index =
                    TickArrayState::get_array_start_index(position_param.tick_lower, tick_spacing);

                if !tick_array_map.contains_key(&tick_array_lower_start_index) {
                    let mut tick_array_refcel = build_tick_array_with_tick_states(
                        pool_state.key(),
                        tick_array_lower_start_index,
                        tick_spacing,
                        vec![],
                    );
                    let tick_array_lower = tick_array_refcel.get_mut();

                    let tick_lower = tick_array_lower
                        .get_tick_state_mut(position_param.tick_lower, tick_spacing)
                        .unwrap();
                    tick_lower.tick = position_param.tick_lower;
                    tick_lower
                        .update(
                            pool_state.tick_current,
                            i128::try_from(liquidity).unwrap(),
                            0,
                            0,
                            false,
                            &[RewardInfo::default(); 3],
                        )
                        .unwrap();

                    tick_array_map.insert(tick_array_lower_start_index, tick_array_refcel);
                } else {
                    let tick_array_lower = tick_array_map
                        .get_mut(&tick_array_lower_start_index)
                        .unwrap();
                    let mut tick_array_lower_borrow_mut = tick_array_lower.borrow_mut();
                    let tick_lower = tick_array_lower_borrow_mut
                        .get_tick_state_mut(position_param.tick_lower, tick_spacing)
                        .unwrap();

                    tick_lower
                        .update(
                            pool_state.tick_current,
                            i128::try_from(liquidity).unwrap(),
                            0,
                            0,
                            false,
                            &[RewardInfo::default(); 3],
                        )
                        .unwrap();
                }
                let tick_array_upper_start_index =
                    TickArrayState::get_array_start_index(position_param.tick_upper, tick_spacing);
                if !tick_array_map.contains_key(&tick_array_upper_start_index) {
                    let mut tick_array_refcel = build_tick_array_with_tick_states(
                        pool_state.key(),
                        tick_array_upper_start_index,
                        tick_spacing,
                        vec![],
                    );
                    let tick_array_upper = tick_array_refcel.get_mut();

                    let tick_upper = tick_array_upper
                        .get_tick_state_mut(position_param.tick_upper, tick_spacing)
                        .unwrap();
                    tick_upper.tick = position_param.tick_upper;

                    tick_upper
                        .update(
                            pool_state.tick_current,
                            i128::try_from(liquidity).unwrap(),
                            0,
                            0,
                            true,
                            &[RewardInfo::default(); 3],
                        )
                        .unwrap();

                    tick_array_map.insert(tick_array_upper_start_index, tick_array_refcel);
                } else {
                    let tick_array_upper = tick_array_map
                        .get_mut(&tick_array_upper_start_index)
                        .unwrap();

                    let mut tick_array_upperr_borrow_mut = tick_array_upper.borrow_mut();
                    let tick_upper = tick_array_upperr_borrow_mut
                        .get_tick_state_mut(position_param.tick_upper, tick_spacing)
                        .unwrap();

                    tick_upper
                        .update(
                            pool_state.tick_current,
                            i128::try_from(liquidity).unwrap(),
                            0,
                            0,
                            true,
                            &[RewardInfo::default(); 3],
                        )
                        .unwrap();
                }
                if pool_state.tick_current >= position_param.tick_lower
                    && pool_state.tick_current < position_param.tick_upper
                {
                    pool_state.liquidity = liquidity_math::add_delta(
                        pool_state.liquidity,
                        i128::try_from(liquidity).unwrap(),
                    )
                    .unwrap();
                }
            }
            for (tickarray_start_index, tick_array_info) in tick_array_map {
                tick_array_states.push_back(tick_array_info);
                pool_state
                    .flip_tick_array_bit(Some(&bitmap_extension), tickarray_start_index)
                    .unwrap();
            }

            use std::convert::identity;
            if zero_for_one {
                tick_array_states.make_contiguous().sort_by(|a, b| {
                    identity(b.borrow().start_tick_index)
                        .cmp(&identity(a.borrow().start_tick_index))
                });
            } else {
                tick_array_states.make_contiguous().sort_by(|a, b| {
                    identity(a.borrow().start_tick_index)
                        .cmp(&identity(b.borrow().start_tick_index))
                });
            }
        }
        let bitmap_extension_state =
            *AccountLoader::<TickArrayBitmapExtension>::try_from(&bitmap_extension)
                .unwrap()
                .load()
                .unwrap()
                .deref();

        (
            amm_config,
            pool_state_refcel,
            tick_array_states,
            observation_state,
            bitmap_extension_state,
            sum_amount_0,
            sum_amount_1,
        )
    }

    #[cfg(test)]
    mod cross_tick_array_test {
        use super::*;

        #[test]
        fn zero_for_one_base_input_test() {
            let mut tick_current = -32395;
            let mut liquidity = 5124165121219;
            let mut sqrt_price_x64 = 3651942632306380802;
            let (amm_config, pool_state, mut tick_array_states, observation_state) =
                build_swap_param(
                    tick_current,
                    60,
                    sqrt_price_x64,
                    liquidity,
                    vec![
                        TickArrayInfo {
                            start_tick_index: -32400,
                            ticks: vec![
                                build_tick(-32400, 277065331032, -277065331032).take(),
                                build_tick(-29220, 1330680689, -1330680689).take(),
                                build_tick(-28860, 6408486554, -6408486554).take(),
                            ],
                        },
                        TickArrayInfo {
                            start_tick_index: -36000,
                            ticks: vec![
                                build_tick(-32460, 1194569667438, 536061033698).take(),
                                build_tick(-32520, 790917615645, 790917615645).take(),
                                build_tick(-32580, 152146472301, 128451145459).take(),
                                build_tick(-32640, 2625605835354, -1492054447712).take(),
                            ],
                        },
                    ],
                );

            // just cross the tickarray boundary(-32400), hasn't reached the next tick array initialized tick
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                12188240002,
                3049500711113990606,
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32460
                    && pool_state.borrow().tick_current < -32400
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity + 277065331032));
            assert!(amount_0 == 12188240002);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // cross the tickarray boundary(-32400) in last step, now tickarray_current is the tickarray with start_index -36000,
            // so we pop the tickarray with start_index -32400
            // in this swap we will cross the tick(-32460), but not reach next tick (-32520)
            tick_array_states.pop_front();
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                121882400020,
                3049500711113990606,
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32520
                    && pool_state.borrow().tick_current < -32460
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 536061033698));
            assert!(amount_0 == 121882400020);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // swap in tickarray with start_index -36000, cross the tick -32520
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                60941200010,
                3049500711113990606,
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32580
                    && pool_state.borrow().tick_current < -32520
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 790917615645));
            assert!(amount_0 == 60941200010);
        }

        #[test]
        fn zero_for_one_base_output_test() {
            let mut tick_current = -32395;
            let mut liquidity = 5124165121219;
            let mut sqrt_price_x64 = 3651942632306380802;
            let (amm_config, pool_state, mut tick_array_states, observation_state) =
                build_swap_param(
                    tick_current,
                    60,
                    sqrt_price_x64,
                    liquidity,
                    vec![
                        TickArrayInfo {
                            start_tick_index: -32400,
                            ticks: vec![
                                build_tick(-32400, 277065331032, -277065331032).take(),
                                build_tick(-29220, 1330680689, -1330680689).take(),
                                build_tick(-28860, 6408486554, -6408486554).take(),
                            ],
                        },
                        TickArrayInfo {
                            start_tick_index: -36000,
                            ticks: vec![
                                build_tick(-32460, 1194569667438, 536061033698).take(),
                                build_tick(-32520, 790917615645, 790917615645).take(),
                                build_tick(-32580, 152146472301, 128451145459).take(),
                                build_tick(-32640, 2625605835354, -1492054447712).take(),
                            ],
                        },
                    ],
                );

            // just cross the tickarray boundary(-32400), hasn't reached the next tick array initialized tick
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                477470480,
                3049500711113990606,
                true,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32460
                    && pool_state.borrow().tick_current < -32400
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity + 277065331032));
            assert!(amount_1 == 477470480);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // cross the tickarray boundary(-32400) in last step, now tickarray_current is the tickarray with start_index -36000,
            // so we pop the tickarray with start_index -32400
            // in this swap we will cross the tick(-32460), but not reach next tick (-32520)
            tick_array_states.pop_front();
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                4751002622,
                3049500711113990606,
                true,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32520
                    && pool_state.borrow().tick_current < -32460
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 536061033698));
            assert!(amount_1 == 4751002622);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // swap in tickarray with start_index -36000
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                2358130642,
                3049500711113990606,
                true,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -32580
                    && pool_state.borrow().tick_current < -32520
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 790917615645));
            assert!(amount_1 == 2358130642);
        }

        #[test]
        fn one_for_zero_base_input_test() {
            let mut tick_current = -32470;
            let mut liquidity = 5124165121219;
            let mut sqrt_price_x64 = 3638127228312488926;
            let (amm_config, pool_state, mut tick_array_states, observation_state) =
                build_swap_param(
                    tick_current,
                    60,
                    sqrt_price_x64,
                    liquidity,
                    vec![
                        TickArrayInfo {
                            start_tick_index: -36000,
                            ticks: vec![
                                build_tick(-32460, 1194569667438, 536061033698).take(),
                                build_tick(-32520, 790917615645, 790917615645).take(),
                                build_tick(-32580, 152146472301, 128451145459).take(),
                                build_tick(-32640, 2625605835354, -1492054447712).take(),
                            ],
                        },
                        TickArrayInfo {
                            start_tick_index: -32400,
                            ticks: vec![
                                build_tick(-32400, 277065331032, -277065331032).take(),
                                build_tick(-29220, 1330680689, -1330680689).take(),
                                build_tick(-28860, 6408486554, -6408486554).take(),
                            ],
                        },
                    ],
                );

            // just cross the tickarray boundary(-32460), hasn't reached the next tick array initialized tick
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                887470480,
                5882283448660210779,
                false,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -32460
                    && pool_state.borrow().tick_current < -32400
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity + 536061033698));
            assert!(amount_1 == 887470480);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // cross the tickarray boundary(-32460) in last step, but not reached tick -32400, because -32400 is the next tickarray boundary,
            // so the tickarray_current still is the tick array with start_index -36000
            // in this swap we will cross the tick(-32400), but not reach next tick (-29220)
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                3087470480,
                5882283448660210779,
                false,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -32400
                    && pool_state.borrow().tick_current < -29220
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 277065331032));
            assert!(amount_1 == 3087470480);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // swap in tickarray with start_index -32400, cross the tick -29220
            tick_array_states.pop_front();
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                200941200010,
                5882283448660210779,
                false,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -29220
                    && pool_state.borrow().tick_current < -28860
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 1330680689));
            assert!(amount_1 == 200941200010);
        }

        #[test]
        fn one_for_zero_base_output_test() {
            let mut tick_current = -32470;
            let mut liquidity = 5124165121219;
            let mut sqrt_price_x64 = 3638127228312488926;
            let (amm_config, pool_state, mut tick_array_states, observation_state) =
                build_swap_param(
                    tick_current,
                    60,
                    sqrt_price_x64,
                    liquidity,
                    vec![
                        TickArrayInfo {
                            start_tick_index: -36000,
                            ticks: vec![
                                build_tick(-32460, 1194569667438, 536061033698).take(),
                                build_tick(-32520, 790917615645, 790917615645).take(),
                                build_tick(-32580, 152146472301, 128451145459).take(),
                                build_tick(-32640, 2625605835354, -1492054447712).take(),
                            ],
                        },
                        TickArrayInfo {
                            start_tick_index: -32400,
                            ticks: vec![
                                build_tick(-32400, 277065331032, -277065331032).take(),
                                build_tick(-29220, 1330680689, -1330680689).take(),
                                build_tick(-28860, 6408486554, -6408486554).take(),
                            ],
                        },
                    ],
                );

            // just cross the tickarray boundary(-32460), hasn't reached the next tick array initialized tick
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                22796232052,
                5882283448660210779,
                false,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -32460
                    && pool_state.borrow().tick_current < -32400
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity + 536061033698));
            assert!(amount_0 == 22796232052);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // cross the tickarray boundary(-32460) in last step, but not reached tick -32400, because -32400 is the next tickarray boundary,
            // so the tickarray_current still is the tick array with start_index -36000
            // in this swap we will cross the tick(-32400), but not reach next tick (-29220)
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                79023558189,
                5882283448660210779,
                false,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -32400
                    && pool_state.borrow().tick_current < -29220
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 277065331032));
            assert!(amount_0 == 79023558189);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;
            liquidity = pool_state.borrow().liquidity;

            // swap in tickarray with start_index -32400, cross the tick -29220
            tick_array_states.pop_front();
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                4315086194758,
                5882283448660210779,
                false,
                false,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -29220
                    && pool_state.borrow().tick_current < -28860
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 1330680689));
            assert!(amount_0 == 4315086194758);
        }
    }

    #[cfg(test)]
    mod find_next_initialized_tick_test {
        use super::*;

        #[test]
        fn zero_for_one_current_tick_array_not_initialized_test() {
            let tick_current = -28776;
            let liquidity = 624165121219;
            let sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(tick_current).unwrap();
            let (amm_config, pool_state, tick_array_states, observation_state) = build_swap_param(
                tick_current,
                60,
                sqrt_price_x64,
                liquidity,
                vec![TickArrayInfo {
                    start_tick_index: -32400,
                    ticks: vec![
                        build_tick(-32400, 277065331032, -277065331032).take(),
                        build_tick(-29220, 1330680689, -1330680689).take(),
                        build_tick(-28860, 6408486554, -6408486554).take(),
                    ],
                }],
            );

            // find the first initialzied tick(-28860) and cross it in tickarray
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                12188240002,
                tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(
                pool_state.borrow().tick_current > -29220
                    && pool_state.borrow().tick_current < -28860
            );
            assert!(pool_state.borrow().sqrt_price_x64 < sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity + 6408486554));
            assert!(amount_0 == 12188240002);
        }

        #[test]
        fn one_for_zero_current_tick_array_not_initialized_test() {
            let tick_current = -32405;
            let liquidity = 1224165121219;
            let sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(tick_current).unwrap();
            let (amm_config, pool_state, tick_array_states, observation_state) = build_swap_param(
                tick_current,
                60,
                sqrt_price_x64,
                liquidity,
                vec![TickArrayInfo {
                    start_tick_index: -32400,
                    ticks: vec![
                        build_tick(-32400, 277065331032, -277065331032).take(),
                        build_tick(-29220, 1330680689, -1330680689).take(),
                        build_tick(-28860, 6408486554, -6408486554).take(),
                    ],
                }],
            );

            // find the first initialzied tick(-32400) and cross it in tickarray
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                12188240002,
                tick_math::get_sqrt_price_at_tick(-28860).unwrap(),
                false,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(
                pool_state.borrow().tick_current > -32400
                    && pool_state.borrow().tick_current < -29220
            );
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(pool_state.borrow().liquidity == (liquidity - 277065331032));
            assert!(amount_1 == 12188240002);
        }
    }

    #[cfg(test)]
    mod liquidity_insufficient_test {
        use super::*;
        use crate::error::ErrorCode;
        #[test]
        fn no_enough_initialized_tickarray_in_pool_test() {
            let tick_current = -28776;
            let liquidity = 121219;
            let sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(tick_current).unwrap();
            let (amm_config, pool_state, tick_array_states, observation_state) = build_swap_param(
                tick_current,
                60,
                sqrt_price_x64,
                liquidity,
                vec![TickArrayInfo {
                    start_tick_index: -32400,
                    ticks: vec![build_tick(-28860, 6408486554, -6408486554).take()],
                }],
            );

            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                12188240002,
                tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            );
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err(),
                ErrorCode::MissingTickArrayBitmapExtensionAccount.into()
            );
        }
    }

    #[test]
    fn explain_why_zero_for_one_less_or_equal_current_tick() {
        let tick_current = -28859;
        let mut liquidity = 121219;
        let sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(tick_current).unwrap();
        let (amm_config, pool_state, tick_array_states, observation_state) = build_swap_param(
            tick_current,
            60,
            sqrt_price_x64,
            liquidity,
            vec![TickArrayInfo {
                start_tick_index: -32400,
                ticks: vec![
                    build_tick(-32400, 277065331032, -277065331032).take(),
                    build_tick(-29220, 1330680689, -1330680689).take(),
                    build_tick(-28860, 6408486554, -6408486554).take(),
                ],
            }],
        );

        // not cross tick(-28860), but pool.tick_current = -28860
        let (amount_0, amount_1) = swap_internal(
            &amm_config,
            &mut pool_state.borrow_mut(),
            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
            &mut observation_state.borrow_mut(),
            &None,
            25,
            tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
            true,
            true,
            oracle::block_timestamp_mock() as u32,
        )
        .unwrap();
        println!("amount_0:{},amount_1:{}", amount_0, amount_1);
        assert!(pool_state.borrow().tick_current < tick_current);
        assert!(pool_state.borrow().tick_current == -28860);
        assert!(
            pool_state.borrow().sqrt_price_x64 > tick_math::get_sqrt_price_at_tick(-28860).unwrap()
        );
        assert!(pool_state.borrow().liquidity == liquidity);
        assert!(amount_0 == 25);

        // just cross tick(-28860), pool.tick_current = -28861
        let (amount_0, amount_1) = swap_internal(
            &amm_config,
            &mut pool_state.borrow_mut(),
            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
            &mut observation_state.borrow_mut(),
            &None,
            3,
            tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
            true,
            true,
            oracle::block_timestamp_mock() as u32,
        )
        .unwrap();
        println!("amount_0:{},amount_1:{}", amount_0, amount_1);
        assert!(pool_state.borrow().tick_current < tick_current);
        assert!(pool_state.borrow().tick_current == -28861);
        assert!(
            pool_state.borrow().sqrt_price_x64 > tick_math::get_sqrt_price_at_tick(-28861).unwrap()
        );
        assert!(pool_state.borrow().liquidity == liquidity + 6408486554);
        assert!(amount_0 == 3);

        liquidity = pool_state.borrow().liquidity;

        // we swap just a little amount, let pool tick_current also equal -28861
        // but pool.sqrt_price_x64 > tick_math::get_sqrt_price_at_tick(-28861)
        let (amount_0, amount_1) = swap_internal(
            &amm_config,
            &mut pool_state.borrow_mut(),
            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
            &mut observation_state.borrow_mut(),
            &None,
            50,
            tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
            true,
            true,
            oracle::block_timestamp_mock() as u32,
        )
        .unwrap();
        println!("amount_0:{},amount_1:{}", amount_0, amount_1);
        assert!(pool_state.borrow().tick_current == -28861);
        assert!(
            pool_state.borrow().sqrt_price_x64 > tick_math::get_sqrt_price_at_tick(-28861).unwrap()
        );
        assert!(pool_state.borrow().liquidity == liquidity);
        assert!(amount_0 == 50);
    }

    #[cfg(test)]
    mod swap_edge_test {
        use super::*;

        #[test]
        fn zero_for_one_swap_edge_case() {
            let mut tick_current = -28859;
            let liquidity = 121219;
            let mut sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(tick_current).unwrap();
            let (amm_config, pool_state, tick_array_states, observation_state) = build_swap_param(
                tick_current,
                60,
                sqrt_price_x64,
                liquidity,
                vec![
                    TickArrayInfo {
                        start_tick_index: -32400,
                        ticks: vec![
                            build_tick(-32400, 277065331032, -277065331032).take(),
                            build_tick(-29220, 1330680689, -1330680689).take(),
                            build_tick(-28860, 6408486554, -6408486554).take(),
                        ],
                    },
                    TickArrayInfo {
                        start_tick_index: -28800,
                        ticks: vec![build_tick(-28800, 3726362727, -3726362727).take()],
                    },
                ],
            );

            // zero for one, just cross tick(-28860),  pool.tick_current = -28861 and pool.sqrt_price_x64 = tick_math::get_sqrt_price_at_tick(-28860)
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                27,
                tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current < tick_current);
            assert!(pool_state.borrow().tick_current == -28861);
            assert!(
                pool_state.borrow().sqrt_price_x64
                    == tick_math::get_sqrt_price_at_tick(-28860).unwrap()
            );
            assert!(pool_state.borrow().liquidity == liquidity + 6408486554);
            assert!(amount_0 == 27);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;

            // we swap just a little amount, it is completely taken by fees, the sqrt price and the tick will remain the same
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                1,
                tick_math::get_sqrt_price_at_tick(-32400).unwrap(),
                true,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current == tick_current);
            assert!(pool_state.borrow().tick_current == -28861);
            assert!(pool_state.borrow().sqrt_price_x64 == sqrt_price_x64);

            tick_current = pool_state.borrow().tick_current;
            sqrt_price_x64 = pool_state.borrow().sqrt_price_x64;

            // reverse swap direction, one_for_zero
            // Actually, the loop for this swap was executed twice because the previous swap happened to have `pool.tick_current` exactly on the boundary that is divisible by `tick_spacing`.
            // In the first iteration of this swap's loop, it found the initial tick (-28860), but at this point, both the initial and final prices were equal to the price at tick -28860.
            // This did not meet the conditions for swapping so both swap_amount_input and swap_amount_output were 0. The actual output was calculated in the second iteration of the loop.
            let (amount_0, amount_1) = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &None,
                10,
                tick_math::get_sqrt_price_at_tick(-28800).unwrap(),
                false,
                true,
                oracle::block_timestamp_mock() as u32,
            )
            .unwrap();
            println!("amount_0:{},amount_1:{}", amount_0, amount_1);
            assert!(pool_state.borrow().tick_current > tick_current);
            assert!(pool_state.borrow().sqrt_price_x64 > sqrt_price_x64);
            assert!(
                pool_state.borrow().tick_current > -28860
                    && pool_state.borrow().tick_current <= -28800
            );
        }
    }

    #[cfg(test)]
    mod sqrt_price_limit_optimization_min_specified_test {
        use super::*;
        #[test]
        fn zero_for_one_base_input_with_min_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = true;
            let is_base_input = true;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX - 1;
            let amount_1 = u64::MAX - 1;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = 1;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MIN_SQRT_PRICE_X64 + 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }

        #[test]
        fn zero_for_one_base_out_with_min_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = true;
            let is_base_input = false;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX - 1;
            let amount_1 = u64::MAX - 1;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = 1;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MIN_SQRT_PRICE_X64 + 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }

        #[test]
        fn one_for_zero_base_in_with_min_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = false;
            let is_base_input = true;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX - 1;
            let amount_1 = u64::MAX - 1;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = 1;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MAX_SQRT_PRICE_X64 - 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }
        #[test]
        fn one_for_zero_base_out_with_min_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = false;
            let is_base_input = false;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX - 1;
            let amount_1 = u64::MAX - 1;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = 1;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MAX_SQRT_PRICE_X64 - 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }
    }
    #[cfg(test)]
    mod sqrt_price_limit_optimization_max_specified_test {
        use super::*;
        #[test]
        fn zero_for_one_base_input_with_max_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = true;
            let is_base_input = true;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX / 2;
            let amount_1 = u64::MAX / 2;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = u64::MAX / 2;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MIN_SQRT_PRICE_X64 + 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }

        #[test]
        fn zero_for_one_base_out_with_max_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = true;
            let is_base_input = false;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX / 2;
            let amount_1 = u64::MAX / 2;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = u64::MAX / 4;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MIN_SQRT_PRICE_X64 + 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }

        #[test]
        fn one_for_zero_base_in_with_max_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = false;
            let is_base_input = true;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX / 2;
            let amount_1 = u64::MAX / 2;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = u64::MAX / 2;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MAX_SQRT_PRICE_X64 - 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }
        #[test]
        fn one_for_zero_base_out_with_min_amount_specified() {
            let tick_spacing = 10;
            let zero_for_one = false;
            let is_base_input = false;
            let tick_lower = tick_math::MIN_TICK + 1;
            let tick_upper = tick_math::MAX_TICK - 1;
            let tick_current = 0;
            let amount_0 = u64::MAX / 2;
            let amount_1 = u64::MAX / 2;

            let (
                amm_config,
                pool_state,
                tick_array_states,
                observation_state,
                bitmap_extension_state,
                sum_amount_0,
                sum_amount_1,
            ) = setup_swap_test(
                tick_current,
                tick_spacing as u16,
                vec![OpenPositionParam {
                    amount_0: amount_0,
                    amount_1: amount_1,
                    tick_lower: tick_lower,
                    tick_upper: tick_upper,
                }],
                zero_for_one,
            );
            println!(
                "sum_amount_0: {}, sum_amount_1: {}",
                sum_amount_0, sum_amount_1,
            );
            let amount_specified = u64::MAX / 4;
            let result = swap_internal(
                &amm_config,
                &mut pool_state.borrow_mut(),
                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                &mut observation_state.borrow_mut(),
                &Some(bitmap_extension_state),
                amount_specified,
                tick_math::MAX_SQRT_PRICE_X64 - 1,
                zero_for_one,
                is_base_input,
                1,
            );
            println!("{:#?}", result);
            let pool = pool_state.borrow();
            let sqrt_price_x64 = pool.sqrt_price_x64;
            let sqrt_price = sqrt_price_x64 as f64 / fixed_point_64::Q64 as f64;
            println!("price: {}", sqrt_price * sqrt_price);
        }
    }
    #[cfg(test)]
    mod sqrt_price_limit_optimization_test {
        use super::*;
        use proptest::prelude::*;
        use std::{convert::identity, u64};

        use proptest::prop_assume;
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            #[test]
            fn zero_for_one_base_input_test(
                tick_current in tick_math::MIN_TICK..tick_math::MAX_TICK,
                amount_0 in 1000000..u64::MAX,
                amount_1 in 1000000..u64::MAX,
                tick_lower in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 10", |x| x % 10 == 0),
                tick_upper in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 10", |x| x % 10 == 0),
            ){
                let tick_spacing = 10;
                let zero_for_one = true;
                let is_base_input = true;
                if tick_lower%tick_spacing ==0 && tick_upper%tick_spacing ==0 && tick_upper>tick_lower{

                    let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  sum_amount_0, sum_amount_1) = setup_swap_test(
                        tick_current,
                        tick_spacing as u16,
                        vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                        zero_for_one
                        );

                    prop_assume!(sum_amount_1 > 1);
                    let mut rng = rand::thread_rng();
                    let amount_specified  = rng.gen_range(1..u64::MAX - sum_amount_0);

                    let result = swap_internal(
                        &amm_config,
                        &mut pool_state.borrow_mut(),
                        &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                        &mut observation_state.borrow_mut(),
                        &Some(bitmap_extension_state),
                        amount_specified,
                        tick_math::MIN_SQRT_PRICE_X64 + 1,
                        zero_for_one,
                        is_base_input,
                        0,
                    );

                    if result.is_ok() {
                        let ( amount_0_before, amount_1_before) = result.unwrap();

                        let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  _sum_amount_0, _sum_amount_1) = setup_swap_test(
                            tick_current,
                            tick_spacing as u16,
                            vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                            zero_for_one
                        );
                        let result = swap_internal(
                            &amm_config,
                            &mut pool_state.borrow_mut(),
                            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                            &mut observation_state.borrow_mut(),
                            &Some(bitmap_extension_state),
                            amount_specified,
                            tick_math::MIN_SQRT_PRICE_X64 + 1,
                            zero_for_one,
                            is_base_input,
                            oracle::block_timestamp_mock() as u32,
                        );
                        assert!(result.is_ok());

                        // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{},liquidity:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper, identity(pool_state.borrow().liquidity));

                        let ( amount_0_after, amount_1_after) = result.unwrap();
                        assert_eq!(amount_0_before, amount_0_after);
                        assert_eq!(amount_1_before, amount_1_after);

                    }else{
                        let err =  result.err().unwrap();
                        if err == crate::error::ErrorCode::MaxTokenOverflow.into(){
                            println!("##### original swap is overflow ");
                            let result = swap_internal(
                                &amm_config,
                                &mut pool_state.borrow_mut(),
                                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                                &mut observation_state.borrow_mut(),
                                &Some(bitmap_extension_state),
                                amount_specified,
                                tick_math::MIN_SQRT_PRICE_X64 + 1,
                                zero_for_one,
                                is_base_input,
                                oracle::block_timestamp_mock() as u32,
                            );
                            if result.is_err(){
                                println!("{:#?}", result);
                            }
                        }else{
                            println!("{}", err);
                        }
                    }
                }
            }

            #[test]
            fn zero_for_one_base_output_test(
                tick_current in tick_math::MIN_TICK..tick_math::MAX_TICK,
                amount_0 in 1000000..u64::MAX,
                amount_1 in 1000000..u64::MAX,
                tick_lower in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
                tick_upper in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
            ){
                let tick_spacing = 10;
                let zero_for_one = true;
                let base_input= false;
                if tick_lower%tick_spacing ==0 && tick_upper%tick_spacing ==0 && tick_upper>tick_lower{
                    let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state, _sum_amount_0, sum_amount_1) = setup_swap_test(
                        tick_current,
                        tick_spacing as u16,
                        vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                        zero_for_one
                    );

                    prop_assume!(sum_amount_1 > 1);
                    let mut rng = rand::thread_rng();
                    let amount_specified  = rng.gen_range(1..sum_amount_1);
                    // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper);
                    let result = swap_internal(
                        &amm_config,
                        &mut pool_state.borrow_mut(),
                        &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                        &mut observation_state.borrow_mut(),
                        &Some(bitmap_extension_state),
                        amount_specified,
                        tick_math::MIN_SQRT_PRICE_X64 + 1,
                        zero_for_one,
                        base_input,
                        0,
                    );

                    if result.is_ok() {
                        let ( amount_0_before, amount_1_before) = result.unwrap();

                        let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state, _sum_amount_0, _sum_amount_1) = setup_swap_test(
                            tick_current,
                            tick_spacing as u16,
                            vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                            zero_for_one
                        );
                        let result = swap_internal(
                            &amm_config,
                            &mut pool_state.borrow_mut(),
                            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                            &mut observation_state.borrow_mut(),
                            &Some(bitmap_extension_state),
                            amount_specified,
                            tick_math::MIN_SQRT_PRICE_X64 + 1,
                            zero_for_one,
                            base_input,
                            oracle::block_timestamp_mock() as u32,
                        );
                        assert!(result.is_ok());

                        println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{},liquidity:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper, identity(pool_state.borrow().liquidity));

                        let ( amount_0_after, amount_1_after) = result.unwrap();
                        assert_eq!(amount_0_before, amount_0_after);
                        assert_eq!(amount_1_before, amount_1_after);

                    }else{
                        let err =  result.err().unwrap();
                        if err == crate::error::ErrorCode::MaxTokenOverflow.into(){
                            println!("##### original swap is overflow");
                            let result = swap_internal(
                                &amm_config,
                                &mut pool_state.borrow_mut(),
                                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                                &mut observation_state.borrow_mut(),
                                &Some(bitmap_extension_state),
                                amount_specified,
                                tick_math::MIN_SQRT_PRICE_X64 + 1,
                                zero_for_one,
                                base_input,
                                oracle::block_timestamp_mock() as u32,
                            );
                            if result.is_err(){
                                println!("{:#?}", result);
                            }
                        }else{
                            println!("{}", err);
                        }
                    }
                }
            }

            #[test]
            fn one_for_zero_base_input_test(
                tick_current in tick_math::MIN_TICK..tick_math::MAX_TICK,
                amount_0 in 1000000..u64::MAX,
                amount_1 in 1000000..u64::MAX,
                tick_lower in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
                tick_upper in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
            ){
                let tick_spacing = 10;
                let zero_for_one = false;
                let is_base_input = true;
                if tick_lower%tick_spacing ==0 && tick_upper%tick_spacing ==0 && tick_current>tick_lower && tick_current<tick_upper{
                    // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper);
                    let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  sum_amount_0, sum_amount_1) = setup_swap_test(
                        tick_current,
                        tick_spacing as u16,
                        vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                        zero_for_one
                    );

                    prop_assume!(sum_amount_0 > 1);
                    let mut rng = rand::thread_rng();
                    let amount_specified  = rng.gen_range(1..u64::MAX - sum_amount_1);

                    let result = swap_internal(
                        &amm_config,
                        &mut pool_state.borrow_mut(),
                        &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                        &mut observation_state.borrow_mut(),
                        &Some(bitmap_extension_state),
                        amount_specified,
                        tick_math::MAX_SQRT_PRICE_X64 - 1,
                        zero_for_one,
                        is_base_input,
                        0,
                    );


                    if result.is_ok() {
                        let ( amount_0_before, amount_1_before) = result.unwrap();

                        let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  _sum_amount_0, _sum_amount_1) = setup_swap_test(
                            tick_current,
                            tick_spacing as u16,
                            vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                            zero_for_one
                        );
                        let result = swap_internal(
                            &amm_config,
                            &mut pool_state.borrow_mut(),
                            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                            &mut observation_state.borrow_mut(),
                            &Some(bitmap_extension_state),
                            amount_specified,
                            tick_math::MAX_SQRT_PRICE_X64 - 1,
                            zero_for_one,
                            is_base_input,
                            oracle::block_timestamp_mock() as u32,
                        );
                        assert!(result.is_ok());

                        // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{},liquidity:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper, identity(pool_state.borrow().liquidity));

                        let (amount_0_after, amount_1_after) = result.unwrap();
                        assert_eq!(amount_0_before, amount_0_after);
                        assert_eq!(amount_1_before, amount_1_after);

                    }else {
                        let err =  result.err().unwrap();
                        if err == crate::error::ErrorCode::MaxTokenOverflow.into(){
                            // println!("##### original swap is overflow ");
                            let _result = swap_internal(
                                &amm_config,
                                &mut pool_state.borrow_mut(),
                                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                                &mut observation_state.borrow_mut(),
                                &Some(bitmap_extension_state),
                                amount_specified,
                                tick_math::MAX_SQRT_PRICE_X64 - 1,
                                zero_for_one,
                                is_base_input,
                                oracle::block_timestamp_mock() as u32,
                            );

                        }else{
                            println!("{}", err);
                        }
                    }
                }
            }

            #[test]
            fn one_for_zero_base_output_test(
                tick_current in tick_math::MIN_TICK..tick_math::MAX_TICK,
                amount_0 in 1000000..u64::MAX,
                amount_1 in 1000000..u64::MAX,
                tick_lower in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
                tick_upper in (tick_math::MIN_TICK..=tick_math::MAX_TICK).prop_filter("Must be multiple of 100", |x| x % 10 == 0),
            ){
                let tick_spacing = 10;
                let zero_for_one = false;
                let is_base_input = false;
                if tick_lower%tick_spacing ==0 && tick_upper%tick_spacing ==0 && tick_current>tick_lower && tick_current<tick_upper{

                    // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper);
                    let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  sum_amount_0, _sum_amount_1) = setup_swap_test(
                        tick_current,
                        tick_spacing as u16,
                        vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                        zero_for_one
                    );
                    prop_assume!(sum_amount_0 > 1);
                    let mut rng = rand::thread_rng();
                    let amount_specified  = rng.gen_range(1..sum_amount_0);

                    let result = swap_internal(
                        &amm_config,
                        &mut pool_state.borrow_mut(),
                        &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                        &mut observation_state.borrow_mut(),
                        &Some(bitmap_extension_state),
                        amount_specified,
                        tick_math::MAX_SQRT_PRICE_X64 - 1,
                        zero_for_one,
                        is_base_input,
                        0,
                    );

                    if result.is_ok() {
                        let ( amount_0_before, amount_1_before) = result.unwrap();

                        let (amm_config, pool_state, tick_array_states, observation_state,bitmap_extension_state,  _sum_amount_0, _sum_amount_1) = setup_swap_test(
                            tick_current,
                            tick_spacing as u16,
                            vec![OpenPositionParam{amount_0:amount_0,amount_1:amount_1, tick_lower:tick_lower, tick_upper:tick_upper}],
                            zero_for_one
                        );
                        let result = swap_internal(
                            &amm_config,
                            &mut pool_state.borrow_mut(),
                            &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                            &mut observation_state.borrow_mut(),
                            &Some(bitmap_extension_state),
                            amount_specified,
                            tick_math::MAX_SQRT_PRICE_X64 - 1,
                            zero_for_one,
                            is_base_input,
                            oracle::block_timestamp_mock() as u32,
                        );
                        assert!(result.is_ok());

                        // println!("----- input: tick_current:{}, amount_0:{}, amount_1:{}, amount_specified:{},tick_lower:{}, tick_upper:{},liquidity:{}", tick_current, amount_0, amount_1,amount_specified, tick_lower, tick_upper, identity(pool_state.borrow().liquidity));

                        let (amount_0_after, amount_1_after) = result.unwrap();
                        assert_eq!(amount_0_before, amount_0_after);
                        assert_eq!(amount_1_before, amount_1_after);

                    }else {
                        let err =  result.err().unwrap();
                        if err == crate::error::ErrorCode::MaxTokenOverflow.into(){
                            println!("##### original swap is overflow ");
                            let _result = swap_internal(
                                &amm_config,
                                &mut pool_state.borrow_mut(),
                                &mut get_tick_array_states_mut(&tick_array_states).borrow_mut(),
                                &mut observation_state.borrow_mut(),
                                &Some(bitmap_extension_state),
                                amount_specified,
                                tick_math::MAX_SQRT_PRICE_X64 - 1,
                                zero_for_one,
                                is_base_input,
                                oracle::block_timestamp_mock() as u32,
                            );
                        }else{
                            println!("{}", err);
                        }
                    }
                }
            }
        }
    }
}
