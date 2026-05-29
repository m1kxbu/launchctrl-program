//! Mollusk instruction-level tests for `admin_withdraw_deployer_bonus`.
//!
//! Recurring deployer-bonus payout from `deployer_vault` to the launch
//! creator. Bundled with `admin_withdraw_rewards` by the
//! `POST /api/staking/[mint]/claim` route when the claimer is the launch
//! creator — they get Diamond Hands holder share PLUS the 6.25% dev-bonus
//! slice in one tx.
//!
//! Eligibility surface (handler at `instructions/admin_withdraw_deployer_bonus.rs:32-66`):
//!
//!   1. Signer pubkey must equal `REWARDS_AUTHORITY`.
//!   2. Amount > 0.
//!   3. `launch.initial_buy_amount > 0` — creator made an initial buy.
//!   4. `creator_token_account.amount >= initial_buy_amount` — still holding.
//!   5. `amount <= vault.lamports() - rent_min(8)` — leave rent-exempt.
//!
//! The still-holding gate is the defense-in-depth that the rewards-only
//! withdraw doesn't have — even a compromised API can't drain the
//! deployer_vault for a creator who dumped below their initial buy.
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test admin_withdraw_deployer_bonus_mollusk`

mod common;

use common::{
    make_deployer_vault, make_launch_state, make_mint, make_signer, make_signer_at,
    make_token_account, PROGRAM_ID,
};
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;
use solana_pubkey::{pubkey, Pubkey};

fn mollusk_with_token_2022() -> Mollusk {
    let mut mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    mollusk
}

/// Discriminator for `admin_withdraw_deployer_bonus`.
const ADMIN_WITHDRAW_DEPLOYER_BONUS_DISC: [u8; 8] = [165, 43, 20, 244, 118, 201, 5, 5];

const REWARDS_AUTHORITY: Pubkey =
    pubkey!("FPFFavVNkhU8zp2vxaSKhSbCnRSRDiYhk2eZorbw2Nuh");

const TEST_INITIAL_BUY: u64 = 100_000_000; // 100 tokens with 6 decimals
const FUNDED_VAULT_LAMPORTS: u64 = 10_000_000_000; // 10 SOL

fn encode_admin_withdraw_deployer_bonus(amount: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 8);
    data.extend_from_slice(&ADMIN_WITHDRAW_DEPLOYER_BONUS_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

struct AdminWithdrawDeployerBonusAccounts {
    rewards_authority: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    creator: (Pubkey, Account),
    creator_token_account: (Pubkey, Account),
    deployer_vault: (Pubkey, Account),
}

impl AdminWithdrawDeployerBonusAccounts {
    fn happy() -> Self {
        let (rewards_authority, ra_acct) = make_signer_at(REWARDS_AUTHORITY);
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        // The creator is NOT a signer here — they're a recipient. Use a
        // fresh keypair; launch_state's `has_one = creator` binds it.
        let (creator, creator_acct) = make_signer();
        let (launch_state, launch_acct) =
            make_launch_state(&mint, &creator, |l| {
                l.initial_buy_amount = TEST_INITIAL_BUY;
            });
        let (creator_ata, creator_ata_acct) =
            make_token_account(&mint, &creator, TEST_INITIAL_BUY);
        let (deployer_vault, deployer_vault_acct) =
            make_deployer_vault(&mint, FUNDED_VAULT_LAMPORTS);

        AdminWithdrawDeployerBonusAccounts {
            rewards_authority: (rewards_authority, ra_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state, launch_acct),
            creator: (creator, creator_acct),
            creator_token_account: (creator_ata, creator_ata_acct),
            deployer_vault: (deployer_vault, deployer_vault_acct),
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.rewards_authority,
            self.mint,
            self.launch_state,
            self.creator,
            self.creator_token_account,
            self.deployer_vault,
        ]
    }
}

fn build_ix(accounts: &AdminWithdrawDeployerBonusAccounts, amount: u64) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_admin_withdraw_deployer_bonus(amount),
        vec![
            AccountMeta::new_readonly(accounts.rewards_authority.0, true), // signer, not mut
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new_readonly(accounts.launch_state.0, false),
            AccountMeta::new(accounts.creator.0, false), // mut (gets credited)
            AccountMeta::new_readonly(accounts.creator_token_account.0, false),
            AccountMeta::new(accounts.deployer_vault.0, false),
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

#[test]
fn admin_withdraw_deployer_bonus_zero_amount_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = AdminWithdrawDeployerBonusAccounts::happy();
    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected ZeroAmount");
    assert_eq!(expect_custom_error(&result), 6022);
}

#[test]
fn admin_withdraw_deployer_bonus_wrong_signer_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawDeployerBonusAccounts::happy();
    accounts.rewards_authority = make_signer(); // random pubkey, not REWARDS_AUTHORITY

    let ix = build_ix(&accounts, 1_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected UnauthorizedRewardsAuthority");
    assert_eq!(expect_custom_error(&result), 6031);
}

#[test]
fn admin_withdraw_deployer_bonus_no_initial_buy_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawDeployerBonusAccounts::happy();
    // launch_state with default initial_buy_amount = 0.
    accounts.launch_state =
        make_launch_state(&accounts.mint.0, &accounts.creator.0, |_| {});

    let ix = build_ix(&accounts, 1_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected NoInitialBuy");
    assert_eq!(expect_custom_error(&result), 6033);
}

/// The defense-in-depth gate. Even with a valid REWARDS_AUTHORITY signature
/// and an initial_buy_amount on file, if the creator has dumped below their
/// initial buy, the on-chain check blocks the withdrawal. This guards
/// against compromised-API drain scenarios.
#[test]
fn admin_withdraw_deployer_bonus_not_holding_initial_buy_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawDeployerBonusAccounts::happy();
    // Creator's ATA holds less than initial_buy_amount.
    accounts.creator_token_account =
        make_token_account(&accounts.mint.0, &accounts.creator.0, TEST_INITIAL_BUY / 2);

    let ix = build_ix(&accounts, 1_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected NotHoldingInitialBuy"
    );
    assert_eq!(expect_custom_error(&result), 6036);
}

#[test]
fn admin_withdraw_deployer_bonus_insufficient_vault_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawDeployerBonusAccounts::happy();
    // Vault has rent + tiny extra — far less than the 5 SOL we request.
    accounts.deployer_vault = make_deployer_vault(&accounts.mint.0, 1_000_000);

    let ix = build_ix(&accounts, 5_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientDeployerVaultBalance"
    );
    // InsufficientDeployerVaultBalance = offset 37 → 6037.
    assert_eq!(expect_custom_error(&result), 6037);
}

// ─── Happy-path test ────────────────────────────────────────────────────────

#[test]
fn admin_withdraw_deployer_bonus_happy_path() {
    let mollusk = mollusk_with_token_2022();
    let accounts = AdminWithdrawDeployerBonusAccounts::happy();

    let creator_pk = accounts.creator.0;
    let creator_before = accounts.creator.1.lamports;
    let vault_pk = accounts.deployer_vault.0;
    let vault_before = accounts.deployer_vault.1.lamports;

    let amount: u64 = 2_000_000_000; // 2 SOL
    let ix = build_ix(&accounts, amount);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    let creator_after = lookup_account(&result, &creator_pk);
    let vault_after = lookup_account(&result, &vault_pk);

    assert_eq!(
        creator_after.lamports,
        creator_before + amount,
        "creator should gain exactly {amount} lamports"
    );
    assert_eq!(
        vault_after.lamports,
        vault_before - amount,
        "deployer_vault should lose exactly {amount} lamports"
    );
}
