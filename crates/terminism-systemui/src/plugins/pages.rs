use std::time::Instant;

use serde_json::json;
use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::file_picker::{Picker, PickerMode};

const LAUNCH_ROW: &str = "/settings/launch/row";
const LAUNCH_INPUT: &str = "/settings/launch/row/input";
const LAUNCH_BROWSE: &str = "/settings/launch/row/browse";
const PICKER: &str = "/settings/picker";

const ICON: &str = "\
┌───────┐
│       │
│  T_   │
│       │
└───────┘";

fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

pub struct SystemPages {
    // process start approximation, drives the home uptime row
    booted: Option<Instant>,
    // live text of the settings locale input, kept via ui.input events
    draft_locale: String,
    // settings tree node paths from the last rebuild
    tree_nodes: Vec<String>,
    // settings detail child paths from the last rebuild
    detail_nodes: Vec<String>,
    // category whose settings are laid out on the right, e.g. "Appearance"
    selected_category: Option<&'static str>,
    // the theme row is expanded inline with its option rows
    theme_open: bool,
    // true while focus sits inside /settings: config refreshes only rebuild
    // the page then, otherwise a boot-time override would steal focus
    in_settings: bool,
    // locale rows offered by the i18n service, and the locale row expansion
    locales: Vec<String>,
    locale_open: bool,
    // per-plugin settings contributed through settings.schema
    plugin_fields: Vec<PluginField>,
    // expanded "(default)" option rows of one plugin setting, keyed by plugin.key
    plugin_open: Option<String>,
    // launch settings category: expanded row, text dialog and java candidates
    launch_open: Option<String>,
    launch_dialog: Option<(String, String)>,
    picker: Option<Picker>,
    launch_java: Vec<Json>,
}

// one setting a plugin contributes to the Plugins category
#[derive(Clone)]
pub struct PluginField {
    plugin: String,
    key: String,
    label: String,
    kind: String,
    values: Vec<String>,
    default: String,
}

// opencode-style catalog: category, key, title; the kind is derived per key
// categories and titles carry catalog keys, resolved at render time
const SETTINGS: &[(&str, &str, &str)] = &[
    ("settings.appearance", "appearance.theme", "settings.theme"),
    ("settings.appearance", "appearance.lock", "settings.lock"),
    ("settings.general", "general.locale", "settings.locale"),
    ("settings.launch", "launch.java", "settings.java"),
    ("settings.launch", "launch.jvm", "settings.jvm"),
    ("settings.launch", "launch.game", "settings.game"),
    ("settings.launch", "launch.authlib", "settings.authlib"),
];

impl Default for SystemPages {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemPages {
    fn t(&self, host: &HostHandle, key: &str) -> String {
        host.i18n()
            .borrow()
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    pub fn new() -> Self {
        Self {
            booted: None,
            draft_locale: String::new(),
            tree_nodes: Vec::new(),
            detail_nodes: Vec::new(),
            selected_category: Some("settings.appearance"),
            theme_open: false,
            in_settings: false,
            locales: Vec::new(),
            locale_open: false,
            plugin_fields: Vec::new(),
            plugin_open: None,
            launch_open: None,
            launch_dialog: None,
            picker: None,
            launch_java: Vec::new(),
        }
    }

    // every installed theme: the factory look plus loaded styles.theme providers
    fn available_themes(&self, host: &HostHandle) -> Vec<String> {
        let mut themes = vec!["default-theme".to_string()];
        for info in host.plugins() {
            let provides = host
                .manifest_of(&info.id)
                .and_then(|m| m.get("provides").cloned())
                .unwrap_or_else(|| serde_json::json!([]));
            if provides
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "styles.theme"))
            {
                themes.push(info.id);
            }
        }
        themes
    }

