//! Mollusk instruction-level tests for `buy`.
//!
//! Targets the buy handler's pre-CPI validation gates. Every test below
//! triggers a specific revert path BEFORE the handler reaches its SOL or
//! token transfer CPIs (lines 124+ in `buy.rs`). The tests cover:
//!
//!   - CurveComplete (post-migration block)
//!   - ZeroAmount (sol_amount must be > 0)
//!   - Blocked (buyer in blocklist)
//!   - SlippageExceeded (min_tokens_out too high)
//!   - InsufficientLiquidity (curve_token_vault doesn't have enough)
//!
//! Anchor account validation must pass for all of these (otherwise we'd
//! see system errors at offset 2000+ instead of LaunchCtrl errors at 6000+).
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test buy_mollusk`

mod common;

use common::{
    make_blocklist, make_curve_state, make_curve_token_vault, make_launch_state, make_mint,
    make_signer, make_sol_vault, make_token_account, PROGRAM_ID, SYSTEM_PROGRAM_ID,
};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;
use solana_pubkey::{pubkey, Pubkey};

/// Build a Mollusk instance with launchctrl + Token-2022 both loaded. The
/// Token-2022 program is needed because the Buy ix's account list
/// references its pubkey, and Mollusk validates referenced program
/// accounts against its loaded program set.
fn mollusk_with_token_2022() -> Mollusk {
    let mut mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    mollusk
}

/// Discriminator for `buy` = sha256("global:buy")[..8].
const BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

/// PLATFORM_FEE_VAULT — must match the hardcoded constant in
/// `programs/launchctrl/src/constants.rs`.
const PLATFORM_FEE_VAULT: Pubkey =
    pubkey!("2Do45QcuM3yvfes3Uc5PuD3BcsRZ29pq56LQWQczyDvi");

/// Standard Solana instructions sysvar ID.
const INSTRUCTIONS_SYSVAR_ID: Pubkey =
    pubkey!("Sysvar1nstructions1111111111111111111111111");

/// Encode the `buy(sol_amount: u64, min_tokens_out: u64)` ix data.
fn encode_buy(sol_amount: u64, min_tokens_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 16);
    data.extend_from_slice(&BUY_DISC);
    data.extend_from_slice(&sol_amount.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    data
}

/// Bundles every account required by the Buy Accounts struct. Created with
/// realistic happy-path defaults; individual tests override the specific
/// account they want to trip the validation on.
struct BuyAccounts {
    buyer: (Pubkey, Account),
    mint: (Pubkey, Account),
    curve_state: (Pubkey, Account),
    blocklist: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    curve_token_vault: (Pubkey, Account),
    sol_vault: (Pubkey, Account),
    fee_vault: (Pubkey, Account),
    buyer_token_account: (Pubkey, Account),
    system_program: (Pubkey, Account),
    token_program: (Pubkey, Account),
    /// `Option<Account<RicochetConfig>>` is encoded by passing the launchctrl
    /// program ID itself as the placeholder — Anchor sees owner != program
    /// and resolves Option to None. No ricochet enforcement runs.
    ricochet_config: (Pubkey, Account),
    instructions_sysvar: (Pubkey, Account),
}

impl BuyAccounts {
    /// Default happy-path-ish state: empty blocklist, default curve reserves,
    /// curve_token_vault holding 800M tokens (enough for normal buys).
    fn happy() -> Self {
        let (buyer, buyer_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (curve_state, curve_acct) = make_curve_state(&mint, &buyer, |_| {});
        let (blocklist, blocklist_acct) = make_blocklist(&mint, &buyer, |_| {});
        let (launch_state, launch_acct) = make_launch_state(&mint, &buyer, |_| {});
        let (curve_token_vault, ctv_acct) =
            make_curve_token_vault(&mint, 800_000_000_000_000); // 800M tokens
        let (sol_vault, sol_vault_acct) = make_sol_vault(&mint, 0);

        // fee_vault — must match hardcoded PLATFORM_FEE_VAULT pubkey.
        let fee_vault_acct = Account {
            lamports: 1_000_000,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };

        let (buyer_ata, buyer_ata_acct) = make_token_account(&mint, &buyer, 0);

        // System program — Mollusk's built-in keyed_account helper builds a
        // properly-recognized System program account (right loader + state).
        let system_program = mollusk_svm::program::keyed_account_for_system_program();
        // Token-2022 program — same idea, from the official Anza helper crate.
        let token_program = mollusk_svm_programs_token::token2022::keyed_account();

        // Ricochet None placeholder — Anchor 1.0 convention per CLAUDE.md:
        // pass PROGRAM_ID itself as the pubkey when the Option<Account<T>>
        // should resolve to None. Anchor's Option<Account<T>>::try_accounts
        // checks pubkey first; if it matches the program's own ID, returns
        // None without examining account data. Account must still be a
        // valid program-format account (executable=true, owned by BPF
        // loader) — Mollusk uses this format internally for our program.
        let ricochet_placeholder =
            mollusk_svm::program::create_program_account_loader_v3(&PROGRAM_ID);

        // Instructions sysvar — Solana's transaction context push REJECTS
        // sysvar-owned accounts with empty data (AccountDataTooSmall), even
        // if the program logic never reads them. So we must populate it
        // properly via Mollusk's helper. Filled in by the test runner since
        // it requires the actual instruction list. Placeholder here; the
        // build_buy_ix step regenerates this with the real ix in scope.
        let instructions_sysvar_acct =
            mollusk_svm::instructions_sysvar::keyed_account(std::iter::empty()).1;

        BuyAccounts {
            buyer: (buyer, buyer_acct),
            mint: (mint, mint_acct),
            curve_state: (curve_state, curve_acct),
            blocklist: (blocklist, blocklist_acct),
            launch_state: (launch_state, launch_acct),
            curve_token_vault: (curve_token_vault, ctv_acct),
            sol_vault: (sol_vault, sol_vault_acct),
            fee_vault: (PLATFORM_FEE_VAULT, fee_vault_acct),
            buyer_token_account: (buyer_ata, buyer_ata_acct),
            system_program,
            token_program,
            ricochet_config: (PROGRAM_ID, ricochet_placeholder),
            instructions_sysvar: (INSTRUCTIONS_SYSVAR_ID, instructions_sysvar_acct),
        }
    }

    /// Convert to the (pubkey, Account) vec Mollusk needs.
    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.buyer,
            self.mint,
            self.curve_state,
            self.blocklist,
            self.launch_state,
            self.curve_token_vault,
            self.sol_vault,
            self.fee_vault,
            self.buyer_token_account,
            self.system_program,
            self.token_program,
            self.ricochet_config,
            self.instructions_sysvar,
        ]
    }
}

/// Build the buy instruction with accounts in canonical order matching the
/// Buy<'info> Accounts struct.
fn build_buy_ix(accounts: &BuyAccounts, sol_amount: u64, min_tokens_out: u64) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_buy(sol_amount, min_tokens_out),
        vec![
            AccountMeta::new(accounts.buyer.0, true), // signer + mut
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.curve_state.0, false),
            AccountMeta::new(accounts.blocklist.0, false),
            AccountMeta::new(accounts.launch_state.0, false),
            AccountMeta::new(accounts.curve_token_vault.0, false),
            AccountMeta::new(accounts.sol_vault.0, false),
            AccountMeta::new(accounts.fee_vault.0, false),
            AccountMeta::new(accounts.buyer_token_account.0, false),
            AccountMeta::new_readonly(accounts.system_program.0, false),
            AccountMeta::new_readonly(accounts.token_program.0, false),
            AccountMeta::new_readonly(accounts.ricochet_config.0, false),
            AccountMeta::new_readonly(accounts.instructions_sysvar.0, false),
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

/// PROPERTY: `sol_amount == 0` must revert with `ZeroAmount`. Tests the
/// handler-body guard at buy.rs:21.
#[test]
fn buy_zero_sol_amount_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = BuyAccounts::happy();
    let ix = build_buy_ix(&accounts, 0, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected ZeroAmount error, got success"
    );
    // 6000 + 22 (ZeroAmount).
    assert_eq!(expect_custom_error(&result), 6022);
}

/// PROPERTY: a buyer in the blocklist must be rejected with `Blocked`.
/// Defends KOL Shield's core function — the on-chain blocklist actually
/// blocks buys.
#[test]
fn buy_blocked_buyer_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = BuyAccounts::happy();
    let buyer_pk = accounts.buyer.0;
    // Re-make the blocklist with the buyer's pubkey in `blocked`.
    accounts.blocklist =
        make_blocklist(&accounts.mint.0, &accounts.buyer.0, |b| {
            b.blocked.push(buyer_pk);
        });

    let ix = build_buy_ix(&accounts, 1_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected Blocked error, got success"
    );
    // 6000 + 25 (Blocked).
    assert_eq!(expect_custom_error(&result), 6025);
}

/// PROPERTY: `curve_state.is_complete == true` must revert any further
/// buy with `CurveComplete`. Defends post-migration; after the curve fills
/// and migration runs, no more bonding-curve buys.
#[test]
fn buy_on_complete_curve_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = BuyAccounts::happy();
    accounts.curve_state =
        make_curve_state(&accounts.mint.0, &accounts.buyer.0, |c| {
            c.is_complete = true;
        });

    let ix = build_buy_ix(&accounts, 1_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected CurveComplete error, got success"
    );
    // 6000 + 21 (CurveComplete).
    assert_eq!(expect_custom_error(&result), 6021);
}

/// PROPERTY: `min_tokens_out` set higher than the actual quote must revert
/// with `SlippageExceeded`. Defends users from sandwich attacks — slippage
/// guard correctly blocks transactions that come out worse than the user
/// signed for.
#[test]
fn buy_slippage_exceeded_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = BuyAccounts::happy();

    // 0.001 SOL in → roughly 33B tokens out at default reserves.
    // Setting min_tokens_out = u64::MAX guarantees SlippageExceeded.
    let ix = build_buy_ix(&accounts, 1_000_000, u64::MAX);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected SlippageExceeded, got success"
    );
    // 6000 + 23 (SlippageExceeded).
    assert_eq!(expect_custom_error(&result), 6023);
}

/// PROPERTY: a curve_token_vault with insufficient supply for the quote
/// must revert with `InsufficientLiquidity`. Defends against state where
/// the vault doesn't actually hold the tokens the math is promising.
#[test]
fn buy_insufficient_liquidity_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = BuyAccounts::happy();
    // Vault holds only 1 token. The quote for 0.001 SOL is far more.
    accounts.curve_token_vault = make_curve_token_vault(&accounts.mint.0, 1);

    let ix = build_buy_ix(&accounts, 1_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientLiquidity, got success"
    );
    // 6000 + 24 (InsufficientLiquidity).
    assert_eq!(expect_custom_error(&result), 6024);
}
