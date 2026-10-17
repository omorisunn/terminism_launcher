use rhai::{Array, Dynamic, Engine, FnPtr, Map, Scope};
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub use crate::config::ConfigStore;
use crate::logger::{LogStore, Logger};
use crate::manifest::{self, Manifest};

pub type Json = serde_json::Value;

pub type I18nStore = Rc<RefCell<HashMap<String, String>>>;

// parsed startup arguments, filled once by the cli plugin at boot
#[derive(Debug, Clone, Default)]
pub struct CliTable {
    pub command: String,
    pub subcommand: Option<String>,
    pub positionals: Vec<String>,
    pub options: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Delivery {
    pub target: String,
    pub event: String,
    pub payload: Json,
}

// closure field types are fixed by the spec
#[allow(clippy::type_complexity)]
pub struct UiApi {
    pub create: Rc<dyn Fn(&str, &str, &str, Json) -> bool>,
    pub patch: Rc<dyn Fn(&str, &str, Json) -> bool>,
    pub remove: Rc<dyn Fn(&str, &str) -> bool>,
    pub remove_owned: Rc<dyn Fn(&str)>,
    pub set_hover: Rc<dyn Fn(&str, &str) -> bool>,
    pub set_focus: Rc<dyn Fn(&str, &str) -> bool>,
}

pub struct DataStore(pub HashMap<String, HashMap<String, Json>>);

#[derive(Debug, Clone)]
pub enum PluginState {
    Active,
    Disabled(String),
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub state: String,
    pub reason: String,
    pub system: bool,
    pub uptime_secs: u64,
    // runtime kind: native | rhai | wasm
    pub kind: &'static str,
    // lineage parent id derived from the logger tag chain
    pub parent: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("unknown plugin: {0}")]
    UnknownPlugin(String),
    #[error("operation not allowed on native plugin: {0}")]
    NativeNotAllowed(String),
    #[error("already started: {0}")]
    AlreadyStarted(String),
    #[error("service not found: {0}")]
    ServiceUnknown(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Plugin(#[from] crate::plugin::PluginError),
    #[error("script error: {0}")]
    Script(String),
}

pub trait NativePlugin {
    fn manifest(&self) -> Manifest;
    fn init(&mut self, host: &mut HostHandle, logger: &Logger);
    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger);
    #[allow(unused_variables)]
    fn on_tick(&mut self, host: &mut HostHandle, logger: &Logger) {}
}

#[derive(Clone)]
pub struct PluginReg {
    pub manifest: Manifest,
    pub state: PluginState,
    pub logger: Logger,
    pub started: Instant,
    pub kind: &'static str,
    pub dir: Option<PathBuf>,
}

impl PluginReg {
    pub fn is_active(&self) -> bool {
        matches!(self.state, PluginState::Active)
    }
    pub fn system(&self) -> bool {
        self.logger.tag == "init.manager" || self.logger.tag.starts_with("init.manager.")
    }
    fn info(&self, id: &str) -> PluginInfo {
        let (state, reason) = match &self.state {
            PluginState::Active => ("active", ""),
            PluginState::Disabled(reason) => ("disabled", reason.as_str()),
        };
        PluginInfo {
            id: id.into(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            state: state.into(),
            reason: reason.into(),
            system: self.system(),
            kind: self.kind,
            parent: self
                .logger
                .tag
                .rsplit_once('.')
                .and_then(|(parent, _)| parent.rsplit('.').next())
                .map(str::to_string),
            uptime_secs: if self.is_active() {
                self.started.elapsed().as_secs()
            } else {
                0
            },
        }
    }
}

fn plugin_snapshot(registry: &PluginRegistry) -> Vec<PluginInfo> {
    let mut infos: Vec<_> = registry
        .iter()
        .map(|(id, record)| record.info(id))
        .collect();
    infos.sort_by(|a, b| a.id.cmp(&b.id));
    infos
}

pub type PluginRegistry = HashMap<String, PluginReg>;

pub(crate) enum Cmd {
    Publish {
        name: String,
        payload: Json,
    },
    Svc {
        sender: String,
        service: String,
        fn_name: String,
        args: Json,
    },
}

#[allow(clippy::large_enum_variant)]
enum Runtime {
    Native(Box<dyn NativePlugin>),
    Script {
        engine: Engine,
        ast: rhai::AST,
        scope: Scope<'static>,
    },
    Wasm {
        store: Box<wasmi::Store<WasmHost>>,
        instance: wasmi::Instance,
    },
}

pub struct WasmHost {
    pub(crate) limits: wasmi::StoreLimits,
    pub(crate) actor: String,
    pub(crate) log: Logger,
    pub(crate) queue: Rc<RefCell<VecDeque<Cmd>>>,
    pub(crate) data: Rc<RefCell<DataStore>>,
    pub(crate) config: Rc<RefCell<ConfigStore>>,
}

fn json_to_dynamic(value: &Json) -> Dynamic {
    rhai::serde::to_dynamic(value).unwrap_or(Dynamic::UNIT)
}

fn dynamic_to_json(value: &Dynamic) -> Json {
    rhai::serde::from_dynamic(value).unwrap_or(Json::Null)
}

fn manifest_json(manifest: &Manifest) -> Json {
    json!({
        "id": manifest.id,
        "name": manifest.name,
        "version": manifest.version,
        "description": manifest.description,
        "provides": manifest.provides,
        "subscribe": manifest.subscribe,
        "ui_grant": manifest.ui_grant,
        "data_grant": manifest.data_grant,
    })
}

// entries are compile-time constants, so FnPtr::new cannot fail
fn ns_map(entries: &[(&str, &str)]) -> Map {
    let mut map = Map::new();
    for (key, fp) in entries {
        map.insert((*key).into(), Dynamic::from(FnPtr::new(*fp).unwrap()));
    }
    map
}

pub struct HostHandle<'a> {
    host: &'a mut PluginHost,
}

impl HostHandle<'_> {
    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.host.plugin_infos()
    }

    pub fn manifest_of(&self, id: &str) -> Option<Json> {
        self.host.manifest_of(id)
    }

    pub fn ui(&self) -> Rc<UiApi> {
        self.host.ui()
    }

    pub fn config(&self) -> Rc<RefCell<ConfigStore>> {
        self.host.config.clone()
    }

    pub fn data(&self) -> Rc<RefCell<DataStore>> {
        self.host.data.clone()
    }

    // native plugins publish with the same reserved-prefix rules as scripts
    pub fn publish(&mut self, name: &str, payload: Json) {
        if manifest::RESERVED_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
        {
            self.host
                .kernel_log
                .log(&format!("reserved event prefix dropped: {name}"));
            return;
        }
        self.host.publish(name, payload);
    }

    pub fn start_native(&mut self, id: &str, logger: &Logger) -> Result<(), HostError> {
        self.host.start_native(id, logger)
    }

    pub fn load_plugin(&mut self, dir: &Path, parent: &Logger) -> Result<String, HostError> {
        self.host.load_plugin(dir, parent)
    }

    pub fn disable(&mut self, id: &str) -> Result<(), HostError> {
        self.host.disable(id)
    }

    pub fn reload(&mut self, id: &str) -> Result<(), HostError> {
        self.host.reload(id)
    }

    pub fn lineage_logger(&self, id: &str) -> Option<Logger> {
        self.host.lineage_logger(id)
    }

    // the directory a script plugin was loaded from; natives have none
    pub fn plugin_dir(&self, id: &str) -> Option<PathBuf> {
        self.host.registry.borrow().get(id)?.dir.clone()
    }

    pub fn set_i18n(&mut self, store: I18nStore) {
        self.host.set_i18n(store)
    }

    // natives read the shared catalog to localize their own UI text
    pub fn i18n(&self) -> I18nStore {
        self.host.i18n.clone()
    }

    pub fn set_cli(&mut self, table: CliTable) {
        self.host.set_cli(table);
    }

    // native plugins are host code and read the whole table
    pub fn cli(&self) -> Rc<RefCell<CliTable>> {
        self.host.cli.clone()
    }
}

