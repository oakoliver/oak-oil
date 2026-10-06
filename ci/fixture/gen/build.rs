use std::{env, fs, path::Path};

fn main() {
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("generated.rs");
    fs::write(out, "pub const ANSWER: u32 = 42;\n").unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
