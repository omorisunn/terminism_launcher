<img width="256" height="256" alt="icon-256" src="https://github.com/user-attachments/assets/80b77b85-1a9a-40b3-bc10-104f4734a716" />

# Terminism Launcher

A terminal-UI Minecraft: Java Edition launcher written in Rust, built around a plugin host.

## Features

- TUIs
- Native, Rhai and Wasm plugins
- Forge, NeoForge, Fabric and LiteLoader profiles are detected and launched

## Requirements

- Windows 10/11 with the MSVC toolchain
- Rust stable

## Build and run

```text
cargo xtask all   # fmt, clippy, Wasm example build, tests
cargo xtask run   # install plugins/theme/i18n and start the launcher
```

## Workspace

| Crate | Responsibility |
| --- | --- |
| `terminism-kernel` | plugin metadata, runtimes, lifecycle, events, config, logging |
| `terminism-systemui` | UI tree, input, layout, rendering, built-in pages, themes |
| `terminism-shell` | terminal driver, boot chain, native services |
| `terminism-wasm-sdk` | guest SDK for Wasm plugins |
| `xtask` | build, plugin check, install and dist commands |

See [the Wasm SDK guide](crates/terminism-wasm-sdk/README.md) for plugin development.

## License

GPL-3.0. See [LICENSE](LICENSE).
