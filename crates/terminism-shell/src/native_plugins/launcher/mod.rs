//! Launcher native plugin: global defaults, version resolution, process supervision, event API.

mod java;
mod process;
mod resolve;
mod settings;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;
use process::RunningGame;
use resolve::LaunchError;
use settings::{LaunchSettings, ResolvedSettings};

pub struct Launcher {
    settings_dir: PathBuf,
    logs_dir: PathBuf,
    running: HashMap<String, RunningGame>,
    java: Option<Vec<java::JavaCandidate>>,
}

struct InstanceEntry {
    name: String,
    root: PathBuf,
    version: String,
    isolated: bool,
    game_dir: Option<String>,
}

impl Launcher {
    pub fn new(home: PathBuf) -> Self {
        Self {
            settings_dir: home.join("instance_settings"),
            logs_dir: home.join("logs").join("launcher"),
            running: HashMap::new(),
            java: None,
        }
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.logs_dir.join(format!("{}.log", sanitize_id(id)))
    }

    fn global_settings(&self, host: &HostHandle) -> LaunchSettings {
        LaunchSettings::global(&host.config().borrow())
    }

    fn instance_settings(&self, id: &str) -> Result<LaunchSettings, String> {
        settings::load_instance(&self.settings_dir, id)
    }

    fn settings_snapshot(
        &self,
        host: &HostHandle,
        instance: Option<&str>,
    ) -> (Value, Option<String>) {
        let global = self.global_settings(host);
        let mut snapshot = json!({"global": global.resolved().to_json()});
        let mut error = None;
        if let Some(id) = instance {
            match self.instance_settings(id) {
                Ok(overrides) => {
                    let resolved = overrides.merged_with(&global).resolved();
                    snapshot["instance"] = json!({
                        "id": id,
                        "overrides": overrides.to_json(),
                        "resolved": resolved.to_json(),
                    });
                }
                Err(load_error) => error = Some(load_error),
            }
        }
        (snapshot, error)
    }

    fn find_instance(host: &HostHandle, id: &str) -> Result<InstanceEntry, LaunchError> {
        let store = host.config();
        let config = store.borrow();
        let list = config
            .get("instances", "list")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let entry = list
            .iter()
            .find(|item| item.get("id").and_then(Json::as_str) == Some(id))
            .ok_or_else(|| LaunchError::new("not_configured", format!("实例 {id} 不存在")))?;
        let string = |key: &str| entry.get(key).and_then(Json::as_str).map(str::to_string);
        let root = string("root").map(PathBuf::from).ok_or_else(|| {
            LaunchError::new(
                "not_configured",
                format!("实例 {id} 未配置 .minecraft 目录"),
            )
        })?;
        let version = string("version")
            .ok_or_else(|| LaunchError::new("not_configured", format!("实例 {id} 未配置版本")))?;
        Ok(InstanceEntry {
            name: string("name").unwrap_or_else(|| id.to_string()),
            root,
            version,
            isolated: entry
                .get("isolated")
                .and_then(Json::as_bool)
                .unwrap_or(true),
            game_dir: string("game_dir").filter(|value| !value.is_empty()),
        })
    }

    fn game_directory(entry: &InstanceEntry) -> Result<PathBuf, LaunchError> {
        if !entry.root.is_dir() {
            return Err(LaunchError::new(
                "missing_root",
                format!("游戏目录不存在: {}", entry.root.display()),
            ));
        }
        if let Some(relative) = &entry.game_dir {
            let path = entry.root.join(relative);
            if !path.is_dir() {
                return Err(LaunchError::new(
                    "game_dir_invalid",
                    format!("实例目录不存在: {}", path.display()),
                ));
            }
            let root = entry.root.canonicalize().map_err(|error| {
                LaunchError::new(
                    "game_dir_invalid",
                    format!("{}: {error}", entry.root.display()),
                )
            })?;
            let path = path.canonicalize().map_err(|error| {
                LaunchError::new(
                    "game_dir_invalid",
                    format!("{}: {error}", entry.root.display()),
                )
            })?;
            if !path.starts_with(&root) {
                return Err(LaunchError::new(
                    "game_dir_invalid",
                    "实例目录必须位于 .minecraft 根目录内",
                ));
            }
            return Ok(path);
        }
        if entry.isolated {
            let path = entry.root.join("versions").join(&entry.version);
            if !path.is_dir() {
                return Err(LaunchError::new(
                    "version_missing",
                    format!("版本目录不存在: {}", path.display()),
                ));
            }
            Ok(path)
        } else {
            Ok(entry.root.clone())
        }
    }

