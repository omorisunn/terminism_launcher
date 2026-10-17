use super::manifest;
use std::collections::HashSet;
use std::path::PathBuf;
use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

pub struct Loader {
    pub plugins_dir: PathBuf,
    pub(crate) seen: HashSet<PathBuf>,
}

impl Loader {
    fn scan(&mut self, host: &mut HostHandle, logger: &Logger) {
        let Some(anchor) = host.lineage_logger("logger") else {
            return;
        };
        let entries = match std::fs::read_dir(&self.plugins_dir) {
            Ok(entries) => entries,
            Err(e) => {
                logger.log(&format!("scan failed: {e}"));
                return;
            }
        };
        let mut dirs: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            if !self.seen.insert(dir.clone()) {
                continue;
            }
            if let Err(e) = host.load_plugin(&dir, &anchor) {
                logger.log(&format!("load failed: {e}"));
            }
        }
    }
}
impl NativePlugin for Loader {
    fn manifest(&self) -> Manifest {
        manifest("loader", "Loader", &[], "Loads Rhai and Wasm plugins")
    }
    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        self.scan(host, logger);
    }
    fn on_tick(&mut self, host: &mut HostHandle, logger: &Logger) {
        self.scan(host, logger);
    }
    fn on_event(&mut self, _: &str, _: &Json, _: &mut HostHandle, _: &Logger) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap, rc::Rc};
    use terminism_kernel::host::{ConfigStore, DataStore, PluginHost, PluginRegistry};
    use terminism_kernel::logger::LogStore;

    #[test]
    fn registered_directories_are_not_parsed_again() {
        let dir = std::env::temp_dir().join(format!("terminism-loader-{}", std::process::id()));
        let plugin_dir = dir.join("probe");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("manifest.toml"), "id = \"scan-probe\"\n").unwrap();
        std::fs::write(
            plugin_dir.join("main.rhai"),
            "fn init() {} fn on_event(n,d) {}",
        )
        .unwrap();
        let sink = LogStore::default();
        let config = Rc::new(RefCell::new(ConfigStore::load(&dir.join("config.toml")).0));
        let mut host = PluginHost::new(
            sink.clone(),
            Rc::new(RefCell::new(DataStore(HashMap::new()))),
            config,
            Rc::new(RefCell::new(PluginRegistry::default())),
        );
        host.register_native(Box::new(super::super::Init));
        host.register_native(Box::new(super::super::Manager));
        host.register_native(Box::new(super::super::LoggerPlugin {
            log_store: sink.clone(),
        }));
        host.register_native(Box::new(Loader {
            plugins_dir: dir,
            seen: Default::default(),
        }));
        host.boot().unwrap();
        assert!(host.plugin_infos().iter().any(|p| p.id == "scan-probe"));
        std::fs::write(plugin_dir.join("manifest.toml"), "invalid").unwrap();
        host.tick();
        assert!(
            !sink
                .borrow()
                .iter()
                .any(|line| line.contains("load failed"))
        );
    }
}
