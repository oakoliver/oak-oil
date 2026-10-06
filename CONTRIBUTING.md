# Contributing

Oak Oil's one rule: **output must be byte-identical to stock Cargo**, and
stock Cargo must find the target dir fresh after Oak Oil. A change that
speeds something up but breaks either is a bug.

Before sending a change:

```sh
cargo test
cargo clippy --release
cargo build --release
# copy a workspace into lab/src and run the oracle on it
OIL_PROJECTS=~/code python3 lab/probe.py setup my-workspace
python3 lab/phase1.py my-workspace path/to/a/lib.rs
```

`phase1.py` must report no differing files (`files_differing_from_stock`)
and `units_recompiled: 0`. For performance changes, include `lab/bench.py`
results with every repetition, measured on a quiet machine.

Bug reports are most useful with the workspace shape (crate count, build
scripts, `--target`, crate types) and the oracle's output.