    fn ensure_java(
        &mut self,
        resolved: &ResolvedSettings,
        required: Option<u32>,
        logger: &Logger,
    ) -> Result<PathBuf, LaunchError> {
        if let Some(configured) = resolved.java_path.as_ref().filter(|path| !path.is_empty()) {
            let path = PathBuf::from(configured);
            if !path.is_file() {
                return Err(LaunchError::new(
                    "java_invalid",
                    format!("Java 路径不存在: {}", path.display()),
                ));
            }
            let candidate = java::probe(&path);
            let Some(major) = candidate.major else {
                return Err(LaunchError::new(
                    "java_invalid",
                    format!("无法执行 Java: {}", path.display()),
                ));
            };
            if let Some(required) = required
                && major != required
            {
                logger.log(&format!(
                    "java mismatch: instance selects Java {major}, version recommends {required}"
                ));
            }
            return Ok(path);
        }
        if self.java.is_none() {
            self.java = Some(java::discover());
        }
        let candidates = self.java.as_deref().unwrap_or_default();
        if let Some(required) = required {
            if let Some(candidate) = java::find_for_major(candidates, Some(required)) {
                return Ok(candidate.path.clone());
            }
            let available: Vec<String> = candidates
                .iter()
                .filter(|candidate| candidate.valid())
                .map(|candidate| {
                    format!(
                        "{} (Java {})",
                        candidate.path.display(),
                        candidate.major.unwrap_or(0)
                    )
                })
                .collect();
            if available.is_empty() {
                return Err(LaunchError::new(
                    "java_missing",
                    "未找到可用的 Java；请在设置里指定 JDK 路径",
                ));
            }
            return Err(LaunchError::new(
                "java_mismatch",
                format!("需要 Java {required}，当前可用: {}", available.join(", ")),
            ));
        }
        if let Some(candidate) = java::find_for_major(candidates, None) {
            return Ok(candidate.path.clone());
        }
        Err(LaunchError::new(
            "java_missing",
            "未找到可用的 Java；请在设置里指定 JDK 路径",
        ))
    }

    fn fail(
        &self,
        host: &mut HostHandle,
        logger: &Logger,
        instance: Option<&str>,
        error: LaunchError,
    ) {
        logger.log(&format!("launch error: {}: {}", error.kind, error.message));
        let mut payload = json!({"kind": error.kind, "message": error.message});
        if let Some(id) = instance {
            payload["instance"] = json!(id);
        }
        host.publish("launcher.error", payload);
        if let Some(id) = instance {
            host.publish(
                "launcher.state",
                json!({"instance": id, "state": "error", "error": error.kind}),
            );
        }
    }

