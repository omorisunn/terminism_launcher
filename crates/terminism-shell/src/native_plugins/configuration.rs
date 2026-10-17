use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

pub struct Configuration;

impl NativePlugin for Configuration {
    fn manifest(&self) -> Manifest {
        manifest(
            "configuration",
            "Configuration",
            &["config.set"],
            "Event gateway for the config store",
        )
    }

    fn init(&mut self, _host: &mut HostHandle, logger: &Logger) {
        logger.log("configuration ready");
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        if name != "config.set" {
            return;
        }
        let (Some(section), Some(key), Some(value)) = (
            payload.get("section").and_then(Json::as_str),
            payload.get("key").and_then(Json::as_str),
            payload.get("value"),
        ) else {
            logger.log("config.set rejected: section, key and value are required");
            return;
        };
        host.config().borrow_mut().set(section, key, value.clone());
    }
}
