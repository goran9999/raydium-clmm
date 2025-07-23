use anchor_client::solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig},
    rpc_filter::{Memcmp, RpcFilterType},
    rpc_request::TokenAccountsFilter,
};
use anchor_client::solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    message::Message,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use anchor_client::{Client, Cluster};
use anchor_lang::prelude::AccountMeta;
use anyhow::{format_err, Result};
use arrayref::array_ref;
use clap::Parser;
use configparser::ini::Ini;
use solana_transaction_status::UiTransactionEncoding;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::{collections::VecDeque, convert::identity, mem::size_of};

mod instructions;
use bincode::serialize;
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
    anchor_client::solana_sdk::signature::read_keypair_file(s)
        .map_err(|_| format_err!("failed to read keypair from {}", s))
}
fn write_keypair_file(keypair: &Keypair, outfile: &str) -> Result<String> {
    anchor_client::solana_sdk::signature::write_keypair_file(keypair, outfile)
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
    let (_, mut current_valid_tick_array_start_index) = pool_state
        .get_first_initialized_tick_array(&Some(*tickarray_bitmap_extension), zero_for_one)
        .unwrap();
    let mut tick_array_keys = Vec::new();
    tick_array_keys.push(
        Pubkey::find_program_address(
            &[
                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                &current_valid_tick_array_start_index.to_be_bytes(),
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
                current_valid_tick_array_start_index,
                zero_for_one,
            )
            .unwrap();
        if next_tick_array_index.is_none() {
            break;
        }
        current_valid_tick_array_start_index = next_tick_array_index.unwrap();
        tick_array_keys.push(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    &current_valid_tick_array_start_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            )
            .0,
        );
        max_array_size -= 1;
    }
    let tick_array_rsps = rpc_client.get_multiple_accounts(&tick_array_keys).unwrap();
    let mut tick_arrays = VecDeque::new();
    for (index, tick_array) in tick_array_rsps.iter().enumerate() {
        let tick_array_state =
            deserialize_anchor_account::<raydium_amm_v3::states::TickArrayState>(
                &tick_array.clone().unwrap(),
            )
            .unwrap();
        tick_arrays.push_back(tick_array_state);
    }
    tick_arrays
}

pub fn load_cur_and_next_five_tick_array_keys(
    rpc_client: &RpcClient,
    pool_config: &ClientConfig,
    pool_state: &PoolState,
    tickarray_bitmap_extension: &TickArrayBitmapExtension,
    zero_for_one: bool,
) -> Vec<Pubkey> {
    let (_, mut current_valid_tick_array_start_index) = pool_state
        .get_first_initialized_tick_array(&Some(*tickarray_bitmap_extension), zero_for_one)
        .unwrap();
    let mut tick_array_keys = Vec::new();
    tick_array_keys.push(
        Pubkey::find_program_address(
            &[
                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                &current_valid_tick_array_start_index.to_be_bytes(),
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
                current_valid_tick_array_start_index,
                zero_for_one,
            )
            .unwrap();
        if next_tick_array_index.is_none() {
            break;
        }
        current_valid_tick_array_start_index = next_tick_array_index.unwrap();
        tick_array_keys.push(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    &current_valid_tick_array_start_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program,
            )
            .0,
        );
        max_array_size -= 1;
    }
    let tick_array_rsps = rpc_client.get_multiple_accounts(&tick_array_keys).unwrap();
    let mut tick_arrays = VecDeque::new();
    for (index, tick_array) in tick_array_rsps.iter().enumerate() {
        let tick_array_state =
            deserialize_anchor_account::<raydium_amm_v3::states::TickArrayState>(
                &tick_array.clone().unwrap(),
            )
            .unwrap();
        tick_arrays.push_back(tick_array_state);
    }
    tick_array_keys
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
    GetSupportmintPda {
        mint: Pubkey,
    },
}

fn main() {}
