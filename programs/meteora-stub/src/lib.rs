//! Meteora DAMM v2 test stub.
//!
//! Deployed under the real Meteora program ID
//! (`cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG`) during Mollusk tests
//! ONLY. It accepts any 8-byte discriminator and returns `Ok(())` without
//! mutating state. Purpose: let our `claim_and_reinject` +
//! `create_meteora_pool` ix dispatch their CPIs without erroring out on
//! "program not loaded" or "discriminator unknown".
//!
//! Limitations:
//!   - No state mutation. After a stubbed `claim_position_fee`, the
//!     wsol_vault still has whatever data we set up before the CPI.
//!   - No fee-flow simulation. Tests that depend on the stub crediting
//!     SOL/tokens must pre-populate the destination accounts.
//!   - NEVER deploy this to a real cluster. Building it pinned to the
//!     real Meteora program ID is for test loading via Mollusk's
//!     `add_program(&METEORA_PROGRAM_ID, "meteora_stub")` API; the
//!     resulting .so is not signed for or deployable to mainnet/devnet.

#![allow(unexpected_cfgs)]

use solana_account_info::AccountInfo;
use solana_program_entrypoint::{entrypoint, ProgramResult};
use solana_pubkey::Pubkey;

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    // Accept anything. Tests don't depend on this stub mutating state —
    // they pre-set account balances for the post-CPI reads (e.g.
    // wsol_vault.amount at offset 64..72 of Token-2022 packed data).
    Ok(())
}
