//! The debug-info object files a unit leaves beside its artifacts.
//!
//! With `split-debuginfo = "unpacked"` (Cargo's macOS default) linked
//! binaries keep no DWARF; their debug map (`N_OSO` stabs) names `.o` files
//! in `deps/`. Those objects belong to the unit that made them: an rlib's
//! members, or a linked unit's own codegen units. Incremental builds leave
//! stale objects with the same name prefix, so the set is read from the
//! artifacts themselves, not guessed from file names.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Member names of an `ar` archive (rlib), BSD or GNU layout.
pub fn ar_members(path: &Path) -> io::Result<Vec<String>> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != b"!<arch>\n" {
        return Ok(vec![]);
    }
    let len = f.metadata()?.len();
    let mut pos = 8u64;
    let mut names = Vec::new();
    let mut gnu_table: Vec<u8> = Vec::new();
    while pos + 60 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 60];
        f.read_exact(&mut h)?;
        let raw = String::from_utf8_lossy(&h[..16]).trim_end().to_string();
        let size: u64 = String::from_utf8_lossy(&h[48..58])
            .trim()
            .parse()
            .unwrap_or(0);
        let data = pos + 60;
        let name = if let Some(n) = raw.strip_prefix("#1/") {
            // BSD long name: stored at the start of the member data
            let n: usize = n.parse().unwrap_or(0);
            let mut b = vec![0u8; n];
            f.read_exact(&mut b)?;
            String::from_utf8_lossy(&b)
                .trim_end_matches('\0')
                .to_string()
        } else if raw == "//" {
            gnu_table = vec![0u8; size as usize];
            f.read_exact(&mut gnu_table)?;
            String::new()
        } else if let Some(off) = raw.strip_prefix('/').and_then(|o| o.parse::<usize>().ok()) {
            let rest = gnu_table.get(off..).unwrap_or_default();
            let end = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
            String::from_utf8_lossy(&rest[..end])
                .trim_end_matches('/')
                .to_string()
        } else {
            raw.trim_end_matches('/').to_string()
        };
        if !name.is_empty() && name != "/" && !name.starts_with("__.SYMDEF") {
            names.push(name);
        }
        pos = data + size + (size & 1);
    }
    Ok(names)
}

/// Object files a linked Mach-O binary's debug map points to (`nm -ap`,
/// `OSO` entries). `None` when the map cannot be read.
#[cfg(target_os = "linux")]
pub fn debug_map_objects(path: &Path) -> Option<Vec<PathBuf>> {
    // ELF builds keep debug info inside the binary (Cargo's default
    // split-debuginfo=off on Linux): no object files are left beside it.
    // With packed/unpacked split debuginfo, fall back to the mtime scan.
    let _ = path;
    let split = std::env::var("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO").is_ok()
        || std::env::var("CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO").is_ok();
    (!split).then(Vec::new)
}

#[cfg(target_os = "macos")]
pub fn debug_map_objects(path: &Path) -> Option<Vec<PathBuf>> {
    let out = std::process::Command::new("nm")
        .arg("-ap")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .filter_map(|l| l.split_once(" OSO ").map(|(_, p)| p.trim()))
            .filter(|p| !p.ends_with(')')) // `lib.rlib(member.o)`: inside an archive
            .map(PathBuf::from)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bsd_archive() {
        let dir = std::env::temp_dir().join(format!("oil-ar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long = "very_long_object_name-0123456789abcdef.cgu.0.rcgu.o";
        let mut a = b"!<arch>\n".to_vec();
        let body = b"hello";
        let size = long.len() + body.len();
        a.extend(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("#1/{}", long.len()),
                0,
                0,
                0,
                644,
                size
            )
            .as_bytes(),
        );
        a.extend(long.as_bytes());
        a.extend(body);
        if size % 2 == 1 {
            a.push(b'\n');
        }
        a.extend(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "lib.rmeta", 0, 0, 0, 644, 2
            )
            .as_bytes(),
        );
        a.extend(b"xy");
        let p = dir.join("t.rlib");
        std::fs::write(&p, a).unwrap();
        assert_eq!(
            ar_members(&p).unwrap(),
            vec![long.to_string(), "lib.rmeta".to_string()]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
