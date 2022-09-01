use super::{add_liquidity, AddLiquidityParam};
use crate::libraries::{big_num::U128, fixed_point_64, full_math::MulDiv};
use crate::states::*;
use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

#[derive(Accounts)]
pub struct IncreaseLiquidity<'info> {
    /// Pays to mint the position
    pub nft_owner: Signer<'info>,

    /// The token account for nft
    #[account(
        constraint = nft_account.mint == personal_position.nft_mint
    )]
    pub nft_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub pool_state: AccountLoader<'info, PoolState>,

    #[account(
        mut,
        seeds = [
            POSITION_SEED.as_bytes(),
            pool_state.key().as_ref(),
            &personal_position.tick_lower_index.to_be_bytes(),
            &personal_position.tick_upper_index.to_be_bytes(),
        ],
        bump,
    )]
    pub protocol_position: Box<Account<'info, ProtocolPositionState>>,

    /// Increase liquidity for this position
    #[account(mut, constraint = personal_position.pool_id == pool_state.key())]
    pub personal_position: Box<Account<'info, PersonalPositionState>>,

    /// Stores init state for the lower tick
    #[account(mut, constraint = tick_array_lower.load()?.amm_pool == pool_state.key())]
    pub tick_array_lower: AccountLoader<'info, TickArrayState>,

    /// Stores init state for the upper tick
    #[account(mut, constraint = tick_array_upper.load()?.amm_pool == pool_state.key())]
    pub tick_array_upper: AccountLoader<'info, TickArrayState>,

    /// The payer's token account for token_0
    #[account(
        mut,
        token::mint = token_vault_0.mint
    )]
    pub token_account_0: Box<Account<'info, TokenAccount>>,

    /// The token account spending token_1 to mint the position
    #[account(
        mut,
        token::mint = token_vault_1.mint
    )]
    pub token_account_1: Box<Account<'info, TokenAccount>>,

    /// The address that holds pool tokens for token_0
    #[account(
        mut,
        constraint = token_vault_0.key() == pool_state.load()?.token_vault_0
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    /// The address that holds pool tokens for token_1
    #[account(
        mut,
        constraint = token_vault_1.key() == pool_state.load()?.token_vault_1
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    /// Program to create mint account and mint tokens
    pub token_program: Program<'info, Token>,
}

pub fn increase_liquidity<'a, 'b, 'c, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, IncreaseLiquidity<'info>>,
    liquidity: u128,
    amount_0_max: u64,
    amount_1_max: u64,
) -> Result<()> {
    let mut pool_state = ctx.accounts.pool_state.load_mut()?;
    let tick_lower = ctx.accounts.personal_position.tick_lower_index;
    let tick_upper = ctx.accounts.personal_position.tick_upper_index;
    let mut add_liquidity_context = AddLiquidityParam {
        payer: &ctx.accounts.nft_owner,
        token_account_0: &mut ctx.accounts.token_account_0,
        token_account_1: &mut ctx.accounts.token_account_1,
        token_vault_0: &mut ctx.accounts.token_vault_0,
        token_vault_1: &mut ctx.accounts.token_vault_1,
        tick_array_lower: &ctx.accounts.tick_array_lower,
        tick_array_upper: &ctx.accounts.tick_array_upper,
        protocol_position: &mut ctx.accounts.protocol_position,
        token_program: ctx.accounts.token_program.clone(),
    };
    let (amount_0, amount_1) = add_liquidity(
        &mut add_liquidity_context,
        &mut pool_state,
        liquidity,
        amount_0_max,
        amount_1_max,
        tick_lower,
        tick_upper,
    )?;
    let updated_protocol_position = add_liquidity_context.protocol_position;

    let personal_position = &mut ctx.accounts.personal_position;
    personal_position.token_fees_owed_0 = calculate_latest_token_fees(
        personal_position.token_fees_owed_0,
        personal_position.fee_growth_inside_0_last_x64,
        updated_protocol_position.fee_growth_inside_0_last_x64,
        personal_position.liquidity,
    );
    personal_position.token_fees_owed_1 = calculate_latest_token_fees(
        personal_position.token_fees_owed_1,
        personal_position.fee_growth_inside_1_last_x64,
        updated_protocol_position.fee_growth_inside_1_last_x64,
        personal_position.liquidity,
    );

    personal_position.fee_growth_inside_0_last_x64 =
        updated_protocol_position.fee_growth_inside_0_last_x64;
    personal_position.fee_growth_inside_1_last_x64 =
        updated_protocol_position.fee_growth_inside_1_last_x64;

    // update rewards, must update before increase liquidity
    personal_position.update_rewards(updated_protocol_position.reward_growth_inside)?;
    personal_position.liquidity = personal_position.liquidity.checked_add(liquidity).unwrap();

    emit!(IncreaseLiquidityEvent {
        position_nft_mint: personal_position.nft_mint,
        liquidity,
        amount_0,
        amount_1
    });

    Ok(())
}

pub fn calculate_latest_token_fees(
    last_total_fees: u64,
    fee_growth_inside_last_x64: u128,
    fee_growth_inside_latest_x64: u128,
    liquidity: u128,
) -> u64 {
    let fee_growth_delta =
        U128::from(fee_growth_inside_latest_x64.saturating_sub(fee_growth_inside_last_x64))
            .mul_div_floor(U128::from(liquidity), U128::from(fixed_point_64::Q64))
            .unwrap()
            .as_u64();

    last_total_fees.checked_add(fee_growth_delta).unwrap()
}
