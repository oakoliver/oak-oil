//! The plan: Cargo's unit graph as exact rustc commands, recorded through the
//! wrapper, plus what must stay unchanged for the plan to hold (manifests,
//! lockfile, configs, toolchain, build-script inputs, env).
//!
//! Like Bun's `rust-target/plan.json`: the plan is rebuilt only when one of
//! those inputs changes; otherwise the executor runs it without Cargo.

use crate::hashing::{HashCache, hash_bytes};
use crate::unit::{Record, record_key_of, records_dir};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const PLAN_VERSION: u32 = 8;

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
    /// Cargo-written dep-info next to uplifted outputs (`debug/foo.d`):
    /// (file, one-file tree object).
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

    /// True when every snapshot is in the store (the drain has run).
    pub fn snapshots_stored(&self) -> bool {
        self.build_runs
            .iter()
            .chain(&self.fingerprints)
            .chain(&self.cargo_dep_infos)
            .all(|(_, o)| !o.is_empty())
    }

    /// Store the snapshots not stored yet (run by the background drain).
    pub fn fill_snapshots(
        &mut self,
        store: &crate::store::Store,
        hc: &HashCache,
    ) -> io::Result<()> {
        for (dir, obj) in self
            .build_runs
            .iter_mut()
            .chain(self.fingerprints.iter_mut())
        {
            if obj.is_empty() && dir.is_dir() {
                *obj = store.put_tree(dir, hc)?;
            }
        }
        for (file, obj) in self.cargo_dep_infos.iter_mut() {
            if obj.is_empty() && file.is_file() {
                let tmp = store
                    .root()
                    .join("tmp")
                    .join(format!("dinfo.{}", std::process::id()));
                let _ = fs::remove_dir_all(&tmp);
                fs::create_dir_all(&tmp)?;
                fs::copy(&*file, tmp.join(file.file_name().unwrap()))?;
                *obj = store.put_tree(&tmp, hc)?;
                let _ = fs::remove_dir_all(&tmp);
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(tmp, path)
    }
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
    let mut attempt = 0;
    let exe = std::env::current_exe()?;
    let (live_keys, mut fp_dirs, run_dirs) = loop {
        let t_cargo = std::time::Instant::now();
        let report = crate::cargo_msgs::run(
            cwd,
            args,
            &[
                ("RUSTC_WRAPPER", exe.as_os_str()),
                ("OAKOIL_WRAPPER", "1".as_ref()),
                ("OAKOIL_TARGET", target.as_os_str()),
            ],
        )?;
        if std::env::var_os("OAKOIL_TRACE").is_some() {
            eprintln!(
                "oil-trace: cargo run {:.3}s ({} units)",
                t_cargo.elapsed().as_secs_f64(),
                report.artifacts.len()
            );
        }
        if !report.ok {
            return Ok(None);
        }
        let by_key = records_by_key(target);
        // uplifted copies and build-script copies carry no unit id: map them
        // back to their record by size and mtime
        let idx = crate::cargo_msgs::stat_index(by_key.values().flat_map(|r| {
            r.outputs
                .iter()
                .map(move |o| (r.key.as_str(), r.out_dir.join(o)))
        }));
        let mut live = HashSet::new();
        let mut fps = Vec::new();
        let mut missing = 0;
        let mut to_force = Vec::new();
        for a in &report.artifacts {
            let key = a.filenames.iter().find_map(|f| {
                let k = record_key_of(f);
                if by_key.contains_key(&k) {
                    return Some(k);
                }
                crate::cargo_msgs::stat_key(f).and_then(|s| idx.get(&s).cloned())
            });
            // the unit's id: from its record's own outputs in deps/ or build/
            // (uplifted binaries and cdylib crates carry none in Cargo's list)
            let unit = key
                .as_ref()
                .and_then(|k| by_key.get(k))
                .and_then(|r| {
                    r.outputs
                        .iter()
                        .find_map(|o| crate::cargo_msgs::unit_of_file(&r.out_dir.join(o)))
                })
                .or_else(|| {
                    a.filenames
                        .iter()
                        .find_map(|f| crate::cargo_msgs::unit_of_file(f))
                });
            let fp: Vec<PathBuf> = match unit {
                Some((profile, id)) => crate::cargo_msgs::fingerprint_dir_by_id(&profile, &id)
                    .into_iter()
                    .collect(),
                None => a
                    .filenames
                    .first()
                    .and_then(|f| crate::cargo_msgs::profile_of_file(f))
                    .map(|p| crate::cargo_msgs::fingerprint_dirs_by_target(&p, &a.target_name))
                    .unwrap_or_default(),
            };
            match key {
                Some(k) => {
                    live.insert(k);
                    fps.extend(fp.into_iter().take(1));
                }
                None => {
                    missing += 1;
                    to_force.extend(fp);
                }
            }
        }
        // build-script runs: `<profile>/build/<pkg>-<id>` ↔ `<profile>/.fingerprint/<pkg>-<id>`
        for d in &report.run_dirs {
            if let (Some(profile), Some(name)) = (d.parent().and_then(Path::parent), d.file_name())
            {
                let f = profile.join(".fingerprint").join(name);
                if f.is_dir() {
                    fps.push(f);
                }
            }
        }
        if missing == 0 {
            break (live, fps, report.run_dirs);
        }
        if attempt == 1 || to_force.is_empty() {
            eprintln!("oil: {missing} units could not be recorded; plan not saved");
            return Ok(None);
        }
        forced = missing;
        for d in &to_force {
            let _ = fs::remove_dir_all(d);
        }
        attempt += 1;
    };

    let trace = std::env::var_os("OAKOIL_TRACE").is_some();
    let t_plan = std::time::Instant::now();
    let by_key = records_by_key(target);
    let mut units: Vec<String> = live_keys
        .into_iter()
        .filter(|k| by_key.contains_key(k))
        .collect();
    units.sort();
    let recs: Vec<Record> = units
        .iter()
        .filter_map(|k| by_key.get(k).cloned())
        .collect();

    let uplifts = find_uplifts(&recs);
    if trace {
        eprintln!(
            "oil-trace: records+uplifts {:.3}s",
            t_plan.elapsed().as_secs_f64()
        );
    }
    // Snapshots of Cargo's own state, stored later by the background drain
    // (`fill_snapshots`); an empty object id means "not stored yet".
    let mut cargo_dep_infos: Vec<(PathBuf, String)> = uplifts
        .iter()
        .map(|(_, dst)| dst.with_extension("d"))
        .filter(|d| d.is_file())
        .map(|d| (d, String::new()))
        .collect();
    cargo_dep_infos.sort();
    cargo_dep_infos.dedup();
    fp_dirs.sort();
    fp_dirs.dedup();
    let fingerprints: Vec<(PathBuf, String)> = fp_dirs
        .into_iter()
        .filter(|d| d.is_dir())
        .map(|d| (d, String::new()))
        .collect();
    let mut build_runs: Vec<(PathBuf, String)> = run_dirs
        .into_iter()
        .filter(|d| d.is_dir())
        .map(|d| (d, String::new()))
        .collect();
    build_runs.sort();
    build_runs.dedup();
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

/// Record key → record, for every unit the wrapper has seen in this target.
fn records_by_key(target: &Path) -> HashMap<String, Record> {
    let mut m = HashMap::new();
    if let Ok(rd) = fs::read_dir(records_dir(target)) {
        for e in rd.filter_map(Result::ok) {
            if let Some(r) = fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
            {
                m.insert(r.key.clone(), r);
            }
        }
    }
    m
}

/// Cargo copies final outputs from `deps/` up into the profile dir; find
/// those copies by size and content.
fn find_uplifts(recs: &[Record]) -> Vec<(PathBuf, PathBuf)> {
    // Cargo's uplifted copy keeps the size and the nanosecond mtime of the
    // file in deps/ (a clone or hard link), so no file needs reading.
    let key = |m: &fs::Metadata| (m.len(), m.mtime(), m.mtime_nsec());
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut candidates: HashMap<(u64, i64, i64), Vec<PathBuf>> = HashMap::new();
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
                if p.extension().is_none_or(|x| x != "d")
                    && let Ok(m) = e.metadata()
                    && m.is_file()
                {
                    candidates.entry(key(&m)).or_default().push(p);
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
            for c in candidates.get(&key(&m)).into_iter().flatten() {
                if c != &src {
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
