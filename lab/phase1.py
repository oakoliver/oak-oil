#!/usr/bin/env python3
"""Phase 1 oracle: Oak Oil (Bun architecture) vs stock Cargo, sha256 per file.

Every Oak Oil result is compared with a stock Cargo build of the same source at
the same target path (absolute paths are part of the artifacts). Builds are
non-incremental, where stock Cargo itself is byte-reproducible.

  phase1.py P [EDIT_FILE] [P2]
      P          project in lab/src (copy it there with `probe.py setup P`)
      EDIT_FILE  a library source file (relative to P) for the edit checks
      P2         a second project, for cross-project store reuse
"""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.argv, ARGS = sys.argv[:1], sys.argv[1:]
import probe  # noqa: E402

LAB = probe.LAB
OIL = LAB.parent / "target" / "release" / "cargo-oil"
STORE = LAB / "store"
ENV = {"CARGO_INCREMENTAL": "0", "OAKOIL_HOME": str(STORE)}
SKIP = ("oakoil/", "cargo-timings/", ".fingerprint/", ".cargo-lock")


def sh(cmd, cwd, target, extra=None):
    env = dict(os.environ, **ENV, CARGO_TARGET_DIR=str(target), **(extra or {}))
    t0 = time.monotonic()
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    dt = time.monotonic() - t0
    if p.returncode != 0:
        sys.exit(f"FAILED {' '.join(map(str, cmd))}\n{p.stderr[-4000:]}")
    return dt, p.stderr


def oil(p, target):
    r = sh([str(OIL), "build"], LAB / "src" / p, target)
    sh([str(OIL), "wait-store"], LAB / "src" / p, target)  # background store writes done
    return r


def stock(p, target):
    return sh(["cargo", "build"], LAB / "src" / p, target)


def man(target):
    m = probe.manifest(target)
    return {k: v for k, v in m.items() if not any(s in k for s in SKIP)}


RACY_D = {}  # stock's copies of Cargo-written profile-dir .d files, by relpath


def keep_racy_d(target):
    """Cargo writes one dep-info for both outputs of a cdylib+rlib crate and
    the last write wins: its first token names the .dylib or the .rlib at
    random, in stock Cargo too. Keep stock's copies to compare normalized."""
    RACY_D.clear()
    for f in target.glob("**/*.d"):
        rel = str(f.relative_to(target))
        if "/deps/" not in rel and "/build/" not in rel and "/incremental/" not in rel:
            RACY_D[rel] = f.read_text(errors="replace")


def _norm_d(text):
    head, sep, rest = text.partition(":")
    for ext in (".dylib", ".rlib"):
        if head.endswith(ext):
            head = head[: -len(ext)] + ".<lib>"
    return head + sep + rest


def diff(a, b, target=None):
    """Files that differ or exist on one side only, ignoring incremental/.
    Profile-dir .d files differing only in the racy first token are equal."""
    keys = {k for k in a.keys() | b.keys() if "/incremental/" not in k}
    out = []
    for k in sorted(keys):
        if (a.get(k) or [0, 0, None])[2] == (b.get(k) or [0, 0, None])[2]:
            continue
        if target is not None and k in RACY_D and (target / k).is_file():
            if _norm_d(RACY_D[k]) == _norm_d((target / k).read_text(errors="replace")):
                continue
        out.append(k)
    return out


def summary_line(stderr):
    return next((l for l in stderr.splitlines()[::-1] if l.startswith("oil:")), "")


def stock_compiled(stderr):
    return sum(1 for l in stderr.splitlines() if l.strip().startswith("Compiling "))


def run(p, edit_file=None, p2=None):
    src = LAB / "src" / p
    t = LAB / "t" / p / "P"
    res = {}
    shutil.rmtree(STORE, ignore_errors=True)

    # Oracle: stock clean build at the same path.
    shutil.rmtree(t, ignore_errors=True)
    res["stock_clean_s"] = round(stock(p, t)[0], 2)
    oracle = man(t)
    keep_racy_d(t)
    shutil.rmtree(t)

    # T1 cold: record through Cargo with an empty store.
    dt, err = oil(p, t)
    m1 = man(t)
    res["T1_cold_record"] = {"s": round(dt, 2), "oil": summary_line(err), "files_differing_from_stock": diff(oracle, m1, t)}

    # T2 nothing changed: executor only.
    dt, err = oil(p, t)
    res["T2_noop_executor"] = {"s": round(dt, 3), "oil": summary_line(err)}

    # T3 outputs wiped, plan kept: every unit from the store.
    for d in t.iterdir():
        if d.is_dir() and d.name not in ("oakoil",):
            shutil.rmtree(d)
    dt, err = oil(p, t)
    m3 = man(t)
    res["T3_wiped_outputs_from_store"] = {"s": round(dt, 3), "oil": summary_line(err), "files_differing_from_stock": diff(oracle, m3, t)}

    # T4 retro-compat: stock Cargo right after Oak Oil.
    dt, err = stock(p, t)
    res["T4_stock_cargo_after_oil"] = {"s": round(dt, 2), "units_recompiled": stock_compiled(err)}

    # T5/T6 edits in the my-dsp style: comment at end, then a private body change.
    edits = edits_for(edit_file)
    for name, path, fn in edits:
        f = src / path
        orig = f.read_text()
        try:
            f.write_text(fn(orig))
            dt, err = oil(p, t)
            mo = man(t)
            # oracle for the edited source, same path
            shutil.copytree(t / "oakoil", LAB / "t" / p / "oakoil.keep", dirs_exist_ok=True)
            shutil.rmtree(t)
            stock(p, t)
            ms = man(t)
            shutil.rmtree(t)
            # restore Oak Oil's state for the next step: rebuild through the store
            res[name] = {"s": round(dt, 3), "oil": summary_line(err), "files_differing_from_stock": diff(ms, mo)}
        finally:
            f.write_text(orig)
            shutil.rmtree(t, ignore_errors=True)
            shutil.rmtree(LAB / "t" / p / "oakoil.keep", ignore_errors=True)
            oil(p, t)  # back to the original source, from the store

    if p2:
        t2 = LAB / "t" / p2 / "P"
        shutil.rmtree(t2, ignore_errors=True)
        stock(p2, t2)
        oracle2 = man(t2)
        shutil.rmtree(t2)
        dt, err = oil(p2, t2)
        res[f"T7_second_project_{p2}"] = {
            "s": round(dt, 2),
            "oil": summary_line(err),
            "files_differing_from_stock": diff(oracle2, man(t2)),
        }
        dt, err = oil(p2, t2)
        res[f"T7b_second_project_noop"] = {"s": round(dt, 3), "oil": summary_line(err)}

    res["store_bytes"] = sum(f.stat().st_blocks * 512 for f in STORE.rglob("*") if f.is_file())
    probe.save(f"{p}-phase1.json", res)
    print(json.dumps(res, indent=1))


def edits_for(edit_file):
    """Two edits to one library file: a comment, then a new private item."""
    if not edit_file:
        return []
    return [
        ("T5_edit_comment", edit_file, lambda s: s + "\n// oil probe\n"),
        ("T6_edit_private_item", edit_file,
         lambda s: s + "\n#[allow(dead_code)]\nfn __oil_probe() -> u32 { 7 }\n"),
    ]


if __name__ == "__main__":
    run(*ARGS)
