#![allow(dead_code)]
use anchor_client::{Client, Cluster};
use anchor_lang::prelude::AccountMeta;
use anyhow::{format_err, Result};
use arrayref::array_ref;
use clap::Parser;
use configparser::ini::Ini;
use rand::rngs::OsRng;
use solana_account_decoder::{
    parse_token::{TokenAccountType, UiAccountState},
    UiAccountData, UiAccountEncoding,
};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig},
    rpc_filter::{Memcmp, RpcFilterType},
    rpc_request::TokenAccountsFilter,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    message::Message,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::{collections::VecDeque, convert::identity, mem::size_of};

mod instructions;
use bincode::serialize;
use instructions::amm_instructions::*;
use instructions::events_instructions_parse::*;
use instructions::rpc::*;
use instructions::token_instructions::*;
use instructions::utils::*;
use raydium_amm_v3::{
    libraries::{fixed_point_64, liquidity_math, tick_math},
    states::{PoolState, TickArrayBitmapExtension, TickArrayState, POOL_TICK_ARRAY_BITMAP_SEED},
};
use spl_associated_token_account::get_associated_token_address;
use spl_token_2022::{
    extension::StateWithExtensions,
    state::Mint,
    state::{Account, AccountState},
};
use spl_token_client::token::ExtensionInitializationParams;

use crate::instructions::utils;
#[derive(Clone, Debug, PartialEq)]
pub struct ClientConfig {
    http_url: String,
    ws_url: String,
    payer_path: String,
    admin_path: String,
    raydium_v3_program: Pubkey,
    slippage: f64,
    amm_config_key: Pubkey,

    mint0: Option<Pubkey>,
    mint1: Option<Pubkey>,
    pool_id_account: Option<Pubkey>,
    tickarray_bitmap_extension: Option<Pubkey>,
    amm_config_index: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PoolAccounts {
    pool_id: Option<Pubkey>,
    pool_config: Option<Pubkey>,
    pool_observation: Option<Pubkey>,
    pool_protocol_positions: Vec<Pubkey>,
    pool_personal_positions: Vec<Pubkey>,
    pool_tick_arrays: Vec<Pubkey>,
}

fn load_cfg(client_config: &String) -> Result<ClientConfig> {
    let mut config = Ini::new();
    let _map = config.load(client_config).unwrap();
    let http_url = config.get("Global", "http_url").unwrap();
    if http_url.is_empty() {
        panic!("http_url must not be empty");
    }
    let ws_url = config.get("Global", "ws_url").unwrap();
    if ws_url.is_empty() {
        panic!("ws_url must not be empty");
    }
    let payer_path = config.get("Global", "payer_path").unwrap();
    if payer_path.is_empty() {
        panic!("payer_path must not be empty");
    }
    let admin_path = config.get("Global", "admin_path").unwrap();
    if admin_path.is_empty() {
        panic!("admin_path must not be empty");
    }

    let raydium_v3_program_str = config.get("Global", "raydium_v3_program").unwrap();
    if raydium_v3_program_str.is_empty() {
        panic!("raydium_v3_program must not be empty");
    }
    let raydium_v3_program = Pubkey::from_str(&raydium_v3_program_str).unwrap();
    let slippage = config.getfloat("Global", "slippage").unwrap().unwrap();

    let mut mint0 = None;
    let mint0_str = config.get("Pool", "mint0").unwrap();
    if !mint0_str.is_empty() {
        mint0 = Some(Pubkey::from_str(&mint0_str).unwrap());
    }
    let mut mint1 = None;
    let mint1_str = config.get("Pool", "mint1").unwrap();
    if !mint1_str.is_empty() {
        mint1 = Some(Pubkey::from_str(&mint1_str).unwrap());
    }
    let amm_config_index = config.getuint("Pool", "amm_config_index").unwrap().unwrap() as u16;

    let (amm_config_key, __bump) = Pubkey::find_program_address(
        &[
            raydium_amm_v3::states::AMM_CONFIG_SEED.as_bytes(),
            &amm_config_index.to_be_bytes(),
        ],
        &raydium_v3_program,
    );

    let pool_id_account = if mint0 != None && mint1 != None {
        if mint0.unwrap() > mint1.unwrap() {
            let temp_mint = mint0;
            mint0 = mint1;
            mint1 = temp_mint;
        }
        Some(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::POOL_SEED.as_bytes(),
                    amm_config_key.to_bytes().as_ref(),
                    mint0.unwrap().to_bytes().as_ref(),
                    mint1.unwrap().to_bytes().as_ref(),
                ],
                &raydium_v3_program,
            )
            .0,
        )
    } else {
        None
    };
    let tickarray_bitmap_extension = if pool_id_account != None {
        Some(
            Pubkey::find_program_address(
                &[
                    POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(),
                    pool_id_account.unwrap().to_bytes().as_ref(),
                ],
                &raydium_v3_program,
            )
            .0,
        )
    } else {
        None
    };

    Ok(ClientConfig {
        http_url,
        ws_url,
        payer_path,
        admin_path,
        raydium_v3_program,
        slippage,
        amm_config_key,
        mint0,
        mint1,
        pool_id_account,
        tickarray_bitmap_extension,
        amm_config_index,
    })
}
fn read_keypair_file(s: &str) -> Result<Keypair> {
    solana_sdk::signature::read_keypair_file(s)
        .map_err(|_| format_err!("failed to read keypair from {}", s))
}
fn write_keypair_file(keypair: &Keypair, outfile: &str) -> Result<String> {
    solana_sdk::signature::write_keypair_file(keypair, outfile)
        .map_err(|_| format_err!("failed to write keypair to {}", outfile))
}
fn path_is_exist(path: &str) -> bool {
    Path::new(path).exists()
}

fn load_cur_and_next_five_tick_array(
    rpc_client: &RpcClient,
    pool_config: &ClientConfig,
    pool_state: &PoolState,
    tickarray_bitmap_extension: &TickArrayBitmapExtension,
    zero_for_one: bool,
) -> VecDeque<TickArrayState> {
    let (_, mut current_vaild_tick_array_start_index) = pool_state
        .get_first_initialized_tick_array(&Some(*tickarray_bitmap_extension), zero_for_one)
        .unwrap();
    let mut tick_array_keys = Vec::new();
    tick_array_keys.push(
        Pubkey::find_program_address(
            &[
                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                &current_vaild_tick_array_start_index.to_be_bytes(),
            ],
            &pool_config.raydium_v3_program,
        )
        .0,
    );
    let mut max_array_size = 5;
    while max_array_size != 0 {
        let next_tick_array_index = pool_state
            .next_initialized_tick_array_start_index(
                &Some(*tickarray_bitmap_extension),
                current_vaild_tick_array_start_index,
                zero_for_one,
            )
            .unwrap();
        if next_tick_array_index.is_none() {
            break;
        }
        current_vaild_tick_array_start_index = next_tick_array_index.unwrap();
        tick_array_keys.push(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    &current_vaild_tick_array_start_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            )
            .0,
        );
        max_array_size -= 1;
    }
    let tick_array_rsps = rpc_client.get_multiple_accounts(&tick_array_keys).unwrap();
    let mut tick_arrays = VecDeque::new();
    for tick_array in tick_array_rsps {
        let tick_array_state =
            deserialize_anchor_account::<raydium_amm_v3::states::TickArrayState>(
                &tick_array.unwrap(),
            )
            .unwrap();
        tick_arrays.push_back(tick_array_state);
    }
    tick_arrays
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PositionNftTokenInfo {
    key: Pubkey,
    program: Pubkey,
    position: Pubkey,
    mint: Pubkey,
    amount: u64,
    decimals: u8,
}
fn get_all_nft_and_position_by_owner(
    client: &RpcClient,
    owner: &Pubkey,
    raydium_amm_v3_program: &Pubkey,
) -> Vec<PositionNftTokenInfo> {
    let mut spl_nfts = get_nft_account_and_position_by_owner(
        client,
        owner,
        spl_token::id(),
        raydium_amm_v3_program,
    );
    let spl_2022_nfts = get_nft_account_and_position_by_owner(
        client,
        owner,
        spl_token_2022::id(),
        raydium_amm_v3_program,
    );
    spl_nfts.extend(spl_2022_nfts);
    spl_nfts
}
fn get_nft_account_and_position_by_owner(
    client: &RpcClient,
    owner: &Pubkey,
    token_program: Pubkey,
    raydium_amm_v3_program: &Pubkey,
) -> Vec<PositionNftTokenInfo> {
    let all_tokens = client
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::ProgramId(token_program))
        .unwrap();
    let mut position_nft_accounts = Vec::new();
    for keyed_account in all_tokens {
        if let UiAccountData::Json(parsed_account) = keyed_account.account.data {
            if parsed_account.program == "spl-token" || parsed_account.program == "spl-token-2022" {
                if let Ok(TokenAccountType::Account(ui_token_account)) =
                    serde_json::from_value(parsed_account.parsed)
                {
                    let _frozen = ui_token_account.state == UiAccountState::Frozen;

                    let token = ui_token_account
                        .mint
                        .parse::<Pubkey>()
                        .unwrap_or_else(|err| panic!("Invalid mint: {}", err));
                    let token_account = keyed_account
                        .pubkey
                        .parse::<Pubkey>()
                        .unwrap_or_else(|err| panic!("Invalid token account: {}", err));
                    let token_amount = ui_token_account
                        .token_amount
                        .amount
                        .parse::<u64>()
                        .unwrap_or_else(|err| panic!("Invalid token amount: {}", err));

                    let _close_authority = ui_token_account.close_authority.map_or(*owner, |s| {
                        s.parse::<Pubkey>()
                            .unwrap_or_else(|err| panic!("Invalid close authority: {}", err))
                    });

                    if ui_token_account.token_amount.decimals == 0 && token_amount == 1 {
                        let (position_pda, _) = Pubkey::find_program_address(
                            &[
                                raydium_amm_v3::states::POSITION_SEED.as_bytes(),
                                token.to_bytes().as_ref(),
                            ],
                            &raydium_amm_v3_program,
                        );
                        position_nft_accounts.push(PositionNftTokenInfo {
                            key: token_account,
                            program: token_program,
                            position: position_pda,
                            mint: token,
                            amount: token_amount,
                            decimals: ui_token_account.token_amount.decimals,
                        });
                    }
                }
            }
        }
    }
    position_nft_accounts
}

