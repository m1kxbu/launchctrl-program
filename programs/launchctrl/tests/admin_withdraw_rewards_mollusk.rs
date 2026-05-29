//! Mollusk instruction-level tests for `admin_withdraw_rewards`.
//!
//! Diamond Hands payout path. The backend (`POST /api/staking/[mint]/claim`)
//! signs this ix with the `REWARDS_AUTHORITY` keypair to move SOL out of a
//! mint's `reward_vault` PDA to an eligible holder. Tight surface:
//!
//!   1. Signer pubkey must equal hardcoded `REWARDS_AUTHORITY`.
//!   2. Amount must be > 0.
//!   3. `amount <= vault.lamports() - rent_min(8)` — leave the PDA rent-exempt.
//!
//! Mirror of `admin_withdraw_deployer_bonus` but without the still-holding
//! gate (this vault is per-holder, not per-creator).
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test admin_withdraw_rewards_mollusk`

mod common;

use common::{make_mint, make_reward_vault, make_signer, make_signer_at, PROGRAM_ID};
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

/// Discriminator for `admin_withdraw_rewards` = sha256("global:admin_withdraw_rewards")[..8].
/// Matches the value documented in CLAUDE.md.
const ADMIN_WITHDRAW_REWARDS_DISC: [u8; 8] = [142, 189, 213, 13, 106, 136, 89, 42];

/// REWARDS_AUTHORITY pubkey — must match the hardcoded constant in
/// `programs/launchctrl/src/constants.rs:206`. Rotation = program upgrade,
/// so this is stable for v1.
const REWARDS_AUTHORITY: Pubkey =
    pubkey!("FPFFavVNkhU8zp2vxaSKhSbCnRSRDiYhk2eZorbw2Nuh");

/// Vault funding that leaves enough room above rent_min for the test
/// withdrawals. Default Solana rent on 8 bytes is ≈ 890_880 lamports.
const FUNDED_VAULT_LAMPORTS: u64 = 10_000_000_000; // 10 SOL

fn encode_admin_withdraw_rewards(amount: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 8);
    data.extend_from_slice(&ADMIN_WITHDRAW_REWARDS_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

struct AdminWithdrawRewardsAccounts {
    rewards_authority: (Pubkey, Account),
    mint: (Pubkey, Account),
    reward_vault: (Pubkey, Account),
    recipient: (Pubkey, Account),
}

impl AdminWithdrawRewardsAccounts {
    fn happy() -> Self {
        let (rewards_authority, ra_acct) = make_signer_at(REWARDS_AUTHORITY);
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (reward_vault, rv_acct) = make_reward_vault(&mint, FUNDED_VAULT_LAMPORTS);
        let (recipient, recipient_acct) = make_signer();

        AdminWithdrawRewardsAccounts {
            rewards_authority: (rewards_authority, ra_acct),
            mint: (mint, mint_acct),
            reward_vault: (reward_vault, rv_acct),
            recipient: (recipient, recipient_acct),
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.rewards_authority,
            self.mint,
            self.reward_vault,
            self.recipient,
        ]
    }
}

fn build_ix(accounts: &AdminWithdrawRewardsAccounts, amount: u64) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_admin_withdraw_rewards(amount),
        vec![
            AccountMeta::new(accounts.rewards_authority.0, true), // signer + mut
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.reward_vault.0, false),
            AccountMeta::new(accounts.recipient.0, false),
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

/// PROPERTY: `amount == 0` must revert with `ZeroAmount`.
#[test]
fn admin_withdraw_rewards_zero_amount_reverts() {
    let mollusk = mollusk_with_token_2022();
    let accounts = AdminWithdrawRewardsAccounts::happy();
    let ix = build_ix(&accounts, 0);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(result.program_result.is_err(), "expected ZeroAmount");
    assert_eq!(expect_custom_error(&result), 6022);
}

/// PROPERTY: a signer at any pubkey OTHER than `REWARDS_AUTHORITY` must
/// revert with `UnauthorizedRewardsAuthority`. This is the core admin gate
/// — without it, anyone could drain the reward_vault.
#[test]
fn admin_withdraw_rewards_wrong_signer_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawRewardsAccounts::happy();
    // Replace the signer with a random pubkey — NOT REWARDS_AUTHORITY.
    accounts.rewards_authority = make_signer();

    let ix = build_ix(&accounts, 1_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected UnauthorizedRewardsAuthority"
    );
    // UnauthorizedRewardsAuthority = offset 31 → 6031.
    assert_eq!(expect_custom_error(&result), 6031);
}

/// PROPERTY: `amount > vault.lamports() - rent_min` must revert with
/// `InsufficientRewardVaultBalance`. The check protects the vault's
/// rent-exempt floor — we can never drain it below the rent minimum,
/// because that would close the PDA and break future accruals.
#[test]
fn admin_withdraw_rewards_insufficient_vault_reverts() {
    let mollusk = mollusk_with_token_2022();
    let mut accounts = AdminWithdrawRewardsAccounts::happy();
    // Vault has only rent + 100 lamports of spendable.
    accounts.reward_vault = make_reward_vault(&accounts.mint.0, 1_000_000);

    // Try to pull 5 SOL — way more than available.
    let ix = build_ix(&accounts, 5_000_000_000);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected InsufficientRewardVaultBalance"
    );
    // InsufficientRewardVaultBalance = offset 32 → 6032.
    assert_eq!(expect_custom_error(&result), 6032);
}

// ─── Happy-path test ────────────────────────────────────────────────────────

/// PROPERTY: a valid REWARDS_AUTHORITY signer + amount within available
/// vault balance moves lamports correctly: vault -amount, recipient +amount,
/// rent-exempt floor preserved.
#[test]
fn admin_withdraw_rewards_happy_path() {
    let mollusk = mollusk_with_token_2022();
    let accounts = AdminWithdrawRewardsAccounts::happy();

    let recipient_pk = accounts.recipient.0;
    let recipient_before = accounts.recipient.1.lamports;
    let vault_pk = accounts.reward_vault.0;
    let vault_before = accounts.reward_vault.1.lamports;

    let amount: u64 = 1_500_000_000; // 1.5 SOL — comfortably below vault total + above rent
    let ix = build_ix(&accounts, amount);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        !result.program_result.is_err(),
        "expected success, got {:?}",
        result.program_result
    );

    let recipient_after = lookup_account(&result, &recipient_pk);
    let vault_after = lookup_account(&result, &vault_pk);

    assert_eq!(
        recipient_after.lamports,
        recipient_before + amount,
        "recipient should gain exactly {amount} lamports"
    );
    assert_eq!(
        vault_after.lamports,
        vault_before - amount,
        "vault should lose exactly {amount} lamports"
    );
    // Sanity: vault is still above rent floor (~890_880 lamports for 8 bytes).
    assert!(
        vault_after.lamports > 1_000_000,
        "vault should still be well above rent-exempt floor"
    );
}
