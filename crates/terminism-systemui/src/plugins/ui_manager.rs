use serde_json::json;
use std::collections::VecDeque;
use std::rc::Rc;
use terminism_kernel::host::{HostHandle, Json, NativePlugin, PluginInfo};
use terminism_kernel::logger::{LogStore, Logger};
use terminism_kernel::manifest::Manifest;

// create-if-possible helper: records the path when the gateway accepts it
#[allow(clippy::type_complexity)]
fn mk(
    create: &Rc<dyn Fn(&str, &str, &str, Json) -> bool>,
    created: &mut Vec<String>,
    path: &str,
    kind: &str,
    props: Json,
) {
    if create("ui-manager", path, kind, props) {
        created.push(path.to_string());
    }
}

use super::manifest;

const LOG_TAIL: usize = 50;

// detail pane layout tiers, picked from the page width announced by ui.resize
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// wide: info grid beside the description (R3)
    Split,
    /// medium: banner, divider, field rows (R1)
    Cards,
    /// narrow: name line plus badge chips (R2)
    Badges,
}

fn tier_for(page_width: u16) -> Tier {
    // detail owns three of four flex shares, minus its own border
    let detail = page_width / 4 * 3;
    if detail >= 50 {
        Tier::Split
    } else if detail >= 30 {
        Tier::Cards
    } else {
        Tier::Badges
    }
}

// what a tick needs to patch the live detail pane in place, no teardown
#[derive(Clone)]
struct DynState {
    tier: Tier,
    version: String,
    dot: &'static str,
    state_text: String,
    is_system: bool,
    // runtime kind: native | rhai | wasm
    kind: &'static str,
    // uptime text changes every tick only while the slot is active
    uptime_live: bool,
}

pub struct UIManager {
    // sidebar entry paths created by the last rebuild, removed on the next one
    listed: Vec<String>,
    // detail child paths created by the last detail rebuild, removed on the next one
    detailed: Vec<String>,
    // sidebar entry paths in display order, headers excluded; arrow hover walks this
    entry_paths: Vec<String>,
    // (id, error) of script loads that never produced a slot
    failed: Vec<(String, String)>,
    selected: Option<String>,
    pending: Option<String>,
    log_store: LogStore,
    // in-place tick refresh targets, set by rebuild_detail
    dyn_state: Option<DynState>,
    log_base: Option<String>,
    log_items: Vec<String>,
    // (line count, last line) of the rendered log window
    log_revision: Option<u64>,
    page_width: Option<u16>,
}

// green loaded, gray manually disabled, red crashed
fn status_color(info: &PluginInfo) -> &'static str {
    if info.state == "active" {
        "green"
    } else if info.reason == "disabled" {
        "#9AA0A6"
    } else {
        "red"
    }
}

fn fmt_uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

// a log line belongs to a plugin when its lineage tag is the id or ends with ".{id}"
fn plugin_log_lines(log: &VecDeque<String>, id: &str, tail: usize) -> Vec<String> {
    let matched: Vec<&String> = log
        .iter()
        .filter(|l| {
            let Some(tag) = l.strip_prefix('[').and_then(|t| t.split(']').next()) else {
                return false;
            };
            tag == id || tag.ends_with(&format!(".{id}"))
        })
        .collect();
    matched[matched.len().saturating_sub(tail)..]
        .iter()
        .map(|s| (*s).clone())
        .collect()
}

// sidebar entry sink: create-callback plus the path books kept per rebuild
#[allow(clippy::type_complexity)]
struct EntrySink<'a> {
    create: &'a Rc<dyn Fn(&str, &str, &str, Json) -> bool>,
    listed: &'a mut Vec<String>,
    entry_paths: &'a mut Vec<String>,
}

// plugin kind as shown to users: native and wasm builds are rust code
fn kind_word(kind: &str) -> &'static str {
    match kind {
        "rhai" => "rhai",
        _ => "rust",
    }
}

fn emit_entry(sink: &mut EntrySink<'_>, info: &PluginInfo, prefix: &str) {
    let path = format!("/plugins/split/list/p-{}", info.id);
    let _ = (sink.create)(
        "ui-manager",
        &path,
        "Item",
        json!({"text": format!("{prefix}{}", info.name), "color": status_color(info)}),
    );
    sink.listed.push(path.clone());
    sink.entry_paths.push(path);
}

