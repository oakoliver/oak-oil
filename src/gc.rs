//! `cargo oil gc`: keep the store (`~/.oakoil`) bounded.
//!
//! An object's last use is when it was last restored into a target dir or
//! confirmed present there, or when it was stored. Objects unused for longer
//! than the age limit go first; then the least recently used, until the
//! store fits the size limit. A build that loses an object mid-restore fails
//! that restore and re-plans through Cargo: GC never produces a wrong output,
//! only a slower build.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct Limits {
    pub max_age: Duration,
    pub max_bytes: u64,
}

impl Limits {
    pub const DEFAULT_AGE_DAYS: u64 = 30;
    pub const DEFAULT_MAX_GIB: u64 = 40;

    /// Defaults, overridden by `OAKOIL_GC_MAX_AGE_DAYS` and `OAKOIL_GC_MAX_SIZE`.
    pub fn from_env() -> Limits {
        let days = std::env::var("OAKOIL_GC_MAX_AGE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(Self::DEFAULT_AGE_DAYS);
        let bytes = std::env::var("OAKOIL_GC_MAX_SIZE")
            .ok()
            .and_then(|v| parse_size(&v))
            .unwrap_or(Self::DEFAULT_MAX_GIB << 30);
        Limits {
            max_age: Duration::from_secs(days * 86_400),
            max_bytes: bytes,
        }
    }
}

/// `40G`, `500M`, `1024` (bytes).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'G' | 'g' => (&s[..s.len() - 1], 1u64 << 30),
        'M' | 'm' => (&s[..s.len() - 1], 1u64 << 20),
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * mult as f64) as u64)
}

pub fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.0} KiB", b as f64 / 1024.0),
    }
}

#[derive(Debug, Clone)]
struct Object {
    dir: PathBuf,
    last_use: SystemTime,
    bytes: u64,
}

#[derive(Default, Debug)]
pub struct Report {
    pub objects: usize,
    pub bytes: u64,
    pub removed: usize,
    pub removed_bytes: u64,
    pub stale_manifests: usize,
}

/// Which objects to remove: past the age limit, then oldest first until the
/// rest fits the size limit.
fn select(mut objs: Vec<Object>, limits: &Limits, now: SystemTime) -> Vec<Object> {
    objs.sort_by_key(|o| o.last_use);
    let mut total: u64 = objs.iter().map(|o| o.bytes).sum();
    let mut out = Vec::new();
    for o in objs {
        let too_old = now
            .duration_since(o.last_use)
            .is_ok_and(|age| age > limits.max_age);
        if too_old || total > limits.max_bytes {
            total -= o.bytes;
            out.push(o);
        }
    }
    out
}

pub fn run(store_root: &Path, limits: &Limits, dry_run: bool) -> io::Result<Report> {
    let now = SystemTime::now();
    let objects = scan_objects(&store_root.join("objects"))?;
    let mut r = Report {
        objects: objects.len(),
        bytes: objects.iter().map(|o| o.bytes).sum(),
        ..Report::default()
    };
    let victims = select(objects, limits, now);
    r.removed = victims.len();
    r.removed_bytes = victims.iter().map(|o| o.bytes).sum();
    if dry_run {
        return Ok(r);
    }
    let tmp = store_root.join("tmp");
    fs::create_dir_all(&tmp)?;
    for o in &victims {
        // Rename first: a reader sees the whole object or none of it.
        let name = o.dir.file_name().unwrap().to_string_lossy().to_string();
        let doomed = tmp.join(format!("gc-{name}-{}", std::process::id()));
        if fs::rename(&o.dir, &doomed).is_ok() {
            let _ = fs::remove_dir_all(&doomed);
        }
    }
    r.stale_manifests =
        prune_manifests(&store_root.join("manifests"), &store_root.join("objects"))?;
    let _ = fs::write(store_root.join("gc-last-run"), b"");
    Ok(r)
}

/// Automatic GC after a background store write, at most once a day.
/// `OAKOIL_GC=off` disables it.
pub fn auto(store_root: &Path) {
    if std::env::var("OAKOIL_GC").is_ok_and(|v| v == "off") {
        return;
    }
    let marker = store_root.join("gc-last-run");
    let recent = fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < Duration::from_secs(86_400));
    if !recent {
        let _ = run(store_root, &Limits::from_env(), false);
    }
}

fn scan_objects(objects: &Path) -> io::Result<Vec<Object>> {
    let mut out = Vec::new();
    let Ok(shards) = fs::read_dir(objects) else {
        return Ok(out);
    };
    for shard in shards.filter_map(Result::ok) {
        let Ok(rd) = fs::read_dir(shard.path()) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let stored = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let used = fs::metadata(dir.join(".last-use"))
                .and_then(|m| m.modified())
                .ok();
            out.push(Object {
                last_use: used.map_or(stored, |u| u.max(stored)),
                bytes: dir_bytes(&dir),
                dir,
            });
        }
    }
    Ok(out)
}

/// Manifest entries whose object is gone, and manifest dirs left empty.
fn prune_manifests(manifests: &Path, objects: &Path) -> io::Result<usize> {
    let mut live: HashSet<String> = HashSet::new();
    if let Ok(shards) = fs::read_dir(objects) {
        for s in shards.filter_map(Result::ok) {
            if let Ok(rd) = fs::read_dir(s.path()) {
                live.extend(
                    rd.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().to_string()),
                );
            }
        }
    }
    let mut removed = 0;
    let Ok(shards) = fs::read_dir(manifests) else {
        return Ok(0);
    };
    for shard in shards.filter_map(Result::ok) {
        let Ok(keys) = fs::read_dir(shard.path()) else {
            continue;
        };
        for key in keys.filter_map(Result::ok) {
            let Ok(entries) = fs::read_dir(key.path()) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(obj) = name.strip_suffix(".json")
                    && !obj.starts_with('.')
                    && !live.contains(obj)
                {
                    let _ = fs::remove_file(entry.path());
                    removed += 1;
                }
            }
            let _ = fs::remove_dir(key.path()); // only succeeds when empty
        }
    }
    Ok(removed)
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.filter_map(Result::ok) {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.blocks() * 512,
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(name: &str, days_ago: u64, gib: u64, now: SystemTime) -> Object {
        Object {
            dir: PathBuf::from(name),
            last_use: now - Duration::from_secs(days_ago * 86_400),
            bytes: gib << 30,
        }
    }

    #[test]
    fn age_then_size() {
        let now = SystemTime::now();
        let limits = Limits {
            max_age: Duration::from_secs(30 * 86_400),
            max_bytes: 10 << 30,
        };
        let objs = vec![
            obj("old", 40, 1, now), // past the age limit
            obj("a", 20, 6, now),   // oldest within age: goes to fit 10 GiB
            obj("b", 10, 6, now),   // kept
            obj("c", 1, 3, now),    // kept
        ];
        let names: Vec<String> = select(objs, &limits, now)
            .into_iter()
            .map(|o| o.dir.display().to_string())
            .collect();
        assert_eq!(names, vec!["old", "a"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("40G"), Some(40 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("x"), None);
    }
}
