#!/usr/bin/env python3
"""Oak Oil probe: stock Cargo as the oracle, Bun-style pipeline as the model.

Subcommands (results land in lab/results/):
  setup   P...   copy project sources (no target/, no .git) into lab/src/P
  oracle  P      3 clean stock builds: A, A again at the same path (determinism),
                 B at another path (path sensitivity); --timings; sha256 manifests;
                 no-op rebuild time
  model   P      replay --timings with a list scheduler: stock (calibration) vs
                 Oak Oil warm store (registry units become store hits)
  dedup   P...   bytes across the projects' A builds vs unique by sha256
  cleanup P      clone the real messy target (APFS clonefile), find the live units
                 via Cargo's fingerprint log, remove the rest, prove the build is
                 still fresh and the binaries are sha-identical
"""

import concurrent.futures as cf
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

LAB = Path(__file__).resolve().parent
# Where the projects under test live: setup P copies $OIL_PROJECTS/P.
PROJECTS = Path(os.environ.get("OIL_PROJECTS", Path.home() / "projects")).expanduser()
RESULTS = LAB / "results"
JOBS = os.cpu_count() or 8


def run(cmd, cwd, env_extra=None, log=None):
    env = dict(os.environ, **(env_extra or {}))
    t0 = time.monotonic()
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    dt = time.monotonic() - t0
    if log:
        Path(log).write_text(p.stdout + "\n--- stderr ---\n" + p.stderr)
    if p.returncode != 0:
        sys.exit(f"failed ({p.returncode}): {' '.join(cmd)}\n{p.stderr[-3000:]}")
    return dt, p


def save(name, data):
    RESULTS.mkdir(parents=True, exist_ok=True)
    (RESULTS / name).write_text(json.dumps(data, indent=1, sort_keys=True))


def load(name):
    return json.loads((RESULTS / name).read_text())


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def manifest(root):
    """relpath -> [len, allocated bytes (first hard link only), sha256]"""
    files, seen = [], set()
    for dirpath, _, names in os.walk(root):
        for n in names:
            p = Path(dirpath, n)
            if p.is_symlink() or not p.is_file():
                continue
            st = p.stat()
            first = (st.st_dev, st.st_ino) not in seen
            seen.add((st.st_dev, st.st_ino))
            files.append((p, st.st_size, st.st_blocks * 512 if first else 0))
    out = {}
    with cf.ThreadPoolExecutor(JOBS) as ex:
        for (p, size, disk), digest in zip(files, ex.map(lambda f: sha256(f[0]), files)):
            out[str(p.relative_to(root))] = [size, disk, digest]
    return out


def disk(man):
    return sum(v[1] for v in man.values())


def kind(rel):
    parts = rel.split("/")
    if len(parts) < 2:
        return "other"
    sub = parts[1] if len(parts) > 2 else "uplifted"
    if sub == "deps":
        ext = rel.rsplit(".", 1)[-1] if "." in parts[-1] else "bin"
        return {"rlib": "rlib", "rmeta": "rmeta", "o": "o", "d": "dep-info"}.get(ext, "deps-bin")
    return {"incremental": "incremental", "build": "build", ".fingerprint": "fingerprint"}.get(sub, sub)


def compare(a, b):
    """Files present in both manifests: identical vs different, by kind."""
    same, diff, by_kind = 0, 0, {}
    for rel, va in a.items():
        vb = b.get(rel)
        if vb is None:
            continue
        k = kind(rel)
        s = by_kind.setdefault(k, [0, 0])
        if va[2] == vb[2]:
            same += 1
            s[0] += 1
        else:
            diff += 1
            s[1] += 1
    diff_paths = sorted(r for r in a.keys() & b.keys() if a[r][2] != b[r][2])
    return {
        "identical": same,
        "different": diff,
        "different_paths": diff_paths[:60],
        "only_in_first": len(a.keys() - b.keys()),
        "only_in_second": len(b.keys() - a.keys()),
        "by_kind_identical_different": by_kind,
    }


# ─── setup ──────────────────────────────────────────────────────────────────


def cmd_setup(projects):
    for p in projects:
        # Only what git sees (tracked + untracked, not ignored): sources without
        # data folders, build outputs or worktrees. mtimes are kept by rsync -a.
        dst = LAB / "src" / p
        dst.mkdir(parents=True, exist_ok=True)
        files = subprocess.run(["git", "ls-files", "-co", "--exclude-standard", "-z"],
                               cwd=PROJECTS / p, capture_output=True, check=True).stdout
        subprocess.run(["rsync", "-a", "--from0", "--files-from=-", f"{PROJECTS / p}/", f"{dst}/"],
                       input=files, check=True)
        print(f"{p}: {dst}")


