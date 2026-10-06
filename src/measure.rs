//! Phase 0: find every Cargo target dir and attribute its bytes to the pains
//! in the plan (P1 duplication, P2 stale artifacts, P4 metadata, P5
//! incremental, P6 debug info).

use crate::walk::{parallel_walk, subdirs};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hasher};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DAY: i64 = 86_400;

/// Directory names never searched for target dirs.
const SKIP_DIRS: &[&str] = &[
    "Library",
    ".Trash",
    "node_modules",
    ".git",
    ".npm",
    ".bun",
    ".pnpm-store",
    ".venv",
    "venv",
    "__pycache__",
    ".rustup",
];
const SKIP_SUFFIXES: &[&str] = &[".photoslibrary", ".app", ".musiclibrary", ".tvlibrary"];

pub struct Options {
    pub roots: Vec<PathBuf>,
    pub out: PathBuf,
    pub threads: usize,
    pub stale_days: i64,
    pub samples: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Incremental,
    Rlib,
    Rmeta,
    Objects,
    DebugInfo,
    DepsOther,
    DepInfo,
    BuildScripts,
    Fingerprint,
    Uplifted,
    Other,
}

impl Kind {
    const ALL: [Kind; 11] = [
        Kind::Incremental,
        Kind::Rlib,
        Kind::Rmeta,
        Kind::Objects,
        Kind::DebugInfo,
        Kind::DepsOther,
        Kind::DepInfo,
        Kind::BuildScripts,
        Kind::Fingerprint,
        Kind::Uplifted,
        Kind::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Incremental => "incremental/ caches",
            Kind::Rlib => "deps/*.rlib",
            Kind::Rmeta => "deps/*.rmeta",
            Kind::Objects => "deps/*.o (debug info on macOS)",
            Kind::DebugInfo => "*.dSYM, *.dwo, *.dwp",
            Kind::DepsOther => "deps/ binaries, dylibs, tests",
            Kind::DepInfo => "*.d dep-info",
            Kind::BuildScripts => "build/ (build scripts, OUT_DIR)",
            Kind::Fingerprint => ".fingerprint/",
            Kind::Uplifted => "uplifted outputs (target/<profile>/*)",
            Kind::Other => "other",
        }
    }
}

struct FileRec {
    path: PathBuf,
    target: usize,
    kind: Kind,
    profile: String,
    len: u64,
    /// Allocated bytes; 0 for the second and later hard links to one inode.
    disk: u64,
    atime: i64,
    mtime: i64,
}

pub fn run(opts: &Options) -> io::Result<PathBuf> {
    let t0 = Instant::now();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    eprintln!("oil: searching for target dirs under {:?}", opts.roots);
    let targets = find_targets(&opts.roots, opts.threads);
    eprintln!(
        "oil: {} target dirs found in {:.1?}",
        targets.len(),
        t0.elapsed()
    );

    let files = scan_targets(&targets, opts.threads);
    eprintln!("oil: {} files scanned in {:.1?}", files.len(), t0.elapsed());

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let cargo_home = disk_usage(&home.join(".cargo"), opts.threads);
    let rustup_home = disk_usage(&home.join(".rustup"), opts.threads);

    eprintln!("oil: hashing duplicate candidates");
    let dups = find_duplicates(&files, opts.threads);
    eprintln!("oil: duplicates done in {:.1?}", t0.elapsed());

    eprintln!("oil: sampling compressibility with zstd");
    let compress = sample_compression(&files, opts.samples, opts.threads);

    let report = render(
        opts,
        now,
        &targets,
        &files,
        &dups,
        &compress,
        cargo_home,
        rustup_home,
        t0.elapsed().as_secs_f64(),
    );

    fs::create_dir_all(&opts.out)?;
    let stamp = format_date(now);
    let md = opts.out.join(format!("measure-{stamp}.md"));
    fs::write(&md, report)?;
    fs::write(
        opts.out.join(format!("targets-{stamp}.csv")),
        targets_csv(&targets, &files),
    )?;
    Ok(md)
}

