//! Mollusk instruction-level tests for `initialize_launch`.
//!
//! `initialize_launch` is the parameter-validation choke point for every
//! new launch. It's the simplest fund-flow ix (5 accounts) but has the
//! richest pre-CPI parameter validation in the system — including the
//! M-2 audit finding's decay-schedule bounds.
//!
//! Coverage:
//!   - NameTooLong / SymbolTooLong / UriTooLong / InvalidSupply
//!     (4 input-validation gates)
//!   - MigrationThresholdOutOfRange (creator-supplied threshold bounds)
//!   - InvalidDecayBps (M-2 — `fee_bps > 5000` rejected)
//!   - InvalidDecaySchedule (M-2 — non-strictly-increasing rejected)
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test initialize_launch_mollusk`

mod common;

use anchor_lang::AnchorSerialize;
use common::{derive_mint_pda, make_mint, make_signer, make_uninit_pda, PROGRAM_ID};
use launchctrl::state::{DecayStep, LaunchParams};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;
use solana_pubkey::Pubkey;

fn mollusk_with_token_2022() -> Mollusk {
    let mut mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    mollusk
}

const INITIALIZE_LAUNCH_DISC: [u8; 8] = [90, 201, 220, 142, 112, 253, 100, 13];

fn encode_initialize_launch(params: &LaunchParams) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&INITIALIZE_LAUNCH_DISC);
    params
        .serialize(&mut data)
        .expect("LaunchParams Borsh serialize cannot fail");
    data
}

/// Sane default params — well within all bounds. Each test deviates one
/// field to trip a specific gate.
fn happy_params() -> LaunchParams {
    LaunchParams {
        name: "Test Token".to_string(),
        symbol: "TST".to_string(),
        uri: "https://example.com/meta.json".to_string(),
        total_supply: 1_000_000_000_000_000,
        decimals: 6,
        migration_threshold_lamports: 120_000_000_000, // 120 SOL — within bounds
        decay_schedule: vec![
            DecayStep { seconds_after_launch: 0, fee_bps: 1000 },
            DecayStep { seconds_after_launch: 300, fee_bps: 500 },
            DecayStep { seconds_after_launch: 600, fee_bps: 100 },
        ],
    }
}

struct InitLaunchAccounts {
    creator: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    system_program: (Pubkey, Account),
    token_program: (Pubkey, Account),
}

impl InitLaunchAccounts {
    fn happy() -> Self {
        let (creator, creator_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 0); // Mint supply not relevant pre-CPI
        let launch_state_pda = derive_mint_pda(b"launch", &mint);
        let (launch_state_pk, launch_state_acct) = make_uninit_pda(launch_state_pda);
        let system_program = mollusk_svm::program::keyed_account_for_system_program();
        let token_program = mollusk_svm_programs_token::token2022::keyed_account();

        InitLaunchAccounts {
            creator: (creator, creator_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state_pk, launch_state_acct),
            system_program,
            token_program,
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.creator,
            self.mint,
            self.launch_state,
            self.system_program,
            self.token_program,
        ]
    }
}

fn build_ix(accounts: &InitLaunchAccounts, params: &LaunchParams) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_initialize_launch(params),
        vec![
            AccountMeta::new(accounts.creator.0, true),
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.launch_state.0, false),
            AccountMeta::new_readonly(accounts.system_program.0, false),
            AccountMeta::new_readonly(accounts.token_program.0, false),
        ],
    )
}

fn expect_custom_error(result: &mollusk_svm::result::InstructionResult) -> u32 {
    match &result.program_result {
        mollusk_svm::result::ProgramResult::Failure(ProgramError::Custom(code)) => *code,
        other => panic!("expected ProgramError::Custom, got {:?}", other),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn init_launch_name_too_long_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.name = "x".repeat(33); // 33 > 32 limit

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected NameTooLong");
    // NameTooLong = offset 0 → 6000.
    assert_eq!(expect_custom_error(&result), 6000);
}

#[test]
fn init_launch_symbol_too_long_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.symbol = "y".repeat(11); // 11 > 10 limit

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected SymbolTooLong");
    assert_eq!(expect_custom_error(&result), 6001);
}

#[test]
fn init_launch_uri_too_long_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.uri = "z".repeat(201); // 201 > 200 limit

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected UriTooLong");
    assert_eq!(expect_custom_error(&result), 6002);
}

#[test]
fn init_launch_zero_supply_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.total_supply = 0;

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected InvalidSupply");
    assert_eq!(expect_custom_error(&result), 6003);
}

/// PROPERTY: `migration_threshold_lamports` below the minimum bound must
/// revert. The bounds defend against creator-griefing: an absurdly low
/// threshold could be hit by a tiny test buy before genuine demand,
/// triggering premature migration to a dead pool.
#[test]
fn init_launch_migration_threshold_too_low_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.migration_threshold_lamports = 1; // below MIN

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected MigrationThresholdOutOfRange"
    );
    // MigrationThresholdOutOfRange = offset 9 → 6009.
    assert_eq!(expect_custom_error(&result), 6009);
}

/// PROPERTY: a `DecayStep` with `fee_bps > MAX_DECAY_FEE_BPS (5000)` must
/// revert with `InvalidDecayBps`. This is the M-2 audit finding's sell-
/// side rug-prevention cap. Without it, a creator could set a 100% sell
/// fee that effectively traps every holder.
#[test]
fn init_launch_decay_bps_too_high_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    // Set one step's fee_bps above the 5000 cap.
    params.decay_schedule = vec![DecayStep {
        seconds_after_launch: 0,
        fee_bps: 5001, // 50.01% — just over the cap
    }];

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected InvalidDecayBps");
    // InvalidDecayBps = offset 7 → 6007.
    assert_eq!(expect_custom_error(&result), 6007);
}

/// PROPERTY: a decay schedule with `seconds_after_launch` NOT strictly
/// increasing must revert with `InvalidDecaySchedule`. Sibling M-2 check.
/// Without strict ordering, sell.rs's `current_decay_bps` walker (which
/// breaks at the first future step) silently skips out-of-order entries.
#[test]
fn init_launch_decay_schedule_non_monotonic_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitLaunchAccounts::happy();
    let mut params = happy_params();
    params.decay_schedule = vec![
        DecayStep { seconds_after_launch: 0, fee_bps: 1000 },
        DecayStep { seconds_after_launch: 100, fee_bps: 500 },
        DecayStep { seconds_after_launch: 100, fee_bps: 100 }, // EQUAL — must be strictly >
    ];

    let ix = build_ix(&accounts, &params);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected InvalidDecaySchedule"
    );
    // InvalidDecaySchedule = offset 8 → 6008.
    assert_eq!(expect_custom_error(&result), 6008);
}
