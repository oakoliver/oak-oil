//! What Cargo says about a build, from its stable JSON messages
//! (`--message-format=json-diagnostic-rendered-ansi`): every unit it
//! considered, fresh or not, with its output files, and every build-script
//! run. Compiler diagnostics are printed as Cargo would print them.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One compile unit as Cargo reported it.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub target_name: String,
    pub filenames: Vec<PathBuf>,
    pub fresh: bool,
}

#[derive(Default, Debug)]
pub struct Report {
    pub ok: bool,
    pub artifacts: Vec<Artifact>,
    /// Build-script run dirs (`<profile>/build/<pkg>-<id>`), from `out_dir`.
    pub run_dirs: Vec<PathBuf>,
}

/// Runs `cargo <args>` with JSON messages; `env` is added to Cargo's env.
pub fn run(cwd: &Path, args: &[String], env: &[(&str, &std::ffi::OsStr)]) -> io::Result<Report> {
    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .arg("--message-format=json-diagnostic-rendered-ansi")
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let mut r = Report::default();
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            println!("{line}"); // not a message: build output Cargo passes through
            continue;
        };
        match v["reason"].as_str() {
            Some("compiler-artifact") => r.artifacts.push(Artifact {
                target_name: v["target"]["name"].as_str().unwrap_or_default().to_string(),
                filenames: v["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str().map(PathBuf::from))
                    .collect(),
                fresh: v["fresh"].as_bool().unwrap_or(false),
            }),
            Some("build-script-executed") => {
                if let Some(run_dir) = v["out_dir"].as_str().and_then(|o| Path::new(o).parent()) {
                    r.run_dirs.push(run_dir.to_path_buf());
                }
            }
            Some("compiler-message") => {
                if let Some(text) = v["message"]["rendered"].as_str() {
                    let _ = io::stderr().write_all(text.as_bytes());
                }
            }
            _ => {}
        }
    }
    r.ok = child.wait()?.success();
    Ok(r)
}

/// `<name>-<16 hex>` (optionally `lib` prefix and extensions) → the hex id.
pub fn hash_id(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, id) = stem.rsplit_once('-')?;
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// The unit's profile dir and Cargo unit id, from one of its output files:
/// `<profile>/deps/libx-<id>.rlib`, `<profile>/examples/x-<id>`, or
/// `<profile>/build/<pkg>-<id>/build-script-build`. `None` for files with no
/// id in their path (uplifted copies, `cdylib` crates).
pub fn unit_of_file(f: &Path) -> Option<(PathBuf, String)> {
    let parent = f.parent()?;
    let dir_name = parent.file_name()?.to_str()?;
    if dir_name == "deps" || dir_name == "examples" {
        let id = hash_id(f.file_name()?.to_str()?)?;
        return Some((parent.parent()?.to_path_buf(), id.to_string()));
    }
    let grand = parent.parent()?;
    if grand.file_name()?.to_str()? == "build" {
        let id = hash_id(dir_name)?;
        return Some((grand.parent()?.to_path_buf(), id.to_string()));
    }
    None
}

/// Files with the same size and nanosecond mtime: Cargo's uplifted copy and
/// its original in `deps/`.
pub fn stat_key(p: &Path) -> Option<(u64, i64, i64)> {
    let m = fs::metadata(p).ok()?;
    Some((m.len(), m.mtime(), m.mtime_nsec()))
}

/// Cargo's fingerprint dirs for a unit that has no id in its file names
/// (`cdylib` crates): every `<profile>/.fingerprint/*` holding
/// `lib-`, `bin-` or `example-<target>`, newest first.
pub fn fingerprint_dirs_by_target(profile: &Path, target_name: &str) -> Vec<PathBuf> {
    let files = ["lib", "bin", "example"].map(|k| format!("{k}-{target_name}"));
    let mut v: Vec<(i64, PathBuf)> = fs::read_dir(profile.join(".fingerprint"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let m = files
                .iter()
                .find_map(|f| fs::metadata(e.path().join(f)).ok())?;
            Some((m.mtime() * 1_000_000_000 + m.mtime_nsec(), e.path()))
        })
        .collect();
    v.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    v.into_iter().map(|(_, p)| p).collect()
}

/// `<profile>/.fingerprint/<pkg>-<id>` for a unit id.
pub fn fingerprint_dir_by_id(profile: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}");
    fs::read_dir(profile.join(".fingerprint"))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
        })
}

/// Profile dir of an output file, whatever its layout.
pub fn profile_of_file(f: &Path) -> Option<PathBuf> {
    if let Some((profile, _)) = unit_of_file(f) {
        return Some(profile);
    }
    let parent = f.parent()?;
    match parent.file_name()?.to_str()? {
        "deps" | "examples" => parent.parent().map(Path::to_path_buf),
        _ => Some(parent.to_path_buf()), // uplifted into the profile dir
    }
}

/// Index of record outputs by (size, mtime) for mapping uplifted files back
/// to their unit.
pub fn stat_index<'a>(
    outputs: impl Iterator<Item = (&'a str, PathBuf)>,
) -> HashMap<(u64, i64, i64), String> {
    let mut m = HashMap::new();
    for (key, p) in outputs {
        if let Some(k) = stat_key(&p) {
            m.insert(k, key.to_string());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_from_paths() {
        assert_eq!(
            unit_of_file(Path::new(
                "/t/x86_64-apple-darwin/debug/deps/libserde-b611c61aabb71f5f.rlib"
            )),
            Some((
                PathBuf::from("/t/x86_64-apple-darwin/debug"),
                "b611c61aabb71f5f".into()
            ))
        );
        assert_eq!(
            unit_of_file(Path::new(
                "/t/debug/build/libc-57a6e4d171dba0c1/build-script-build"
            )),
            Some((PathBuf::from("/t/debug"), "57a6e4d171dba0c1".into()))
        );
        assert_eq!(unit_of_file(Path::new("/t/debug/my-app")), None);
        assert_eq!(
            unit_of_file(Path::new("/t/debug/deps/libffi_shim.dylib")),
            None
        );
        assert_eq!(
            profile_of_file(Path::new("/t/debug/my-app")),
            Some(PathBuf::from("/t/debug"))
        );
    }
}
