use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::{LogStore, Logger};
use terminism_kernel::manifest::Manifest;

use super::manifest;

pub struct LoggerPlugin {
    pub log_store: LogStore,
}

impl NativePlugin for LoggerPlugin {
    fn manifest(&self) -> Manifest {
        manifest(
            "logger",
            "Logger",
            &["plugin.loaded", "plugin.disabled", "plugin.crashed"],
            "Feeds the shared log buffer shown on the log page",
        )
    }

    fn init(&mut self, _host: &mut HostHandle, logger: &Logger) {
        logger.log("logger ready");
    }

    fn on_event(&mut self, name: &str, payload: &Json, _host: &mut HostHandle, _logger: &Logger) {
        let mut store = self.log_store.borrow_mut();
        store.push(format!("{name} {payload}"));
    }
}
