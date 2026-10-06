//! `cargo oil clean`: remove build residue Cargo will never use again, keep
//! everything it does use. Live = the units Cargo's own fingerprint check
//! visits for the given commands; the rest of those profile dirs is residue.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Options {
    pub commands: Vec<Vec<String>>,
    pub apply: bool,
    pub other_dirs: bool,
}

#[derive(Default)]
struct Residue {
    stale_units: Vec<PathBuf>,
    old_incremental: Vec<PathBuf>,
    other_dirs: Vec<PathBuf>,
}

/// Default commands: dev build and tests, plus release when a release dir
/// exists.
pub fn default_commands(target: &Path) -> Vec<Vec<String>> {
    let mut v = vec![
        vec!["build".to_string()],
        vec!["test".to_string(), "--no-run".to_string()],
    ];
    let has_release = target.join("release").is_dir()
        || fs::read_dir(target)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|e| e.path().join("release").is_dir());
    if has_release {
        v.push(vec!["build".to_string(), "--release".to_string()]);
    }
    v
}

pub fn run(cwd: &Path, target: &Path, opts: &Options) -> io::Result<bool> {
    let before = dir_bytes(target);
    let (live, compiled) = live_units(cwd, target, &opts.commands)?;
    if compiled > 0 {
        eprintln!("oil: {compiled} units were out of date and are now built (they are live)");
    }
    let profiles: HashSet<PathBuf> = live.iter().map(|(p, _)| p.clone()).collect();
    let ids: HashSet<(PathBuf, String)> = live;
    let r = find_residue(target, &profiles, &ids)?;

    let sum = |v: &[PathBuf]| v.iter().map(|p| path_bytes(p)).sum::<u64>();
    let (a, b, c) = (
        sum(&r.stale_units),
        sum(&r.old_incremental),
        sum(&r.other_dirs),
    );
    println!("target dir: {} ({})", target.display(), human(before));
    println!(
        "  stale unit variants      {:>10}  ({} entries)",
        human(a),
        r.stale_units.len()
    );
    println!(
        "  old incremental sessions {:>10}  ({} dirs)",
        human(b),
        r.old_incremental.len()
    );
    println!(
        "  other dirs in target/    {:>10}  ({}){}",
        human(c),
        r.other_dirs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy())
            .collect::<Vec<_>>()
            .join(", "),
        if opts.other_dirs {
            ""
        } else {
            "  [kept; --other-dirs removes]"
        }
    );
    let removable = a + b + if opts.other_dirs { c } else { 0 };
    if !opts.apply {
        println!("would free {}  (run with --apply)", human(removable));
        return Ok(true);
    }

    let mut doomed: Vec<&PathBuf> = r.stale_units.iter().chain(&r.old_incremental).collect();
    if opts.other_dirs {
        doomed.extend(&r.other_dirs);
    }
    for p in doomed {
        let _ = if p.is_dir() {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        };
    }
    let after = dir_bytes(target);
    println!(
        "freed {} ({} → {})",
        human(before.saturating_sub(after)),
        human(before),
        human(after)
    );

    // Proof: the same commands now compile nothing.
    let (_, recompiled) = live_units(cwd, target, &opts.commands)?;
    if recompiled == 0 {
        println!(
            "verified: cargo recompiles nothing for {} command(s)",
            opts.commands.len()
        );
        Ok(true)
    } else {
        eprintln!(
            "oil: WARNING: {recompiled} units were rebuilt after cleaning (they are rebuilt now)"
        );
        Ok(false)
    }
}

