//! Mollusk instruction-level tests for `create_meteora_pool`.
//!
//! `create_meteora_pool` is the permissionless cranker ix that wraps the
//! bonding curve's drained SOL into WSOL and CPIs into Meteora DAMM v2's
//! `initialize_customizable_pool`. The two pre-CPI gates we test here:
//!
//!   1. **InsufficientMigrationFunds** — `sol_amount > 0` /
//!      `token_amount > 0`. Migration vaults must be non-empty.
//!   2. **InvalidInitSqrtPrice (M-1 audit finding)** — the cranker-provided
//!      `init_sqrt_price` must be within ±10% of the price implied by the
//!      migration vault contents. This is the front-running protection on
//!      permissionless pool creation: without it, an attacker who races the
//!      cranker can pass a degenerate sqrt_price and permanently misprice
//!      the pool. See `security/SECURITY_AUDIT.md` M-1.
//!
//! Both gates fire BEFORE any Meteora CPI runs. The Meteora stub is loaded
//! in the harness for future happy-path tests but not exercised by these
//! negative-path tests.
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test create_pool_mollusk`

mod common;

use common::{
    derive_mint_pda, make_deployer_vault, make_launch_state, make_mint,
    make_packed_token_account, make_reward_vault, make_signer, make_uninit_pda, PROGRAM_ID,
    SYSTEM_PROGRAM_ID,
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
    // (initialize_account3 + close_account CPIs inside create_meteora_pool).
    mollusk_svm_programs_token::token::add_program(&mut mollusk);
    // Built from `programs/meteora-stub`. Returns Ok(()) on any ix data.
    // Loaded under the real Meteora DAMM v2 pubkey so our CPIs dispatch.
    mollusk.add_program(&METEORA_DAMM_V2_PROGRAM, "meteora_stub");
    mollusk
}

const CREATE_METEORA_POOL_DISC: [u8; 8] = [246, 254, 33, 37, 225, 176, 41, 232];

const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const SPL_TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const METEORA_DAMM_V2_PROGRAM: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
const METEORA_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
const METEORA_EVENT_AUTHORITY: Pubkey = pubkey!("3rmHSu74h1ZcmAisVcWerTCiRDQbUrBKmcwptYGjHfet");

fn encode_create_meteora_pool(
    init_sqrt_price: u128,
    liquidity_delta: u128,
    drip_total: u64,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 16 + 16 + 8);
    data.extend_from_slice(&CREATE_METEORA_POOL_DISC);
    data.extend_from_slice(&init_sqrt_price.to_le_bytes());
    data.extend_from_slice(&liquidity_delta.to_le_bytes());
    data.extend_from_slice(&drip_total.to_le_bytes());
    data
}

/// Realistic migration vault sizes for the happy-path setup: ~85 SOL + 280M
/// tokens, similar to a fully-bonded curve. The sqrt-price math evaluates
/// against these to compute `expected_sqrt_price`.
const MIGRATION_SOL_LAMPORTS: u64 = 85_500_000_000; // 85.5 SOL (incl. rent floor headroom)
const MIGRATION_TOKEN_AMOUNT: u64 = 280_000_000_000_000; // 280M with 6 decimals

struct CreatePoolAccounts {
    cranker: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    migration_authority: (Pubkey, Account),
    migration_token_vault: (Pubkey, Account),
    migration_sol_vault: (Pubkey, Account),
    reward_vault: (Pubkey, Account),
    deployer_vault: (Pubkey, Account),
    wsol_vault: (Pubkey, Account),
    wsol_mint: (Pubkey, Account),
    position_nft_mint: (Pubkey, Account),
    pool_authority: (Pubkey, Account),
    meteora_pool: (Pubkey, Account),
    meteora_position: (Pubkey, Account),
    position_nft_account: (Pubkey, Account),
    token_a_vault: (Pubkey, Account),
    token_b_vault: (Pubkey, Account),
    event_authority: (Pubkey, Account),
    meteora_program: (Pubkey, Account),
    token_2022_program: (Pubkey, Account),
    spl_token_program: (Pubkey, Account),
    system_program: (Pubkey, Account),
}

/// Build an empty account at a target pubkey — system-owned, no data, no
/// lamports. Used for the Meteora-side accounts that the real Meteora
/// program would initialize on first call; our stub does nothing so they
/// stay empty.
fn empty_account_at(pubkey: Pubkey) -> (Pubkey, Account) {
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

impl CreatePoolAccounts {
    fn happy() -> Self {
        let (cranker, cranker_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);

        // Derive canonical PDAs + bumps. The bumps for migration_sol_vault
        // and migration_token_vault are stored on launch_state (set during
        // migrate_to_pool) and used by Anchor as the `bump = ...` source.
        // If we leave them at 0, Anchor's seeds check computes a different
        // PDA than what we supply, failing with ConstraintSeeds (2006).
        let (migration_sol_vault_pda, migration_sol_bump) =
            Pubkey::find_program_address(&[b"migration_sol", mint.as_ref()], &PROGRAM_ID);
        let (migration_token_vault_pda, migration_token_bump) =
            Pubkey::find_program_address(&[b"migration_vault", mint.as_ref()], &PROGRAM_ID);
        let migration_authority_pda = derive_mint_pda(b"migration_authority", &mint);

        // launch_state: migrated, no pool yet recorded, with the bumps set
        // to their canonical values.
        let (launch_state, launch_acct) = make_launch_state(&mint, &cranker, |l| {
            l.is_migrated = true;
            l.meteora_pool = Pubkey::default();
            l.migration_sol_vault_bump = migration_sol_bump;
            l.drip_vault_bump = migration_token_bump; // vestigial name; stores migration_vault bump
        });

        // migration_token_vault — owned by migration_authority PDA, 280M tokens.
        let migration_token_acct = make_packed_token_account(
            &mint,
            &migration_authority_pda,
            MIGRATION_TOKEN_AMOUNT,
        );
        use anchor_lang::Discriminator;
        use launchctrl::state::MigrationSolVault;
        let mut msv_data = Vec::with_capacity(8);
        msv_data.extend_from_slice(MigrationSolVault::DISCRIMINATOR);
        let migration_sol_acct = Account {
            lamports: MIGRATION_SOL_LAMPORTS,
            data: msv_data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        // reward_vault + deployer_vault — Anchor `init`s these inside the ix,
        // so they must be UNINITIALIZED at call time. System-owned + 0 data.
        let (reward_vault_pk, reward_vault_acct) =
            make_uninit_pda(derive_mint_pda(b"reward_vault", &mint));
        let (deployer_vault_pk, deployer_vault_acct) =
            make_uninit_pda(derive_mint_pda(b"deployer_vault", &mint));

        // wsol_vault: PDA, will be create_account'd inside the handler.
        let wsol_vault_pda = derive_mint_pda(b"wsol_vault", &mint);
        let wsol_vault_acct = empty_account_at(wsol_vault_pda).1;

        // wsol_mint: properly-shaped SPL classic Mint so initialize_account3
        // accepts it (decimals=9, is_initialized=true, no authorities — matches
        // real WSOL). Required once we run the full pipeline through Meteora
        // stub instead of bailing pre-CPI.
        let wsol_mint_acct = {
            let mut data = vec![0u8; 82];
            data[44] = 9; // decimals
            data[45] = 1; // is_initialized
            Account {
                lamports: 1_000_000,
                data,
                owner: SPL_TOKEN_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }
        };

        // position_nft_mint: PDA, will be initialized inside the handler.
        let position_nft_mint_pda = derive_mint_pda(b"position_nft_mint", &mint);

        // Meteora-side accounts: pool_authority + event_authority must be the
        // canonical pubkeys (Anchor constraint). The rest are dynamic — pass
        // empty system-owned placeholders for the runtime to forward to the
        // stub.
        let pool_authority_acct = Account {
            lamports: 1,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        let token_2022_program = mollusk_svm_programs_token::token2022::keyed_account();
        let system_program = mollusk_svm::program::keyed_account_for_system_program();

        // SPL classic Token program — real loader-owned executable so
        // initialize_account3 / close_account CPIs dispatch correctly.
        let (_, spl_token_program_acct) = mollusk_svm_programs_token::token::keyed_account();

        let meteora_program_acct =
            mollusk_svm::program::create_program_account_loader_v3(&METEORA_DAMM_V2_PROGRAM);

        CreatePoolAccounts {
            cranker: (cranker, cranker_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state, launch_acct),
            migration_authority: (migration_authority_pda, empty_account_at(migration_authority_pda).1),
            migration_token_vault: (migration_token_vault_pda, migration_token_acct),
            migration_sol_vault: (migration_sol_vault_pda, migration_sol_acct),
            reward_vault: (reward_vault_pk, reward_vault_acct),
            deployer_vault: (deployer_vault_pk, deployer_vault_acct),
            wsol_vault: (wsol_vault_pda, wsol_vault_acct),
            wsol_mint: (WSOL_MINT, wsol_mint_acct),
            position_nft_mint: empty_account_at(position_nft_mint_pda),
            pool_authority: (METEORA_POOL_AUTHORITY, pool_authority_acct),
            meteora_pool: empty_account_at(Pubkey::new_unique()),
            meteora_position: empty_account_at(Pubkey::new_unique()),
            position_nft_account: empty_account_at(Pubkey::new_unique()),
            token_a_vault: empty_account_at(Pubkey::new_unique()),
            token_b_vault: empty_account_at(Pubkey::new_unique()),
            event_authority: empty_account_at(METEORA_EVENT_AUTHORITY),
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
            self.launch_state,
            self.migration_authority,
            self.migration_token_vault,
            self.migration_sol_vault,
            self.reward_vault,
            self.deployer_vault,
            self.wsol_vault,
            self.wsol_mint,
            self.position_nft_mint,
            self.pool_authority,
            self.meteora_pool,
            self.meteora_position,
            self.position_nft_account,
            self.token_a_vault,
            self.token_b_vault,
            self.event_authority,
            self.meteora_program,
            self.token_2022_program,
            self.spl_token_program,
            self.system_program,
        ]
    }
}

fn build_ix(
    accounts: &CreatePoolAccounts,
    init_sqrt_price: u128,
    liquidity_delta: u128,
    drip_total: u64,
) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_create_meteora_pool(init_sqrt_price, liquidity_delta, drip_total),
        vec![
            AccountMeta::new(accounts.cranker.0, true),
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.launch_state.0, false),
            AccountMeta::new(accounts.migration_authority.0, false),
            AccountMeta::new(accounts.migration_token_vault.0, false),
            AccountMeta::new(accounts.migration_sol_vault.0, false),
            AccountMeta::new(accounts.reward_vault.0, false),
            AccountMeta::new(accounts.deployer_vault.0, false),
            AccountMeta::new(accounts.wsol_vault.0, false),
            AccountMeta::new_readonly(accounts.wsol_mint.0, false),
            AccountMeta::new(accounts.position_nft_mint.0, false),
            AccountMeta::new_readonly(accounts.pool_authority.0, false),
            AccountMeta::new(accounts.meteora_pool.0, false),
            AccountMeta::new(accounts.meteora_position.0, false),
            AccountMeta::new(accounts.position_nft_account.0, false),
            AccountMeta::new(accounts.token_a_vault.0, false),
            AccountMeta::new(accounts.token_b_vault.0, false),
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

// ─── Tests ──────────────────────────────────────────────────────────────────

/// PROPERTY: migration_sol_vault holding only rent (no curve SOL above the
/// rent floor) must revert with `InsufficientMigrationFunds`. This is the
/// `sol_amount > 0` guard at `create_pool.rs:57`. Guards against creating
/// a pool with degenerate liquidity.
#[test]
fn create_pool_zero_sol_reverts() {
    let mollusk = mollusk_with_meteora_stub();

    let mut accounts = CreatePoolAccounts::happy();
    // Set migration_sol_vault.lamports = rent_min(8) exactly (or below).
    // Then sol_amount = current - rent_min = 0 → InsufficientMigrationFunds.
    // Default Solana rent on 8 bytes ≈ 890_880 lamports — set to that.
    accounts.migration_sol_vault.1.lamports = 890_880;

    let ix = build_ix(&accounts, 1, 1, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientMigrationFunds"
    );
    // InsufficientMigrationFunds = offset 15 → 6015.
    assert_eq!(expect_custom_error(&result), 6015);
}

/// PROPERTY: `init_sqrt_price` more than ±10% off the price implied by the
/// migration vault contents must revert with `InvalidInitSqrtPrice`. This
/// is the M-1 audit finding: the front-running protection on permissionless
/// pool creation. Without it, an attacker who races the cranker can pass a
/// degenerate sqrt_price and permanently misprice the launch.
///
/// Passing `init_sqrt_price = 1` is guaranteed to be outside the ±10%
/// envelope around the expected sqrt(85.5 SOL / 280M tokens) << 64 value,
/// which is many orders of magnitude larger than 1.
#[test]
fn create_pool_invalid_sqrt_price_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let accounts = CreatePoolAccounts::happy();

    let ix = build_ix(&accounts, 1, 1, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InvalidInitSqrtPrice"
    );
    // InvalidInitSqrtPrice = offset 18 → 6018.
    assert_eq!(expect_custom_error(&result), 6018);
}

/// Mirror of the on-chain `expected_sqrt_price` math at `create_pool.rs:74-88`.
/// Lets tests compute a sqrt-price exactly at the center of the ±10% gate
/// so they pass M-1 and reach later gates.
fn compute_expected_sqrt_price(sol_amount: u64, token_amount: u64, mint: &Pubkey) -> u128 {
    let wsol_is_token_a = WSOL_MINT.to_bytes() < mint.to_bytes();
    let (deposited_a, deposited_b) = if wsol_is_token_a {
        (sol_amount as u128, token_amount as u128)
    } else {
        (token_amount as u128, sol_amount as u128)
    };
    let isqrt_a = deposited_a.isqrt();
    let isqrt_b = deposited_b.isqrt();
    (isqrt_b << 64) / isqrt_a
}

/// PROPERTY: if the bonded SOL (migration_sol_vault.lamports - sol_vault_rent)
/// is below `CRANKER_MIGRATION_REIMBURSEMENT_LAMPORTS` (0.035 SOL), the
/// `checked_sub` underflow guard in step 1 must revert with
/// `InsufficientMigrationFunds`.
///
/// In practice this case is unreachable in production: the curve only
/// completes when `real_sol_reserves >= migration_threshold_lamports`, and
/// `MIN_MIGRATION_THRESHOLD_LAMPORTS = 1 SOL` is enforced at
/// `initialize_launch`. So `bonded_sol >= 1 SOL >> 35M lamports` always.
///
/// But the underflow guard is still load-bearing — without `checked_sub`,
/// the cranker reimbursement subtraction would wrap to a huge positive
/// number and the LP would be drained of nonsensical amounts. The
/// 2026-05-26 program upgrade added this guard; this test pins its behavior
/// so a future refactor that drops the guard fails loudly.
#[test]
fn create_pool_below_reimbursement_reverts() {
    let mollusk = mollusk_with_meteora_stub();

    let mut accounts = CreatePoolAccounts::happy();
    // Set migration_sol_vault to sol_vault_rent + 20_000_000 (20M lamports
    // bonded — less than the 35M reimbursement). Default 8-byte rent ≈
    // 890,880 lamports.
    let sol_vault_rent = 890_880u64;
    accounts.migration_sol_vault.1.lamports = sol_vault_rent + 20_000_000;

    let ix = build_ix(&accounts, 1, 1, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientMigrationFunds (bonded_sol < reimbursement)"
    );
    // InsufficientMigrationFunds = offset 15 → 6015.
    assert_eq!(expect_custom_error(&result), 6015);
}

/// PROPERTY: after `initialize_customizable_pool` runs, the handler reloads
/// `migration_token_vault` and rejects with `InsufficientPoolLiquidity` if
/// MORE than `MAX_POOL_INIT_TOKEN_RESIDUAL_BPS` (10%) of the original
/// tokens are still in the vault. Defends against a griefing cranker who
/// passes an absurdly small `liquidity_delta`: Meteora would deposit
/// nothing, the bulk of migration tokens would be stranded, and the pool
/// would have negligible locked liquidity.
///
/// This test exercises the full PRE-CPI pipeline (Anchor validation, M-1
/// sqrt-price gate, system + spl_token CPIs to create wsol_vault) AND
/// the stub Meteora CPI (initialize_customizable_pool dispatched + Ok'd),
/// then trips the post-CPI sanity check correctly. Reaching this gate
/// proves the full Meteora-stub harness path works.
#[test]
fn create_pool_insufficient_pool_liquidity_reverts() {
    let mollusk = mollusk_with_meteora_stub();
    let accounts = CreatePoolAccounts::happy();

    // The handler subtracts sol_vault_rent from migration_sol_vault to
    // compute sol_amount. Mirror that here.
    let sol_vault_rent = 890_880u64; // Rent for 8-byte program-owned account
    let sol_amount = MIGRATION_SOL_LAMPORTS - sol_vault_rent;
    let token_amount = MIGRATION_TOKEN_AMOUNT;

    // Compute the exact expected sqrt-price → passes the ±10% M-1 gate.
    let init_sqrt = compute_expected_sqrt_price(sol_amount, token_amount, &accounts.mint.0);

    let ix = build_ix(&accounts, init_sqrt, 1, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientPoolLiquidity"
    );
    // InsufficientPoolLiquidity = offset 19 → 6019.
    assert_eq!(expect_custom_error(&result), 6019);
}
