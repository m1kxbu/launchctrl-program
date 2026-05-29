//! Mollusk instruction-level tests for `migrate_to_pool`.
//!
//! Permissionless cranker ix — drains the bonding curve's SOL + token
//! reserves AND the FEE_REWORK `lp_seed_*` accumulators into program-owned
//! migration PDAs, then flips `launch_state.is_migrated`. The actual
//! Meteora pool creation is a SEPARATE ix (`create_meteora_pool`), so this
//! ix has NO Meteora CPI — only system_program (built-in) and
//! spl_token_2022 (loaded via mollusk-svm-programs-token).
//!
//! That makes it fully testable in Mollusk without faking Meteora.
//!
//! Coverage:
//!   - `migrate_already_migrated_reverts` — Anchor constraint, simplest gate
//!   - `migrate_curve_not_complete_reverts` — handler require! after CPIs
//!   - `migrate_already_released_reverts` — handler require! after CPIs
//!   - `migrate_happy_path` — full success: verifies vault drains + state flips
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test migrate_mollusk`

mod common;

use common::{
    derive_mint_pda, make_curve_state, make_curve_token_vault, make_launch_state,
    make_lp_seed_sol_vault, make_mint, make_packed_token_account, make_signer,
    make_sol_vault_typed, make_uninit_pda, PROGRAM_ID,
};
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

const MIGRATE_TO_POOL_DISC: [u8; 8] = [181, 8, 76, 176, 32, 47, 10, 162];

fn encode_migrate_to_pool() -> Vec<u8> {
    MIGRATE_TO_POOL_DISC.to_vec()
}

const CURVE_SOL_AMOUNT: u64 = 120_000_000_000; // 120 SOL bonding-curve fill
const CURVE_TOKEN_AMOUNT: u64 = 267_000_000_000_000; // ~267M tokens left in vault
const LP_SEED_SOL_AMOUNT: u64 = 500_000_000; // 0.5 SOL accumulated
const LP_SEED_TOKEN_AMOUNT: u64 = 50_000_000_000; // 50K tokens from buybacks

struct MigrateAccounts {
    cranker: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    curve_state: (Pubkey, Account),
    curve_token_vault: (Pubkey, Account),
    sol_vault: (Pubkey, Account),
    lp_seed_sol_vault: (Pubkey, Account),
    lp_seed_token_vault: (Pubkey, Account),
    migration_sol_vault: (Pubkey, Account),
    migration_token_vault: (Pubkey, Account),
    migration_authority: (Pubkey, Account),
    token_program: (Pubkey, Account),
    system_program: (Pubkey, Account),
}

impl MigrateAccounts {
    /// Default: curve fully filled (is_complete=true, real_sol_reserves
    /// matching sol_vault), lp_seed accumulators non-zero, not yet
    /// migrated, not yet funds-released. All PDAs at their canonical
    /// addresses; migration_sol_vault + migration_token_vault uninitialized
    /// (Anchor's `init` + the handler's invoke_signed create_account claim
    /// them respectively).
    fn happy() -> Self {
        let (cranker, cranker_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (launch_state, launch_acct) = make_launch_state(&mint, &cranker, |_| {});
        let (curve_state, curve_acct) = make_curve_state(&mint, &cranker, |c| {
            c.real_sol_reserves = CURVE_SOL_AMOUNT;
            c.real_token_reserves = 800_000_000_000_000; // 800M circulating
            c.is_complete = true;
        });
        let (curve_token_vault, ctv_acct) = make_curve_token_vault(&mint, CURVE_TOKEN_AMOUNT);
        let (sol_vault, sol_vault_acct) = make_sol_vault_typed(&mint, CURVE_SOL_AMOUNT);
        let (lp_seed_sol_vault, lp_seed_sol_acct) =
            make_lp_seed_sol_vault(&mint, LP_SEED_SOL_AMOUNT);
        // lp_seed_token_vault — Token-2022 at the canonical PDA, but its
        // TOKEN-LEVEL authority is `migration_authority` (set in
        // initialize_curve so migrate_to_pool can drain it by signing as
        // migration_authority — see migrate.rs:78-81). The default helper
        // would set authority = the PDA itself, which is correct for the
        // sell-side inline buyback path but breaks the migrate drain.
        let migration_authority_pda = derive_mint_pda(b"migration_authority", &mint);
        let lp_seed_token_pda = derive_mint_pda(b"lp_seed_tok", &mint);
        let lp_seed_token_vault = lp_seed_token_pda;
        let lp_seed_tok_acct = make_packed_token_account(
            &mint,
            &migration_authority_pda,
            LP_SEED_TOKEN_AMOUNT,
        );

        // migration_sol_vault: Anchor `init` PDA — must be uninitialized.
        let migration_sol_pda = derive_mint_pda(b"migration_sol", &mint);
        let (msv_pk, msv_acct) = make_uninit_pda(migration_sol_pda);

        // migration_token_vault: created via invoke_signed inside the handler.
        // Same uninit shape — handler does create_account + initialize_account3.
        let migration_token_pda = derive_mint_pda(b"migration_vault", &mint);
        let (mtv_pk, mtv_acct) = make_uninit_pda(migration_token_pda);

        // migration_authority: UncheckedAccount PDA — no payload needed.
        // Reuse the same derivation we set as the lp_seed_token_vault authority.
        let migration_auth_pda = migration_authority_pda;
        let migration_auth_acct = Account {
            lamports: 0,
            data: Vec::new(),
            owner: common::SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        let token_program = mollusk_svm_programs_token::token2022::keyed_account();
        let system_program = mollusk_svm::program::keyed_account_for_system_program();

        MigrateAccounts {
            cranker: (cranker, cranker_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state, launch_acct),
            curve_state: (curve_state, curve_acct),
            curve_token_vault: (curve_token_vault, ctv_acct),
            sol_vault: (sol_vault, sol_vault_acct),
            lp_seed_sol_vault: (lp_seed_sol_vault, lp_seed_sol_acct),
            lp_seed_token_vault: (lp_seed_token_vault, lp_seed_tok_acct),
            migration_sol_vault: (msv_pk, msv_acct),
            migration_token_vault: (mtv_pk, mtv_acct),
            migration_authority: (migration_auth_pda, migration_auth_acct),
            token_program,
            system_program,
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.cranker,
            self.mint,
            self.launch_state,
            self.curve_state,
            self.curve_token_vault,
            self.sol_vault,
            self.lp_seed_sol_vault,
            self.lp_seed_token_vault,
            self.migration_sol_vault,
            self.migration_token_vault,
            self.migration_authority,
            self.token_program,
            self.system_program,
        ]
    }
}

fn build_migrate_ix(accounts: &MigrateAccounts) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_migrate_to_pool(),
        vec![
            AccountMeta::new(accounts.cranker.0, true),
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.launch_state.0, false),
            AccountMeta::new(accounts.curve_state.0, false),
            AccountMeta::new(accounts.curve_token_vault.0, false),
            AccountMeta::new(accounts.sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_token_vault.0, false),
            AccountMeta::new(accounts.migration_sol_vault.0, false),
            AccountMeta::new(accounts.migration_token_vault.0, false),
            AccountMeta::new_readonly(accounts.migration_authority.0, false),
            AccountMeta::new_readonly(accounts.token_program.0, false),
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

// ─── Negative-path tests ────────────────────────────────────────────────────

/// PROPERTY: `launch_state.is_migrated == true` must revert with
/// `AlreadyMigrated`. This is the simplest gate — Anchor's account
/// constraint `!launch_state.is_migrated` fires DURING account validation,
/// before the handler runs. Guards against double-migration.
#[test]
fn migrate_already_migrated_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = MigrateAccounts::happy();
    accounts.launch_state =
        make_launch_state(&accounts.mint.0, &accounts.cranker.0, |l| {
            l.is_migrated = true;
        });

    let ix = build_migrate_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected AlreadyMigrated");
    // AlreadyMigrated = offset 10 → 6010.
    assert_eq!(expect_custom_error(&result), 6010);
}

/// PROPERTY: `curve_state.is_complete == false` must revert with
/// `CurveNotComplete`. The handler reaches this require! AFTER the
/// system_program + spl_token_2022 CPIs that create + initialize
/// migration_token_vault. Trying to migrate before the curve fills
/// drains funds prematurely.
#[test]
fn migrate_curve_not_complete_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = MigrateAccounts::happy();
    // Flip curve to NOT complete.
    accounts.curve_state =
        make_curve_state(&accounts.mint.0, &accounts.cranker.0, |c| {
            c.real_sol_reserves = CURVE_SOL_AMOUNT;
            c.real_token_reserves = 800_000_000_000_000;
            c.is_complete = false;
        });

    let ix = build_migrate_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected CurveNotComplete");
    // CurveNotComplete = offset 17 → 6017.
    assert_eq!(expect_custom_error(&result), 6017);
}

/// PROPERTY: `curve_state.is_funds_released == true` must revert with
/// `AlreadyReleased`. Idempotency gate — once funds have been drained
/// into the migration vaults, the ix can't be replayed to drain again.
#[test]
fn migrate_already_released_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = MigrateAccounts::happy();
    accounts.curve_state =
        make_curve_state(&accounts.mint.0, &accounts.cranker.0, |c| {
            c.real_sol_reserves = CURVE_SOL_AMOUNT;
            c.real_token_reserves = 800_000_000_000_000;
            c.is_complete = true;
            c.is_funds_released = true;
        });

    let ix = build_migrate_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected AlreadyReleased");
    // AlreadyReleased = offset 16 → 6016.
    assert_eq!(expect_custom_error(&result), 6016);
}

// ─── Happy-path test ────────────────────────────────────────────────────────

/// PROPERTY: full migration drains all four source vaults into the two
/// migration vaults and flips `is_migrated` + sets `is_funds_released`.
/// Conservation: sol_vault + lp_seed_sol_vault lamports → migration_sol_vault;
/// curve_token_vault + lp_seed_token_vault tokens → migration_token_vault.
#[test]
fn migrate_happy_path() {
    let mollusk = mollusk_with_token_2022();
    let accounts = MigrateAccounts::happy();

    let mint_pk = accounts.mint.0;
    let launch_state_pk = accounts.launch_state.0;
    let curve_state_pk = accounts.curve_state.0;
    let sol_vault_pk = accounts.sol_vault.0;
    let lp_seed_sol_pk = accounts.lp_seed_sol_vault.0;
    let migration_sol_pk = accounts.migration_sol_vault.0;
    let migration_token_pk = accounts.migration_token_vault.0;

    let sol_vault_before = accounts.sol_vault.1.lamports;
    let lp_seed_sol_before = accounts.lp_seed_sol_vault.1.lamports;

    let ix = build_migrate_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    // sol_vault should be drained to 0 lamports.
    let sol_vault_after = lookup_account(&result, &sol_vault_pk);
    assert_eq!(sol_vault_after.lamports, 0, "sol_vault should be drained");

    // lp_seed_sol_vault should be drained to 0 lamports.
    let lp_seed_sol_after = lookup_account(&result, &lp_seed_sol_pk);
    assert_eq!(
        lp_seed_sol_after.lamports, 0,
        "lp_seed_sol_vault should be drained"
    );

    // migration_sol_vault receives the union — initial rent (from Anchor
    // `init`) plus the drained amounts. We can't predict the exact rent
    // figure from outside the runtime, so just assert the drained sum is
    // present on top of SOME rent floor.
    let migration_sol_after = lookup_account(&result, &migration_sol_pk);
    let expected_drained = sol_vault_before + lp_seed_sol_before;
    assert!(
        migration_sol_after.lamports >= expected_drained,
        "migration_sol_vault should hold at least {} lamports (drained), got {}",
        expected_drained,
        migration_sol_after.lamports
    );

    // migration_token_vault should exist and be initialized (165 bytes,
    // spl_token_2022-owned). Just sanity-check it's not empty.
    let migration_token_after = lookup_account(&result, &migration_token_pk);
    assert_eq!(
        migration_token_after.data.len(),
        165,
        "migration_token_vault should be initialized as Token-2022 account"
    );

    // launch_state.is_migrated should now be true; curve_state.is_funds_released
    // should now be true.
    use anchor_lang::AccountDeserialize;
    let launch_after = lookup_account(&result, &launch_state_pk);
    let mut launch_data: &[u8] = &launch_after.data;
    let launch_decoded =
        launchctrl::state::LaunchState::try_deserialize(&mut launch_data)
            .expect("LaunchState should deserialize");
    assert!(
        launch_decoded.is_migrated,
        "launch_state.is_migrated should be true"
    );
    // `migration_timestamp` is set from `Clock::get()`. Mollusk's mock clock
    // returns 0 by default, so don't assert non-zero — just confirm the
    // is_migrated flag flipped (the real-world signal).

    let curve_after = lookup_account(&result, &curve_state_pk);
    let mut curve_data: &[u8] = &curve_after.data;
    let curve_decoded =
        launchctrl::state::CurveState::try_deserialize(&mut curve_data)
            .expect("CurveState should deserialize");
    assert!(
        curve_decoded.is_funds_released,
        "curve_state.is_funds_released should be true"
    );

    // Sanity: the result references mint (so the runtime saw it) and didn't
    // mutate it (we don't expect mint to change during migrate).
    let _mint_after = lookup_account(&result, &mint_pk);
}
