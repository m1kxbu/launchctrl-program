//! Shared synthetic-account constructors for Mollusk-based tests.
//!
//! Each helper returns a `(Pubkey, Account)` tuple in the shape Mollusk's
//! `process_instruction(&ix, &accounts)` accepts. The helpers handle:
//!
//!   - Anchor's 8-byte discriminator prefix on `#[account]`-typed state
//!   - Borsh serialization of the payload struct fields
//!   - Owner = `launchctrl::ID` for Anchor-owned accounts
//!   - Rent-exempt-ish lamport balance (good enough; Mollusk doesn't enforce)
//!   - PDA derivation matching the on-chain seed pattern
//!
//! The Mint helper produces a bare Token-2022 Mint (82 bytes, no extensions)
//! whose only role in these tests is to satisfy Anchor's
//! `InterfaceAccount<'info, Mint>` deserialization + the owner-equals-
//! Token-2022 constraint. We never CPI into the Token-2022 program, so it
//! doesn't need to be loaded into Mollusk.

use anchor_lang::{AnchorSerialize, Discriminator};
use launchctrl::constants::MAX_BLOCKLIST_SIZE;
use launchctrl::state::{
    Blocklist, CurveState, DeployerVault, LaunchState, LpSeedSolVault, MigrationSolVault,
    RewardVault, SolVault,
};
use solana_account::Account;
use solana_pubkey::{pubkey, Pubkey};

/// The deployed `launchctrl` program ID. Matches `declare_id!()` in lib.rs.
pub const PROGRAM_ID: Pubkey = pubkey!("EJTstPiwyJ7a9wMUKrBDf19GLwqGwD7H2BXLFD1v1rAo");

/// SPL Token-2022 program ID. Hardcoded because we don't load the actual
/// program — Mint accounts just need this in their `owner` field for
/// `InterfaceAccount<Mint>`'s validation to accept them.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// System program ID — used for plain SOL transfer accounts (signer, etc.).
pub const SYSTEM_PROGRAM_ID: Pubkey =
    pubkey!("11111111111111111111111111111111");

// ─── Anchor account construction ────────────────────────────────────────────

/// Wrap a Borsh-encoded payload with the Anchor discriminator prefix and
/// pad to `padded_size` total bytes (matching the on-chain `space = ...`
/// allocation). Padding bytes are zero — Anchor's `try_deserialize` reads
/// only the prefix + struct, so trailing zeros are ignored.
fn encode_anchor_account<T: AnchorSerialize + Discriminator>(
    state: &T,
    padded_size: usize,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(padded_size);
    data.extend_from_slice(T::DISCRIMINATOR);
    state
        .serialize(&mut data)
        .expect("Borsh serialization of state struct cannot fail for owned values");
    if data.len() < padded_size {
        data.resize(padded_size, 0);
    }
    data
}

