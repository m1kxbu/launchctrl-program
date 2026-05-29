//! Mollusk instruction-level tests for `initialize_ricochet_config`.
//!
//! The simplest fund-flow-adjacent ix in the system: 5 accounts, one
//! `require!` gate, no CPIs. Optional per-mint Ricochet enablement —
//! replaces the standalone ricochet program's `initialize_mint_enforce`
//! pair.
//!
//! Coverage:
//!   - RicochetDurationOutOfRange (duration_seconds = 0 or > 86_400)
//!   - Unauthorized (signer != launch_state.creator)
//!   - Happy path (verifies expires_at = launch_timestamp + duration)
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test initialize_ricochet_config_mollusk`

mod common;

use anchor_lang::AccountDeserialize;
use common::{derive_mint_pda, make_launch_state, make_mint, make_signer, make_uninit_pda, PROGRAM_ID};
use launchctrl::state::RicochetConfig;
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

const INITIALIZE_RICOCHET_CONFIG_DISC: [u8; 8] = [143, 82, 23, 104, 28, 149, 88, 190];
const LAUNCH_TIMESTAMP: i64 = 1_700_000_000;

fn encode_initialize_ricochet_config(duration_seconds: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 4);
    data.extend_from_slice(&INITIALIZE_RICOCHET_CONFIG_DISC);
    data.extend_from_slice(&duration_seconds.to_le_bytes());
    data
}

struct InitRicochetAccounts {
    creator: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    ricochet_config: (Pubkey, Account),
    system_program: (Pubkey, Account),
}

impl InitRicochetAccounts {
    fn happy() -> Self {
        let (creator, creator_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (launch_state, launch_acct) =
            make_launch_state(&mint, &creator, |l| {
                l.launch_timestamp = LAUNCH_TIMESTAMP;
            });
        let (ricochet_config_pk, ricochet_config_acct) =
            make_uninit_pda(derive_mint_pda(b"ricochet_config", &mint));
        let system_program = mollusk_svm::program::keyed_account_for_system_program();

        InitRicochetAccounts {
            creator: (creator, creator_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state, launch_acct),
            ricochet_config: (ricochet_config_pk, ricochet_config_acct),
            system_program,
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.creator,
            self.mint,
            self.launch_state,
            self.ricochet_config,
            self.system_program,
        ]
    }
}

fn build_ix(accounts: &InitRicochetAccounts, duration_seconds: u32) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_initialize_ricochet_config(duration_seconds),
        vec![
            AccountMeta::new(accounts.creator.0, true),
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new_readonly(accounts.launch_state.0, false),
            AccountMeta::new(accounts.ricochet_config.0, false),
            AccountMeta::new_readonly(accounts.system_program.0, false),
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

/// PROPERTY: `duration_seconds == 0` must revert with
/// `RicochetDurationOutOfRange`. Defends against an "empty window"
/// config that lets the per-mint Ricochet enforcement appear enabled but
/// expire instantly (i.e. never applies to any buy/sell).
#[test]
fn init_ricochet_zero_duration_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitRicochetAccounts::happy();

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected RicochetDurationOutOfRange"
    );
    // RicochetDurationOutOfRange = offset 40 → 6040.
    assert_eq!(expect_custom_error(&result), 6040);
}

/// PROPERTY: `duration_seconds > MAX_DURATION_SECONDS (86_400)` must
/// revert with `RicochetDurationOutOfRange`. Sanity cap against typo'd
/// inputs that would persist enforcement past migration.
#[test]
fn init_ricochet_too_long_duration_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitRicochetAccounts::happy();

    let ix = build_ix(&accounts, 86_401);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        result.program_result.is_err(),
        "expected RicochetDurationOutOfRange"
    );
    assert_eq!(expect_custom_error(&result), 6040);
}

/// PROPERTY: signer != `launch_state.creator` must revert with
/// `Unauthorized`. Defends against an unrelated wallet enabling Ricochet
/// on someone else's mint (which would degrade their UX by adding the
/// allowlist check to every buy/sell).
#[test]
fn init_ricochet_wrong_creator_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = InitRicochetAccounts::happy();
    // Replace signer with a different pubkey — won't match
    // launch_state.creator.
    accounts.creator = make_signer();

    let ix = build_ix(&accounts, 600);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected Unauthorized");
    // Unauthorized = offset 26 → 6026.
    assert_eq!(expect_custom_error(&result), 6026);
}

/// PROPERTY: full happy path — duration in bounds + correct signer.
/// Verifies the RicochetConfig PDA is created and `expires_at` is
/// computed as `launch_timestamp + duration_seconds`.
#[test]
fn init_ricochet_happy_path() {
    let mollusk = mollusk_with_token_2022();
    let accounts = InitRicochetAccounts::happy();

    let mint_pk = accounts.mint.0;
    let ricochet_config_pk = accounts.ricochet_config.0;
    const DURATION: u32 = 3600; // 1 hour

    let ix = build_ix(&accounts, DURATION);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    // Decode the post-ix RicochetConfig and verify fields.
    let ricochet_after = lookup_account(&result, &ricochet_config_pk);
    let mut data: &[u8] = &ricochet_after.data;
    let cfg = RicochetConfig::try_deserialize(&mut data)
        .expect("RicochetConfig should deserialize from post-ix data");

    assert_eq!(cfg.mint, mint_pk, "config.mint should match the mint");
    assert_eq!(
        cfg.expires_at,
        LAUNCH_TIMESTAMP + DURATION as i64,
        "expires_at should equal launch_timestamp + duration_seconds"
    );
}