// ─── Discovery ──────────────────────────────────────────────────────────────

fn find_targets(roots: &[PathBuf], threads: usize) -> Vec<PathBuf> {
    let found = Mutex::new(Vec::new());
    parallel_walk(roots.to_vec(), threads, |dir, entries| {
        if is_cargo_target(dir, &entries) {
            found.lock().unwrap().push(dir.to_path_buf());
            return Vec::new();
        }
        subdirs(&entries)
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !SKIP_DIRS.contains(&name.as_ref())
                    && !SKIP_SUFFIXES.iter().any(|s| name.ends_with(s))
            })
            .map(|e| e.path())
            .collect()
    });
    let mut v = found.into_inner().unwrap();
    v.sort();
    v
}

/// Cargo writes `.rustc_info.json` and a `CACHEDIR.TAG` naming cargo into the
/// root of every target dir.
fn is_cargo_target(dir: &Path, entries: &[fs::DirEntry]) -> bool {
    let mut tag = false;
    for e in entries {
        match e.file_name().to_str() {
            Some(".rustc_info.json") => return true,
            Some("CACHEDIR.TAG") => tag = true,
            _ => {}
        }
    }
    tag && fs::read_to_string(dir.join("CACHEDIR.TAG"))
        .map(|s| s.contains("cargo"))
        .unwrap_or(false)
}

fn scan_targets(targets: &[PathBuf], threads: usize) -> Vec<FileRec> {
    let seen = Mutex::new(HashSet::<(u64, u64)>::new());
    let mut files = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let recs = Mutex::new(Vec::new());
        parallel_walk(vec![t.clone()], threads, |_, entries| {
            let mut local = Vec::new();
            let mut dirs = Vec::new();
            for e in entries {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    dirs.push(e.path());
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let path = e.path();
                let (kind, profile) = classify(path.strip_prefix(t).unwrap_or(&path));
                let first = md.nlink() <= 1 || seen.lock().unwrap().insert((md.dev(), md.ino()));
                local.push(FileRec {
                    path,
                    target: i,
                    kind,
                    profile,
                    len: md.len(),
                    disk: if first { md.blocks() * 512 } else { 0 },
                    atime: md.atime(),
                    mtime: md.mtime(),
                });
            }
            recs.lock().unwrap().extend(local);
            dirs
        });
        files.extend(recs.into_inner().unwrap());
    }
    files
}

fn disk_usage(root: &Path, threads: usize) -> u64 {
    let total = Mutex::new(0u64);
    parallel_walk(vec![root.to_path_buf()], threads, |_, entries| {
        let mut sum = 0;
        let mut dirs = Vec::new();
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                dirs.push(e.path());
            } else if ft.is_file() {
                sum += e.metadata().map(|m| m.blocks() * 512).unwrap_or(0);
            }
        }
        *total.lock().unwrap() += sum;
        dirs
    });
    total.into_inner().unwrap()
}

/// Kind and profile of a file from its path relative to the target dir.
/// Handles both `<profile>/…` and `<triple>/<profile>/…` layouts.
fn classify(rel: &Path) -> (Kind, String) {
    let comps: Vec<&str> = rel.iter().map(|c| c.to_str().unwrap_or("")).collect();
    if comps.len() < 2 {
        return (Kind::Other, "-".into());
    }
    let triple = comps[0].matches('-').count() >= 2 && comps.len() >= 3;
    let (profile, idx) = if triple {
        (format!("{}/{}", comps[0], comps[1]), 2)
    } else {
        (comps[0].to_string(), 1)
    };
    let name = comps[comps.len() - 1];
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    if comps.iter().any(|c| c.ends_with(".dSYM")) || ext == "dwo" || ext == "dwp" {
        return (Kind::DebugInfo, profile);
    }
    let kind = match comps.get(idx).copied() {
        _ if comps.len() == idx + 1 => {
            if ext == "d" {
                Kind::DepInfo
            } else {
                Kind::Uplifted
            }
        }
        Some("incremental") => Kind::Incremental,
        Some("deps") => match ext {
            "rlib" => Kind::Rlib,
            "rmeta" => Kind::Rmeta,
            "o" => Kind::Objects,
            "d" => Kind::DepInfo,
            _ => Kind::DepsOther,
        },
        Some("build") => Kind::BuildScripts,
        Some(".fingerprint") => Kind::Fingerprint,
        Some("examples") => Kind::DepsOther,
        _ => Kind::Other,
    };
    (kind, profile)
}

