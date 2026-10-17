use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Clone)]
pub struct ConfigChange {
    pub section: String,
    pub key: String,
    pub value: Value,
}

#[derive(Clone)]
pub struct ConfigStore {
    path: PathBuf,
    map: BTreeMap<String, BTreeMap<String, Value>>,
    changes: Vec<ConfigChange>,
    dirty: bool,
    revision: u64,
}

impl ConfigStore {
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let mut store = Self {
            path: path.to_path_buf(),
            map: BTreeMap::new(),
            changes: Vec::new(),
            dirty: false,
            revision: 0,
        };
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(map) => {
                    store.map = map;
                    (store, None)
                }
                Err(e) => (store, Some(e.to_string())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (store, None),
            Err(e) => (store, Some(e.to_string())),
        }
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&Value> {
        self.map.get(section)?.get(key)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set(&mut self, section: &str, key: &str, value: Value) -> bool {
        if self.get(section, key) == Some(&value) {
            return false;
        }
        self.map
            .entry(section.into())
            .or_default()
            .insert(key.into(), value.clone());
        self.changes.push(ConfigChange {
            section: section.into(),
            key: key.into(),
            value,
        });
        self.revision += 1;
        self.dirty = true;
        true
    }

    pub(crate) fn take_changes(&mut self) -> Vec<ConfigChange> {
        std::mem::take(&mut self.changes)
    }

    pub fn flush(&mut self) -> std::io::Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        let text = toml::to_string(&self.map).map_err(std::io::Error::other)?;
        let mut temp_name = self.path.as_os_str().to_os_string();
        temp_name.push(".tmp");
        let temp_path = PathBuf::from(temp_name);
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temp_path, &self.path)?;
        self.dirty = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("terminism-config-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn writes_are_batched_and_equal_values_are_ignored() {
        let path = path("batch");
        let (mut cfg, _) = ConfigStore::load(&path);
        assert!(cfg.set("a", "one", json!(1)));
        assert!(cfg.set("a", "two", json!(2)));
        assert!(!path.exists());
        assert!(cfg.flush().unwrap());
        assert!(!cfg.set("a", "two", json!(2)));
        assert!(!cfg.flush().unwrap());
        assert_eq!(cfg.revision(), 2);
        assert_eq!(cfg.take_changes().len(), 2);
        assert_eq!(ConfigStore::load(&path).0.get("a", "two"), Some(&json!(2)));
    }

    #[test]
    fn failed_write_keeps_the_previous_file() {
        let path = path("failure");
        std::fs::write(&path, "[a]\nkey = 1\n").unwrap();
        let (mut cfg, _) = ConfigStore::load(&path);
        let temp = PathBuf::from(format!("{}.tmp", path.display()));
        let _ = std::fs::remove_file(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        cfg.set("a", "key", json!(2));
        assert!(cfg.flush().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[a]\nkey = 1\n");
        std::fs::remove_dir(&temp).unwrap();
        assert!(cfg.flush().unwrap());
        assert_eq!(ConfigStore::load(&path).0.get("a", "key"), Some(&json!(2)));
        assert!(!cfg.flush().unwrap());
    }
}
