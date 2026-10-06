#!/usr/bin/env python3
"""Apples-to-apples: stock Cargo vs Oak Oil, same scenarios, same settings.

Each scenario is a real user action, timed end to end (wall clock of the
command the user types). Stock and Oak Oil alternate within each repetition;
the median of REPS is reported. Oak Oil starts every repetition with an empty
store, so nothing is carried over between repetitions.

  bench.py P CRATE_FILE [REPS]
     P          project in lab/src
     CRATE_FILE source file of a library crate with dependents, edited in the
                edit scenarios (relative to the project)
"""

import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

sys.argv, ARGS = sys.argv[:1], sys.argv[1:]
import probe  # noqa: E402

LAB = probe.LAB
OIL = LAB.parent / "target" / "release" / "cargo-oil"
STORE = LAB / "bench-store"


DRAIN = []  # seconds Oak Oil's background store writes ran on after a build


def timed(cmd, cwd, env):
    t0 = time.monotonic()
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    dt = time.monotonic() - t0
    if p.returncode != 0:
        sys.exit(f"FAILED {cmd} in {cwd}\n{p.stderr[-3000:]}")
    if cmd[0] == str(OIL):
        # not part of the build's wall time, but it must not overlap the next
        # measurement; recorded separately
        t1 = time.monotonic()
        subprocess.run([str(OIL), "wait-store"], cwd=cwd, env=env, capture_output=True)
        DRAIN.append(time.monotonic() - t1)
    return dt


def tool_cmd(tool):
    return [str(OIL), "build"] if tool == "oil" else ["cargo", "build"]


def env_for(tool, target, incremental):
    e = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL=incremental, OAKOIL_HOME=str(STORE))
    return e


def one_rep(p, crate_file, incremental, rep):
    src = LAB / "src" / p
    # second checkout of the same project at another path (the worktree case)
    src2 = LAB / "src" / f"{p}-copy"
    shutil.rmtree(src2, ignore_errors=True)
    shutil.copytree(src, src2, symlinks=True)
    f = src / crate_file
    orig = f.read_text()
    out = {}
    try:
        for tool in (("stock", "oil") if rep % 2 == 0 else ("oil", "stock")):
            t = LAB / "t" / p / f"bench-{tool}"
            t2 = LAB / "t" / p / f"bench-{tool}-copy"
            shutil.rmtree(t, ignore_errors=True)
            shutil.rmtree(t2, ignore_errors=True)
            if tool == "oil":
                shutil.rmtree(STORE, ignore_errors=True)
            env = env_for(tool, t, incremental)
            cmd = tool_cmd(tool)
            r = {}
            r["1 clean build"] = timed(cmd, src, env)
            r["2 no-change build"] = timed(cmd, src, env)
            f.write_text(orig + "\n// bench comment\n")
            r["3 comment edit"] = timed(cmd, src, env)
            f.write_text(orig + f"\n#[allow(dead_code)]\nfn __oil_bench_probe() -> u32 {{ {rep + 7} }}\n")
            r["4 private code edit"] = timed(cmd, src, env)
            f.write_text(orig)
            r["5 revert edit"] = timed(cmd, src, env)
            shutil.rmtree(t)
            r["6 rm -rf target, build"] = timed(cmd, src, env)
            r["7 second checkout, build"] = timed(cmd, src2, env_for(tool, t2, incremental))
            out[tool] = r
    finally:
        f.write_text(orig)
        shutil.rmtree(src2, ignore_errors=True)
    return out


def main(p, crate_file, reps="3"):
    reps = int(reps)
    report = {}
    for incremental in ("1", "0"):
        runs = [one_rep(p, crate_file, incremental, i) for i in range(reps)]
        rows = {}
        for scen in runs[0]["stock"]:
            s = statistics.median(r["stock"][scen] for r in runs)
            o = statistics.median(r["oil"][scen] for r in runs)
            rows[scen] = {"stock_s": round(s, 3), "oil_s": round(o, 3), "speedup": round(s / o, 2) if o else None,
                          "stock_all": [round(r["stock"][scen], 3) for r in runs],
                          "oil_all": [round(r["oil"][scen], 3) for r in runs]}
        report[f"CARGO_INCREMENTAL={incremental}"] = rows
    report["oil_background_store_s"] = {"total": round(sum(DRAIN), 2), "max": round(max(DRAIN, default=0), 2), "builds": len(DRAIN)}
    probe.save(f"{p}-bench.json", report)
    print(f"oak oil background store writes after builds: {report['oil_background_store_s']}")
    for mode, rows in report.items():
        if not mode.startswith("CARGO_"):
            continue
        print(f"\n{p}  {mode}  (median of {reps})")
        print(f"{'scenario':28} {'stock':>8} {'oak oil':>8} {'x':>6}")
        for scen, v in rows.items():
            print(f"{scen:28} {v['stock_s']:8.2f} {v['oil_s']:8.2f} {v['speedup']:6.2f}")


if __name__ == "__main__":
    main(*ARGS)
