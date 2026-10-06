//! rustc's `--emit=dep-info` file: every source file the compile read, and
//! every `env!`/`option_env!` variable with the value it saw.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct DepInfo {
    pub files: Vec<PathBuf>,
    /// (name, value); `None` = the variable was unset.
    pub env: Vec<(String, Option<String>)>,
}

/// Paths are made absolute against `cwd`; paths under `exclude` (the unit's
/// own out dir, i.e. its outputs) are dropped.
pub fn parse(text: &str, cwd: &Path, exclude: &Path) -> DepInfo {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut env = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            match rest.split_once('=') {
                Some((k, v)) => env.push((k.to_string(), Some(unescape_env(v)))),
                None => env.push((rest.to_string(), None)),
            }
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        for (i, tok) in split_escaped(line).into_iter().enumerate() {
            // The first token of a rule line is its target, ending in ':'.
            let tok = if i == 0 {
                tok.strip_suffix(':').map(str::to_string).unwrap_or(tok)
            } else {
                tok
            };
            if tok.is_empty() {
                continue;
            }
            let p = cwd.join(&tok);
            if !p.starts_with(exclude) && !files.contains(&p) {
                files.push(p);
            }
        }
    }
    DepInfo { files, env }
}

/// Splits on unescaped spaces; `\ ` is a space inside a path.
fn split_escaped(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            ' ' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unescape_env(v: &str) -> String {
    v.replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_dep_info() {
        let text = "/t/debug/deps/libmy_lib-a.rmeta: crates/lib/src/lib.rs crates/lib/src/my\\ file.rs /t/debug/build/x/out/gen.rs\n\
                    /t/debug/deps/my_lib-a.d: crates/lib/src/lib.rs\n\n\
                    crates/lib/src/lib.rs:\n\n\
                    # env-dep:CARGO_PKG_NAME=my-lib\n# env-dep:MISSING\n";
        let d = parse(text, Path::new("/w"), Path::new("/t/debug/deps"));
        assert_eq!(
            d.files,
            vec![
                PathBuf::from("/w/crates/lib/src/lib.rs"),
                PathBuf::from("/w/crates/lib/src/my file.rs"),
                PathBuf::from("/t/debug/build/x/out/gen.rs"),
            ]
        );
        assert_eq!(
            d.env,
            vec![
                ("CARGO_PKG_NAME".into(), Some("my-lib".into())),
                ("MISSING".into(), None)
            ]
        );
    }
}
