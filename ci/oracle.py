#!/usr/bin/env python3
"""Oak Oil's contract, checked on a workspace (default: ci/fixture).

Every step compares Oak Oil with stock Cargo at the same target path, by
sha256 of every output file. Non-incremental builds are used, where stock
Cargo is itself byte-reproducible. Exits non-zero on the first violation.

  oracle.py CARGO_OIL_BINARY [WORKSPACE]

1. A cold `cargo oil build` is byte-identical to `cargo build`.
2. A second `cargo oil build` rebuilds nothing.
3. Outputs wiped, restored from the store, byte-identical again.
4. Stock `cargo build` afterwards recompiles nothing.
5. An edit, built by Oak Oil, is byte-identical to a stock build of it.
6. `cargo oil clean --apply` keeps everything Cargo uses.
7. After `cargo oil gc` empties the store, builds stay byte-identical.
8. Store warm, target dir rebuilt by stock Cargo, then `cargo oil build`:
   byte-identical, and stock Cargo recompiles nothing afterwards.
9. `cargo oil test` builds what `cargo test` needs: stock `cargo test
   --no-run` recompiles nothing afterwards, also after `rm -rf target`.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# Cargo's own caches and Oak Oil's state: not build outputs.
SKIP = (".rustc_info.json", "oakoil/", ".fingerprint/", "/incremental/", ".cargo-lock")


def run(cmd, cwd, env, check=True):
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    if check and p.returncode != 0:
        fail(f"`{' '.join(map(str, cmd))}` exited {p.returncode}\n{p.stderr[-3000:]}")
    return p


def fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)


def manifest(target):
    out = {}
    for f in target.rglob("*"):
        rel = str(f.relative_to(target))
        if f.is_file() and not f.is_symlink() and not any(s in rel for s in SKIP):
            out[rel] = hashlib.sha256(f.read_bytes()).hexdigest()
    return out


def norm_d(text):
    # Cargo writes one profile-dir dep-info for both outputs of a cdylib+rlib
    # crate; the last write wins, so its first token names either. Racy in
    # stock Cargo too.
    head, sep, rest = text.partition(":")
    for ext in (".dylib", ".rlib"):
        if head.endswith(ext):
            head = head[: -len(ext)] + ".<lib>"
    return head + sep + rest


def same(stock, oil, stock_dir, oil_dir, what):
    diff = []
    for rel in sorted(stock.keys() | oil.keys()):
        if stock.get(rel) == oil.get(rel):
            continue
        if rel.endswith(".d") and rel in stock and rel in oil:
            a = norm_d((stock_dir / rel).read_text(errors="replace"))
            b = norm_d((oil_dir / rel).read_text(errors="replace"))
            if a == b:
                continue
        diff.append(rel)
    if diff:
        fail(f"{what}: {len(diff)} files differ from stock Cargo: {diff[:10]}")
    print(f"ok: {what} ({len(stock)} files byte-identical)")


def compiled(stderr):
    return sum(1 for l in stderr.splitlines() if l.strip().startswith("Compiling "))


def main(oil, src):
    oil = str(Path(oil).resolve())
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        ws = tmp / "ws"
        shutil.copytree(src, ws)  # copy2 keeps nanosecond mtimes
        target, snap = tmp / "target", tmp / "stock-snapshot"
        env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL="0", OAKOIL_HOME=str(tmp / "store"))
        stock = lambda: run(["cargo", "build", "--locked"], ws, env)
        oil_build = lambda: (run([oil, "build", "--locked"], ws, env), run([oil, "wait-store"], ws, env))[0]

        stock()
        ref = manifest(target)
        shutil.copytree(target, snap)
        shutil.rmtree(target)

        oil_build()
        same(ref, manifest(target), snap, target, "1. cold build")

        out = oil_build().stderr
        if "0 from store, 0 compiled" not in out:
            fail(f"2. no-change build did work: {out.strip().splitlines()[-1:]}")
        print("ok: 2. no-change build rebuilt nothing")

        for d in target.iterdir():
            if d.is_dir() and d.name != "oakoil":
                shutil.rmtree(d)
        out = oil_build().stderr
        if " compiled in" not in out or ", 0 compiled" not in out:
            fail(f"3. restore compiled units: {out.strip().splitlines()[-1:]}")
        same(ref, manifest(target), snap, target, "3. restore from store")

        n = compiled(stock().stderr)
        if n:
            fail(f"4. stock Cargo recompiled {n} units after Oak Oil")
        print("ok: 4. stock Cargo recompiles nothing afterwards")

        lib = ws / "core" / "src" / "lib.rs"
        lib.write_text(lib.read_text() + "\npub fn edited() -> u32 {\n    1\n}\n")
        oil_build()
        got = manifest(target)
        edited_target = tmp / "oil-edited"
        shutil.copytree(target, edited_target)
        shutil.rmtree(target)
        stock()
        same(manifest(target), got, target, edited_target, "5. edit")

        out = run([oil, "clean", "--apply", "--cmd", "build --locked", "--cmd", "test --no-run --locked"], ws, env)
        if "verified: cargo recompiles nothing" not in out.stdout:
            fail(f"6. clean: {out.stdout.strip()[-500:]} {out.stderr.strip()[-500:]}")
        print("ok: 6. clean kept everything Cargo uses")

        # GC may remove objects a plan still names: builds must stay correct.
        run([oil, "gc", "--max-size", "0"], ws, env)
        left = [p for p in (tmp / "store" / "objects").rglob("*") if p.is_file()]
        if left:
            fail(f"7. gc --max-size 0 left {len(left)} files in the store")
        oil_build()
        for d in target.iterdir():
            if d.is_dir() and d.name != "oakoil":
                shutil.rmtree(d)
        oil_build()
        got = manifest(target)
        gc_target = tmp / "oil-after-gc"
        shutil.copytree(target, gc_target)
        shutil.rmtree(target)
        stock()
        same(manifest(target), got, target, gc_target, "7. build after gc emptied the store")

        # Stock Cargo owns the target dir, the store already has every unit:
        # Oak Oil re-records over it and must leave Cargo's freshness intact.
        shutil.rmtree(target)
        stock()
        ref = manifest(target)
        mixed = tmp / "stock-snapshot-8"
        shutil.copytree(target, mixed)
        oil_build()
        same(ref, manifest(target), mixed, target, "8. oil over a stock-built target (warm store)")
        n = compiled(stock().stderr)
        if n:
            fail(f"8. stock Cargo recompiled {n} units after oil over a stock-built target")
        print("ok: 8. stock Cargo recompiles nothing afterwards")

        # Same with incremental workspace crates (the default dev build): their
        # bytes differ between builds, so a re-record restores some units from
        # the store and finds others identical in place. All must end up newer
        # than their inputs, or stock Cargo rebuilds.
        env_inc = {k: v for k, v in env.items() if k != "CARGO_INCREMENTAL"}
        env_inc["CARGO_TARGET_DIR"] = str(tmp / "target-incremental")
        run([oil, "build", "--locked"], ws, env_inc)
        run([oil, "wait-store"], ws, env_inc)
        shutil.rmtree(tmp / "target-incremental")
        run(["cargo", "build", "--locked"], ws, env_inc)
        run([oil, "build", "--locked"], ws, env_inc)
        n = compiled(run(["cargo", "build", "--locked"], ws, env_inc).stderr)
        if n:
            fail(f"8. incremental: stock Cargo recompiled {n} units after oil over a stock-built target")
        print("ok: 8. incremental: stock Cargo recompiles nothing afterwards")

        test = lambda: (run([oil, "test", "--locked"], ws, env), run([oil, "wait-store"], ws, env))[0]
        stock_test = lambda: run(["cargo", "test", "--locked", "--no-run"], ws, env)
        test()
        n = compiled(stock_test().stderr)
        if n:
            fail(f"9. stock cargo test recompiled {n} units after cargo oil test")
        shutil.rmtree(target)
        test()
        n = compiled(stock_test().stderr)
        if n:
            fail(f"9. stock cargo test recompiled {n} units after cargo oil test from the store")
        print("ok: 9. cargo oil test leaves nothing for cargo test to compile")
    print("oracle: all checks passed")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else Path(__file__).parent / "fixture")
