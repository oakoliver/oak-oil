//! The plan: Cargo's unit graph as exact rustc commands, recorded through the
//! wrapper, plus what must stay unchanged for the plan to hold (manifests,
//! lockfile, configs, toolchain, build-script inputs, env).
//!
//! Like Bun's `rust-target/plan.json`: the plan is rebuilt only when one of
//! those inputs changes; otherwise the executor runs it without Cargo.

use crate::hashing::{HashCache, hash_bytes};
use crate::unit::{Record, load_record, records_dir};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PLAN_VERSION: u32 = 6;

#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub cwd: PathBuf,
    pub cargo_args: Vec<String>,
    pub target: PathBuf,
    /// Record keys of the live units.
    pub units: Vec<String>,
    /// (output in deps/, uplifted copy in the profile dir)
    pub uplifts: Vec<(PathBuf, PathBuf)>,
    pub watch_files: Vec<(PathBuf, String)>,
    pub watch_stamps: Vec<(PathBuf, u64, i64)>,
    pub watch_env: Vec<(String, Option<String>)>,
    /// Build-script run dirs (`build/<pkg>-<id>/`): restored before units run.
    pub build_runs: Vec<(PathBuf, String)>,
    /// Cargo fingerprint dirs: restored after all outputs, so a later stock
    /// `cargo build` sees everything fresh.
    pub fingerprints: Vec<(PathBuf, String)>,
    /// Cargo-written dep-info next to uplifted outputs (`debug/foo.d`),
    /// stored as one-file trees keyed by their dir.
    pub cargo_dep_infos: Vec<(PathBuf, String)>,
    /// `CACHEDIR.TAG` files Cargo wrote below the target dir (e.g. in
    /// `x86_64-apple-darwin/`).
    pub cachedir_tags: Vec<PathBuf>,
}

pub fn plan_path(store_root: &Path, cwd: &Path, args: &[String]) -> PathBuf {
    let id = hash_bytes(format!("{}\0{}", cwd.display(), args.join("\0")).as_bytes());
    store_root.join("plans").join(format!("{}.json", &id[..32]))
}

pub fn load(path: &Path) -> Option<Plan> {
    let p: Plan = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (p.version == PLAN_VERSION).then_some(p)
}

impl Plan {
    /// `None` when valid, else the first reason it is not.
    pub fn invalid_reason(&self, hc: &HashCache) -> Option<String> {
        for (k, v) in &self.watch_env {
            if std::env::var(k).ok() != *v {
                return Some(format!("env {k} changed"));
            }
        }
        for (p, h) in &self.watch_files {
            match hc.get(p) {
                Ok(now) if &now == h => {}
                Ok(_) => return Some(format!("{} changed", p.display())),
                Err(_) if h.is_empty() => {}
                Err(_) => return Some(format!("{} is gone", p.display())),
            }
        }
        for (p, len, mtime) in &self.watch_stamps {
            match fs::metadata(p) {
                Ok(m) if m.len() == *len && m.mtime() == *mtime => {}
                _ => return Some(format!("build-script input {} changed", p.display())),
            }
        }
        if !self.units.iter().all(|k| {
            records_dir(&self.target)
                .join(format!("{k}.json"))
                .is_file()
        }) {
            return Some("unit records missing".into());
        }
        None
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(tmp, path)
    }
}

/// Runs Cargo with Oak Oil as `RUSTC_WRAPPER` and returns the fingerprint
/// dirs of every unit Cargo considered (fresh or not).
fn run_cargo(cwd: &Path, args: &[String], target: &Path) -> io::Result<(bool, Vec<PathBuf>)> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new("cargo")
        .args(args)
        .current_dir(cwd)
        .env("RUSTC_WRAPPER", &exe)
        .env("OAKOIL_WRAPPER", "1")
        .env("OAKOIL_TARGET", target)
        .env("CARGO_LOG", "cargo::core::compiler::fingerprint=debug")
        .stderr(Stdio::piped())
        .spawn()?;
    let mut fps = Vec::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line?;
        if let Some(i) = line.find("fingerprint at: ") {
            fps.push(PathBuf::from(line[i + 16..].trim()));
        } else if !line.contains(" cargo::core::compiler::fingerprint") {
            eprintln!("{line}");
        }
    }
    Ok((child.wait()?.success(), fps))
}

/// fingerprint file `…/<profile>/.fingerprint/<pkg>-<id>/<kind>-<name>` →
/// `(fingerprint dir, unit id, alt key)` for compile units (not build-script
/// runs). `alt key` is the record key a unit without extra-filename would
/// have (`lib-ffi_shim` → `ffi_shim@<hash of <profile>/deps>`).
fn compile_unit_of(fp: &Path) -> Option<(PathBuf, String, String)> {
    let name = fp.file_name()?.to_string_lossy().to_string();
    if name.starts_with("run-build-script") {
        return None;
    }
    let dir = fp.parent()?;
    let id = dir
        .file_name()?
        .to_string_lossy()
        .rsplit_once('-')?
        .1
        .to_string();
    let profile = dir.parent()?.parent()?;
    let target_name = name.strip_prefix("lib-").unwrap_or(&name).replace('-', "_");
    let alt = crate::unit::unit_key(&target_name, "", &profile.join("deps"));
    Some((dir.to_path_buf(), id, alt))
}

