# Findings

What building Oak Oil showed about Rust build output, measured on one Apple
Silicon Mac with Rust 1.95. Projects are named by size only:

| Project | Workspace crates | Packages | Build scripts |
| --- | --- | --- | --- |
| A | 5 | 104 | yes |
| B, B′ (two sibling checkouts) | 22 | 139 | yes |
| C | 138 | 169 | 9, cross-compiled with `--target` |

## Disk

A scan of every Cargo target dir on the machine found 469 GiB in 142 dirs:

- 177 GiB were byte-identical copies of files stored elsewhere.
- 185 GiB were incremental caches.
- 109 GiB were debug-info objects (`deps/*.o`, kept for macOS debug maps).
- 80% of the bytes were written in the previous 7 days: cleanup by age
  alone frees little.
- Agent worktrees of one project held a separate ~30 GiB target dir each.

`cargo oil clean --apply` on a copy of a real 46 GiB target dir removed
2.5 GiB of superseded unit variants and 7.8 GiB of old incremental sessions;
Cargo then recompiled nothing. Another 34.7 GiB sat in other directories
inside `target/` (target dirs nested by tools, an IDE check cache), which
`clean` reports but keeps unless asked.

## Correctness

Every check compares Oak Oil with stock Cargo at the same path, by sha256 of
every file:

| Check | A | B / B′ | C |
| --- | --- | --- | --- |
| Cold build, byte-identical | yes | yes | yes |
| Target wiped, restored from the store, byte-identical | yes | yes | yes |
| Stock `cargo build` afterwards | 0 recompiled | 0 recompiled | 0 recompiled |
| Edits, byte-identical to a stock rebuild | yes | — | — |
| B′ built from B's store, byte-identical | — | yes | — |
| Incremental restore: objects the debug maps reference | — | — | 3,562 of 3,562 present |

Stock nondeterminism the checks account for: Cargo's `.rustc_info.json`
cache, a clang module-cache index written by one build script, and Cargo's
shared dep-info for `cdylib`+`rlib` crates (insight 7).

## Speed

Median of 3, tools alternating within each repetition.

| Scenario | A stock | A Oak Oil | | C stock | C Oak Oil | |
| --- | --- | --- | --- | --- | --- | --- |
| Clean build | 9.46 s | 10.25 s | 0.92× | 10.71 s | 12.45 s | 0.86× |
| No-change build | 0.06 s | 0.07 s | 0.8× | 0.15 s | 0.14 s | 1.1× |
| Comment edit | 0.72 s | 0.68 s | 1.05× | 2.56 s | 1.97 s | 1.3× |
| Private code edit | 0.78 s | 0.76 s | 1.02× | 3.02 s | 2.79 s | 1.08× |
| Revert the edit | 0.78 s | 0.08 s | 9.3× | 3.23 s | 0.58 s | 5.5× |
| `rm -rf target`, build | 9.26 s | 0.57 s | 16× | 10.90 s | 1.66 s | 6.6× |
| Second checkout | 9.72 s | 10.22 s | 0.95× | 10.96 s | 9.52 s | 1.15× |

Incremental compilation on. With it off, C's revert is 40× and its
`rm -rf target` 8.3× faster; edits are at parity or better. Background store
writes after C's builds took 35 s of efficiency-core CPU across 42 builds
(at most 4.2 s after one build).

## Insights

1. **Speed comes from reuse, not from compiling faster.** Reverts, wiped
   targets and repeated checkouts are 5–300× faster; compiling new code
   still costs what rustc costs. "Nothing new to compile" is sub-second.
2. **Stock Cargo is byte-reproducible without incremental compilation, and
   not with it.** That makes a strict sha256 oracle possible for every unit.
3. **Absolute paths limit sharing.** Dep-info, macOS debug maps (`N_OSO`)
   and build-script `OUT_DIR` paths end up inside artifacts. Only registry
   and git library artifacts are path-independent; linked outputs and
   workspace crates stay per project.
4. **Early cutoff needs rustc's help.** Without incremental compilation even
   a comment changes `.rmeta`, which stores source hashes and line tables,
   and the crate hash covers every function body.
5. **Cargo freshness is mtime ordering.** Restoring artifacts means stamping
   them in dependency order. With `--target`, build scripts are compiled in
   the host profile dir and run in the target's, so they pair by package.
6. **Cargo drops `-C extra-filename` for `cdylib`/`dylib` crates**, so unit
   identity cannot rely on it.
7. **Cargo races on one file.** For a `cdylib`+`rlib` crate it writes one
   profile-dir dep-info per output and the last write wins; its first token
   names the `.dylib` or the `.rlib` at random, in plain stock builds too.
8. **Incremental builds leave stale objects.** One `deps/` dir held 19,886
   files, 19,405 of them objects; for one crate, 616 of 709 were stale.
   Outputs must come from the artifacts (rlib members, debug maps), not from
   file names.
9. **An executor's cost is everything except rustc.** The edit loop went
   from 0.63× to 1.1–1.3× stock through a jobserver, critical-path order,
   stat-only freshness, outputs from rustc's notices, and store writes moved
   out of the build.
10. **Measure on a quiet machine, or interleave.** Several early results
    were load noise; alternating tools within each repetition and keeping
    every repetition kept the conclusions honest.
11. **Bun's build architecture fits Rust.** "Cargo plans, ninja executes"
    needed no change to Cargo or rustc.

## Future work

| Item | Why | Effort |
| --- | --- | --- |
| Path-independent outputs (`--remap-path-prefix`) and cached build-script runs | Share workspace crates and linked outputs across checkouts and worktrees. | Medium |
| Store garbage collection | The store grows without bound. | Low |
| Plan from Cargo's JSON messages | Drop the dependency on Cargo's debug log; trim the ~10% clean-build cost. | Low–medium |
| rustc interface hash, relink-only | Skip dependents after private edits; turn binary rebuilds into a link. | High, upstream |
| `clean` without building | Find live units without bringing stale profiles up to date. | Medium |
| Linux support | Debug maps and `split-debuginfo=unpacked` are macOS specifics. | Medium |
| Store integrity | Verify hashes on restore; recover from an interrupted store write. | Low |
| Upstream reports | Cargo's dep-info race; target-dir GC and shared-cache work. | Low |
