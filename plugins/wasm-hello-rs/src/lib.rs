use terminism_wasm_sdk as sdk;

sdk::terminism_plugin! {
    init: { sdk::log("plugin loaded"); },
    on_event: |event, payload| {
        sdk::log(&format!("{event} {payload}"));
        if event == "probe.memory" {
            sdk::data_set("echo", payload);
            let _ = sdk::data_get("echo");
            sdk::config_set("wasm-hello-rs", "last-event", "true");
            let _ = sdk::config_get("wasm-hello-rs", "last-event");
        }
    },
}