pub struct Recorded {
    pub plan: Plan,
    pub forced_units: usize,
}

/// Builds through Cargo and records the plan. Units Cargo found fresh were
/// never shown to the wrapper; their fingerprints are removed once so Cargo
/// sends them through it (cheap when the store has them).
pub fn record(
    cwd: &Path,
    args: &[String],
    target: &Path,
    hc: &HashCache,
    store: &crate::store::Store,
) -> io::Result<Option<Recorded>> {
    let watch_env: Vec<(String, Option<String>)> = {
        let mut v: Vec<_> = std::env::vars()
            .filter(|(k, _)| {
                (k.starts_with("CARGO") || k.starts_with("RUST")) && !k.starts_with("OAKOIL")
            })
            .map(|(k, v)| (k, Some(v)))
            .collect();
        for k in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_INCREMENTAL",
            "CARGO_TARGET_DIR",
            "RUSTC",
        ] {
            if !v.iter().any(|(n, _)| n == k) {
                v.push((k.to_string(), None));
            }
        }
        v.sort();
        v
    };

    let mut forced = 0;
    let mut live_ids;
    let mut fp_dirs: Vec<PathBuf>;
    let mut attempt = 0;
    loop {
        let t_cargo = std::time::Instant::now();
        let (ok, fps) = run_cargo(cwd, args, target)?;
        if std::env::var_os("OAKOIL_TRACE").is_some() {
            eprintln!(
                "oil-trace: cargo run {:.3}s ({} fingerprint lines)",
                t_cargo.elapsed().as_secs_f64(),
                fps.len()
            );
        }
        if !ok {
            return Ok(None);
        }
        fp_dirs = fps
            .iter()
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();
        let units: Vec<(PathBuf, String, String)> =
            fps.iter().filter_map(|f| compile_unit_of(f)).collect();
        let by_id = records_by_id(target);
        let known = |id: &String, alt: &String| by_id.contains_key(id) || by_id.contains_key(alt);
        let missing: Vec<&PathBuf> = units
            .iter()
            .filter(|(_, id, alt)| !known(id, alt))
            .map(|(d, _, _)| d)
            .collect();
        live_ids = units
            .iter()
            .map(|(_, id, alt)| {
                if by_id.contains_key(id) {
                    id.clone()
                } else {
                    alt.clone()
                }
            })
            .collect::<HashSet<_>>();
        if missing.is_empty() || attempt == 1 {
            if !missing.is_empty() {
                eprintln!(
                    "oil: {} units could not be recorded; plan not saved",
                    missing.len()
                );
                return Ok(None);
            }
            break;
        }
        forced = missing.len();
        for d in missing {
            let _ = fs::remove_dir_all(d);
        }
        attempt += 1;
    }

    let trace = std::env::var_os("OAKOIL_TRACE").is_some();
    let t_plan = std::time::Instant::now();
    let by_id = records_by_id(target);
    let mut units: Vec<String> = live_ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|r| r.key.clone()))
        .collect();
    units.sort();
    let recs: Vec<Record> = units
        .iter()
        .filter_map(|k| load_record(target, k))
        .collect();

    let uplifts = find_uplifts(&recs, hc);
    if trace {
        eprintln!(
            "oil-trace: records+uplifts {:.3}s",
            t_plan.elapsed().as_secs_f64()
        );
    }
    let mut cargo_dep_infos = Vec::new();
    for (_, dst) in &uplifts {
        let d = dst.with_extension("d");
        if d.is_file() && d != *dst {
            let tmp = store
                .root()
                .join("tmp")
                .join(format!("dinfo.{}", std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp)?;
            fs::copy(&d, tmp.join(d.file_name().unwrap()))?;
            cargo_dep_infos.push((d.parent().unwrap().to_path_buf(), store.put_tree(&tmp, hc)?));
            let _ = fs::remove_dir_all(&tmp);
        }
    }
    cargo_dep_infos.sort();
    cargo_dep_infos.dedup();
    fp_dirs.sort();
    fp_dirs.dedup();
    let mut fingerprints = Vec::new();
    let mut build_runs = Vec::new();
    for d in &fp_dirs {
        if d.is_dir() {
            fingerprints.push((d.clone(), store.put_tree(d, hc)?));
        }
        // a run unit's fingerprint dir `<profile>/.fingerprint/<pkg>-<id>` pairs
        // with its run dir `<profile>/build/<pkg>-<id>` (out/, output, …)
        let is_run = fs::read_dir(d)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("run-build-script")
            });
        if is_run
            && let (Some(name), Some(profile)) = (d.file_name(), d.parent().and_then(Path::parent))
        {
            let run_dir = profile.join("build").join(name);
            if run_dir.is_dir() {
                build_runs.push((run_dir.clone(), store.put_tree(&run_dir, hc)?));
            }
        }
    }
    if trace {
        eprintln!(
            "oil-trace: snapshots {:.3}s",
            t_plan.elapsed().as_secs_f64()
        );
    }
    let (watch_files, watch_stamps) = watch_inputs(cwd, target, hc)?;
    if trace {
        eprintln!(
            "oil-trace: watch inputs {:.3}s",
            t_plan.elapsed().as_secs_f64()
        );
    }
    let plan = Plan {
        version: PLAN_VERSION,
        cwd: cwd.to_path_buf(),
        cargo_args: args.to_vec(),
        target: target.to_path_buf(),
        units,
        uplifts,
        watch_files,
        watch_stamps,
        watch_env,
        build_runs,
        fingerprints,
        cargo_dep_infos,
        cachedir_tags: fs::read_dir(target)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path().join("CACHEDIR.TAG"))
            .filter(|p| p.is_file())
            .collect(),
    };
    Ok(Some(Recorded {
        plan,
        forced_units: forced,
    }))
}

