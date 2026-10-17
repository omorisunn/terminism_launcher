//! Instance plugin: registry, .minecraft root discovery, page and launch controls.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::file_picker::{Picker, PickerMode};
use super::manifest;

const ACTOR: &str = "instances";
const PAGE: &str = "/instances";
const SPLIT: &str = "/instances/split";
const TREE: &str = "/instances/split/tree";
const ROOTS: &str = "/instances/split/tree/roots";
const TOOLBAR: &str = "/instances/split/tree/actions";
const ADD_BUTTON: &str = "/instances/split/tree/actions/add";
const REFRESH_BUTTON: &str = "/instances/split/tree/actions/refresh";
const DETAIL: &str = "/instances/split/detail";
const LAYOUT: &str = "/instances/split/detail/layout";
const TABS: &str = "/instances/split/detail/layout/tabs";
const STATUS: &str = "/instances/split/detail/layout/status";
const ACTIONS: &str = "/instances/split/detail/layout/actions";
const SETTINGS_LIST: &str = "/instances/split/detail/layout/settings";
const MEMORY_SLIDER: &str = "/instances/split/detail/layout/settings/memory";
const LOG: &str = "/instances/split/detail/layout/log";
const DIALOG: &str = "/instances/dialog";
const DIALOG_INPUT: &str = "/instances/dialog/row/input";
const DIALOG_BROWSE: &str = "/instances/dialog/row/browse";
const PICKER: &str = "/instances/picker";
const DIALOG_OK: &str = "/instances/dialog/ok";
const DIALOG_CANCEL: &str = "/instances/dialog/cancel";

const SETTING_KEYS: &[&str] = &["java_path", "jvm_args", "game_args", "game_dir", "isolated"];
const MEMORY_PRESETS: &[u64] = &[1024, 2048, 4096, 8192];

