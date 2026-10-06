//! Freshness without reading files: per unit, the stamps (size, mtime,
//! inode) of everything it read and wrote, as of its last build. A unit whose
//! stamps all still match, under the same command and env, is fresh. Any
//! difference sends it to the content-keyed path (store lookup, early cutoff,
//! or rustc).

use crate::depinfo;
use crate::hashing::{Stamp, hash_bytes};
use crate::unit::Record;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct UnitStamp {
    /// Hash of the unit's command, cwd and env as recorded.
    sig: String,
    files: Vec<(PathBuf, Stamp)>,
    env: Vec<(String, Option<String>)>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Stamps {
    units: HashMap<String, UnitStamp>,
}

pub fn path(target: &Path) -> PathBuf {
    target.join("oakoil").join("stamps.json")
}

fn sig(rec: &Record) -> String {
    hash_bytes(
        serde_json::to_string(&(&rec.rustc, &rec.args, &rec.cwd, &rec.env))
            .unwrap_or_default()
            .as_bytes(),
    )
}

impl Stamps {
    pub fn load(target: &Path) -> Stamps {
        fs::read(path(target))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, target: &Path) -> io::Result<()> {
        let p = path(target);
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(tmp, p)
    }

    /// True when nothing the unit read or wrote has changed since its stamp.
    pub fn is_fresh(&self, rec: &Record) -> bool {
        let Some(u) = self.units.get(&rec.key) else {
            return false;
        };
        u.sig == sig(rec)
            && u.env.iter().all(|(k, v)| rec.env_value(k) == *v)
            && u.files
                .iter()
                .all(|(p, s)| Stamp::of_path(p).as_ref() == Some(s))
    }

    /// Stamp a unit that just finished: rustc, its sources (rustc's
    /// dep-info), its dependencies, and its main outputs.
    pub fn record(&mut self, rec: &Record, outputs: &[String]) {
        let Some(inv) = rec.invocation() else { return };
        let Ok(text) = fs::read_to_string(inv.dep_info()) else {
            return;
        };
        let d = depinfo::parse(&text, &rec.cwd, &inv.out_dir);
        let mut paths: Vec<PathBuf> = vec![rec.rustc.clone()];
        paths.extend(d.files);
        paths.extend(inv.externs.iter().cloned());
        paths.extend(outputs.iter().map(|o| inv.out_dir.join(o)));
        let mut files = Vec::with_capacity(paths.len());
        for p in paths {
            match Stamp::of_path(&p) {
                Some(s) => files.push((p, s)),
                None => {
                    self.units.remove(&rec.key);
                    return;
                }
            }
        }
        self.units.insert(
            rec.key.clone(),
            UnitStamp {
                sig: sig(rec),
                files,
                env: d.env,
            },
        );
    }
}