# ─── oracle ─────────────────────────────────────────────────────────────────


def build(p, target, tag, extra=()):
    src = LAB / "src" / p
    logs = RESULTS / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    dt, _ = run(["cargo", "build", "--timings", *extra], src,
                {"CARGO_TARGET_DIR": str(target)}, logs / f"{p}-{tag}.log")
    timing = target / "cargo-timings" / "cargo-timing.html"
    if timing.exists():
        shutil.copy(timing, RESULTS / f"{p}-{tag}-timing.html")
    return dt


def cmd_oracle(p):
    t = LAB / "t" / p
    a, b = t / "A", t / "B"
    res = {}
    for d in (a, b):
        shutil.rmtree(d, ignore_errors=True)

    res["clean_build_s_A1"] = build(p, a, "A1")
    m1 = manifest(a)
    shutil.rmtree(a)
    res["clean_build_s_A2"] = build(p, a, "A2")
    m2 = manifest(a)
    res["noop_build_s"] = build(p, a, "noop")
    res["clean_build_s_B"] = build(p, b, "B")
    mb = manifest(b)

    res["disk_bytes_A"] = disk(m2)
    res["files_A"] = len(m2)
    res["same_path_rebuild"] = compare(m1, m2)
    res["other_path_build"] = compare(m2, mb)
    save(f"{p}-manifest-A.json", m2)
    save(f"{p}-oracle.json", res)
    shutil.rmtree(b)
    print(json.dumps(res, indent=1))


# ─── model ──────────────────────────────────────────────────────────────────


def unit_data(html):
    text = Path(html).read_text()
    m = re.search(r"const UNIT_DATA = (\[.*?\]);\s*const CONCURRENCY_DATA", text, re.S)
    return json.loads(m.group(1))


def lock_sources(p):
    """(name, version) -> 'registry' | 'git' | 'local' from Cargo.lock."""
    out, cur = {}, {}
    for line in (LAB / "src" / p / "Cargo.lock").read_text().splitlines() + ["[[package]]"]:
        if line.startswith("[[package]]"):
            if "name" in cur:
                src = cur.get("source", "")
                out[(cur["name"], cur["version"])] = (
                    "registry" if src.startswith("registry") else "git" if src.startswith("git") else "local")
            cur = {}
        elif " = " in line:
            k, v = line.split(" = ", 1)
            cur[k] = v.strip('"')
    return out


def simulate(units, duration, jobs):
    """List-schedule the unit graph. A unit waits for full completion of the
    units listing it in unblocked_units and for the rmeta of those listing it
    in unblocked_rmeta_units (Cargo's pipelining). Returns makespan."""
    n = len(units)
    idx = {u["i"]: k for k, u in enumerate(units)}
    deps_full = [[] for _ in range(n)]
    deps_meta = [[] for _ in range(n)]
    for k, u in enumerate(units):
        for v in u["unblocked_units"]:
            deps_full[idx[v]].append(k)
        for v in u["unblocked_rmeta_units"]:
            deps_meta[idx[v]].append(k)
    # rmeta is ready at the same fraction of the unit as Cargo observed
    meta_frac = []
    for u in units:
        rm = None
        for name, sec in (u.get("sections") or []):
            if name == "frontend":
                rm = sec["end"]
        meta_frac.append(rm / u["duration"] if rm and u["duration"] > 0 else 1.0)

    start = [None] * n
    end = [None] * n
    free = [0.0] * jobs
    remaining = set(range(n))
    while remaining:
        ready = []
        for k in remaining:
            if all(end[d] is not None for d in deps_full[k]) and all(end[d] is not None for d in deps_meta[k]):
                t_ready = max([end[d] for d in deps_full[k]] +
                              [start[d] + duration[d] * meta_frac[d] for d in deps_meta[k]] + [0.0])
                ready.append((t_ready, k))
        t_ready, k = min(ready)
        j = min(range(jobs), key=lambda i: free[i])
        start[k] = max(t_ready, free[j])
        end[k] = start[k] + duration[k]
        free[j] = end[k]
        remaining.remove(k)
    return max(end) if end else 0.0


