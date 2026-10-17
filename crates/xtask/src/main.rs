mod check;
mod tasks;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("cargo exited with {code}")]
    Cargo { code: i32 },
    #[error("{0}")]
    Manifest(#[from] terminism_kernel::manifest::ManifestError),
    #[error("not a directory: {}", .0.display())]
    NotADirectory(PathBuf),
    #[error(transparent)]
    Plugin(#[from] terminism_kernel::plugin::PluginError),
}

const HELP: &str = "\
usage: cargo xtask <command>

commands:
  build                     cargo build --workspace --all-targets
  build-wasm-plugins        build and stage the Rust wasm example
  test                      cargo test --workspace
  check                     cargo fmt --check, then cargo clippy -D warnings
  all                       check, then test
  install-plugins [--release]
  run [--release]           install-plugins, then cargo run -p terminism-shell
  dist                      build release and stage dist/terminism-<version>/
  gen-rhai-defs             write plugins/terminism.d.rhai for editor completion
  new-wasm-plugin <dir>     scaffold a wasm plugin project in <dir>
  wt <command>              run 'cargo xtask <command>' in a new Windows Terminal
  plugin check <dir>        validate a plugin directory
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        return help_exit();
    };
    let release = rest.iter().any(|arg| arg == "--release");
    let result = match cmd.as_str() {
        "build" => tasks::build(),
        "build-wasm-plugins" => tasks::build_wasm_plugins(),
        "test" => tasks::test(),
        "check" => tasks::check(),
        "all" => tasks::all(),
        "install-plugins" => tasks::install_plugins(release),
        "run" => tasks::run(release),
        "dist" => tasks::dist(),
        "gen-rhai-defs" => tasks::gen_rhai_defs(),
        "new-wasm-plugin" => match rest {
            [dir] => tasks::new_wasm_plugin(Path::new(dir)),
            _ => return help_exit(),
        },
        "wt" => {
            if rest.is_empty() {
                return help_exit();
            }
            tasks::wt(rest)
        }
        "plugin" => match rest.split_first() {
            Some((sub, [dir])) if sub == "check" => {
                check::run(Path::new(dir)).map(|id| println!("ok: {id}"))
            }
            _ => return help_exit(),
        },
        _ => return help_exit(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Cargo { code }) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn help_exit() -> ExitCode {
    print!("{HELP}");
    ExitCode::from(2)
}
