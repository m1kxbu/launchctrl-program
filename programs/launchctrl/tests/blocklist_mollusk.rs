//! Mollusk instruction-level tests for `add_to_blocklist` / `remove_from_blocklist`.
//!
//! These two instructions are the simplest fund-flow-adjacent surface — no
//! Token-2022 CPIs, just creator-signer + Anchor-typed state accounts +
//! mint (read-only). They protect the **F-4 blocklist-freeze invariant**
//! from the 2026-04-28 security audit: the blocklist must be sealed on the
//! first buy and immutable thereafter.
//!
//! Run: `SBF_OUT_DIR=$(pwd)/target/deploy cargo test --test blocklist_mollusk`

mod common;

use common::{make_blocklist, make_curve_state, make_mint, make_signer, PROGRAM_ID};
use mollusk_svm::Mollusk;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// Discriminator for `add_to_blocklist` = sha256("global:add_to_blocklist")[..8].
const ADD_TO_BLOCKLIST_DISC: [u8; 8] = [201, 138, 75, 216, 252, 201, 26, 106];

/// Helper: encode the `addresses: Vec<Pubkey>` argument as Borsh after the
/// discriminator. Layout: [4-byte little-endian length][N × 32 bytes].
fn encode_add_to_blocklist(addresses: &[Pubkey]) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 4 + addresses.len() * 32);
    data.extend_from_slice(&ADD_TO_BLOCKLIST_DISC);
    data.extend_from_slice(&(addresses.len() as u32).to_le_bytes());
    for addr in addresses {
        data.extend_from_slice(&addr.to_bytes());
    }
    data
}

/// Build an `add_to_blocklist` instruction with the given accounts in
/// canonical order. Mirrors the `AddToBlocklist` Accounts struct in
/// `programs/launchctrl/src/instructions/blocklist.rs`.
fn build_add_ix(
    creator: Pubkey,
    mint: Pubkey,
    blocklist: Pubkey,
    curve_state: Pubkey,
    addresses_to_add: &[Pubkey],
) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &encode_add_to_blocklist(addresses_to_add),
        vec![
            AccountMeta::new(creator, true), // signer + mut
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(blocklist, false), // mut
            AccountMeta::new_readonly(curve_state, false),
        ],
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// PROPERTY (F-4 invariant from 2026-04-28 audit): the on-chain
/// `!blocklist.frozen` constraint must reject any add attempt on a
/// blocklist that's already been frozen (first buy seals it). This is the
/// defining anti-rug guarantee for KOL Shield: list-at-launch, immutable-
/// thereafter.
///
/// Setup: blocklist with `frozen = true`. add_to_blocklist with any
/// addresses. Expected: `BlocklistFrozen` error (code 6028 = 6000 + variant
/// index 28).
#[test]
fn add_to_frozen_blocklist_reverts() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    let (creator_pk, creator_acct) = make_signer();
    let (mint_pk, mint_acct) = make_mint(6, 0);
    let (blocklist_pk, blocklist_acct) = make_blocklist(&mint_pk, &creator_pk, |b| {
        b.frozen = true; // ← the constraint violation we're testing
    });
    let (curve_pk, curve_acct) = make_curve_state(&mint_pk, &creator_pk, |_| {});

    let new_addr = Pubkey::new_unique();
    let ix = build_add_ix(creator_pk, mint_pk, blocklist_pk, curve_pk, &[new_addr]);
    let accounts = vec![
        (creator_pk, creator_acct),
        (mint_pk, mint_acct),
        (blocklist_pk, blocklist_acct),
        (curve_pk, curve_acct),
    ];

    let result = mollusk.process_instruction(&ix, &accounts);

    assert!(
        result.program_result.is_err(),
        "expected BlocklistFrozen error, got success: {:?}",
        result.program_result
    );
    // 6000 (Anchor user-error offset) + 28 (BlocklistFrozen variant index).
    // If this assertion ever drifts, check errors.rs ordering — the variant
    // index = its declaration order in the enum.
    let err_code = expect_custom_error(&result);
    assert_eq!(err_code, 6028, "wrong error code; got {}", err_code);
}

