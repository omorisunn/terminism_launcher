use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

pub(crate) fn build() -> Result<(), Error> {
    build_wasm_plugins()?;
    cargo(&["build", "--workspace", "--all-targets"])
}

pub(crate) fn test() -> Result<(), Error> {
    build_wasm_plugins()?;
    cargo(&["test", "--workspace"])
}

pub(crate) fn check() -> Result<(), Error> {
    cargo(&["fmt", "--all", "--", "--check"])?;
    cargo(&[
        "fmt",
        "--manifest-path",
        "plugins/wasm-hello-rs/Cargo.toml",
        "--",
        "--check",
    ])?;
    cargo(&[
        "clippy",
        "--manifest-path",
        "plugins/wasm-hello-rs/Cargo.toml",
        "--target",
        "wasm32-unknown-unknown",
        "--target-dir",
        "target",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])
}

pub(crate) fn all() -> Result<(), Error> {
    check()?;
    test()
}

pub(crate) fn build_wasm_plugins() -> Result<(), Error> {
    let root = workspace_root();
    let flags = format!(
        "build.rustflags={}",
        serde_json::json!([format!("--remap-path-prefix={}=/terminism", root.display())])
    );
    cargo(&[
        "build",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
        "--manifest-path",
        "plugins/wasm-hello-rs/Cargo.toml",
        "--target-dir",
        "target",
        "--config",
        &flags,
    ])?;
    fs::copy(
        root.join("target/wasm32-unknown-unknown/release/wasm_hello_rs.wasm"),
        root.join("plugins/wasm-hello-rs/plugin.wasm"),
    )?;
    Ok(())
}

pub(crate) fn install_plugins(release: bool) -> Result<(), Error> {
    build_wasm_plugins()?;
    let plugins = workspace_root().join("plugins");
    let target = target_dir(release).join(".terminism").join("plugins");
    for entry in fs::read_dir(&plugins)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        copy_dir(&entry.path(), &target.join(entry.file_name()))?;
    }
    install_theme(&target_dir(release).join(".terminism").join("theme"))?;
    install_i18n(&target_dir(release).join(".terminism").join("i18n"))
}

fn install_i18n(target: &std::path::Path) -> Result<(), Error> {
    let source = workspace_root().join("i18n");
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "json") {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn install_theme(target: &std::path::Path) -> Result<(), Error> {
    let source = workspace_root().join("theme");
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "tmss") {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub(crate) fn run(release: bool) -> Result<(), Error> {
    install_plugins(release)?;
    if release {
        cargo(&["run", "-p", "terminism-shell", "--release"])
    } else {
        cargo(&["run", "-p", "terminism-shell"])
    }
}

// scaffolds a wasm plugin project that depends on the workspace SDK: no
// template directory, the skeleton is generated from strings here
pub(crate) fn new_wasm_plugin(target: &std::path::Path) -> Result<(), Error> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| Error::Io(std::io::Error::other("target needs a directory name")))?;
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(Error::Io(std::io::Error::other(
            "plugin id must be lowercase letters, digits and hyphens",
        )));
    }
    // TOML has no raw strings; forward slashes keep Windows paths valid
    let sdk = workspace_root()
        .join("crates")
        .join("terminism-wasm-sdk")
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR_STR, "/");
    fs::create_dir_all(target.join("src"))?;
    fs::create_dir_all(target.join(".cargo"))?;
    fs::write(
        target.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
terminism-wasm-sdk = {{ path = "{sdk}" }}

# standalone project, even when scaffolded inside another workspace
[workspace]
"#
        ),
    )?;
    fs::write(
        target.join("manifest.toml"),
        format!(
            r#"id = "{name}"
name = "{name}"
version = "0.1.0"
description = "Describe the plugin"
kind = "wasm"
provides = []
subscribe = ["plugin.loaded"]
ui_grant = []
data_grant = []
cli_grant = []
"#
        ),
    )?;
    fs::write(
        target.join("src").join("lib.rs"),
        r#"use terminism_wasm_sdk as sdk;

sdk::terminism_plugin! {
    init: {
        sdk::log("plugin loaded");
    },
    on_event: |event, payload| {
        sdk::log(&format!("{event} {payload}"));
    },
}
"#,
    )?;
    fs::write(
        target.join(".cargo").join("config.toml"),
        "[build]
target = \"wasm32-unknown-unknown\"
",
    )?;
    println!("created {}", target.display());
    println!("next:");
    println!("  cd {}", target.display());
    println!("  cargo build --release");
    let artifact = name.replace('-', "_");
    println!("  copy target/wasm32-unknown-unknown/release/{artifact}.wasm plugin.wasm");
    Ok(())
}