fn normalize_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return PathBuf::from(format!("\\\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn normalize_path_text(text: &str) -> String {
    normalize_path(Path::new(text)).display().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstanceEntry {
    id: String,
    name: String,
    root: String,
    version: String,
    isolated: bool,
    game_dir: Option<String>,
}

impl InstanceEntry {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            name: value
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            root: value
                .get("root")
                .and_then(Json::as_str)
                .map(normalize_path_text)
                .unwrap_or_default(),
            version: value
                .get("version")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            isolated: value
                .get("isolated")
                .and_then(Json::as_bool)
                .unwrap_or(true),
            game_dir: value
                .get("game_dir")
                .and_then(Json::as_str)
                .filter(|value| !value.is_empty())
                .map(normalize_path_text),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "root": self.root,
            "version": self.version,
            "isolated": self.isolated,
            "game_dir": self.game_dir.clone().unwrap_or_default(),
        })
    }

    fn version_json(&self) -> PathBuf {
        if self.root.is_empty() || self.version.is_empty() {
            return PathBuf::new();
        }
        Path::new(&self.root)
            .join("versions")
            .join(&self.version)
            .join(format!("{}.json", self.version))
    }

    fn installed(&self) -> bool {
        self.version_json().is_file()
    }

    fn game_dir(&self) -> PathBuf {
        if let Some(relative) = &self.game_dir {
            return Path::new(&self.root).join(relative);
        }
        if self.isolated {
            Path::new(&self.root).join("versions").join(&self.version)
        } else {
            PathBuf::from(&self.root)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoaderKind {
    Fabric,
    Forge,
    NeoForge,
    LiteLoader,
}

impl LoaderKind {
    fn label(self) -> &'static str {
        match self {
            LoaderKind::Fabric => "Fabric",
            LoaderKind::Forge => "Forge",
            LoaderKind::NeoForge => "NeoForge",
            LoaderKind::LiteLoader => "LiteLoader",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoaderInfo {
    kind: LoaderKind,
    version: Option<String>,
}

fn game_args(json: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(args) = json
        .get("arguments")
        .and_then(|arguments| arguments.get("game"))
        .and_then(Json::as_array)
    {
        for argument in args {
            match argument {
                Value::String(text) => out.push(text.clone()),
                Value::Object(map) => {
                    let Some(value) = map.get("value") else {
                        continue;
                    };
                    if let Some(text) = value.as_str() {
                        out.push(text.to_string());
                    } else if let Some(items) = value.as_array() {
                        out.extend(items.iter().filter_map(Json::as_str).map(str::to_string));
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(legacy) = json.get("minecraftArguments").and_then(Json::as_str) {
        out.extend(legacy.split_whitespace().map(str::to_string));
    }
    out
}

fn library_names(json: &Value) -> Vec<&str> {
    json.get("libraries")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("name").and_then(Json::as_str))
                .collect()
        })
        .unwrap_or_default()
}

fn argument_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == key)
        .map(|window| window[1].as_str())
}

fn coordinate_version(name: &str) -> Option<String> {
    name.split(':').nth(2).map(str::to_string)
}

fn forge_version(value: String) -> String {
    value
        .split_once('-')
        .map(|(_, version)| version.to_string())
        .unwrap_or(value)
}

fn detect_loader(json: &Value, version_id: &str) -> Option<LoaderInfo> {
    let libraries = library_names(json);
    let args = game_args(json);
    let has_arg = |key: &str| args.iter().any(|arg| arg == key);
    if libraries
        .iter()
        .any(|name| name.starts_with("net.neoforged"))
        || has_arg("--fml.neoForgeVersion")
    {
        let version = argument_value(&args, "--fml.neoForgeVersion")
            .map(str::to_string)
            .or_else(|| {
                libraries
                    .iter()
                    .find(|name| name.starts_with("net.neoforged.fancymodloader:loader:"))
                    .and_then(|name| coordinate_version(name))
            });
        return Some(LoaderInfo {
            kind: LoaderKind::NeoForge,
            version,
        });
    }
    if libraries.iter().any(|name| {
        name.starts_with("net.minecraftforge")
            && (name.contains("fmlloader") || name.contains("fmlearlydisplay"))
    }) || has_arg("--fml.forgeVersion")
    {
        let version = argument_value(&args, "--fml.forgeVersion")
            .map(str::to_string)
            .or_else(|| {
                libraries
                    .iter()
                    .find(|name| name.contains("fmlloader"))
                    .and_then(|name| coordinate_version(name))
                    .map(forge_version)
            });
        return Some(LoaderInfo {
            kind: LoaderKind::Forge,
            version,
        });
    }
    if let Some(name) = libraries
        .iter()
        .find(|name| name.starts_with("net.fabricmc:fabric-loader:"))
    {
        return Some(LoaderInfo {
            kind: LoaderKind::Fabric,
            version: coordinate_version(name),
        });
    }
    if libraries
        .iter()
        .any(|name| name.starts_with("com.mumfrey:liteloader:"))
        || args
            .iter()
            .any(|arg| arg.contains("com.mumfrey.liteloader"))
    {
        let version = libraries
            .iter()
            .find(|name| name.starts_with("com.mumfrey:liteloader:"))
            .and_then(|name| coordinate_version(name));
        return Some(LoaderInfo {
            kind: LoaderKind::LiteLoader,
            version,
        });
    }
    let lower = version_id.to_ascii_lowercase();
    let kind = if lower.contains("neoforge") {
        LoaderKind::NeoForge
    } else if lower.contains("fabric-loader") {
        LoaderKind::Fabric
    } else if lower.contains("liteloader") {
        LoaderKind::LiteLoader
    } else if lower.contains("forge") {
        LoaderKind::Forge
    } else {
        return None;
    };
    Some(LoaderInfo {
        kind,
        version: None,
    })
}

#[derive(Clone, Debug, PartialEq)]
enum DialogKind {
    AddRoot,
    ForgetRoot(String),
    Setting(String),
}

#[derive(Clone, Debug)]
struct Dialog {
    kind: DialogKind,
    input: String,
}

pub struct Instances {
    selected: Option<String>,
    last_focus: Option<String>,
    collapsed: HashSet<String>,
    expanded: Option<String>,
    page_width: Option<u16>,
    states: HashMap<String, Value>,
    logs: HashMap<String, Vec<String>>,
    global: Value,
    overrides: Value,
    resolved: Value,
    java: Vec<Value>,
    dialog: Option<Dialog>,
    picker: Option<Picker>,
    tree_nodes: Vec<String>,
    detail_nodes: Vec<String>,
    log_nodes: Vec<String>,
}

impl Default for Instances {
    fn default() -> Self {
        Self::new()
    }
}

impl Instances {
    pub fn new() -> Self {
        Self {
            selected: None,
            last_focus: None,
            collapsed: HashSet::new(),
            expanded: None,
            page_width: None,
            states: HashMap::new(),
            logs: HashMap::new(),
            global: Value::Null,
            overrides: Value::Null,
            resolved: Value::Null,
            java: Vec::new(),
            dialog: None,
            picker: None,
            tree_nodes: Vec::new(),
            detail_nodes: Vec::new(),
            log_nodes: Vec::new(),
        }
    }

    fn t(&self, host: &HostHandle, key: &str) -> String {
        host.i18n()
            .borrow()
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    fn set(&self, host: &HostHandle, path: &str, kind: &str, props: Value) -> bool {
        (host.ui().create)(ACTOR, path, kind, props)
    }

    fn remove_paths(host: &HostHandle, paths: &mut Vec<String>) {
        let ui = host.ui();
        for path in std::mem::take(paths) {
            let _ = (ui.remove)(ACTOR, &path);
        }
    }

    /* ---------------- config ---------------- */

    fn roots(host: &HostHandle) -> Vec<String> {
        host.config()
            .borrow()
            .get("instances", "roots")
            .and_then(Json::as_array)
            .map(|items| {
                let mut roots: Vec<String> = Vec::new();
                for text in items.iter().filter_map(Json::as_str) {
                    let root = normalize_path_text(text);
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                roots
            })
            .unwrap_or_default()
    }

    fn entries(host: &HostHandle) -> Vec<InstanceEntry> {
        host.config()
            .borrow()
            .get("instances", "list")
            .and_then(Json::as_array)
            .map(|items| items.iter().filter_map(InstanceEntry::from_json).collect())
            .unwrap_or_default()
    }

    fn selected_entry(&self, host: &HostHandle) -> Option<InstanceEntry> {
        let id = self.selected.as_ref()?;
        Self::entries(host)
            .into_iter()
            .find(|entry| &entry.id == id)
    }

    fn save(&self, host: &mut HostHandle, roots: &[String], entries: &[InstanceEntry]) {
        {
            let store = host.config();
            let mut config = store.borrow_mut();
            config.set("instances", "roots", json!(roots));
            config.set(
                "instances",
                "list",
                Value::Array(entries.iter().map(InstanceEntry::to_json).collect()),
            );
        }
        host.publish("instances.updated", json!({"count": entries.len()}));
    }

    fn update_entry(
        &self,
        host: &mut HostHandle,
        id: &str,
        update: impl FnOnce(&mut InstanceEntry),
    ) {
        let mut entries = Self::entries(host);
        let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        update(entry);
        let roots = Self::roots(host);
        self.save(host, &roots, &entries);
    }

    /* ---------------- discovery ---------------- */

    fn is_minecraft_root(path: &Path) -> bool {
        path.join("versions").is_dir()
    }

    fn scan_versions(root: &str) -> Vec<String> {
        let versions = Path::new(root).join("versions");
        let Ok(entries) = std::fs::read_dir(&versions) else {
            return Vec::new();
        };
        let mut found: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|id| versions.join(id).join(format!("{id}.json")).is_file())
            .collect();
        found.sort();
        found
    }

    fn stable_id(root: &str, version: &str) -> String {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in root
            .bytes()
            .chain(std::iter::once(b'|'))
            .chain(version.bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("inst-{:08x}", hash as u32)
    }

    fn discover_from(candidates: Vec<PathBuf>) -> Vec<String> {
        let mut roots = Vec::new();
        for candidate in candidates {
            if !Self::is_minecraft_root(&candidate) {
                continue;
            }
            let resolved = normalize_path(&candidate.canonicalize().unwrap_or(candidate));
            let text = resolved.display().to_string();
            if !roots.contains(&text) {
                roots.push(text);
            }
        }
        roots
    }

    fn discover_common_roots() -> Vec<String> {
        let mut candidates = Vec::new();
        if let Ok(appdata) = std::env::var("APPDATA") {
            candidates.push(Path::new(&appdata).join(".minecraft"));
        }
        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            candidates.push(Path::new(&home).join(".minecraft"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(".minecraft"));
        }
        Self::discover_from(candidates)
    }

    fn import_versions(roots: &[String], existing: &[InstanceEntry]) -> Vec<InstanceEntry> {
        let mut entries = existing.to_vec();
        for root in roots {
            for version in Self::scan_versions(root) {
                if entries
                    .iter()
                    .any(|entry| &entry.root == root && entry.version == version)
                {
                    continue;
                }
                let base = Self::stable_id(root, &version);
                let mut id = base.clone();
                let mut suffix = 1;
                while entries.iter().any(|entry| entry.id == id) {
                    id = format!("{base}-{suffix}");
                    suffix += 1;
                }
                entries.push(InstanceEntry {
                    id,
                    name: version.clone(),
                    root: root.clone(),
                    version,
                    isolated: true,
                    game_dir: None,
                });
            }
        }
        entries
    }

    fn refresh(&mut self, host: &mut HostHandle, logger: &Logger) {
        let roots = Self::roots(host);
        let existing = Self::entries(host);
        let entries = Self::import_versions(&roots, &existing);
        if entries != existing {
            logger.log(&format!(
                "instances refreshed: {} -> {}",
                existing.len(),
                entries.len()
            ));
            self.save(host, &roots, &entries);
            self.rebuild_tree(host);
        }
        self.rebuild_detail(host);
    }

    /* ---------------- settings data ---------------- */

    fn settings_snapshot(&mut self, host: &mut HostHandle) {
        let instance = self.selected.clone();
        let mut payload = json!({"action": "settings.get"});
        if let Some(id) = &instance {
            payload["instance"] = json!(id);
        }
        host.publish("launcher.command", payload);
    }

    fn setting_override(&self, key: &str) -> Option<Value> {
        self.overrides
            .get(key)
            .cloned()
            .filter(|value| !value.is_null())
    }

    fn setting_resolved(&self, key: &str) -> Option<Value> {
        self.resolved
            .get(key)
            .cloned()
            .filter(|value| !value.is_null())
    }

    fn setting_text(&self, host: &HostHandle, key: &str, entry: &InstanceEntry) -> String {
        let default = self.t(host, "instances.default");
        match key {
            "java_path" => {
                if let Some(value) = self.setting_override(key) {
                    return value.as_str().unwrap_or_default().to_string();
                }
                let resolved = self
                    .setting_resolved(key)
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| self.t(host, "instances.auto_java"));
                format!("{default} ({resolved})")
            }
            "memory_mb" => {
                if let Some(value) = self.setting_override(key) {
                    return format!("{} MB", value.as_u64().unwrap_or(0));
                }
                let resolved = self
                    .setting_resolved(key)
                    .and_then(|value| value.as_u64())
                    .unwrap_or(4096);
                format!("{default} ({resolved} MB)")
            }
            "jvm_args" | "game_args" => {
                if let Some(value) = self.setting_override(key) {
                    let text = value
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Json::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    return if text.is_empty() {
                        format!("({})", self.t(host, "instances.none"))
                    } else {
                        text
                    };
                }
                default
            }
            "game_dir" => entry
                .game_dir
                .clone()
                .unwrap_or_else(|| format!("{default} ({})", entry.game_dir().display())),
            "isolated" => {
                if entry.isolated {
                    self.t(host, "instances.on")
                } else {
                    self.t(host, "instances.off")
                }
            }
            _ => String::new(),
        }
    }

    fn options(&self, host: &HostHandle, key: &str) -> Vec<String> {
        let default = self.t(host, "instances.default");
        let manual = self.t(host, "instances.manual");
        match key {
            "java_path" => {
                let mut options = vec![default];
                for candidate in &self.java {
                    if candidate.get("valid").and_then(Json::as_bool) != Some(true) {
                        continue;
                    }
                    let path = candidate
                        .get("path")
                        .and_then(Json::as_str)
                        .unwrap_or_default();
                    let major = candidate.get("major").and_then(Json::as_u64).unwrap_or(0);
                    options.push(format!("Java {major} · {path}"));
                }
                options.push(manual);
                options
            }
            "memory_mb" => {
                let mut options = vec![default];
                options.extend(MEMORY_PRESETS.iter().map(|memory| format!("{memory} MB")));
                options.push(manual);
                options
            }
            "jvm_args" | "game_args" | "game_dir" => vec![default, manual],
            "isolated" => vec![self.t(host, "instances.on"), self.t(host, "instances.off")],
            _ => Vec::new(),
        }
    }

    /* ---------------- UI ---------------- */

    fn rebuild_tree(&mut self, host: &HostHandle) {
        Self::remove_paths(host, &mut self.tree_nodes);
        let roots = Self::roots(host);
        let entries = Self::entries(host);
        if roots.is_empty() && entries.is_empty() {
            let path = format!("{ROOTS}/empty");
            if self.set(
                host,
                &path,
                "Item",
                json!({"text": self.t(host, "instances.empty"), "color": "#9AA0A6"}),
            ) {
                self.tree_nodes.push(path);
            }
            return;
        }
        for (index, root) in roots.iter().enumerate() {
            let collapsed = self.collapsed.contains(root);
            let glyph = if collapsed { "▸" } else { "▾" };
            let name = Path::new(root)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(root);
            let path = format!("{ROOTS}/h-{index}");
            if self.set(
                host,
                &path,
                "Item",
                json!({"text": format!("{glyph} {name}  {root}")}),
            ) {
                self.tree_nodes.push(path);
            }
            if collapsed {
                continue;
            }
            for entry in entries.iter().filter(|entry| &entry.root == root) {
                self.instance_row(host, entry);
            }
        }
        // legacy or unconfigured entries (empty root, or a root no longer listed)
        let unconfigured: Vec<&InstanceEntry> = entries
            .iter()
            .filter(|entry| !roots.contains(&entry.root))
            .collect();
        if !unconfigured.is_empty() {
            let path = format!("{ROOTS}/h-legacy");
            if self.set(
                host,
                &path,
                "Item",
                json!({"text": self.t(host, "instances.unconfigured"), "color": "#9AA0A6"}),
            ) {
                self.tree_nodes.push(path);
            }
            for entry in unconfigured {
                self.instance_row(host, entry);
            }
        }
    }

    fn instance_row(&mut self, host: &HostHandle, entry: &InstanceEntry) {
        let path = format!("{ROOTS}/p-{}", entry.id);
        let (dot, color) = self.state_dot(entry);
        let tag = Self::loader_for(entry)
            .map(|info| format!("  [{}]", info.kind.label()))
            .unwrap_or_default();
        let text = format!("  {dot} {}{tag}", entry.name);
        if self.set(host, &path, "Item", json!({"text": text, "color": color})) {
            self.tree_nodes.push(path);
        }
    }

    fn loader_for(entry: &InstanceEntry) -> Option<LoaderInfo> {
        let text = std::fs::read_to_string(entry.version_json()).ok()?;
        let json: Value = serde_json::from_str(&text).ok()?;
        detect_loader(&json, &entry.version)
    }

    fn state_dot(&self, entry: &InstanceEntry) -> (&'static str, &'static str) {
        if !entry.installed() {
            return ("!", "red");
        }
        match self.states.get(&entry.id) {
            Some(state) if state.get("state").and_then(Json::as_str) == Some("running") => {
                ("●", "green")
            }
            Some(state) if state.get("state").and_then(Json::as_str) == Some("error") => {
                ("●", "red")
            }
            Some(_) => ("●", "#9AA0A6"),
            None => ("○", "#9AA0A6"),
        }
    }

    fn rebuild_detail(&mut self, host: &HostHandle) {
        Self::remove_paths(host, &mut self.detail_nodes);
        Self::remove_paths(host, &mut self.log_nodes);
        let Some(entry) = self.selected_entry(host) else {
            let _ = (host.ui().patch)(
                ACTOR,
                DETAIL,
                json!({"title": self.t(host, "instances.detail")}),
            );
            let path = format!("{DETAIL}/hint");
            if self.set(
                host,
                &path,
                "Label",
                json!({"text": self.t(host, "instances.hint"), "align": "center"}),
            ) {
                self.detail_nodes.push(path);
            }
            return;
        };
        let _ = (host.ui().patch)(ACTOR, DETAIL, json!({"title": entry.name}));
        let narrow = self.page_width.is_some_and(|width| width < 80);
        // running instance tabs
        let running: Vec<InstanceEntry> = Self::entries(host)
            .into_iter()
            .filter(|item| {
                self.states
                    .get(&item.id)
                    .and_then(|state| state.get("state"))
                    .and_then(Json::as_str)
                    == Some("running")
            })
            .collect();
        if !running.is_empty() {
            if self.set(
                host,
                TABS,
                "Split",
                json!({"direction": "row", "flex": 1.0}),
            ) {
                self.detail_nodes.push(TABS.to_string());
            }
            for item in running.iter().take(6) {
                let path = format!("{TABS}/t-{}", item.id);
                let color = if self.selected.as_deref() == Some(item.id.as_str()) {
                    "#56B6C2"
                } else {
                    "white"
                };
                if self.set(
                    host,
                    &path,
                    "Button",
                    json!({"text": item.name, "color": color}),
                ) {
                    self.detail_nodes.push(path);
                }
            }
        }
        // status rows
        if self.set(
            host,
            STATUS,
            "List",
            json!({"title": self.t(host, "instances.status"), "flex": 2.0}),
        ) {
            self.detail_nodes.push(STATUS.to_string());
        }
        let state_text = self.state_text(host, &entry);
        let loader = Self::loader_for(&entry);
        let loader_text = loader
            .as_ref()
            .map(|info| {
                info.version
                    .as_ref()
                    .map(|version| format!("{} {version}", info.kind.label()))
                    .unwrap_or_else(|| info.kind.label().to_string())
            })
            .unwrap_or_else(|| self.t(host, "instances.loader.none"));
        let rows = [
            format!("{}  {}", self.t(host, "instances.state"), state_text),
            format!("{}  {}", self.t(host, "instances.version"), entry.version),
            format!("{}  {loader_text}", self.t(host, "instances.loader")),
            format!("{}  {}", self.t(host, "instances.root"), entry.root),
            format!(
                "{}  {}",
                self.t(host, "instances.game_dir"),
                entry.game_dir().display()
            ),
            format!(
                "{}  {}",
                self.t(host, "instances.mods"),
                entry.game_dir().join("mods").display()
            ),
            format!(
                "{}  {}",
                self.t(host, "instances.isolation"),
                self.setting_text(host, "isolated", &entry)
            ),
            format!(
                "{}  {}",
                self.t(host, "instances.java"),
                self.setting_text(host, "java_path", &entry)
            ),
            format!(
                "{}  {}",
                self.t(host, "instances.memory"),
                self.setting_text(host, "memory_mb", &entry)
            ),
        ];
        for (index, text) in rows.iter().enumerate() {
            let path = format!("{STATUS}/s-{index}");
            if self.set(host, &path, "Item", json!({"text": text})) {
                self.detail_nodes.push(path);
            }
        }
        // actions
        if self.set(
            host,
            ACTIONS,
            "Split",
            json!({"direction": "row", "flex": 1.0}),
        ) {
            self.detail_nodes.push(ACTIONS.to_string());
        }
        let running_now = self
            .states
            .get(&entry.id)
            .and_then(|state| state.get("state"))
            .and_then(Json::as_str)
            == Some("running");
        let (action_path, action_text) = if running_now {
            (format!("{ACTIONS}/stop"), self.t(host, "instances.stop"))
        } else {
            (
                format!("{ACTIONS}/launch"),
                self.t(host, "instances.launch"),
            )
        };
        if self.set(
            host,
            &action_path,
            "Button",
            json!({"text": action_text, "flex": 1.0}),
        ) {
            self.detail_nodes.push(action_path);
        }
        let forget_path = format!("{ACTIONS}/forget");
        if self.set(
            host,
            &forget_path,
            "Button",
            json!({"text": self.t(host, "instances.forget"), "flex": 1.0}),
        ) {
            self.detail_nodes.push(forget_path);
        }
        // settings rows
        if !narrow {
            if self.set(
                host,
                SETTINGS_LIST,
                "List",
                json!({"title": self.t(host, "instances.settings"), "flex": 5.0}),
            ) {
                self.detail_nodes.push(SETTINGS_LIST.to_string());
            }
            let row_count = SETTING_KEYS.len() + 1;
            let mut row_index = 0;
            for key in SETTING_KEYS {
                row_index += 1;
                let glyph = if row_index == row_count {
                    "└─"
                } else {
                    "├─"
                };
                let path = format!("{SETTINGS_LIST}/r-{key}");
                let label = self.t(host, &format!("instances.setting.{key}"));
                let text = format!(
                    "{glyph} {label:<14}{}",
                    self.setting_text(host, key, &entry)
                );
                if self.set(host, &path, "Item", json!({"text": text})) {
                    self.detail_nodes.push(path);
                }
                if *key == "java_path" {
                    row_index += 1;
                    let memory = self
                        .setting_resolved("memory_mb")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(4096);
                    let glyph = if row_index == row_count {
                        "└─"
                    } else {
                        "├─"
                    };
                    let text =
                        format!("{glyph} {}|{} MB", self.t(host, "instances.memory"), memory);
                    if self.set(
                        host,
                        MEMORY_SLIDER,
                        "Slider",
                        json!({"value": memory_ratio(memory), "text": text}),
                    ) {
                        self.detail_nodes.push(MEMORY_SLIDER.to_string());
                    }
                }
                if self.expanded.as_deref() == Some(*key) {
                    for (index, option) in self.options(host, key).iter().enumerate() {
                        let path = format!("{SETTINGS_LIST}/o-{key}-{index}");
                        if self.set(
                            host,
                            &path,
                            "Item",
                            json!({"text": format!("   ├─ {option}"), "color": "#9AA0A6"}),
                        ) {
                            self.detail_nodes.push(path);
                        }
                    }
                }
            }
        }
        // log window
        if !narrow {
            if self.set(
                host,
                LOG,
                "List",
                json!({"title": self.t(host, "instances.log"), "flex": 4.0}),
            ) {
                self.detail_nodes.push(LOG.to_string());
            }
            self.rebuild_log_nodes(host);
        }
        self.restore_detail_focus(host);
    }

    fn rebuild_log_nodes(&mut self, host: &HostHandle) {
        Self::remove_paths(host, &mut self.log_nodes);
        let Some(id) = self.selected.clone() else {
            return;
        };
        let lines = self.logs.get(&id).cloned().unwrap_or_default();
        let start = lines.len().saturating_sub(50);
        for (index, line) in lines[start..].iter().enumerate() {
            let path = format!("{LOG}/l-{index}");
            if self.set(
                host,
                &path,
                "Item",
                json!({"text": line, "color": log_color(line)}),
            ) {
                self.log_nodes.push(path);
            }
        }
    }

    fn state_text(&self, host: &HostHandle, entry: &InstanceEntry) -> String {
        if !entry.installed() {
            return format!("! {}", self.t(host, "instances.missing"));
        }
        match self.states.get(&entry.id) {
            Some(state) => {
                let kind = state.get("state").and_then(Json::as_str).unwrap_or("");
                match kind {
                    "running" => format!(
                        "● {} (pid {})",
                        self.t(host, "instances.running"),
                        state.get("pid").and_then(Json::as_u64).unwrap_or(0)
                    ),
                    "exited" => format!(
                        "● {} ({:?})",
                        self.t(host, "instances.exited"),
                        state.get("code").and_then(Json::as_i64)
                    ),
                    "stopped" => format!("● {}", self.t(host, "instances.stopped")),
                    "error" => format!(
                        "● {} ({})",
                        self.t(host, "instances.error"),
                        state.get("error").and_then(Json::as_str).unwrap_or("?")
                    ),
                    _ => format!("○ {}", self.t(host, "instances.idle")),
                }
            }
            None => format!("○ {}", self.t(host, "instances.idle")),
        }
    }

    fn restore_detail_focus(&self, host: &HostHandle) {
        let old = self.last_focus.clone();
        if let Some(path) = old
            && path.starts_with("/instances/")
            && (host.ui().set_focus)(ACTOR, &path)
        {
            return;
        }
        if let Some(id) = &self.selected {
            let _ = (host.ui().set_focus)(ACTOR, &format!("{ROOTS}/p-{id}"));
        }
    }

    fn show_dialog(&self, host: &HostHandle) {
        let Some(dialog) = &self.dialog else {
            let _ = (host.ui().remove)(ACTOR, DIALOG);
            return;
        };
        let ui = host.ui();
        let _ = (ui.remove)(ACTOR, DIALOG);
        let _ = (ui.create)(
            ACTOR,
            DIALOG,
            "Popup",
            json!({"title": self.t(host, "instances.dialog.title")}),
        );
        let hint = match &dialog.kind {
            DialogKind::AddRoot => self.t(host, "instances.dialog.add_root"),
            DialogKind::ForgetRoot(_) => self.t(host, "instances.dialog.forget"),
            DialogKind::Setting(key) => self.t(host, &format!("instances.setting.{key}")),
        };
        let _ = (ui.create)(
            ACTOR,
            &format!("{DIALOG}/hint"),
            "Label",
            json!({"text": hint, "flex": 1.0}),
        );
        if !matches!(dialog.kind, DialogKind::ForgetRoot(_)) {
            let row = format!("{DIALOG}/row");
            let _ = (ui.create)(
                ACTOR,
                &row,
                "Split",
                json!({"direction": "row", "flex": 1.0}),
            );
            if self.picker_mode().is_some() {
                let _ = (ui.create)(
                    ACTOR,
                    DIALOG_BROWSE,
                    "Button",
                    json!({"text": "📁", "flex": 1.0}),
                );
            }
            let _ = (ui.create)(
                ACTOR,
                DIALOG_INPUT,
                "TextInput",
                json!({"text": dialog.input, "flex": 4.0}),
            );
        }
        let _ = (ui.create)(
            ACTOR,
            DIALOG_OK,
            "Button",
            json!({"text": self.t(host, "instances.dialog.ok"), "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            DIALOG_CANCEL,
            "Button",
            json!({"text": self.t(host, "instances.dialog.cancel"), "flex": 1.0}),
        );
    }

    fn open_dialog(&mut self, host: &HostHandle, kind: DialogKind, input: String) {
        self.dialog = Some(Dialog { kind, input });
        self.show_dialog(host);
        let _ = (host.ui().set_focus)(ACTOR, DIALOG_INPUT);
    }

    fn picker_mode(&self) -> Option<PickerMode> {
        match &self.dialog.as_ref()?.kind {
            DialogKind::AddRoot => Some(PickerMode::Dir),
            DialogKind::Setting(key) if key == "java_path" => Some(PickerMode::File),
            DialogKind::Setting(key) if key == "game_dir" => Some(PickerMode::Dir),
            _ => None,
        }
    }

    fn close_dialog(&mut self, host: &HostHandle) {
        self.dialog = None;
        let _ = (host.ui().remove)(ACTOR, DIALOG);
        self.rebuild_detail(host);
    }

    fn command(&self, host: &mut HostHandle, payload: Value) {
        host.publish("launcher.command", payload);
    }

    /* ---------------- settings writes ---------------- */

    fn set_instance_setting(&self, host: &mut HostHandle, key: &str, value: Value) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        host.publish(
            "launcher.command",
            json!({"action": "settings.set", "scope": "instance", "instance": id, "key": key, "value": value}),
        );
    }

    fn apply_option(&mut self, host: &mut HostHandle, key: &str, index: usize) {
        let Some(entry) = self.selected_entry(host) else {
            return;
        };
        match key {
            "isolated" => {
                let isolated = index == 0;
                self.update_entry(host, &entry.id, |entry| entry.isolated = isolated);
                self.rebuild_tree(host);
                self.rebuild_detail(host);
            }
            "game_dir" => {
                if index == 0 {
                    self.update_entry(host, &entry.id, |entry| entry.game_dir = None);
                    self.rebuild_detail(host);
                } else {
                    let current = entry.game_dir.clone().unwrap_or_default();
                    self.open_dialog(host, DialogKind::Setting("game_dir".into()), current);
                }
            }
            "java_path" => {
                let options = self.options(host, key);
                if index == 0 {
                    self.set_instance_setting(host, key, Value::Null);
                } else if index + 1 == options.len() {
                    self.open_dialog(host, DialogKind::Setting(key.into()), String::new());
                } else {
                    let mut candidate_index = 0;
                    for candidate in &self.java {
                        if candidate.get("valid").and_then(Json::as_bool) != Some(true) {
                            continue;
                        }
                        candidate_index += 1;
                        if candidate_index == index {
                            let path = candidate
                                .get("path")
                                .and_then(Json::as_str)
                                .unwrap_or_default();
                            self.set_instance_setting(host, key, json!(path));
                            break;
                        }
                    }
                }
            }
            "memory_mb" => {
                let options = self.options(host, key);
                if index == 0 {
                    self.set_instance_setting(host, key, Value::Null);
                } else if index + 1 == options.len() {
                    self.open_dialog(host, DialogKind::Setting(key.into()), String::new());
                } else if let Some(memory) = MEMORY_PRESETS.get(index - 1) {
                    self.set_instance_setting(host, key, json!(memory));
                }
            }
            "jvm_args" | "game_args" => {
                if index == 0 {
                    self.set_instance_setting(host, key, Value::Null);
                } else {
                    let current = self
                        .setting_override(key)
                        .and_then(|value| {
                            value.as_array().map(|items| {
                                items
                                    .iter()
                                    .filter_map(Json::as_str)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                        })
                        .unwrap_or_default();
                    self.open_dialog(host, DialogKind::Setting(key.into()), current);
                }
            }
            _ => {}
        }
    }

    fn commit_dialog(&mut self, host: &mut HostHandle, logger: &Logger) {
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        match dialog.kind {
            DialogKind::AddRoot => {
                let path = PathBuf::from(dialog.input.trim());
                if !path.is_dir() {
                    logger.log(&format!("add root rejected: {}", path.display()));
                    return;
                }
                let resolved = normalize_path(&path.canonicalize().unwrap_or(path));
                let text = resolved.display().to_string();
                let mut roots = Self::roots(host);
                if !roots.contains(&text) {
                    roots.push(text.clone());
                    let entries = Self::import_versions(&roots, &Self::entries(host));
                    self.save(host, &roots, &entries);
                    logger.log(&format!("root added: {text}"));
                }
                self.close_dialog(host);
                self.rebuild_tree(host);
                self.rebuild_detail(host);
            }
            DialogKind::ForgetRoot(root) => {
                let mut roots = Self::roots(host);
                roots.retain(|item| item != &root);
                let entries: Vec<InstanceEntry> = Self::entries(host)
                    .into_iter()
                    .filter(|entry| entry.root != root)
                    .collect();
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|id| !entries.iter().any(|entry| &entry.id == id))
                {
                    self.selected = None;
                }
                self.save(host, &roots, &entries);
                self.close_dialog(host);
                self.rebuild_tree(host);
                self.rebuild_detail(host);
            }
            DialogKind::Setting(key) => {
                let input = dialog.input.trim().to_string();
                match key.as_str() {
                    "game_dir" => {
                        if let Some(entry) = self.selected.clone() {
                            let relative = input
                                .trim_start_matches(|character: char| {
                                    character == '/' || character == std::path::MAIN_SEPARATOR
                                })
                                .to_string();
                            let value = if relative.is_empty() {
                                None
                            } else {
                                Some(relative)
                            };
                            self.update_entry(host, &entry, |entry| entry.game_dir = value);
                        }
                    }
                    "memory_mb" => match input.parse::<u64>() {
                        Ok(memory) if memory >= 512 => {
                            self.set_instance_setting(host, &key, json!(memory))
                        }
                        _ => logger.log("memory input rejected"),
                    },
                    "jvm_args" | "game_args" => {
                        let args: Vec<String> =
                            input.split_whitespace().map(str::to_string).collect();
                        self.set_instance_setting(host, &key, json!(args));
                    }
                    _ => self.set_instance_setting(host, &key, json!(input)),
                }
                self.close_dialog(host);
            }
        }
    }

    fn forget_root(&mut self, host: &mut HostHandle, root: String) {
        self.dialog = Some(Dialog {
            kind: DialogKind::ForgetRoot(root),
            input: String::new(),
        });
        self.show_dialog(host);
    }
}

impl NativePlugin for Instances {
    fn manifest(&self) -> Manifest {
        manifest(
            "instances",
            "Instances",
            &[
                "instances.command",
                "launcher.state",
                "launcher.log",
                "launcher.settings",
                "launcher.java",
                "launcher.error",
            ],
            "Owns the game instance registry, .minecraft root discovery and the /instances page",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let discovered = host
            .config()
            .borrow()
            .get("instances", "discovered")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let mut roots = Self::roots(host);
        if roots.is_empty() && !discovered {
            roots = Self::discover_common_roots();
            if !roots.is_empty() {
                logger.log(&format!("discovered .minecraft roots: {}", roots.len()));
            }
        }
        let entries = Self::import_versions(&roots, &Self::entries(host));
        self.save(host, &roots, &entries);
        host.config()
            .borrow_mut()
            .set("instances", "discovered", json!(true));
        let ui = host.ui();
        let _ = (ui.create)(ACTOR, PAGE, "Page", json!({"title": "Instances"}));
        let _ = (ui.create)(ACTOR, SPLIT, "Split", json!({"direction": "row"}));
        let _ = (ui.create)(
            ACTOR,
            TREE,
            "Split",
            json!({"direction": "col", "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            ROOTS,
            "List",
            json!({"title": "Instances", "flex": 4.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            TOOLBAR,
            "Split",
            json!({"direction": "row", "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            ADD_BUTTON,
            "Button",
            json!({"text": "＋", "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            REFRESH_BUTTON,
            "Button",
            json!({"text": "⟳", "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            DETAIL,
            "Section",
            json!({"title": self.t(host, "instances.detail"), "flex": 3.0}),
        );
        let _ = (ui.create)(ACTOR, LAYOUT, "Split", json!({"direction": "col"}));
        self.rebuild_tree(host);
        self.rebuild_detail(host);
        self.settings_snapshot(host);
        host.publish("launcher.command", json!({"action": "java.detect"}));
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        match name {
            "ui.blur" => {
                if let Some(path) = payload.get("path").and_then(Json::as_str)
                    && self.last_focus.as_deref() == Some(path)
                {
                    self.last_focus = None;
                }
            }
            "instances.command" => {
                let action = payload.get("action").and_then(Json::as_str).unwrap_or("");
                let roots = Self::roots(host);
                let mut entries = Self::entries(host);
                match action {
                    "create" => {
                        let name = payload
                            .get("name")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if name.is_empty() || entries.iter().any(|entry| entry.name == name) {
                            return;
                        }
                        let mut counter = 1;
                        let id = loop {
                            let candidate = format!("inst-{counter}");
                            if !entries.iter().any(|entry| entry.id == candidate) {
                                break candidate;
                            }
                            counter += 1;
                        };
                        entries.push(InstanceEntry {
                            id,
                            name,
                            root: String::new(),
                            version: String::new(),
                            isolated: true,
                            game_dir: None,
                        });
                        self.save(host, &roots, &entries);
                        self.rebuild_tree(host);
                    }
                    "delete" => {
                        let Some(id) = payload.get("id").and_then(Json::as_str) else {
                            return;
                        };
                        entries.retain(|entry| entry.id != id);
                        if self.selected.as_deref() == Some(id) {
                            self.selected = None;
                        }
                        self.save(host, &roots, &entries);
                        self.rebuild_tree(host);
                        self.rebuild_detail(host);
                    }
                    _ => {}
                }
            }
            "ui.focus" => {
                let Some(path) = payload.get("path").and_then(Json::as_str) else {
                    return;
                };
                self.last_focus = Some(path.to_string());
                if let Some(id) = path.strip_prefix(&format!("{ROOTS}/p-"))
                    && self.selected.as_deref() != Some(id)
                {
                    self.selected = Some(id.to_string());
                    self.settings_snapshot(host);
                    self.rebuild_detail(host);
                }
            }
            "ui.input" => {
                if payload.get("path").and_then(Json::as_str) == Some(MEMORY_SLIDER)
                    && let Some(value) = payload.get("value").and_then(Json::as_f64)
                {
                    let memory = memory_from_ratio(value as f32);
                    self.set_instance_setting(host, "memory_mb", json!(memory));
                    let text = format!("├─ {}|{} MB", self.t(host, "instances.memory"), memory);
                    let _ = (host.ui().patch)(ACTOR, MEMORY_SLIDER, json!({"text": text}));
                    return;
                }
                if payload.get("path").and_then(Json::as_str) == Some(DIALOG_INPUT)
                    && let Some(dialog) = &mut self.dialog
                {
                    dialog.input = payload
                        .get("text")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            "ui.edit" => {
                if payload.get("path").and_then(Json::as_str) == Some(MEMORY_SLIDER) {
                    let memory = self
                        .setting_resolved("memory_mb")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(4096);
                    self.open_dialog(
                        host,
                        DialogKind::Setting("memory_mb".into()),
                        memory.to_string(),
                    );
                }
            }
            "ui.resize" => {
                if payload.get("page").and_then(Json::as_str) != Some(PAGE) {
                    return;
                }
                let Some(width) = payload.get("width").and_then(Json::as_u64) else {
                    return;
                };
                let width = width as u16;
                if self.page_width != Some(width) {
                    self.page_width = Some(width);
                    self.rebuild_detail(host);
                }
            }
            "ui.activate" => {
                let Some(path) = payload.get("path").and_then(Json::as_str) else {
                    return;
                };
                if path.starts_with(&format!("{PICKER}/")) {
                    if path == format!("{PICKER}/cancel") {
                        self.picker = None;
                        Picker::close(host, ACTOR, PICKER);
                        self.show_dialog(host);
                        return;
                    }
                    let Some(mut picker) = self.picker.take() else {
                        return;
                    };
                    let title = self.t(host, "instances.dialog.title");
                    if let Some(selected) = picker.activate(host, ACTOR, PICKER, path, &title) {
                        if let Some(dialog) = &mut self.dialog {
                            dialog.input = selected.display().to_string();
                        }
                        Picker::close(host, ACTOR, PICKER);
                        self.show_dialog(host);
                        let _ = (host.ui().set_focus)(ACTOR, DIALOG_INPUT);
                    } else {
                        self.picker = Some(picker);
                    }
                    return;
                }
                if path == DIALOG_BROWSE {
                    if let Some(mode) = self.picker_mode() {
                        let start = self.dialog.as_ref().map(|dialog| dialog.input.clone());
                        let title = self.t(host, "instances.dialog.title");
                        let picker = Picker::new(mode, start.as_deref());
                        picker.show(host, ACTOR, PICKER, &title);
                        self.picker = Some(picker);
                    }
                    return;
                }
                if path == ADD_BUTTON {
                    self.open_dialog(host, DialogKind::AddRoot, String::new());
                    return;
                }
                if path == REFRESH_BUTTON {
                    self.refresh(host, logger);
                    return;
                }
                if path == DIALOG_OK {
                    self.commit_dialog(host, logger);
                    return;
                }
                if path == DIALOG_CANCEL {
                    self.close_dialog(host);
                    return;
                }
                if let Some(index) = path.strip_prefix(&format!("{ROOTS}/h-")) {
                    let Ok(index) = index.parse::<usize>() else {
                        return;
                    };
                    let roots = Self::roots(host);
                    let Some(root) = roots.get(index).cloned() else {
                        return;
                    };
                    if !self.collapsed.remove(&root) {
                        self.collapsed.insert(root);
                    }
                    self.rebuild_tree(host);
                    let _ = (host.ui().set_focus)(ACTOR, path);
                    return;
                }
                if let Some(id) = path.strip_prefix(&format!("{ROOTS}/p-")) {
                    self.selected = Some(id.to_string());
                    self.settings_snapshot(host);
                    self.rebuild_detail(host);
                    return;
                }
                if let Some(id) = path.strip_prefix(&format!("{TABS}/t-")) {
                    self.selected = Some(id.to_string());
                    self.settings_snapshot(host);
                    self.rebuild_detail(host);
                    return;
                }
                if path == format!("{ACTIONS}/launch") {
                    if let Some(id) = self.selected.clone() {
                        self.command(host, json!({"action": "launch", "instance": id}));
                    }
                    return;
                }
                if path == format!("{ACTIONS}/stop") {
                    if let Some(id) = self.selected.clone() {
                        self.command(host, json!({"action": "stop", "instance": id}));
                    }
                    return;
                }
                if path == format!("{ACTIONS}/forget") {
                    if let Some(entry) = self.selected_entry(host) {
                        self.forget_root(host, entry.root);
                    }
                    return;
                }
                if let Some(key) = path.strip_prefix(&format!("{SETTINGS_LIST}/r-")) {
                    self.expanded = if self.expanded.as_deref() == Some(key) {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    self.rebuild_detail(host);
                    let _ = (host.ui().set_focus)(ACTOR, path);
                    return;
                }
                if let Some(tail) = path.strip_prefix(&format!("{SETTINGS_LIST}/o-")) {
                    let Some((key, index)) = tail.rsplit_once('-') else {
                        return;
                    };
                    let Ok(index) = index.parse::<usize>() else {
                        return;
                    };
                    self.expanded = None;
                    self.apply_option(host, key, index);
                }
            }
            "launcher.state" => {
                let Some(id) = payload.get("instance").and_then(Json::as_str) else {
                    return;
                };
                self.states.insert(id.to_string(), payload.clone());
                self.rebuild_tree(host);
                if self.selected.as_deref() == Some(id) {
                    self.rebuild_detail(host);
                }
            }
            "launcher.log" => {
                let Some(id) = payload.get("instance").and_then(Json::as_str) else {
                    return;
                };
                let reset = payload
                    .get("reset")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let lines: Vec<String> = payload
                    .get("lines")
                    .and_then(Json::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Json::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let log = self.logs.entry(id.to_string()).or_default();
                if reset {
                    log.clear();
                }
                log.extend(lines);
                if log.len() > 200 {
                    let drain = log.len() - 200;
                    log.drain(..drain);
                }
                if self.selected.as_deref() == Some(id) {
                    self.rebuild_log_nodes(host);
                }
            }
            "launcher.settings" => {
                if let Some(instance) = payload.get("instance").and_then(Json::as_str) {
                    if self.selected.as_deref() != Some(instance) {
                        return;
                    }
                    self.overrides = payload
                        .get("instance")
                        .and_then(|value| value.get("overrides"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.resolved = payload
                        .get("instance")
                        .and_then(|value| value.get("resolved"))
                        .cloned()
                        .unwrap_or(Value::Null);
                }
                self.global = payload.get("global").cloned().unwrap_or(Value::Null);
                self.rebuild_detail(host);
            }
            "launcher.java" => {
                self.java = payload
                    .get("candidates")
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                if self.expanded.as_deref() == Some("java_path") {
                    self.rebuild_detail(host);
                }
            }
            "launcher.error" => {
                if let Some(id) = payload.get("instance").and_then(Json::as_str) {
                    self.states.insert(id.to_string(), payload.clone());
                    logger.log(&format!(
                        "instance error: {} {}",
                        id,
                        payload
                            .get("message")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                    ));
                    self.rebuild_tree(host);
                    if self.selected.as_deref() == Some(id) {
                        self.rebuild_detail(host);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, _host: &mut HostHandle, _logger: &Logger) {}
}

fn memory_ratio(memory: u64) -> f32 {
    ((memory.saturating_sub(512)) as f32 / (16384.0 - 512.0)).clamp(0.0, 1.0)
}

fn memory_from_ratio(ratio: f32) -> u64 {
    let memory = 512.0 + ratio.clamp(0.0, 1.0) * (16384.0 - 512.0);
    ((memory / 512.0).round() as u64 * 512).clamp(512, 16384)
}

fn log_color(line: &str) -> &'static str {
    let upper = line.to_ascii_uppercase();
    if upper.contains("ERROR")
        || upper.contains("FATAL")
        || upper.contains("EXCEPTION")
        || upper.contains("SEVERE")
        || upper.contains("CRASHED")
    {
        "red"
    } else if upper.contains("WARN") || upper.contains("FALLBACK") {
        "yellow"
    } else if upper.contains("DEBUG") || upper.contains("TRACE") {
        "#6B7280"
    } else {
        "#9AA0A6"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("terminism-instances-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("versions/1.20.1")).unwrap();
        std::fs::write(dir.join("versions/1.20.1/1.20.1.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("versions/1.21")).unwrap();
        std::fs::write(dir.join("versions/1.21/1.21.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("versions/not-a-version")).unwrap();
        dir
    }

    #[test]
    fn scans_only_version_directories_with_json() {
        let root = temp_root("scan");
        let versions = Instances::scan_versions(root.to_str().unwrap());
        assert_eq!(versions, vec!["1.20.1", "1.21"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_is_stable_and_deduplicated() {
        let root = temp_root("import");
        let roots = vec![root.display().to_string()];
        let first = Instances::import_versions(&roots, &[]);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].id, Instances::stable_id(&roots[0], "1.20.1"));
        let second = Instances::import_versions(&roots, &first);
        assert_eq!(second, first);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn loader_json(libraries: &[&str], args: &[&str]) -> Value {
        serde_json::json!({
            "libraries": libraries.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
            "arguments": {"game": args},
        })
    }

    #[test]
    fn detects_fabric_forge_neoforge_and_liteloader() {
        let fabric = detect_loader(
            &loader_json(&["net.fabricmc:fabric-loader:0.18.4"], &[]),
            "fabric-loader-0.18.4-1.21.11",
        )
        .unwrap();
        assert_eq!(fabric.kind, LoaderKind::Fabric);
        assert_eq!(fabric.version.as_deref(), Some("0.18.4"));

        let forge = detect_loader(
            &loader_json(
                &["net.minecraftforge:fmlloader:1.20.1-47.4.22"],
                &[
                    "--launchTarget",
                    "forgeclient",
                    "--fml.forgeVersion",
                    "47.4.22",
                ],
            ),
            "1.20.1-Forge_47.4.22",
        )
        .unwrap();
        assert_eq!(forge.kind, LoaderKind::Forge);
        assert_eq!(forge.version.as_deref(), Some("47.4.22"));

        let neoforge = detect_loader(
            &loader_json(
                &["net.neoforged.fancymodloader:loader:4.0.42"],
                &[
                    "--launchTarget",
                    "forgeclient",
                    "--fml.neoForgeVersion",
                    "21.1.77",
                ],
            ),
            "Aeronautics - Create Customised",
        )
        .unwrap();
        assert_eq!(neoforge.kind, LoaderKind::NeoForge);
        assert_eq!(neoforge.version.as_deref(), Some("21.1.77"));

        let liteloader = detect_loader(
            &loader_json(
                &["com.mumfrey:liteloader:1.12.2"],
                &[
                    "--tweakClass",
                    "com.mumfrey.liteloader.launch.LiteLoaderTweaker",
                ],
            ),
            "1.12.2-LiteLoader1.12.2",
        )
        .unwrap();
        assert_eq!(liteloader.kind, LoaderKind::LiteLoader);
        assert_eq!(liteloader.version.as_deref(), Some("1.12.2"));
    }

    #[test]
    fn vanilla_has_no_loader_and_names_fall_back() {
        assert_eq!(detect_loader(&loader_json(&[], &[]), "1.20.1"), None);
        let fabric = detect_loader(&loader_json(&[], &[]), "fabric-loader-0.15.11-1.20.1").unwrap();
        assert_eq!(fabric.kind, LoaderKind::Fabric);
        assert_eq!(fabric.version, None);
        let forge = detect_loader(&loader_json(&[], &[]), "1.20.1-forge-47.2.0").unwrap();
        assert_eq!(forge.kind, LoaderKind::Forge);
        let neoforge = detect_loader(&loader_json(&[], &[]), "neoforge-21.1.77").unwrap();
        assert_eq!(neoforge.kind, LoaderKind::NeoForge);
        let liteloader = detect_loader(&loader_json(&[], &[]), "1.12.2-LiteLoader1.12.2").unwrap();
        assert_eq!(liteloader.kind, LoaderKind::LiteLoader);
    }

    #[test]
    fn verbatim_windows_paths_are_normalized() {
        assert_eq!(
            normalize_path_text(r"\\?\C:\prof\.minecraft"),
            r"C:\prof\.minecraft"
        );
        assert_eq!(
            normalize_path_text(r"\\?\UNC\server\share\.minecraft"),
            r"\\server\share\.minecraft"
        );
        let entry = InstanceEntry::from_json(&serde_json::json!({
            "id": "inst-1",
            "name": "demo",
            "root": r"\\?\C:\prof\.minecraft",
            "version": "1.20.1",
        }))
        .unwrap();
        assert_eq!(entry.root, r"C:\prof\.minecraft");
    }

    #[test]
    fn discovery_requires_a_versions_directory() {
        let root = temp_root("discover");
        let other = root.join("empty");
        std::fs::create_dir_all(&other).unwrap();
        let found = Instances::discover_from(vec![root.clone(), other]);
        assert_eq!(found.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
