//! Mollusk instruction-level tests for `claim_bootstrap`.
//!
//! `claim_bootstrap` is the one-shot 5 SOL pull from `deployer_vault` to the
//! launch creator. Designed to fund early-launch costs (Dexscreener listing,
//! initial marketing) before the LP flywheel has accrued enough to make the
//! per-cycle dev bonus meaningful.
//!
//! Eligibility surface (handler at `instructions/claim_bootstrap.rs:32-58`):
//!
//!   1. Caller is `LaunchState.creator` (Anchor `has_one = creator` + Signer).
//!   2. `launch.initial_buy_amount > 0` — creator made an initial buy.
//!   3. `!launch.bootstrap_claimed` — hasn't been claimed yet.
//!   4. `creator_token_account.amount >= initial_buy_amount` — still holding.
//!   5. `deployer_vault.lamports() >= rent_min(8) + 5 SOL` — vault is funded.
//!
//! Coverage:
//!   - 4 negative-path tests (one per eligibility gate, in handler order)
//!   - 1 happy-path test (verifies state transitions: flag flipped, lamports
//!     moved correctly from vault to creator)
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test claim_bootstrap_mollusk`

mod common;

use common::{
    make_deployer_vault, make_launch_state, make_mint, make_signer, make_token_account,
    PROGRAM_ID,
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

/// Discriminator for `claim_bootstrap` = sha256("global:claim_bootstrap")[..8].
const CLAIM_BOOTSTRAP_DISC: [u8; 8] = [74, 255, 9, 64, 60, 180, 29, 90];

/// `BOOTSTRAP_LAMPORTS` from `constants.rs`. Hardcoded here to assert against
/// it directly in the happy-path test (cross-check between test + program).
const BOOTSTRAP_LAMPORTS: u64 = 5_000_000_000;

/// claim_bootstrap takes no instruction args — just the 8-byte discriminator.
fn encode_claim_bootstrap() -> Vec<u8> {
    CLAIM_BOOTSTRAP_DISC.to_vec()
}

/// Initial-buy amount used by the "happy path" + most non-zero tests. Small
/// enough that funding the creator's ATA to >= this is trivial, large enough
/// to be obviously non-zero in logs.
const TEST_INITIAL_BUY: u64 = 100_000_000; // 100 tokens with 6 decimals

/// Vault lamport balance sufficient to satisfy the rent_min + 5 SOL gate.
/// Default Solana rent on 8 bytes is ≈ 890_880 lamports — well under 0.001
/// SOL. Using 10 SOL gives plenty of headroom.
const FUNDED_VAULT_LAMPORTS: u64 = 10_000_000_000;

struct ClaimBootstrapAccounts {
    creator: (Pubkey, Account),
    mint: (Pubkey, Account),
    launch_state: (Pubkey, Account),
    creator_token_account: (Pubkey, Account),
    deployer_vault: (Pubkey, Account),
}

impl ClaimBootstrapAccounts {
    /// Default "happy path" state: initial_buy_amount captured, not yet
    /// claimed, creator holding their initial buy, vault funded.
    fn happy() -> Self {
        let (creator, creator_acct) = make_signer();
        let (mint, mint_acct) = make_mint(6, 1_000_000_000_000_000);
        let (launch_state, launch_acct) =
            make_launch_state(&mint, &creator, |l| {
                l.initial_buy_amount = TEST_INITIAL_BUY;
                // bootstrap_claimed defaults to false
            });
        let (creator_ata, creator_ata_acct) =
            make_token_account(&mint, &creator, TEST_INITIAL_BUY);
        let (deployer_vault, deployer_vault_acct) =
            make_deployer_vault(&mint, FUNDED_VAULT_LAMPORTS);

        ClaimBootstrapAccounts {
            creator: (creator, creator_acct),
            mint: (mint, mint_acct),
            launch_state: (launch_state, launch_acct),
            creator_token_account: (creator_ata, creator_ata_acct),
            deployer_vault: (deployer_vault, deployer_vault_acct),
        }
    }

    fn to_account_vec(self) -> Vec<(Pubkey, Account)> {
        vec![
            self.creator,
            self.mint,
            self.launch_state,
            self.creator_token_account,
            self.deployer_vault,
        ]
    }
}

fn build_claim_bootstrap_ix(accounts: &ClaimBootstrapAccounts) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_claim_bootstrap(),
        vec![
            AccountMeta::new(accounts.creator.0, true), // signer + mut
            AccountMeta::new_readonly(accounts.mint.0, false),
            AccountMeta::new(accounts.launch_state.0, false),
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

/// Look up an account's post-execution state from `resulting_accounts`.
/// Panics if the pubkey isn't in the result — every pubkey we pass in
/// SHOULD come back out, so a missing one is a test-setup bug.
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

/// PROPERTY: `initial_buy_amount == 0` must revert with `NoInitialBuy`.
/// Guards against claiming bootstrap on a launch where the creator never
/// did their initial buy — the dev-bonus stream wouldn't be earning for
/// them anyway, and the gameability gate (initial buy = "skin in the game")
/// must not be bypassable via the bootstrap path.
#[test]
fn claim_bootstrap_no_initial_buy_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = ClaimBootstrapAccounts::happy();
    // Override: launch_state with default initial_buy_amount = 0.
    accounts.launch_state =
        make_launch_state(&accounts.mint.0, &accounts.creator.0, |_| {
            // Leave initial_buy_amount = 0 (default).
        });

    let ix = build_claim_bootstrap_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected NoInitialBuy error, got success"
    );
    // NoInitialBuy = offset 33 → 6033.
    assert_eq!(expect_custom_error(&result), 6033);
}

/// PROPERTY: `bootstrap_claimed == true` must revert with
/// `BootstrapAlreadyClaimed`. Guards the one-shot semantic — a creator
/// can only ever pull 5 SOL once per launch.
#[test]
fn claim_bootstrap_already_claimed_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = ClaimBootstrapAccounts::happy();
    accounts.launch_state =
        make_launch_state(&accounts.mint.0, &accounts.creator.0, |l| {
            l.initial_buy_amount = TEST_INITIAL_BUY;
            l.bootstrap_claimed = true;
        });

    let ix = build_claim_bootstrap_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected BootstrapAlreadyClaimed error, got success"
    );
    // BootstrapAlreadyClaimed = offset 34 → 6034.
    assert_eq!(expect_custom_error(&result), 6034);
}

/// PROPERTY: creator's ATA balance < `initial_buy_amount` must revert with
/// `NotHoldingInitialBuy`. This is the on-chain "skin in the game" gate —
/// devs only get bootstrap funds if they still hold their initial position
/// when claiming.
#[test]
fn claim_bootstrap_not_holding_initial_buy_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = ClaimBootstrapAccounts::happy();
    // Creator's ATA holds half of what they're supposed to hold.
    accounts.creator_token_account = make_token_account(
        &accounts.mint.0,
        &accounts.creator.0,
        TEST_INITIAL_BUY / 2,
    );

    let ix = build_claim_bootstrap_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected NotHoldingInitialBuy error, got success"
    );
    // NotHoldingInitialBuy = offset 36 → 6036.
    assert_eq!(expect_custom_error(&result), 6036);
}

/// PROPERTY: `deployer_vault.lamports() < rent_min + 5 SOL` must revert
/// with `BootstrapNotReady`. Prevents draining the vault below rent-exempt
/// (which would close the PDA and break future dev-bonus accrual) and
/// guards against trying to claim before the LP flywheel has actually
/// accumulated enough.
#[test]
fn claim_bootstrap_vault_not_ready_reverts() {
    let mollusk = mollusk_with_token_2022();

    let mut accounts = ClaimBootstrapAccounts::happy();
    // Vault holds rent-exempt minimum + only 1 lamport — not enough for 5 SOL.
    accounts.deployer_vault =
        make_deployer_vault(&accounts.mint.0, 1_000_000); // ~1M lamports total

    let ix = build_claim_bootstrap_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    assert!(
        result.program_result.is_err(),
        "expected BootstrapNotReady error, got success"
    );
    // BootstrapNotReady = offset 35 → 6035.
    assert_eq!(expect_custom_error(&result), 6035);
}

// ─── Happy-path test ────────────────────────────────────────────────────────

/// PROPERTY: under the full eligibility surface, claim_bootstrap moves
/// exactly 5 SOL from the deployer_vault to the creator's wallet and flips
/// the launch state's `bootstrap_claimed` flag. This is the only test that
/// asserts a POSITIVE behavior — the rest are reverts.
#[test]
fn claim_bootstrap_happy_path_pays_5_sol() {
    let mollusk = mollusk_with_token_2022();
    let accounts = ClaimBootstrapAccounts::happy();

    let creator_pk = accounts.creator.0;
    let creator_lamports_before = accounts.creator.1.lamports;
    let deployer_vault_pk = accounts.deployer_vault.0;
    let deployer_vault_lamports_before = accounts.deployer_vault.1.lamports;
    let launch_state_pk = accounts.launch_state.0;

    let ix = build_claim_bootstrap_ix(&accounts);
    let result = mollusk.process_instruction(&ix, &accounts.to_account_vec());

    // The handler should succeed.
    assert!(
        !result.program_result.is_err(),
        "expected claim_bootstrap to succeed, got {:?}",
        result.program_result
    );

    // Creator gained exactly 5 SOL.
    let creator_after = lookup_account(&result, &creator_pk);
    assert_eq!(
        creator_after.lamports,
        creator_lamports_before + BOOTSTRAP_LAMPORTS,
        "creator should have gained exactly 5 SOL"
    );

    // Deployer vault lost exactly 5 SOL.
    let vault_after = lookup_account(&result, &deployer_vault_pk);
    assert_eq!(
        vault_after.lamports,
        deployer_vault_lamports_before - BOOTSTRAP_LAMPORTS,
        "deployer_vault should have lost exactly 5 SOL"
    );

    // `bootstrap_claimed` flag flipped. The flag is at the end of LaunchState
    // after Borsh serialization. Easier check: re-deserialize through the
    // anchor account header and inspect the field. But for a one-shot test,
    // a re-claim attempt is a stronger functional test — the second claim
    // MUST fail with BootstrapAlreadyClaimed, proving the flag persisted.
    //
    // We don't actually re-run because the deployer vault is now under
    // the 5 SOL threshold (lost 5 SOL above, so it'd hit BootstrapNotReady
    // instead). Instead, decode the boolean directly from the account data.
    let launch_after = lookup_account(&result, &launch_state_pk);
    // LaunchState layout after the 8-byte Anchor discriminator:
    //   creator: Pubkey                    32 bytes  (offset 8)
    //   mint: Pubkey                       32 bytes  (offset 40)
    //   name: String                       4 + len   (offset 72)
    //   symbol: String                     4 + len   (variable)
    //   uri: String                        4 + len   (variable)
    //   total_supply: u64                  8 bytes
    //   decimals: u8                       1 byte
    //   launch_timestamp: i64              8 bytes
    //   decay_schedule: Vec<DecayStep>     4 + len*10
    //   migration_threshold_lamports: u64  8 bytes
    //   is_migrated: bool                  1 byte
    //   bump: u8                           1 byte
    //   fee_authority_bump: u8             1 byte
    //   migration_timestamp: i64           8 bytes
    //   drip_total: u64                    8 bytes
    //   drip_injected: u64                 8 bytes
    //   meteora_pool: Pubkey               32 bytes
    //   drip_vault_bump: u8                1 byte
    //   migration_sol_vault_bump: u8       1 byte
    //   initial_buy_amount: u64            8 bytes
    //   bootstrap_claimed: bool            1 byte  ← target
    //
    // Variable-length strings + Vec make computing the offset by hand
    // brittle. Use Borsh deserialization through Anchor's deserialize fn
    // for correctness rather than offset math.
    use anchor_lang::AccountDeserialize;
    let mut data_slice: &[u8] = &launch_after.data;
    let launch_decoded =
        launchctrl::state::LaunchState::try_deserialize(&mut data_slice)
            .expect("LaunchState should deserialize from post-ix data");
    assert!(
        launch_decoded.bootstrap_claimed,
        "bootstrap_claimed flag should be true after successful claim"
    );
    assert_eq!(
        launch_decoded.initial_buy_amount, TEST_INITIAL_BUY,
        "initial_buy_amount should be unchanged"
    );
}
