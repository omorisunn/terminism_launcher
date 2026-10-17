use std::collections::HashMap;
use std::path::{Path, PathBuf};

use terminism_kernel::host::{HostHandle, I18nStore, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

pub struct I18n {
    pub i18n_dir: PathBuf,
    pub plugins_dir: PathBuf,
    // the store the gateway and other natives already hold a handle to
    pub store: I18nStore,
}

impl I18n {
    // locale names offered by the catalogs on disk, sorted and deduped
    fn available_locales(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let add_dir = |dir: &Path, out: &mut Vec<String>| {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().is_some_and(|x| x == "json")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && !out.iter().any(|l| l == stem)
                {
                    out.push(stem.to_string());
                }
            }
        };
        add_dir(&self.i18n_dir, &mut out);
        if let Ok(entries) = std::fs::read_dir(&self.plugins_dir) {
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for dir in dirs {
                add_dir(&dir.join("i18n"), &mut out);
            }
        }
        out.sort();
        out
    }

    // global catalog first, then each plugin's own i18n/<locale>.json in
    // directory order: user plugins override the base strings
    fn load_catalog(&self, locale: &str, logger: &Logger) -> HashMap<String, String> {
        let mut table = read_catalog(&self.i18n_dir, locale, logger);
        let Ok(entries) = std::fs::read_dir(&self.plugins_dir) else {
            return table;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let user = read_catalog(&dir.join("i18n"), locale, logger);
            table.extend(user);
        }
        table
    }
}

fn read_catalog(dir: &Path, locale: &str, logger: &Logger) -> HashMap<String, String> {
    let path = dir.join(format!("{locale}.json"));
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            logger.log(&format!("i18n parse failed: {}", path.display()));
            logger.log(&e.to_string());
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    }
}

impl NativePlugin for I18n {
    fn manifest(&self) -> Manifest {
        manifest(
            "i18n",
            "I18n",
            &[],
            "Loads translation catalogs from .terminism/i18n and plugin i18n dirs",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let locale = host
            .config()
            .borrow()
            .get("i18n", "locale")
            .and_then(Json::as_str)
            .unwrap_or("en")
            .to_string();
        let table = self.load_catalog(&locale, logger);
        let keys = table.len();
        // fill the shared catalog in place so every holder sees the strings
        *self.store.borrow_mut() = table;
        host.set_i18n(self.store.clone());
        let locales = self.available_locales();
        logger.log(&format!(
            "i18n ready: {locale}, {keys} keys, locales {:?}",
            locales
        ));
        host.publish("i18n.locales", serde_json::json!({"locales": locales}));
    }

    fn on_event(&mut self, _name: &str, _payload: &Json, _host: &mut HostHandle, _logger: &Logger) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use terminism_kernel::logger::LogStore;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn user_catalog_overrides_global() {
        let root = std::env::temp_dir().join(format!("terminism-i18n-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let global = root.join("i18n");
        let plugin = root.join("plugins/zh-cn/i18n");
        write(&global, "zh.json", r#"{"a":"base-a","b":"base-b"}"#);
        write(&plugin, "zh.json", r#"{"b":"user-b","c":"user-c"}"#);
        let logger = Logger::new("test", LogStore::default());
        let i18n = I18n {
            i18n_dir: global,
            plugins_dir: root.join("plugins"),
            store: Rc::new(RefCell::new(HashMap::new())),
        };
        let table = i18n.load_catalog("zh", &logger);
        assert_eq!(table.get("a").map(String::as_str), Some("base-a"));
        assert_eq!(table.get("b").map(String::as_str), Some("user-b"));
        assert_eq!(table.get("c").map(String::as_str), Some("user-c"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
