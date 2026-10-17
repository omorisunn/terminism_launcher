//! Launch parameter settings: global defaults in config, per-instance overrides in TOML.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use terminism_kernel::host::ConfigStore;

pub const DEFAULT_MEMORY_MB: u64 = 4096;
pub const DEFAULT_PLAYER_NAME: &str = "Player";

/// Sparse settings: a missing field falls back to the global default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaunchSettings {
    pub java_path: Option<String>,
    pub memory_mb: Option<u64>,
    pub jvm_args: Option<Vec<String>>,
    pub game_args: Option<Vec<String>>,
}

/// Fully resolved settings, ready for launch plan assembly.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSettings {
    pub java_path: Option<String>,
    pub memory_mb: u64,
    pub jvm_args: Vec<String>,
    pub game_args: Vec<String>,
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
fn json_strings(value: &Value, key: &str) -> Option<Vec<String>> {
    value.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

impl LaunchSettings {
    #[cfg(test)]
    pub fn from_json(value: &Value) -> Self {
        Self {
            java_path: value
                .get("java_path")
                .and_then(Value::as_str)
                .and_then(non_empty),
            memory_mb: value.get("memory_mb").and_then(Value::as_u64),
            jvm_args: json_strings(value, "jvm_args"),
            game_args: json_strings(value, "game_args"),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(java) = &self.java_path {
            map.insert("java_path".into(), json!(java));
        }
        if let Some(memory) = self.memory_mb {
            map.insert("memory_mb".into(), json!(memory));
        }
        if let Some(args) = &self.jvm_args {
            map.insert("jvm_args".into(), json!(args));
        }
        if let Some(args) = &self.game_args {
            map.insert("game_args".into(), json!(args));
        }
        Value::Object(map)
    }

    pub fn global(store: &ConfigStore) -> Self {
        let strings = |key: &str| {
            store
                .get("launcher", key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        };
        Self {
            java_path: store
                .get("launcher", "java_path")
                .and_then(Value::as_str)
                .and_then(non_empty),
            memory_mb: store.get("launcher", "memory_mb").and_then(Value::as_u64),
            jvm_args: strings("jvm_args"),
            game_args: strings("game_args"),
        }
    }

    pub fn merged_with(&self, base: &Self) -> Self {
        Self {
            java_path: self.java_path.clone().or_else(|| base.java_path.clone()),
            memory_mb: self.memory_mb.or(base.memory_mb),
            jvm_args: self.jvm_args.clone().or_else(|| base.jvm_args.clone()),
            game_args: self.game_args.clone().or_else(|| base.game_args.clone()),
        }
    }

    pub fn resolved(&self) -> ResolvedSettings {
        ResolvedSettings {
            java_path: self.java_path.clone(),
            memory_mb: self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB).max(512),
            jvm_args: self.jvm_args.clone().unwrap_or_default(),
            game_args: self.game_args.clone().unwrap_or_default(),
        }
    }
}

impl ResolvedSettings {
    pub fn to_json(&self) -> Value {
        json!({
            "java_path": self.java_path,
            "memory_mb": self.memory_mb,
            "jvm_args": self.jvm_args,
            "game_args": self.game_args,
        })
    }
}

pub fn instance_settings_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.toml"))
}

pub fn load_instance(dir: &Path, id: &str) -> Result<LaunchSettings, String> {
    let path = instance_settings_path(dir, id);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_instance(&text).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LaunchSettings::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn parse_instance(text: &str) -> Result<LaunchSettings, String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let table = value.as_table().ok_or("settings root must be a table")?;
    let memory = table
        .get("memory_mb")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok());
    let strings = |key: &str| {
        table.get(key).and_then(toml::Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };
    Ok(LaunchSettings {
        java_path: table
            .get("java_path")
            .and_then(toml::Value::as_str)
            .and_then(non_empty),
        memory_mb: memory,
        jvm_args: strings("jvm_args"),
        game_args: strings("game_args"),
    })
}

pub fn save_instance(dir: &Path, id: &str, settings: &LaunchSettings) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let mut table = toml::Table::new();
    if let Some(java) = &settings.java_path {
        table.insert("java_path".into(), toml::Value::String(java.clone()));
    }
    if let Some(memory) = settings.memory_mb {
        table.insert("memory_mb".into(), toml::Value::Integer(memory as i64));
    }
    if let Some(args) = &settings.jvm_args {
        table.insert(
            "jvm_args".into(),
            toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
        );
    }
    if let Some(args) = &settings.game_args {
        table.insert(
            "game_args".into(),
            toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
        );
    }
    let text = toml::to_string(&toml::Value::Table(table)).map_err(|error| error.to_string())?;
    let path = instance_settings_path(dir, id);
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).map_err(|error| error.to_string())?;
    std::fs::rename(&temp, &path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_json_roundtrip_keeps_only_overrides() {
        let value = json!({"memory_mb": 8192, "jvm_args": ["-XX:+UseG1GC"]});
        let settings = LaunchSettings::from_json(&value);
        assert_eq!(settings.memory_mb, Some(8192));
        assert_eq!(settings.java_path, None);
        assert_eq!(settings.to_json(), value);
    }

    #[test]
    fn merge_prefers_instance_then_global_then_defaults() {
        let global = LaunchSettings {
            java_path: Some("global".into()),
            memory_mb: Some(2048),
            jvm_args: Some(vec!["-Xss1M".into()]),
            game_args: None,
        };
        let instance = LaunchSettings {
            memory_mb: Some(4096),
            ..LaunchSettings::default()
        };
        let merged = instance.merged_with(&global).resolved();
        assert_eq!(merged.java_path.as_deref(), Some("global"));
        assert_eq!(merged.memory_mb, 4096);
        assert_eq!(merged.jvm_args, vec!["-Xss1M".to_string()]);
        assert!(merged.game_args.is_empty());
        assert_eq!(
            LaunchSettings::default().resolved().memory_mb,
            DEFAULT_MEMORY_MB
        );
    }

    #[test]
    fn instance_file_roundtrips_and_missing_file_is_default() {
        let dir = std::env::temp_dir().join(format!(
            "terminism-launcher-settings-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = LaunchSettings {
            java_path: Some("C:/jdk/bin/java.exe".into()),
            memory_mb: Some(6144),
            jvm_args: Some(vec!["-Xss1M".into()]),
            game_args: Some(vec!["--fullscreen".into()]),
        };
        save_instance(&dir, "inst-1", &settings).unwrap();
        assert_eq!(load_instance(&dir, "inst-1").unwrap(), settings);
        assert_eq!(
            load_instance(&dir, "missing").unwrap(),
            LaunchSettings::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
