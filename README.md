<p align="center">
  <img src="https://raw.githubusercontent.com/oakoliver/oak-oil/main/docs/banner.png" alt="Oak Oil: lean, fast Rust builds" width="100%">
</p>

# Oak Oil

Lean, fast Rust builds. Oak Oil removes build "rust" (stale and duplicate
artifacts) and keeps Rust builds moving: Cargo plans the build, Oak Oil
executes it against a content-addressed store, and the output is
byte-identical to what stock Cargo would produce.

> **Status: experimental.** macOS (Apple Silicon tested) and Linux (new:
> btrfs, ext4 and XFS checked), Rust 1.95. Verified on four real workspaces
> on macOS; not yet on yours. Stock Cargo keeps
> working on the same target dir at any time.

## What it does

| Command | What it does |
| --- | --- |
| `cargo oil build [ARGS]` | Builds like `cargo build [ARGS]`. The first run records Cargo's plan; later runs execute it without Cargo, restore unchanged units from the store, and compile the rest with stock rustc. |
| `cargo oil clean [--apply]` | Finds build residue Cargo will never read again (superseded unit variants, old incremental sessions), removes it with `--apply`, then checks Cargo recompiles nothing. |
| `cargo oil measure` | Reports bytes per kind of waste across every target dir on the machine. |
| `cargo oil gc [--max-age-days N] [--max-size SIZE] [--dry-run]` | Trims the store: objects unused for 30 days, then least recently used until it fits 40 GiB. Also runs after builds, at most once a day. |
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
   (a clone on APFS, btrfs and XFS; a copy elsewhere); a miss runs stock
   rustc.
4. **Compatibility.** Cargo's fingerprints, build-script outputs and
   dep-info are restored in the order Cargo's checks expect, so a later
   plain `cargo build` finds everything fresh.

Library artifacts from registry and git crates are shared across projects.
Linked outputs and workspace crates are per project, because macOS debug
maps and dep-info hold absolute paths.

### On Linux, the filesystem matters

The store costs almost no disk where files can share blocks (btrfs, XFS
with reflink, APFS). On ext4 every stored output is a real copy: builds
are as fast, but the store adds disk instead of sharing it. Measured on
ripgrep 14.1.1 (Linux, 16 threads, low priority on a busy machine):

| | btrfs | ext4 |
| --- | --- | --- |
| Stock `cargo build`, cold | 6.47 s | 6.47 s |
| `cargo oil build`, cold | 6.68 s | 6.62 s |
| No-change build | 0.05 s | 0.05 s |
| After `rm -rf target` | 0.28 s | 0.37 s |
| Disk: stock target dir | 332 MiB | 330 MiB |
| Disk: target dir + store | 336 MiB | 540 MiB |

Hardlinks would avoid the copies on ext4 but are not used: projects would
share one inode and its mtime, so restoring in one project would make
stock Cargo rebuild in another, and an in-place write would change the
store. Keep the store and your target dirs on the same filesystem; `gc`
bounds the store's size.

## Verifying it

`ci/oracle.py` checks Oak Oil's contract on a small workspace
(`ci/fixture`) on every push: a cold build, a no-change build, a restore
from the store and an edit are byte-identical to stock Cargo, stock Cargo
recompiles nothing afterwards, `clean` keeps everything Cargo uses, and
builds stay byte-identical after `gc` empties the store.

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

`cargo oil gc` keeps the store bounded. Limits come from
`OAKOIL_GC_MAX_AGE_DAYS` and `OAKOIL_GC_MAX_SIZE` (e.g. `20G`);
`OAKOIL_GC=off` stops the daily automatic run. Removing an object never
breaks a build: a unit that needed it compiles again, and missing Cargo
state is recorded again through Cargo. Where target dirs share blocks with
the store (APFS, btrfs, XFS), the disk can gain less than `gc` reports.

## Limitations

- macOS and Linux only. On Linux, `split-debuginfo` other than Cargo's
  default (`off`) falls back to a slower scan for a unit's object files.
- Linux is new: CI checks it on ext4, and btrfs and XFS were checked by
  hand, but no real workspace has had the full byte-identity check there
  yet.
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

Prebuilt macOS binaries (Apple Silicon and Intel) are on the
[releases page](https://github.com/oakoliver/oak-oil/releases); `cargo binstall`
fetches them from there. Linux support is on `main` and not released yet:
`cargo install --git https://github.com/oakoliver/oak-oil cargo-oil`.

## Where releases come from

Every release is built and published by
[`.github/workflows/release.yml`](.github/workflows/release.yml) from a
`v*` tag, after the same checks as CI (lint, tests, and the oracle below):

- **Binaries** on the GitHub release carry a signed build provenance
  attestation linking them to the workflow run and commit:
  `gh attestation verify cargo-oil-<version>-<target>.tar.gz --repo oakoliver/oak-oil`.
  `SHA256SUMS` lists their checksums.
- **The crate** is published to crates.io by the same workflow through
  trusted publishing (OIDC); no long-lived token exists. (0.1.0, the first
  release, had to use a one-time token, since revoked.) The published crate
  records the git commit it was built from.
- Workflow actions are pinned to full commit SHAs.

## License

MIT or Apache-2.0, at your option.