pub struct PluginHost {
    sink: LogStore,
    kernel_log: Logger,
    data: Rc<RefCell<DataStore>>,
    config: Rc<RefCell<ConfigStore>>,
    registry: Rc<RefCell<PluginRegistry>>,
    ui: Rc<RefCell<Rc<UiApi>>>,
    i18n: I18nStore,
    cli: Rc<RefCell<CliTable>>,
    queue: Rc<RefCell<VecDeque<Cmd>>>,
    runtimes: HashMap<String, Runtime>,
    pending: HashMap<String, Box<dyn NativePlugin>>,
    subscriptions: Vec<(String, String)>,
    services: HashMap<String, String>,
    order: Vec<String>,
}

impl PluginHost {
    pub fn new(
        sink: LogStore,
        data: Rc<RefCell<DataStore>>,
        config: Rc<RefCell<ConfigStore>>,
        registry: Rc<RefCell<PluginRegistry>>,
    ) -> Self {
        Self {
            kernel_log: Logger::new("kernel", sink.clone()),
            sink,
            data,
            config,
            registry,
            ui: Rc::new(RefCell::new(neutral_ui())),
            i18n: Default::default(),
            cli: Default::default(),
            queue: Default::default(),
            runtimes: HashMap::new(),
            pending: HashMap::new(),
            subscriptions: Vec::new(),
            services: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn register_native(&mut self, plugin: Box<dyn NativePlugin>) {
        self.pending.insert(plugin.manifest().id, plugin);
    }
    pub fn set_ui(&mut self, ui: Rc<UiApi>) {
        *self.ui.borrow_mut() = ui;
    }
    pub fn set_i18n(&mut self, store: I18nStore) {
        self.i18n = store;
    }
    pub fn set_cli(&mut self, table: CliTable) {
        *self.cli.borrow_mut() = table;
    }
    pub fn boot(&mut self) -> Result<(), HostError> {
        self.start_native("init", &Logger::new("init", self.sink.clone()))
    }
    fn lineage_logger(&self, id: &str) -> Option<Logger> {
        self.registry.borrow().get(id).map(|r| r.logger.clone())
    }

    pub fn deliver(&mut self, target: &str, event: &str, payload: Json) {
        let logger = self
            .registry
            .borrow()
            .get(target)
            .filter(|r| r.is_active())
            .map(|r| r.logger.clone());
        let Some(logger) = logger else {
            return;
        };
        let Some(mut runtime) = self.runtimes.remove(target) else {
            return;
        };
        let crash = match &mut runtime {
            Runtime::Native(plugin) => {
                plugin.on_event(event, &payload, &mut HostHandle { host: self }, &logger);
                None
            }
            Runtime::Script { engine, ast, scope } => engine
                .call_fn::<Dynamic>(
                    scope,
                    ast,
                    "on_event",
                    (event.to_string(), json_to_dynamic(&payload)),
                )
                .err()
                .map(|e| e.to_string()),
            Runtime::Wasm { store, instance } => {
                crate::wasm::invoke(store, *instance, event, &payload).err()
            }
        };
        self.runtimes.insert(target.into(), runtime);
        if let Some(error) = crash {
            let _ = self.disable_reason(target, &error);
            self.kernel_log
                .log(&format!("script crashed: {target}: {error}"));
            self.publish("plugin.crashed", json!({"id":target,"error":error}));
        }
    }

    pub fn publish(&mut self, name: &str, payload: Json) {
        self.queue.borrow_mut().push_back(Cmd::Publish {
            name: name.into(),
            payload,
        });
    }
    pub fn drain(&mut self) {
        for _ in 0..100 {
            let changes = self.config.borrow_mut().take_changes();
            for c in changes {
                self.publish(
                    "config.updated",
                    json!({"section":c.section,"key":c.key,"value":c.value}),
                );
            }
            let cmds = std::mem::take(&mut *self.queue.borrow_mut());
            if cmds.is_empty() {
                break;
            }
            for cmd in cmds {
                self.execute(cmd);
            }
        }
        if !self.queue.borrow().is_empty() {
            self.queue.borrow_mut().clear();
            self.kernel_log.log("event flood, dropping");
        }
        let result = self.config.borrow_mut().flush();
        if let Err(e) = result {
            self.kernel_log.log(&format!("config write failed: {e}"));
        }
    }
    fn execute(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Publish { name, payload } => {
                let targets: Vec<_> = self
                    .subscriptions
                    .iter()
                    .filter(|(_, e)| *e == name)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in targets {
                    self.deliver(&id, &name, payload.clone());
                }
            }
            Cmd::Svc {
                sender,
                service,
                fn_name,
                args,
            } => self.execute_svc(sender, service, fn_name, args),
        }
    }
    fn execute_svc(&mut self, sender: String, service: String, fn_name: String, args: Json) {
        let Some(provider) = self.services.get(&service).cloned() else {
            self.kernel_log.log(&format!("svc unknown: {service}"));
            return;
        };
        if provider == sender {
            self.kernel_log.log("svc self-send denied");
            return;
        }
        let active = self
            .registry
            .borrow()
            .get(&provider)
            .is_some_and(PluginReg::is_active);
        if !active {
            return;
        }
        let Some(mut runtime) = self.runtimes.remove(&provider) else {
            return;
        };
        let error = match &mut runtime {
            Runtime::Script { engine, ast, scope } => engine
                .call_fn::<Dynamic>(scope, ast, &fn_name, (json_to_dynamic(&args),))
                .err()
                .map(|e| e.to_string()),
            Runtime::Native(_) => {
                self.kernel_log
                    .log(&format!("svc native provider unsupported: {service}"));
                None
            }
            Runtime::Wasm { .. } => {
                self.kernel_log
                    .log(&format!("svc wasm provider unsupported: {service}"));
                None
            }
        };
        self.runtimes.insert(provider.clone(), runtime);
        if let Some(error) = error {
            let _ = self.disable_reason(&provider, &error);
            self.publish("plugin.crashed", json!({"id":provider,"error":error}));
        }
    }
    pub fn tick(&mut self) {
        let ids: Vec<_> = self
            .registry
            .borrow()
            .iter()
            .filter(|(_, r)| r.kind == "native" && r.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(Runtime::Native(mut plugin)) = self.runtimes.remove(&id) else {
                continue;
            };
            let logger = self.registry.borrow()[&id].logger.clone();
            plugin.on_tick(&mut HostHandle { host: self }, &logger);
            self.runtimes.insert(id, Runtime::Native(plugin));
        }
    }
    pub fn shutdown(&mut self) {
        for id in self.order.clone().into_iter().rev() {
            if self
                .registry
                .borrow()
                .get(&id)
                .is_some_and(|r| r.kind != "native" && r.is_active())
            {
                let _ = self.disable(&id);
            }
        }
    }
    fn preflight(&self, manifest: &Manifest) -> Result<(), HostError> {
        if self.registry.borrow().contains_key(&manifest.id) {
            return Err(HostError::AlreadyStarted(manifest.id.clone()));
        }
        if let Some(service) = manifest
            .provides
            .iter()
            .find(|s| self.services.contains_key(*s))
        {
            return Err(HostError::Script(format!(
                "service already provided: {service}"
            )));
        }
        Ok(())
    }
    fn link(&mut self, id: &str) {
        let record = self.registry.borrow();
        let manifest = &record[id].manifest;
        for event in &manifest.subscribe {
            self.subscriptions.push((id.into(), event.clone()));
        }
        for service in &manifest.provides {
            self.services.insert(service.clone(), id.into());
        }
        if !self.order.iter().any(|p| p == id) {
            self.order.push(id.into());
        }
    }
    pub fn start_native(&mut self, id: &str, logger: &Logger) -> Result<(), HostError> {
        if self.registry.borrow().contains_key(id) {
            return Err(HostError::AlreadyStarted(id.into()));
        }
        let Some(mut plugin) = self.pending.remove(id) else {
            return Err(HostError::UnknownPlugin(id.into()));
        };
        let manifest = plugin.manifest();
        if let Err(e) = self.preflight(&manifest) {
            self.pending.insert(id.into(), plugin);
            return Err(e);
        }
        self.registry.borrow_mut().insert(
            id.into(),
            PluginReg {
                manifest,
                state: PluginState::Active,
                logger: logger.clone(),
                started: Instant::now(),
                kind: "native",
                dir: None,
            },
        );
        self.link(id);
        plugin.init(&mut HostHandle { host: self }, logger);
        self.runtimes.insert(id.into(), Runtime::Native(plugin));
        Ok(())
    }
    pub fn load_plugin(&mut self, dir: &Path, parent: &Logger) -> Result<String, HostError> {
        let descriptor = match crate::plugin::PluginDescriptor::read(dir) {
            Ok(descriptor) => descriptor,
            Err(e) => {
                let id = match &e {
                    crate::plugin::PluginError::MissingEntry { id, .. } => id.clone(),
                    _ => dir
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                };
                self.publish("plugin.failed", json!({"id":id,"error":e.to_string()}));
                return Err(e.into());
            }
        };
        let logger = parent.child(&descriptor.manifest.id);
        self.load_descriptor(descriptor, &logger)
    }
    fn load_descriptor(
        &mut self,
        descriptor: crate::plugin::PluginDescriptor,
        logger: &Logger,
    ) -> Result<String, HostError> {
        let entry_check = descriptor.check_entry();
        let crate::plugin::PluginDescriptor {
            manifest,
            dir,
            entry,
        } = descriptor;
        let id = manifest.id.clone();
        let preflight = self.preflight(&manifest);
        if matches!(&preflight, Err(HostError::AlreadyStarted(_))) {
            return preflight.map(|()| id);
        }
        let kind = if manifest.kind == "wasm" {
            "wasm"
        } else {
            "rhai"
        };
        self.registry.borrow_mut().insert(
            id.clone(),
            PluginReg {
                manifest: manifest.clone(),
                state: PluginState::Active,
                logger: logger.clone(),
                started: Instant::now(),
                kind,
                dir: Some(dir),
            },
        );
        let queue_len = self.queue.borrow().len();
        let config = self.config.borrow().clone();
        let data = self.data.borrow().0.get(&id).cloned();
        let runtime = preflight
            .and_then(|()| entry_check.map_err(HostError::from))
            .and_then(|()| self.create_runtime(&entry, &manifest, logger));
        match runtime {
            Ok(runtime) => {
                self.runtimes.insert(id.clone(), runtime);
                self.link(&id);
            }
            Err(e) => {
                self.unlink(&id);
                self.registry.borrow_mut().get_mut(&id).unwrap().state =
                    PluginState::Disabled(e.to_string());
                self.queue.borrow_mut().truncate(queue_len);
                *self.config.borrow_mut() = config;
                let mut store = self.data.borrow_mut();
                if let Some(data) = data {
                    store.0.insert(id.clone(), data);
                } else {
                    store.0.remove(&id);
                }
                drop(store);
                self.kernel_log
                    .log(&format!("plugin load failed: {id}: {e}"));
                self.publish("plugin.failed", json!({"id":id,"error":e.to_string()}));
                return Err(e);
            }
        }
        self.kernel_log.log(&format!(
            "{} loaded: {id}",
            if kind == "wasm" { "wasm" } else { "script" }
        ));
        self.publish("plugin.loaded", json!({"id":id,"name":manifest.name}));
        Ok(id)
    }
    fn create_runtime(
        &self,
        entry: &Path,
        manifest: &Manifest,
        logger: &Logger,
    ) -> Result<Runtime, HostError> {
        if manifest.kind == "wasm" {
            let state = crate::wasm::host_state(
                &manifest.id,
                logger.clone(),
                self.queue.clone(),
                self.data.clone(),
                self.config.clone(),
            );
            let (store, instance) = crate::wasm::instantiate(&std::fs::read(entry)?, state)
                .map_err(HostError::Script)?;
            Ok(Runtime::Wasm {
                store: Box::new(store),
                instance,
            })
        } else {
            let mut engine = Engine::new();
            engine.set_max_expr_depths(64, 64);
            engine.set_max_operations(1_000_000);
            engine.set_max_call_levels(64);
            let log = logger.clone();
            engine.on_print(move |text| log.log(text));
            let log = logger.clone();
            engine.on_debug(move |text, _, _| log.log(text));
            self.register_script_api(&mut engine, &manifest.id, manifest);
            let ast = engine
                .compile(std::fs::read_to_string(entry)?)
                .map_err(|e| HostError::Script(e.to_string()))?;
            let mut scope = Scope::new();
            self.push_api_scope(&mut scope);
            let _ = engine
                .call_fn::<Dynamic>(&mut scope, &ast, "init", ())
                .map_err(|e| HostError::Script(e.to_string()))?;
            Ok(Runtime::Script { engine, ast, scope })
        }
    }
    pub fn disable(&mut self, id: &str) -> Result<(), HostError> {
        self.disable_reason(id, "disabled")
    }
    fn disable_reason(&mut self, id: &str, reason: &str) -> Result<(), HostError> {
        {
            let mut registry = self.registry.borrow_mut();
            let record = registry
                .get_mut(id)
                .ok_or_else(|| HostError::UnknownPlugin(id.into()))?;
            if record.kind == "native" {
                return Err(HostError::NativeNotAllowed(id.into()));
            }
            record.state = PluginState::Disabled(reason.into());
        }
        self.runtimes.remove(id);
        self.unlink(id);
        self.kernel_log
            .log(&format!("script disabled: {id}: {reason}"));
        self.publish("plugin.disabled", json!({"id":id,"reason":reason}));
        Ok(())
    }
    pub fn reload(&mut self, id: &str) -> Result<(), HostError> {
        let record = self
            .registry
            .borrow()
            .get(id)
            .cloned()
            .ok_or_else(|| HostError::UnknownPlugin(id.into()))?;
        let dir = record
            .dir
            .ok_or_else(|| HostError::NativeNotAllowed(id.into()))?;
        let descriptor = crate::plugin::PluginDescriptor::read(&dir)?;
        if descriptor.manifest.id != id {
            return Err(HostError::Script("reload cannot change plugin id".into()));
        }
        self.unlink(id);
        self.runtimes.remove(id);
        self.registry.borrow_mut().remove(id);
        self.load_descriptor(descriptor, &record.logger)?;
        self.kernel_log.log(&format!("script reloaded: {id}"));
        Ok(())
    }
    fn unlink(&mut self, id: &str) {
        self.subscriptions.retain(|(p, _)| p != id);
        self.services.retain(|_, p| p != id);
        (self.ui().remove_owned)(id);
    }
    fn push_api_scope(&self, scope: &mut Scope) {
        scope.push_constant(
            "ui",
            ns_map(&[
                ("create", "__ui_create"),
                ("patch", "__ui_patch"),
                ("remove", "__ui_remove"),
                ("focus", "__ui_focus"),
            ]),
        );
        scope.push_constant("events", ns_map(&[("publish", "__events_publish")]));
        scope.push_constant("svc", ns_map(&[("send", "__svc_send")]));
        scope.push_constant(
            "data",
            ns_map(&[
                ("set", "__data_set"),
                ("get", "__data_get"),
                ("remove", "__data_remove"),
                ("keys", "__data_keys"),
                ("get_in", "__data_get_in"),
            ]),
        );
        scope.push_constant(
            "config",
            ns_map(&[("get", "__config_get"), ("set", "__config_set")]),
        );
        scope.push_constant(
            "host",
            ns_map(&[
                ("info", "__host_info"),
                ("now", "__host_now"),
                ("manifest", "__host_manifest"),
            ]),
        );
        scope.push_constant("i18n", ns_map(&[("t", "__i18n_t")]));
        scope.push_constant(
            "cli",
            ns_map(&[("get", "__cli_get"), ("flag", "__cli_flag")]),
        );
    }

    fn register_script_api(&self, engine: &mut Engine, id: &str, manifest: &Manifest) {
        let actor = id.to_string();
        let ui_cell = self.ui.clone();
        engine.register_fn(
            "__ui_create",
            move |_: &mut Map, path: &str, kind: &str, props: Map| -> bool {
                let api = ui_cell.borrow().clone();
                (api.create)(&actor, path, kind, dynamic_to_json(&Dynamic::from(props)))
            },
        );
        let actor = id.to_string();
        let ui_cell = self.ui.clone();
        engine.register_fn(
            "__ui_patch",
            move |_: &mut Map, path: &str, props: Map| -> bool {
                let api = ui_cell.borrow().clone();
                (api.patch)(&actor, path, dynamic_to_json(&Dynamic::from(props)))
            },
        );
        let actor = id.to_string();
        let ui_cell = self.ui.clone();
        engine.register_fn("__ui_remove", move |_: &mut Map, path: &str| -> bool {
            let api = ui_cell.borrow().clone();
            (api.remove)(&actor, path)
        });
        let actor = id.to_string();
        let ui_cell = self.ui.clone();
        engine.register_fn("__ui_focus", move |_: &mut Map, path: &str| -> bool {
            let api = ui_cell.borrow().clone();
            (api.set_focus)(&actor, path)
        });
        let queue = self.queue.clone();
        let kernel_log = self.kernel_log.clone();
        engine.register_fn(
            "__events_publish",
            move |_: &mut Map, name: &str, payload: Map| {
                if manifest::RESERVED_PREFIXES
                    .iter()
                    .any(|p| name.starts_with(p))
                {
                    kernel_log.log(&format!("reserved event prefix dropped: {name}"));
                    return;
                }
                queue.borrow_mut().push_back(Cmd::Publish {
                    name: name.to_string(),
                    payload: dynamic_to_json(&Dynamic::from(payload)),
                });
            },
        );
        let actor = id.to_string();
        let queue = self.queue.clone();
        engine.register_fn(
            "__svc_send",
            move |_: &mut Map, service: &str, fn_name: &str, args: Dynamic| {
                queue.borrow_mut().push_back(Cmd::Svc {
                    sender: actor.clone(),
                    service: service.to_string(),
                    fn_name: fn_name.to_string(),
                    args: dynamic_to_json(&args),
                });
            },
        );
        let actor = id.to_string();
        let data = self.data.clone();
        engine.register_fn(
            "__data_set",
            move |_: &mut Map, key: &str, value: Dynamic| -> bool {
                data.borrow_mut()
                    .0
                    .entry(actor.clone())
                    .or_default()
                    .insert(key.to_string(), dynamic_to_json(&value));
                true
            },
        );
        let actor = id.to_string();
        let data = self.data.clone();
        engine.register_fn("__data_get", move |_: &mut Map, key: &str| -> Dynamic {
            let value = data
                .borrow()
                .0
                .get(&actor)
                .and_then(|region| region.get(key))
                .cloned();
            value.map_or(Dynamic::UNIT, |v| json_to_dynamic(&v))
        });
        let actor = id.to_string();
        let data = self.data.clone();
        engine.register_fn("__data_remove", move |_: &mut Map, key: &str| -> bool {
            data.borrow_mut()
                .0
                .get_mut(&actor)
                .is_some_and(|region| region.remove(key).is_some())
        });
        let actor = id.to_string();
        let data = self.data.clone();
        engine.register_fn("__data_keys", move |_: &mut Map| -> Array {
            let guard = data.borrow();
            let mut keys = Array::new();
            if let Some(region) = guard.0.get(&actor) {
                for key in region.keys() {
                    keys.push(Dynamic::from(key.clone()));
                }
            }
            keys
        });
        let data = self.data.clone();
        let grants = manifest.data_grant.clone();
        let kernel_log = self.kernel_log.clone();
        engine.register_fn(
            "__data_get_in",
            move |_: &mut Map, region: &str, key: &str| -> Dynamic {
                if !grants.iter().any(|g| g == region) {
                    kernel_log.log(&format!("data.get_in denied: {region}"));
                    return Dynamic::UNIT;
                }
                let value = data
                    .borrow()
                    .0
                    .get(region)
                    .and_then(|r| r.get(key))
                    .cloned();
                value.map_or(Dynamic::UNIT, |v| json_to_dynamic(&v))
            },
        );
        let actor = id.to_string();
        let config = self.config.clone();
        engine.register_fn(
            "__config_get",
            move |_: &mut Map, key: &str, default: Dynamic| -> Dynamic {
                let value = config.borrow().get(&actor, key).cloned();
                value.map_or(default, |v| json_to_dynamic(&v))
            },
        );
        let actor = id.to_string();
        let config = self.config.clone();
        engine.register_fn(
            "__config_set",
            move |_: &mut Map, key: &str, value: Dynamic| -> bool {
                config
                    .borrow_mut()
                    .set(&actor, key, dynamic_to_json(&value));
                true
            },
        );
        let plugins = self.registry.clone();
        engine.register_fn("__host_info", move |_: &mut Map| -> Map {
            let mut out = Map::new();
            out.insert("version".into(), env!("CARGO_PKG_VERSION").into());
            let list: Array = plugin_snapshot(&plugins.borrow())
                .into_iter()
                .map(|info| {
                    let mut entry = Map::new();
                    entry.insert("id".into(), info.id.into());
                    entry.insert("name".into(), info.name.into());
                    entry.insert("version".into(), info.version.into());
                    entry.insert("state".into(), info.state.into());
                    Dynamic::from(entry)
                })
                .collect();
            out.insert("plugins".into(), Dynamic::from(list));
            out
        });
        engine.register_fn("__host_now", |_: &mut Map| -> i64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64)
        });
        let plugins = self.registry.clone();
        engine.register_fn("__host_manifest", move |_: &mut Map, id: &str| -> Dynamic {
            let manifest = plugins.borrow().get(id).map(|slot| slot.manifest.clone());
            manifest.map_or(Dynamic::UNIT, |m| json_to_dynamic(&manifest_json(&m)))
        });
        let cli = self.cli.clone();
        let grants: Vec<String> = manifest.cli_grant.clone();
        let g1 = grants.clone();
        engine.register_fn("__cli_get", move |_: &mut Map, key: &str| -> String {
            if !g1.iter().any(|g| g == key) {
                return String::new();
            }
            cli.borrow().options.get(key).cloned().unwrap_or_default()
        });
        let cli = self.cli.clone();
        let g2 = grants;
        engine.register_fn("__cli_flag", move |_: &mut Map, name: &str| -> bool {
            g2.iter().any(|g| g == name) && cli.borrow().options.contains_key(name)
        });
        let i18n = self.i18n.clone();
        engine.register_fn("__i18n_t", move |_: &mut Map, key: &str| -> String {
            i18n.borrow()
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.to_string())
        });
    }

    pub fn plugin_infos(&self) -> Vec<PluginInfo> {
        plugin_snapshot(&self.registry.borrow())
    }
    fn manifest_of(&self, id: &str) -> Option<Json> {
        self.registry
            .borrow()
            .get(id)
            .map(|r| manifest_json(&r.manifest))
            .or_else(|| self.pending.get(id).map(|p| manifest_json(&p.manifest())))
    }
    fn ui(&self) -> Rc<UiApi> {
        self.ui.borrow().clone()
    }
}

