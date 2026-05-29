# launchctrl-program

This repository is the source mirror for the on-chain Solana program that powers [inctrl.fun](https://inctrl.fun). It exists so the deployed program can be independently verified, tooled, and audited against public source.

The frontend, bot, and operational tooling are not part of this repository.

## Program

| | |
|---|---|
| Program ID (mainnet-beta) | `CTRLY9aJ4eSnFU1W3S9QUVoHhMme7T5fR8gMCpkTFuWe` |
| Upgrade authority | Squads V4 multisig `Gvqpq2sECBKdJbzVMD994d6Jcz5UnK2jXEN6PK4UFUgd` (2-of-3) |
| On-chain `security.txt` contact | `security@inctrl.fun` |

Tagged commits in this repo correspond to deployed mainnet program slots. The current deployment matches the tag at `mainnet-3858ad4` (`solana-verify get-program-hash` returns `746c6f111ab4dce92ac88e808d0e2916e20b7939bc3bc081072d48b46f5636d2`).

## Build

The program is built with the canonical Solana toolchain (CLI 3.1.10, platform-tools v1.52, Rust 1.89.0 for SBF).

Local reproducible build:

```bash
cargo build-sbf --features mainnet --manifest-path programs/launchctrl/Cargo.toml
```

Resulting `.so` at `target/deploy/launchctrl.so` matches the on-chain deployed program for the tagged commit.

## Verifying against the deployed program

Once the OtterSec verification image catches up with our toolchain, you can verify against the on-chain program with:

```bash
solana-verify get-program-hash -u https://api.mainnet-beta.solana.com \
  CTRLY9aJ4eSnFU1W3S9QUVoHhMme7T5fR8gMCpkTFuWe
```

and compare against the local `solana-verify get-executable-hash` of `target/deploy/launchctrl.so`.

## Reporting a vulnerability

Email `security@inctrl.fun`. The same address is embedded on-chain in the program's `security.txt` section.

Please do not file vulnerability reports as public GitHub issues.

## License

See `LICENSE`.
