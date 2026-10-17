use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/native_plugins/privacy.rs");
    println!("cargo:rerun-if-changed=src/native_plugins/privacy.example.rs");
    let local = Path::new("src/native_plugins/privacy.rs");
    let example = Path::new("src/native_plugins/privacy.example.rs");
    let source = fs::read_to_string(local).or_else(|_| fs::read_to_string(example));
    let source =
        source.unwrap_or_else(|_| String::from("pub(crate) const AZURE_CLIENT_ID: &str = \"\";\n"));
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("privacy.rs");
    fs::write(out, source).expect("write privacy.rs");
}