/// `libserde-1a2b3c4d5e6f7a8b.rlib` → `serde`.
fn crate_name(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    let (name, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(name)
}

// ─── Duplicates ─────────────────────────────────────────────────────────────

struct Dups {
    /// Bytes removable if identical files were stored once (P1).
    identical_bytes: u64,
    identical_files: usize,
    /// Bytes in files whose name (and so Cargo's metadata hash) repeats in
    /// another target dir.
    same_name_bytes: u64,
    /// Wasted bytes and copies per crate, from identical content.
    per_crate: Vec<(String, u64, usize)>,
}

fn find_duplicates(files: &[FileRec], threads: usize) -> Dups {
    let candidate = |f: &FileRec| {
        f.disk > 0
            && f.len >= 4096
            && matches!(
                f.kind,
                Kind::Rlib | Kind::Rmeta | Kind::Objects | Kind::DepsOther | Kind::BuildScripts
            )
    };

    let mut by_len: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate().filter(|(_, f)| candidate(f)) {
        by_len.entry(f.len).or_default().push(i);
    }
    let to_hash: Vec<usize> = by_len
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();

    let hashes = Mutex::new(HashMap::<usize, u64>::new());
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&i) = to_hash.get(k) else { break };
                    if let Ok(h) = hash_file(&files[i].path) {
                        local.push((i, h));
                    }
                }
                hashes.lock().unwrap().extend(local);
            });
        }
    });

    let mut groups: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, h) in hashes.into_inner().unwrap() {
        groups.entry((files[i].len, h)).or_default().push(i);
    }
    let mut identical_bytes = 0;
    let mut identical_files = 0;
    let mut per_crate: HashMap<String, (u64, usize)> = HashMap::new();
    for g in groups.values().filter(|g| g.len() > 1) {
        let extra: u64 = g.iter().skip(1).map(|&i| files[i].disk).sum();
        identical_bytes += extra;
        identical_files += g.len() - 1;
        if let Some(name) = crate_name(&files[g[0]].path) {
            let e = per_crate.entry(name.to_string()).or_default();
            e.0 += extra;
            e.1 = e.1.max(g.len());
        }
    }

    let mut by_name: HashMap<&std::ffi::OsStr, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.disk > 0 && matches!(f.kind, Kind::Rlib | Kind::Rmeta) {
            by_name
                .entry(f.path.file_name().unwrap())
                .or_default()
                .push(i);
        }
    }
    let same_name_bytes = by_name
        .values()
        .filter(|g| {
            g.iter()
                .map(|&i| files[i].target)
                .collect::<HashSet<_>>()
                .len()
                > 1
        })
        .map(|g| g.iter().skip(1).map(|&i| files[i].disk).sum::<u64>())
        .sum();

    let mut per_crate: Vec<_> = per_crate.into_iter().map(|(k, (b, n))| (k, b, n)).collect();
    per_crate.sort_by_key(|c| std::cmp::Reverse(c.1));
    Dups {
        identical_bytes,
        identical_files,
        same_name_bytes,
        per_crate,
    }
}

fn hash_file(path: &Path) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut h = DefaultHasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
    }
    Ok(h.finish())
}

// ─── Compression ────────────────────────────────────────────────────────────

/// (kind, sampled bytes, zstd -3 bytes, files sampled)
type Compression = Vec<(Kind, u64, u64, usize)>;