// writes the rhai API definition file next to the plugins directory
pub(crate) fn gen_rhai_defs() -> Result<(), Error> {
    // the Rhai language server reads workspace .rhai/definitions by default
    let dir = workspace_root().join(".rhai").join("definitions");
    fs::create_dir_all(&dir)?;
    let path = dir.join("terminism.d.rhai");
    terminism_kernel::host::write_rhai_definitions(&path)?;
    let lines = fs::read_to_string(&path).map(|t| t.lines().count())?;
    println!("{}: {lines} lines", path.display());
    Ok(())
}

// spawns the command in a separate Windows Terminal; cmd /k keeps the window open
pub(crate) fn wt(args: &[String]) -> Result<(), Error> {
    let root = workspace_root();
    let cmdline = format!("cargo xtask {}", args.join(" "));
    Command::new("wt")
        .args(["-d", &root.display().to_string(), "cmd", "/k", &cmdline])
        .spawn()?;
    Ok(())
}

pub(crate) fn dist() -> Result<(), Error> {
    build_wasm_plugins()?;
    cargo(&["build", "--release", "-p", "terminism-shell"])?;
    let root = workspace_root();
    let stage = root
        .join("dist")
        .join(format!("terminism-{}", env!("CARGO_PKG_VERSION")));
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir_all(stage.join(".terminism").join("i18n"))?;
    fs::write(stage.join(".terminism").join("config.toml"), "")?;
    fs::copy(
        target_dir(true).join("terminism-shell.exe"),
        stage.join("terminism-shell.exe"),
    )?;
    copy_dir(
        &root.join("plugins"),
        &stage.join(".terminism").join("plugins"),
    )?;
    install_theme(&stage.join(".terminism").join("theme"))?;
    install_i18n(&stage.join(".terminism").join("i18n"))?;
    Ok(())
}

fn target_dir(release: bool) -> PathBuf {
    workspace_root()
        .join("target")
        .join(if release { "release" } else { "debug" })
}

// CARGO_MANIFEST_DIR is crates/xtask; two levels up is the workspace root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn cargo(args: &[&str]) -> Result<(), Error> {
    // MSVC link.exe may fail to create temp lnk files in the user TEMP directory
    let scratch = workspace_root().join("target");
    let status = Command::new("cargo")
        .current_dir(workspace_root())
        .env("TMP", &scratch)
        .env("TEMP", &scratch)
        .args(args)
        .status()?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(Error::Cargo { code }),
        None => Err(Error::Cargo { code: 1 }),
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), Error> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_wasm_plugin_scaffolds_project() {
        let dir = temp_dir("scaffold").join("my-plugin");
        new_wasm_plugin(&dir).unwrap();
        for file in [
            "Cargo.toml",
            "manifest.toml",
            "src/lib.rs",
            ".cargo/config.toml",
        ] {
            assert!(dir.join(file).exists(), "missing {file}");
        }
        let manifest = fs::read_to_string(dir.join("manifest.toml")).unwrap();
        assert!(manifest.contains("id = \"my-plugin\""));
        assert!(manifest.contains("kind = \"wasm\""));
        let cargo = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("terminism-wasm-sdk"));
        assert!(cargo.contains("cdylib"));
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn new_wasm_plugin_rejects_bad_id() {
        let dir = temp_dir("scaffold-bad").join("Bad Name");
        assert!(new_wasm_plugin(&dir).is_err());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn copy_dir_roundtrip() {
        let src = temp_dir("copy-src");
        let dst = temp_dir("copy-dst");
        fs::create_dir_all(src.join("outer").join("inner")).unwrap();
        fs::write(src.join("outer").join("inner").join("leaf.txt"), "leaf").unwrap();
        fs::write(src.join("root.txt"), "root").unwrap();
        copy_dir(&src, &dst.join("copy")).unwrap();
        assert_eq!(
            fs::read_to_string(
                dst.join("copy")
                    .join("outer")
                    .join("inner")
                    .join("leaf.txt")
            )
            .unwrap(),
            "leaf"
        );
        assert_eq!(
            fs::read_to_string(dst.join("copy").join("root.txt")).unwrap(),
            "root"
        );
        fs::remove_dir_all(&src).unwrap();
        fs::remove_dir_all(&dst).unwrap();
    }
}
