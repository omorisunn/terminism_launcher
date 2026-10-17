use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

pub struct Init;

impl NativePlugin for Init {
    fn manifest(&self) -> Manifest {
        manifest(
            "init",
            "Init",
            &["plugin.crashed"],
            "Starts the service tree and reports plugin crashes",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let _ = host.start_native("logger", &logger.child("logger"));
        let _ = host.start_native("manager", &logger.child("manager"));
    }

    fn on_event(&mut self, name: &str, payload: &Json, _host: &mut HostHandle, logger: &Logger) {
        if name != "plugin.crashed" {
            return;
        }
        let id = payload.get("id").and_then(Json::as_str).unwrap_or("?");
        let error = payload.get("error").and_then(Json::as_str).unwrap_or("?");
        logger.log(&format!("crash: {id} {error}"));
    }
}
