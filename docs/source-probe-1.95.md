# Source probe: Cargo 0.96.0 / rustc 1.95.0

Clones: cargo `f2d3ce0b` (2026-03-21), rust `59807616` (1.95.0), in `upstream/`.
`C` = `upstream/cargo`, `R` = `upstream/rust/compiler`. Items marked ✔ were
spot-checked by hand on 2026-10-05.

## 1. Unit metadata hash (store key candidate)
- `C/src/cargo/core/compiler/build_runner/compilation_files.rs:690` `compute_metadata`.
  Inputs: METADATA_VERSION, `package_id().stable_hash(ws_root)`, features,
  `Profile::comparable` (opt-level, lto, codegen-backend, CGUs, debuginfo,
  split-debuginfo, debug-assertions, overflow-checks, rpath, incremental, panic,
  strip, profile rustflags, trim_paths; not the profile name), mode, lto, compile
  kind, target name/kind, `hash_rustc_version` (:854), RUSTC_WORKSPACE_WRAPPER
  path (members only), `__CARGO_DEFAULT_LIB_METADATA`, is_std, host/target
  config bool.
- `c_metadata` adds sorted deps' c_metadata; `unit_id` (file names) adds deps'
  unit_ids + `cargo rustc` args + RUSTFLAGS (skipped if they contain
  `--remap-path-prefix`).
- ✔ `SourceId::stable_hash` (`C/src/cargo/core/source_id.rs:564`) strips the
  workspace root only for path sources; registry = kind + URL, git = kind +
  canonical URL. **No target dir or workspace path enters a registry/git
  dependency's hash.**

## 2. Freshness
- `prepare_target` `C/src/cargo/core/compiler/fingerprint/mod.rs:452`; mtime-based
  dep-info check (`-Zchecksum-freshness` unstable). Dep-info paths stored
  package-root- or build-root-relative (`dep_info.rs:291`).
- Absolute paths remain for non-path sources, rustc/wrapper paths (rustc-info
  cache also mixes size + mtime), and OUT_DIR / CARGO_MANIFEST_DIR env.

## 3. `--unit-graph`
- `C/src/cargo/core/compiler/unit_graph.rs:83`, schema version 1, no metadata
  hash in output. ✔ Unstable: `command_prelude.rs:823`.

## 4. GC / tracker / build-dir
- Global cache tracker (`$CARGO_HOME/.global-cache`, SQLite) covers downloads
  only; no target/build-dir tracking (`gc.rs:307` "in the future … target
  cleaning"). `cargo clean gc` unstable.
- ✔ `build.build-dir` stable since 1.91 (`features.rs:1374`); templates
  `{workspace-root}`, `{cargo-cache-home}`, `{workspace-path-hash}`.
  `-Zbuild-dir-new-layout` → per-unit dirs; `-Zfine-grain-locking` implies it.

## 5. Shared cache
- None in this version. Related: build-dir split, unstable `--artifact-dir`,
  `-Zbuild-analysis`, `-Zfine-grain-locking`, `-Zno-embed-metadata`.

## 6. Uplifting
- `link_targets` `C/src/cargo/core/compiler/mod.rs:634` → `_link_or_copy`
  (`C/crates/cargo-util/src/paths.rs:610`): dirs symlinked; files on macOS
  `fs::copy` (clonefile), hard link elsewhere.

## 7. Wrappers
- argv = `[RUSTC_WRAPPER, RUSTC_WORKSPACE_WRAPPER?, rustc, …args]`
  (`util/rustc.rs:121-133`); the plain wrapper applies to all units and to the
  `-vV` probe. Wrapper binaries are hashed into the rustc-info fingerprint.

## 8. Metadata embedding
- `encode_metadata` `R/rustc_metadata/src/rmeta/encoder.rs:2416`; written by
  `R/rustc_metadata/src/fs.rs:36`; embedded in rlib by `link_rlib`
  (`R/rustc_codegen_ssa/src/back/link.rs:298-312`).
- ✔ `-Zembed-metadata` bool, default true, TRACKED (`R/rustc_session/src/options.rs:2320`).

## 9. Compression
- ✔ Not compressed. `create_compressed_metadata_file`
  (`R/rustc_codegen_ssa/src/back/metadata.rs:575`) only adds header + length.

## 10. Incremental
- `R/rustc_incremental/src/persist/fs.rs`: GC keeps the newest finalized session
  per crate dir; no size or count cap. Cargo disables incremental for non-local
  packages (`C/src/cargo/core/profiles.rs:309-316`).

## 11. `-Zshare-generics`
- Default on at opt-level 0/1/s/z, off at 2/3 (`R/rustc_session/src/config.rs:1475`).

## 12. Debug info / threads
- rustc default for Apple is Packed; ✔ Cargo forces `unpacked` on `-apple-`
  when debuginfo is on (`C/src/cargo/core/profiles.rs:289-301`). Unpacked keeps
  `.o` in `deps/`, referenced by path from binaries (`link.rs:1431-1451`).
- `-Zthreads` default 1, unstable.

## 13. Absolute paths in artifacts
- Source map paths are absolute in rmeta (`encoder.rs:536-597`). Registry crates
  are workspace-independent (same CARGO_HOME). `include!(concat!(env!("OUT_DIR"), …))`
  embeds the build-dir path. `--remap-path-scope` is stable; Cargo `trim-paths`
  unstable.
