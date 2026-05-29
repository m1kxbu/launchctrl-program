use anchor_lang::prelude::*;

#[error_code]
pub enum LaunchCtrlError {
    // ── Launch / token creation ─────────────────────────────────────────────
    #[msg("Token name must be 32 characters or fewer")]
    NameTooLong,
    #[msg("Token symbol must be 10 characters or fewer")]
    SymbolTooLong,
    #[msg("Token metadata URI must be 200 characters or fewer")]
    UriTooLong,
    #[msg("Total supply must be greater than zero")]
    InvalidSupply,
    #[msg("Initial fee cannot exceed 50% (5000 bps)")]
    FeeTooHigh,
    #[msg("Mint must be owned by the Token-2022 program")]
    InvalidMintOwner,
    #[msg("Mint must have no Token-2022 extensions — TransferFee/TransferHook/etc are forbidden")]
    MintHasUnexpectedExtensions,
    #[msg("Decay schedule fee_bps must be ≤ 5000 (50% — sell-side rug-prevention cap)")]
    InvalidDecayBps,
    #[msg("Decay schedule seconds_after_launch must be strictly increasing")]
    InvalidDecaySchedule,
    #[msg("Migration threshold must be between 1 and 200 SOL")]
    MigrationThresholdOutOfRange,

    // ── Migration / pool ────────────────────────────────────────────────────
    #[msg("Token has already been migrated to pool")]
    AlreadyMigrated,
    #[msg("Token has not yet migrated to pool")]
    NotMigrated,
    #[msg("Pool address has already been set")]
    PoolAlreadySet,
    #[msg("Meteora pool has not been created yet — run create_meteora_pool first")]
    PoolNotCreated,
    #[msg("Invalid program account")]
    InvalidProgram,
    #[msg("Insufficient funds in migration vaults to create pool")]
    InsufficientMigrationFunds,
    #[msg("Migration funds have already been released")]
    AlreadyReleased,
    #[msg("Bonding curve has not yet completed — migration threshold not reached")]
    CurveNotComplete,
    #[msg("init_sqrt_price is more than 10% away from the price implied by migration vault contents")]
    InvalidInitSqrtPrice,
    #[msg("Pool initialization left more than 10% of migration tokens stranded — liquidity_delta too small")]
    InsufficientPoolLiquidity,
    #[msg("LP reinjection consumed zero wSOL — liquidity_delta too small")]
    InsufficientLpReinjection,

    // ── Curve trading ───────────────────────────────────────────────────────
    #[msg("Bonding curve has completed — token has migrated to Meteora")]
    CurveComplete,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Slippage tolerance exceeded")]
    SlippageExceeded,
    #[msg("Insufficient liquidity in curve")]
    InsufficientLiquidity,
    #[msg("Wallet is blocklisted for this token")]
    Blocked,
    #[msg("Only the token creator can modify the blocklist")]
    Unauthorized,
    #[msg("Blocklist is full — maximum 50 addresses per token")]
    BlocklistFull,
    #[msg("Blocklist is frozen — first buy permanently sealed it; no further edits allowed")]
    BlocklistFrozen,
    #[msg("Creator initial buy cannot exceed 2% of token supply")]
    CreatorBuyCapExceeded,
    #[msg("Too many buys in this slot — try again next slot")]
    SlotBuyLimitReached,

    // ── Diamond Hands rewards ───────────────────────────────────────────────
    #[msg("Caller is not the configured rewards authority")]
    UnauthorizedRewardsAuthority,
    #[msg("Reward vault has insufficient balance for this withdrawal")]
    InsufficientRewardVaultBalance,

    // ── Deployer bonus / bootstrap claim (NEST_EGG_TO_DEV_BONUS rework) ─────
    #[msg("Creator never made an initial buy at launch — deployer bonus and bootstrap are unavailable")]
    NoInitialBuy,
    #[msg("Bootstrap claim has already been used for this launch")]
    BootstrapAlreadyClaimed,
    #[msg("Deployer vault has not yet accumulated enough lamports for the bootstrap claim")]
    BootstrapNotReady,
    #[msg("Creator no longer holds at least their initial buy amount — bootstrap requires still holding")]
    NotHoldingInitialBuy,
    #[msg("Deployer vault has insufficient balance for this withdrawal")]
    InsufficientDeployerVaultBalance,

    // ── Ricochet (inline bot-platform protection) ───────────────────────────
    #[msg("Transaction includes a program not on the Ricochet allowlist")]
    UnauthorizedPlatform,
    #[msg("Ricochet config does not match the mint")]
    RicochetMintMismatch,
    #[msg("Ricochet duration must be between 1 second and 24 hours")]
    RicochetDurationOutOfRange,

    // ── Shared ──────────────────────────────────────────────────────────────
    #[msg("Math overflow")]
    MathOverflow,
}
