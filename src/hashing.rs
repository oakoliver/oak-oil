//! Content hashes (blake3), cached by (len, mtime, inode) so a file is read at
//! most once while it does not change.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub fn hash_bytes(b: &[u8]) -> String {
    blake3::hash(b).to_hex().to_string()
}

pub fn hash_file(p: &Path) -> io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_mmap(p)?;
    Ok(h.finalize().to_hex().to_string())
}

/// What a file looks like without reading it: size, mtime, inode.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Stamp {
    len: u64,
    mtime: i64,
    mtime_ns: i64,
    ino: u64,
}

impl Stamp {
    pub fn of_path(p: &Path) -> Option<Stamp> {
        fs::metadata(p).ok().map(|m| Stamp::of(&m))
    }

    fn of(md: &fs::Metadata) -> Stamp {
        Stamp {
            len: md.len(),
            mtime: md.mtime(),
            mtime_ns: md.mtime_nsec(),
            ino: md.ino(),
        }
    }
}

#[derive(Default)]
pub struct HashCache {
    map: Mutex<HashMap<PathBuf, (Stamp, String)>>,
    /// Dirs whose files may carry the hash xattr (the target dir). Source
    /// files are never touched: an xattr would change their ctime.
    xattr_roots: Mutex<Vec<PathBuf>>,
}

impl HashCache {
    pub fn load(path: &Path) -> HashCache {
        let map = fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(PathBuf, Stamp, String)>>(&b).ok())
            .map(|v| v.into_iter().map(|(p, s, h)| (p, (s, h))).collect())
            .unwrap_or_default();
        HashCache {
            map: Mutex::new(map),
            xattr_roots: Mutex::default(),
        }
    }

    pub fn allow_xattr(&self, root: &Path) {
        self.xattr_roots.lock().unwrap().push(root.to_path_buf());
    }

    fn may_tag(&self, p: &Path) -> bool {
        self.xattr_roots
            .lock()
            .unwrap()
            .iter()
            .any(|r| p.starts_with(r))
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let v: Vec<(PathBuf, Stamp, String)> = self
            .map
            .lock()
            .unwrap()
            .iter()
            .map(|(p, (s, h))| (p.clone(), s.clone(), h.clone()))
            .collect();
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(&v)?)?;
        fs::rename(tmp, path)
    }

    pub fn get(&self, p: &Path) -> io::Result<String> {
        let md = fs::metadata(p)?;
        let stamp = Stamp::of(&md);
        if let Some((s, h)) = self.map.lock().unwrap().get(p)
            && *s == stamp
        {
            return Ok(h.clone());
        }
        // Shared across processes: the hash rides on the file as an xattr,
        // valid while size, mtime and inode match.
        let tag = self.may_tag(p);
        let h = match xattr_get(p).filter(|(s, _)| tag && *s == stamp) {
            Some((_, h)) => h,
            None => {
                let h = hash_file(p)?;
                if tag {
                    xattr_set(p, &stamp, &h);
                }
                h
            }
        };
        self.map
            .lock()
            .unwrap()
            .insert(p.to_path_buf(), (stamp, h.clone()));
        Ok(h)
    }

    /// Record a hash we already know (a file we just wrote).
    pub fn seed(&self, p: &Path, hash: &str) {
        if let Ok(md) = fs::metadata(p) {
            let stamp = Stamp::of(&md);
            if self.may_tag(p) {
                xattr_set(p, &stamp, hash);
            }
            self.map
                .lock()
                .unwrap()
                .insert(p.to_path_buf(), (stamp, hash.to_string()));
        }
    }
}

const XATTR: &[u8] = b"dev.oakoil.b3\0";

fn cpath(p: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).ok()
}

fn xattr_get(p: &Path) -> Option<(Stamp, String)> {
    let c = cpath(p)?;
    let mut buf = [0u8; 256];
    // SAFETY: valid NUL-terminated path and name, buffer of the given size.
    let n = unsafe {
        libc::getxattr(
            c.as_ptr(),
            XATTR.as_ptr().cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if n <= 0 {
        return None;
    }
    let text = std::str::from_utf8(&buf[..n as usize]).ok()?;
    let mut it = text.split(':');
    let stamp = Stamp {
        len: it.next()?.parse().ok()?,
        mtime: it.next()?.parse().ok()?,
        mtime_ns: it.next()?.parse().ok()?,
        ino: it.next()?.parse().ok()?,
    };
    Some((stamp, it.next()?.to_string()))
}

fn xattr_set(p: &Path, s: &Stamp, h: &str) {
    let Some(c) = cpath(p) else { return };
    let v = format!("{}:{}:{}:{}:{h}", s.len, s.mtime, s.mtime_ns, s.ino);
    // SAFETY: valid NUL-terminated path and name; value pointer and length match.
    // Setting an xattr does not change mtime; failures (read-only files) are ignored.
    unsafe {
        libc::setxattr(
            c.as_ptr(),
            XATTR.as_ptr().cast(),
            v.as_ptr().cast(),
            v.len(),
            0,
            libc::XATTR_NOFOLLOW,
        );
    }
}
