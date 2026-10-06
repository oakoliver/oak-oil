//! The executor: runs a plan's units without Cargo, in parallel, starting a
//! unit as soon as the `.rmeta` (or, for linking units, the full output) of
//! each dependency exists — Cargo's pipelining, Bun's "Cargo plans, ninja
//! executes".

use crate::plan::Plan;
use crate::stamps::Stamps;
use crate::unit::{Ctx, Echo, Jobserver, Outcome, Record, load_record, record_key_of, run_unit};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::SystemTime;

/// Cargo's text (cargo-util `paths.rs`), byte for byte.
const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55
# This file is a cache directory tag created by cargo.
# For information about cache directory tags see https://bford.info/cachedir/
";

#[derive(Default, Debug)]
pub struct Stats {
    pub fresh: usize,
    pub hit: usize,
    pub compiled: usize,
}

pub enum ExecError {
    /// The plan does not hold (records missing); rebuild it through Cargo.
    Replan(String),
    /// A unit failed to compile; the exit code to return.
    Failed(i32),
    Io(io::Error),
}

#[derive(Clone, Copy, PartialEq)]
enum Need {
    Meta,
    Full,
}

struct Sched {
    started: Vec<bool>,
    meta: Vec<bool>,
    done: Vec<bool>,
    running: usize,
    failed: Option<i32>,
    stats: Stats,
}

