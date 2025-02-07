use crate::error::ErrorCode;
use crate::swap_v2::{exact_internal_v2, SwapSingleV2};
use crate::{states::*, SwapSingleV2Bumps};
use anchor_lang::prelude::*;
use anchor_spl::{
    token::Token,
    token_interface::{Mint, Token2022, TokenAccount},
};

#[event_cpi]
#[derive(Accounts)]
pub struct SwapRouterBaseIn<'info> {
    /// The user performing the swap
    pub payer: Signer<'info>,

    /// The token account that pays input tokens for the swap
    #[account(mut)]
    pub input_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The mint of input token
    #[account(mut)]
    pub input_token_mint: InterfaceAccount<'info, Mint>,

    /// SPL program for token transfers
    pub token_program: Program<'info, Token>,
    /// SPL program 2022 for token transfers
    pub token_program_2022: Program<'info, Token2022>,

    /// CHECK:
    // #[account(
    //     address = spl_memo::id()
    // )]
    pub memo_program: UncheckedAccount<'info>,
}

pub fn swap_router_base_in<'a, 'b, 'c: 'info, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, SwapRouterBaseIn<'info>>,
    amount_in: u64,
    amount_out_minimum: u64,
) -> Result<()> {
    let mut amount_in_internal = amount_in;
    let mut input_token_account = Box::new(ctx.accounts.input_token_account.clone());
    let mut input_token_mint = Box::new(ctx.accounts.input_token_mint.clone());
    let mut accounts: &[AccountInfo] = ctx.remaining_accounts;
    while !accounts.is_empty() {
        let mut remaining_accounts = accounts.iter();
        let account_info = remaining_accounts.next().unwrap();
        if accounts.len() != ctx.remaining_accounts.len()
            && account_info.data_len() != AmmConfig::LEN
        {
            accounts = remaining_accounts.as_slice();
            continue;
        }
        let amm_config = Box::new(Account::<AmmConfig>::try_from(account_info)?);
        let pool_state_loader =
            AccountLoader::<PoolState>::try_from(remaining_accounts.next().unwrap())?;
        let output_token_account = Box::new(InterfaceAccount::<TokenAccount>::try_from(
            &remaining_accounts.next().unwrap(),
        )?);
        let input_vault = Box::new(InterfaceAccount::<TokenAccount>::try_from(
            remaining_accounts.next().unwrap(),
        )?);
        let output_vault = Box::new(InterfaceAccount::<TokenAccount>::try_from(
            remaining_accounts.next().unwrap(),
        )?);
        let output_token_mint = Box::new(InterfaceAccount::<Mint>::try_from(
            remaining_accounts.next().unwrap(),
        )?);
        let observation_state =
            AccountLoader::<ObservationState>::try_from(remaining_accounts.next().unwrap())?;

        {
            let pool_state = pool_state_loader.load()?;
            // check observation account is owned by the pool
            require_keys_eq!(pool_state.observation_key, observation_state.key());
            // check ammConfig account is associate with the pool
            require_keys_eq!(pool_state.amm_config, amm_config.key());
        }

        // solana_program::log::sol_log_compute_units();
        accounts = remaining_accounts.as_slice();

        let swap_accounts = &mut SwapSingleV2 {
            payer: ctx.accounts.payer.clone(),
            amm_config,
            input_token_account: input_token_account.clone(),
            pool_state: pool_state_loader,
            output_token_account: output_token_account.clone(),
            input_vault: input_vault.clone(),
            output_vault: output_vault.clone(),
            input_vault_mint: input_token_mint.clone(),
            output_vault_mint: output_token_mint.clone(),
            observation_state,
            token_program: ctx.accounts.token_program.clone(),
            token_program_2022: ctx.accounts.token_program_2022.clone(),
            memo_program: ctx.accounts.memo_program.clone(),
            event_authority: ctx.accounts.event_authority.clone(),
            program: ctx.accounts.program.clone(),
        };

        let swap_context = Context::new(
            &ctx.program_id,
            swap_accounts,
            accounts,
            SwapSingleV2Bumps {
                event_authority: ctx.bumps.event_authority,
            },
        );

        amount_in_internal = exact_internal_v2(swap_context, amount_in_internal, 0, true)?;
        // output token is the new swap input token
        input_token_account = output_token_account;
        input_token_mint = output_token_mint;
    }
    require_gte!(
        amount_in_internal,
        amount_out_minimum,
        ErrorCode::TooLittleOutputReceived
    );

    Ok(())
}