fn sample_compression(files: &[FileRec], samples: usize, threads: usize) -> Compression {
    if Command::new("zstd")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("oil: zstd not found, skipping compression samples");
        return Vec::new();
    }
    let kinds = [
        Kind::Rmeta,
        Kind::Rlib,
        Kind::Incremental,
        Kind::Objects,
        Kind::DebugInfo,
    ];
    let mut out = Vec::new();
    for kind in kinds {
        let pool: Vec<&FileRec> = files
            .iter()
            .filter(|f| f.kind == kind && f.disk > 0 && f.len >= 1024)
            .collect();
        if pool.is_empty() {
            continue;
        }
        let step = (pool.len() / samples.max(1)).max(1);
        let picked: Vec<&FileRec> = pool.iter().step_by(step).take(samples).copied().collect();
        let totals = Mutex::new((0u64, 0u64));
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.max(1) {
                s.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = picked.get(k) else { break };
                        let Ok(o) = Command::new("zstd")
                            .args(["-3", "-q", "-c"])
                            .arg(&f.path)
                            .output()
                        else {
                            continue;
                        };
                        if o.status.success() {
                            let mut t = totals.lock().unwrap();
                            t.0 += f.len;
                            t.1 += o.stdout.len() as u64;
                        }
                    }
                });
            }
        });
        let (raw, packed) = totals.into_inner().unwrap();
        out.push((kind, raw, packed, picked.len()));
    }
    out
}

