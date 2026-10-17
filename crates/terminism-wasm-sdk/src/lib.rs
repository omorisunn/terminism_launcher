//! Guest SDK for terminism wasm plugins.
//!
//! A plugin crate depends on this SDK, calls [`terminism_plugin!`] once and
//! fills in the two callbacks. Host capabilities are reached through the
//! `sdk::*` helpers; everything crosses the boundary as UTF-8 strings.
//!
//! On non-wasm targets every helper is an inert stub so the crate still builds
//! in the workspace and can be unit-tested.

pub use imp::{config_get, config_set, data_get, data_set, log, publish};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::quote;
    use std::alloc::{Layout, alloc as raw_alloc, dealloc as raw_dealloc};

    // import names are fixed by the host ABI; Rust-side names avoid clashing
    // with the safe wrappers below
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        #[link_name = "log"]
        fn host_log(ptr: *const u8, len: usize);
        #[link_name = "publish"]
        fn host_publish(ptr: *const u8, len: usize);
        #[link_name = "data_set"]
        fn host_data_set(ptr: *const u8, len: usize);
        #[link_name = "data_get"]
        fn host_data_get(ptr: *const u8, len: usize) -> u64;
        #[link_name = "config_get"]
        fn host_config_get(ptr: *const u8, len: usize) -> u64;
        #[link_name = "config_set"]
        fn host_config_set(ptr: *const u8, len: usize);
    }

    /// Writes a line through the host logger.
    pub fn log(text: &str) {
        unsafe { host_log(text.as_ptr(), text.len()) }
    }

    /// Publishes an event; `payload` is raw JSON.
    pub fn publish(name: &str, payload: &str) {
        let json = format!(r#"{{"name":{},"payload":{}}}"#, quote(name), payload);
        unsafe { host_publish(json.as_ptr(), json.len()) }
    }

    /// Sets one key in the plugin's own data region.
    pub fn data_set(key: &str, value_json: &str) {
        let json = format!(r#"{{"key":{},"value":{}}}"#, quote(key), value_json);
        unsafe { host_data_set(json.as_ptr(), json.len()) }
    }

    /// Reads one key from the plugin's own data region; empty when unset.
    pub fn data_get(key: &str) -> String {
        let json = format!(r#"{{"key":{}}}"#, quote(key));
        read_back(unsafe { host_data_get(json.as_ptr(), json.len()) })
    }

    /// Reads a config value; empty when absent.
    pub fn config_get(section: &str, key: &str) -> String {
        let json = format!(r#"{{"section":{},"key":{}}}"#, quote(section), quote(key));
        read_back(unsafe { host_config_get(json.as_ptr(), json.len()) })
    }

    /// Writes a config value through the host config store.
    pub fn config_set(section: &str, key: &str, value_json: &str) {
        let json = format!(
            r#"{{"section":{},"key":{},"value":{}}}"#,
            quote(section),
            quote(key),
            value_json
        );
        unsafe { host_config_set(json.as_ptr(), json.len()) }
    }

    // the host packs (ptr<<32)|len for returned strings; 0 means empty
    fn read_back(packed: u64) -> String {
        if packed == 0 {
            return String::new();
        }
        let len = (packed & 0xffff_ffff) as usize;
        let ptr = (packed >> 32) as usize as *const u8;
        // SAFETY: the host wrote len bytes at ptr through our allocator
        let text =
            unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).to_string() };
        unsafe {
            dealloc(ptr as *mut u8, len);
        }
        text
    }

    /// Allocates a buffer released through `dealloc`.
    #[unsafe(no_mangle)]
    pub extern "C" fn alloc(len: usize) -> *mut u8 {
        let layout = Layout::from_size_align(len.max(1), 8).expect("valid layout");
        let ptr = unsafe { raw_alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr
    }

    /// # Safety
    /// `ptr` and `len` must match a live allocation from `alloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
        let layout = Layout::from_size_align(len.max(1), 8).expect("valid layout");
        unsafe {
            raw_dealloc(ptr, layout);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn log(_text: &str) {}
    pub fn publish(_name: &str, _payload: &str) {}
    pub fn data_set(_key: &str, _value_json: &str) {}
    pub fn data_get(_key: &str) -> String {
        String::new()
    }
    pub fn config_get(_section: &str, _key: &str) -> String {
        String::new()
    }
    pub fn config_set(_section: &str, _key: &str, _value_json: &str) {}
}

/// Declares the plugin entry points:
///
/// ```ignore
/// terminism_plugin! {
///     init: { sdk::log("loaded"); },
///     on_event: |event, payload| {
///         sdk::log(&format!("{event} {payload}"));
///     },
/// }
/// ```
#[macro_export]
macro_rules! terminism_plugin {
    (
        init: $init:expr,
        on_event: |$event:ident, $payload:ident| $body:block $(,)?
    ) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn init() {
            $init
        }

        /// # Safety
        /// The host must provide two valid UTF-8 buffers for this call.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_event(
            name_ptr: *const u8,
            name_len: usize,
            data_ptr: *const u8,
            data_len: usize,
        ) -> i32 {
            let name = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
            let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
            let (Ok($event), Ok($payload)) = (std::str::from_utf8(name), std::str::from_utf8(data))
            else {
                return 1;
            };
            let _ = $body;
            0
        }
    };
}

#[cfg(any(target_arch = "wasm32", test))]
fn quote(text: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape_control_characters() {
        assert_eq!(quote("a\n\t\"\\"), "\"a\\u000a\\u0009\\\"\\\\\"");
    }

    #[test]
    fn host_stubs_are_inert() {
        log("x");
        publish("e", "{}");
        data_set("k", "1");
        assert_eq!(data_get("k"), "");
        assert_eq!(config_get("s", "k"), "");
        config_set("s", "k", "1");
    }
}