#[derive(Debug, Parser)]
pub struct Opts {
    #[clap(subcommand)]
    pub command: CommandsName,
}
#[derive(Debug, Parser)]
pub enum CommandsName {
    NewMint {
        #[arg(short, long)]
        decimals: u8,
        authority: Option<Pubkey>,
        #[arg(short, long)]
        token_2022: bool,
        #[arg(short, long)]
        enable_freeze: bool,
        #[arg(short, long)]
        enable_close: bool,
        #[arg(short, long)]
        enable_non_transferable: bool,
        #[arg(short, long)]
        enable_permanent_delegate: bool,
        rate_bps: Option<i16>,
        default_account_state: Option<String>,
        transfer_fee: Option<Vec<u64>>,
        confidential_transfer_auto_approve: Option<bool>,
    },
    NewToken {
        mint: Pubkey,
        authority: Pubkey,
        #[arg(short, long)]
        not_ata: bool,
    },
    MintTo {
        mint: Pubkey,
        to_token: Pubkey,
        amount: u64,
    },
    WrapSol {
        amount: u64,
    },
    UnWrapSol {
        wrap_sol_account: Pubkey,
    },
    CreateConfig {
        config_index: u16,
        tick_spacing: u16,
        trade_fee_rate: u32,
        protocol_fee_rate: u32,
        fund_fee_rate: u32,
    },
    UpdateConfig {
        config_index: u16,
        param: u8,
        value: u32,
        remaining: Option<Pubkey>,
    },
    CreateOperation,
    UpdateOperation {
        param: u8,
        keys: Vec<Pubkey>,
    },
    CreatePool {
        config_index: u16,
        price: f64,
        mint0: Pubkey,
        mint1: Pubkey,
        #[arg(short, long, default_value_t = 0)]
        open_time: u64,
    },
    InitReward {
        open_time: u64,
        end_time: u64,
        emissions: f64,
        reward_mint: Pubkey,
    },
    SetRewardParams {
        index: u8,
        open_time: u64,
        end_time: u64,
        emissions: f64,
        reward_mint: Pubkey,
    },
    TransferRewardOwner {
        pool_id: Pubkey,
        new_owner: Pubkey,
        #[arg(short, long)]
        encode: bool,
        authority: Option<Pubkey>,
    },
    OpenPosition {
        tick_lower_price: f64,
        tick_upper_price: f64,
        #[arg(short, long)]
        is_base_0: bool,
        input_amount: u64,
        #[arg(short, long)]
        with_metadata: bool,
    },
    IncreaseLiquidity {
        tick_lower_price: f64,
        tick_upper_price: f64,
        #[arg(short, long)]
        is_base_0: bool,
        imput_amount: u64,
    },
    DecreaseLiquidity {
        tick_lower_index: i32,
        tick_upper_index: i32,
        liquidity: Option<u128>,
        #[arg(short, long)]
        simulate: bool,
    },
    Swap {
        input_token: Pubkey,
        output_token: Pubkey,
        #[arg(short, long)]
        base_in: bool,
        #[arg(short, long)]
        simulate: bool,
        amount: u64,
        limit_price: Option<f64>,
    },
    SwapV2 {
        input_token: Pubkey,
        output_token: Pubkey,
        #[arg(short, long)]
        base_in: bool,
        #[arg(short, long)]
        simulate: bool,
        amount: u64,
        limit_price: Option<f64>,
    },
    PPositionByOwner {
        user_wallet: Pubkey,
    },
    PTickState {
        tick: i32,
        pool_id: Option<Pubkey>,
    },
    CompareKey {
        key0: Pubkey,
        key1: Pubkey,
    },
    PMint {
        mint: Pubkey,
    },
    PToken {
        token: Pubkey,
    },
    POperation,
    PObservation,
    PConfig {
        config_index: u16,
    },
    PriceToTick {
        price: f64,
    },
    TickToPrice {
        tick: i32,
    },
    TickWithSpacing {
        tick: i32,
        tick_spacing: u16,
    },
    TickArraryStartIndex {
        tick: i32,
        tick_spacing: u16,
    },
    LiquidityToAmounts {
        tick_lower: i32,
        tick_upper: i32,
        liquidity: i128,
    },
    PPersonalPositionByPool {
        pool_id: Option<Pubkey>,
    },
    PProtocolPositionByPool {
        pool_id: Option<Pubkey>,
    },
    PTickArrayByPool {
        pool_id: Option<Pubkey>,
    },
    PPool {
        pool_id: Option<Pubkey>,
    },
    PBitmapExtension {
        bitmap_extension: Option<Pubkey>,
    },
    PProtocol {
        protocol_id: Pubkey,
    },
    PPersonal {
        personal_id: Pubkey,
    },
    DecodeInstruction {
        instr_hex_data: String,
    },
    DecodeEvent {
        log_event: String,
    },
    DecodeTxLog {
        tx_id: String,
    },
}
// #[cfg(not(feature = "async"))]
fn main() -> Result<()> {
    println!("Starting...");
    let client_config = "client_config.ini";
    let pool_config = load_cfg(&client_config.to_string()).unwrap();
    // Admin and cluster params.
    let payer = read_keypair_file(&pool_config.payer_path)?;
    let admin = read_keypair_file(&pool_config.admin_path)?;
    // solana rpc client
    let rpc_client = RpcClient::new(pool_config.http_url.to_string());

    // anchor client.
    let anchor_config = pool_config.clone();
    let url = Cluster::Custom(anchor_config.http_url, anchor_config.ws_url);
    let wallet = read_keypair_file(&pool_config.payer_path)?;
    let anchor_client = Client::new(url, Rc::new(wallet));
    let program = anchor_client.program(pool_config.raydium_v3_program)?;

    let opts = Opts::parse();
    match opts.command {
        CommandsName::NewMint {
            authority,
            decimals,
            token_2022,
            enable_freeze,
            enable_close,
            enable_non_transferable,
            enable_permanent_delegate,
            rate_bps,
            default_account_state,
            transfer_fee,
            confidential_transfer_auto_approve,
        } => {
            let token_program = if token_2022 {
                spl_token_2022::id()
            } else {
                spl_token::id()
            };
            let authority = if let Some(key) = authority {
                key
            } else {
                payer.pubkey()
            };
            let freeze_authority = if enable_freeze { Some(authority) } else { None };
            let mut extensions = vec![];
            if enable_close {
                extensions.push(ExtensionInitializationParams::MintCloseAuthority {
                    close_authority: Some(authority),
                });
            }
            if enable_permanent_delegate {
                extensions.push(ExtensionInitializationParams::PermanentDelegate {
                    delegate: authority,
                });
            }
            if let Some(rate_bps) = rate_bps {
                extensions.push(ExtensionInitializationParams::InterestBearingConfig {
                    rate_authority: Some(authority),
                    rate: rate_bps,
                })
            }
            if let Some(state) = default_account_state {
                assert!(
                    enable_freeze,
                    "Token requires a freeze authority to default to frozen accounts"
                );
                let account_state;
                match state.as_str() {
                    "Uninitialized" => account_state = AccountState::Uninitialized,
                    "Initialized" => account_state = AccountState::Initialized,
                    "Frozen" => account_state = AccountState::Frozen,
                    _ => panic!("error default_account_state[Uninitialized, Initialized, Frozen]"),
                }
                extensions.push(ExtensionInitializationParams::DefaultAccountState {
                    state: account_state,
                })
            }
            if let Some(transfer_fee_value) = transfer_fee {
                let transfer_fee_basis_points = transfer_fee_value[0] as u16;
                let maximum_fee = transfer_fee_value[1];
                extensions.push(ExtensionInitializationParams::TransferFeeConfig {
                    transfer_fee_config_authority: Some(authority),
                    withdraw_withheld_authority: Some(authority),
                    transfer_fee_basis_points,
                    maximum_fee,
                });
            }
            if enable_non_transferable {
                extensions.push(ExtensionInitializationParams::NonTransferable);
            }
            if let Some(auto_approve) = confidential_transfer_auto_approve {
                extensions.push(ExtensionInitializationParams::ConfidentialTransferMint {
                    authority: Some(authority),
                    auto_approve_new_accounts: auto_approve,
                    auditor_elgamal_pubkey: None,
                });
            }

            let mint = Keypair::generate(&mut OsRng);
            let create_and_init_instr = create_and_init_mint_instr(
                &pool_config.clone(),
                token_program,
                &mint.pubkey(),
                &authority,
                freeze_authority.as_ref(),
                extensions,
                decimals as u8,
            )?;
            // send
            let signers = vec![&payer, &mint];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_and_init_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::NewToken {
            mint,
            authority,
            not_ata,
        } => {
            let mut signers = vec![&payer];
            let auxiliary_token_keypair = Keypair::generate(&mut OsRng);
            let create_ata_instr = if not_ata {
                signers.push(&auxiliary_token_keypair);
                create_and_init_auxiliary_token(
                    &pool_config.clone(),
                    &auxiliary_token_keypair.pubkey(),
                    &mint,
                    &authority,
                )?
            } else {
                let mint_account = rpc_client.get_account(&mint)?;
                create_ata_token_account_instr(
                    &pool_config.clone(),
                    mint_account.owner,
                    &mint,
                    &authority,
                )?
            };
            // send
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_ata_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::MintTo {
            mint,
            to_token,
            amount,
        } => {
            let mint_account = rpc_client.get_account(&mint)?;
            let mint_to_instr = spl_token_mint_to_instr(
                &pool_config.clone(),
                mint_account.owner,
                &mint,
                &to_token,
                amount,
                &payer,
            )?;
            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &mint_to_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::WrapSol { amount } => {
            let wrap_sol_instr = wrap_sol_instr(&pool_config, amount)?;
            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &wrap_sol_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::UnWrapSol { wrap_sol_account } => {
            let unwrap_sol_instr =
                close_token_account(&pool_config, &wrap_sol_account, &payer.pubkey(), &payer)?;
            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &unwrap_sol_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::CreateConfig {
            config_index,
            tick_spacing,
            trade_fee_rate,
            protocol_fee_rate,
            fund_fee_rate,
        } => {
            let create_instr = create_amm_config_instr(
                &pool_config.clone(),
                config_index,
                tick_spacing,
                trade_fee_rate,
                protocol_fee_rate,
                fund_fee_rate,
            )?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::UpdateConfig {
            config_index,
            param,
            value,
            remaining,
        } => {
            let mut remaing_accounts = Vec::new();
            let mut update_value = 0;
            let match_param = Some(param);
            match match_param {
                Some(0) => update_value = value,
                Some(1) => update_value = value,
                Some(2) => update_value = value,
                Some(3) => {
                    let remaining_key = remaining.unwrap();
                    remaing_accounts.push(AccountMeta::new_readonly(remaining_key, false));
                }
                Some(4) => {
                    let remaining_key = remaining.unwrap();
                    remaing_accounts.push(AccountMeta::new_readonly(remaining_key, false));
                }
                _ => panic!("error input"),
            }
            let (amm_config_key, __bump) = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::AMM_CONFIG_SEED.as_bytes(),
                    &config_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            );
            let update_amm_config_instr = update_amm_config_instr(
                &pool_config.clone(),
                amm_config_key,
                remaing_accounts,
                param,
                update_value,
            )?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &update_amm_config_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::CreateOperation => {
            let create_instr = create_operation_account_instr(&pool_config.clone())?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::UpdateOperation { param, keys } => {
            let create_instr = update_operation_account_instr(&pool_config.clone(), param, keys)?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::CreatePool {
            config_index,
            price,
            mint0,
            mint1,
            open_time,
        } => {
            let mut price = price;
            let mut mint0 = mint0;
            let mut mint1 = mint1;
            if mint0 > mint1 {
                std::mem::swap(&mut mint0, &mut mint1);
                price = 1.0 / price;
            }
            println!("mint0:{}, mint1:{}, price:{}", mint0, mint1, price);
            let load_pubkeys = vec![mint0, mint1];
            let rsps = rpc_client.get_multiple_accounts(&load_pubkeys)?;
            let mint0_owner = rsps[0].clone().unwrap().owner;
            let mint1_owner = rsps[1].clone().unwrap().owner;
            let mint0_account =
                spl_token::state::Mint::unpack(&rsps[0].as_ref().unwrap().data).unwrap();
            let mint1_account =
                spl_token::state::Mint::unpack(&rsps[1].as_ref().unwrap().data).unwrap();
            let sqrt_price_x64 =
                price_to_sqrt_price_x64(price, mint0_account.decimals, mint1_account.decimals);
            let (amm_config_key, __bump) = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::AMM_CONFIG_SEED.as_bytes(),
                    &config_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            );
            let tick = tick_math::get_tick_at_sqrt_price(sqrt_price_x64).unwrap();
            println!(
                "tick:{}, price:{}, sqrt_price_x64:{}, amm_config_key:{}",
                tick, price, sqrt_price_x64, amm_config_key
            );

            let create_pool_instr = create_pool_instr(
                &pool_config.clone(),
                amm_config_key,
                mint0,
                mint1,
                mint0_owner,
                mint1_owner,
                pool_config.tickarray_bitmap_extension.unwrap(),
                sqrt_price_x64,
                open_time,
            )?;

            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_pool_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::InitReward {
            open_time,
            end_time,
            emissions,
            reward_mint,
        } => {
            let mint_account = rpc_client.get_account(&reward_mint)?;
            let emissions_per_second_x64 = (emissions * fixed_point_64::Q64 as f64) as u128;
            let program = anchor_client.program(pool_config.raydium_v3_program)?;
            println!("{}", pool_config.pool_id_account.unwrap());
            let pool_account: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;
            let operator_account_key = Pubkey::find_program_address(
                &[raydium_amm_v3::states::OPERATION_SEED.as_bytes()],
                &program.id(),
            )
            .0;

            let reward_token_vault = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::POOL_REWARD_VAULT_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    reward_mint.to_bytes().as_ref(),
                ],
                &program.id(),
            )
            .0;
            let user_reward_token = get_associated_token_address(&admin.pubkey(), &reward_mint);
            let create_instr = initialize_reward_instr(
                &pool_config.clone(),
                pool_config.pool_id_account.unwrap(),
                pool_account.amm_config,
                operator_account_key,
                reward_mint,
                reward_token_vault,
                user_reward_token,
                mint_account.owner,
                open_time,
                end_time,
                emissions_per_second_x64,
            )?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::SetRewardParams {
            index,
            open_time,
            end_time,
            emissions,
            reward_mint,
        } => {
            let emissions_per_second_x64 = (emissions * fixed_point_64::Q64 as f64) as u128;

            let program = anchor_client.program(pool_config.raydium_v3_program)?;
            println!("{}", pool_config.pool_id_account.unwrap());
            let pool_account: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;
            let operator_account_key = Pubkey::find_program_address(
                &[raydium_amm_v3::states::OPERATION_SEED.as_bytes()],
                &program.id(),
            )
            .0;

            let reward_token_vault = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::POOL_REWARD_VAULT_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    reward_mint.to_bytes().as_ref(),
                ],
                &program.id(),
            )
            .0;
            let user_reward_token = get_associated_token_address(&admin.pubkey(), &reward_mint);
            let create_instr = set_reward_params_instr(
                &pool_config.clone(),
                pool_account.amm_config,
                pool_config.pool_id_account.unwrap(),
                reward_token_vault,
                user_reward_token,
                operator_account_key,
                index,
                open_time,
                end_time,
                emissions_per_second_x64,
            )?;
            // send
            let signers = vec![&payer, &admin];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &create_instr,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            let signature = send_txn(&rpc_client, &txn, true)?;
            println!("{}", signature);
        }
        CommandsName::TransferRewardOwner {
            pool_id,
            new_owner,
            encode,
            authority,
        } => {
            let transfer_reward_owner_instrs =
                transfer_reward_owner(&pool_config.clone(), pool_id, new_owner, encode, authority)
                    .unwrap();
            if encode {
                println!(
                    "instruction.data:{:?}",
                    transfer_reward_owner_instrs[0].data
                );
                let message = Message::new(&transfer_reward_owner_instrs, None);
                let serialize_data = serialize(&message).unwrap();
                let raw_data = bs58::encode(serialize_data).into_string();
                println!("raw_data:{:?}", raw_data);
            } else {
                // send
                let signers = vec![&payer, &admin];
                let recent_hash = rpc_client.get_latest_blockhash()?;
                let txn = Transaction::new_signed_with_payer(
                    &transfer_reward_owner_instrs,
                    Some(&payer.pubkey()),
                    &signers,
                    recent_hash,
                );
                let signature = send_txn(&rpc_client, &txn, true)?;
                println!("{}", signature);
            }
        }
        CommandsName::OpenPosition {
            tick_lower_price,
            tick_upper_price,
            is_base_0,
            input_amount,
            with_metadata,
        } => {
            // load pool to get observation
            let pool: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;

            let tick_lower_price_x64 = price_to_sqrt_price_x64(
                tick_lower_price,
                pool.mint_decimals_0,
                pool.mint_decimals_1,
            );
            let tick_upper_price_x64 = price_to_sqrt_price_x64(
                tick_upper_price,
                pool.mint_decimals_0,
                pool.mint_decimals_1,
            );
            let tick_lower_index = tick_with_spacing(
                tick_math::get_tick_at_sqrt_price(tick_lower_price_x64)?,
                pool.tick_spacing.into(),
            );
            let tick_upper_index = tick_with_spacing(
                tick_math::get_tick_at_sqrt_price(tick_upper_price_x64)?,
                pool.tick_spacing.into(),
            );
            println!(
                "tick_lower_index:{}, tick_upper_index:{}",
                tick_lower_index, tick_upper_index
            );
            let tick_lower_price_x64 = tick_math::get_sqrt_price_at_tick(tick_lower_index)?;
            let tick_upper_price_x64 = tick_math::get_sqrt_price_at_tick(tick_upper_index)?;
            let liquidity = if is_base_0 {
                liquidity_math::get_liquidity_from_single_amount_0(
                    pool.sqrt_price_x64,
                    tick_lower_price_x64,
                    tick_upper_price_x64,
                    input_amount,
                )
            } else {
                liquidity_math::get_liquidity_from_single_amount_1(
                    pool.sqrt_price_x64,
                    tick_lower_price_x64,
                    tick_upper_price_x64,
                    input_amount,
                )
            };
            let (amount_0, amount_1) = liquidity_math::get_delta_amounts_signed(
                pool.tick_current,
                pool.sqrt_price_x64,
                tick_lower_index,
                tick_upper_index,
                liquidity as i128,
            )?;
            println!(
                "amount_0:{}, amount_1:{}, liquidity:{}",
                amount_0, amount_1, liquidity
            );
            // calc with slippage
            let amount_0_with_slippage =
                amount_with_slippage(amount_0 as u64, pool_config.slippage, true);
            let amount_1_with_slippage =
                amount_with_slippage(amount_1 as u64, pool_config.slippage, true);
            // calc with transfer_fee
            let transfer_fee = get_pool_mints_inverse_fee(
                &rpc_client,
                pool.token_mint_0,
                pool.token_mint_1,
                amount_0_with_slippage,
                amount_1_with_slippage,
            );
            println!(
                "transfer_fee_0:{}, transfer_fee_1:{}",
                transfer_fee.0.transfer_fee, transfer_fee.1.transfer_fee
            );
            let amount_0_max = (amount_0_with_slippage as u64)
                .checked_add(transfer_fee.0.transfer_fee)
                .unwrap();
            let amount_1_max = (amount_1_with_slippage as u64)
                .checked_add(transfer_fee.1.transfer_fee)
                .unwrap();

            let tick_array_lower_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_lower_index,
                    pool.tick_spacing.into(),
                );
            let tick_array_upper_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_upper_index,
                    pool.tick_spacing.into(),
                );
            // load position
            let position_nft_infos = get_all_nft_and_position_by_owner(
                &rpc_client,
                &payer.pubkey(),
                &pool_config.raydium_v3_program,
            );
            let positions: Vec<Pubkey> = position_nft_infos
                .iter()
                .map(|item| item.position)
                .collect();
            let rsps = rpc_client.get_multiple_accounts(&positions)?;
            let mut user_positions = Vec::new();
            for rsp in rsps {
                match rsp {
                    None => continue,
                    Some(rsp) => {
                        let position = deserialize_anchor_account::<
                            raydium_amm_v3::states::PersonalPositionState,
                        >(&rsp)?;
                        user_positions.push(position);
                    }
                }
            }
            let mut find_position = raydium_amm_v3::states::PersonalPositionState::default();
            for position in user_positions {
                if position.pool_id == pool_config.pool_id_account.unwrap()
                    && position.tick_lower_index == tick_lower_index
                    && position.tick_upper_index == tick_upper_index
                {
                    find_position = position.clone();
                }
            }
            if find_position.nft_mint == Pubkey::default() {
                // personal position not exist
                // new nft mint
                let nft_mint = Keypair::generate(&mut OsRng);
                let mut remaining_accounts = Vec::new();
                remaining_accounts.push(AccountMeta::new(
                    pool_config.tickarray_bitmap_extension.unwrap(),
                    false,
                ));

                let mut instructions = Vec::new();
                let request_inits_instr =
                    ComputeBudgetInstruction::set_compute_unit_limit(1400_000u32);
                instructions.push(request_inits_instr);
                let open_position_instr = open_position_with_token22_nft_instr(
                    &pool_config.clone(),
                    pool_config.pool_id_account.unwrap(),
                    pool.token_vault_0,
                    pool.token_vault_1,
                    pool.token_mint_0,
                    pool.token_mint_1,
                    nft_mint.pubkey(),
                    payer.pubkey(),
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint0.unwrap(),
                        &transfer_fee.0.owner,
                    ),
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint1.unwrap(),
                        &transfer_fee.1.owner,
                    ),
                    remaining_accounts,
                    liquidity,
                    amount_0_max,
                    amount_1_max,
                    tick_lower_index,
                    tick_upper_index,
                    tick_array_lower_start_index,
                    tick_array_upper_start_index,
                    with_metadata,
                )?;
                instructions.extend(open_position_instr);
                // send
                let signers = vec![&payer, &nft_mint];
                let recent_hash = rpc_client.get_latest_blockhash()?;
                let txn = Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&payer.pubkey()),
                    &signers,
                    recent_hash,
                );
                let signature = send_txn(&rpc_client, &txn, true)?;
                println!("{}", signature);
            } else {
                // personal position exist
                println!("personal position exist:{:?}", find_position);
            }
        }
        CommandsName::IncreaseLiquidity {
            tick_lower_price,
            tick_upper_price,
            is_base_0,
            imput_amount,
        } => {
            // load pool to get observation
            let pool: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;

            // load position
            let position_nft_infos = get_all_nft_and_position_by_owner(
                &rpc_client,
                &payer.pubkey(),
                &pool_config.raydium_v3_program,
            );
            let positions: Vec<Pubkey> = position_nft_infos
                .iter()
                .map(|item| item.position)
                .collect();
            let rsps = rpc_client.get_multiple_accounts(&positions)?;
            let mut user_positions = Vec::new();
            for rsp in rsps {
                match rsp {
                    None => continue,
                    Some(rsp) => {
                        let position = deserialize_anchor_account::<
                            raydium_amm_v3::states::PersonalPositionState,
                        >(&rsp)?;
                        user_positions.push(position);
                    }
                }
            }

            let tick_lower_price_x64 = price_to_sqrt_price_x64(
                tick_lower_price,
                pool.mint_decimals_0,
                pool.mint_decimals_1,
            );
            let tick_upper_price_x64 = price_to_sqrt_price_x64(
                tick_upper_price,
                pool.mint_decimals_0,
                pool.mint_decimals_1,
            );
            let tick_lower_index = tick_with_spacing(
                tick_math::get_tick_at_sqrt_price(tick_lower_price_x64)?,
                pool.tick_spacing.into(),
            );
            let tick_upper_index = tick_with_spacing(
                tick_math::get_tick_at_sqrt_price(tick_upper_price_x64)?,
                pool.tick_spacing.into(),
            );
            println!(
                "tick_lower_index:{}, tick_upper_index:{}",
                tick_lower_index, tick_upper_index
            );
            let tick_lower_price_x64 = tick_math::get_sqrt_price_at_tick(tick_lower_index)?;
            let tick_upper_price_x64 = tick_math::get_sqrt_price_at_tick(tick_upper_index)?;
            let liquidity = if is_base_0 {
                liquidity_math::get_liquidity_from_single_amount_0(
                    pool.sqrt_price_x64,
                    tick_lower_price_x64,
                    tick_upper_price_x64,
                    imput_amount,
                )
            } else {
                liquidity_math::get_liquidity_from_single_amount_1(
                    pool.sqrt_price_x64,
                    tick_lower_price_x64,
                    tick_upper_price_x64,
                    imput_amount,
                )
            };
            let (amount_0, amount_1) = liquidity_math::get_delta_amounts_signed(
                pool.tick_current,
                pool.sqrt_price_x64,
                tick_lower_index,
                tick_upper_index,
                liquidity as i128,
            )?;
            println!(
                "amount_0:{}, amount_1:{}, liquidity:{}",
                amount_0, amount_1, liquidity
            );
            // calc with slippage
            let amount_0_with_slippage =
                amount_with_slippage(amount_0 as u64, pool_config.slippage, true);
            let amount_1_with_slippage =
                amount_with_slippage(amount_1 as u64, pool_config.slippage, true);
            // calc with transfer_fee
            let transfer_fee = get_pool_mints_inverse_fee(
                &rpc_client,
                pool.token_mint_0,
                pool.token_mint_1,
                amount_0_with_slippage,
                amount_1_with_slippage,
            );
            println!(
                "transfer_fee_0:{}, transfer_fee_1:{}",
                transfer_fee.0.transfer_fee, transfer_fee.1.transfer_fee
            );
            let amount_0_max = (amount_0_with_slippage as u64)
                .checked_add(transfer_fee.0.transfer_fee)
                .unwrap();
            let amount_1_max = (amount_1_with_slippage as u64)
                .checked_add(transfer_fee.1.transfer_fee)
                .unwrap();

            let tick_array_lower_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_lower_index,
                    pool.tick_spacing.into(),
                );
            let tick_array_upper_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_upper_index,
                    pool.tick_spacing.into(),
                );
            let mut find_position = raydium_amm_v3::states::PersonalPositionState::default();
            for position in user_positions {
                if position.pool_id == pool_config.pool_id_account.unwrap()
                    && position.tick_lower_index == tick_lower_index
                    && position.tick_upper_index == tick_upper_index
                {
                    find_position = position.clone();
                }
            }
            if find_position.nft_mint != Pubkey::default()
                && find_position.pool_id == pool_config.pool_id_account.unwrap()
            {
                let user_nft_token_info = position_nft_infos
                    .iter()
                    .find(|&nft_info| nft_info.mint == find_position.nft_mint)
                    .unwrap();
                // personal position exist
                let mut remaining_accounts = Vec::new();
                remaining_accounts.push(AccountMeta::new_readonly(
                    pool_config.tickarray_bitmap_extension.unwrap(),
                    false,
                ));

                let increase_instr = increase_liquidity_instr(
                    &pool_config.clone(),
                    pool_config.pool_id_account.unwrap(),
                    pool.token_vault_0,
                    pool.token_vault_1,
                    pool.token_mint_0,
                    pool.token_mint_1,
                    find_position.nft_mint,
                    user_nft_token_info.key,
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint0.unwrap(),
                        &transfer_fee.0.owner,
                    ),
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint1.unwrap(),
                        &transfer_fee.0.owner,
                    ),
                    remaining_accounts,
                    liquidity,
                    amount_0_max,
                    amount_1_max,
                    tick_lower_index,
                    tick_upper_index,
                    tick_array_lower_start_index,
                    tick_array_upper_start_index,
                )?;
                // send
                let signers = vec![&payer];
                let recent_hash = rpc_client.get_latest_blockhash()?;
                let txn = Transaction::new_signed_with_payer(
                    &increase_instr,
                    Some(&payer.pubkey()),
                    &signers,
                    recent_hash,
                );
                let signature = send_txn(&rpc_client, &txn, true)?;
                println!("{}", signature);
            } else {
                // personal position not exist
                println!("personal position exist:{:?}", find_position);
            }
        }
        CommandsName::DecreaseLiquidity {
            tick_lower_index,
            tick_upper_index,
            liquidity,
            simulate,
        } => {
            // load pool to get observation
            let pool: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;

            let tick_array_lower_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_lower_index,
                    pool.tick_spacing.into(),
                );
            let tick_array_upper_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick_upper_index,
                    pool.tick_spacing.into(),
                );
            // load position
            let position_nft_infos = get_all_nft_and_position_by_owner(
                &rpc_client,
                &payer.pubkey(),
                &pool_config.raydium_v3_program,
            );
            let positions: Vec<Pubkey> = position_nft_infos
                .iter()
                .map(|item| item.position)
                .collect();
            let rsps = rpc_client.get_multiple_accounts(&positions)?;
            let mut user_positions = Vec::new();
            for rsp in rsps {
                match rsp {
                    None => continue,
                    Some(rsp) => {
                        let position = deserialize_anchor_account::<
                            raydium_amm_v3::states::PersonalPositionState,
                        >(&rsp)?;
                        user_positions.push(position);
                    }
                }
            }
            let mut find_position = raydium_amm_v3::states::PersonalPositionState::default();
            for position in user_positions {
                if position.pool_id == pool_config.pool_id_account.unwrap()
                    && position.tick_lower_index == tick_lower_index
                    && position.tick_upper_index == tick_upper_index
                {
                    find_position = position.clone();
                    println!("liquidity:{:?}", find_position);
                }
            }
            if find_position.nft_mint != Pubkey::default()
                && find_position.pool_id == pool_config.pool_id_account.unwrap()
            {
                let user_nft_token_info = position_nft_infos
                    .iter()
                    .find(|&nft_info| nft_info.mint == find_position.nft_mint)
                    .unwrap();
                let mut reward_vault_with_user_vault: Vec<Pubkey> = Vec::new();
                for item in pool.reward_infos.into_iter() {
                    if item.token_mint != Pubkey::default() {
                        reward_vault_with_user_vault.push(item.token_vault);
                        reward_vault_with_user_vault.push(get_associated_token_address(
                            &payer.pubkey(),
                            &item.token_mint,
                        ));
                        reward_vault_with_user_vault.push(item.token_mint);
                    }
                }
                let liquidity = if let Some(liquidity) = liquidity {
                    liquidity
                } else {
                    find_position.liquidity
                };
                let (amount_0, amount_1) = liquidity_math::get_delta_amounts_signed(
                    pool.tick_current,
                    pool.sqrt_price_x64,
                    tick_lower_index,
                    tick_upper_index,
                    -(liquidity as i128),
                )?;
                let amount_0_with_slippage =
                    amount_with_slippage(amount_0, pool_config.slippage, false);
                let amount_1_with_slippage =
                    amount_with_slippage(amount_1, pool_config.slippage, false);
                let transfer_fee = get_pool_mints_transfer_fee(
                    &rpc_client,
                    pool.token_mint_0,
                    pool.token_mint_1,
                    amount_0_with_slippage,
                    amount_1_with_slippage,
                );
                let amount_0_min = amount_0_with_slippage
                    .checked_sub(transfer_fee.0.transfer_fee)
                    .unwrap();
                let amount_1_min = amount_1_with_slippage
                    .checked_sub(transfer_fee.1.transfer_fee)
                    .unwrap();

                let mut remaining_accounts = Vec::new();
                remaining_accounts.push(AccountMeta::new(
                    pool_config.tickarray_bitmap_extension.unwrap(),
                    false,
                ));

                let mut accounts = reward_vault_with_user_vault
                    .into_iter()
                    .map(|item| AccountMeta::new(item, false))
                    .collect();
                remaining_accounts.append(&mut accounts);
                // personal position exist
                let mut decrease_instr = decrease_liquidity_instr(
                    &pool_config.clone(),
                    pool_config.pool_id_account.unwrap(),
                    pool.token_vault_0,
                    pool.token_vault_1,
                    pool.token_mint_0,
                    pool.token_mint_1,
                    find_position.nft_mint,
                    user_nft_token_info.key,
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint0.unwrap(),
                        &transfer_fee.0.owner,
                    ),
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &payer.pubkey(),
                        &pool_config.mint1.unwrap(),
                        &transfer_fee.1.owner,
                    ),
                    remaining_accounts,
                    liquidity,
                    amount_0_min,
                    amount_1_min,
                    tick_lower_index,
                    tick_upper_index,
                    tick_array_lower_start_index,
                    tick_array_upper_start_index,
                )?;
                if liquidity == find_position.liquidity {
                    let close_position_instr = close_personal_position_instr(
                        &pool_config.clone(),
                        find_position.nft_mint,
                        user_nft_token_info.key,
                        user_nft_token_info.program,
                    )?;
                    decrease_instr.extend(close_position_instr);
                }
                // send
                let signers = vec![&payer];
                let recent_hash = rpc_client.get_latest_blockhash()?;
                let txn = Transaction::new_signed_with_payer(
                    &decrease_instr,
                    Some(&payer.pubkey()),
                    &signers,
                    recent_hash,
                );
                if simulate {
                    let ret = simulate_transaction(
                        &rpc_client,
                        &txn,
                        true,
                        CommitmentConfig::confirmed(),
                    )?;
                    println!("{:#?}", ret);
                } else {
                    let signature = send_txn(&rpc_client, &txn, true)?;
                    println!("{}", signature);
                }
            } else {
                // personal position not exist
                println!("personal position exist:{:?}", find_position);
            }
        }
        CommandsName::Swap {
            input_token,
            output_token,
            base_in,
            simulate,
            amount,
            limit_price,
        } => {
            // load mult account
            let load_accounts = vec![
                input_token,
                output_token,
                pool_config.amm_config_key,
                pool_config.pool_id_account.unwrap(),
                pool_config.tickarray_bitmap_extension.unwrap(),
            ];
            let rsps = rpc_client.get_multiple_accounts(&load_accounts)?;
            let [user_input_account, user_output_account, amm_config_account, pool_account, tickarray_bitmap_extension_account] =
                array_ref![rsps, 0, 5];
            let user_input_state =
                StateWithExtensions::<Account>::unpack(&user_input_account.as_ref().unwrap().data)
                    .unwrap();
            let user_output_state =
                StateWithExtensions::<Account>::unpack(&user_output_account.as_ref().unwrap().data)
                    .unwrap();
            let amm_config_state = deserialize_anchor_account::<raydium_amm_v3::states::AmmConfig>(
                amm_config_account.as_ref().unwrap(),
            )?;
            let pool_state = deserialize_anchor_account::<raydium_amm_v3::states::PoolState>(
                pool_account.as_ref().unwrap(),
            )?;
            let tickarray_bitmap_extension =
                deserialize_anchor_account::<raydium_amm_v3::states::TickArrayBitmapExtension>(
                    tickarray_bitmap_extension_account.as_ref().unwrap(),
                )?;
            let zero_for_one = user_input_state.base.mint == pool_state.token_mint_0
                && user_output_state.base.mint == pool_state.token_mint_1;
            // load tick_arrays
            let mut tick_arrays = load_cur_and_next_five_tick_array(
                &rpc_client,
                &pool_config,
                &pool_state,
                &tickarray_bitmap_extension,
                zero_for_one,
            );

            let mut sqrt_price_limit_x64 = None;
            if limit_price.is_some() {
                let sqrt_price_x64 = price_to_sqrt_price_x64(
                    limit_price.unwrap(),
                    pool_state.mint_decimals_0,
                    pool_state.mint_decimals_1,
                );
                sqrt_price_limit_x64 = Some(sqrt_price_x64);
            }

            let (mut other_amount_threshold, mut tick_array_indexs) =
                utils::get_out_put_amount_and_remaining_accounts(
                    amount,
                    sqrt_price_limit_x64,
                    zero_for_one,
                    base_in,
                    &amm_config_state,
                    &pool_state,
                    &tickarray_bitmap_extension,
                    &mut tick_arrays,
                )
                .unwrap();
            println!(
                "amount:{}, other_amount_threshold:{}",
                amount, other_amount_threshold
            );
            if base_in {
                // min out
                other_amount_threshold =
                    amount_with_slippage(other_amount_threshold, pool_config.slippage, false);
            } else {
                // max in
                other_amount_threshold =
                    amount_with_slippage(other_amount_threshold, pool_config.slippage, true);
            }

            let current_or_next_tick_array_key = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    &tick_array_indexs.pop_front().unwrap().to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            )
            .0;
            let mut remaining_accounts = Vec::new();
            remaining_accounts.push(AccountMeta::new_readonly(
                pool_config.tickarray_bitmap_extension.unwrap(),
                false,
            ));
            let mut accounts = tick_array_indexs
                .into_iter()
                .map(|index| {
                    AccountMeta::new(
                        Pubkey::find_program_address(
                            &[
                                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                                &index.to_be_bytes(),
                            ],
                            &pool_config.raydium_v3_program,
                        )
                        .0,
                        false,
                    )
                })
                .collect();
            remaining_accounts.append(&mut accounts);
            let mut instructions = Vec::new();
            let request_inits_instr = ComputeBudgetInstruction::set_compute_unit_limit(1400_000u32);
            instructions.push(request_inits_instr);

            let event_authority = Pubkey::find_program_address(
                &["__event_authority".as_bytes()],
                &pool_config.raydium_v3_program,
            )
            .0;

            let swap_instr = swap_instr(
                &pool_config.clone(),
                pool_state.amm_config,
                pool_config.pool_id_account.unwrap(),
                if zero_for_one {
                    pool_state.token_vault_0
                } else {
                    pool_state.token_vault_1
                },
                if zero_for_one {
                    pool_state.token_vault_1
                } else {
                    pool_state.token_vault_0
                },
                pool_state.observation_key,
                input_token,
                output_token,
                current_or_next_tick_array_key,
                event_authority,
                remaining_accounts,
                amount,
                other_amount_threshold,
                sqrt_price_limit_x64,
                base_in,
            )
            .unwrap();
            instructions.extend(swap_instr);
            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &instructions,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            if simulate {
                let ret =
                    simulate_transaction(&rpc_client, &txn, true, CommitmentConfig::confirmed())?;
                println!("{:#?}", ret);
            } else {
                let signature = send_txn(&rpc_client, &txn, true)?;
                println!("{}", signature);
            }
        }
        CommandsName::SwapV2 {
            input_token,
            output_token,
            base_in,
            simulate,
            amount,
            limit_price,
        } => {
            // load mult account
            let load_accounts = vec![
                input_token,
                output_token,
                pool_config.amm_config_key,
                pool_config.pool_id_account.unwrap(),
                pool_config.tickarray_bitmap_extension.unwrap(),
                pool_config.mint0.unwrap(),
                pool_config.mint1.unwrap(),
            ];
            let rsps = rpc_client.get_multiple_accounts(&load_accounts)?;
            let epoch = rpc_client.get_epoch_info().unwrap().epoch;
            let [user_input_account, user_output_account, amm_config_account, pool_account, tickarray_bitmap_extension_account, mint0_account, mint1_account] =
                array_ref![rsps, 0, 7];

            let user_input_token_data = user_input_account.clone().unwrap().data;
            let user_input_state = StateWithExtensions::<Account>::unpack(&user_input_token_data)?;
            let user_output_token_data = user_output_account.clone().unwrap().data;
            let user_output_state =
                StateWithExtensions::<Account>::unpack(&user_output_token_data)?;
            let mint0_data = mint0_account.clone().unwrap().data;
            let mint0_state = StateWithExtensions::<Mint>::unpack(&mint0_data)?;
            let mint1_data = mint1_account.clone().unwrap().data;
            let mint1_state = StateWithExtensions::<Mint>::unpack(&mint1_data)?;
            let amm_config_state = deserialize_anchor_account::<raydium_amm_v3::states::AmmConfig>(
                amm_config_account.as_ref().unwrap(),
            )?;
            let pool_state = deserialize_anchor_account::<raydium_amm_v3::states::PoolState>(
                pool_account.as_ref().unwrap(),
            )?;
            let tickarray_bitmap_extension =
                deserialize_anchor_account::<raydium_amm_v3::states::TickArrayBitmapExtension>(
                    tickarray_bitmap_extension_account.as_ref().unwrap(),
                )?;
            let zero_for_one = user_input_state.base.mint == pool_state.token_mint_0
                && user_output_state.base.mint == pool_state.token_mint_1;

            let transfer_fee = if base_in {
                if zero_for_one {
                    get_transfer_fee(&mint0_state, epoch, amount)
                } else {
                    get_transfer_fee(&mint1_state, epoch, amount)
                }
            } else {
                0
            };
            let amount_specified = amount.checked_sub(transfer_fee).unwrap();
            // load tick_arrays
            let mut tick_arrays = load_cur_and_next_five_tick_array(
                &rpc_client,
                &pool_config,
                &pool_state,
                &tickarray_bitmap_extension,
                zero_for_one,
            );

            let mut sqrt_price_limit_x64 = None;
            if limit_price.is_some() {
                let sqrt_price_x64 = price_to_sqrt_price_x64(
                    limit_price.unwrap(),
                    pool_state.mint_decimals_0,
                    pool_state.mint_decimals_1,
                );
                sqrt_price_limit_x64 = Some(sqrt_price_x64);
            }

            let (mut other_amount_threshold, tick_array_indexs) =
                utils::get_out_put_amount_and_remaining_accounts(
                    amount_specified,
                    sqrt_price_limit_x64,
                    zero_for_one,
                    base_in,
                    &amm_config_state,
                    &pool_state,
                    &tickarray_bitmap_extension,
                    &mut tick_arrays,
                )
                .unwrap();
            println!(
                "amount:{}, other_amount_threshold:{}",
                amount, other_amount_threshold
            );
            if base_in {
                // calc mint out amount with slippage
                other_amount_threshold =
                    amount_with_slippage(other_amount_threshold, pool_config.slippage, false);
            } else {
                // calc max in with slippage
                other_amount_threshold =
                    amount_with_slippage(other_amount_threshold, pool_config.slippage, true);
                // calc max in with transfer_fee
                let transfer_fee = if zero_for_one {
                    get_transfer_inverse_fee(&mint0_state, epoch, other_amount_threshold)
                } else {
                    get_transfer_inverse_fee(&mint1_state, epoch, other_amount_threshold)
                };
                other_amount_threshold += transfer_fee;
            }

            let mut remaining_accounts = Vec::new();
            remaining_accounts.push(AccountMeta::new_readonly(
                pool_config.tickarray_bitmap_extension.unwrap(),
                false,
            ));
            let mut accounts = tick_array_indexs
                .into_iter()
                .map(|index| {
                    AccountMeta::new(
                        Pubkey::find_program_address(
                            &[
                                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                                &index.to_be_bytes(),
                            ],
                            &pool_config.raydium_v3_program,
                        )
                        .0,
                        false,
                    )
                })
                .collect();
            remaining_accounts.append(&mut accounts);
            let mut instructions = Vec::new();
            let request_inits_instr = ComputeBudgetInstruction::set_compute_unit_limit(1400_000u32);
            instructions.push(request_inits_instr);

            let event_authority = Pubkey::find_program_address(
                &["__event_authority".as_bytes()],
                &pool_config.raydium_v3_program,
            )
            .0;

            let swap_instr = swap_v2_instr(
                &pool_config.clone(),
                pool_state.amm_config,
                pool_config.pool_id_account.unwrap(),
                if zero_for_one {
                    pool_state.token_vault_0
                } else {
                    pool_state.token_vault_1
                },
                if zero_for_one {
                    pool_state.token_vault_1
                } else {
                    pool_state.token_vault_0
                },
                pool_state.observation_key,
                input_token,
                output_token,
                if zero_for_one {
                    pool_state.token_mint_0
                } else {
                    pool_state.token_mint_1
                },
                if zero_for_one {
                    pool_state.token_mint_1
                } else {
                    pool_state.token_mint_0
                },
                event_authority,
                remaining_accounts,
                amount,
                other_amount_threshold,
                sqrt_price_limit_x64,
                base_in,
            )
            .unwrap();
            instructions.extend(swap_instr);
            // send
            let signers = vec![&payer];
            let recent_hash = rpc_client.get_latest_blockhash()?;
            let txn = Transaction::new_signed_with_payer(
                &instructions,
                Some(&payer.pubkey()),
                &signers,
                recent_hash,
            );
            if simulate {
                let ret =
                    simulate_transaction(&rpc_client, &txn, true, CommitmentConfig::confirmed())?;
                println!("{:#?}", ret);
            } else {
                let signature = send_txn(&rpc_client, &txn, true)?;
                println!("{}", signature);
            }
        }
        CommandsName::PPositionByOwner { user_wallet } => {
            // load position
            let position_nft_infos = get_all_nft_and_position_by_owner(
                &rpc_client,
                &user_wallet,
                &pool_config.raydium_v3_program,
            );
            let positions: Vec<Pubkey> = position_nft_infos
                .iter()
                .map(|item| item.position)
                .collect();
            let rsps = rpc_client.get_multiple_accounts(&positions)?;
            let mut user_positions = Vec::new();
            for rsp in rsps {
                match rsp {
                    None => continue,
                    Some(rsp) => {
                        let position = deserialize_anchor_account::<
                            raydium_amm_v3::states::PersonalPositionState,
                        >(&rsp)?;
                        let (personal_position_key, __bump) = Pubkey::find_program_address(
                            &[
                                raydium_amm_v3::states::POSITION_SEED.as_bytes(),
                                position.nft_mint.to_bytes().as_ref(),
                            ],
                            &program.id(),
                        );
                        println!("id:{}, lower:{}, upper:{}, liquidity:{}, fees_owed_0:{}, fees_owed_1:{}, fee_growth_inside_0:{}, fee_growth_inside_1:{}", personal_position_key, position.tick_lower_index, position.tick_upper_index, position.liquidity, position.token_fees_owed_0, position.token_fees_owed_1, position.fee_growth_inside_0_last_x64, position.fee_growth_inside_1_last_x64);
                        user_positions.push(position);
                    }
                }
            }
        }
        CommandsName::PTickState { tick, pool_id } => {
            let pool_id = if let Some(pool_id) = pool_id {
                pool_id
            } else {
                pool_config.pool_id_account.unwrap()
            };
            println!("pool_id:{}", pool_id);
            let pool: raydium_amm_v3::states::PoolState = program.account(pool_id)?;

            let tick_array_start_index =
                raydium_amm_v3::states::TickArrayState::get_array_start_index(
                    tick,
                    pool.tick_spacing.into(),
                );
            let program = anchor_client.program(pool_config.raydium_v3_program)?;
            let (tick_array_key, __bump) = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_id.to_bytes().as_ref(),
                    &tick_array_start_index.to_be_bytes(),
                ],
                &program.id(),
            );
            let mut tick_array_account: raydium_amm_v3::states::TickArrayState =
                program.account(tick_array_key)?;
            let tick_state = tick_array_account
                .get_tick_state_mut(tick, pool.tick_spacing.into())
                .unwrap();
            println!("{:?}", tick_state);
        }
        CommandsName::CompareKey { key0, key1 } => {
            let mut token_mint_0 = key0;
            let mut token_mint_1 = key1;
            if token_mint_0 > token_mint_1 {
                std::mem::swap(&mut token_mint_0, &mut token_mint_1);
            }
            println!("mint0:{}, mint1:{}", token_mint_0, token_mint_1);
        }
        CommandsName::PMint { mint } => {
            let mint_data = &rpc_client.get_account_data(&mint)?;
            let mint_state = StateWithExtensions::<Mint>::unpack(mint_data)?;
            println!("mint_state:{:?}", mint_state);
            let extensions = get_account_extensions(&mint_state);
            println!("mint_extensions:{:#?}", extensions);
        }
        CommandsName::PToken { token } => {
            let token_data = &rpc_client.get_account_data(&token)?;
            let token_state = StateWithExtensions::<Account>::unpack(token_data)?;
            println!("token_state:{:?}", token_state);
            let extensions = get_account_extensions(&token_state);
            println!("token_extensions:{:#?}", extensions);
        }
        CommandsName::POperation => {
            let (operation_account_key, __bump) = Pubkey::find_program_address(
                &[raydium_amm_v3::states::OPERATION_SEED.as_bytes()],
                &program.id(),
            );
            println!("{}", operation_account_key);
            let operation_account: raydium_amm_v3::states::OperationState =
                program.account(operation_account_key)?;
            println!("{:#?}", operation_account);
        }
        CommandsName::PObservation => {
            let pool: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;
            println!("{}", pool.observation_key);
            let observation_account: raydium_amm_v3::states::ObservationState =
                program.account(pool.observation_key)?;
            println!("{:#?}", observation_account);
        }
        CommandsName::PConfig { config_index } => {
            let (amm_config_key, __bump) = Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::AMM_CONFIG_SEED.as_bytes(),
                    &config_index.to_be_bytes(),
                ],
                &program.id(),
            );
            println!("{}", amm_config_key);
            let amm_config_account: raydium_amm_v3::states::AmmConfig =
                program.account(amm_config_key)?;
            println!("{:#?}", amm_config_account);
        }
        CommandsName::PriceToTick { price } => {
            println!("price:{}, tick:{}", price, price_to_tick(price));
        }
        CommandsName::TickToPrice { tick } => {
            println!("tick:{}, price:{}", tick, tick_to_price(tick));
        }
        CommandsName::TickWithSpacing { tick, tick_spacing } => {
            println!(
                "tick:{}, tick_spacing:{}, tick_with_spacing:{}",
                tick,
                tick_spacing,
                tick_with_spacing(tick, tick_spacing as i32)
            );
        }
        CommandsName::TickArraryStartIndex { tick, tick_spacing } => {
            println!(
                "tick:{}, tick_spacing:{},tick_array_start_index:{}",
                tick,
                tick_spacing,
                raydium_amm_v3::states::TickArrayState::get_array_start_index(tick, tick_spacing,)
            );
        }
        CommandsName::LiquidityToAmounts {
            tick_lower,
            tick_upper,
            liquidity,
        } => {
            let pool_account: raydium_amm_v3::states::PoolState =
                program.account(pool_config.pool_id_account.unwrap())?;
            let amounts = raydium_amm_v3::libraries::get_delta_amounts_signed(
                pool_account.tick_current,
                pool_account.sqrt_price_x64,
                tick_lower,
                tick_upper,
                liquidity,
            )?;
            println!("amount_0:{}, amount_1:{}", amounts.0, amounts.1);
        }
        CommandsName::PPersonalPositionByPool { pool_id } => {
            let pool_id = if let Some(pool_id) = pool_id {
                pool_id
            } else {
                pool_config.pool_id_account.unwrap()
            };
            println!("pool_id:{}", pool_id);
            let position_accounts_by_pool = rpc_client.get_program_accounts_with_config(
                &pool_config.raydium_v3_program,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            8 + 1 + size_of::<Pubkey>(),
                            &pool_id.to_bytes(),
                        )),
                        RpcFilterType::DataSize(
                            raydium_amm_v3::states::PersonalPositionState::LEN as u64,
                        ),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        ..RpcAccountInfoConfig::default()
                    },
                    with_context: Some(false),
                },
            )?;

            let mut total_fees_owed_0 = 0;
            let mut total_fees_owed_1 = 0;
            let mut total_reward_owed = 0;
            for position in position_accounts_by_pool {
                let personal_position = deserialize_anchor_account::<
                    raydium_amm_v3::states::PersonalPositionState,
                >(&position.1)?;
                if personal_position.pool_id == pool_id {
                    println!(
                        "personal_position:{}, lower:{}, upper:{}, liquidity:{}, token_fees_owed_0:{}, token_fees_owed_1:{}, reward_amount_owed:{}, fee_growth_inside:{}, fee_growth_inside_1:{}, reward_inside:{}",
                        position.0,
                        personal_position.tick_lower_index,
                        personal_position.tick_upper_index,
                        personal_position.liquidity,
                        personal_position.token_fees_owed_0,
                        personal_position.token_fees_owed_1,
                        personal_position.reward_infos[0].reward_amount_owed,
                        personal_position.fee_growth_inside_0_last_x64,
                        personal_position.fee_growth_inside_1_last_x64,
                        personal_position.reward_infos[0].growth_inside_last_x64,
                    );
                    total_fees_owed_0 += personal_position.token_fees_owed_0;
                    total_fees_owed_1 += personal_position.token_fees_owed_1;
                    total_reward_owed += personal_position.reward_infos[0].reward_amount_owed;
                }
            }
            println!(
                "total_fees_owed_0:{}, total_fees_owed_1:{}, total_reward_owed:{}",
                total_fees_owed_0, total_fees_owed_1, total_reward_owed
            );
        }
        CommandsName::PProtocolPositionByPool { pool_id } => {
            let pool_id = if let Some(pool_id) = pool_id {
                pool_id
            } else {
                pool_config.pool_id_account.unwrap()
            };
            println!("pool_id:{}", pool_id);
            let position_accounts_by_pool = rpc_client.get_program_accounts_with_config(
                &pool_config.raydium_v3_program,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            8 + 1,
                            &pool_id.to_bytes(),
                        )),
                        RpcFilterType::DataSize(
                            raydium_amm_v3::states::ProtocolPositionState::LEN as u64,
                        ),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64Zstd),
                        ..RpcAccountInfoConfig::default()
                    },
                    with_context: Some(false),
                },
            )?;

            for position in position_accounts_by_pool {
                let protocol_position = deserialize_anchor_account::<
                    raydium_amm_v3::states::ProtocolPositionState,
                >(&position.1)?;
                if protocol_position.pool_id == pool_id {
                    println!(
                        "protocol_position:{} lower_index:{}, upper_index:{}, liquidity:{}",
                        position.0,
                        protocol_position.tick_lower_index,
                        protocol_position.tick_upper_index,
                        protocol_position.liquidity,
                    );
                }
            }
        }
        CommandsName::PTickArrayByPool { pool_id } => {
            let pool_id = if let Some(pool_id) = pool_id {
                pool_id
            } else {
                pool_config.pool_id_account.unwrap()
            };
            println!("pool_id:{}", pool_id);
            let tick_arrays_by_pool = rpc_client.get_program_accounts_with_config(
                &pool_config.raydium_v3_program,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, &pool_id.to_bytes())),
                        RpcFilterType::DataSize(raydium_amm_v3::states::TickArrayState::LEN as u64),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64Zstd),
                        ..RpcAccountInfoConfig::default()
                    },
                    with_context: Some(false),
                },
            )?;

            for tick_array in tick_arrays_by_pool {
                let tick_array_state = deserialize_anchor_account::<
                    raydium_amm_v3::states::TickArrayState,
                >(&tick_array.1)?;
                if tick_array_state.pool_id == pool_id {
                    println!(
                        "tick_array:{}, {}, {}",
                        tick_array.0,
                        identity(tick_array_state.start_tick_index),
                        identity(tick_array_state.initialized_tick_count)
                    );
                    for tick_state in tick_array_state.ticks {
                        if tick_state.liquidity_gross != 0 {
                            println!("{:#?}", tick_state);
                        }
                    }
                }
            }
        }
        CommandsName::PPool { pool_id } => {
            let pool_id = if let Some(pool_id) = pool_id {
                pool_id
            } else {
                pool_config.pool_id_account.unwrap()
            };
            println!("pool_id:{}", pool_id);
            let pool_account: raydium_amm_v3::states::PoolState = program.account(pool_id)?;
            println!("{:#?}", pool_account);
        }
        CommandsName::PBitmapExtension { bitmap_extension } => {
            let bitmap_extension = if let Some(bitmap_extension) = bitmap_extension {
                bitmap_extension
            } else {
                pool_config.tickarray_bitmap_extension.unwrap()
            };
            println!("bitmap_extension:{}", bitmap_extension);
            let bitmap_extension_account: raydium_amm_v3::states::TickArrayBitmapExtension =
                program.account(bitmap_extension)?;
            println!("{:#?}", bitmap_extension_account);
        }
        CommandsName::PProtocol { protocol_id } => {
            let protocol_account: raydium_amm_v3::states::ProtocolPositionState =
                program.account(protocol_id)?;
            println!("{:#?}", protocol_account);
        }
        CommandsName::PPersonal { personal_id } => {
            let personal_account: raydium_amm_v3::states::PersonalPositionState =
                program.account(personal_id)?;
            println!("{:#?}", personal_account);
        }
        CommandsName::DecodeInstruction { instr_hex_data } => {
            handle_program_instruction(&instr_hex_data, InstructionDecodeType::BaseHex)?;
        }
        CommandsName::DecodeEvent { log_event } => {
            handle_program_log(
                &pool_config.raydium_v3_program.to_string(),
                &log_event,
                false,
            )?;
        }
        CommandsName::DecodeTxLog { tx_id } => {
            let signature = Signature::from_str(&tx_id)?;
            let tx = rpc_client.get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(0),
                },
            )?;
            let transaction = tx.transaction;
            // get meta
            let meta = if transaction.meta.is_some() {
                transaction.meta
            } else {
                None
            };
            // get encoded_transaction
            let encoded_transaction = transaction.transaction;
            // decode instruction data
            parse_program_instruction(
                &pool_config.raydium_v3_program.to_string(),
                encoded_transaction,
                meta.clone(),
            )?;
            // decode logs
            parse_program_event(&pool_config.raydium_v3_program.to_string(), meta.clone())?;
        }
    }

    Ok(())
}