// ─── Report ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render(
    opts: &Options,
    now: i64,
    targets: &[PathBuf],
    files: &[FileRec],
    dups: &Dups,
    compress: &Compression,
    cargo_home: u64,
    rustup_home: u64,
    secs: f64,
) -> String {
    let total: u64 = files.iter().map(|f| f.disk).sum();
    let sum_where = |p: &dyn Fn(&FileRec) -> bool| -> u64 {
        files.iter().filter(|f| p(f)).map(|f| f.disk).sum()
    };
    let pct = |b: u64| {
        if total == 0 {
            0.0
        } else {
            b as f64 * 100.0 / total as f64
        }
    };
    let mut r = String::new();

    let _ = writeln!(r, "# Oak Oil measurement, {}\n", format_date(now));
    let _ = writeln!(
        r,
        "Roots: {}. Scan time: {secs:.0} s. Sizes are allocated disk bytes; each hard-linked file is counted once.\n",
        opts.roots
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let _ = writeln!(r, "## Summary\n");
    let _ = writeln!(r, "| Item | Size |\n| --- | --- |");
    let _ = writeln!(r, "| Cargo target dirs found | {} |", targets.len());
    let _ = writeln!(r, "| Files in target dirs | {} |", files.len());
    let _ = writeln!(r, "| Total in target dirs | {} |", human(total));
    let _ = writeln!(
        r,
        "| `~/.cargo` (registry, git, bin) | {} |",
        human(cargo_home)
    );
    let _ = writeln!(r, "| `~/.rustup` (toolchains) | {} |\n", human(rustup_home));

    // Pains
    let stale = opts.stale_days * DAY;
    let older_variants = older_variant_bytes(files);
    let rmeta_twice = rmeta_with_rlib_bytes(files);
    let _ = writeln!(r, "## Bytes per pain\n");
    let _ = writeln!(r, "Rows overlap; they are not meant to add up.\n");
    let _ = writeln!(
        r,
        "| Pain | Measure | Size | Share |\n| --- | --- | --- | --- |"
    );
    let mut row = |pain: &str, what: &str, b: u64| {
        let _ = writeln!(r, "| {pain} | {what} | {} | {:.1}% |", human(b), pct(b));
    };
    row(
        "P1",
        "Identical files stored more than once (removable by G1)",
        dups.identical_bytes,
    );
    row(
        "P1",
        "Same rlib/rmeta file name in more than one target dir",
        dups.same_name_bytes,
    );
    row(
        "P2",
        "Older variants of the same crate in one deps/ dir (upper bound)",
        older_variants,
    );
    row(
        "P2",
        &format!("Not modified in {}+ days", opts.stale_days),
        sum_where(&|f| now - f.mtime > stale),
    );
    row(
        "P2",
        &format!(
            "Not accessed in {}+ days (atime, may be unreliable)",
            opts.stale_days
        ),
        sum_where(&|f| now - f.atime > stale),
    );
    row(
        "P4",
        "`.rmeta` files whose crate also has an `.rlib` (metadata stored twice)",
        rmeta_twice,
    );
    row(
        "P5",
        "Incremental caches",
        sum_where(&|f| f.kind == Kind::Incremental),
    );
    row(
        "P6",
        "Debug info: dSYM/dwo + deps `.o` files",
        sum_where(&|f| matches!(f.kind, Kind::DebugInfo | Kind::Objects)),
    );
    let _ = writeln!(
        r,
        "\nIdentical duplicate files: {}.\n",
        dups.identical_files
    );

    // Kinds
    let _ = writeln!(r, "## Bytes by kind\n");
    let _ = writeln!(r, "| Kind | Size | Share |\n| --- | --- | --- |");
    let mut kinds: Vec<(Kind, u64)> = Kind::ALL
        .iter()
        .map(|&k| (k, sum_where(&|f| f.kind == k)))
        .collect();
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    for (k, b) in kinds.iter().filter(|(_, b)| *b > 0) {
        let _ = writeln!(r, "| {} | {} | {:.1}% |", k.label(), human(*b), pct(*b));
    }

    // Profiles
    let mut profiles: HashMap<&str, u64> = HashMap::new();
    for f in files {
        *profiles.entry(f.profile.as_str()).or_default() += f.disk;
    }
    let mut profiles: Vec<_> = profiles.into_iter().collect();
    profiles.sort_by_key(|p| std::cmp::Reverse(p.1));
    let _ = writeln!(r, "\n## Bytes by profile (P3)\n");
    let _ = writeln!(r, "| Profile | Size | Share |\n| --- | --- | --- |");
    for (p, b) in profiles.iter().take(12) {
        let _ = writeln!(r, "| `{p}` | {} | {:.1}% |", human(*b), pct(*b));
    }

    // Ages
    let _ = writeln!(r, "\n## Age of bytes\n");
    let _ = writeln!(
        r,
        "| Age | By last modified | By last access |\n| --- | --- | --- |"
    );
    for (label, lo, hi) in [
        ("0–7 days", 0, 7),
        ("7–30 days", 7, 30),
        ("30–90 days", 30, 90),
        ("90+ days", 90, i64::MAX / DAY),
    ] {
        let m = sum_where(&|f| (lo * DAY..hi * DAY).contains(&(now - f.mtime)));
        let a = sum_where(&|f| (lo * DAY..hi * DAY).contains(&(now - f.atime)));
        let _ = writeln!(r, "| {label} | {} | {} |", human(m), human(a));
    }

    // Compression
    if !compress.is_empty() {
        let _ = writeln!(r, "\n## Compressibility (G6), zstd -3 on samples\n");
        let _ = writeln!(
            r,
            "| Kind | Files sampled | Sampled size | Compressed | Ratio |\n| --- | --- | --- | --- | --- |"
        );
        for (k, raw, packed, n) in compress {
            let ratio = if *packed == 0 {
                0.0
            } else {
                *raw as f64 / *packed as f64
            };
            let _ = writeln!(
                r,
                "| {} | {n} | {} | {} | {ratio:.1}× |",
                k.label(),
                human(*raw),
                human(*packed)
            );
        }
    }

    // Top crates
    let _ = writeln!(r, "\n## Most duplicated crates (identical content)\n");
    let _ = writeln!(
        r,
        "| Crate | Most copies of one file | Removable |\n| --- | --- | --- |"
    );
    for (name, b, n) in dups.per_crate.iter().take(20) {
        let _ = writeln!(r, "| `{name}` | {n} | {} |", human(*b));
    }

    // Top targets
    let mut per_target = vec![(0u64, 0i64); targets.len()];
    for f in files {
        per_target[f.target].0 += f.disk;
        per_target[f.target].1 = per_target[f.target].1.max(f.mtime);
    }
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(per_target[i].0));
    let _ = writeln!(r, "\n## Largest target dirs\n");
    let _ = writeln!(r, "| Target dir | Size | Last build |\n| --- | --- | --- |");
    for &i in order.iter().take(25) {
        let _ = writeln!(
            r,
            "| `{}` | {} | {} |",
            targets[i].display(),
            human(per_target[i].0),
            format_date(per_target[i].1)
        );
    }
    r
}

