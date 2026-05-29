//! Mollusk instruction-level tests for `initialize_curve`.
//!
//! Sets up the per-mint bonding curve scaffolding immediately after
//! `initialize_launch`: curve_state, sol_vault, lp_seed_sol_vault,
//! blocklist (all Anchor-`init`), plus curve_token_vault and
//! lp_seed_token_vault (handler manually create_account's via
//! invoke_signed to stay under the BPF 4 KiB stack limit).
//!
//! The single in-handler require! tests the
//! `migration_threshold_lamports` bounds — same defense-in-depth gate
//! mirrored from initialize_launch. CurveState is the source-of-truth
//! for the migration trigger (buy.rs reads this field), so this gate is
//! the LOAD-BEARING one.
//!
//! Coverage:
//!   - MigrationThresholdOutOfRange (too-low + too-high)
//!   - Happy path (verifies CurveState + Blocklist init via AccountDeserialize)
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test initialize_curve_mollusk`

mod common;

use anchor_lang::AccountDeserialize;
use common::{
    derive_mint_pda, make_mint, make_signer, make_uninit_pda, PROGRAM_ID,
};
use launchctrl::state::{Blocklist, CurveState};
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

const INITIALIZE_CURVE_DISC: [u8; 8] = [170, 84, 186, 253, 131, 149, 95, 213];

fn encode_initialize_curve(migration_threshold_lamports: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 8);
    data.extend_from_slice(&INITIALIZE_CURVE_DISC);
    data.extend_from_slice(&migration_threshold_lamports.to_le_bytes());
    data
}

struct InitCurveAccounts {
    creator: (Pubkey, Account),
    mint: (Pubkey, Account),
    curve_state: (Pubkey, Account),
    curve_token_vault: (Pubkey, Account),
    sol_vault: (Pubkey, Account),
    lp_seed_sol_vault: (Pubkey, Account),
    lp_seed_token_vault: (Pubkey, Account),
    blocklist: (Pubkey, Account),
    system_program: (Pubkey, Account),
    token_program: (Pubkey, Account),
}

impl InitCurveAccounts {
    fn happy() -> Self {
        let (creator, creator_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);

        // All six init/created-inside PDAs start uninitialized — empty
        // system-owned accounts at the canonical addresses. Anchor's `init`
        // + the handler's invoke_signed create_account fill them in.
        let curve_state = make_uninit_pda(derive_mint_pda(b"curve", &mint));
        let curve_token_vault = make_uninit_pda(derive_mint_pda(b"curve_vault", &mint));
        let sol_vault = make_uninit_pda(derive_mint_pda(b"sol_vault", &mint));
        let lp_seed_sol_vault = make_uninit_pda(derive_mint_pda(b"lp_seed_sol", &mint));
        let lp_seed_token_vault = make_uninit_pda(derive_mint_pda(b"lp_seed_tok", &mint));
        let blocklist = make_uninit_pda(derive_mint_pda(b"blocklist", &mint));

        let system_program = mollusk_svm::program::keyed_account_for_system_program();
        let token_program = mollusk_svm_programs_token::token2022::keyed_account();

        InitCurveAccounts {
            creator: (creator, creator_acct),
            mint: (mint, mint_acct),
            curve_state,
            curve_token_vault,
            sol_vault,
            lp_seed_sol_vault,
            lp_seed_token_vault,
            blocklist,
            system_program,
            token_program,
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.creator,
            self.mint,
            self.curve_state,
            self.curve_token_vault,
            self.sol_vault,
            self.lp_seed_sol_vault,
            self.lp_seed_token_vault,
            self.blocklist,
            self.system_program,
            self.token_program,
        ]
    }
}

fn build_ix(accounts: &InitCurveAccounts, migration_threshold_lamports: u64) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_initialize_curve(migration_threshold_lamports),
        vec![
            AccountMeta::new(accounts.creator.0, true),
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.curve_state.0, false),
            AccountMeta::new(accounts.curve_token_vault.0, false),
            AccountMeta::new(accounts.sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_token_vault.0, false),
            AccountMeta::new(accounts.blocklist.0, false),
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

fn lookup_account<'a>(
    result: &'a mollusk_svm::result::InstructionResult,
    pubkey: &Pubkey,
) -> &'a Account {
    result
        .resulting_accounts
        .iter()
        .find(|(pk, _)| pk == pubkey)
        .map(|(_, a)| a)
        .expect("account not found in resulting_accounts")
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// PROPERTY: `migration_threshold_lamports` below the MIN bound (non-zero)
/// must revert. Defense-in-depth: CurveState is the source-of-truth for
/// the migration trigger (buy.rs reads it), so this gate is the
/// LOAD-BEARING one. A zero value would default to
/// DEFAULT_MIGRATION_THRESHOLD_LAMPORTS, so we use a small non-zero value.
#[test]
fn init_curve_threshold_too_low_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitCurveAccounts::happy();

    let ix = build_ix(&accounts, 1); // 1 lamport — way below MIN
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected MigrationThresholdOutOfRange"
    );
    // MigrationThresholdOutOfRange = offset 9 → 6009.
    assert_eq!(expect_custom_error(&result), 6009);
}

/// PROPERTY: `migration_threshold_lamports` above the MAX bound (200 SOL)
/// must revert. Without this, a creator could set the threshold so high
/// that the curve never migrates, trapping buyers.
#[test]
fn init_curve_threshold_too_high_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitCurveAccounts::happy();

    // 1000 SOL — well above the 200 SOL cap.
    let ix = build_ix(&accounts, 1_000_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected MigrationThresholdOutOfRange"
    );
    assert_eq!(expect_custom_error(&result), 6009);
}

/// PROPERTY: with a valid threshold + all-uninit PDAs + proper mint,
/// initialize_curve runs end-to-end through Anchor's 4 inits + the
/// handler's 2 manual create_account + 2 initialize_account3 CPIs.
/// Verifies the resulting CurveState + Blocklist via Borsh
/// deserialization.
#[test]
fn init_curve_happy_path() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitCurveAccounts::happy();

    let mint_pk = accounts.mint.0;
    let creator_pk = accounts.creator.0;
    let curve_state_pk = accounts.curve_state.0;
    let blocklist_pk = accounts.blocklist.0;

    const THRESHOLD: u64 = 120_000_000_000; // 120 SOL — well within bounds
    let ix = build_ix(&accounts, THRESHOLD);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    // CurveState fields populated correctly.
    let curve_after = lookup_account(&result, &curve_state_pk);
    let mut curve_data: &[u8] = &curve_after.data;
    let curve = CurveState::try_deserialize(&mut curve_data)
        .expect("CurveState should deserialize from post-ix data");
    assert_eq!(curve.mint, mint_pk);
    assert_eq!(curve.creator, creator_pk);
    assert_eq!(curve.migration_threshold_lamports, THRESHOLD);
    assert!(!curve.is_complete);
    assert!(!curve.is_funds_released);
    assert_eq!(curve.real_sol_reserves, 0);
    assert_eq!(curve.real_token_reserves, 0);

    // Blocklist initialized empty + unfrozen.
    let blocklist_after = lookup_account(&result, &blocklist_pk);
    let mut bl_data: &[u8] = &blocklist_after.data;
    let bl = Blocklist::try_deserialize(&mut bl_data)
        .expect("Blocklist should deserialize");
    assert_eq!(bl.mint, mint_pk);
    assert_eq!(bl.creator, creator_pk);
    assert!(bl.blocked.is_empty());
    assert!(!bl.frozen, "blocklist starts mutable; freezes on first buy");
}