/// Unit id (extra-filename without the dash) → record; units without an
/// extra-filename are indexed by their record key.
fn records_by_id(target: &Path) -> HashMap<String, Record> {
    let mut m = HashMap::new();
    if let Ok(rd) = fs::read_dir(records_dir(target)) {
        for e in rd.filter_map(Result::ok) {
            if let Some(r) = fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
                && let Some(inv) = r.invocation()
            {
                let id = if inv.extra.is_empty() {
                    r.key.clone()
                } else {
                    inv.extra.trim_start_matches('-').to_string()
                };
                m.insert(id, r);
            }
        }
    }
    m
}

/// Cargo copies final outputs from `deps/` up into the profile dir; find
/// those copies by size and content.
fn find_uplifts(recs: &[Record], hc: &HashCache) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut candidates: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for r in recs {
        for dir in [r.out_dir.parent(), Some(r.out_dir.as_path())]
            .into_iter()
            .flatten()
        {
            if !seen_dirs.insert(dir.to_path_buf()) {
                continue;
            }
            let Ok(rd) = fs::read_dir(dir) else { continue };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if p.is_file()
                    && p.extension().is_none_or(|x| x != "d")
                    && let Ok(m) = e.metadata()
                {
                    candidates.entry(m.len()).or_default().push(p);
                }
            }
        }
    }
    for r in recs {
        for o in r
            .outputs
            .iter()
            .filter(|o| !o.ends_with(".d") && !o.ends_with(".o") && !o.ends_with(".rmeta"))
        {
            let src = r.out_dir.join(o);
            let Ok(m) = fs::metadata(&src) else { continue };
            let Ok(h) = hc.get(&src) else { continue };
            for c in candidates.get(&m.len()).into_iter().flatten() {
                if c != &src && hc.get(c).is_ok_and(|ch| ch == h) {
                    out.push((src.clone(), c.clone()));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

type WatchFiles = Vec<(PathBuf, String)>;
type WatchStamps = Vec<(PathBuf, u64, i64)>;

/// Files whose change makes Cargo plan differently.
fn watch_inputs(
    cwd: &Path,
    target: &Path,
    hc: &HashCache,
) -> io::Result<(WatchFiles, WatchStamps)> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stamps = Vec::new();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());

    let meta = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(cwd)
        .output()?;
    let meta: serde_json::Value = serde_json::from_slice(&meta.stdout).unwrap_or_default();
    let root = meta["workspace_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());
    files.push(root.join("Cargo.lock"));
    for pkg in meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["source"].is_null())
    {
        let manifest = PathBuf::from(pkg["manifest_path"].as_str().unwrap_or_default());
        let has_build_script = pkg["targets"].as_array().into_iter().flatten().any(|t| {
            t["kind"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k == "custom-build")
        });
        if has_build_script && let Some(dir) = manifest.parent() {
            stamp_tree(dir, target, &mut stamps);
        }
        files.push(manifest);
    }
    files.push(root.join("Cargo.toml"));
    for dir in root.ancestors() {
        for f in [
            ".cargo/config.toml",
            ".cargo/config",
            "rust-toolchain.toml",
            "rust-toolchain",
        ] {
            files.push(dir.join(f));
        }
    }
    files.push(home.join(".cargo/config.toml"));
    files.push(home.join(".rustup/settings.toml"));
    files.sort();
    files.dedup();
    let watch_files = files.into_iter().map(|p| {
        let h = hc.get(&p).unwrap_or_default();
        (p, h)
    });
    Ok((watch_files.collect(), stamps))
}

fn stamp_tree(dir: &Path, target: &Path, out: &mut Vec<(PathBuf, u64, i64)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || name == "target"
            || name == "node_modules"
            || p.starts_with(target)
        {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => stamp_tree(&p, target, out),
            Ok(t) if t.is_file() => {
                if let Ok(m) = e.metadata() {
                    out.push((p, m.len(), m.mtime()));
                }
            }
            _ => {}
        }
    }
}
