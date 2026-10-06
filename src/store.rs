//! The content-addressed store under `~/.oakoil` (or `$OAKOIL_HOME`).
//!
//! Two levels, like a compiler cache:
//! - `manifests/<mkey>/<obj>.json`: for one command line + flags + dependency
//!   contents (the manifest key), the source files and env vars a past compile
//!   read and their hashes then. Several entries per key, one per source state.
//! - `objects/<obj>/`: the outputs of that compile, stored once.
//!
//! Paths inside the target dir are stored as `{T}` so units whose artifacts
//! hold no target path can be shared across projects.

use crate::hashing::{HashCache, hash_bytes};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const TARGET_TOKEN: &str = "{T}";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub obj: String,
    /// (path with `{T}`, blake3)
    pub inputs: Vec<(String, String)>,
    pub env: Vec<(String, Option<String>)>,
    /// (file name, blake3 of the stored bytes)
    pub files: Vec<(String, String)>,
    /// rustc's stderr (JSON diagnostics and artifact notices), with `{T}`.
    pub stderr: Vec<String>,
}

pub struct Store {
    root: PathBuf,
}

pub fn norm(s: &str, target: &str) -> String {
    s.replace(target, TARGET_TOKEN)
}

pub fn denorm(s: &str, target: &str) -> String {
    s.replace(TARGET_TOKEN, target)
}

impl Store {
    pub fn open() -> io::Result<Store> {
        let root = std::env::var_os("OAKOIL_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".oakoil")
            });
        for d in ["objects", "manifests", "tmp"] {
            fs::create_dir_all(root.join(d))?;
        }
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn manifest_dir(&self, mkey: &str) -> PathBuf {
        self.root.join("manifests").join(&mkey[..2]).join(mkey)
    }

    fn object_dir(&self, obj: &str) -> PathBuf {
        self.root.join("objects").join(&obj[..2]).join(obj)
    }