def cmd_model(p, hit_ms=None):
    units = unit_data(RESULTS / f"{p}-A2-timing.html")
    src = lock_sources(p)
    oracle = load(f"{p}-oracle.json")
    if hit_ms is None:
        hit_ms = load("hit-cost.json")["per_unit_ms"]

    def cacheable(u):
        origin = src.get((u["name"], u["version"]), "local")
        # Phase 1 scope: registry/git crates, not build-script runs (OUT_DIR paths)
        return origin in ("registry", "git") and u["mode"] != "run-custom-build"

    stock_d = [u["duration"] for u in units]
    warm_d = [hit_ms / 1000 if cacheable(u) else u["duration"] for u in units]
    by = {}
    for u in units:
        key = ("cacheable" if cacheable(u) else "local") + ":" + u["mode"]
        c = by.setdefault(key, [0, 0.0])
        c[0] += 1
        c[1] += u["duration"]
    res = {
        "units": len(units),
        "unit_seconds_by_class": {k: [v[0], round(v[1], 1)] for k, v in sorted(by.items())},
        "measured_clean_s": round(oracle["clean_build_s_A2"], 1),
        "model_stock_s": round(simulate(units, stock_d, JOBS), 1),
        "model_oakoil_warm_s": round(simulate(units, warm_d, JOBS), 1),
        "measured_noop_s": round(oracle["noop_build_s"], 1),
        "hit_cost_ms": hit_ms,
    }
    save(f"{p}-model.json", res)
    print(json.dumps(res, indent=1))


def cmd_hitcost(p):
    """Time to materialize store outputs into a target dir: clonefile + utime."""
    a = LAB / "t" / p / "A" / "debug" / "deps"
    files = [f for f in a.iterdir() if f.suffix in (".rlib", ".rmeta")]
    dst = LAB / "t" / p / "hitcost"
    shutil.rmtree(dst, ignore_errors=True)
    dst.mkdir(parents=True)
    t0 = time.monotonic()
    for f in files:
        subprocess.run(["cp", "-c", str(f), str(dst / f.name)], check=True)
        os.utime(dst / f.name)
    clone_s = time.monotonic() - t0
    t0 = time.monotonic()
    for f in files:
        os.symlink(f, dst / (f.name + ".lnk"))
        os.utime(dst / (f.name + ".lnk"), follow_symlinks=False)
    link_s = time.monotonic() - t0
    shutil.rmtree(dst)
    units = len({f.stem for f in files})
    res = {
        "files": len(files),
        "clone_ms_per_file_incl_fork": round(clone_s * 1000 / len(files), 2),
        "symlink_ms_per_file": round(link_s * 1000 / len(files), 3),
        # a hit = rlib + rmeta + dep-info + rewritten stderr replay; fork of cp dominates the clone
        # figure, an in-process clonefile(2) is cheaper; use 2 files of clone cost as the estimate
        "per_unit_ms": round(2 * clone_s * 1000 / len(files), 2),
        "units_sampled": units,
    }
    save("hit-cost.json", res)
    print(json.dumps(res, indent=1))


# ─── dedup ──────────────────────────────────────────────────────────────────


def cmd_dedup(projects):
    total, uniq, seen = 0, 0, set()
    per = {}
    for p in projects:
        m = load(f"{p}-manifest-A.json")
        per[p] = disk(m)
        for rel, (size, d, digest) in m.items():
            total += d
            if digest not in seen:
                seen.add(digest)
                uniq += d
    res = {"per_project_bytes": per, "total_bytes": total, "unique_bytes": uniq,
           "saved_bytes": total - uniq, "saved_share": round((total - uniq) / total, 3) if total else 0}
    save("dedup.json", res)
    print(json.dumps(res, indent=1))


# ─── cleanup ────────────────────────────────────────────────────────────────

LIVE_CMDS = [["build"], ["build", "--release"], ["test", "--no-run"]]
UNIT_RE = re.compile(r"^(?:lib)?(.+)-([0-9a-f]{16})(?:\.|$)")


def fingerprint_run(p, target, tag):
    """Run each live command with Cargo's fingerprint log; return live unit
    dirs per profile and the number of units Cargo had to (re)compile."""
    src = LAB / "src" / p
    live, compiled, secs = set(), 0, {}
    for c in LIVE_CMDS:
        dt, proc = run(["cargo", *c, "-v"], src,
                       {"CARGO_TARGET_DIR": str(target),
                        "CARGO_LOG": "cargo::core::compiler::fingerprint=debug"},
                       RESULTS / "logs" / f"{p}-{tag}-{'-'.join(c)}.log")
        secs[" ".join(c)] = round(dt, 1)
        for line in proc.stderr.splitlines():
            m = re.search(r"fingerprint at: (\S+)", line)
            if m:
                fp = Path(m.group(1)).parent
                live.add(str(fp.relative_to(target)))
            elif re.match(r"\s*(Compiling|Dirty) ", line):
                compiled += line.lstrip().startswith("Compiling ")
    return live, compiled, secs


