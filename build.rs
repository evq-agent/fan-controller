use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    // Put `memory.x` in our output directory and tell rustc to
    // link against it.
    fs::copy("memory.x", PathBuf::from(&out_dir).join("memory.x")).unwrap();
    println!("cargo:rustc-link-search=Native={out_dir}");
    println!("cargo:rerun-if-changed=memory.x");
}
