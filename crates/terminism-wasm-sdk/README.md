# terminism-wasm-sdk

Guest SDK for writing terminism plugins in Rust targeting `wasm32-unknown-unknown`.

## Usage

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
terminism-wasm-sdk = "0.0.1"
```

```rust
use terminism_wasm_sdk as sdk;

sdk::terminism_plugin! {
    init: { sdk::log("loaded"); },
    on_event: |event, payload| {
        sdk::log(&format!("{event} {payload}"));
    },
}
```

Build with `cargo build --release --target wasm32-unknown-unknown`, rename the artifact to `plugin.wasm`, and place it next to a `manifest.toml` with `kind = "wasm"`.

The workspace example lives in `plugins/wasm-hello-rs`. Run `cargo xtask build-wasm-plugins` to rebuild it; `cargo xtask all` also builds the example and runs the host tests.

## API

`log`, `publish`, `data_get`/`data_set`, `config_get`/`config_set`. Values are JSON text; missing values read as `null`.

## ABI

Required exports: `memory`, `alloc(len) -> ptr`, `dealloc(ptr, len)`, `init()`, and `on_event(name_ptr, name_len, payload_ptr, payload_len) -> i32`. Event names are UTF-8 and payloads are JSON; zero means success. A non-zero result or a trap disables the plugin.

The host owns event arguments; the SDK copies results before freeing its buffer. Defaults: 64 MiB linear memory, 1 MiB per string, and 1,000,000 fuel per call.

Plugins built against an older SDK must be rebuilt. Missing exports or mismatched signatures are rejected before initialization.