def cmd_cleanup(p):
    real = PROJECTS / p / "target"
    messy = LAB / "messy" / p
    shutil.rmtree(messy, ignore_errors=True)
    messy.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cp", "-cRp", str(real), str(messy)], check=True)  # APFS clone: no extra disk
    before = manifest(messy)

    live_fp, compiled1, secs1 = fingerprint_run(p, messy, "live")
    after_first = manifest(messy)
    # live units: "<profile-dir>/.fingerprint/<pkg>-<unitid>" -> (profile-dir, unitid)
    live = set()
    for fp in live_fp:
        parts = Path(fp).parts
        prof = "/".join(parts[: parts.index(".fingerprint")])
        live.add((prof, parts[-1].rsplit("-", 1)[-1]))
    live_profiles = {pr for pr, _ in live}

    removed, kept_unknown = 0, 0
    rm_paths = []
    for rel, (size, d, digest) in after_first.items():
        parts = rel.split("/")
        # profile dir = first component, or triple/profile
        if parts[0] in ("debug", "release"):
            prof, rest = parts[0], parts[1:]
        elif len(parts) > 2 and parts[1] in ("debug", "release"):
            prof, rest = "/".join(parts[:2]), parts[2:]
        else:
            prof, rest = parts[0], parts[1:]
        if prof not in live_profiles:
            # whole profile/tool dir unused by the live commands (flycheck0, tmp, other triples)
            rm_paths.append(rel)
            continue
        if not rest or rest[0] not in ("deps", "build", ".fingerprint", "incremental", "examples"):
            continue  # uplifted outputs and top-level files stay
        if rest[0] == "incremental":
            continue  # handled below by newest-session rule
        name = rest[1] if len(rest) > 1 else rest[0]
        m = UNIT_RE.match(name)
        if not m:
            kept_unknown += 1
            continue
        if (prof, m.group(2)) not in live:
            rm_paths.append(rel)

    # incremental: keep the newest dir per crate name
    for prof in live_profiles:
        inc = messy / prof / "incremental"
        if not inc.is_dir():
            continue
        newest = {}
        for d in inc.iterdir():
            crate = d.name.rsplit("-", 1)[0]
            if crate not in newest or d.stat().st_mtime > newest[crate].stat().st_mtime:
                newest[crate] = d
        keep = {str(d) for d in newest.values()}
        for d in inc.iterdir():
            if str(d) not in keep:
                for f in d.rglob("*"):
                    if f.is_file():
                        rm_paths.append(str(f.relative_to(messy)))

    removed = sum(after_first[r][1] for r in rm_paths if r in after_first)
    for r in rm_paths:
        try:
            (messy / r).unlink()
        except FileNotFoundError:
            pass
    # drop now-empty dirs
    for dirpath, dirnames, filenames in os.walk(messy, topdown=False):
        if not os.listdir(dirpath) and Path(dirpath) != messy:
            os.rmdir(dirpath)

    after_clean = manifest(messy)
    _, compiled2, secs2 = fingerprint_run(p, messy, "verify")
    after_verify = manifest(messy)
    uplifted = [r for r in after_clean if kind(r) == "uplifted" and not r.endswith(".d")]
    sha_same = all(after_clean[r][2] == after_verify.get(r, [0, 0, None])[2] for r in uplifted)

    res = {
        "real_target_bytes": disk(before),
        "after_live_commands_bytes": disk(after_first),
        "units_compiled_by_live_commands": compiled1,
        "live_command_seconds": secs1,
        "live_units": len(live),
        "removed_bytes": removed,
        "removed_files": len(rm_paths),
        "kept_unrecognized_files": kept_unknown,
        "after_cleanup_bytes": disk(after_clean),
        "verify_units_recompiled": compiled2,
        "verify_seconds": secs2,
        "uplifted_outputs_checked": len(uplifted),
        "uplifted_sha_identical_after_verify": sha_same,
    }
    save(f"{p}-cleanup.json", res)
    print(json.dumps(res, indent=1))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd, args = sys.argv[1], sys.argv[2:]
    {"setup": lambda: cmd_setup(args),
     "oracle": lambda: cmd_oracle(args[0]),
     "hitcost": lambda: cmd_hitcost(args[0]),
     "model": lambda: cmd_model(args[0]),
     "dedup": lambda: cmd_dedup(args),
     "cleanup": lambda: cmd_cleanup(args[0])}[cmd]()