    fn launch(&mut self, host: &mut HostHandle, logger: &Logger, id: Option<String>) {
        let Some(id) = id else {
            self.fail(
                host,
                logger,
                None,
                LaunchError::new("not_configured", "缺少 instance"),
            );
            return;
        };
        if self.running.contains_key(&id) {
            self.fail(
                host,
                logger,
                Some(&id),
                LaunchError::new("already_running", format!("实例 {id} 已在运行")),
            );
            return;
        }
        let entry = match Self::find_instance(host, &id) {
            Ok(entry) => entry,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let game_dir = match Self::game_directory(&entry) {
            Ok(dir) => dir,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let global = self.global_settings(host);
        let overrides = match self.instance_settings(&id) {
            Ok(overrides) => overrides,
            Err(load_error) => {
                logger.log(&format!("instance settings load failed: {load_error}"));
                LaunchSettings::default()
            }
        };
        let resolved = overrides.merged_with(&global).resolved();
        let identity = match resolve::client_identity(&host.config().borrow()) {
            Ok(identity) => identity,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let authlib_jar = host
            .config()
            .borrow()
            .get("launcher", "authlib_jar")
            .and_then(Json::as_str)
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from);
        let required = match resolve::required_java(&entry.root, &entry.version) {
            Ok(required) => required,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let java = match self.ensure_java(&resolved, required, logger) {
            Ok(java) => java,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let plan = match resolve::build_plan(
            &entry.root,
            &entry.version,
            &game_dir,
            &resolved,
            java,
            &identity,
            authlib_jar.as_deref(),
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.fail(host, logger, Some(&id), error);
                return;
            }
        };
        let log_path = self.log_path(&id);
        match process::spawn(&plan, &log_path) {
            Ok(game) => {
                let pid = game.pid;
                self.running.insert(id.clone(), game);
                host.publish(
                    "launcher.state",
                    json!({"instance": id, "state": "running", "pid": pid}),
                );
                host.publish(
                    "launcher.log",
                    json!({"instance": id, "lines": [], "revision": 0, "reset": true}),
                );
                logger.log(&format!("game started: {id} ({}) pid {pid}", entry.name));
            }
            Err(message) => self.fail(
                host,
                logger,
                Some(&id),
                LaunchError::new("spawn_failed", message),
            ),
        }
    }

    fn stop(&mut self, host: &mut HostHandle, logger: &Logger, id: Option<String>) {
        let Some(id) = id else {
            self.fail(
                host,
                logger,
                None,
                LaunchError::new("not_configured", "缺少 instance"),
            );
            return;
        };
        let Some(mut game) = self.running.remove(&id) else {
            self.fail(
                host,
                logger,
                Some(&id),
                LaunchError::new("not_running", format!("实例 {id} 未在运行")),
            );
            return;
        };
        let lines = process::read_new_lines(&mut game);
        if !lines.is_empty() {
            host.publish(
                "launcher.log",
                json!({"instance": id, "lines": lines, "revision": game.revision}),
            );
        }
        let _ = game.child.kill();
        let code = game.child.wait().ok().and_then(|status| status.code());
        host.publish(
            "launcher.state",
            json!({"instance": id, "state": "stopped", "code": code}),
        );
        logger.log(&format!("game stopped: {id}"));
    }

    fn settings_get(&self, host: &mut HostHandle, logger: &Logger, instance: Option<String>) {
        let (snapshot, error) = self.settings_snapshot(host, instance.as_deref());
        if let Some(error) = error {
            logger.log(&format!("settings load failed: {error}"));
        }
        host.publish("launcher.settings", snapshot);
    }

    fn settings_set(&mut self, host: &mut HostHandle, logger: &Logger, payload: &Json) {
        let scope = payload.get("scope").and_then(Json::as_str).unwrap_or("");
        let key = payload.get("key").and_then(Json::as_str).unwrap_or("");
        let value = payload.get("value").cloned().unwrap_or(Value::Null);
        let instance = payload
            .get("instance")
            .and_then(Json::as_str)
            .map(str::to_string);
        if !valid_setting_key(key) {
            self.fail(
                host,
                logger,
                instance.as_deref(),
                LaunchError::new("bad_setting", format!("未知设置项: {key}")),
            );
            return;
        }
        match (scope, instance.as_deref()) {
            ("global", _) => {
                host.config().borrow_mut().set("launcher", key, value);
                self.settings_get(host, logger, None);
            }
            ("instance", Some(id)) => {
                let mut overrides = match self.instance_settings(id) {
                    Ok(overrides) => overrides,
                    Err(error) => {
                        logger.log(&format!("instance settings load failed: {error}"));
                        LaunchSettings::default()
                    }
                };
                if let Err(error) = apply_setting(&mut overrides, key, &value) {
                    self.fail(
                        host,
                        logger,
                        Some(id),
                        LaunchError::new("bad_setting", error),
                    );
                    return;
                }
                if let Err(error) = settings::save_instance(&self.settings_dir, id, &overrides) {
                    self.fail(
                        host,
                        logger,
                        Some(id),
                        LaunchError::new("settings_io", error),
                    );
                    return;
                }
                self.settings_get(host, logger, Some(id.to_string()));
            }
            _ => self.fail(
                host,
                logger,
                None,
                LaunchError::new(
                    "bad_setting",
                    "settings.set 需要 scope 与 instance".to_string(),
                ),
            ),
        }
    }

    fn log_get(&self, host: &mut HostHandle, instance: Option<String>) {
        let Some(id) = instance else {
            return;
        };
        let lines = process::tail_lines(&self.log_path(&id), 50);
        host.publish(
            "launcher.log",
            json!({"instance": id, "lines": lines, "revision": 0, "reset": true}),
        );
    }

    fn java_detect(&mut self, host: &mut HostHandle) {
        self.java = Some(java::discover());
        let candidates: Vec<Value> = self
            .java
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|candidate| {
                json!({
                    "path": candidate.path.display().to_string(),
                    "major": candidate.major,
                    "valid": candidate.valid(),
                })
            })
            .collect();
        host.publish("launcher.java", json!({"candidates": candidates}));
    }
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn valid_setting_key(key: &str) -> bool {
    matches!(
        key,
        "java_path" | "memory_mb" | "jvm_args" | "game_args" | "authlib_jar"
    )
}

fn setting_strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn apply_setting(settings: &mut LaunchSettings, key: &str, value: &Value) -> Result<(), String> {
    match key {
        "java_path" => {
            settings.java_path = match value {
                Value::Null => None,
                Value::String(text) if text.is_empty() => None,
                Value::String(text) => Some(text.clone()),
                _ => return Err("java_path 必须是字符串".into()),
            }
        }
        "memory_mb" => {
            settings.memory_mb = match value {
                Value::Null => None,
                Value::Number(number) => match number.as_u64() {
                    Some(memory) if memory >= 512 => Some(memory),
                    _ => return Err("memory_mb 必须是 >=512 的整数".into()),
                },
                _ => return Err("memory_mb 必须是 >=512 的整数".into()),
            }
        }
        "jvm_args" | "game_args" => {
            let parsed = match value {
                Value::Null => None,
                _ => Some(setting_strings(value).ok_or_else(|| format!("{key} 必须是字符串数组"))?),
            };
            if key == "jvm_args" {
                settings.jvm_args = parsed;
            } else {
                settings.game_args = parsed;
            }
        }
        _ => return Err(format!("未知设置项: {key}")),
    }
    Ok(())
}

impl NativePlugin for Launcher {
    fn manifest(&self) -> Manifest {
        manifest(
            "launcher",
            "Launcher",
            &["launcher.command"],
            "Resolves launch parameters and supervises game processes",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        for dir in [&self.settings_dir, &self.logs_dir] {
            if let Err(error) = std::fs::create_dir_all(dir) {
                logger.log(&format!("launcher dir failed: {}: {error}", dir.display()));
            }
        }
        let (snapshot, _) = self.settings_snapshot(host, None);
        host.publish("launcher.settings", snapshot);
        logger.log("launcher ready");
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        if name != "launcher.command" {
            return;
        }
        let action = payload.get("action").and_then(Json::as_str).unwrap_or("");
        let instance = payload
            .get("instance")
            .and_then(Json::as_str)
            .map(str::to_string);
        match action {
            "launch" => self.launch(host, logger, instance),
            "stop" => self.stop(host, logger, instance),
            "settings.get" => self.settings_get(host, logger, instance),
            "settings.set" => self.settings_set(host, logger, payload),
            "log.get" => self.log_get(host, instance),
            "java.detect" => self.java_detect(host),
            other => logger.log(&format!("unknown launcher command: {other}")),
        }
    }

    fn on_tick(&mut self, host: &mut HostHandle, logger: &Logger) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            let mut lines = Vec::new();
            let mut revision = 0;
            let mut exited = None;
            if let Some(game) = self.running.get_mut(&id) {
                lines = process::read_new_lines(game);
                revision = game.revision;
                exited = game.child.try_wait().ok().flatten();
            }
            if !lines.is_empty() {
                host.publish(
                    "launcher.log",
                    json!({"instance": id, "lines": lines, "revision": revision}),
                );
            }
            if let Some(status) = exited {
                let mut game = self.running.remove(&id).expect("checked above");
                let tail = process::read_new_lines(&mut game);
                if !tail.is_empty() {
                    host.publish(
                        "launcher.log",
                        json!({"instance": id, "lines": tail, "revision": game.revision}),
                    );
                }
                host.publish(
                    "launcher.state",
                    json!({"instance": id, "state": "exited", "code": status.code()}),
                );
                logger.log(&format!("game exited: {id} {:?}", status.code()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_keys_are_validated() {
        assert!(valid_setting_key("memory_mb"));
        assert!(!valid_setting_key("nope"));
        let mut settings = LaunchSettings::default();
        assert!(apply_setting(&mut settings, "memory_mb", &json!(4096)).is_ok());
        assert_eq!(settings.memory_mb, Some(4096));
        assert!(apply_setting(&mut settings, "memory_mb", &json!(10)).is_err());
        assert!(apply_setting(&mut settings, "java_path", &json!(null)).is_ok());
        assert_eq!(settings.java_path, None);
    }

    #[test]
    fn log_file_names_are_sanitized() {
        assert_eq!(sanitize_id("inst-1a2b"), "inst-1a2b");
        assert_eq!(sanitize_id("../evil"), "___evil");
    }

    #[test]
    fn global_settings_read_flat_config_keys() {
        let dir =
            std::env::temp_dir().join(format!("terminism-launcher-global-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut config, _) = terminism_kernel::host::ConfigStore::load(&dir.join("config.toml"));
        config.set("launcher", "memory_mb", json!(8192));
        let settings = LaunchSettings::global(&config);
        assert_eq!(settings.memory_mb, Some(8192));
        assert_eq!(settings.java_path, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