fn neutral_ui() -> Rc<UiApi> {
    Rc::new(UiApi {
        create: Rc::new(|_: &str, _: &str, _: &str, _: Json| false),
        patch: Rc::new(|_: &str, _: &str, _: Json| false),
        remove: Rc::new(|_: &str, _: &str| false),
        remove_owned: Rc::new(|_: &str| {}),
        set_hover: Rc::new(|_: &str, _: &str| false),
        set_focus: Rc::new(|_: &str, _: &str| false),
    })
}

/// Writes a `.d.rhai` definition file describing the script API surface so
/// editors with a Rhai language server can complete and hover the symbols.
/// Signatures mirror `register_script_api`; the closures are inert.
pub fn write_rhai_definitions(path: &std::path::Path) -> std::io::Result<()> {
    let mut engine = Engine::new();
    engine.register_fn(
        "__ui_create",
        |_: &mut Map, _: &str, _: &str, _: Map| -> bool { false },
    );
    engine.register_fn("__ui_patch", |_: &mut Map, _: &str, _: Map| -> bool {
        false
    });
    engine.register_fn("__ui_remove", |_: &mut Map, _: &str| -> bool { false });
    engine.register_fn("__ui_focus", |_: &mut Map, _: &str| -> bool { false });
    engine.register_fn("__events_publish", |_: &mut Map, _: &str, _: Map| {});
    engine.register_fn("__svc_send", |_: &mut Map, _: &str, _: &str, _: Dynamic| {});
    engine.register_fn("__data_set", |_: &mut Map, _: &str, _: Dynamic| -> bool {
        false
    });
    engine.register_fn("__data_get", |_: &mut Map, _: &str| -> Dynamic {
        Dynamic::UNIT
    });
    engine.register_fn("__data_remove", |_: &mut Map, _: &str| -> bool { false });
    engine.register_fn("__data_keys", |_: &mut Map| -> Array { Array::new() });
    engine.register_fn(
        "__data_get_in",
        |_: &mut Map, _: &str, _: &str| -> Dynamic { Dynamic::UNIT },
    );
    engine.register_fn(
        "__config_get",
        |_: &mut Map, _: &str, _: Dynamic| -> Dynamic { Dynamic::UNIT },
    );
    engine.register_fn("__config_set", |_: &mut Map, _: &str, _: Dynamic| -> bool {
        false
    });
    engine.register_fn("__host_info", |_: &mut Map| -> Map { Map::new() });
    engine.register_fn("__host_now", |_: &mut Map| -> i64 { 0 });
    engine.register_fn("__host_manifest", |_: &mut Map, _: &str| -> Dynamic {
        Dynamic::UNIT
    });
    engine.register_fn("__i18n_t", |_: &mut Map, key: &str| -> String {
        key.to_string()
    });
    engine.register_fn("__cli_get", |_: &mut Map, _: &str| -> String {
        String::new()
    });
    engine.register_fn("__cli_flag", |_: &mut Map, _: &str| -> bool { false });
    let mut scope = Scope::new();
    scope.push_constant(
        "ui",
        ns_map(&[
            ("create", "__ui_create"),
            ("patch", "__ui_patch"),
            ("remove", "__ui_remove"),
            ("focus", "__ui_focus"),
        ]),
    );
    scope.push_constant("events", ns_map(&[("publish", "__events_publish")]));
    scope.push_constant("svc", ns_map(&[("send", "__svc_send")]));
    scope.push_constant(
        "data",
        ns_map(&[
            ("set", "__data_set"),
            ("get", "__data_get"),
            ("remove", "__data_remove"),
            ("keys", "__data_keys"),
            ("get_in", "__data_get_in"),
        ]),
    );
    scope.push_constant(
        "config",
        ns_map(&[("get", "__config_get"), ("set", "__config_set")]),
    );
    scope.push_constant(
        "host",
        ns_map(&[
            ("info", "__host_info"),
            ("now", "__host_now"),
            ("manifest", "__host_manifest"),
        ]),
    );
    scope.push_constant("i18n", ns_map(&[("t", "__i18n_t")]));
    scope.push_constant(
        "cli",
        ns_map(&[("get", "__cli_get"), ("flag", "__cli_flag")]),
    );
    engine.definitions_with_scope(&scope).write_to_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn failed_initialization_can_be_reloaded() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "retry-init",
            "id = \"retry\"\nprovides = [\"retry-svc\"]\nsubscribe = [\"probe\"]\n",
            "fn init() { throw \"broken\"; } fn on_event(n,d) {}",
        );
        assert!(host.load_plugin(&dir, &Logger::new("test", sink)).is_err());
        assert!(!host.runtimes.contains_key("retry"));
        assert!(!host.services.contains_key("retry-svc"));
        assert!(!host.subscriptions.iter().any(|(id, _)| id == "retry"));
        fs::write(dir.join("main.rhai"), "fn init() {} fn on_event(n,d) {}").unwrap();
        host.reload("retry").unwrap();
        assert!(host.registry.borrow()["retry"].is_active());
        fs::remove_file(dir.join("main.rhai")).unwrap();
        assert!(host.reload("retry").is_err());
        fs::write(dir.join("main.rhai"), "fn init() {} fn on_event(n,d) {}").unwrap();
        host.reload("retry").unwrap();
        let missing = script_dir("retry-entry", "id = \"missing\"\n", "");
        fs::remove_file(missing.join("main.rhai")).unwrap();
        assert!(
            host.load_plugin(&missing, &Logger::new("test", LogStore::default()))
                .is_err()
        );
        fs::write(
            missing.join("main.rhai"),
            "fn init() {} fn on_event(n,d) {}",
        )
        .unwrap();
        host.reload("missing").unwrap();
        fs::write(dir.join("main.rhai"), "fn init() { throw \"again\"; }").unwrap();
        assert!(host.reload("retry").is_err());
        fs::write(dir.join("main.rhai"), "fn init() {} fn on_event(n,d) {}").unwrap();
        host.reload("retry").unwrap();
        assert!(host.registry.borrow()["retry"].is_active());
    }

    #[test]
    fn failed_initialization_rolls_back_commands_config_and_data() {
        let (mut host, sink, _) = test_host();
        let watcher = script_dir(
            "rollback-watcher",
            "id = \"watcher\"\nsubscribe = [\"probe\",\"config.updated\"]\nprovides = [\"watcher-svc\"]\n",
            "fn init() {} fn on_event(n,d) { data.set(n,true); if n == \"config.updated\" { data.set(d.section,true); } } fn receive(d) { data.set(\"svc\",true); }",
        );
        load_ok(&mut host, &sink, &watcher);
        host.drain();
        host.config.borrow_mut().set("prior", "keep", json!(42));
        let revision = host.config.borrow().revision();
        host.data
            .borrow_mut()
            .0
            .insert("broken".into(), HashMap::from([("old".into(), json!(7))]));
        host.publish("prior.event", json!({}));
        let dir = script_dir(
            "rollback-init",
            "id = \"broken\"\n",
            "fn init() { events.publish(\"probe\", #{}); svc.send(\"watcher-svc\", \"receive\", #{}); config.set(\"leaked\",1); data.set(\"old\",9); data.set(\"new\",true); throw \"fail\"; } fn on_event(n,d) {}",
        );
        assert!(host.load_plugin(&dir, &Logger::new("test", sink)).is_err());
        assert_eq!(host.config.borrow().revision(), revision);
        assert_eq!(host.config.borrow().get("broken", "leaked"), None);
        assert_eq!(
            host.data.borrow().0["broken"],
            HashMap::from([("old".into(), json!(7))])
        );
        assert!(
            host.queue
                .borrow()
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Publish { name, .. } if name == "prior.event"))
        );
        host.drain();
        let data = host.data.borrow();
        assert!(!data.0["watcher"].contains_key("probe"));
        assert!(!data.0["watcher"].contains_key("svc"));
        assert!(!data.0["watcher"].contains_key("broken"));
        assert_eq!(data.0["watcher"]["prior"], json!(true));
        assert_eq!(host.config.borrow().get("prior", "keep"), Some(&json!(42)));
    }

    #[test]
    fn failed_load_leaves_no_services_or_subscriptions() {
        let (mut host, sink, _) = test_host();
        let first = script_dir(
            "service-owner",
            "id = \"owner\"\nprovides = [\"taken\"]\n",
            "fn init() {} fn on_event(n,d) {}",
        );
        load_ok(&mut host, &sink, &first);
        let second = script_dir(
            "service-conflict",
            "id = \"conflict\"\nprovides = [\"unused\",\"taken\"]\nsubscribe = [\"probe\"]\n",
            "fn init() {} fn on_event(n,d) {}",
        );
        assert!(
            host.load_plugin(&second, &Logger::new("test", sink.clone()))
                .is_err()
        );
        assert!(!host.services.contains_key("unused"));
        assert!(!host.subscriptions.iter().any(|(id, _)| id == "conflict"));
        host.disable("owner").unwrap();
        host.reload("conflict").unwrap();
        assert_eq!(host.services["unused"], "conflict");
    }

    #[test]
    fn script_observes_its_metadata_during_callback() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "visible-meta",
            "id = \"visible\"\n",
            "fn init() {} fn on_event(n,d) { let found = false; for p in host.info().plugins { if p.id == \"visible\" { found = true; } } data.set(\"found\", found); return; }",
        );
        load_ok(&mut host, &sink, &dir);
        host.deliver("visible", "probe", json!({}));
        assert_eq!(host.data.borrow().0["visible"]["found"], json!(true));
    }

    #[test]
    fn sdk_guest_remains_active_after_event() {
        let (mut host, sink, _) = test_host();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/wasm-hello-rs");
        host.load_plugin(
            &dir,
            &Logger::new("init.logger.wasm-hello-rs", sink.clone()),
        )
        .unwrap();
        host.deliver("wasm-hello-rs", "plugin.loaded", json!({"id":"probe"}));
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "wasm-hello-rs")
            .unwrap();
        assert_eq!(info.state, "active", "{}", sink.borrow().join("\n"));
        assert!(sink.borrow().iter().any(|s| s.contains("plugin.loaded")));
        for _ in 0..10 {
            host.deliver("wasm-hello-rs", "probe.memory", json!({"n":1}));
        }
        let memory_size = |host: &PluginHost| {
            let Runtime::Wasm { store, instance } = &host.runtimes["wasm-hello-rs"] else {
                panic!("missing runtime");
            };
            instance
                .get_memory(&**store, "memory")
                .unwrap()
                .data_size(&**store)
        };
        let before = memory_size(&host);
        for _ in 0..1000 {
            host.deliver("wasm-hello-rs", "probe.memory", json!({"n":1}));
        }
        host.deliver("wasm-hello-rs", "quote\"\\\n", json!({"n":1}));
        assert_eq!(memory_size(&host), before);
        assert_eq!(
            host.data.borrow().0["wasm-hello-rs"]["echo"],
            json!({"n":1})
        );
        assert_eq!(
            host.plugin_infos()
                .iter()
                .find(|p| p.id == "wasm-hello-rs")
                .unwrap()
                .state,
            "active"
        );
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("terminism-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script_dir(tag: &str, manifest: &str, code: &str) -> PathBuf {
        let dir = tmpdir(tag);
        fs::write(dir.join("manifest.toml"), manifest).unwrap();
        fs::write(dir.join("main.rhai"), code).unwrap();
        dir
    }

    fn test_host() -> (PluginHost, LogStore, Rc<RefCell<PluginRegistry>>) {
        let sink = LogStore::default();
        let data = Rc::new(RefCell::new(DataStore(HashMap::new())));
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = tmpdir(&format!(
            "host-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
        .join("config.toml");
        let config = Rc::new(RefCell::new(ConfigStore::load(&path).0));
        let registry = Rc::new(RefCell::new(PluginRegistry::new()));
        (
            PluginHost::new(sink.clone(), data, config, registry.clone()),
            sink,
            registry,
        )
    }

    fn load_ok(host: &mut PluginHost, sink: &LogStore, dir: &Path) -> String {
        host.load_plugin(dir, &Logger::new("test", sink.clone()))
            .unwrap()
    }

    #[test]
    fn data_region_isolation() {
        let (mut host, sink, _) = test_host();
        let dir_a = script_dir(
            "region-a",
            "id = \"region-a\"\n",
            "fn init() { } fn on_event(n, d) { data.set(\"secret\", 42); }",
        );
        let dir_b = script_dir(
            "region-b",
            "id = \"region-b\"\ndata_grant = []\n",
            "fn init() { } fn on_event(n, d) { let v = data.get_in(\"region-a\", \"secret\"); data.set(\"out\", v == ()); }",
        );
        load_ok(&mut host, &sink, &dir_a);
        load_ok(&mut host, &sink, &dir_b);
        host.deliver("region-a", "go", json!({}));
        host.deliver("region-b", "go", json!({}));
        let data = host.data.borrow();
        assert_eq!(data.0["region-a"]["secret"], json!(42));
        assert_eq!(data.0["region-b"]["out"], json!(true));
        assert!(
            sink.borrow()
                .iter()
                .any(|line| line == "[kernel] data.get_in denied: region-a")
        );
        let _ = fs::remove_dir_all(dir_a);
        let _ = fs::remove_dir_all(dir_b);
    }

    #[test]
    fn config_roundtrip() {
        let dir = tmpdir("config-roundtrip");
        let path = dir.join("config.toml");
        let (mut cfg, err) = ConfigStore::load(&path);
        assert!(err.is_none());
        assert!(cfg.get("missing", "key").is_none());
        cfg.set("sec", "key", json!("v1"));
        assert!(!path.exists(), "set must only change memory");
        cfg.flush().unwrap();
        assert!(path.exists());
        let (cfg, err) = ConfigStore::load(&path);
        assert!(err.is_none());
        assert_eq!(cfg.get("sec", "key"), Some(&json!("v1")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_changes_notify_after_memory_is_updated() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "config-notify",
            "id = \"config-observer\"\nsubscribe = [\"config.updated\"]\n",
            "fn init() {} fn on_event(n, d) { data.set(d.key, d.value); return; }",
        );
        load_ok(&mut host, &sink, &dir);
        host.config.borrow_mut().set("app", "first", json!(1));
        host.config.borrow_mut().set("app", "second", json!(2));
        host.drain();
        assert_eq!(host.data.borrow().0["config-observer"]["first"], json!(1));
        assert_eq!(
            host.data.borrow().0["config-observer"].get("second"),
            Some(&json!(2)),
            "{}",
            sink.borrow().join("\n")
        );
    }

    #[test]
    fn config_corrupt() {
        let dir = tmpdir("config-corrupt");
        let path = dir.join("config.toml");
        fs::write(&path, "[broken").unwrap();
        let (cfg, err) = ConfigStore::load(&path);
        assert!(cfg.get("missing", "key").is_none());
        assert!(err.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    type UiCalls = Rc<RefCell<Vec<(String, String, String, Json)>>>;

    #[test]
    fn ui_closure_wiring() {
        let (mut host, sink, _) = test_host();
        let calls: UiCalls = Rc::new(RefCell::new(Vec::new()));
        let create_calls = calls.clone();
        host.set_ui(Rc::new(UiApi {
            create: Rc::new(move |actor: &str, path: &str, kind: &str, props: Json| {
                create_calls.borrow_mut().push((
                    actor.to_string(),
                    path.to_string(),
                    kind.to_string(),
                    props,
                ));
                true
            }),
            patch: Rc::new(|_: &str, _: &str, _: Json| false),
            remove: Rc::new(|_: &str, _: &str| false),
            remove_owned: Rc::new(|_: &str| {}),
            set_hover: Rc::new(|_: &str, _: &str| false),
            set_focus: Rc::new(|_: &str, _: &str| false),
        }));
        let dir = script_dir(
            "ui-wiring",
            "id = \"p-ui\"\n",
            "fn init() { } fn on_event(n, d) { ui.create(\"/btn\", \"Button\", #{text: \"hi\"}); ui.focus(\"/btn\"); data.set(\"ver\", host.info().version); }",
        );
        load_ok(&mut host, &sink, &dir);
        host.deliver("p-ui", "go", json!({}));
        let got = calls.borrow();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "p-ui");
        assert_eq!(got[0].1, "/btn");
        assert_eq!(got[0].2, "Button");
        assert_eq!(got[0].3, json!({"text": "hi"}));
        assert_eq!(host.data.borrow().0["p-ui"]["ver"], json!("0.0.1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_dir_reports_script_directory() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir("dir-api", "id = \"p-dir\"\n", "fn init() { }");
        load_ok(&mut host, &sink, &dir);
        {
            let handle = HostHandle { host: &mut host };
            assert_eq!(handle.plugin_dir("p-dir"), Some(dir.clone()));
            assert_eq!(handle.plugin_dir("missing"), None);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn native_publish_queues_and_drops_reserved() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "native-pub",
            "id = \"p-sub\"\nsubscribe = [\"cfg.go\", \"plugin.hijack\"]\n",
            "fn init() { } fn on_event(n, d) { data.set(n, d); }",
        );
        load_ok(&mut host, &sink, &dir);
        {
            let mut handle = HostHandle { host: &mut host };
            handle.publish("cfg.go", json!({"ok": true}));
            handle.publish("plugin.hijack", json!({}));
        }
        host.drain();
        let got = host.data.borrow().0["p-sub"]["cfg.go"].is_object();
        assert!(got, "cfg.go must reach the subscriber");
        let hijacked = host.data.borrow().0["p-sub"].contains_key("plugin.hijack");
        assert!(
            !hijacked,
            "reserved prefixes must be dropped for natives too"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn publish_bfs() {
        let (mut host, sink, _) = test_host();
        let dir_a = script_dir(
            "evt-a",
            "id = \"evt-a\"\nsubscribe = [\"from-b\"]\n",
            "fn init() { } fn on_event(n, d) { data.set(\"got\", 1); data.set(\"other\", host.info().plugins[0].id); }",
        );
        let dir_b = script_dir(
            "evt-b",
            "id = \"evt-b\"\nsubscribe = [\"from-a\"]\n",
            "fn init() { } fn on_event(n, d) { events.publish(\"from-b\", #{n: 1}); }",
        );
        load_ok(&mut host, &sink, &dir_a);
        load_ok(&mut host, &sink, &dir_b);
        host.deliver("evt-b", "from-a", json!({}));
        host.drain();
        let data = host.data.borrow();
        assert_eq!(data.0["evt-a"]["got"], json!(1));
        assert_eq!(data.0["evt-a"]["other"], json!("evt-a"));
        let _ = fs::remove_dir_all(dir_a);
        let _ = fs::remove_dir_all(dir_b);
    }

    #[test]
    fn svc_rules() {
        let (mut host, sink, _) = test_host();
        let dir_a = script_dir(
            "svc-a",
            "id = \"svc-a\"\nprovides = [\"math.add\"]\n",
            "fn add(a) { data.set(\"sum\", a + 3); } fn init() { } fn on_event(n, d) { svc.send(\"math.add\", \"add\", 0); }",
        );
        let dir_b = script_dir(
            "svc-b",
            "id = \"svc-b\"\nsubscribe = [\"kick-unknown\", \"kick-legal\"]\n",
            "fn init() { } fn on_event(n, d) { if n == \"kick-legal\" { svc.send(\"math.add\", \"add\", 2); } else { svc.send(\"no-svc\", \"f\", 0); } }",
        );
        load_ok(&mut host, &sink, &dir_a);
        load_ok(&mut host, &sink, &dir_b);
        host.deliver("svc-b", "kick-unknown", json!({}));
        host.drain();
        assert!(
            sink.borrow()
                .iter()
                .any(|line| line == "[kernel] svc unknown: no-svc")
        );
        host.deliver("svc-a", "go", json!({}));
        host.drain();
        assert!(
            sink.borrow()
                .iter()
                .any(|line| line == "[kernel] svc self-send denied")
        );
        assert!(
            host.data
                .borrow()
                .0
                .get("svc-a")
                .is_none_or(|r| !r.contains_key("sum"))
        );
        host.deliver("svc-b", "kick-legal", json!({}));
        host.drain();
        assert_eq!(host.data.borrow().0["svc-a"]["sum"], json!(5));
        let _ = fs::remove_dir_all(dir_a);
        let _ = fs::remove_dir_all(dir_b);
    }

    #[test]
    fn crash_disables() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "crash",
            "id = \"p-crash\"\n",
            "fn init() { } fn on_event(n, d) { throw \"boom\"; }",
        );
        load_ok(&mut host, &sink, &dir);
        host.deliver("p-crash", "go", json!({}));
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-crash")
            .unwrap();
        assert_eq!(info.state, "disabled");
        assert!(info.reason.contains("boom"));
        let crashed = host
            .queue
            .borrow()
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Publish { name, .. } if name == "plugin.crashed"));
        assert!(crashed);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_failure_publishes_plugin_failed() {
        let (mut host, sink, _) = test_host();
        let logger = Logger::new("test", sink.clone());
        // manifest parses, but main.rhai is missing: failure carries the real id
        let dir = tmpdir("fail-run");
        fs::write(dir.join("manifest.toml"), "id = \"p-fail\"\n").unwrap();
        assert!(host.load_plugin(&dir, &logger).is_err());
        // manifest is broken: the dir name stands in for the id
        let dir2 = tmpdir("fail-parse");
        fs::write(dir2.join("manifest.toml"), "id = 123\n").unwrap();
        fs::write(dir2.join("main.rhai"), "fn init() {}").unwrap();
        assert!(host.load_plugin(&dir2, &logger).is_err());
        let events: Vec<(String, String)> = host
            .queue
            .borrow()
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::Publish { name, payload } => Some((
                    name.clone(),
                    payload
                        .get("id")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )),
                _ => None,
            })
            .collect();
        assert!(
            events
                .iter()
                .any(|(n, id)| n == "plugin.failed" && id == "p-fail"),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|(n, id)| n == "plugin.failed" && id.contains("fail-parse")),
            "{events:?}"
        );
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(dir2);
    }

    #[test]
    fn uptime_counts_while_active_and_zero_when_disabled() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "uptime",
            "id = \"p-up\"\n",
            "fn init() { } fn on_event(n, d) { }",
        );
        load_ok(&mut host, &sink, &dir);
        let find = |host: &mut PluginHost| {
            host.plugin_infos()
                .into_iter()
                .find(|p| p.id == "p-up")
                .unwrap()
        };
        assert!(find(&mut host).uptime_secs <= 2);
        host.disable("p-up").unwrap();
        assert_eq!(find(&mut host).uptime_secs, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_over_disabled_slot_is_rejected() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "disabled-slot",
            "id = \"p-dis\"\n",
            "fn init() { } fn on_event(n, d) { }",
        );
        load_ok(&mut host, &sink, &dir);
        host.disable("p-dis").unwrap();
        // a disabled slot is never silently replaced by a re-scan
        let err = host
            .load_plugin(&dir, &Logger::new("test", sink.clone()))
            .unwrap_err();
        assert!(matches!(err, HostError::AlreadyStarted(_)));
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-dis")
            .unwrap();
        assert_eq!(info.state, "disabled");
        // reload() drops the slot first, so a restart still works
        host.reload("p-dis").unwrap();
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-dis")
            .unwrap();
        assert_eq!(info.state, "active");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disable_retains_metadata_and_releases_runtime() {
        let (mut host, sink, registry) = test_host();
        let dir = script_dir(
            "disable",
            "id = \"p-off\"\n",
            "fn init() { } fn on_event(n, d) { }",
        );
        load_ok(&mut host, &sink, &dir);
        assert!(registry.borrow().contains_key("p-off"));
        host.disable("p-off").unwrap();
        assert!(!registry.borrow()["p-off"].is_active());
        assert!(!host.runtimes.contains_key("p-off"));
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-off")
            .unwrap();
        assert_eq!(info.state, "disabled");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn native_i18n_reads_shared_catalog() {
        let (mut host, _sink, _) = test_host();
        host.set_i18n(Rc::new(RefCell::new(HashMap::from([(
            "hello".to_string(),
            "你好".to_string(),
        )]))));
        let store = HostHandle { host: &mut host }.i18n();
        assert_eq!(
            store.borrow().get("hello").map(String::as_str),
            Some("你好")
        );
    }

    #[test]
    fn wasm_guest_loads_and_receives_events() {
        let (mut host, sink, _) = test_host();
        let dir = tmpdir("wasm-hello");
        fs::write(
            dir.join("manifest.toml"),
            "id = \"w-hello\"\nkind = \"wasm\"\nsubscribe = [\"hello.ping\"]\n",
        )
        .unwrap();
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/wasm-hello/plugin.wat"),
            dir.join("plugin.wat"),
        )
        .unwrap();
        let logger = Logger::new("test", sink.clone());
        host.load_plugin(&dir, &logger).unwrap();
        assert!(sink.borrow().iter().any(|l| l.contains("wasm-hello init")));
        assert!(host.plugin_infos().iter().any(|p| p.id == "w-hello"));
        host.deliver("w-hello", "hello.ping", json!({"n": 1}));
        assert!(sink.borrow().iter().any(|l| l.contains("wasm-hello event")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wasm_bad_module_is_rejected() {
        let (mut host, sink, _) = test_host();
        let dir = tmpdir("wasm-bad");
        fs::write(
            dir.join("manifest.toml"),
            "id = \"w-bad\"\nkind = \"wasm\"\n",
        )
        .unwrap();
        fs::write(dir.join("plugin.wat"), "(module (bogus)").unwrap();
        let logger = Logger::new("test", sink.clone());
        assert!(host.load_plugin(&dir, &logger).is_err());
        // the failure is reported through the standard channel
        let failed = host
            .queue
            .borrow()
            .iter()
            .any(|c| matches!(c, Cmd::Publish { name, .. } if name == "plugin.failed"));
        assert!(failed);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rhai_definitions_cover_api_surface() {
        let dir = tmpdir("rhai-defs");
        let path = dir.join("terminism.d.rhai");
        write_rhai_definitions(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for symbol in ["__ui_create", "__cli_get", "__i18n_t", "ui", "config"] {
            assert!(
                text.contains(symbol),
                "missing {symbol} in:
{text}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn script_cli_reads_only_granted_options() {
        let (mut host, sink, _) = test_host();
        host.set_cli(CliTable {
            command: "servers".to_string(),
            options: BTreeMap::from([
                ("host".to_string(), "1.2.3.4".to_string()),
                ("token".to_string(), "secret".to_string()),
                ("debug".to_string(), String::new()),
            ]),
            ..CliTable::default()
        });
        let dir = script_dir(
            "cli-grant",
            "id = \"p-cli\"\ncli_grant = [\"host\", \"debug\"]\n",
            "fn init() { } fn on_event(n, d) { data.set(\"host\", cli.get(\"host\")); data.set(\"token\", cli.get(\"token\")); data.set(\"debug\", cli.flag(\"debug\")); data.set(\"quiet\", cli.flag(\"quiet\")); }",
        );
        load_ok(&mut host, &sink, &dir);
        host.deliver("p-cli", "go", json!({}));
        let data = host.data.borrow();
        assert_eq!(data.0["p-cli"]["host"], json!("1.2.3.4"));
        // not granted: empty even though the option exists
        assert_eq!(data.0["p-cli"]["token"], json!(""));
        assert_eq!(data.0["p-cli"]["debug"], json!(true));
        assert_eq!(data.0["p-cli"]["quiet"], json!(false));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v2_i18n_and_lineage() {
        let (mut host, sink, registry) = test_host();
        // engine closures capture the store Rc, so install before load
        host.set_i18n(Rc::new(RefCell::new(HashMap::from([(
            "greeting".to_string(),
            "hello".to_string(),
        )]))));
        let dir = script_dir(
            "i18n",
            "id = \"p-i18n\"\n",
            "fn init() { } fn on_event(n, d) { data.set(\"t\", i18n.t(\"greeting\")); data.set(\"miss\", i18n.t(\"nope\")); }",
        );
        let logger = Logger::new("init.logger", sink.clone());
        host.load_plugin(&dir, &logger).unwrap();
        let tag = {
            let handle = HostHandle { host: &mut host };
            handle.lineage_logger("p-i18n").unwrap().tag
        };
        assert_eq!(tag, "init.logger.p-i18n");
        assert!(!registry.borrow()["p-i18n"].system());
        host.deliver("p-i18n", "e", json!({}));
        {
            let data = host.data.borrow();
            assert_eq!(data.0["p-i18n"]["t"], json!("hello"));
            assert_eq!(data.0["p-i18n"]["miss"], json!("nope"));
        }
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-i18n")
            .unwrap();
        assert!(!info.system);
        let dir_mgr = script_dir(
            "i18n-mgr",
            "id = \"p-mgr\"\n",
            "fn init() { } fn on_event(n, d) { }",
        );
        host.load_plugin(&dir_mgr, &Logger::new("init.manager", sink.clone()))
            .unwrap();
        let mgr = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-mgr")
            .unwrap();
        assert!(mgr.system);
        assert!(registry.borrow()["p-mgr"].system());
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(dir_mgr);
    }

    #[test]
    fn sandbox_ops_limit_disables() {
        let (mut host, sink, _) = test_host();
        let dir = script_dir(
            "sandbox",
            "id = \"p-spin\"\n",
            "fn init() { } fn on_event(n, d) { loop { } }",
        );
        load_ok(&mut host, &sink, &dir);
        host.deliver("p-spin", "e", json!({}));
        let info = host
            .plugin_infos()
            .into_iter()
            .find(|p| p.id == "p-spin")
            .unwrap();
        assert_eq!(info.state, "disabled");
        assert!(!info.reason.is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