    /// The newest entry for `mkey` whose recorded inputs still hash the same.
    pub fn lookup(
        &self,
        mkey: &str,
        target: &str,
        hc: &HashCache,
        env_get: &dyn Fn(&str) -> Option<String>,
    ) -> Option<Entry> {
        let dir = self.manifest_dir(mkey);
        let mut entries: Vec<(SystemTime, PathBuf)> = fs::read_dir(&dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (_, path) in entries {
            let Ok(e) = fs::read(&path)
                .map_err(drop)
                .and_then(|b| serde_json::from_slice::<Entry>(&b).map_err(drop))
            else {
                continue;
            };
            let inputs_ok = e.inputs.iter().all(|(p, h)| {
                hc.get(Path::new(&denorm(p, target)))
                    .is_ok_and(|now| &now == h)
            });
            let env_ok = e.env.iter().all(|(k, v)| env_get(k) == *v);
            if inputs_ok && env_ok && self.object_dir(&e.obj).is_dir() {
                return Some(e);
            }
        }
        None
    }

    /// True when `out_dir` already holds exactly this entry's outputs.
    pub fn is_present(&self, e: &Entry, out_dir: &Path, hc: &HashCache) -> bool {
        e.files.iter().all(|(name, h)| {
            name.ends_with(".d") && out_dir.join(name).is_file()
                || hc.get(&out_dir.join(name)).is_ok_and(|now| &now == h)
        })
    }

    /// Clone the entry's files into `out_dir` (APFS clonefile through
    /// `fs::copy`: independent files, no extra disk) and stamp them now, so
    /// Cargo and later units see them as new.
    pub fn materialize(
        &self,
        e: &Entry,
        out_dir: &Path,
        target: &str,
        hc: &HashCache,
    ) -> io::Result<()> {
        let obj = self.object_dir(&e.obj);
        let now = SystemTime::now();
        fs::create_dir_all(out_dir)?;
        for (name, h) in &e.files {
            let dst = out_dir.join(name);
            let _ = fs::remove_file(&dst);
            if name.ends_with(".d") {
                fs::write(&dst, denorm(&fs::read_to_string(obj.join(name))?, target))?;
            } else {
                fs::copy(obj.join(name), &dst)?;
                File::options().write(true).open(&dst)?.set_modified(now)?;
                hc.seed(&dst, h);
            }
        }
        // Rewritten .d files and seeded hashes are newer than the clones.
        for (name, _) in e.files.iter().filter(|(n, _)| n.ends_with(".d")) {
            File::options()
                .write(true)
                .open(out_dir.join(name))?
                .set_modified(now)?;
        }
        let _ = fs::write(obj.join(".last-use"), b"");
        Ok(())
    }

    /// Store the outputs of a finished compile under `mkey`.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        mkey: &str,
        inputs: Vec<(String, String)>,
        env: Vec<(String, Option<String>)>,
        outputs: &[PathBuf],
        stderr: Vec<String>,
        target: &str,
        hc: &HashCache,
    ) -> io::Result<Entry> {
        let obj = hash_bytes(
            format!(
                "{mkey}\n{}",
                serde_json::to_string(&(&inputs, &env)).unwrap_or_default()
            )
            .as_bytes(),
        );
        let final_dir = self.object_dir(&obj);
        let mut files = Vec::new();
        if final_dir.is_dir() {
            for p in outputs {
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                let h = if name.ends_with(".d") {
                    String::new()
                } else {
                    hc.get(p)?
                };
                files.push((name, h));
            }
        } else {
            let tmp = self
                .root
                .join("tmp")
                .join(format!("{obj}.{}", std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp)?;
            for p in outputs {
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                if name.ends_with(".d") {
                    fs::write(tmp.join(&name), norm(&fs::read_to_string(p)?, target))?;
                    files.push((name, String::new()));
                } else {
                    fs::copy(p, tmp.join(&name))?;
                    files.push((name, hc.get(p)?));
                }
            }
            fs::create_dir_all(final_dir.parent().unwrap())?;
            if fs::rename(&tmp, &final_dir).is_err() {
                let _ = fs::remove_dir_all(&tmp); // another process stored it first
            }
        }
        let entry = Entry {
            obj: obj.clone(),
            inputs,
            env,
            files,
            stderr,
        };
        let mdir = self.manifest_dir(mkey);
        fs::create_dir_all(&mdir)?;
        let tmp = mdir.join(format!(".{obj}.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(&entry)?)?;
        fs::rename(tmp, mdir.join(format!("{obj}.json")))?;
        Ok(entry)
    }

    /// Store a whole directory (Cargo state such as a fingerprint dir or a
    /// build-script run dir) as one object; returns its id.
    pub fn put_tree(&self, dir: &Path, hc: &HashCache) -> io::Result<String> {
        let mut files = Vec::new();
        collect_files(dir, dir, &mut files)?;
        files.sort();
        let mut listing = Vec::new();
        for rel in &files {
            listing.push((rel.clone(), hc.get(&dir.join(rel))?));
        }
        let obj = hash_bytes(serde_json::to_string(&listing)?.as_bytes());
        let final_dir = self.object_dir(&obj);
        if !final_dir.is_dir() {
            let tmp = self
                .root
                .join("tmp")
                .join(format!("{obj}.{}", std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            for rel in &files {
                let dst = tmp.join("tree").join(rel);
                fs::create_dir_all(dst.parent().unwrap())?;
                fs::copy(dir.join(rel), dst)?;
            }
            fs::create_dir_all(&tmp)?;
            fs::write(tmp.join("tree.json"), serde_json::to_vec(&listing)?)?;
            fs::create_dir_all(final_dir.parent().unwrap())?;
            if fs::rename(&tmp, &final_dir).is_err() {
                let _ = fs::remove_dir_all(&tmp);
            }
        }
        Ok(obj)
    }

    /// Recreate a stored directory, every file stamped now.
    pub fn restore_tree(&self, obj: &str, dir: &Path) -> io::Result<()> {
        let src = self.object_dir(obj);
        let listing: Vec<(PathBuf, String)> =
            serde_json::from_slice(&fs::read(src.join("tree.json"))?)?;
        let now = SystemTime::now();
        for (rel, _) in &listing {
            let dst = dir.join(rel);
            fs::create_dir_all(dst.parent().unwrap())?;
            let _ = fs::remove_file(&dst);
            fs::copy(src.join("tree").join(rel), &dst)?;
            File::options().write(true).open(&dst)?.set_modified(now)?;
        }
        fs::create_dir_all(dir)
    }
}

impl Store {
    /// Like `restore_tree`, but only files that are missing.
    pub fn restore_tree_missing(&self, obj: &str, dir: &Path) -> io::Result<()> {
        let src = self.object_dir(obj);
        let listing: Vec<(PathBuf, String)> =
            serde_json::from_slice(&fs::read(src.join("tree.json"))?)?;
        let now = SystemTime::now();
        for (rel, _) in &listing {
            let dst = dir.join(rel);
            if dst.exists() {
                continue;
            }
            fs::create_dir_all(dst.parent().unwrap())?;
            fs::copy(src.join("tree").join(rel), &dst)?;
            File::options().write(true).open(&dst)?.set_modified(now)?;
        }
        Ok(())
    }
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            collect_files(root, &e.path(), out)?;
        } else if t.is_file() {
            out.push(e.path().strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}