// depth-first walk over the lineage tree; branches use box-drawing glyphs so
// the sidebar reads as a real tree
fn emit_tree(
    sink: &mut EntrySink<'_>,
    all: &[&PluginInfo],
    parent: &str,
    prefix: &str,
    walked: &mut Vec<String>,
) {
    let kids: Vec<&PluginInfo> = all
        .iter()
        .copied()
        .filter(|i| match &i.parent {
            Some(p) => p == parent,
            None => parent == "init",
        })
        .filter(|i| !walked.contains(&i.id))
        .collect();
    for (i, info) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let branch = if last { "└─ " } else { "├─ " };
        walked.push(info.id.clone());
        emit_entry(sink, info, &format!("{prefix}{branch}"));
        let next = format!("{prefix}{}", if last { "   " } else { "│  " });
        emit_tree(sink, all, &info.id, &next, walked);
    }
}

impl UIManager {
    fn t(&self, host: &HostHandle, key: &str) -> String {
        host.i18n()
            .borrow()
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    pub fn new(log_store: LogStore) -> Self {
        Self {
            listed: Vec::new(),
            detailed: Vec::new(),
            entry_paths: Vec::new(),
            failed: Vec::new(),
            selected: None,
            pending: None,
            log_store,
            page_width: None,
            dyn_state: None,
            log_base: None,
            log_items: Vec::new(),
            log_revision: None,
        }
    }

    fn rebuild_list(&mut self, host: &HostHandle) {
        let ui = host.ui();
        let create = &ui.create;
        let remove = &ui.remove;
        for path in std::mem::take(&mut self.listed) {
            let _ = remove("ui-manager", &path);
        }
        self.entry_paths.clear();
        let infos = host.plugins();
        if !infos.iter().any(|i| Some(&i.id) == self.selected.as_ref())
            && !self
                .failed
                .iter()
                .any(|(id, _)| Some(id) == self.selected.as_ref())
        {
            self.selected = None;
        }
        let _ = create(
            "ui-manager",
            "/plugins/split/list/h-sys",
            "Item",
            json!({"text": format!("◈ {}", self.t(host, "manager.system")), "color": "#9AA0A6"}),
        );
        self.listed.push("/plugins/split/list/h-sys".to_string());
        // system plugins render as a lineage tree rooted at init
        let system: Vec<&PluginInfo> = {
            let mut v: Vec<&PluginInfo> = infos.iter().filter(|i| i.system).collect();
            v.sort_by(|a, b| a.id.cmp(&b.id));
            v
        };
        let user_label = format!("◇ {}", self.t(host, "manager.user"));
        let mut sink = EntrySink {
            create,
            listed: &mut self.listed,
            entry_paths: &mut self.entry_paths,
        };
        emit_tree(&mut sink, &system, "init", "", &mut Vec::new());
        let mut user_header_done = false;
        for info in infos.iter().filter(|i| !i.system) {
            if !user_header_done {
                user_header_done = true;
                let _ = create(
                    "ui-manager",
                    "/plugins/split/list/h-usr",
                    "Item",
                    json!({"text": user_label, "color": "white"}),
                );
                sink.listed.push("/plugins/split/list/h-usr".to_string());
            }
            emit_entry(&mut sink, info, "· ");
        }
        let _ = &sink.entry_paths;
        if !self.failed.is_empty() && !user_header_done {
            let _ = create(
                "ui-manager",
                "/plugins/split/list/h-usr",
                "Item",
                json!({"text": format!("◇ {}", self.t(host, "manager.user")), "color": "white"}),
            );
            self.listed.push("/plugins/split/list/h-usr".to_string());
        }
        for (id, _) in &self.failed {
            let path = format!("/plugins/split/list/f-{id}");
            let _ = create(
                "ui-manager",
                &path,
                "Item",
                json!({"text": format!("· {id} [rhai]"), "color": "red"}),
            );
            self.listed.push(path.clone());
            self.entry_paths.push(path);
        }
        // the entries were just recreated: re-anchor focus so the page never
        // falls back to home when the focused entry was replaced
        self.refocus_selected(host);
        self.rebuild_detail(host);
    }

    fn rebuild_detail(&mut self, host: &HostHandle) {
        let ui = host.ui();
        let create = &ui.create;
        let remove = &ui.remove;
        let patch = &ui.patch;
        let mut created: Vec<String> = Vec::new();
        for path in std::mem::take(&mut self.detailed) {
            let _ = remove("ui-manager", &path);
        }
        let Some(id) = self.selected.clone() else {
            let _ = patch(
                "ui-manager",
                "/plugins/split/detail",
                json!({"title": self.t(host, "manager.detail")}),
            );
            mk(
                create,
                &mut created,
                "/plugins/split/detail/hint",
                "Label",
                json!({"text": self.t(host, "manager.hint"), "align": "center"}),
            );
            self.detailed = created;
            return;
        };
        // shared fields across tiers
        let failed = self
            .failed
            .iter()
            .find(|(fid, _)| fid == &id)
            .map(|(_, e)| e.clone());
        let info = host.plugins().into_iter().find(|i| i.id == id);
        if info.is_none() && failed.is_none() {
            self.selected = None;
            return self.rebuild_detail(host);
        }
        let manifest = host.manifest_of(&id);
        let field = |key: &str| {
            manifest
                .as_ref()
                .and_then(|m| m.get(key))
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (name, version, is_system) = match &info {
            Some(i) => (
                if field("name").is_empty() {
                    id.clone()
                } else {
                    field("name")
                },
                if field("version").is_empty() {
                    "-".to_string()
                } else {
                    field("version")
                },
                i.system,
            ),
            None => (id.clone(), "-".to_string(), false),
        };
        let is_active = matches!(&info, Some(i) if i.state == "active");
        let kind_label = info.as_ref().map_or("rust", |i| kind_word(i.kind));
        let (dot, _dot_color, state_text, uptime_text) = match (&info, &failed) {
            (Some(i), _) if i.state == "active" => (
                "●",
                "green",
                self.t(host, "manager.active"),
                fmt_uptime(i.uptime_secs),
            ),
            (Some(i), _) if i.reason.is_empty() || i.reason == "disabled" => (
                "●",
                "#9AA0A6",
                self.t(host, "manager.stopped"),
                "-".to_string(),
            ),
            (Some(i), _) => (
                "●",
                "red",
                format!("stopped ({})", i.reason),
                "-".to_string(),
            ),
            (None, Some(e)) => ("●", "red", format!("failed ({e})"), "-".to_string()),
            (None, None) => unreachable!("guarded above"),
        };
        let description = {
            let d = field("description");
            if d.is_empty() {
                self.t(host, "manager.no_description")
            } else {
                d
            }
        };
        let _ = patch(
            "ui-manager",
            "/plugins/split/detail",
            json!({"title": name}),
        );
        let tier = self.page_width.map_or(Tier::Split, tier_for);
        self.dyn_state = Some(DynState {
            tier,
            version: version.clone(),
            dot,
            state_text: state_text.clone(),
            is_system,
            kind: kind_label,
            uptime_live: is_active,
        });
        const BASE: &str = "/plugins/split/detail/layout";
        mk(
            create,
            &mut created,
            BASE,
            "Split",
            json!({"direction": "col"}),
        );
        match tier {
            Tier::Split => {
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/top"),
                    "Split",
                    json!({"direction": "row", "flex": 3.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/top/info"),
                    "List",
                    json!({"title": "State", "flex": 1.0}),
                );
                for (path, text, color) in [
                    ("i-state", format!("{dot} {state_text}"), "#56B6C2"),
                    ("i-up", format!("↑ {uptime_text}"), "#9AA0A6"),
                    ("i-ver", format!("v{version}"), "#9AA0A6"),
                    (
                        "i-kind",
                        if is_system { "◈ system" } else { "◇ user" }.to_string(),
                        "#9AA0A6",
                    ),
                ] {
                    mk(
                        create,
                        &mut created,
                        &format!("{BASE}/top/info/{path}"),
                        "Item",
                        json!({"text": text, "color": color}),
                    );
                }
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/top/about"),
                    "Label",
                    json!({"text": description, "flex": 2.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/div-a"),
                    "Label",
                    json!({"text": "\u{2500}", "flex": 1.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/log"),
                    "List",
                    json!({"title": format!("≡ {}", self.t(host, "manager.log")), "flex": 5.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/actions"),
                    "Split",
                    json!({"direction": "row", "flex": 1.0}),
                );
            }
            Tier::Cards => {
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/banner"),
                    "Label",
                    json!({"text": format!("◈ {name}  v{version}"), "flex": 2.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/div-a"),
                    "Label",
                    json!({"text": "\u{2500}", "flex": 1.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/fields"),
                    "Label",
                    json!({
                        "text": format!(
                            "State   {dot} {state_text}\nUptime  {uptime_text}\nKind    {}",
                            if is_system { "system" } else { "user" }
                        ),
                        "flex": 4.0,
                    }),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/div-b"),
                    "Label",
                    json!({"text": "\u{2500}", "flex": 1.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/log"),
                    "List",
                    json!({"title": format!("≡ {}", self.t(host, "manager.log")), "flex": 4.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/actions"),
                    "Split",
                    json!({"direction": "row", "flex": 1.0}),
                );
            }
            Tier::Badges => {
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/name"),
                    "Label",
                    json!({"text": format!("◈ {name}"), "flex": 2.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/badges"),
                    "Label",
                    json!({"text": format!("⟨v{version}⟩ ⟨{dot} {state_text}⟩ ⟨{uptime_text}⟩")}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/desc"),
                    "Label",
                    json!({"text": description, "flex": 3.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/log"),
                    "List",
                    json!({"title": format!("≡ {}", self.t(host, "manager.log")), "flex": 4.0}),
                );
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/actions"),
                    "Split",
                    json!({"direction": "row", "flex": 1.0}),
                );
            }
        }
        if info.as_ref().is_some_and(|i| i.kind != "native") {
            let mut buttons = vec![("reload", self.t(host, "manager.reload"))];
            if is_active {
                buttons.insert(0, ("off", self.t(host, "manager.disable")));
            }
            for (path, text) in buttons {
                mk(
                    create,
                    &mut created,
                    &format!("{BASE}/actions/{path}"),
                    "Button",
                    json!({"text": text}),
                );
            }
        }
        // log items live under whichever tier's log list was just created
        self.detailed = created;
        let log_base = self
            .detailed
            .iter()
            .find(|p| p.ends_with("/log"))
            .cloned()
            .unwrap_or_else(|| format!("{BASE}/log"));
        self.rebuild_log(host, &id, &log_base);
        // the teardown above may have removed the focused node; anchor focus
        // so the page never falls back to home
        if self.pending.is_some() {
            let _ = (host.ui().set_focus)("ui-manager", "/plugins/confirm/yes");
        } else {
            self.refocus_selected(host);
        }
    }

    // renders the log window; also records the signature ticks compare against
    fn rebuild_log(&mut self, host: &HostHandle, id: &str, log_base: &str) {
        self.log_base = Some(log_base.to_string());
        let lines = {
            let store = self.log_store.borrow();
            plugin_log_lines(&store, id, LOG_TAIL)
        };
        self.log_revision = Some(self.log_store.borrow().revision());
        let ui = host.ui();
        let remove = &ui.remove;
        let removed = std::mem::take(&mut self.log_items);
        self.detailed.retain(|p| !removed.contains(p));
        for path in removed {
            let _ = remove("ui-manager", &path);
        }
        let create = &ui.create;
        for (i, line) in lines.iter().enumerate() {
            let path = format!("{log_base}/i-{i}");
            let _ = create(
                "ui-manager",
                &path,
                "Item",
                json!({"text": line, "color": "#9AA0A6"}),
            );
            self.log_items.push(path.clone());
            self.detailed.push(path);
        }
    }

    fn open_confirm(&self, host: &HostHandle, id: &str) {
        let ui = host.ui();
        let create = &ui.create;
        let _ = create(
            "ui-manager",
            "/plugins/confirm",
            "Popup",
            json!({"title": "Confirm"}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/confirm/text",
            "Label",
            json!({"text": format!("disable {id}?")}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/confirm/yes",
            "Button",
            json!({"text": "yes"}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/confirm/no",
            "Button",
            json!({"text": "no"}),
        );
    }

    // up/down walk the sidebar entries, wrapping around; any other starting
    // point (header, detail button) restarts from the first entry
    fn hover_step(&mut self, payload: &Json, host: &HostHandle) {
        let key = payload.get("key").and_then(Json::as_str).unwrap_or("");
        if key != "up" && key != "down" {
            return;
        }
        let Some(first) = self.entry_paths.first().cloned() else {
            return;
        };
        let path = payload.get("path").and_then(Json::as_str).unwrap_or("");
        let next = match self.entry_paths.iter().position(|p| p == path) {
            Some(i) if key == "up" => {
                self.entry_paths[(i + self.entry_paths.len() - 1) % self.entry_paths.len()].clone()
            }
            Some(i) => self.entry_paths[(i + 1) % self.entry_paths.len()].clone(),
            None => first,
        };
        let _ = (host.ui().set_hover)("ui-manager", &next);
    }
}

impl NativePlugin for UIManager {
    fn manifest(&self) -> Manifest {
        manifest(
            "ui-manager",
            "UIManager",
            &[
                "plugin.loaded",
                "plugin.disabled",
                "plugin.crashed",
                "plugin.failed",
                "ui.focus",
                "ui.activate",
                "ui.resize",
                "ui.key",
            ],
            "Plugin manager page with sidebar list and detail pane",
        )
    }

    fn init(&mut self, host: &mut HostHandle, _logger: &Logger) {
        let ui = host.ui();
        let create = &ui.create;
        let _ = create(
            "ui-manager",
            "/plugins",
            "Page",
            json!({"title": "Plugins"}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/split",
            "Split",
            json!({"direction": "row"}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/split/list",
            "List",
            json!({"title": "Plugins", "flex": 1.0}),
        );
        let _ = create(
            "ui-manager",
            "/plugins/split/detail",
            "Section",
            json!({"title": self.t(host, "manager.detail"), "flex": 3.0}),
        );
        // the detail subtree is rebuilt per width tier by rebuild_detail
        self.rebuild_list(host);
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        if name == "plugin.loaded"
            && let Some(id) = payload.get("id").and_then(Json::as_str)
        {
            self.failed.retain(|(fid, _)| fid != id);
        } else if name == "plugin.failed"
            && let (Some(id), Some(error)) = (
                payload.get("id").and_then(Json::as_str),
                payload.get("error").and_then(Json::as_str),
            )
        {
            self.failed.retain(|(fid, _)| fid != id);
            if !host.plugins().iter().any(|p| p.id == id) {
                self.failed.push((id.to_string(), error.to_string()));
            }
        }
        if name.starts_with("plugin.") {
            self.rebuild_list(host);
            return;
        }
        if name == "ui.resize" {
            let Some(width) = payload.get("width").and_then(Json::as_u64) else {
                return;
            };
            let width = width as u16;
            if self.page_width != Some(width) {
                self.page_width = Some(width);
                self.rebuild_detail(host);
            }
            return;
        }
        if name == "ui.focus" {
            let Some(path) = payload.get("path").and_then(Json::as_str) else {
                return;
            };
            let Some(id) = path
                .strip_prefix("/plugins/split/list/p-")
                .or_else(|| path.strip_prefix("/plugins/split/list/f-"))
            else {
                return;
            };
            if !id.is_empty()
                && self.selected.as_deref() != Some(id)
                && (host.plugins().iter().any(|i| i.id == id)
                    || self.failed.iter().any(|(fid, _)| fid == id))
            {
                self.selected = Some(id.to_string());
                // the list is left untouched so the focused item keeps focus
                self.rebuild_detail(host);
            }
            return;
        }
        if name == "ui.key" {
            self.hover_step(payload, host);
            return;
        }
        if name != "ui.activate" {
            return;
        }
        let Some(path) = payload.get("path").and_then(Json::as_str) else {
            return;
        };
        match path {
            "/plugins/split/detail/layout/actions/off" => {
                if let Some(id) = self.selected.clone() {
                    self.pending = Some(id.clone());
                    self.open_confirm(host, &id);
                }
            }
            "/plugins/split/detail/layout/actions/reload" => {
                if let Some(id) = self.selected.clone()
                    && let Err(e) = host.reload(&id)
                {
                    logger.log(&format!("reload failed: {e}"));
                }
                self.refocus_selected(host);
            }
            "/plugins/confirm/yes" => {
                if let Some(id) = self.pending.take()
                    && let Err(e) = host.disable(&id)
                {
                    logger.log(&format!("disable failed: {e}"));
                }
                let _ = (host.ui().remove)("ui-manager", "/plugins/confirm");
                self.rebuild_list(host);
                self.refocus_selected(host);
            }
            "/plugins/confirm/no" => {
                self.pending = None;
                let _ = (host.ui().remove)("ui-manager", "/plugins/confirm");
                self.refocus_selected(host);
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, host: &mut HostHandle, _logger: &Logger) {
        if self.pending.is_some() {
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.refresh_uptime(host, &id);
        self.refresh_log_if_changed(host, &id);
    }
}

impl UIManager {
    // patches only the uptime-bearing node text; no structural churn, no focus loss
    fn refresh_uptime(&self, host: &HostHandle, id: &str) {
        let Some(dyns) = &self.dyn_state else {
            return;
        };
        if !dyns.uptime_live {
            return;
        }
        let Some(info) = host.plugins().into_iter().find(|i| i.id == id) else {
            return;
        };
        let up = fmt_uptime(info.uptime_secs);
        let ui = host.ui();
        let patch = &ui.patch;
        const BASE: &str = "/plugins/split/detail/layout";
        match dyns.tier {
            Tier::Split => {
                let _ = patch(
                    "ui-manager",
                    &format!("{BASE}/top/info/i-up"),
                    json!({"text": format!("↑ {up}")}),
                );
            }
            Tier::Cards => {
                let _ = patch(
                    "ui-manager",
                    &format!("{BASE}/fields"),
                    json!({
                        "text": format!(
                            "State   {} {}\nUptime  {}\nKind    {}",
                            dyns.dot,
                            dyns.state_text,
                            up,
                            if dyns.is_system { "system" } else { "user" }
                        )
                    }),
                );
            }
            Tier::Badges => {
                let _ = patch(
                    "ui-manager",
                    &format!("{BASE}/badges"),
                    json!({
                        "text": format!(
                            "⟨v{}⟩ ⟨{} {}⟩ ⟨{}⟩ ⟨{}⟩",
                            dyns.version, dyns.dot, dyns.state_text, up, dyns.kind
                        )
                    }),
                );
            }
        }
    }

    // new log lines only: the signature comparison keeps idle ticks free
    fn refresh_log_if_changed(&mut self, host: &HostHandle, id: &str) {
        if self.log_revision == Some(self.log_store.borrow().revision()) {
            return;
        }
        let Some(base) = self.log_base.clone() else {
            return;
        };
        self.rebuild_log(host, id, &base);
    }
}

impl UIManager {
    // lifecycle actions remove the focused node (popup or button); without a
    // focus anchor the gateway falls back to the first page and jumps away
    fn refocus_selected(&self, host: &HostHandle) {
        let Some(id) = &self.selected else {
            return;
        };
        let entry = if self.failed.iter().any(|(fid, _)| fid == id) {
            format!("/plugins/split/list/f-{id}")
        } else {
            format!("/plugins/split/list/p-{id}")
        };
        let _ = (host.ui().set_focus)("ui-manager", &entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_match_lineage_suffix() {
        let log = VecDeque::from(vec![
            "[init.manager.loader] scan done".to_string(),
            "[init.logger.hello-terminism] hi".to_string(),
            "[init.ui-manager] ready".to_string(),
            "no tag line".to_string(),
        ]);
        let got = plugin_log_lines(&log, "hello-terminism", 50);
        assert_eq!(got, ["[init.logger.hello-terminism] hi".to_string()]);
        let got = plugin_log_lines(&log, "ui-manager", 50);
        assert_eq!(got, ["[init.ui-manager] ready".to_string()]);
        let lines: VecDeque<String> = (0..60).map(|i| format!("[x] {i}")).collect();
        let tail = plugin_log_lines(&lines, "x", 50);
        assert_eq!(tail.len(), 50);
        assert_eq!(tail[0], "[x] 10");
    }

    #[test]
    fn uptime_format() {
        assert_eq!(fmt_uptime(45), "45s");
        assert_eq!(fmt_uptime(192), "3m12s");
        assert_eq!(fmt_uptime(3900), "1h05m");
    }

    #[test]
    fn detail_tiers_follow_page_width() {
        // detail inner = width * 3/4; thresholds at 50 and 30
        assert_eq!(tier_for(80), Tier::Split);
        assert_eq!(tier_for(70), Tier::Split);
        assert_eq!(tier_for(60), Tier::Cards);
        assert_eq!(tier_for(40), Tier::Cards);
        assert_eq!(tier_for(30), Tier::Badges);
    }
}