/// Bytes of rlib/rmeta files that have a newer variant (other hash) of the
/// same crate in the same deps/ dir. An upper bound for stale artifacts: two
/// variants can both be live (different features, build vs. normal deps).
fn older_variant_bytes(files: &[FileRec]) -> u64 {
    let mut groups: HashMap<(usize, &str, &str, &str), Vec<&FileRec>> = HashMap::new();
    for f in files
        .iter()
        .filter(|f| matches!(f.kind, Kind::Rlib | Kind::Rmeta))
    {
        let Some(name) = crate_name(&f.path) else {
            continue;
        };
        let ext = f.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        groups
            .entry((f.target, f.profile.as_str(), name, ext))
            .or_default()
            .push(f);
    }
    groups
        .values()
        .filter(|g| g.len() > 1)
        .map(|g| {
            let newest = g.iter().map(|f| f.mtime).max().unwrap_or(0);
            g.iter()
                .filter(|f| f.mtime < newest)
                .map(|f| f.disk)
                .sum::<u64>()
        })
        .sum()
}

fn rmeta_with_rlib_bytes(files: &[FileRec]) -> u64 {
    let rlibs: HashSet<PathBuf> = files
        .iter()
        .filter(|f| f.kind == Kind::Rlib)
        .map(|f| f.path.with_extension(""))
        .collect();
    files
        .iter()
        .filter(|f| f.kind == Kind::Rmeta && rlibs.contains(&f.path.with_extension("")))
        .map(|f| f.disk)
        .sum()
}

fn targets_csv(targets: &[PathBuf], files: &[FileRec]) -> String {
    let mut per = vec![(0u64, 0usize, 0i64); targets.len()];
    for f in files {
        per[f.target].0 += f.disk;
        per[f.target].1 += 1;
        per[f.target].2 = per[f.target].2.max(f.mtime);
    }
    let mut s = String::from("target_dir,disk_bytes,files,last_modified\n");
    for (t, (b, n, m)) in targets.iter().zip(per) {
        let _ = writeln!(s, "\"{}\",{b},{n},{}", t.display(), format_date(m));
    }
    s
}

fn human(b: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let b = b as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else {
        format!("{:.0} MiB", b / MIB)
    }
}

/// Unix seconds → `YYYY-MM-DD` (UTC).
fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(DAY);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_layouts() {
        assert_eq!(
            classify(Path::new("debug/deps/libserde-0123456789abcdef.rlib")).0,
            Kind::Rlib
        );
        assert_eq!(
            classify(Path::new("debug/incremental/x-1/s-2/dep-graph.bin")).0,
            Kind::Incremental
        );
        let (k, p) = classify(Path::new(
            "aarch64-apple-darwin/release/deps/foo-0123456789abcdef.rmeta",
        ));
        assert_eq!(
            (k, p.as_str()),
            (Kind::Rmeta, "aarch64-apple-darwin/release")
        );
        assert_eq!(classify(Path::new("debug/myapp")).0, Kind::Uplifted);
        assert_eq!(
            classify(Path::new("debug/deps/myapp.dSYM/Contents/Info.plist")).0,
            Kind::DebugInfo
        );
    }

    #[test]
    fn crate_names() {
        assert_eq!(
            crate_name(Path::new("libserde_json-0123456789abcdef.rlib")),
            Some("serde_json")
        );
        assert_eq!(
            crate_name(Path::new("tokio-0123456789abcdef.rmeta")),
            Some("tokio")
        );
        assert_eq!(crate_name(Path::new("libfoo.rlib")), None);
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_791_158_400), "2026-10-05");
    }
}
