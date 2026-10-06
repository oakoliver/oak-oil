# Contributing

Oak Oil's one rule: **output must be byte-identical to stock Cargo**, and
stock Cargo must find the target dir fresh after Oak Oil. A change that
speeds something up but breaks either is a bug.

Before sending a change:

```sh
cargo fmt --check
cargo clippy --locked --release -- -D warnings
cargo test --locked
cargo build --locked --release
python3 ci/oracle.py target/release/cargo-oil
```

CI runs exactly these. For changes to scheduling, freshness or the store,
also run the oracle on a real workspace:

```sh
OIL_PROJECTS=~/code python3 lab/probe.py setup my-workspace
python3 lab/phase1.py my-workspace path/to/a/lib.rs
```

It must report no differing files and `units_recompiled: 0`. For
performance changes, include `lab/bench.py` results with every repetition,
measured on a quiet machine.

Bug reports are most useful with the workspace shape (crate count, build
scripts, `--target`, crate types) and the oracle's output.
