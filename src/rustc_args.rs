//! What Oak Oil needs to know about one rustc invocation, read from the
//! command line Cargo built for it.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Invocation {
    pub crate_name: String,
    /// `-C extra-filename`, e.g. `-8d5a75ee21fc7ed2`.
    pub extra: String,
    pub out_dir: PathBuf,
    /// `--extern name=path` paths, in order.
    pub externs: Vec<PathBuf>,
    /// `-L native=…` / `-L all=…` / bare `-L` dirs.
    pub native_dirs: Vec<PathBuf>,
    pub crate_types: Vec<String>,
    pub test: bool,
    pub emit: Vec<String>,
}

impl Invocation {
    /// `None` for anything that is not a cacheable compile: version probes,
    /// `--print` queries, stdin input, missing out dir.
    pub fn parse(args: &[String]) -> Option<Invocation> {
        let mut crate_name = None;
        let mut extra = String::new();
        let mut out_dir = None;
        let mut externs = Vec::new();
        let mut native_dirs = Vec::new();
        let mut crate_types = Vec::new();
        let mut test = false;
        let mut emit = Vec::new();

        let mut i = 0;
        // `--flag value` or `--flag=value`
        let take = |i: &mut usize, flag: &str| -> Option<String> {
            let a = &args[*i];
            if a == flag {
                *i += 1;
                args.get(*i).cloned()
            } else {
                a.strip_prefix(flag)
                    .and_then(|r| r.strip_prefix('='))
                    .map(str::to_string)
            }
        };
        while i < args.len() {
            let a = args[i].as_str();
            if a == "-" || a.starts_with("--print") || a == "-vV" || a == "-V" || a == "--version" {
                return None;
            }
            if a == "--test" {
                test = true;
            } else if a == "--crate-name" || a.starts_with("--crate-name=") {
                crate_name = take(&mut i, "--crate-name");
            } else if a == "--crate-type" || a.starts_with("--crate-type=") {
                if let Some(v) = take(&mut i, "--crate-type") {
                    crate_types.extend(v.split(',').map(str::to_string));
                }
            } else if a == "--out-dir" || a.starts_with("--out-dir=") {
                out_dir = take(&mut i, "--out-dir").map(PathBuf::from);
            } else if a == "--emit" || a.starts_with("--emit=") {
                if let Some(v) = take(&mut i, "--emit") {
                    emit.extend(
                        v.split(',')
                            .map(|e| e.split('=').next().unwrap_or(e).to_string()),
                    );
                }
            } else if a == "--extern" {
                i += 1;
                if let Some((_, p)) = args.get(i).and_then(|v| v.split_once('=')) {
                    externs.push(PathBuf::from(p));
                }
            } else if a == "-C" || (a.starts_with("-C") && a.len() > 2) {
                let v = if a == "-C" {
                    i += 1;
                    args.get(i).cloned().unwrap_or_default()
                } else {
                    a[2..].to_string()
                };
                if let Some(e) = v.strip_prefix("extra-filename=") {
                    extra = e.to_string();
                }
            } else if a == "-L" || (a.starts_with("-L") && a.len() > 2) {
                let v = if a == "-L" {
                    i += 1;
                    args.get(i).cloned().unwrap_or_default()
                } else {
                    a[2..].to_string()
                };
                match v.split_once('=') {
                    Some(("dependency" | "crate", _)) => {}
                    Some((_, p)) => native_dirs.push(PathBuf::from(p)),
                    None => native_dirs.push(PathBuf::from(v)),
                }
            }
            i += 1;
        }
        let crate_name = crate_name?;
        let out_dir = out_dir?;
        if emit.is_empty() {
            emit.push("link".into());
        }
        Some(Invocation {
            crate_name,
            extra,
            out_dir,
            externs,
            native_dirs,
            crate_types,
            test,
            emit,
        })
    }

    /// Final link outputs need every transitive rlib, not just the direct
    /// `--extern`s.
    pub fn links(&self) -> bool {
        self.emit.iter().any(|e| e == "link")
            && (self.test
                || self.crate_types.is_empty()
                || self.crate_types.iter().any(|t| t != "lib" && t != "rlib"))
    }

    /// Files this unit writes into `out_dir` start with one of these, then
    /// end or continue with `.`.
    pub fn output_prefixes(&self) -> [String; 2] {
        [
            format!("{}{}", self.crate_name, self.extra),
            format!("lib{}{}", self.crate_name, self.extra),
        ]
    }

    #[cfg(test)]
    pub fn is_output(&self, file_name: &str) -> bool {
        matches_prefixes(&self.output_prefixes(), file_name)
    }

    /// rustc's own dep-info for this unit.
    pub fn dep_info(&self) -> PathBuf {
        self.out_dir
            .join(format!("{}{}.d", self.crate_name, self.extra))
    }
}

/// `file_name` is one of the prefixes, or a prefix followed by `.…`.
pub fn matches_prefixes(prefixes: &[String], file_name: &str) -> bool {
    prefixes.iter().any(|p| {
        file_name == p
            || file_name
                .strip_prefix(p.as_str())
                .is_some_and(|r| r.starts_with('.'))
    })
}

/// The Cargo target dir an out dir lives in: the nearest ancestor holding the
/// `CACHEDIR.TAG` Cargo writes.
pub fn target_dir_of(out_dir: &Path) -> Option<PathBuf> {
    out_dir
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_cargo_lib_invocation() {
        let inv = Invocation::parse(&args(
            "--crate-name my_lib --edition=2021 crates/lib/src/lib.rs --error-format=json \
             --crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no \
             -C extra-filename=-a8a074f93add8f56 --out-dir /t/debug/deps \
             -L dependency=/t/debug/deps -L native=/t/debug/build/x/out \
             --extern serde=/t/debug/deps/libserde-b611c61aabb71f5f.rmeta",
        ))
        .unwrap();
        assert_eq!(inv.crate_name, "my_lib");
        assert_eq!(inv.extra, "-a8a074f93add8f56");
        assert_eq!(
            inv.externs,
            vec![PathBuf::from(
                "/t/debug/deps/libserde-b611c61aabb71f5f.rmeta"
            )]
        );
        assert_eq!(inv.native_dirs, vec![PathBuf::from("/t/debug/build/x/out")]);
        assert!(!inv.links());
        assert!(inv.is_output("libmy_lib-a8a074f93add8f56.rlib"));
        assert!(inv.is_output("my_lib-a8a074f93add8f56.d"));
        assert!(!inv.is_output("libmy_lib-a8a074f93add8f56x.rlib"));
    }

    #[test]
    fn bins_and_probes() {
        let bin = Invocation::parse(&args(
            "--crate-name my_app src/main.rs --crate-type bin --emit=dep-info,link \
             -C extra-filename=-2690356174c6854d --out-dir /t/debug/deps",
        ))
        .unwrap();
        assert!(bin.links());
        assert!(bin.is_output("my_app-2690356174c6854d.abc.rcgu.o"));
        assert!(Invocation::parse(&args("- --crate-name ___ --print=file-names")).is_none());
        assert!(Invocation::parse(&args("-vV")).is_none());
    }
}
