<p align="center">
  <img src="https://raw.githubusercontent.com/oakoliver/oak-oil/main/docs/banner.png" alt="Oak Oil: lean, fast Rust builds" width="100%">
</p>

# Oak Oil

The WD-40 for Rust builds. Oak Oil removes build "rust" (stale and duplicate
artifacts) and keeps Rust builds moving: Cargo plans the build, Oak Oil
executes it against a content-addressed store, and the output is
byte-identical to what stock Cargo would produce.

> **Status: experimental.** macOS only (Apple Silicon tested), Rust 1.95.
> Verified on four real workspaces; not yet on yours. Stock Cargo keeps
> working on the same target dir at any time.

## What it does

| Command | What it does |
| --- | --- |
| `cargo oil build [ARGS]` | Builds like `cargo build [ARGS]`. The first run records Cargo's plan; later runs execute it without Cargo, restore unchanged units from the store, and compile the rest with stock rustc. |
| `cargo oil clean [--apply]` | Finds build residue Cargo will never read again (superseded unit variants, old incremental sessions), removes it with `--apply`, then checks Cargo recompiles nothing. |
| `cargo oil measure` | Reports bytes per kind of waste across every target dir on the machine. |
| `cargo oil wait-store` | Waits for background store writes to finish. |

## Results

Median of 3, stock Cargo and Oak Oil alternating, same machine and settings,
incremental compilation on (Cargo's default for dev builds). Project C has
138 workspace crates and 169 packages; the edit changes a library ~125
crates depend on.

| Scenario | Stock | Oak Oil | |
| --- | --- | --- | --- |
| Clean build | 10.69 s | 10.60 s | 1.0× |
| No-change build | 0.15 s | 0.14 s | 1.1× |
| Comment edit | 2.56 s | 1.97 s | 1.3× |
| Private code edit | 3.02 s | 2.79 s | 1.1× |
| Revert the edit | 3.23 s | 0.58 s | 5.5× |
| `rm -rf target`, build | 10.90 s | 1.66 s | 6.6× |
| Second checkout of the same project | 10.96 s | 9.52 s | 1.15× |

On a smaller workspace (5 crates, 104 packages), edits are at parity, a
clean build is 1.04× and reuse scenarios are 9–18× faster. Filling the store
is not free: it runs after the build in a background process on the
efficiency cores (about 5.5 s after a clean build of Project C); no build
waits for it.

Cleanup on a real 46 GiB target dir freed 10.1 GiB, after which Cargo
recompiled nothing. More detail: [docs/findings.md](docs/findings.md).

## How it works

1. **Plan.** The first `cargo oil build` runs stock Cargo with Oak Oil as
   `RUSTC_WRAPPER` and records every rustc invocation (command, env,
   outputs). The plan is rebuilt only when manifests, the lockfile, configs,
   the toolchain, env or build-script inputs change.
2. **Execute.** Later builds run the plan with Oak Oil's scheduler:
   `.rmeta` pipelining, a Cargo-style jobserver, critical path first.
3. **Freshness.** A unit whose inputs and outputs are unchanged (size,
   mtime, inode) is skipped without reading anything. A changed unit gets a
   content key: rustc version, command, env, dependency contents, and the
   sources from rustc's own dep-info. A key in the store is restored
   (APFS clone); a miss runs stock rustc.
4. **Compatibility.** Cargo's fingerprints, build-script outputs and
   dep-info are restored in the order Cargo's checks expect, so a later
   plain `cargo build` finds everything fresh.

Library artifacts from registry and git crates are shared across projects.
Linked outputs and workspace crates are per project, because macOS debug
maps and dep-info hold absolute paths.

## Verifying it

`ci/oracle.py` checks Oak Oil's contract on a small workspace
(`ci/fixture`) on every push: a cold build, a no-change build, a restore
from the store and an edit are byte-identical to stock Cargo, stock Cargo
recompiles nothing afterwards, and `clean` keeps everything Cargo uses.

`lab/` holds the heavier checks used during development:

- `phase1.py` builds a project with stock Cargo and with Oak Oil at the same
  path and compares every output by sha256, then checks stock Cargo
  recompiles nothing afterwards. Non-incremental builds are used, where stock
  Cargo is itself byte-reproducible.
- `bench.py` runs the scenarios above for both tools.

## Going back to plain Cargo

Nothing to undo. Oak Oil writes Cargo's own layout into the target dir and
keeps its plan in `target/oakoil/`; plain `cargo build` works at any time.
The store lives in `~/.oakoil` (or `$OAKOIL_HOME`) and can be deleted.

## Limitations

- macOS only: uses `nm` debug maps, APFS clones, xattrs and thread QoS.
- No store garbage collection yet; `~/.oakoil` grows until deleted.
- `cargo oil clean` runs your Cargo commands to find live units, which
  brings out-of-date profiles up to date first.
- Early cutoff after a private-code edit is limited: rustc stores source
  hashes and line tables in `.rmeta`, so dependents usually rebuild.

## Install

```sh
cargo install cargo-oil        # build from the crates.io source
cargo binstall cargo-oil       # or: download the prebuilt binary of the release
cargo oil build
```

Prebuilt binaries (Apple Silicon and Intel) are on the
[releases page](https://github.com/oakoliver/oak-oil/releases); `cargo binstall`
fetches them from there.

## Where releases come from

Every release is built and published by
[`.github/workflows/release.yml`](.github/workflows/release.yml) from a
`v*` tag, after the same checks as CI (lint, tests, and the oracle below):

- **Binaries** on the GitHub release carry a signed build provenance
  attestation linking them to the workflow run and commit:
  `gh attestation verify cargo-oil-<version>-<target>.tar.gz --repo oakoliver/oak-oil`.
  `SHA256SUMS` lists their checksums.
- **The crate** is published to crates.io by the same workflow through
  trusted publishing (OIDC); no long-lived token exists. The published
  crate records the git commit it was built from.
- Workflow actions are pinned to full commit SHAs.

## License

MIT or Apache-2.0, at your option.