/// PROPERTY: the on-chain `blocklist.creator == creator.key()` constraint
/// must reject any add attempt by a signer that isn't the launch creator.
///
/// Setup: signer A creates the launch (sets `blocklist.creator = A`).
/// Signer B attempts to add to the blocklist. Expected: `Unauthorized`
/// error (code 6026 = 6000 + variant index 26).
#[test]
fn add_by_non_creator_reverts() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    let (creator_a_pk, _) = make_signer(); // real creator (used in state)
    let (creator_b_pk, creator_b_acct) = make_signer(); // attacker signer
    let (mint_pk, mint_acct) = make_mint(6, 0);
    // blocklist.creator = creator_a, but we try to add as creator_b.
    let (blocklist_pk, blocklist_acct) = make_blocklist(&mint_pk, &creator_a_pk, |_| {});
    let (curve_pk, curve_acct) = make_curve_state(&mint_pk, &creator_a_pk, |_| {});

    let ix = build_add_ix(
        creator_b_pk,
        mint_pk,
        blocklist_pk,
        curve_pk,
        &[Pubkey::new_unique()],
    );
    let accounts = vec![
        (creator_b_pk, creator_b_acct),
        (mint_pk, mint_acct),
        (blocklist_pk, blocklist_acct),
        (curve_pk, curve_acct),
    ];

    let result = mollusk.process_instruction(&ix, &accounts);

    assert!(
        result.program_result.is_err(),
        "expected Unauthorized error, got success: {:?}",
        result.program_result
    );
    let err_code = expect_custom_error(&result);
    assert_eq!(err_code, 6026, "wrong error code; got {}", err_code);
}

/// PROPERTY: empty addresses argument must revert with `ZeroAmount` from
/// the handler body. Validates that Anchor account validation passes for
/// well-formed state, and the handler-level guard fires.
#[test]
fn add_empty_addresses_reverts_with_zero_amount() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    let (creator_pk, creator_acct) = make_signer();
    let (mint_pk, mint_acct) = make_mint(6, 0);
    let (blocklist_pk, blocklist_acct) = make_blocklist(&mint_pk, &creator_pk, |_| {});
    let (curve_pk, curve_acct) = make_curve_state(&mint_pk, &creator_pk, |_| {});

    let ix = build_add_ix(creator_pk, mint_pk, blocklist_pk, curve_pk, &[]);
    let accounts = vec![
        (creator_pk, creator_acct),
        (mint_pk, mint_acct),
        (blocklist_pk, blocklist_acct),
        (curve_pk, curve_acct),
    ];

    let result = mollusk.process_instruction(&ix, &accounts);

    assert!(
        result.program_result.is_err(),
        "expected ZeroAmount error, got success: {:?}",
        result.program_result
    );
    // 6000 + 22 (ZeroAmount variant index).
    let err_code = expect_custom_error(&result);
    assert_eq!(err_code, 6022, "wrong error code; got {}", err_code);
}

/// PROPERTY: add_to_blocklist must reject after migration. The `curve_state`
/// constraint `!curve_state.is_complete` blocks any further blocklist
/// mutation once the bonding curve has filled.
#[test]
fn add_after_curve_complete_reverts() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    let (creator_pk, creator_acct) = make_signer();
    let (mint_pk, mint_acct) = make_mint(6, 0);
    let (blocklist_pk, blocklist_acct) = make_blocklist(&mint_pk, &creator_pk, |_| {});
    let (curve_pk, curve_acct) = make_curve_state(&mint_pk, &creator_pk, |c| {
        c.is_complete = true; // ← curve filled, no more blocklist edits
    });

    let ix = build_add_ix(
        creator_pk,
        mint_pk,
        blocklist_pk,
        curve_pk,
        &[Pubkey::new_unique()],
    );
    let accounts = vec![
        (creator_pk, creator_acct),
        (mint_pk, mint_acct),
        (blocklist_pk, blocklist_acct),
        (curve_pk, curve_acct),
    ];

    let result = mollusk.process_instruction(&ix, &accounts);

    assert!(
        result.program_result.is_err(),
        "expected CurveComplete error, got success: {:?}",
        result.program_result
    );
    // 6000 + 21 (CurveComplete variant index).
    let err_code = expect_custom_error(&result);
    assert_eq!(err_code, 6021, "wrong error code; got {}", err_code);
}

// Touch this if Mollusk changes its result API. Currently it surfaces the
// program result through `program_result.raw()` returning a u64 with the
// custom error code in the upper bits, or via the `ProgramError::Custom`
// variant when matched.
fn expect_custom_error(result: &mollusk_svm::result::InstructionResult) -> u32 {
    use solana_program_error::ProgramError;
    match &result.program_result {
        mollusk_svm::result::ProgramResult::Failure(ProgramError::Custom(code)) => *code,
        other => panic!("expected ProgramError::Custom, got {:?}", other),
    }
}

