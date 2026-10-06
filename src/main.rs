//! `cargo oil` — Oak Oil: lean, fast Rust builds.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "Oak Oil supports macOS and Linux. See https://github.com/oakoliver/oak-oil#limitations"
);

mod cargo_msgs;
mod clean;
mod depinfo;
mod drain;
mod exec;
mod gc;
mod hashing;
mod measure;
mod objects;
mod plan;
mod rustc_args;
mod stamps;
mod store;
mod unit;
mod walk;
mod wrapper;

use hashing::HashCache;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "\
Usage:
  cargo oil build [--replan] [CARGO BUILD ARGS]...
  cargo oil test [CARGO TEST ARGS]... [-- TEST ARGS]
      Builds the test targets through Oak Oil, then runs `cargo test`.
      Runs the recorded plan with the Oak Oil executor (store hits, rmeta
      pipelining). Records the plan through Cargo when there is none or when
      manifests, lockfile, configs, toolchain or build-script inputs changed.
  cargo oil clean [--apply] [--other-dirs] [--cmd \"ARGS\"]...
      Reports (and with --apply removes) build residue in this project's
      target dir: unit variants and incremental sessions Cargo no longer uses
      for the commands (default: build, test --no-run, build --release when a
      release dir exists). Re-checks that Cargo then recompiles nothing.
      --other-dirs also removes dirs in target/ no command uses.
  cargo oil gc [--max-age-days N] [--max-size SIZE] [--dry-run]
      Trim the store (~/.oakoil): objects unused for N days (default 30),
      then least recently used until it fits SIZE (default 40G). Also runs
      after builds, at most once a day (OAKOIL_GC=off disables it).
  cargo oil measure [--root DIR]... [--out DIR] [--stale-days N] [--samples N] [--threads N]
      Phase 0: bytes per pain in every Cargo target dir under the roots.";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // Cargo runs `RUSTC_WRAPPER /path/to/rustc …` while recording.
    if std::env::var_os("OAKOIL_WRAPPER").is_some()
        && argv.get(1).is_some_and(|a| looks_like_rustc(a))
    {
        return wrapper::run(argv[1..].to_vec());
    }
    let mut args: Vec<String> = argv[1..].to_vec();
    // `cargo oil …` runs `cargo-oil oil …`.
    if args.first().map(String::as_str) == Some("oil") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        Some("build") => build(&args[1..]),
        Some("test") => test_cmd(&args[1..]),
        Some("clean") => clean_cmd(&args[1..]),
        Some("gc") => gc_cmd(&args[1..]),
        Some("store-drain") => match args.get(1).map(|f| drain::run(Path::new(f))) {
            Some(Ok(())) => ExitCode::SUCCESS,
            _ => ExitCode::FAILURE,
        },
        Some("wait-store") => match store::Store::open() {
            Ok(st) => {
                let waited = drain::wait(st.root());
                eprintln!(
                    "oil: store writes finished (waited {:.2}s)",
                    waited.as_secs_f64()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("oil: {e}");
                ExitCode::FAILURE
            }
        },
        Some("measure") => match parse_measure(&args[1..]) {
            Ok(opts) => match measure::run(&opts) {
                Ok(path) => {
                    println!("{}", path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("oil: {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("oil: {e}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn gc_cmd(args: &[String]) -> ExitCode {
    let mut limits = gc::Limits::from_env();
    let mut dry_run = false;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        let parsed: Result<(), String> = match flag.as_str() {
            "--dry-run" => {
                dry_run = true;
                Ok(())
            }
            "--max-age-days" => value().and_then(|v| {
                let d: u64 = v.parse().map_err(|e| format!("{flag}: {e}"))?;
                limits.max_age = std::time::Duration::from_secs(d * 86_400);
                Ok(())
            }),
            "--max-size" => value().and_then(|v| {
                limits.max_bytes = gc::parse_size(v).ok_or(format!("{flag}: bad size {v}"))?;
                Ok(())
            }),
            _ => Err(format!("unknown gc argument {flag}")),
        };
        if let Err(e) = parsed {
            eprintln!("oil: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let result = store::Store::open().and_then(|st| {
        drain::wait(st.root());
        gc::run(st.root(), &limits, dry_run)
    });
    match result {
        Ok(r) => {
            eprintln!(
                "oil: store: {} objects, {}; {} {} objects, {}",
                r.objects,
                gc::human(r.bytes),
                if dry_run { "would remove" } else { "removed" },
                r.removed,
                gc::human(r.removed_bytes),
            );
            eprintln!(
                "oil: target dirs share blocks with the store (APFS clones), so the disk can gain less than that."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("oil: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_measure(args: &[String]) -> Result<measure::Options, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let mut opts = measure::Options {
        roots: Vec::new(),
        out: PathBuf::from("reports"),
        threads: std::thread::available_parallelism().map_or(8, |n| n.get()),
        stale_days: 30,
        samples: 200,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--root" => opts.roots.push(PathBuf::from(value()?)),
            "--out" => opts.out = PathBuf::from(value()?),
            "--stale-days" => {
                opts.stale_days = value()?.parse().map_err(|e| format!("{flag}: {e}"))?
            }
            "--samples" => opts.samples = value()?.parse().map_err(|e| format!("{flag}: {e}"))?,
            "--threads" => opts.threads = value()?.parse().map_err(|e| format!("{flag}: {e}"))?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if opts.roots.is_empty() {
        opts.roots.push(home);
    }
    Ok(opts)
}

fn looks_like_rustc(a: &str) -> bool {
    Path::new(a).file_name().is_some_and(|n| {
        n.to_string_lossy().starts_with("rustc") || n.to_string_lossy().starts_with("clippy-driver")
    })
}

/// `cargo oil test [ARGS] [-- TEST ARGS]`: executes the plan of
/// `cargo test --no-run` (exactly the units `cargo test` needs, recorded
/// separately from the `build` plan), then runs `cargo test ARGS`, which
/// finds every unit fresh and only runs the tests (doctests still go through
/// rustdoc, not the executor).
fn test_cmd(rest: &[String]) -> ExitCode {
    let mut cargo_args = vec!["test".to_string(), "--no-run".to_string()];
    cargo_args.extend(test_build_args(rest));
    let built = run_plan(cargo_args);
    if built != ExitCode::SUCCESS {
        return built;
    }
    match std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("test")
        .args(rest)
        .status()
    {
        Ok(s) => ExitCode::from(s.code().map_or(1, |c| c.clamp(0, 255) as u8)),
        Err(e) => {
            eprintln!("oil: running cargo test: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The arguments of `cargo test` that also mean something to `cargo build`:
/// options (and the values of those that take one), not test-name filters,
/// test-only flags or anything after `--`.
fn test_build_args(rest: &[String]) -> Vec<String> {
    const WITH_VALUE: &[&str] = &[
        "-p",
        "--package",
        "--exclude",
        "-F",
        "--features",
        "--target",
        "--target-dir",
        "--profile",
        "-j",
        "--jobs",
        "--manifest-path",
        "--color",
        "--config",
        "-Z",
        "--bin",
        "--example",
        "--test",
        "--bench",
        "--message-format",
        "--lockfile-path",
    ];
    const TEST_ONLY: &[&str] = &[
        "--no-run",
        "--no-fail-fast",
        "--doc",
        "--future-incompat-report",
    ];
    let mut out = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if TEST_ONLY.contains(&a.as_str()) || !a.starts_with('-') {
            continue; // test-only flag or a TESTNAME filter
        }
        out.push(a.clone());
        if WITH_VALUE.contains(&a.as_str())
            && let Some(v) = it.next()
        {
            out.push(v.clone());
        }
    }
    out
}

fn build(rest: &[String]) -> ExitCode {
    let mut cargo_args = vec!["build".to_string()];
    cargo_args.extend(rest.iter().cloned());
    run_plan(cargo_args)
}

/// Records (when needed) and executes the plan of one Cargo command line.
fn run_plan(cargo_args: Vec<String>) -> ExitCode {
    match try_build(cargo_args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("oil: {e}");
            ExitCode::FAILURE
        }
    }
}

fn try_build(mut cargo_args: Vec<String>) -> std::io::Result<ExitCode> {
    let t0 = Instant::now();
    let replan = cargo_args.iter().any(|a| a == "--replan");
    cargo_args.retain(|a| a != "--replan");
    let cwd = std::env::current_dir()?;
    let store_root = store::Store::open()?.root().to_path_buf();
    let plan_file = plan::plan_path(&store_root, &cwd, &cargo_args);
    let jobs = std::thread::available_parallelism().map_or(8, |n| n.get());

    if !replan && let Some(p) = plan::load(&plan_file) {
        let hc_file = p.target.join("oakoil").join("hashcache.json");
        let hc = HashCache::load(&hc_file);
        match p.invalid_reason(&hc) {
            Some(why) => eprintln!("oil: re-planning: {why}"),
            None => {
                let ctx = unit::Ctx::new(p.target.clone(), hc)?;
                let result = exec::run(&p, &ctx, jobs);
                let _ = ctx.hc.save(&hc_file);
                match result {
                    Ok(s) => {
                        eprintln!(
                            "oil: {} units: {} fresh, {} from store, {} compiled in {:.2}s",
                            p.units.len(),
                            s.fresh,
                            s.hit,
                            s.compiled,
                            t0.elapsed().as_secs_f64()
                        );
                        return Ok(ExitCode::SUCCESS);
                    }
                    Err(exec::ExecError::Failed(code)) => {
                        return Ok(ExitCode::from(code.clamp(1, 255) as u8));
                    }
                    Err(exec::ExecError::Replan(why)) => eprintln!("oil: re-planning: {why}"),
                    Err(exec::ExecError::Io(e)) => {
                        eprintln!("oil: executor error, re-planning: {e}")
                    }
                }
            }
        }
    }

    let target = target_dir(&cwd)?;
    // Cargo creates the target dir itself (CACHEDIR.TAG, Time Machine
    // exclusion); Oak Oil only adds `oakoil/` inside it afterwards.
    let oil_dir = target.join("oakoil");
    let _ = std::fs::remove_file(oil_dir.join("wrapper.log"));
    let hc_file = oil_dir.join("hashcache.json");
    let hc = HashCache::load(&hc_file);
    hc.allow_xattr(&target);
    let recorded = plan::record(&cwd, &cargo_args, &target, &hc)?;
    let _ = std::fs::create_dir_all(&oil_dir).and_then(|_| hc.save(&hc_file));
    let Some(rec) = recorded else {
        return Ok(ExitCode::FAILURE);
    };
    rec.plan.save(&plan_file)?;
    // Store writes queued by the wrapper and the plan's snapshots of Cargo
    // state: in the background (OAKOIL_SYNC_STORE=1: now).
    drain::after_record(&target, &plan_file)?;
    let log = std::fs::read_to_string(oil_dir.join("wrapper.log")).unwrap_or_default();
    let count = |o: &str| {
        log.lines()
            .filter(|l| l.split('\t').nth(1) == Some(o))
            .count()
    };
    eprintln!(
        "oil: planned {} units through cargo in {:.2}s (rustc calls: {} from store, {} compiled, {} fresh; {} re-sent)",
        rec.plan.units.len(),
        t0.elapsed().as_secs_f64(),
        count("Hit"),
        count("Compiled"),
        count("Fresh"),
        rec.forced_units
    );
    Ok(ExitCode::SUCCESS)
}

fn target_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR") {
        return Ok(cwd.join(t));
    }
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(cwd)
        .output()?;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(std::io::Error::other)?;
    v["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::other("no target_directory"))
}

fn clean_cmd(rest: &[String]) -> ExitCode {
    let run = || -> std::io::Result<bool> {
        let cwd = std::env::current_dir()?;
        let target = target_dir(&cwd)?;
        let mut commands = Vec::new();
        let mut it = rest.iter();
        let mut apply = false;
        let mut other_dirs = false;
        while let Some(a) = it.next() {
            match a.as_str() {
                "--apply" => apply = true,
                "--other-dirs" => other_dirs = true,
                "--cmd" => commands.push(
                    it.next()
                        .ok_or_else(|| std::io::Error::other("--cmd needs a value"))?
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                ),
                other => return Err(std::io::Error::other(format!("unknown argument {other}"))),
            }
        }
        if commands.is_empty() {
            commands = clean::default_commands(&target);
        }
        clean::run(
            &cwd,
            &target,
            &clean::Options {
                commands,
                apply,
                other_dirs,
            },
        )
    };
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("oil: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_build_args;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_args_keep_build_options_only() {
        assert_eq!(
            test_build_args(&v(&[
                "-p",
                "zflow-heft",
                "--release",
                "schedule",
                "--no-fail-fast",
                "--features",
                "x",
                "--",
                "--nocapture"
            ])),
            v(&["-p", "zflow-heft", "--release", "--features", "x"])
        );
        assert_eq!(
            test_build_args(&v(&["--workspace", "--exclude", "a"])),
            v(&["--workspace", "--exclude", "a"])
        );
    }
}