    fn setting_read(&self, key: &str, host: &HostHandle) -> String {
        let store = host.config();
        let cfg = store.borrow();
        match key {
            "appearance.theme" => cfg
                .get("styles", "theme")
                .and_then(Json::as_str)
                .unwrap_or("default-theme")
                .to_string(),
            "appearance.lock" => {
                let locked = cfg
                    .get("system-ui", "layout")
                    .and_then(|j| j.get("locked"))
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                if locked { "on" } else { "off" }.to_string()
            }
            "general.locale" => cfg
                .get("i18n", "locale")
                .and_then(Json::as_str)
                .unwrap_or("en")
                .to_string(),
            "launch.java" => {
                let path = cfg
                    .get("launcher", "java_path")
                    .and_then(Json::as_str)
                    .unwrap_or("");
                if path.is_empty() {
                    self.t(host, "settings.auto")
                } else {
                    path.to_string()
                }
            }
            "launch.memory" => format!(
                "{} MB",
                cfg.get("launcher", "memory_mb")
                    .and_then(Json::as_u64)
                    .unwrap_or(4096)
            ),
            "launch.jvm" => cfg
                .get("launcher", "jvm_args")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Json::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| self.t(host, "settings.none")),
            "launch.authlib" => cfg
                .get("launcher", "authlib_jar")
                .and_then(Json::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| self.t(host, "settings.none")),
            "launch.game" => cfg
                .get("launcher", "game_args")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Json::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| self.t(host, "settings.none")),
            _ => "-".to_string(),
        }
    }

    // the tree lists categories only; picking one lays its settings out right
    fn rebuild_tree(&mut self, host: &HostHandle) {
        let ui = host.ui();
        let remove = &ui.remove;
        let create = &ui.create;
        for path in std::mem::take(&mut self.tree_nodes) {
            let _ = remove("system-pages", &path);
        }
        let mut categories: Vec<&str> = Vec::new();
        for (cat, _, _) in SETTINGS {
            if !categories.contains(cat) {
                categories.push(cat);
            }
        }
        if !self.plugin_fields.is_empty() {
            categories.push("settings.plugins");
        }
        let category_count = categories.len();
        for (index, &cat) in categories.iter().enumerate() {
            let glyph = if index + 1 == category_count {
                "└─"
            } else {
                "├─"
            };
            let path = format!("/settings/split/tree/c-{cat}");
            let icon = match cat {
                "settings.general" => "⌘",
                "settings.plugins" => "◫",
                "settings.launch" => "▶",
                _ => "◐",
            };
            let label = self.t(host, cat);
            let color = if self.selected_category == Some(cat) {
                "#56B6C2"
            } else {
                "white"
            };
            let _ = create(
                "system-pages",
                &path,
                "Item",
                json!({"text": format!("{glyph} {icon} {label}"), "color": color}),
            );
            self.tree_nodes.push(path);
        }
    }

    // each setting is one list row: title left, value right. The theme row
    // expands inline: its options become list rows right below it
    fn rebuild_setting_detail(&mut self, host: &HostHandle) {
        let ui = host.ui();
        let remove = &ui.remove;
        let create = &ui.create;
        for path in std::mem::take(&mut self.detail_nodes) {
            let _ = remove("system-pages", &path);
        }
        let Some(cat) = self.selected_category else {
            return;
        };
        const BODY: &str = "/settings/split/detail/body";
        let _ = create("system-pages", BODY, "Split", json!({"direction": "col"}));
        self.detail_nodes.push(BODY.into());
        const LIST: &str = "/settings/split/detail/body/list";
        let _ = create(
            "system-pages",
            LIST,
            "List",
            json!({"title": self.t(host, cat), "flex": 5.0}),
        );
        self.detail_nodes.push(LIST.into());
        if cat == "settings.plugins" {
            self.rebuild_plugin_rows(host, LIST);
            return;
        }
        let launch_slider = cat == "settings.launch";
        let base_rows = SETTINGS.iter().filter(|(c, _, _)| *c == cat).count();
        let row_count = base_rows + usize::from(launch_slider);
        let mut row_index = 0;
        for (_, key, title_key) in SETTINGS.iter().filter(|(c, _, _)| *c == cat) {
            row_index += 1;
            let glyph = if row_index == row_count {
                "└─"
            } else {
                "├─"
            };
            let title = self.t(host, title_key);
            let (value, mark) = match *key {
                "appearance.lock" => {
                    let on = self.setting_read(key, host) == "on";
                    (if on { "[x] on" } else { "[ ] off" }.to_string(), "")
                }
                "appearance.theme" => {
                    let caret = if self.theme_open { "v" } else { ">" };
                    (format!("{} {caret}", self.setting_read(key, host)), "")
                }
                "general.locale" => {
                    let caret = if self.locale_open { "v" } else { ">" };
                    (format!("{} {caret}", self.setting_read(key, host)), "")
                }
                "launch.java" | "launch.memory" => {
                    let caret = if self.launch_open.as_deref() == Some(*key) {
                        "v"
                    } else {
                        ">"
                    };
                    (format!("{} {caret}", self.setting_read(key, host)), "")
                }
                "launch.jvm" | "launch.game" | "launch.authlib" => {
                    (format!("{} >", self.setting_read(key, host)), "")
                }
                _ => (self.setting_read(key, host), ""),
            };
            let path = format!("{LIST}/r-{key}");
            let _ = create(
                "system-pages",
                &path,
                "Item",
                json!({"text": format!("{glyph} {mark}{title:<14}{value}")}),
            );
            self.detail_nodes.push(path);
            if *key == "general.locale" && self.locale_open {
                let current = self.setting_read(key, host);
                for (i, locale) in self.locales.clone().iter().enumerate() {
                    let is_current = *locale == current;
                    let sel = if is_current { "*" } else { " " };
                    let color = if is_current { "#56B6C2" } else { "#9AA0A6" };
                    let path = format!("{LIST}/l-{i}");
                    let _ = create(
                        "system-pages",
                        &path,
                        "Item",
                        json!({"text": format!("   ├─ {sel} {locale}"), "color": color}),
                    );
                    self.detail_nodes.push(path);
                }
            }
            if *key == "appearance.theme" && self.theme_open {
                let themes = self.available_themes(host);
                let current = self.setting_read(key, host);
                for (i, theme) in themes.iter().enumerate() {
                    let is_current = *theme == current;
                    let sel = if is_current { "*" } else { " " };
                    let color = if is_current { "#56B6C2" } else { "#9AA0A6" };
                    let path = format!("{LIST}/m-{i}");
                    let _ = create(
                        "system-pages",
                        &path,
                        "Item",
                        json!({"text": format!("   ├─ {sel} {theme}"), "color": color}),
                    );
                    self.detail_nodes.push(path);
                }
            }
            if launch_slider && *key == "launch.java" {
                row_index += 1;
                const SLIDER: &str = "/settings/split/detail/body/list/memory";
                let memory = host
                    .config()
                    .borrow()
                    .get("launcher", "memory_mb")
                    .and_then(Json::as_u64)
                    .unwrap_or(4096);
                let glyph = if row_index == row_count {
                    "└─"
                } else {
                    "├─"
                };
                let text = format!("{glyph} {}|{} MB", self.t(host, "settings.memory"), memory);
                let _ = create(
                    "system-pages",
                    SLIDER,
                    "Slider",
                    json!({"value": memory_ratio(memory), "text": text}),
                );
                self.detail_nodes.push(SLIDER.into());
            }
            if *key == "launch.java" && self.launch_open.as_deref() == Some(*key) {
                for (i, option) in self.launch_options(host, key).iter().enumerate() {
                    let path = format!("{LIST}/lo-{key}-{i}");
                    let _ = create(
                        "system-pages",
                        &path,
                        "Item",
                        json!({"text": format!("   ├─ {option}"), "color": "#9AA0A6"}),
                    );
                    self.detail_nodes.push(path);
                }
            }
        }
    }

    // layout holds order and flex too: clone it and flip locked only
    fn set_lock(&self, on: bool, host: &mut HostHandle) {
        let store = host.config();
        let mut layout = store
            .borrow()
            .get("system-ui", "layout")
            .cloned()
            .unwrap_or_else(|| json!({}));
        drop(store);
        if !layout.is_object() {
            layout = json!({});
        }
        layout["locked"] = json!(on);
        host.publish(
            "config.set",
            json!({"section": "system-ui", "key": "layout", "value": layout}),
        );
    }

    // one row per contributed plugin field; enum rows expand their options
    // with "(default)" first, bool rows show 默认/开/关, text stays read-only
    fn rebuild_plugin_rows(&mut self, host: &HostHandle, list: &str) {
        let create = &host.ui().create;
        let defaults = self.t(host, "settings.default");
        let on_word = self.t(host, "settings.on");
        let off_word = self.t(host, "settings.off");
        let fields = self.plugin_fields.clone();
        let field_count = fields.len();
        for (field_index, field) in fields.iter().enumerate() {
            let glyph = if field_index + 1 == field_count {
                "└─"
            } else {
                "├─"
            };
            let stored = host
                .config()
                .borrow()
                .get(&format!("plugins.{}", field.plugin), &field.key)
                .cloned();
            // the default chip shows the schema default so users see what
            // "unset" resolves to
            let defaults = if field.default.is_empty() {
                format!("({defaults})")
            } else {
                format!("({defaults}: {})", field.default)
            };
            let value = match field.kind.as_str() {
                "bool" => match stored {
                    Some(Json::Bool(true)) => format!("[x] {on_word}"),
                    Some(Json::Bool(false)) => format!("[ ] {off_word}"),
                    _ => defaults.clone(),
                },
                "enum" => stored
                    .as_ref()
                    .and_then(Json::as_str)
                    .map_or_else(|| defaults.clone(), str::to_string),
                _ => stored
                    .as_ref()
                    .and_then(Json::as_str)
                    .map_or_else(|| defaults.clone(), str::to_string),
            };
            let id = format!("{}.{}", field.plugin, field.key);
            let open = self.plugin_open.as_deref() == Some(id.as_str());
            let caret = if open { "v" } else { ">" };
            let path = format!("{list}/r-p-{id}");
            let _ = create(
                "system-pages",
                &path,
                "Item",
                json!({"text": format!("{glyph} {:<16}{value} {caret}", field.label)}),
            );
            self.detail_nodes.push(path);
            if field.kind == "enum" && open {
                let mut options: Vec<String> = vec![defaults.clone()];
                options.extend(field.values.iter().cloned());
                for (i, opt) in options.iter().enumerate() {
                    let current = stored
                        .as_ref()
                        .and_then(Json::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| defaults.clone());
                    let is_current = *opt == current;
                    let mark = if is_current { "*" } else { " " };
                    let color = if is_current { "#56B6C2" } else { "#9AA0A6" };
                    let path = format!("{list}/o-p-{id}-{i}");
                    let _ = create(
                        "system-pages",
                        &path,
                        "Item",
                        json!({"text": format!("   ├─ {mark} {opt}"), "color": color}),
                    );
                    self.detail_nodes.push(path);
                }
            }
        }
    }

    // applies one plugin field write through the configuration gateway;
    // Json::Null means "unset, fall back to the global default"
    fn write_plugin_field(&self, plugin: &str, key: &str, value: Json, host: &mut HostHandle) {
        host.publish(
            "config.set",
            json!({"section": format!("plugins.{plugin}"), "key": key, "value": value}),
        );
    }

    // cycles a bool field default -> on -> off -> default
    fn cycle_plugin_bool(&self, field: &PluginField, host: &mut HostHandle) {
        let stored = host
            .config()
            .borrow()
            .get(&format!("plugins.{}", field.plugin), &field.key)
            .cloned();
        let next = match stored {
            None | Some(Json::Null) => Json::Bool(true),
            Some(Json::Bool(true)) => Json::Bool(false),
            _ => Json::Null,
        };
        self.write_plugin_field(&field.plugin, &field.key, next, host);
    }

    // anchors focus on the selected category so detail rebuilds never clear
    // focus and fall back to the first page
    fn refocus_selected_category(&self, host: &HostHandle) {
        let Some(cat) = self.selected_category else {
            return;
        };
        let _ = (host.ui().set_focus)("system-pages", &format!("/settings/split/tree/c-{cat}"));
    }

    fn info_row(&self, host: &HostHandle, key: &str) -> String {
        let value = match key {
            "version" => env!("CARGO_PKG_VERSION").to_string(),
            "theme" => host
                .config()
                .borrow()
                .get("styles", "theme")
                .and_then(Json::as_str)
                .unwrap_or("default-theme")
                .to_string(),
            "plugins" => host.plugins().len().to_string(),
            "account" => {
                let store = host.config();
                let config = store.borrow();
                let current = config
                    .get("accounts", "current")
                    .and_then(Json::as_str)
                    .unwrap_or("");
                let found = config
                    .get("accounts", "list")
                    .and_then(Json::as_array)
                    .and_then(|entries| {
                        entries
                            .iter()
                            .find(|entry| entry.get("id").and_then(Json::as_str) == Some(current))
                    });
                match found {
                    Some(entry) => {
                        let name = entry.get("name").and_then(Json::as_str).unwrap_or("?");
                        let kind = entry
                            .get("kind")
                            .and_then(Json::as_str)
                            .unwrap_or("offline");
                        format!("{name} ({})", self.t(host, &format!("account.kind.{kind}")))
                    }
                    None => self.t(host, "account.none"),
                }
            }
            "uptime" => self
                .booted
                .map(|b| fmt_duration(b.elapsed().as_secs()))
                .unwrap_or_else(|| "-".to_string()),
            _ => "-".to_string(),
        };
        format!("{:<9}{value}", self.t(host, &format!("home.info.{key}")))
    }

    fn patch_info(&self, host: &HostHandle, keys: &[&str]) {
        let ui = host.ui();
        for key in keys {
            let _ = (ui.patch)(
                "system-pages",
                &format!("/home/info/{key}"),
                json!({"text": self.info_row(host, key)}),
            );
        }
    }
}

