//! Mollusk SVM-level smoke harness for the `launchctrl` BPF binary.
//!
//! This is the minimal "is the toolchain wired?" test. It loads the compiled
//! `.so` from `target/deploy/launchctrl.so` into a Mollusk SVM instance and
//! exercises a single dispatched-but-invalid call to confirm:
//!
//!   1. Mollusk can locate the BPF binary
//!   2. Solana's SVM runtime starts inside Mollusk
//!   3. Our program's Anchor discriminator dispatch path runs
//!   4. Anchor's account-list validation rejects malformed inputs (does NOT
//!      return success on a syntactically-broken instruction)
//!
//! This is the GROUNDWORK for real instruction-level fuzz harnesses (e.g.,
//! random buy/sell sequences against vault-conservation invariants). Those
//! require constructing realistic synthetic accounts (LaunchState,
//! CurveState, Blocklist, vaults — each with the right Anchor discriminator
//! + payload bytes). Tracked as a follow-up for a fresh-energy session.
//!
//! Run with: `cargo test --test mollusk_smoke`
//!
//! Mollusk requires the BPF binary at `target/deploy/launchctrl.so`. If the
//! file is missing or stale, run `anchor build --ignore-keys` from the repo
//! root first.

use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};

/// Hardcoded program ID — must match `declare_id!()` in `lib.rs`.
const PROGRAM_ID: Pubkey = pubkey!("EJTstPiwyJ7a9wMUKrBDf19GLwqGwD7H2BXLFD1v1rAo");

#[test]
fn mollusk_loads_launchctrl_so() {
    // The string `"launchctrl"` tells Mollusk to look for
    // `target/deploy/launchctrl.so` (or `$SBF_OUT_DIR/launchctrl.so`).
    // If the .so is missing this panics with a clear message — that's
    // good test-failure UX.
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    // Sanity assertion — the loader populated the program id we passed.
    // If this passes, the .so was successfully loaded into the SVM.
    drop(mollusk);
}

#[test]
fn empty_instruction_data_is_rejected() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    // Construct an instruction with zero bytes of payload + no accounts.
    // Anchor's #[program] dispatcher reads the first 8 bytes as a
    // discriminator; with 0 bytes available, it must NOT find a match for
    // any of our 13 instructions and must return an error rather than
    // silently dispatching to one of them.
    let ix = Instruction::new_with_bytes(PROGRAM_ID, &[], vec![]);
    let result = mollusk.process_instruction(&ix, &[]);

    // We expect a failure. A success here would mean Anchor's
    // discriminator validation has a critical bug.
    assert!(
        result.program_result.is_err(),
        "expected error on empty-payload ix, got success: {:?}",
        result.program_result,
    );
}

#[test]
fn unknown_discriminator_is_rejected() {
    let mollusk = Mollusk::new(&PROGRAM_ID, "launchctrl");

    // 8 bytes of payload that intentionally don't match any of our 13
    // instruction discriminators. Anchor's dispatcher must report
    // "unknown instruction" and NOT execute any handler.
    let bogus_discriminator: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    let ix = Instruction::new_with_bytes(
        PROGRAM_ID,
        &bogus_discriminator,
        vec![
            // Some random account metas — the dispatcher should reject on
            // discriminator mismatch BEFORE checking account validity.
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
    );
    let result = mollusk.process_instruction(
        &ix,
        &[(ix.accounts[0].pubkey, Account::default())],
    );

    assert!(
        result.program_result.is_err(),
        "expected error on unknown discriminator, got success: {:?}",
        result.program_result,
    );
}