/// Runs each command with Cargo's fingerprint log. Returns the live
/// (profile dir, unit id) pairs and how many units Cargo compiled.
fn live_units(
    cwd: &Path,
    target: &Path,
    commands: &[Vec<String>],
) -> io::Result<(HashSet<(PathBuf, String)>, usize)> {
    let mut live = HashSet::new();
    let mut compiled = 0;
    for c in commands {
        let mut child = Command::new("cargo")
            .args(c)
            .current_dir(cwd)
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_LOG", "cargo::core::compiler::fingerprint=debug")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        for line in BufReader::new(child.stderr.take().unwrap()).lines() {
            let line = line?;
            if let Some(i) = line.find("fingerprint at: ") {
                let fp = PathBuf::from(line[i + 16..].trim());
                if let Some(dir) = fp.parent()
                    && let Some(profile) = dir.parent().and_then(Path::parent)
                    && let Some((_, id)) = dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.rsplit_once('-'))
                {
                    live.insert((profile.to_path_buf(), id.to_string()));
                }
            } else if line.trim_start().starts_with("Compiling ") {
                compiled += 1;
            }
        }
        if !child.wait()?.success() {
            eprintln!(
                "oil: `cargo {}` failed; its units are kept as they are",
                c.join(" ")
            );
        }
    }
    Ok((live, compiled))
}

/// `<name>-<16 hex>` (optionally `lib` prefix and extensions) → id.
fn unit_id(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, id) = stem.rsplit_once('-')?;
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

fn find_residue(
    target: &Path,
    profiles: &HashSet<PathBuf>,
    live: &HashSet<(PathBuf, String)>,
) -> io::Result<Residue> {
    let mut r = Residue::default();
    for prof in profiles {
        for sub in ["deps", "build", ".fingerprint", "examples"] {
            let Ok(rd) = fs::read_dir(prof.join(sub)) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(id) = unit_id(&name)
                    && !live.contains(&(prof.clone(), id.to_string()))
                {
                    r.stale_units.push(e.path());
                }
            }
        }
        // Incremental: rustc keeps one session per crate dir; dirs of older
        // unit variants of the same crate are never read again.
        if let Ok(rd) = fs::read_dir(prof.join("incremental")) {
            let mut newest: HashMap<String, (i64, PathBuf)> = HashMap::new();
            let mut all = Vec::new();
            for e in rd.filter_map(Result::ok) {
                let name = e.file_name().to_string_lossy().to_string();
                let Some((krate, _)) = name.rsplit_once('-') else {
                    continue;
                };
                let mtime = e.metadata().map(|m| m.mtime()).unwrap_or(0);
                all.push(e.path());
                let slot = newest.entry(krate.to_string()).or_insert((mtime, e.path()));
                if mtime > slot.0 {
                    *slot = (mtime, e.path());
                }
            }
            let keep: HashSet<PathBuf> = newest.into_values().map(|(_, p)| p).collect();
            r.old_incremental
                .extend(all.into_iter().filter(|p| !keep.contains(p)));
        }
    }
    // Dirs in target/ that hold no live profile: other profiles, other
    // triples, nested target dirs, tool caches.
    for e in fs::read_dir(target)?.filter_map(Result::ok) {
        let p = e.path();
        if !p.is_dir() || e.file_name() == "oakoil" {
            continue;
        }
        if !profiles.iter().any(|prof| prof.starts_with(&p)) {
            r.other_dirs.push(p);
        }
    }
    r.other_dirs.sort();
    Ok(r)
}

fn path_bytes(p: &Path) -> u64 {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => dir_bytes(p),
        Ok(m) => m.blocks() * 512,
        Err(_) => 0,
    }
}

fn dir_bytes(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    let mut seen = HashSet::new();
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.filter_map(Result::ok) {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if seen.insert((m.dev(), m.ino())) {
                total += m.blocks() * 512;
            }
        }
    }
    total
}

fn human(b: u64) -> String {
    let b = b as f64;
    if b >= (1u64 << 30) as f64 {
        format!("{:.1} GiB", b / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MiB", b / (1u64 << 20) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::unit_id;

    #[test]
    fn unit_ids() {
        assert_eq!(
            unit_id("libserde-b611c61aabb71f5f.rlib"),
            Some("b611c61aabb71f5f")
        );
        assert_eq!(
            unit_id("my_app-2690356174c6854d.0ab.rcgu.o"),
            Some("2690356174c6854d")
        );
        assert_eq!(unit_id("serde-87fecf52bc4fa236"), Some("87fecf52bc4fa236"));
        assert_eq!(unit_id("libffi_shim.rlib"), None);
    }
}
