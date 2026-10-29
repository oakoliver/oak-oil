//! A replayed plan must see the current value of every variable a crate
//! reads with `env!`/`option_env!` (rustc's `# env-dep:` lines) and every
//! variable a build script names in `cargo:rerun-if-env-changed`, as stock
//! Cargo does: a change re-plans, and the binary embeds the new value.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VAR: &str = "OIL_TEST_ENV_DEP";
const BS_VAR: &str = "OIL_TEST_BUILD_SCRIPT_ENV";

fn fixture(root: &Path) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"envdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{\n    \
             println!(\"cargo:rerun-if-changed=build.rs\");\n    \
             println!(\"cargo:rerun-if-env-changed={BS_VAR}\");\n    \
             let v = std::env::var(\"{BS_VAR}\").unwrap_or_else(|_| \"bs-unset\".into());\n    \
             println!(\"cargo:rustc-env=ENVDEP_FROM_BUILD={{v}}\");\n}}\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        format!(
            "fn main() {{\n    \
             println!(\"{{}} {{}}\", option_env!(\"{VAR}\").unwrap_or(\"unset\"), env!(\"ENVDEP_FROM_BUILD\"));\n}}\n"
        ),
    )
    .unwrap();
    // A second binary that fails to compile when the variable is "fail",
    // after (or while) the first one compiled with that value.
    fs::create_dir_all(root.join("src/bin")).unwrap();
    fs::write(
        root.join("src/bin/gate.rs"),
        format!(
            "const fn is_fail(v: Option<&str>) -> bool {{\n    \
             match v {{\n        \
             Some(s) => matches!(s.as_bytes(), [b'f', b'a', b'i', b'l']),\n        \
             None => false,\n    }}\n}}\n\
             const _: () = assert!(!is_fail(option_env!(\"{VAR}\")), \"fail requested\");\n\
             fn main() {{}}\n"
        ),
    )
    .unwrap();
}

struct Ws {
    /// Store writes now (`OAKOIL_SYNC_STORE`), or in the background as
    /// users run it, racing the next build.
    sync: bool,
    dir: PathBuf,
    target: PathBuf,
    store: PathBuf,
}

impl Ws {
    fn new(name: &str) -> Ws {
        Ws::with_store_writes(name, true)
    }

    fn with_store_writes(name: &str, sync: bool) -> Ws {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = fs::remove_dir_all(&base);
        let ws = Ws {
            sync,
            dir: base.join("ws"),
            target: base.join("target"),
            store: base.join("store"),
        };
        fixture(&ws.dir);
        ws
    }

    fn cmd(&self, program: &str, var: Option<&str>, bs_var: Option<&str>) -> Command {
        let mut c = Command::new(program);
        c.current_dir(&self.dir)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("OAKOIL_HOME", &self.store)
            .env("CARGO_INCREMENTAL", "0")
            .env_remove(VAR)
            .env_remove(BS_VAR);
        if self.sync {
            c.env("OAKOIL_SYNC_STORE", "1");
        } else {
            c.env_remove("OAKOIL_SYNC_STORE");
        }
        if let Some(v) = var {
            c.env(VAR, v);
        }
        if let Some(v) = bs_var {
            c.env(BS_VAR, v);
        }
        c
    }

    fn oil_build(&self, var: Option<&str>, bs_var: Option<&str>) -> std::process::Output {
        self.cmd(env!("CARGO_BIN_EXE_cargo-oil"), var, bs_var)
            .args(["build", "--offline"])
            .output()
            .unwrap()
    }

    /// `cargo oil build`, then the built binary's output; and oil's stderr.
    fn build(&self, var: Option<&str>, bs_var: Option<&str>) -> (String, String) {
        let out = self.oil_build(var, bs_var);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "cargo oil build failed:\n{stderr}");
        // The binary runs with neither variable set: what it prints was
        // embedded at compile time.
        let run = Command::new(self.target.join("debug").join("envdep"))
            .env_remove(VAR)
            .env_remove(BS_VAR)
            .output()
            .unwrap();
        assert!(run.status.success());
        (
            String::from_utf8_lossy(&run.stdout)
                .trim_end_matches('\n')
                .to_string(),
            stderr,
        )
    }
}

fn env_dep_steps(ws: &Ws) {
    let steps: &[(Option<&str>, &str)] = &[
        (Some("aaaa"), "aaaa"), // recorded with the variable set
        (Some("aaaa"), "aaaa"), // replayed, unchanged
        (Some("bbbb"), "bbbb"), // set -> other value
        (Some("aaaa"), "aaaa"), // back: a store hit must not be the bbbb object
        (None, "unset"),        // set -> unset
        (Some("cccc"), "cccc"), // unset -> set
        (Some(""), ""),         // set but empty is not unset
        (None, "unset"),
    ];
    for (i, (var, want)) in steps.iter().enumerate() {
        let (got, stderr) = ws.build(*var, None);
        assert_eq!(
            got,
            format!("{want} bs-unset"),
            "step {i}: {VAR}={var:?}\n{stderr}"
        );
    }
}

#[test]
fn env_dep_change_rebuilds() {
    env_dep_steps(&Ws::new("env_dep_change_rebuilds"));
}

/// The previous build's background store writes race the rebuild of the
/// same unit: they must store that build's bytes or nothing.
#[test]
fn env_dep_change_with_background_store_writes() {
    let ws = Ws::with_store_writes("env_dep_change_with_background_store_writes", false);
    env_dep_steps(&ws);
    env_dep_steps(&ws); // again, now served from what the drains stored
    let out = ws
        .cmd(env!("CARGO_BIN_EXE_cargo-oil"), None, None)
        .arg("wait-store")
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn unchanged_env_replays_without_work() {
    let ws = Ws::new("unchanged_env_replays_without_work");
    ws.build(Some("same"), Some("same-bs"));
    let (got, stderr) = ws.build(Some("same"), Some("same-bs"));
    assert_eq!(got, "same same-bs");
    assert!(
        stderr.contains("0 from store, 0 compiled") && !stderr.contains("re-planning"),
        "no-change build did work:\n{stderr}"
    );
}

#[test]
fn build_script_rerun_if_env_changed() {
    let ws = Ws::new("build_script_rerun_if_env_changed");
    let steps: &[(Option<&str>, &str)] = &[
        (Some("one"), "one"),
        (Some("one"), "one"),
        (Some("two"), "two"),
        (None, "bs-unset"),
        (Some("one"), "one"),
    ];
    for (i, (bs, want)) in steps.iter().enumerate() {
        let (got, stderr) = ws.build(Some("x"), *bs);
        assert_eq!(
            got,
            format!("x {want}"),
            "step {i}: {BS_VAR}={bs:?}\n{stderr}"
        );
    }
}

/// A re-record that fails after rebuilding some units under a new value must
/// not leave the previous plan to replay those units' new records.
#[test]
fn failed_rerecord_leaves_no_stale_plan() {
    let ws = Ws::new("failed_rerecord_leaves_no_stale_plan");
    assert_eq!(ws.build(Some("aaaa"), None).0, "aaaa bs-unset");
    let out = ws.oil_build(Some("fail"), None);
    assert!(!out.status.success(), "the gate binary should not compile");
    let (got, stderr) = ws.build(Some("aaaa"), None);
    assert_eq!(got, "aaaa bs-unset", "{stderr}");
}
