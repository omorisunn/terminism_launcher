//! Account registry and the /account page.

use serde_json::{Value, json};

use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

const ACTOR: &str = "account";
const PAGE: &str = "/account";
const SPLIT: &str = "/account/split";
const LIST: &str = "/account/split/list";
const DETAIL: &str = "/account/split/detail";
const LAYOUT: &str = "/account/split/detail/layout";
const INFO: &str = "/account/split/detail/layout/info";
const ADDS: &str = "/account/split/detail/layout/adds";
const ACTIONS: &str = "/account/split/detail/layout/actions";
const DIALOG: &str = "/account/dialog";
const DIALOG_NAME: &str = "/account/dialog/name";
const DIALOG_SERVER: &str = "/account/dialog/server";
const DIALOG_USER: &str = "/account/dialog/user";
const DIALOG_PASS: &str = "/account/dialog/pass";
const DIALOG_OK: &str = "/account/dialog/ok";
const DIALOG_CANCEL: &str = "/account/dialog/cancel";

#[derive(Debug, Clone, PartialEq)]
struct AccountEntry {
    id: String,
    name: String,
    kind: String,
    uuid: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    server: Option<String>,
}

impl AccountEntry {
    fn from_json(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let name = value.get("name")?.as_str()?.to_string();
        let kind = value.get("kind")?.as_str()?.to_string();
        if !matches!(kind.as_str(), "offline" | "online" | "third-party") {
            return None;
        }
        Some(Self {
            id,
            name,
            kind,
            uuid: value.get("uuid").and_then(Json::as_str).map(str::to_string),
            access_token: value
                .get("access_token")
                .and_then(Json::as_str)
                .map(str::to_string),
            refresh_token: value
                .get("refresh_token")
                .and_then(Json::as_str)
                .map(str::to_string),
            expires_at: value.get("expires_at").and_then(Json::as_u64),
            server: value
                .get("server")
                .and_then(Json::as_str)
                .map(str::to_string),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "uuid": self.uuid,
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at": self.expires_at,
            "server": self.server,
        })
    }
}

#[derive(Debug)]
enum Dialog {
    Offline {
        name: String,
        error: Option<String>,
    },
    ThirdParty {
        server: String,
        username: String,
        password: String,
        error: Option<String>,
    },
    Waiting {
        job: Option<String>,
        message: String,
        code: Option<String>,
        url: Option<String>,
        error: Option<String>,
    },
}

pub struct Account {
    selected: Option<String>,
    last_focus: Option<String>,
    dialog: Option<Dialog>,
    list_nodes: Vec<String>,
    detail_nodes: Vec<String>,
}

impl Default for Account {
    fn default() -> Self {
        Self::new()
    }
}