pub fn run(plan: &Plan, ctx: &Ctx, jobs: usize) -> Result<Stats, ExecError> {
    let mut recs: Vec<Record> = Vec::with_capacity(plan.units.len());
    for k in &plan.units {
        recs.push(
            load_record(&plan.target, k)
                .ok_or_else(|| ExecError::Replan(format!("record {k} missing")))?,
        );
    }
    let index: HashMap<&str, usize> = recs
        .iter()
        .enumerate()
        .map(|(i, r)| (r.key.as_str(), i))
        .collect();

    // Dependencies from each unit's --extern paths; linking units wait for
    // the full output of everything they reach.
    let mut direct: Vec<Vec<(usize, Need)>> = vec![vec![]; recs.len()];
    let mut links = vec![false; recs.len()];
    for (i, r) in recs.iter().enumerate() {
        let inv = r
            .invocation()
            .ok_or_else(|| ExecError::Replan(format!("bad record {}", r.key)))?;
        links[i] = inv.links();
        for e in &inv.externs {
            let Some(&j) = index.get(record_key_of(e).as_str()) else {
                return Err(ExecError::Replan(format!(
                    "{} needs {} outside the plan",
                    r.key,
                    e.display()
                )));
            };
            let need = if e.extension().is_some_and(|x| x == "rmeta") {
                Need::Meta
            } else {
                Need::Full
            };
            direct[i].push((j, need));
        }
    }
    // Build-script results the units read (OUT_DIR) come back first; their
    // timestamps are set again once their build script is in place.
    let needs_restore = plan
        .build_runs
        .iter()
        .chain(&plan.fingerprints)
        .any(|(d, _)| !d.is_dir());
    if needs_restore && !plan.snapshots_stored() {
        return Err(ExecError::Replan("Cargo state not in the store yet".into()));
    }
    let mut restored_runs: Vec<PathBuf> = Vec::new();
    for (dir, obj) in &plan.build_runs {
        if !dir.is_dir() {
            ctx.store.restore_tree(obj, dir).map_err(restore_err)?;
            restored_runs.push(dir.clone());
        }
    }
    // `build/<pkg>-<id>` → package name, to pair a run dir with the unit(s)
    // compiling its build script. With `--target`, scripts are compiled in
    // the host profile dir and run in the target's, so only the name matches.
    let pkg_of = |p: &std::path::Path| {
        Some(
            p.file_name()?
                .to_string_lossy()
                .rsplit_once('-')?
                .0
                .to_string(),
        )
    };
    let mut script_units: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in recs.iter().enumerate() {
        if r.key.starts_with("build_script_")
            && let Some(k) = pkg_of(&r.out_dir)
        {
            script_units.entry(k).or_default().push(i);
        }
    }
    let mut runs_of: Vec<Vec<PathBuf>> = vec![vec![]; recs.len()];
    for d in &restored_runs {
        // stamped once the last of its build-script units is in place
        if let Some(&j) = pkg_of(d)
            .and_then(|k| script_units.get(&k))
            .and_then(|v| v.iter().max())
        {
            runs_of[j].push(d.clone());
        }
    }
    let mut out_dir_edges = Vec::new();
    for (i, r) in recs.iter().enumerate() {
        if let Some((_, out)) = r.env.iter().find(|(k, _)| k == "OUT_DIR")
            && let Some(run_dir) = std::path::Path::new(out).parent()
            && let Some(units) = pkg_of(run_dir).and_then(|k| script_units.get(&k))
        {
            for &j in units.iter().filter(|&&j| j != i) {
                out_dir_edges.push((i, j));
            }
        }
    }
    for (i, j) in out_dir_edges {
        direct[i].push((j, Need::Full));
    }
    let deps: Vec<Vec<(usize, Need)>> = (0..recs.len())
        .map(|i| {
            if !links[i] {
                return direct[i].clone();
            }
            let mut seen = HashSet::new();
            let mut stack: Vec<usize> = direct[i].iter().map(|(j, _)| *j).collect();
            while let Some(j) = stack.pop() {
                if seen.insert(j) {
                    stack.extend(direct[j].iter().map(|(k, _)| *k));
                }
            }
            seen.into_iter().map(|j| (j, Need::Full)).collect()
        })
        .collect();
    let mut uplifts: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (src, dst) in &plan.uplifts {
        uplifts.entry(src.clone()).or_default().push(dst.clone());
    }

    let n = recs.len();
    // Critical path first, like Cargo: a unit's priority is the length of the
    // longest chain of units waiting on it.
    let mut dependents: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, ds) in deps.iter().enumerate() {
        for &(j, _) in ds {
            dependents[j].push(i);
        }
    }
    let mut depth = vec![0usize; n];
    let mut order: Vec<usize> = (0..n).collect();
    // dependents before their dependencies: repeat relaxation to a fixed point
    for _ in 0..n {
        let mut changed = false;
        for &i in &order {
            let d = dependents[i]
                .iter()
                .map(|&k| depth[k] + 1)
                .max()
                .unwrap_or(0);
            if d > depth[i] {
                depth[i] = d;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        order.reverse();
    }
    let js = Jobserver::new(jobs.max(1)).map_err(ExecError::Io)?;
    ctx.defer_puts();
    let t_exec = std::time::Instant::now();

    let state = Mutex::new(Sched {
        started: vec![false; n],
        meta: vec![false; n],
        done: vec![false; n],
        running: 0,
        failed: None,
        stats: Stats::default(),
    });
    let cv = Condvar::new();
    let io_err: Mutex<Option<io::Error>> = Mutex::new(None);

    let ready = |s: &Sched, i: usize| {
        deps[i].iter().all(|&(j, need)| match need {
            Need::Meta => s.meta[j] || s.done[j],
            Need::Full => s.done[j],
        })
    };

    let old_stamps = Stamps::load(&plan.target);
    let new_stamps = Mutex::new(Stamps::load(&plan.target));
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.max(1)).map(|_| scope.spawn(|| loop {
                let i = {
                    let mut s = state.lock().unwrap();
                    loop {
                        if s.failed.is_some() || s.done.iter().all(|d| *d) {
                            cv.notify_all();
                            return;
                        }
                        if let Some(i) = (0..n).filter(|&i| !s.started[i] && ready(&s, i)).max_by_key(|&i| (depth[i], usize::MAX - i)) {
                            s.started[i] = true;
                            s.running += 1;
                            break i;
                        }
                        if s.running == 0 {
                            // Nothing runnable and nothing running: a cycle.
                            s.failed = Some(1);
                            cv.notify_all();
                            return;
                        }
                        s = cv.wait(s).unwrap();
                    }
                };

                let t_unit = std::time::Instant::now();
                let on_meta = || {
                    state.lock().unwrap().meta[i] = true;
                    cv.notify_all();
                };
                // Stamps first: nothing it read or wrote changed → fresh, no
                // hashing, no store. Otherwise the content-keyed path.
                let result = if old_stamps.is_fresh(&recs[i]) {
                    on_meta();
                    Ok(Outcome::Fresh)
                } else {
                    run_unit(ctx, &recs[i], &on_meta, Echo::Rendered, Some(&js)).and_then(|(outcome, outputs)| {
                    if outcome != Outcome::Fresh {
                        uplift(&recs[i], &outputs, &uplifts)?;
                    }
                    if !matches!(outcome, Outcome::Failed(_)) {
                        new_stamps.lock().unwrap().record(&recs[i], &outputs);
                    }
                    Ok(outcome)
                    })
                }
                .and_then(|outcome| {
                    for d in &runs_of[i] {
                        touch_tree(d)?;
                    }
                    Ok(outcome)
                });

                let trace = match &result {
                    Ok(o) if *o != Outcome::Fresh && std::env::var_os("OAKOIL_TRACE").is_some() => {
                        let p = crate::unit::PHASES.with(|p| p.get());
                        Some(format!(
                            "oil-trace: {:?} {} start={:.2} wall={:.2} key={:.3} lookup={:.3} wait={:.3} rustc={:.2} post={:.3}",
                            o, recs[i].key, (t_unit - t_exec).as_secs_f64(), t_unit.elapsed().as_secs_f64(), p[0], p[1], p[2], p[3], p[4]
                        ))
                    }
                    _ => None,
                };
                if let Some(t) = trace {
                    eprintln!("{t}"); // outside the scheduler lock
                }
                let mut s = state.lock().unwrap();
                s.running -= 1;
                match result {
                    Ok(Outcome::Failed(code)) => s.failed = Some(code),
                    Ok(o) => {
                        s.meta[i] = true;
                        s.done[i] = true;
                        match o {
                            Outcome::Fresh => s.stats.fresh += 1,
                            Outcome::Hit => s.stats.hit += 1,
                            _ => s.stats.compiled += 1,
                        }
                    }
                    Err(e) => {
                        *io_err.lock().unwrap() = Some(e);
                        s.failed = Some(1);
                    }
                }
                cv.notify_all();
            })).collect();
        for w in workers {
            let _ = w.join();
        }
    });
    let _ = new_stamps.into_inner().unwrap().save(&plan.target);

    if let Some(e) = io_err.into_inner().unwrap() {
        return Err(ExecError::Io(e));
    }
    let s = state.into_inner().unwrap();
    // Store writes never hold up the build: a detached background process
    // does them (OAKOIL_SYNC_STORE=1: here, before returning).
    if std::env::var_os("OAKOIL_SYNC_STORE").is_some() {
        ctx.drain_puts(jobs);
    } else {
        let _ = crate::drain::spawn(ctx);
    }
    if let Some(code) = s.failed {
        return Err(ExecError::Failed(code));
    }
    // Cargo's cache-dir tags, dep-info and fingerprints last, newer than every output.
    for tag in &plan.cachedir_tags {
        if tag.parent().is_some_and(|d| d.is_dir()) && !tag.exists() {
            fs::write(tag, CACHEDIR_TAG).map_err(ExecError::Io)?;
        }
    }
    for (file, obj) in &plan.cargo_dep_infos {
        if !file.exists() && !obj.is_empty() {
            ctx.store
                .restore_tree_missing(obj, file.parent().unwrap())
                .map_err(restore_err)?;
        }
    }
    for (dir, obj) in &plan.fingerprints {
        if !dir.is_dir() {
            ctx.store.restore_tree(obj, dir).map_err(restore_err)?;
        }
    }
    Ok(s.stats)
}