impl SystemPages {
    /* ---------------- launch defaults ---------------- */

    fn launch_label(key: &str) -> &'static str {
        match key {
            "java_path" => "settings.java",
            "authlib_jar" => "settings.authlib",
            "memory_mb" => "settings.memory",
            "jvm_args" => "settings.jvm",
            "game_args" => "settings.game",
            _ => "settings.launch",
        }
    }

    fn launch_config_value(&self, host: &HostHandle, key: &str) -> String {
        let store = host.config();
        let config = store.borrow();
        match key {
            "java_path" => config
                .get("launcher", "java_path")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            "memory_mb" => config
                .get("launcher", "memory_mb")
                .and_then(Json::as_u64)
                .map(|value| value.to_string())
                .unwrap_or_default(),
            "authlib_jar" => config
                .get("launcher", key)
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            "jvm_args" | "game_args" => config
                .get("launcher", key)
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Json::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn launch_options(&self, host: &HostHandle, key: &str) -> Vec<String> {
        let auto = self.t(host, "settings.auto");
        let manual = self.t(host, "settings.manual");
        match key {
            "launch.java" => {
                let mut options = vec![auto];
                for candidate in &self.launch_java {
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
            "launch.memory" => vec![
                "1024 MB".to_string(),
                "2048 MB".to_string(),
                "4096 MB".to_string(),
                "8192 MB".to_string(),
                manual,
            ],
            _ => vec![manual],
        }
    }

    fn launch_command(&self, host: &mut HostHandle, payload: Json) {
        host.publish("launcher.command", payload);
    }

    fn launch_apply(&mut self, host: &mut HostHandle, key: &str, index: usize) {
        match key {
            "launch.java" => {
                let options = self.launch_options(host, key);
                if index == 0 {
                    self.launch_command(
                        host,
                        json!({"action": "settings.set", "scope": "global", "key": "java_path", "value": ""}),
                    );
                } else if index + 1 == options.len() {
                    self.open_launch_dialog(host, "java_path");
                } else {
                    let mut valid = 0;
                    for candidate in &self.launch_java {
                        if candidate.get("valid").and_then(Json::as_bool) != Some(true) {
                            continue;
                        }
                        valid += 1;
                        if valid == index {
                            let path = candidate
                                .get("path")
                                .and_then(Json::as_str)
                                .unwrap_or_default();
                            self.launch_command(
                                host,
                                json!({"action": "settings.set", "scope": "global", "key": "java_path", "value": path}),
                            );
                            break;
                        }
                    }
                }
            }
            "launch.memory" => {
                const MEMORY: [u64; 4] = [1024, 2048, 4096, 8192];
                if index + 1 == self.launch_options(host, key).len() {
                    self.open_launch_dialog(host, "memory_mb");
                } else if let Some(memory) = MEMORY.get(index) {
                    self.launch_command(
                        host,
                        json!({"action": "settings.set", "scope": "global", "key": "memory_mb", "value": memory}),
                    );
                }
            }
            _ => {}
        }
        self.launch_open = None;
        self.rebuild_setting_detail(host);
        self.refocus_selected_category(host);
    }

    fn handle_launch_activate(&mut self, host: &mut HostHandle, path: &str) -> bool {
        const LIST: &str = "/settings/split/detail/body/list";
        if path.starts_with(&format!("{PICKER}/")) {
            if path == format!("{PICKER}/cancel") {
                self.picker = None;
                Picker::close(host, "system-pages", PICKER);
                self.show_launch_dialog(host);
                return true;
            }
            let Some(mut picker) = self.picker.take() else {
                return true;
            };
            let title = self
                .launch_dialog
                .as_ref()
                .map(|(key, _)| self.t(host, Self::launch_label(key)))
                .unwrap_or_default();
            if let Some(selected) = picker.activate(host, "system-pages", PICKER, path, &title) {
                if let Some((_, input)) = &mut self.launch_dialog {
                    *input = selected.display().to_string();
                }
                Picker::close(host, "system-pages", PICKER);
                self.show_launch_dialog(host);
                let _ = (host.ui().set_focus)("system-pages", LAUNCH_INPUT);
            } else {
                self.picker = Some(picker);
            }
            return true;
        }
        if path == LAUNCH_BROWSE {
            let start = self.launch_dialog.as_ref().map(|(_, input)| input.clone());
            let picker = Picker::new(PickerMode::File, start.as_deref());
            let title = self
                .launch_dialog
                .as_ref()
                .map(|(key, _)| self.t(host, Self::launch_label(key)))
                .unwrap_or_default();
            picker.show(host, "system-pages", PICKER, &title);
            self.picker = Some(picker);
            return true;
        }
        if let Some(key) = path.strip_prefix(&format!("{LIST}/r-launch.")) {
            let full = format!("launch.{key}");
            match key {
                "java" | "memory" => {
                    self.launch_open = if self.launch_open.as_deref() == Some(full.as_str()) {
                        None
                    } else {
                        Some(full)
                    };
                    self.rebuild_setting_detail(host);
                    let _ =
                        (host.ui().set_focus)("system-pages", &format!("{LIST}/r-launch.{key}"));
                }
                "jvm" => self.open_launch_dialog(host, "jvm_args"),
                "game" => self.open_launch_dialog(host, "game_args"),
                "authlib" => self.open_launch_dialog(host, "authlib_jar"),
                _ => {}
            }
            return true;
        }
        if let Some(tail) = path.strip_prefix(&format!("{LIST}/lo-"))
            && let Some((key, index)) = tail.rsplit_once('-')
            && let Ok(index) = index.parse::<usize>()
        {
            self.launch_apply(host, key, index);
            return true;
        }
        if path == "/settings/launch/ok" {
            self.commit_launch_dialog(host);
            return true;
        }
        if path == "/settings/launch/cancel" {
            self.close_launch_dialog(host);
            return true;
        }
        false
    }

    fn open_launch_dialog(&mut self, host: &mut HostHandle, key: &str) {
        let input = self.launch_config_value(host, key);
        self.launch_dialog = Some((key.to_string(), input));
        self.show_launch_dialog(host);
        let target = if key == "java_path" || key == "authlib_jar" {
            LAUNCH_INPUT
        } else {
            "/settings/launch/input"
        };
        let _ = (host.ui().set_focus)("system-pages", target);
    }

    fn show_launch_dialog(&self, host: &HostHandle) {
        let _ = (host.ui().remove)("system-pages", "/settings/launch");
        let Some((key, input)) = &self.launch_dialog else {
            return;
        };
        let ui = host.ui();
        let _ = (ui.create)(
            "system-pages",
            "/settings/launch",
            "Popup",
            json!({"title": self.t(host, "settings.launch")}),
        );
        let _ = (ui.create)(
            "system-pages",
            "/settings/launch/hint",
            "Label",
            json!({"text": self.t(host, Self::launch_label(key)), "flex": 1.0}),
        );
        if key == "java_path" || key == "authlib_jar" {
            let _ = (ui.create)(
                "system-pages",
                LAUNCH_ROW,
                "Split",
                json!({"direction": "row", "flex": 1.0}),
            );
            let _ = (ui.create)(
                "system-pages",
                LAUNCH_BROWSE,
                "Button",
                json!({"text": "📁", "flex": 1.0}),
            );
            let _ = (ui.create)(
                "system-pages",
                LAUNCH_INPUT,
                "TextInput",
                json!({"text": input, "flex": 4.0}),
            );
        } else {
            let _ = (ui.create)(
                "system-pages",
                "/settings/launch/input",
                "TextInput",
                json!({"text": input, "flex": 1.0}),
            );
        }
        let _ = (ui.create)(
            "system-pages",
            "/settings/launch/ok",
            "Button",
            json!({"text": self.t(host, "settings.ok"), "flex": 1.0}),
        );
        let _ = (ui.create)(
            "system-pages",
            "/settings/launch/cancel",
            "Button",
            json!({"text": self.t(host, "settings.cancel"), "flex": 1.0}),
        );
    }

    fn close_launch_dialog(&mut self, host: &mut HostHandle) {
        self.launch_dialog = None;
        let _ = (host.ui().remove)("system-pages", "/settings/launch");
        self.rebuild_setting_detail(host);
        self.refocus_selected_category(host);
    }

    fn commit_launch_dialog(&mut self, host: &mut HostHandle) {
        let Some((key, input)) = self.launch_dialog.clone() else {
            return;
        };
        let value = match key.as_str() {
            "memory_mb" => match input.trim().parse::<u64>() {
                Ok(memory) if memory >= 512 => json!(memory),
                _ => {
                    self.close_launch_dialog(host);
                    return;
                }
            },
            "jvm_args" | "game_args" => json!(
                input
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            ),
            _ => json!(input.trim()),
        };
        self.launch_command(
            host,
            json!({"action": "settings.set", "scope": "global", "key": key, "value": value}),
        );
        self.close_launch_dialog(host);
    }
}

fn memory_ratio(memory: u64) -> f32 {
    ((memory.saturating_sub(512)) as f32 / (16384.0 - 512.0)).clamp(0.0, 1.0)
}

fn memory_from_ratio(ratio: f32) -> u64 {
    let memory = 512.0 + ratio.clamp(0.0, 1.0) * (16384.0 - 512.0);
    ((memory / 512.0).round() as u64 * 512).clamp(512, 16384)
}

impl NativePlugin for SystemPages {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "system-pages".to_string(),
            name: "System Pages".to_string(),
            version: "0.0.1".to_string(),
            description: "Built-in launcher pages".to_string(),
            provides: Vec::new(),
            subscribe: vec![
                "plugin.loaded".to_string(),
                "plugin.disabled".to_string(),
                "plugin.crashed".to_string(),
                "plugin.failed".to_string(),
                "config.updated".to_string(),
                "ui.input".to_string(),
                "ui.activate".to_string(),
                "ui.focus".to_string(),
                "i18n.locales".to_string(),
                "settings.schema".to_string(),
                "launcher.java".to_string(),
                "accounts.updated".to_string(),
            ],
            ui_grant: Vec::new(),
            data_grant: Vec::new(),
            cli_grant: Vec::new(),
            kind: "rhai".to_string(),
        }
    }

    fn init(&mut self, host: &mut HostHandle, _logger: &Logger) {
        self.booted = Some(Instant::now());
        let ui = host.ui();
        let create = &ui.create;
        // /home first: no focus means the first built page is what boots visible
        let _ = create(
            "system-pages",
            "/home",
            "Page",
            json!({"title": "terminism"}),
        );
        let _ = create(
            "system-pages",
            "/home/banner",
            "Split",
            json!({"direction": "row"}),
        );
        let _ = create(
            "system-pages",
            "/home/icon",
            "Label",
            json!({"text": ICON, "align": "center"}),
        );
        let _ = create(
            "system-pages",
            "/home/info",
            "Split",
            json!({"direction": "col"}),
        );
        for key in ["account", "version", "theme", "plugins", "uptime"] {
            let _ = create(
                "system-pages",
                &format!("/home/info/{key}"),
                "Label",
                json!({"text": self.info_row(host, key)}),
            );
        }
        // a generic widget: the gateway repaints it every second on its own
        let _ = create("system-pages", "/home/info/clock", "Clock", json!({}));
        let _ = create("system-pages", "/home/empty", "Section", json!({}));
        let _ = create(
            "system-pages",
            "/home/empty/title",
            "Label",
            json!({"text": format!("⏳ {}", self.t(host, "home.empty.title")), "align": "center"}),
        );
        let _ = create(
            "system-pages",
            "/home/empty/note",
            "Label",
            json!({"text": self.t(host, "home.empty.note"), "align": "center"}),
        );
        // /log rides with the default theme: it always runs, so the log survives any crash
        let _ = create("system-pages", "/log", "Page", json!({"title": "Log"}));
        let _ = create("system-pages", "/log/list", "List", json!({}));
        // settings page: collapsible category tree beside a setting editor
        let _ = create(
            "system-pages",
            "/settings",
            "Page",
            json!({"title": "Settings"}),
        );
        let _ = create(
            "system-pages",
            "/settings/split",
            "Split",
            json!({"direction": "row"}),
        );
        let _ = create(
            "system-pages",
            "/settings/split/tree",
            "List",
            json!({"title": "Settings", "flex": 2.0}),
        );
        let _ = create(
            "system-pages",
            "/settings/split/detail",
            "Section",
            json!({"title": "Detail", "flex": 3.0}),
        );
        self.rebuild_tree(host);
        // build the selected category's rows up front: the page may be opened
        // before any config change arrives
        self.rebuild_setting_detail(host);
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, _logger: &Logger) {
        if name.starts_with("plugin.") {
            self.patch_info(host, &["plugins"]);
            self.rebuild_tree(host);
            return;
        }
        match name {
            "accounts.updated" => self.patch_info(host, &["account"]),
            "i18n.locales" => {
                let found = payload
                    .get("locales")
                    .and_then(Json::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Json::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                self.locales = found;
                if self.in_settings {
                    self.rebuild_setting_detail(host);
                }
            }
            "settings.schema" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let fields = payload
                    .get("fields")
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.plugin_fields.retain(|f| f.plugin != plugin);
                for field in fields {
                    let kind = field
                        .get("kind")
                        .and_then(Json::as_str)
                        .unwrap_or("text")
                        .to_string();
                    self.plugin_fields.push(PluginField {
                        plugin: plugin.clone(),
                        key: field
                            .get("key")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_string(),
                        label: field
                            .get("label")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_string(),
                        values: field
                            .get("values")
                            .and_then(Json::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Json::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        default: field
                            .get("default")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_string(),
                        kind,
                    });
                }
                if self.in_settings {
                    self.rebuild_tree(host);
                    self.rebuild_setting_detail(host);
                }
            }
            "ui.edit" => {
                if payload.get("path").and_then(Json::as_str)
                    == Some("/settings/split/detail/body/list/memory")
                {
                    let memory = host
                        .config()
                        .borrow()
                        .get("launcher", "memory_mb")
                        .and_then(Json::as_u64)
                        .unwrap_or(4096);
                    self.open_launch_dialog(host, "memory_mb");
                    if let Some((_, input)) = &mut self.launch_dialog {
                        *input = memory.to_string();
                    }
                    self.show_launch_dialog(host);
                }
            }
            "ui.input" => {
                if payload.get("path").and_then(Json::as_str)
                    == Some("/settings/split/detail/body/list/memory")
                    && let Some(value) = payload.get("value").and_then(Json::as_f64)
                {
                    let memory = memory_from_ratio(value as f32);
                    host.publish(
                        "launcher.command",
                        json!({"action": "settings.set", "scope": "global", "key": "memory_mb", "value": memory}),
                    );
                    let text = format!("├─ {}|{} MB", self.t(host, "settings.memory"), memory);
                    let _ = (host.ui().patch)(
                        "system-pages",
                        "/settings/split/detail/body/list/memory",
                        json!({"text": text}),
                    );
                }
                let input_path = payload.get("path").and_then(Json::as_str);
                if (input_path == Some(LAUNCH_INPUT)
                    || input_path == Some("/settings/launch/input"))
                    && let Some((_, input)) = &mut self.launch_dialog
                {
                    *input = payload
                        .get("text")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            "launcher.java" => {
                self.launch_java = payload
                    .get("candidates")
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                if self.in_settings && self.selected_category == Some("settings.launch") {
                    self.rebuild_setting_detail(host);
                }
            }
            "config.updated" => {
                if self.in_settings {
                    self.rebuild_tree(host);
                    self.rebuild_setting_detail(host);
                    // the rebuild just removed the focused control: anchor
                    // focus back onto the selected category (unless a dialog owns it)
                    if self.launch_dialog.is_none() {
                        self.refocus_selected_category(host);
                    }
                }
                self.patch_info(host, &["theme"]);
            }
            "ui.blur" => {
                if let Some(path) = payload.get("path").and_then(Json::as_str)
                    && path.starts_with("/settings")
                {
                    self.in_settings = false;
                }
            }
            "ui.focus" => {
                let Some(path) = payload.get("path").and_then(Json::as_str) else {
                    return;
                };
                self.in_settings = path.starts_with("/settings");
                // focus only drives selection for keyboard navigation;
                // clicks select categories via ui.activate
                if let Some(cat) = path
                    .strip_prefix("/settings/split/tree/c-")
                    .filter(|c| !c.is_empty())
                    && let Some((cat, _, _)) = SETTINGS.iter().find(|(c, _, _)| *c == cat)
                {
                    self.selected_category = Some(cat);
                    self.draft_locale.clear();
                    if *cat == "settings.launch" {
                        host.publish("launcher.command", json!({"action": "java.detect"}));
                    }
                    self.rebuild_setting_detail(host);
                }
            }
            "ui.activate" => {
                let Some(path) = payload.get("path").and_then(Json::as_str) else {
                    return;
                };
                const LIST: &str = "/settings/split/detail/body/list";
                if self.handle_launch_activate(host, path) {
                    return;
                }
                // list rows: one line per setting, click to act
                if let Some(key) = path
                    .strip_prefix(&format!("{LIST}/r-"))
                    .filter(|k| !k.is_empty())
                {
                    match key {
                        "appearance.lock" => {
                            let on = self.setting_read(key, host) == "on";
                            self.set_lock(!on, host);
                        }
                        "appearance.theme" => {
                            self.theme_open = !self.theme_open;
                            self.locale_open = false;
                            self.rebuild_setting_detail(host);
                            let _ = (host.ui().set_focus)(
                                "system-pages",
                                &format!("{LIST}/r-appearance.theme"),
                            );
                        }
                        "general.locale" => {
                            self.locale_open = !self.locale_open;
                            self.theme_open = false;
                            self.rebuild_setting_detail(host);
                            let _ = (host.ui().set_focus)(
                                "system-pages",
                                &format!("{LIST}/r-general.locale"),
                            );
                        }
                        _ => {}
                    }
                    return;
                }
                // inline theme option rows sit right under the theme row
                if let Some(id) = path.strip_prefix(&format!("{LIST}/m-")) {
                    let themes = self.available_themes(host);
                    if let Some(theme) = id.parse::<usize>().ok().and_then(|i| themes.get(i)) {
                        host.publish(
                            "config.set",
                            json!({"section": "styles", "key": "theme", "value": theme}),
                        );
                    }
                    self.theme_open = false;
                    self.rebuild_setting_detail(host);
                    let _ = (host.ui().set_focus)(
                        "system-pages",
                        &format!("{LIST}/r-appearance.theme"),
                    );
                    return;
                }
                if let Some(id) = path.strip_prefix(&format!("{LIST}/l-")) {
                    if let Some(locale) = id.parse::<usize>().ok().and_then(|i| self.locales.get(i))
                    {
                        host.publish(
                            "config.set",
                            json!({"section": "i18n", "key": "locale", "value": locale}),
                        );
                    }
                    self.locale_open = false;
                    self.rebuild_setting_detail(host);
                    return;
                }
                // plugin field rows: bool cycles, enum toggles its options
                const LIST2: &str = "/settings/split/detail/body/list";
                if let Some(id) = path.strip_prefix(&format!("{LIST2}/r-p-")) {
                    let field = self
                        .plugin_fields
                        .iter()
                        .find(|f| format!("{}.{}", f.plugin, f.key) == id)
                        .cloned();
                    if let Some(field) = field {
                        if field.kind == "bool" {
                            self.cycle_plugin_bool(&field, host);
                        } else if field.kind == "enum" {
                            let now = self.plugin_open.clone();
                            self.plugin_open = if now.as_deref() == Some(id) {
                                None
                            } else {
                                Some(id.to_string())
                            };
                            self.rebuild_setting_detail(host);
                            let _ =
                                (host.ui().set_focus)("system-pages", &format!("{LIST2}/r-p-{id}"));
                        }
                    }
                    return;
                }
                if let Some(tail) = path.strip_prefix(&format!("{LIST2}/o-p-")) {
                    // tail = "<plugin>.<key>-<option index>"
                    if let Some((id, idx)) = tail.rsplit_once('-')
                        && let Ok(idx) = idx.parse::<usize>()
                        && let Some(field) = self
                            .plugin_fields
                            .iter()
                            .find(|f| format!("{}.{}", f.plugin, f.key) == id)
                            .cloned()
                    {
                        let value = if idx == 0 {
                            Json::Null
                        } else {
                            json!(field.values.get(idx - 1).cloned().unwrap_or_default())
                        };
                        self.write_plugin_field(&field.plugin, &field.key, value, host);
                        self.plugin_open = None;
                        self.rebuild_setting_detail(host);
                    }
                    return;
                }
                // clicking a category item selects it (Items activate on click)
                if let Some(cat) = path.strip_prefix("/settings/split/tree/c-")
                    && let Some((cat, _, _)) = SETTINGS.iter().find(|(c, _, _)| *c == cat)
                {
                    self.selected_category = Some(cat);
                    self.draft_locale.clear();
                    self.theme_open = false;
                    self.locale_open = false;
                    if *cat == "settings.launch" {
                        host.publish("launcher.command", json!({"action": "java.detect"}));
                    }
                    self.rebuild_setting_detail(host);
                    self.refocus_selected_category(host);
                }
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, host: &mut HostHandle, _logger: &Logger) {
        self.patch_info(host, &["uptime"]);
    }
}