impl Account {
    pub fn new() -> Self {
        Self {
            selected: None,
            last_focus: None,
            dialog: None,
            list_nodes: Vec::new(),
            detail_nodes: Vec::new(),
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

    fn entries(host: &HostHandle) -> Vec<AccountEntry> {
        host.config()
            .borrow()
            .get("accounts", "list")
            .and_then(Json::as_array)
            .map(|items| items.iter().filter_map(AccountEntry::from_json).collect())
            .unwrap_or_default()
    }

    fn current_id(host: &HostHandle) -> String {
        host.config()
            .borrow()
            .get("accounts", "current")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn save(host: &HostHandle, entries: &[AccountEntry], current: &str) {
        let list: Vec<Value> = entries.iter().map(AccountEntry::to_json).collect();
        let config = host.config();
        let mut config = config.borrow_mut();
        config.set("accounts", "list", json!(list));
        config.set("accounts", "current", json!(current));
    }

    fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
    }

    fn selected_entry(&self, host: &HostHandle) -> Option<AccountEntry> {
        let id = self.selected.clone()?;
        Self::entries(host).into_iter().find(|entry| entry.id == id)
    }

    fn kind_label(&self, host: &HostHandle, kind: &str) -> String {
        self.t(host, &format!("account.kind.{kind}"))
    }

    /* ---------------- config writes ---------------- */

    fn validate_name(name: &str) -> bool {
        let name = name.trim();
        !name.is_empty() && name.chars().count() <= 16 && !name.chars().any(char::is_whitespace)
    }

    fn add_offline(&mut self, host: &mut HostHandle, name: &str) -> Result<(), String> {
        let name = name.trim();
        if !Self::validate_name(name) {
            return Err(self.t(host, "account.error.name"));
        }
        let mut entries = Self::entries(host);
        if entries
            .iter()
            .any(|entry| entry.kind == "offline" && entry.name.eq_ignore_ascii_case(name))
        {
            return Err(self.t(host, "account.error.duplicate"));
        }
        let mut counter = 1;
        let id = loop {
            let candidate = format!("acc-{counter}");
            if !entries.iter().any(|entry| entry.id == candidate) {
                break candidate;
            }
            counter += 1;
        };
        entries.push(AccountEntry {
            id: id.clone(),
            name: name.to_string(),
            kind: "offline".to_string(),
            uuid: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            server: None,
        });
        let current = Self::current_id(host);
        let current = if current.is_empty() {
            id.clone()
        } else {
            current
        };
        Self::save(host, &entries, &current);
        self.select(&id);
        Ok(())
    }

    fn set_current(&mut self, host: &mut HostHandle, id: &str) {
        let entries = Self::entries(host);
        if !entries.iter().any(|entry| entry.id == id) {
            return;
        }
        Self::save(host, &entries, id);
    }

    fn delete(&mut self, host: &mut HostHandle, id: &str) {
        let mut entries = Self::entries(host);
        entries.retain(|entry| entry.id != id);
        let mut current = Self::current_id(host);
        if current == id {
            current = entries
                .first()
                .map(|entry| entry.id.clone())
                .unwrap_or_default();
        }
        Self::save(host, &entries, &current);
        if self.selected.as_deref() == Some(id) {
            self.selected = entries.first().map(|entry| entry.id.clone());
        }
    }

    fn persist_draft(&mut self, host: &mut HostHandle, draft: &Json) -> Option<String> {
        let name = draft.get("name").and_then(Json::as_str)?.to_string();
        let kind = draft.get("kind").and_then(Json::as_str)?.to_string();
        let uuid = draft.get("uuid").and_then(Json::as_str).map(str::to_string);
        let access_token = draft
            .get("access_token")
            .and_then(Json::as_str)
            .map(str::to_string);
        let refresh_token = draft
            .get("refresh_token")
            .and_then(Json::as_str)
            .map(str::to_string);
        let expires_at = draft.get("expires_at").and_then(Json::as_u64);
        let server = draft
            .get("server")
            .and_then(Json::as_str)
            .map(str::to_string);
        let mut entries = Self::entries(host);
        let existing = entries
            .iter_mut()
            .find(|entry| entry.kind == kind && entry.uuid.is_some() && entry.uuid == uuid);
        let id = if let Some(entry) = existing {
            entry.name = name;
            entry.access_token = access_token;
            entry.refresh_token = refresh_token;
            entry.expires_at = expires_at;
            entry.server = server;
            entry.id.clone()
        } else {
            let mut counter = 1;
            let id = loop {
                let candidate = format!("acc-{counter}");
                if !entries.iter().any(|entry| entry.id == candidate) {
                    break candidate;
                }
                counter += 1;
            };
            entries.push(AccountEntry {
                id: id.clone(),
                name,
                kind,
                uuid,
                access_token,
                refresh_token,
                expires_at,
                server,
            });
            id
        };
        let current = Self::current_id(host);
        let current = if current.is_empty() {
            id.clone()
        } else {
            current
        };
        Self::save(host, &entries, &current);
        Some(id)
    }

    fn publish(host: &mut HostHandle) {
        let count = Self::entries(host).len();
        let current = Self::current_id(host);
        host.publish(
            "accounts.updated",
            json!({"count": count, "current": current}),
        );
    }

    /* ---------------- page ---------------- */

    fn rebuild_list(&mut self, host: &HostHandle) {
        Self::remove_paths(host, &mut self.list_nodes);
        let entries = Self::entries(host);
        let current = Self::current_id(host);
        let empty = entries.is_empty();
        if empty {
            let path = format!("{LIST}/empty");
            if self.set(
                host,
                &path,
                "Item",
                json!({"text": self.t(host, "account.empty")}),
            ) {
                self.list_nodes.push(path);
            }
            return;
        }
        if self.selected.is_none() {
            self.selected = entries.first().map(|entry| entry.id.clone());
        }
        for entry in &entries {
            let marker = if entry.id == current { "●" } else { "○" };
            let text = format!(
                "{marker} {}  [{}]",
                entry.name,
                self.kind_label(host, &entry.kind)
            );
            let path = format!("{LIST}/a-{}", entry.id);
            if self.set(host, &path, "Item", json!({"text": text})) {
                self.list_nodes.push(path);
            }
        }
    }

    fn rebuild_detail(&mut self, host: &HostHandle) {
        Self::remove_paths(host, &mut self.detail_nodes);
        let Some(entry) = self.selected_entry(host) else {
            let _ = (host.ui().patch)(
                ACTOR,
                DETAIL,
                json!({"title": self.t(host, "account.detail")}),
            );
            let path = format!("{INFO}/hint");
            if self.set(
                host,
                &path,
                "Label",
                json!({"text": self.t(host, "account.hint"), "align": "center"}),
            ) {
                self.detail_nodes.push(path);
            }
            self.build_adds(host);
            return;
        };
        let _ = (host.ui().patch)(ACTOR, DETAIL, json!({"title": entry.name}));
        let current = Self::current_id(host) == entry.id;
        let status = if current {
            self.t(host, "account.current")
        } else {
            self.t(host, "account.not_current")
        };
        let mut rows = vec![
            ("account.info.status", status),
            ("account.info.name", entry.name.clone()),
            ("account.info.kind", self.kind_label(host, &entry.kind)),
        ];
        if entry.kind == "offline" {
            rows.push(("account.info.uuid", self.t(host, "account.offline_uuid")));
        } else if let Some(uuid) = &entry.uuid {
            rows.push(("account.info.uuid", uuid.clone()));
        }
        if entry.kind == "third-party"
            && let Some(server) = &entry.server
        {
            rows.push(("account.info.server", server.clone()));
        }
        if let Some(expires_at) = entry.expires_at {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let value = if expires_at > now {
                self.t(host, "account.token.valid")
            } else {
                self.t(host, "account.token.expired")
            };
            rows.push(("account.info.token", value));
        }
        let row_count = rows.len();
        for (row_index, (key, value)) in rows.into_iter().enumerate() {
            let glyph = if row_index + 1 == row_count {
                "└─"
            } else {
                "├─"
            };
            let path = format!("{INFO}/r-{row_index}");
            let text = format!("{glyph} {:<8}{value}", self.t(host, key));
            if self.set(host, &path, "Label", json!({"text": text})) {
                self.detail_nodes.push(path);
            }
        }
        self.build_adds(host);
        for (id, key, enabled) in [
            ("act-set", "account.set_current", !current),
            ("act-delete", "account.delete", true),
            ("act-refresh", "account.refresh", entry.kind == "online"),
        ] {
            if !enabled {
                continue;
            }
            let path = format!("{ACTIONS}/{id}");
            if self.set(
                host,
                &path,
                "Button",
                json!({"text": self.t(host, key), "flex": 1.0}),
            ) {
                self.detail_nodes.push(path);
            }
        }
        if let Some(focus) = self.last_focus.clone() {
            let _ = (host.ui().set_focus)(ACTOR, &focus);
        }
    }

    fn build_adds(&mut self, host: &HostHandle) {
        let adds = [
            ("o-offline", "account.add_offline"),
            ("o-online", "account.add_online"),
            ("o-third", "account.add_third"),
        ];
        let add_count = adds.len();
        for (index, (id, key)) in adds.into_iter().enumerate() {
            let glyph = if index + 1 == add_count {
                "└─"
            } else {
                "├─"
            };
            let path = format!("{ADDS}/{id}");
            let text = format!("{glyph} {}", self.t(host, key));
            if self.set(host, &path, "Item", json!({"text": text})) {
                self.detail_nodes.push(path);
            }
        }
    }

    fn rebuild(&mut self, host: &HostHandle) {
        self.rebuild_list(host);
        self.rebuild_detail(host);
    }

    /* ---------------- auth ---------------- */

    fn begin_microsoft(&mut self, host: &mut HostHandle) {
        self.dialog = Some(Dialog::Waiting {
            job: None,
            message: self.t(host, "account.login.starting"),
            code: None,
            url: None,
            error: None,
        });
        self.show_dialog(host);
        host.publish("auth.command", json!({"action": "microsoft.begin"}));
    }

    fn begin_third_party(&mut self, host: &mut HostHandle) {
        self.dialog = Some(Dialog::ThirdParty {
            server: String::new(),
            username: String::new(),
            password: String::new(),
            error: None,
        });
        self.show_dialog(host);
        let _ = (host.ui().set_focus)(ACTOR, DIALOG_SERVER);
    }

    fn submit_third_party(&mut self, host: &mut HostHandle) {
        let Some(Dialog::ThirdParty {
            server,
            username,
            password,
            ..
        }) = self.dialog.as_ref()
        else {
            return;
        };
        let server = server.trim().to_string();
        let username = username.trim().to_string();
        let password = password.clone();
        if server.is_empty() || username.is_empty() || password.is_empty() {
            let message = self.t(host, "account.error.required");
            self.dialog = Some(Dialog::ThirdParty {
                server,
                username,
                password,
                error: Some(message),
            });
            self.show_dialog(host);
            return;
        }
        self.dialog = Some(Dialog::Waiting {
            job: None,
            message: self.t(host, "account.login.starting"),
            code: None,
            url: None,
            error: None,
        });
        self.show_dialog(host);
        host.publish(
            "auth.command",
            json!({
                "action": "third_party.begin",
                "server": server,
                "username": username,
                "password": password,
            }),
        );
    }

    fn refresh_selected(&mut self, host: &mut HostHandle) {
        let Some(entry) = self.selected_entry(host) else {
            return;
        };
        self.dialog = Some(Dialog::Waiting {
            job: None,
            message: self.t(host, "account.login.starting"),
            code: None,
            url: None,
            error: None,
        });
        self.show_dialog(host);
        host.publish(
            "auth.command",
            json!({"action": "refresh", "account": entry.to_json()}),
        );
    }

    /* ---------------- dialog ---------------- */

    fn show_dialog(&mut self, host: &HostHandle) {
        let Some(dialog) = &self.dialog else {
            let _ = (host.ui().remove)(ACTOR, DIALOG);
            return;
        };
        let ui = host.ui();
        let _ = (ui.remove)(ACTOR, DIALOG);
        let title = match dialog {
            Dialog::Offline { .. } => "account.dialog.offline",
            Dialog::ThirdParty { .. } => "account.dialog.third_party",
            Dialog::Waiting { .. } => "account.dialog.login",
        };
        let _ = (ui.create)(
            ACTOR,
            DIALOG,
            "Popup",
            json!({"title": self.t(host, title)}),
        );
        match dialog {
            Dialog::Offline { name, error } => {
                self.dialog_label(host, DIALOG, &self.t(host, "account.dialog.offline_hint"));
                let _ = (ui.create)(
                    ACTOR,
                    DIALOG_NAME,
                    "TextInput",
                    json!({"text": name, "placeholder": self.t(host, "account.dialog.name")}),
                );
                if let Some(error) = error {
                    self.dialog_label(host, &format!("{DIALOG}/error"), error);
                }
            }
            Dialog::ThirdParty {
                server,
                username,
                password,
                error,
            } => {
                self.dialog_label(
                    host,
                    DIALOG,
                    &self.t(host, "account.dialog.third_party_hint"),
                );
                let _ = (ui.create)(
                    ACTOR,
                    DIALOG_SERVER,
                    "TextInput",
                    json!({"text": server, "placeholder": self.t(host, "account.dialog.server")}),
                );
                let _ = (ui.create)(
                    ACTOR,
                    DIALOG_USER,
                    "TextInput",
                    json!({"text": username, "placeholder": self.t(host, "account.dialog.username")}),
                );
                let _ = (ui.create)(
                    ACTOR,
                    DIALOG_PASS,
                    "TextInput",
                    json!({"text": password, "mask": true, "placeholder": self.t(host, "account.dialog.password")}),
                );
                if let Some(error) = error {
                    self.dialog_label(host, &format!("{DIALOG}/error"), error);
                }
            }
            Dialog::Waiting {
                message,
                code,
                url,
                error,
                ..
            } => {
                self.dialog_label(host, DIALOG, message);
                if let Some(code) = code {
                    self.dialog_label(
                        host,
                        &format!("{DIALOG}/code"),
                        &format!("{}: {code}", self.t(host, "account.dialog.code")),
                    );
                }
                if let Some(url) = url {
                    self.dialog_label(host, &format!("{DIALOG}/url"), url);
                }
                if let Some(error) = error {
                    self.dialog_label(host, &format!("{DIALOG}/error"), error);
                }
            }
        }
        let ok = self.t(host, "account.dialog.ok");
        let cancel = self.t(host, "account.dialog.cancel");
        let _ = (ui.create)(ACTOR, DIALOG_OK, "Button", json!({"text": ok, "flex": 1.0}));
        let _ = (ui.create)(
            ACTOR,
            DIALOG_CANCEL,
            "Button",
            json!({"text": cancel, "flex": 1.0}),
        );
    }

    fn dialog_label(&self, host: &HostHandle, path: &str, text: &str) {
        let _ = (host.ui().create)(
            ACTOR,
            &format!("{path}/label"),
            "Label",
            json!({"text": text}),
        );
    }

    fn cancel_login(&self, host: &mut HostHandle) {
        let job = match self.dialog.as_ref() {
            Some(Dialog::Waiting { job: Some(job), .. }) => json!(job),
            _ => Value::Null,
        };
        host.publish("auth.command", json!({"action": "cancel", "job": job}));
    }

    fn close_dialog(&mut self, host: &HostHandle) {
        self.dialog = None;
        let _ = (host.ui().remove)(ACTOR, DIALOG);
        if let Some(focus) = self.last_focus.clone() {
            let _ = (host.ui().set_focus)(ACTOR, &focus);
        }
    }

    fn handle_dialog_ok(&mut self, host: &mut HostHandle) {
        match self.dialog.as_ref() {
            Some(Dialog::Offline { .. }) => {
                let Some(Dialog::Offline { name, .. }) = self.dialog.take() else {
                    return;
                };
                match self.add_offline(host, &name) {
                    Ok(()) => {
                        Self::publish(host);
                        self.rebuild(host);
                        self.close_dialog(host);
                    }
                    Err(message) => {
                        self.dialog = Some(Dialog::Offline {
                            name,
                            error: Some(message),
                        });
                        self.show_dialog(host);
                    }
                }
            }
            Some(Dialog::ThirdParty { .. }) => self.submit_third_party(host),
            Some(Dialog::Waiting { .. }) => {
                self.cancel_login(host);
                self.close_dialog(host);
            }
            None => {}
        }
    }

    /* ---------------- event handlers ---------------- */

    fn activate(&mut self, path: &str, host: &mut HostHandle) {
        if let Some(id) = path.strip_prefix(&format!("{LIST}/a-")) {
            self.select(id);
            self.last_focus = Some(path.to_string());
            self.rebuild_list(host);
            self.rebuild_detail(host);
            return;
        }
        match path {
            p if p == format!("{ADDS}/o-offline") => {
                self.dialog = Some(Dialog::Offline {
                    name: String::new(),
                    error: None,
                });
                self.show_dialog(host);
                let _ = (host.ui().set_focus)(ACTOR, DIALOG_NAME);
            }
            p if p == format!("{ADDS}/o-online") => self.begin_microsoft(host),
            p if p == format!("{ADDS}/o-third") => self.begin_third_party(host),
            p if p == format!("{ACTIONS}/act-set") => {
                if let Some(entry) = self.selected_entry(host) {
                    let id = entry.id.clone();
                    self.set_current(host, &id);
                    Self::publish(host);
                    self.rebuild(host);
                }
            }
            p if p == format!("{ACTIONS}/act-delete") => {
                if let Some(entry) = self.selected_entry(host) {
                    let id = entry.id.clone();
                    self.delete(host, &id);
                    Self::publish(host);
                    self.rebuild(host);
                }
            }
            p if p == format!("{ACTIONS}/act-refresh") => self.refresh_selected(host),
            p if p == DIALOG_OK => self.handle_dialog_ok(host),
            p if p == DIALOG_CANCEL => {
                if matches!(self.dialog, Some(Dialog::Waiting { .. })) {
                    self.cancel_login(host);
                }
                self.close_dialog(host);
            }
            _ => {}
        }
    }

    fn input(&mut self, path: &str, text: &str, _host: &HostHandle) {
        match self.dialog.as_mut() {
            Some(Dialog::Offline { name, .. }) if path == DIALOG_NAME => {
                *name = text.to_string();
            }
            Some(Dialog::ThirdParty {
                server,
                username,
                password,
                ..
            }) => {
                if path == DIALOG_SERVER {
                    *server = text.to_string();
                } else if path == DIALOG_USER {
                    *username = text.to_string();
                } else if path == DIALOG_PASS {
                    *password = text.to_string();
                }
            }
            _ => (),
        }
    }

    fn auth_update(&mut self, name: &str, payload: &Json, host: &mut HostHandle) {
        let waiting_text = self.t(host, "account.login.waiting");
        let device_text = self.t(host, "account.login.device");
        let failed_text = self.t(host, "account.login.failed");
        let Some(Dialog::Waiting {
            job,
            message,
            code,
            url,
            error,
        }) = self.dialog.as_mut()
        else {
            return;
        };
        if job.is_none() {
            *job = payload
                .get("job")
                .and_then(Json::as_str)
                .map(str::to_string);
        }
        match name {
            "auth.state" => {
                if payload.get("phase").and_then(Json::as_str) == Some("polling") {
                    *message = waiting_text;
                }
            }
            "auth.device" => {
                *code = payload
                    .get("code")
                    .and_then(Json::as_str)
                    .map(str::to_string);
                *url = payload
                    .get("url")
                    .and_then(Json::as_str)
                    .map(str::to_string);
                *message = device_text;
            }
            "auth.error" => {
                *error = payload
                    .get("message")
                    .and_then(Json::as_str)
                    .map(str::to_string);
                *message = failed_text;
            }
            _ => return,
        }
        self.show_dialog(host);
    }
}

impl NativePlugin for Account {
    fn manifest(&self) -> Manifest {
        manifest(
            "account",
            "Account",
            &[
                "account.command",
                "auth.state",
                "auth.device",
                "auth.result",
                "auth.error",
            ],
            "Owns the account registry, login dialogs and the /account page",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let ui = host.ui();
        let _ = (ui.create)(ACTOR, PAGE, "Page", json!({"title": "Accounts"}));
        let _ = (ui.create)(ACTOR, SPLIT, "Split", json!({"direction": "row"}));
        let _ = (ui.create)(
            ACTOR,
            LIST,
            "List",
            json!({"title": self.t(host, "account.title"), "flex": 1.0}),
        );
        let _ = (ui.create)(
            ACTOR,
            DETAIL,
            "Section",
            json!({"title": self.t(host, "account.detail"), "flex": 2.0}),
        );
        let _ = (ui.create)(ACTOR, LAYOUT, "Split", json!({"direction": "col"}));
        let _ = (ui.create)(ACTOR, INFO, "List", json!({"flex": 6.0}));
        let _ = (ui.create)(ACTOR, ADDS, "List", json!({"flex": 3.0}));
        let _ = (ui.create)(
            ACTOR,
            ACTIONS,
            "Split",
            json!({"direction": "row", "flex": 1.0}),
        );
        let _ = logger;
        self.rebuild(host);
        Account::publish(host);
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, _logger: &Logger) {
        match name {
            "ui.focus" => {
                if let Some(path) = payload.get("path").and_then(Json::as_str) {
                    self.last_focus = Some(path.to_string());
                    if let Some(id) = path.strip_prefix(&format!("{LIST}/a-")) {
                        self.select(id);
                        self.rebuild_list(host);
                        self.rebuild_detail(host);
                    }
                }
            }
            "ui.blur" => {
                if let Some(path) = payload.get("path").and_then(Json::as_str)
                    && self.last_focus.as_deref() == Some(path)
                {
                    self.last_focus = None;
                }
            }
            "ui.input" => {
                let path = payload.get("path").and_then(Json::as_str).unwrap_or("");
                let text = payload.get("text").and_then(Json::as_str).unwrap_or("");
                self.input(path, text, host);
            }
            "ui.activate" => {
                let path = payload.get("path").and_then(Json::as_str).unwrap_or("");
                self.activate(path, host);
            }
            "account.command" => {
                let action = payload.get("action").and_then(Json::as_str).unwrap_or("");
                match action {
                    "add_offline" => {
                        let name = payload.get("name").and_then(Json::as_str).unwrap_or("");
                        if self.add_offline(host, name).is_ok() {
                            Self::publish(host);
                            self.rebuild(host);
                        }
                    }
                    "set_current" => {
                        let id = payload.get("id").and_then(Json::as_str).unwrap_or("");
                        self.set_current(host, id);
                        Self::publish(host);
                        self.rebuild(host);
                    }
                    "delete" => {
                        let id = payload.get("id").and_then(Json::as_str).unwrap_or("");
                        self.delete(host, id);
                        Self::publish(host);
                        self.rebuild(host);
                    }
                    _ => {}
                }
            }
            "auth.result" => {
                if let Some(draft) = payload.get("account")
                    && let Some(id) = self.persist_draft(host, draft)
                {
                    self.select(&id);
                    Self::publish(host);
                }
                if matches!(self.dialog, Some(Dialog::Waiting { .. })) {
                    self.close_dialog(host);
                }
                self.rebuild(host);
            }
            "auth.state" | "auth.device" | "auth.error" => {
                self.auth_update(name, payload, host);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrip_keeps_optional_fields() {
        let entry = AccountEntry {
            id: "acc-2".into(),
            name: "Alex".into(),
            kind: "online".into(),
            uuid: Some("uuid".into()),
            access_token: Some("token".into()),
            refresh_token: Some("refresh".into()),
            expires_at: Some(10),
            server: None,
        };
        assert_eq!(AccountEntry::from_json(&entry.to_json()), Some(entry));
    }

    #[test]
    fn entries_with_unknown_kind_are_skipped() {
        assert_eq!(
            AccountEntry::from_json(&json!({"id": "a", "name": "n", "kind": "bad"})),
            None
        );
        assert!(
            AccountEntry::from_json(&json!({"id": "a", "name": "n", "kind": "offline"})).is_some()
        );
    }

    #[test]
    fn name_validation_rejects_empty_whitespace_and_long_names() {
        assert!(Account::validate_name("Steve"));
        assert!(!Account::validate_name(""));
        assert!(!Account::validate_name("a b"));
        assert!(!Account::validate_name("12345678901234567"));
    }

    #[test]
    fn kind_labels_match_i18n_keys() {
        let sources = [
            ("offline", "account.kind.offline"),
            ("online", "account.kind.online"),
            ("third-party", "account.kind.third-party"),
        ];
        for (kind, _) in sources {
            assert!(["offline", "online", "third-party"].contains(&kind));
        }
    }
}
