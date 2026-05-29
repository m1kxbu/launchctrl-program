//! Mollusk instruction-level tests for `claim_and_reinject` — the LP
//! flywheel crank.
//!
//! `claim_and_reinject` claims Meteora LP fees, reinjects ~62.5% via
//! `add_liquidity`, and raw-debits the residual 25% / 6.25% / 6.25% split
//! to reward_vault / deployer_vault / PLATFORM_FEE_VAULT. With 23 accounts
//! and two Meteora CPIs, it's the most account-heavy ix in the system.
//!
//! Tests here focus on the **Anchor account-validation gates** that fire
//! BEFORE the handler runs, before any CPI. These are the substitution
//! defenses — they prove that an attacker who passes the wrong meteora
//! program / pool authority / fee vault / etc. is rejected.
//!
//! Full happy-path coverage would require a richer Meteora stub that
//! mutates wsol_vault during `claim_position_fee` so the post-CPI
//! `wsol_consumed > 0` require! passes — deferred.
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test claim_and_reinject_mollusk`

mod common;

use common::{
    derive_mint_pda, make_deployer_vault, make_launch_state, make_lp_seed_token_vault, make_mint,
    make_packed_token_account, make_reward_vault, make_signer, PROGRAM_ID, SYSTEM_PROGRAM_ID,
};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;
use solana_pubkey::{pubkey, Pubkey};

fn mollusk_with_meteora_stub() -> Mollusk {
    let mut mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    // Classic SPL Token program — needed for WSOL vault lifecycle
    // (initialize_account3 + close_account CPIs inside reinject).
    mollusk_svm_programs_token::token::add_program(&mut mollusk);
    mollusk.add_program(&METEORA_DAMM_V2_PROGRAM, "meteora_stub");
    mollusk
}

/// Build a minimal-but-valid SPL classic Mint at the WSOL pubkey. Real WSOL
/// has decimals=9 and no mint/freeze authority. `initialize_account3` reads
/// the mint to validate decimals + initialized state; this gives it the
/// shape it expects without pulling in spl-token as a dev-dep.
fn wsol_mint_account() -> Account {
    // SPL Mint layout (82 bytes):
    //   mint_authority: COption<Pubkey>  bytes 0..36   (tag at 0..4, pubkey at 4..36)
    //   supply:         u64              bytes 36..44
    //   decimals:       u8               byte 44
    //   is_initialized: bool             byte 45
    //   freeze_authority: COption<Pubkey> bytes 46..82
    let mut data = vec![0u8; 82];
    data[44] = 9; // decimals
    data[45] = 1; // is_initialized = true
    Account {
        lamports: 1_000_000,
        data,
        owner: SPL_TOKEN_PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

const CLAIM_AND_REINJECT_DISC: [u8; 8] = [35, 140, 109, 177, 34, 188, 92, 190];

const PLATFORM_FEE_VAULT: Pubkey = pubkey!("2Do45QcuM3yvfes3Uc5PuD3BcsRZ29pq56LQWQczyDvi");
const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const SPL_TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const METEORA_DAMM_V2_PROGRAM: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
const METEORA_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
const METEORA_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");

fn encode_claim_and_reinject(liquidity_delta: u128) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 16);
    data.extend_from_slice(&CLAIM_AND_REINJECT_DISC);
    data.extend_from_slice(&liquidity_delta.to_le_bytes());
    data
}

fn empty_system_account_at(pubkey: Pubkey) -> (Pubkey, Account) {
    (
        pubkey,
        Account {
            lamports: 0,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

struct ReinjectAccounts {
    cranker: (Pubkey, Account),
    mint: (Pubkey, Account),
    wsol_mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    creator: (Pubkey, Account),
    creator_token_account: (Pubkey, Account),
    migration_authority: (Pubkey, Account),
    lp_seed_token_vault: (Pubkey, Account),
    wsol_vault: (Pubkey, Account),
    migration_sol_vault: (Pubkey, Account),
    reward_vault: (Pubkey, Account),
    deployer_vault: (Pubkey, Account),
    fee_vault: (Pubkey, Account),
    meteora_pool: (Pubkey, Account),
    meteora_position: (Pubkey, Account),
    position_nft_account: (Pubkey, Account),
    token_a_vault: (Pubkey, Account),
    token_b_vault: (Pubkey, Account),
    pool_authority: (Pubkey, Account),
    event_authority: (Pubkey, Account),
    meteora_program: (Pubkey, Account),
    token_2022_program: (Pubkey, Account),
    spl_token_program: (Pubkey, Account),
    system_program: (Pubkey, Account),
}

impl ReinjectAccounts {
    /// Default state: launch is migrated with a recorded meteora_pool,
    /// reward/deployer vaults exist, creator still holds initial buy. All
    /// Anchor-pinned pubkeys at their canonical addresses. Designed to pass
    /// every account-validation constraint so each negative test only
    /// has to deviate one thing.
    fn happy() -> Self {
        let (cranker, cranker_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (creator, creator_acct) = make_signer();

        // launch_state: is_migrated + has a non-default meteora_pool. The
        // migration_sol_vault_bump must match the canonical bump for the
        // Anchor seeds check to pass.
        let (_, migration_sol_bump) =
            Pubkey::find_program_address(&[b"migration_sol", mint.as_ref()], &PROGRAM_ID);
        let meteora_pool_pk = Pubkey::new_unique();
        let (launch_state, launch_acct) = make_launch_state(&mint, &creator, |l| {
            l.is_migrated = true;
            l.meteora_pool = meteora_pool_pk;
            l.migration_sol_vault_bump = migration_sol_bump;
            l.initial_buy_amount = 100_000_000; // 100 tokens — for dev-bonus eligibility branch
        });

        // creator_token_account: Token-2022 ATA owned by creator, holding
        // at least initial_buy_amount (so the eligibility branch sees the
        // creator as still-holding).
        let (creator_ata, creator_ata_acct) =
            (Pubkey::new_unique(), make_packed_token_account(&mint, &creator, 200_000_000));

        let migration_authority_pda = derive_mint_pda(b"migration_authority", &mint);
        let (lp_seed_token_vault, lp_seed_token_acct) = make_lp_seed_token_vault(&mint, 0);
        let wsol_vault_pda = derive_mint_pda(b"wsol_vault", &mint);

        // migration_sol_vault: typed program-owned with non-zero lamports.
        let migration_sol_vault_pda = derive_mint_pda(b"migration_sol", &mint);
        use anchor_lang::Discriminator;
        use launchctrl::state::MigrationSolVault;
        let mut msv_data = Vec::with_capacity(8);
        msv_data.extend_from_slice(MigrationSolVault::DISCRIMINATOR);
        let migration_sol_acct = Account {
            lamports: 100_000_000,
            data: msv_data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        let (reward_vault, reward_vault_acct) = make_reward_vault(&mint, 1_000_000);
        let (deployer_vault, deployer_vault_acct) = make_deployer_vault(&mint, 1_000_000);

        // fee_vault: must equal PLATFORM_FEE_VAULT.
        let fee_vault_acct = Account {
            lamports: 1_000_000,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        // Meteora-side accounts: pinned ones at canonical pubkeys, others
        // are dynamic (no Anchor key check). meteora_pool must match
        // launch_state.meteora_pool.
        let pool_authority_acct = Account {
            lamports: 1,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        let token_2022_program = mollusk_svm_programs_token::token2022::keyed_account();
        let system_program = mollusk_svm::program::keyed_account_for_system_program();

        // SPL classic Token program — must be a real loader-owned executable
        // so the runtime can actually dispatch to it on initialize_account3 /
        // close_account CPIs. Passing a fake system-owned placeholder
        // satisfies the Anchor pubkey constraint but breaks the runtime
        // with `UnsupportedProgramId` at CPI time.
        let (_, spl_token_program_acct) = mollusk_svm_programs_token::token::keyed_account();

        let meteora_program_acct =
            mollusk_svm::program::create_program_account_loader_v3(&METEORA_DAMM_V2_PROGRAM);

        // wsol_mint: properly-shaped SPL classic Mint so initialize_account3
        // accepts it for the wsol_vault (9 decimals, is_initialized=true,
        // no authorities — matches real WSOL).
        let wsol_mint_acct = wsol_mint_account();

        ReinjectAccounts {
            cranker: (cranker, cranker_acct),
            mint: (mint, mint_acct),
            wsol_mint: (WSOL_MINT, wsol_mint_acct),
            launch_state: (launch_state, launch_acct),
            creator: (creator, creator_acct),
            creator_token_account: (creator_ata, creator_ata_acct),
            migration_authority: empty_system_account_at(migration_authority_pda),
            lp_seed_token_vault: (lp_seed_token_vault, lp_seed_token_acct),
            wsol_vault: empty_system_account_at(wsol_vault_pda),
            migration_sol_vault: (migration_sol_vault_pda, migration_sol_acct),
            reward_vault: (reward_vault, reward_vault_acct),
            deployer_vault: (deployer_vault, deployer_vault_acct),
            fee_vault: (PLATFORM_FEE_VAULT, fee_vault_acct),
            meteora_pool: empty_system_account_at(meteora_pool_pk),
            meteora_position: empty_system_account_at(Pubkey::new_unique()),
            position_nft_account: empty_system_account_at(Pubkey::new_unique()),
            token_a_vault: empty_system_account_at(Pubkey::new_unique()),
            token_b_vault: empty_system_account_at(Pubkey::new_unique()),
            pool_authority: (METEORA_POOL_AUTHORITY, pool_authority_acct),
            event_authority: empty_system_account_at(METEORA_EVENT_AUTHORITY),
            meteora_program: (METEORA_DAMM_V2_PROGRAM, meteora_program_acct),
            token_2022_program,
            spl_token_program: (SPL_TOKEN_PROGRAM_ID, spl_token_program_acct),
            system_program,
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.cranker,
            self.mint,
            self.wsol_mint,
            self.launch_state,
            self.creator,
            self.creator_token_account,
            self.migration_authority,
            self.lp_seed_token_vault,
            self.wsol_vault,
            self.migration_sol_vault,
            self.reward_vault,
            self.deployer_vault,
            self.fee_vault,
            self.meteora_pool,
            self.meteora_position,
            self.position_nft_account,
            self.token_a_vault,
            self.token_b_vault,
            self.pool_authority,
            self.event_authority,
            self.meteora_program,
            self.token_2022_program,
            self.spl_token_program,
            self.system_program,
        ]
    }
}

fn build_ix(accounts: &ReinjectAccounts, liquidity_delta: u128) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_claim_and_reinject(liquidity_delta),
        vec![
            AccountMeta::new(accounts.cranker.0, true),
            AccountMeta::new(accounts.mint.0, false), // mut
            AccountMeta::new_readonly(accounts.wsol_mint.0, false),
            AccountMeta::new_readonly(accounts.launch_state.0, false),
            AccountMeta::new_readonly(accounts.creator.0, false),
            AccountMeta::new_readonly(accounts.creator_token_account.0, false),
            AccountMeta::new_readonly(accounts.migration_authority.0, false),
            AccountMeta::new(accounts.lp_seed_token_vault.0, false),
            AccountMeta::new(accounts.wsol_vault.0, false),
            AccountMeta::new(accounts.migration_sol_vault.0, false),
            AccountMeta::new(accounts.reward_vault.0, false),
            AccountMeta::new(accounts.deployer_vault.0, false),
            AccountMeta::new(accounts.fee_vault.0, false),
            AccountMeta::new(accounts.meteora_pool.0, false),
            AccountMeta::new(accounts.meteora_position.0, false),
            AccountMeta::new_readonly(accounts.position_nft_account.0, false),
            AccountMeta::new(accounts.token_a_vault.0, false),
            AccountMeta::new(accounts.token_b_vault.0, false),
            AccountMeta::new_readonly(accounts.pool_authority.0, false),
            AccountMeta::new_readonly(accounts.event_authority.0, false),
            AccountMeta::new_readonly(accounts.meteora_program.0, false),
            AccountMeta::new_readonly(accounts.token_2022_program.0, false),
            AccountMeta::new_readonly(accounts.spl_token_program.0, false),
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

/// PROPERTY: `launch_state.is_migrated == false` must revert with
/// `NotMigrated`. Anchor's account-level constraint, fires before the
/// handler runs. claim_and_reinject only makes sense post-migration —
/// during the curve phase there's no Meteora pool to claim from.
#[test]
fn reinject_not_migrated_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let mut accounts = ReinjectAccounts::happy();

    let (_, migration_sol_bump) =
        Pubkey::find_program_address(&[b"migration_sol", accounts.mint.0.as_ref()], &PROGRAM_ID);
    let meteora_pool_pk = accounts.meteora_pool.0;
    accounts.launch_state = make_launch_state(&accounts.mint.0, &accounts.creator.0, |l| {
        l.is_migrated = false; // ← deviation
        l.meteora_pool = meteora_pool_pk;
        l.migration_sol_vault_bump = migration_sol_bump;
        l.initial_buy_amount = 100_000_000;
    });

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected NotMigrated");
    // NotMigrated = offset 11 → 6011.
    assert_eq!(expect_custom_error(&result), 6011);
}

/// PROPERTY: `launch_state.meteora_pool == Pubkey::default()` must revert
/// with `PoolNotCreated`. Guards against calling reinject before
/// create_meteora_pool has run (i.e. before there's anything to reinject
/// against).
#[test]
fn reinject_pool_not_created_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let mut accounts = ReinjectAccounts::happy();

    let (_, migration_sol_bump) =
        Pubkey::find_program_address(&[b"migration_sol", accounts.mint.0.as_ref()], &PROGRAM_ID);
    accounts.launch_state = make_launch_state(&accounts.mint.0, &accounts.creator.0, |l| {
        l.is_migrated = true;
        l.meteora_pool = Pubkey::default(); // ← deviation
        l.migration_sol_vault_bump = migration_sol_bump;
        l.initial_buy_amount = 100_000_000;
    });

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected PoolNotCreated");
    // PoolNotCreated = offset 13 → 6013.
    assert_eq!(expect_custom_error(&result), 6013);
}

/// PROPERTY: an `meteora_program` pubkey other than `METEORA_DAMM_V2_PROGRAM`
/// must revert with `InvalidProgram`. Core substitution defense — without
/// this, an attacker could swap in a malicious program at that account slot
/// and our CPI dispatch would call into THEIR code instead of Meteora's.
#[test]
fn reinject_wrong_meteora_program_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let mut accounts = ReinjectAccounts::happy();
    // Substitute a random pubkey for meteora_program.
    accounts.meteora_program = empty_system_account_at(Pubkey::new_unique());

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected InvalidProgram");
    // InvalidProgram = offset 14 → 6014.
    assert_eq!(expect_custom_error(&result), 6014);
}

/// PROPERTY: a `pool_authority` pubkey other than `METEORA_POOL_AUTHORITY`
/// must revert with `InvalidProgram`. Sibling check to the meteora_program
/// substitution — without it, an attacker could swap in a malicious
/// authority and short-circuit Meteora's pool-side validation.
#[test]
fn reinject_wrong_pool_authority_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let mut accounts = ReinjectAccounts::happy();
    accounts.pool_authority = empty_system_account_at(Pubkey::new_unique());

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected InvalidProgram");
    assert_eq!(expect_custom_error(&result), 6014);
}

/// PROPERTY: a `fee_vault` pubkey other than `PLATFORM_FEE_VAULT` must
/// revert with `Unauthorized`. Without this, an attacker could redirect
/// the 6.25% platform fee slice to a wallet they control on every
/// reinject cycle.
#[test]
fn reinject_wrong_fee_vault_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let mut accounts = ReinjectAccounts::happy();
    accounts.fee_vault = empty_system_account_at(Pubkey::new_unique());

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());
    assert!(result.program_result.is_err(), "expected Unauthorized");
    // Unauthorized = offset 26 → 6026.
    assert_eq!(expect_custom_error(&result), 6026);
}

// ─── Happy-path test (zero-claim case via stub) ─────────────────────────────

/// PROPERTY: full claim_and_reinject pipeline runs end-to-end when the
/// Meteora CPIs are stubbed to no-ops. With wsol_claimed = 0 (the stub
/// didn't credit anything to wsol_vault), the handler's branch at
/// `if wsol_claimed > 0 { require!(wsol_consumed > 0) }` is correctly
/// skipped, the 4-way split is all zeros, and the ix returns Ok.
///
/// This validates the COMPLETE harness wiring:
///   - All 24 accounts pass Anchor validation
///   - System program CPI (create_account for wsol_vault) succeeds
///   - SPL classic Token CPI (initialize_account3) succeeds against a
///     properly-shaped WSOL mint
///   - Stub Meteora CPIs (claim_position_fee, add_liquidity) dispatch
///     and return Ok
///   - Post-CPI parsing reads wsol_vault.data[64..72] as 0
///   - Conditional require! at sell.rs:259 is correctly skipped
///   - SPL classic Token CPI (close_account) succeeds, draining
///     wsol_vault rent into migration_sol_vault
///   - 4-way zero-amount split logic runs without underflow
///   - wsol_rent refund to cranker completes
///
/// Future work: enhance the stub to credit wsol_vault during
/// claim_position_fee + decrement it during add_liquidity, then write a
/// non-zero-claim happy path that exercises the actual 4-way split with
/// real lamport flows.
#[test]
fn reinject_zero_claim_happy_path() {
    let mollusk = mollusk_with_meteora_stub();
    let accounts = ReinjectAccounts::happy();

    let cranker_pk = accounts.cranker.0;
    let cranker_before = accounts.cranker.1.lamports;
    let reward_vault_pk = accounts.reward_vault.0;
    let reward_vault_before = accounts.reward_vault.1.lamports;
    let deployer_vault_pk = accounts.deployer_vault.0;
    let deployer_vault_before = accounts.deployer_vault.1.lamports;

    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    // With wsol_claimed = 0, all 4 split takes are 0:
    //   - reward_vault should be unchanged
    //   - deployer_vault should be unchanged
    //   - cranker should be roughly even (paid wsol_rent up front,
    //     refunded wsol_rent at the end — net ~0 minus tx-level costs
    //     which Mollusk doesn't enforce on signers)
    let reward_after = lookup_account(&result, &reward_vault_pk);
    assert_eq!(
        reward_after.lamports, reward_vault_before,
        "reward_vault should be unchanged when wsol_claimed = 0"
    );

    let deployer_after = lookup_account(&result, &deployer_vault_pk);
    assert_eq!(
        deployer_after.lamports, deployer_vault_before,
        "deployer_vault should be unchanged when wsol_claimed = 0"
    );

    // Cranker funded wsol_vault rent up front (step 1: create_account)
    // and gets it back at the end (step 7: refund). Net change is 0.
    let cranker_after = lookup_account(&result, &cranker_pk);
    assert_eq!(
        cranker_after.lamports, cranker_before,
        "cranker should be net-even after wsol_rent loan + refund"
    );
}
