//! Mollusk instruction-level tests for `sell`.
//!
//! Targets the sell handler's pre-CPI validation gates. Every test below
//! triggers a specific revert path BEFORE the handler reaches its token
//! transfer or SOL-debit lines (`sell.rs:154+`). The tests cover:
//!
//!   - CurveComplete (post-migration block — requires is_complete && is_migrated)
//!   - ZeroAmount (token_amount must be > 0)
//!   - Blocked (seller in blocklist)
//!   - SlippageExceeded (min_sol_out too high)
//!   - InsufficientLiquidity (gross_sol_out > curve.real_sol_reserves)
//!
//! Mirrors `buy_mollusk.rs` patterns — same Mollusk setup, same Ricochet
//! None placeholder, same instructions sysvar dance. Sell adds two extra
//! accounts vs Buy: `lp_seed_sol_vault` (program-owned marker) and
//! `lp_seed_token_vault` (Token-2022 PDA-authority account).
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test sell_mollusk`

mod common;

use common::{
    make_blocklist, make_curve_state, make_curve_token_vault, make_launch_state,
    make_lp_seed_sol_vault, make_lp_seed_token_vault, make_mint, make_signer, make_sol_vault,
    make_token_account, PROGRAM_ID,
};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;
use solana_pubkey::{pubkey, Pubkey};

/// Build a Mollusk instance with launchctrl + Token-2022 both loaded.
fn mollusk_with_token_2022() -> Mollusk {
    let mut mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    mollusk
}

/// Discriminator for `sell` = sha256("global:sell")[..8].
const SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Standard Solana instructions sysvar ID.
const INSTRUCTIONS_SYSVAR_ID: Pubkey =
    pubkey!("Sysvar1nstructions1111111111111111111111111");

/// Encode the `sell(token_amount: u64, min_sol_out: u64)` ix data.
fn encode_sell(token_amount: u64, min_sol_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 16);
    data.extend_from_slice(&SELL_DISC);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_out.to_le_bytes());
    data
}

/// Bundles every account required by the Sell Accounts struct.
struct SellAccounts {
    seller: (Pubkey, Account),
    mint: (Pubkey, Account),
    curve_state: (Pubkey, Account),
    blocklist: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    curve_token_vault: (Pubkey, Account),
    sol_vault: (Pubkey, Account),
    lp_seed_sol_vault: (Pubkey, Account),
    lp_seed_token_vault: (Pubkey, Account),
    seller_token_account: (Pubkey, Account),
    token_program: (Pubkey, Account),
    /// `Option<Account<RicochetConfig>>` None encoding — pubkey == PROGRAM_ID +
    /// BPF-loader-format account at that pubkey. See buy_mollusk.rs for the
    /// debugging history; same trick applies here.
    ricochet_config: (Pubkey, Account),
    instructions_sysvar: (Pubkey, Account),
}

impl SellAccounts {
    /// Default state: curve has had buys (real reserves non-zero so a sell
    /// is mathematically valid), seller holds enough tokens to sell, empty
    /// blocklist.
    fn happy() -> Self {
        let (seller, seller_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        // Curve mid-fill — 10 SOL bought, 100M tokens in circulation. This
        // makes sell math non-degenerate (real_sol_reserves > 0 so we don't
        // underflow on small sells).
        let (curve_state, curve_acct) = make_curve_state(&mint, &seller, |c| {
            c.real_sol_reserves = 10_000_000_000; // 10 SOL bought
            c.real_token_reserves = 100_000_000_000_000; // 100M tokens out
        });
        let (blocklist, blocklist_acct) = make_blocklist(&mint, &seller, |_| {});
        let (launch_state, launch_acct) = make_launch_state(&mint, &seller, |_| {});
        // Curve vault holds the OTHER 900M tokens (1B total - 100M circulating).
        let (curve_token_vault, ctv_acct) =
            make_curve_token_vault(&mint, 900_000_000_000_000);
        let (sol_vault, sol_vault_acct) = make_sol_vault(&mint, 10_000_000_000);
        let (lp_seed_sol_vault, lp_seed_sol_acct) = make_lp_seed_sol_vault(&mint, 0);
        let (lp_seed_token_vault, lp_seed_tok_acct) =
            make_lp_seed_token_vault(&mint, 0);

        // Seller holds 1M tokens — enough for the slippage/CurveComplete/Blocked
        // tests which use small amounts; insufficient-liquidity test overrides
        // both the seller balance and curve state to scale up.
        let (seller_ata, seller_ata_acct) =
            make_token_account(&mint, &seller, 1_000_000_000_000);

        let token_program = mollusk_svm_programs_token::token2022::keyed_account();

        let ricochet_placeholder =
            mollusk_svm::program::create_program_account_loader_v3(&PROGRAM_ID);

        let instructions_sysvar_acct =
            mollusk_svm::instructions_sysvar::keyed_account(std::iter::empty()).1;

        SellAccounts {
            seller: (seller, seller_acct),
            mint: (mint, mint_acct),
            curve_state: (curve_state, curve_acct),
            blocklist: (blocklist, blocklist_acct),
            launch_state: (launch_state, launch_acct),
            curve_token_vault: (curve_token_vault, ctv_acct),
            sol_vault: (sol_vault, sol_vault_acct),
            lp_seed_sol_vault: (lp_seed_sol_vault, lp_seed_sol_acct),
            lp_seed_token_vault: (lp_seed_token_vault, lp_seed_tok_acct),
            seller_token_account: (seller_ata, seller_ata_acct),
            token_program,
            ricochet_config: (PROGRAM_ID, ricochet_placeholder),
            instructions_sysvar: (INSTRUCTIONS_SYSVAR_ID, instructions_sysvar_acct),
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.seller,
            self.mint,
            self.curve_state,
            self.blocklist,
            self.launch_state,
            self.curve_token_vault,
            self.sol_vault,
            self.lp_seed_sol_vault,
            self.lp_seed_token_vault,
            self.seller_token_account,
            self.token_program,
            self.ricochet_config,
            self.instructions_sysvar,
        ]
    }
}

/// Build the sell instruction with accounts in canonical order matching the
/// Sell<'info> Accounts struct in `programs/launchctrl/src/instructions/sell.rs`.
fn build_sell_ix(accounts: &SellAccounts, token_amount: u64, min_sol_out: u64) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_sell(token_amount, min_sol_out),
        vec![
            AccountMeta::new(accounts.seller.0, true), // signer + mut
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.curve_state.0, false),
            AccountMeta::new_readonly(accounts.blocklist.0, false),
            AccountMeta::new_readonly(accounts.launch_state.0, false),
            AccountMeta::new(accounts.curve_token_vault.0, false),
            AccountMeta::new(accounts.sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_sol_vault.0, false),
            AccountMeta::new(accounts.lp_seed_token_vault.0, false),
            AccountMeta::new(accounts.seller_token_account.0, false),
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

/// PROPERTY: `token_amount == 0` must revert with `ZeroAmount`. Tests the
/// handler-body guard at sell.rs:42.
#[test]
fn sell_zero_token_amount_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = SellAccounts::happy();
    let ix = build_sell_ix(&accounts, 0, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected ZeroAmount error, got success"
    );
    assert_eq!(expect_custom_error(&result), 6022);
}

/// PROPERTY: a seller in the blocklist must be rejected with `Blocked`.
/// KOL Shield's blocklist applies to BOTH directions — a wallet can't trade
/// at all (buy OR sell) once blocklisted.
#[test]
fn sell_blocked_seller_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = SellAccounts::happy();
    let seller_pk = accounts.seller.0;
    accounts.blocklist =
        make_blocklist(&accounts.mint.0, &accounts.seller.0, |b| {
            b.blocked.push(seller_pk);
        });

    let ix = build_sell_ix(&accounts, 1_000_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected Blocked error, got success"
    );
    assert_eq!(expect_custom_error(&result), 6025);
}

/// PROPERTY: a curve that has BOTH `is_complete` AND the launch's
/// `is_migrated` set must revert with `CurveComplete`. This blocks the
/// post-migration sell path — once Meteora has the liquidity, sells route
/// through the DEX, not the bonding curve.
///
/// Note: setting `is_complete` alone (the pre-migration completion window)
/// does NOT block sells — there's still a window where users can dump
/// tokens against the curve before migration runs. Sell handler check at
/// `sell.rs:38-41` is `is_complete && is_migrated`.
#[test]
fn sell_on_migrated_curve_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = SellAccounts::happy();
    accounts.curve_state =
        make_curve_state(&accounts.mint.0, &accounts.seller.0, |c| {
            c.real_sol_reserves = 10_000_000_000;
            c.real_token_reserves = 100_000_000_000_000;
            c.is_complete = true;
        });
    accounts.launch_state =
        make_launch_state(&accounts.mint.0, &accounts.seller.0, |l| {
            l.is_migrated = true;
        });

    let ix = build_sell_ix(&accounts, 1_000_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected CurveComplete error, got success"
    );
    assert_eq!(expect_custom_error(&result), 6021);
}

/// PROPERTY: `min_sol_out` set higher than the actual quote must revert
/// with `SlippageExceeded`. Mirror of the buy-side slippage gate at
/// `sell.rs:111` — defends against sandwiches and bad quotes.
#[test]
fn sell_slippage_exceeded_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = SellAccounts::happy();

    let ix = build_sell_ix(&accounts, 1_000_000_000, u64::MAX);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected SlippageExceeded, got success"
    );
    assert_eq!(expect_custom_error(&result), 6023);
}

/// PROPERTY: a sell that quotes more SOL than `curve.real_sol_reserves`
/// actually holds must revert with `InsufficientLiquidity`. Defends against
/// state where the curve's accounting promises more than the SOL vault
/// can deliver. Sell handler check at `sell.rs:112-115`.
///
/// Setup: large real_token_reserves (so the seller-side subtraction at
/// step 7 doesn't underflow first) + a tiny real_sol_reserves (so the
/// quote computes > real_sol_reserves). The curve's virtual reserves at
/// 30 SOL / 1.073B-token defaults mean even small sells return non-trivial
/// gross_sol_out.
#[test]
fn sell_insufficient_liquidity_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = SellAccounts::happy();
    // 1B circulating tokens (real_token_reserves ≈ near virtual limit) +
    // only 1000 lamports of real SOL → the constant-product quote on a
    // large sell will compute many SOL out, far exceeding the 1000-lamport
    // floor. Hits InsufficientLiquidity at sell.rs:112 before any state
    // mutation. Stays under virtual_token_reserves (1.073e15) so the
    // pre-quote subtraction in quote_sell doesn't trip MathOverflow.
    accounts.curve_state =
        make_curve_state(&accounts.mint.0, &accounts.seller.0, |c| {
            c.real_sol_reserves = 1_000;
            c.real_token_reserves = 1_000_000_000_000_000; // 1B tokens
        });
    // Seller holds enough to sell what we're trying to sell.
    accounts.seller_token_account = make_token_account(
        &accounts.mint.0,
        &accounts.seller.0,
        500_000_000_000_000,
    );

    let ix = build_sell_ix(&accounts, 500_000_000_000_000, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientLiquidity, got success"
    );
    assert_eq!(expect_custom_error(&result), 6024);
}