/// Mirror Cargo's uplift of final outputs into the profile dir.
fn uplift(
    rec: &Record,
    outputs: &[String],
    map: &HashMap<PathBuf, Vec<PathBuf>>,
) -> io::Result<()> {
    let now = SystemTime::now();
    for o in outputs {
        for dst in map.get(&rec.out_dir.join(o)).into_iter().flatten() {
            let _ = fs::remove_file(dst);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            uplift_file(&rec.out_dir.join(o), dst, now)?;
        }
    }
    Ok(())
}

fn touch_tree(dir: &std::path::Path) -> io::Result<()> {
    let now = SystemTime::now();
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            touch_tree(&e.path())?;
        } else {
            File::options()
                .write(true)
                .open(e.path())?
                .set_modified(now)?;
        }
    }
    Ok(())
}

/// A snapshot object `gc` removed means re-planning through Cargo, which
/// stores it again.
fn restore_err(e: std::io::Error) -> ExecError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ExecError::Replan("Cargo state no longer in the store (gc)".into())
    } else {
        ExecError::Io(e)
    }
}

/// Cargo's own uplift: a hardlink on Linux, a clone stamped now on macOS.
fn uplift_file(src: &Path, dst: &Path, now: SystemTime) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst)?;
    File::options().write(true).open(dst)?.set_modified(now)
}