/// Build a synthetic CurveState account at the canonical PDA for `mint`.
/// Returns the PDA pubkey + the Account struct ready for Mollusk.
pub fn make_curve_state(
    mint: &Pubkey,
    creator: &Pubkey,
    state_overrides: impl FnOnce(&mut CurveState),
) -> (Pubkey, Account) {
    let (pda, bump) = Pubkey::find_program_address(&[b"curve", mint.as_ref()], &PROGRAM_ID);
    let (_, vault_bump) =
        Pubkey::find_program_address(&[b"curve_vault", mint.as_ref()], &PROGRAM_ID);
    let (_, sol_vault_bump) =
        Pubkey::find_program_address(&[b"sol_vault", mint.as_ref()], &PROGRAM_ID);

    let mut state = CurveState {
        mint: *mint,
        creator: *creator,
        // Production defaults from launchctrl's curve init.
        virtual_sol_reserves: 70_000_000_000, // 70 SOL
        virtual_token_reserves: 1_160_000_000_000_000, // 1.16B with 6 decimals
        real_sol_reserves: 0,
        real_token_reserves: 0,
        migration_threshold_lamports: 120_000_000_000, // 120 SOL default
        creation_slot: 0,
        last_buy_slot: 0,
        is_complete: false,
        is_funds_released: false,
        bump,
        vault_bump,
        sol_vault_bump,
        buys_this_slot: 0,
    };
    state_overrides(&mut state);

    let padded_size = CurveState::LEN;
    let data = encode_anchor_account(&state, padded_size);
    (
        pda,
        Account {
            lamports: 1_000_000, // rent-ish, not enforced by Mollusk
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// Build a synthetic Blocklist account at the canonical PDA for `mint`.
pub fn make_blocklist(
    mint: &Pubkey,
    creator: &Pubkey,
    state_overrides: impl FnOnce(&mut Blocklist),
) -> (Pubkey, Account) {
    let (pda, bump) =
        Pubkey::find_program_address(&[b"blocklist", mint.as_ref()], &PROGRAM_ID);

    let mut state = Blocklist {
        mint: *mint,
        creator: *creator,
        blocked: Vec::new(),
        bump,
        frozen: false,
    };
    state_overrides(&mut state);

    let padded_size = Blocklist::space(MAX_BLOCKLIST_SIZE);
    let data = encode_anchor_account(&state, padded_size);
    (
        pda,
        Account {
            lamports: 1_000_000,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// Build a synthetic LaunchState account at the canonical PDA for `mint`.
///
/// Note: only sets the fields the test surface needs. Many fields are
/// vestigial per CLAUDE.md (`fee_authority_bump`, `drip_total`,
/// `drip_injected`, etc.) and we leave them at default values.
pub fn make_launch_state(
    mint: &Pubkey,
    creator: &Pubkey,
    state_overrides: impl FnOnce(&mut LaunchState),
) -> (Pubkey, Account) {
    let (pda, bump) =
        Pubkey::find_program_address(&[b"launch", mint.as_ref()], &PROGRAM_ID);

    let mut state = LaunchState {
        creator: *creator,
        mint: *mint,
        name: "Test".to_string(),
        symbol: "TST".to_string(),
        uri: "https://example.com/meta.json".to_string(),
        total_supply: 1_000_000_000_000_000, // 1B with 6 decimals
        decimals: 6,
        launch_timestamp: 0,
        decay_schedule: Vec::new(), // empty schedule → flat 1% via on-chain floor
        migration_threshold_lamports: 120_000_000_000,
        is_migrated: false,
        bump,
        fee_authority_bump: 0, // vestigial
        migration_timestamp: 0,
        drip_total: 0,         // vestigial
        drip_injected: 0,      // vestigial
        meteora_pool: Pubkey::default(),
        drip_vault_bump: 0,           // vestigial
        migration_sol_vault_bump: 0,  // vestigial
        initial_buy_amount: 0,
        bootstrap_claimed: false,
    };
    state_overrides(&mut state);

    let padded_size = LaunchState::LEN;
    let data = encode_anchor_account(&state, padded_size);
    (
        pda,
        Account {
            lamports: 1_000_000,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

// ─── SPL Token-2022 Mint (via mollusk-svm-programs-token helpers) ───────────

/// Build a bare Token-2022 Mint via the official Anza helper. Uses
/// `spl_token_interface::state::Mint` for type-correct packing — replaces
/// our earlier hand-rolled byte layout which failed Anchor's
/// `InterfaceAccount<Mint>` deserialization with `AccountDataTooSmall`.
pub fn make_mint(decimals: u8, supply: u64) -> (Pubkey, Account) {
    use spl_token_interface::state::Mint;
    let mint_pubkey = Pubkey::new_unique();
    let account = mollusk_svm_programs_token::token2022::create_account_for_mint(Mint {
        mint_authority: solana_program_option::COption::None,
        supply,
        decimals,
        is_initialized: true,
        freeze_authority: solana_program_option::COption::None,
    });
    (mint_pubkey, account)
}

// ─── SPL Token-2022 Account (via mollusk-svm-programs-token helpers) ────────

pub fn make_packed_token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    use spl_token_interface::state::{Account as SplTokenAccount, AccountState};
    mollusk_svm_programs_token::token2022::create_account_for_token_account(SplTokenAccount {
        mint: *mint,
        owner: *owner,
        amount,
        delegate: solana_program_option::COption::None,
        state: AccountState::Initialized,
        is_native: solana_program_option::COption::None,
        delegated_amount: 0,
        close_authority: solana_program_option::COption::None,
    })
}

/// Token-2022 token account at a fresh pubkey, owned by the given wallet
/// pubkey. Used for buyer ATAs in tests.
pub fn make_token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> (Pubkey, Account) {
    (Pubkey::new_unique(), make_packed_token_account(mint, owner, amount))
}

/// curve_token_vault PDA — Token-2022 account at the canonical
/// `["curve_vault", mint]` PDA, whose authority is the PDA itself (the
/// program signs as the PDA when releasing tokens to buyers).
pub fn make_curve_token_vault(mint: &Pubkey, amount: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"curve_vault", mint.as_ref()], &PROGRAM_ID);
    (pda, make_packed_token_account(mint, &pda, amount))
}

/// Build the sol_vault PDA — system-owned UncheckedAccount, just holds
/// lamports. Used by buy/sell for the SOL leg of trades.
pub fn make_sol_vault(mint: &Pubkey, lamports: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"sol_vault", mint.as_ref()], &PROGRAM_ID);
    (
        pda,
        Account {
            lamports,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// Build the lp_seed_sol_vault PDA — program-owned, zero-data marker
/// (`#[account] pub struct LpSeedSolVault {}`). Data is just the 8-byte
/// Anchor discriminator; lamports field accumulates retained sell-fee SOL.
pub fn make_lp_seed_sol_vault(mint: &Pubkey, lamports: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"lp_seed_sol", mint.as_ref()], &PROGRAM_ID);
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(LpSeedSolVault::DISCRIMINATOR);
    (
        pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// lp_seed_token_vault — Token-2022 account at the canonical
/// `["lp_seed_tok", mint]` PDA. Mirror of curve_token_vault but for the
/// buyback-token accumulator. Authority is the PDA itself.
pub fn make_lp_seed_token_vault(mint: &Pubkey, amount: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"lp_seed_tok", mint.as_ref()], &PROGRAM_ID);
    (pda, make_packed_token_account(mint, &pda, amount))
}

/// Build the deployer_vault PDA — program-owned, zero-data marker
/// (`#[account] pub struct DeployerVault {}`). Same layout as LpSeedSolVault
/// (8-byte discriminator only); lamports field accumulates the 6.25% dev-bonus
/// slice from `claim_and_reinject` and pays out the 5 SOL bootstrap.
pub fn make_deployer_vault(mint: &Pubkey, lamports: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"deployer_vault", mint.as_ref()], &PROGRAM_ID);
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(DeployerVault::DISCRIMINATOR);
    (
        pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

// ─── Signer / system-owned account ──────────────────────────────────────────

/// Build a system-owned signer account (a "wallet"). Mollusk distinguishes
/// signer-ness via `AccountMeta::new(pubkey, true)` on the instruction —
/// this helper just provides the account data with non-zero lamports.
pub fn make_signer() -> (Pubkey, Account) {
    let pubkey = Pubkey::new_unique();
    (pubkey, make_signer_account(100_000_000_000))
}

/// Build a system-owned signer account at a SPECIFIC pubkey. Useful for
/// privileged-signer tests where the on-chain constraint pins the signer
/// to a hardcoded address (e.g. `REWARDS_AUTHORITY`). Mollusk doesn't
/// verify cryptographic signatures — `AccountMeta::new(pk, true)` is
/// enough for the runtime to treat it as signed.
pub fn make_signer_at(pubkey: Pubkey) -> (Pubkey, Account) {
    (pubkey, make_signer_account(100_000_000_000))
}

fn make_signer_account(lamports: u64) -> Account {
    Account {
        lamports,
        data: Vec::new(),
        owner: SYSTEM_PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// TYPED variant of make_sol_vault. The `sol_vault` PDA is treated as
/// `UncheckedAccount` in buy/sell (no Anchor validation; raw lamport
/// mutations) but as `Account<'info, SolVault>` in migrate + initialize_curve
/// (typed validation). For migrate-style tests, supply a program-owned
/// account with the SolVault discriminator.
pub fn make_sol_vault_typed(mint: &Pubkey, lamports: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"sol_vault", mint.as_ref()], &PROGRAM_ID);
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(SolVault::DISCRIMINATOR);
    (
        pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// Empty system-owned account at a target PDA. Used for `init`-style PDAs
/// where Anchor will run the create_account CPI to claim the slot (any
/// pre-existing data would fail the init check). Also used for raw
/// "uninitialized" accounts that the handler will create_account inside
/// the ix (e.g. `migration_token_vault` in migrate_to_pool).
pub fn make_uninit_pda(pda: Pubkey) -> (Pubkey, Account) {
    (
        pda,
        Account {
            lamports: 0,
            data: Vec::new(),
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}

/// Derive a PDA at `[seed, mint]` against the launchctrl program ID.
/// Convenience for tests that need the pubkey but no account payload
/// (e.g. `migration_authority` UncheckedAccount).
pub fn derive_mint_pda(seed: &[u8], mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, mint.as_ref()], &PROGRAM_ID).0
}

/// Build the reward_vault PDA — program-owned, zero-data marker
/// (`#[account] pub struct RewardVault {}`). Receives 25% of each
/// `claim_and_reinject` cycle's wSOL share; drained via
/// `admin_withdraw_rewards`.
pub fn make_reward_vault(mint: &Pubkey, lamports: u64) -> (Pubkey, Account) {
    let (pda, _) =
        Pubkey::find_program_address(&[b"reward_vault", mint.as_ref()], &PROGRAM_ID);
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(RewardVault::DISCRIMINATOR);
    (
        pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
}
