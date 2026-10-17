use super::manifest;
use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

pub struct Manager;
impl NativePlugin for Manager {
    fn manifest(&self) -> Manifest {
        manifest("manager", "Manager", &[], "Starts launcher services")
    }
    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        for id in [
            "configuration",
            "cli",
            "i18n",
            "system-pages",
            "styles-manager",
            "account",
            "auth",
            "instances",
            "launcher",
            "ui-manager",
            "loader",
        ] {
            let _ = host.start_native(id, &logger.child(id));
        }
    }
    fn on_event(&mut self, _: &str, _: &Json, _: &mut HostHandle, _: &Logger) {}
}
