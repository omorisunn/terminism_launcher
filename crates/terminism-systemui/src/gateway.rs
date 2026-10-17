use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use indextree::{Arena, NodeId};
use ratatui::layout::Rect;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use terminism_kernel::host::{ConfigStore, Delivery, Json, PluginRegistry, UiApi};
use terminism_kernel::logger::LogStore;

use crate::render;
use crate::types::{Direction, Kind, NodeData, Props, parse_kind, parse_props};

pub(crate) const ANIM_MS: u128 = 150;

#[derive(Clone)]
pub(crate) struct Anim {
    pub path: String,
    pub from: Rect,
    pub to: Rect,
    pub start: Instant,
}

pub(crate) fn lerp_rect(anim: &Anim, now: Instant) -> Rect {
    let t = now.duration_since(anim.start).as_millis() as f32 / ANIM_MS as f32;
    if t >= 1.0 {
        return anim.to;
    }
    let ease = 1.0 - (1.0 - t).powi(3);
    let mix = |a: u16, b: u16| (a as f32 + (b as f32 - a as f32) * ease).round() as u16;
    Rect {
        x: mix(anim.from.x, anim.to.x),
        y: mix(anim.from.y, anim.to.y),
        width: mix(anim.from.width, anim.to.width),
        height: mix(anim.from.height, anim.to.height),
    }
}

pub(crate) enum InsertAt {
    Before(String),
    End,
}

pub(crate) enum DragState {
    Idle,
    Maybe {
        start: (u16, u16),
        path: String,
        allow_drag: bool,
    },
    Slider {
        path: String,
    },
    Drag {
        parent: String,
        path: String,
        pending: InsertAt,
        cursor: (u16, u16),
        grab: (u16, u16),
        size: (u16, u16),
    },
}

pub struct Gateway {
    pub(crate) arena: Arena<NodeData>,
    pub(crate) root: NodeId,
    pub(crate) paths: HashMap<String, NodeId>,
    pub(crate) focus: Option<String>,
    // last shown page; keeps the view stable when its focused node is removed
    page: Option<String>,
    pub(crate) hover: Option<String>,
    pub(crate) inputs: HashMap<String, Input>,
    pub(crate) lock: bool,
    pub(crate) drag: DragState,
    // last mouse-down stamp, used to gate pane drags behind a double click
    last_down: Option<(Instant, (u16, u16))>,
    pub(crate) anim: Vec<Anim>,
    pub(crate) last_rects: HashMap<String, Rect>,
    // status bar tab strip hit zones: (page path, x0, x1, row), refilled each draw
    pub(crate) tab_hits: Vec<(String, u16, u16, u16)>,
    // last main-area size announced to the current page owner via ui.resize
    pub(crate) last_size: Option<((u16, u16), String)>,
    // deliveries produced by draw (ui.resize), drained by the main loop
    pub(crate) pending: Vec<Delivery>,
    // second stamp of the last clock_tick, for second-boundary detection
    pub(crate) clock_second: Option<u64>,
    // open select path and its highlighted option index
    pub(crate) select_open: Option<String>,
    pub(crate) select_pending: usize,
    // select option hit zones: (select path, idx, x0, x1, y), refilled each draw
    pub(crate) select_hits: Vec<(String, usize, u16, u16, u16)>,
    saved_order: HashMap<String, Vec<String>>,
    saved_flex: HashMap<String, f32>,
    pub(crate) log_store: LogStore,
    pub(crate) theme: crate::tmss::Theme,
    // shared translation catalog; native plugins fill it at boot
    pub(crate) i18n: terminism_kernel::host::I18nStore,
    config: Rc<RefCell<ConfigStore>>,
    registry: Rc<RefCell<PluginRegistry>>,
    dirty: bool,
    config_revision: u64,
    theme_revision: u64,
    log_revision: u64,
}

impl Gateway {
    pub fn new(
        log_store: LogStore,
        config: Rc<RefCell<ConfigStore>>,
        registry: Rc<RefCell<PluginRegistry>>,
        theme: crate::tmss::Theme,
        i18n: terminism_kernel::host::I18nStore,
    ) -> Rc<RefCell<Gateway>> {
        let mut arena = Arena::new();
        let root = arena.new_node(NodeData {
            path: "/".to_string(),
            owner: "system-ui".to_string(),
            kind: Kind::Page,
            props: Props::default(),
            direction: None,
        });
        let layout = config.borrow().get("system-ui", "layout").cloned();
        let lock = layout
            .as_ref()
            .and_then(|j| j.get("locked"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let saved_order: HashMap<String, Vec<String>> = layout
            .as_ref()
            .and_then(|j| j.get("order"))
            .and_then(Json::as_object)
            .map_or(HashMap::new(), |m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let list = v.as_array()?;
                        Some((
                            k.clone(),
                            list.iter()
                                .filter_map(Json::as_str)
                                .map(str::to_string)
                                .collect(),
                        ))
                    })
                    .collect()
            });
        let saved_flex: HashMap<String, f32> = layout
            .as_ref()
            .and_then(|j| j.get("flex"))
            .and_then(Json::as_object)
            .map_or(HashMap::new(), |m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let f = v.as_f64()? as f32;
                        (f > 0.0).then_some((k.clone(), f))
                    })
                    .collect()
            });
        let config_revision = config.borrow().revision();
        let theme_revision = theme.borrow().revision();
        let log_revision = log_store.borrow().revision();
        let gw = Gateway {
            arena,
            root,
            paths: HashMap::new(),
            focus: None,
            page: None,
            theme,
            hover: None,
            inputs: HashMap::new(),
            lock,
            drag: DragState::Idle,
            last_down: None,
            anim: Vec::new(),
            last_rects: HashMap::new(),
            tab_hits: Vec::new(),
            last_size: None,
            pending: Vec::new(),
            clock_second: None,
            select_open: None,
            select_pending: 0,
            select_hits: Vec::new(),
            saved_order,
            saved_flex,
            log_store,
            i18n,
            config,
            registry,
            dirty: true,
            config_revision,
            theme_revision,
            log_revision,
        };
        // every page belongs to a plugin now: /home and /log are the default
        // theme's, so a broken theme still leaves a usable fallback shell
        Rc::new(RefCell::new(gw))
    }

    // `self: &Rc<Self>` needs the unstable arbitrary_self_types feature, so the
    // receiver is a plain parameter; the signature keeps the spec's types.
    pub fn ui_api(gw: &Rc<RefCell<Gateway>>) -> Rc<UiApi> {
        let create_gw = gw.clone();
        let patch_gw = gw.clone();
        let remove_gw = gw.clone();
        let owned_gw = gw.clone();
        let hover_gw = gw.clone();
        let focus_gw = gw.clone();
        Rc::new(UiApi {
            create: Rc::new(move |actor: &str, path: &str, kind: &str, props: Json| {
                create_gw.borrow_mut().create(actor, path, kind, &props)
            }),
            patch: Rc::new(move |actor: &str, path: &str, props: Json| {
                patch_gw.borrow_mut().patch(actor, path, &props)
            }),
            remove: Rc::new(move |actor: &str, path: &str| {
                remove_gw.borrow_mut().remove(actor, path)
            }),
            remove_owned: Rc::new(move |id: &str| owned_gw.borrow_mut().remove_owned(id)),
            set_hover: Rc::new(move |actor: &str, path: &str| {
                hover_gw.borrow_mut().set_hover_by(actor, path)
            }),
            set_focus: Rc::new(move |actor: &str, path: &str| {
                focus_gw.borrow_mut().set_focus_by(actor, path)
            }),
        })
    }

    // programmatic focus for page navigation: any Page is reachable, other
    // nodes need ownership; state only, deliveries are dropped like set_hover
    fn set_focus_by(&mut self, actor: &str, path: &str) -> bool {
        let Some(segs) = parse_path(path) else {
            return false;
        };
        let Some(&id) = self.paths.get(path) else {
            return false;
        };
        if self.arena[id].get().kind != Kind::Page && !self.can_touch(actor, &segs) {
            return false;
        }
        let _ = self.set_focus(Some(path.to_string()));
        true
    }

    // programmatic hover: state and visuals only, the caller already knows the target
    fn set_hover_by(&mut self, actor: &str, path: &str) -> bool {
        let Some(segs) = parse_path(path) else {
            return false;
        };
        if !self.paths.contains_key(path) || !self.can_touch(actor, &segs) {
            return false;
        }
        let _ = self.set_hover(Some(path.to_string()));
        true
    }

    // main loop drains draw-produced deliveries (ui.resize) once per frame
    pub fn take_pending(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut self.pending)
    }

    fn visible_clock(&self) -> bool {
        self.current_page()
            .descendants(&self.arena)
            .any(|id| self.arena[id].get().kind == Kind::Clock)
    }

    fn invalidate(&mut self, id: NodeId) {
        let page = self.current_page();
        if self.arena[id].get().kind == Kind::Page || id.ancestors(&self.arena).any(|a| a == page) {
            self.dirty = true;
        }
    }

    fn sync_shared(&mut self) {
        let revision = self.theme.borrow().revision();
        if revision != self.theme_revision {
            self.theme_revision = revision;
            self.dirty = true;
        }
        let revision = self.log_store.borrow().revision();
        if revision != self.log_revision {
            self.log_revision = revision;
            let page = self.current_page();
            if page
                .descendants(&self.arena)
                .any(|id| self.arena[id].get().path == "/log/list")
            {
                self.dirty = true;
            }
        }
        let revision = self.config.borrow().revision();
        if revision == self.config_revision {
            return;
        }
        self.config_revision = revision;
        let layout = self.config.borrow().get("system-ui", "layout").cloned();
        let Some(layout) = layout else {
            return;
        };
        if let Some(locked) = layout.get("locked").and_then(Json::as_bool)
            && self.lock != locked
        {
            self.lock = locked;
            self.dirty = true;
            if let Some(&id) = self.paths.get("/home/lock") {
                self.arena[id].get_mut().props.text = Some(lock_text(self));
            }
        }
        if let Some(flex) = layout.get("flex").and_then(Json::as_object) {
            for (path, value) in flex {
                if let Some(value) = value.as_f64().map(|v| v as f32).filter(|v| *v > 0.0) {
                    self.saved_flex.insert(path.clone(), value);
                    if let Some(&id) = self.paths.get(path)
                        && self.arena[id].get().props.flex != Some(value)
                    {
                        self.arena[id].get_mut().props.flex = Some(value);
                        self.invalidate(id);
                    }
                }
            }
        }
        if let Some(order) = layout.get("order").and_then(Json::as_object) {
            for (path, value) in order {
                let Some(values) = value.as_array() else {
                    continue;
                };
                let values: Vec<_> = values
                    .iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect();
                if self.saved_order.get(path) == Some(&values) {
                    continue;
                }
                if let Some(&parent) = self.paths.get(path) {
                    let mut kids: Vec<_> = parent.children(&self.arena).collect();
                    kids.sort_by_key(|id| {
                        values
                            .iter()
                            .position(|p| p == &self.arena[*id].get().path)
                            .unwrap_or(usize::MAX)
                    });
                    for id in kids {
                        parent.append(id, &mut self.arena);
                    }
                    self.invalidate(parent);
                }
                self.saved_order.insert(path.clone(), values);
            }
        }
    }

    pub fn take_dirty(&mut self) -> bool {
        self.sync_shared();
        if self.clock_tick() || self.anim_active() {
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }

    pub fn next_frame_delay(&self) -> Option<Duration> {
        if self.dirty {
            return Some(Duration::ZERO);
        }
        if self.anim_active() {
            return Some(Duration::from_millis(30));
        }
        if self.visible_clock() {
            let elapsed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            return Some(
                Duration::from_secs(1) - Duration::from_nanos(elapsed.subsec_nanos() as u64),
            );
        }
        None
    }

    pub fn clock_tick(&mut self) -> bool {
        if !self.visible_clock() {
            return false;
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if self.clock_second == Some(secs) {
            return false;
        }
        self.clock_second = Some(secs);
        true
    }

    pub fn anim_active(&self) -> bool {
        !self.anim.is_empty()
    }

    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        self.sync_shared();
        self.dirty = false;
        render::draw(self, frame);
    }

    // --- input ---

    pub fn handle_event(&mut self, ev: &Event) -> Vec<Delivery> {
        match ev {
            Event::Key(ke) if ke.kind == KeyEventKind::Press => self.handle_key(ke),
            Event::Mouse(me) => self.handle_mouse(me),
            Event::Paste(text) => self.handle_paste(text),
            Event::Resize(..) => {
                self.dirty = true;
                self.last_rects.clear();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    // reserved keys route first; a focused TextInput only sees what remains
    fn handle_key(&mut self, ke: &KeyEvent) -> Vec<Delivery> {
        match (ke.code, ke.modifiers) {
            (KeyCode::Esc, _) => {
                self.dirty |= self.select_open.is_some() || !matches!(self.drag, DragState::Idle);
                if matches!(self.drag, DragState::Drag { .. }) {
                    self.drag = DragState::Idle;
                }
                self.select_open = None;
                Vec::new()
            }
            (KeyCode::Char('L'), KeyModifiers::NONE) => {
                self.toggle_lock();
                Vec::new()
            }
            (KeyCode::Left | KeyCode::Right, m) if m == KeyModifiers::CONTROL => {
                self.resize_split(Direction::Row, ke.code == KeyCode::Right)
            }
            (KeyCode::Up | KeyCode::Down, m) if m == KeyModifiers::CONTROL => {
                self.resize_split(Direction::Col, ke.code == KeyCode::Down)
            }
            (KeyCode::Tab, KeyModifiers::NONE) => self.page_step(true),
            (KeyCode::BackTab, _) => self.page_step(false),
            (KeyCode::Enter, KeyModifiers::NONE) => self.activate_focused(),
            (KeyCode::Left | KeyCode::Right, KeyModifiers::NONE)
                if self.focused_slider().is_some() =>
            {
                self.adjust_slider(if ke.code == KeyCode::Right {
                    0.05
                } else {
                    -0.05
                })
            }
            (KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down, KeyModifiers::NONE) => {
                if self
                    .focus
                    .as_ref()
                    .is_some_and(|f| self.inputs.contains_key(f))
                {
                    self.edit_focused(ke)
                } else {
                    self.arrow_key(ke.code)
                }
            }
            _ => self.edit_focused(ke),
        }
    }

    // plain arrows go to the hover owner (or focus owner when nothing hovers);
    // the plugin decides what the press means
    fn arrow_key(&mut self, code: KeyCode) -> Vec<Delivery> {
        // an open select consumes vertical arrows to move its highlight
        if let Some(open) = self.select_open.clone()
            && let Some(&id) = self.paths.get(&open)
        {
            let len = self.arena[id]
                .get()
                .props
                .titles
                .as_ref()
                .map_or(0, Vec::len);
            if len > 0 {
                match code {
                    KeyCode::Up => {
                        self.select_pending = (self.select_pending + len - 1) % len;
                        self.dirty = true;
                        return Vec::new();
                    }
                    KeyCode::Down => {
                        self.select_pending = (self.select_pending + 1) % len;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
            }
        }
        let name = match code {
            KeyCode::Left => "left",
            KeyCode::Right => "right",
            KeyCode::Up => "up",
            _ => "down",
        };
        let Some(path) = self.hover.clone().or_else(|| self.focus.clone()) else {
            return Vec::new();
        };
        let Some(&id) = self.paths.get(&path) else {
            return Vec::new();
        };
        vec![Delivery {
            target: self.arena[id].get().owner.clone(),
            event: "ui.key".to_string(),
            payload: json!({"path": path, "key": name}),
        }]
    }

    fn focused_slider(&self) -> Option<String> {
        self.focus.clone().filter(|path| {
            self.paths
                .get(path)
                .is_some_and(|&id| self.arena[id].get().kind == Kind::Slider)
        })
    }

    fn adjust_slider(&mut self, delta: f32) -> Vec<Delivery> {
        let Some(path) = self.focused_slider() else {
            return Vec::new();
        };
        let Some(&id) = self.paths.get(&path) else {
            return Vec::new();
        };
        let node = self.arena[id].get_mut();
        let value = (node.props.value.unwrap_or(0.0) + delta).clamp(0.0, 1.0);
        node.props.value = Some(value);
        let owner = node.owner.clone();
        self.dirty = true;
        vec![Delivery {
            target: owner,
            event: "ui.input".to_string(),
            payload: json!({"path": path, "value": value}),
        }]
    }

    fn slider_fraction(&self, path: &str, column: u16) -> f32 {
        let Some(&id) = self.paths.get(path) else {
            return 0.0;
        };
        let Some(rect) = self.last_rects.get(path).copied() else {
            return 0.0;
        };
        let node = self.arena[id].get();
        let text = node.props.text.clone().unwrap_or_default();
        let (label, value) = match text.split_once('|') {
            Some((label, value)) => (label.to_string(), value.to_string()),
            None => (text, String::new()),
        };
        let label_width = label.chars().count();
        let value_width = value.chars().count();
        let label_space = usize::from(label_width > 0);
        let value_space = usize::from(value_width > 0);
        let bar_width = (rect.width as usize)
            .saturating_sub(label_width + value_width + label_space + value_space)
            .max(3);
        let bar_start = rect.x as usize + label_width + label_space;
        let offset = (column as usize)
            .saturating_sub(bar_start)
            .min(bar_width - 1);
        offset as f32 / (bar_width - 1) as f32
    }

    fn set_slider_from_column(&mut self, path: &str, column: u16) -> Vec<Delivery> {
        let Some(&id) = self.paths.get(path) else {
            return Vec::new();
        };
        let value = self.slider_fraction(path, column);
        let node = self.arena[id].get_mut();
        node.props.value = Some(value);
        let owner = node.owner.clone();
        self.dirty = true;
        vec![Delivery {
            target: owner,
            event: "ui.input".to_string(),
            payload: json!({"path": path, "value": value}),
        }]
    }

    fn activate_focused(&mut self) -> Vec<Delivery> {
        // Enter lands on the hovered node first, like a click without the mouse
        if let Some(hovered) = self.hover.clone()
            && self.focus.as_deref() != Some(hovered.as_str())
            && self.focus_target(&hovered).is_some()
        {
            return self.click(&hovered);
        }
        let Some(path) = self.focus.clone() else {
            return Vec::new();
        };
        let Some(&id) = self.paths.get(&path) else {
            return Vec::new();
        };
        let node = self.arena[id].get();
        match node.kind {
            Kind::Button => {
                let owner = node.owner.clone();
                vec![Delivery {
                    target: owner,
                    event: "ui.activate".to_string(),
                    payload: json!({"path": path}),
                }]
            }
            Kind::Toggle => {
                let owner = node.owner.clone();
                let flipped = !node.props.checked.unwrap_or(false);
                vec![Delivery {
                    target: owner,
                    event: "ui.activate".to_string(),
                    payload: json!({"path": path, "value": flipped}),
                }]
            }
            Kind::Tabs => self.cycle_tabs(id, path),
            Kind::Select => {
                if self.select_open.as_deref() == Some(path.as_str()) {
                    self.commit_select(id, &path)
                } else {
                    self.open_select(id, &path);
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    // opens a select and parks the highlight on the current option
    fn open_select(&mut self, id: NodeId, path: &str) {
        self.dirty = true;
        let sel = self.arena[id].get().props.selected.unwrap_or(0).max(0) as usize;
        self.select_pending = sel;
        self.select_open = Some(path.to_string());
    }

    // commits the highlighted option: activate + close + move the visual
    fn commit_select(&mut self, id: NodeId, path: &str) -> Vec<Delivery> {
        self.dirty = true;
        let idx = self.select_pending;
        let owner = self.arena[id].get().owner.clone();
        self.arena[id].get_mut().props.selected = Some(idx as i64);
        self.select_open = None;
        vec![Delivery {
            target: owner,
            event: "ui.activate".to_string(),
            payload: json!({"path": path, "selected": idx}),
        }]
    }

    fn cycle_tabs(&mut self, id: NodeId, path: String) -> Vec<Delivery> {
        self.dirty = true;
        let len = self.arena[id]
            .get()
            .props
            .titles
            .as_ref()
            .map_or(0, |t| t.len()) as i64;
        if len == 0 {
            return Vec::new();
        }
        let node = self.arena[id].get_mut();
        let next = (node.props.selected.unwrap_or(0) + 1) % len;
        node.props.selected = Some(next);
        let owner = node.owner.clone();
        vec![Delivery {
            target: owner,
            event: "ui.activate".to_string(),
            payload: json!({"path": path, "selected": next}),
        }]
    }

    fn edit_focused(&mut self, ke: &KeyEvent) -> Vec<Delivery> {
        let Some(path) = self.focus.clone() else {
            return Vec::new();
        };
        if !self.inputs.contains_key(path.as_str()) {
            return Vec::new();
        }
        let changed = self
            .inputs
            .get_mut(&path)
            .expect("checked above")
            .handle_event(&Event::Key(*ke))
            .is_some();
        if !changed {
            return Vec::new();
        }
        self.sync_input(&path)
    }

    fn sync_input(&mut self, path: &str) -> Vec<Delivery> {
        self.dirty = true;
        let (text, cursor) = {
            let input = &self.inputs[path];
            (input.value().to_string(), input.cursor())
        };
        let Some(&id) = self.paths.get(path) else {
            return Vec::new();
        };
        self.arena[id].get_mut().props.text = Some(text.clone());
        let owner = self.arena[id].get().owner.clone();
        vec![Delivery {
            target: owner,
            event: "ui.input".to_string(),
            payload: json!({"path": path, "text": text, "cursor": cursor}),
        }]
    }

    fn handle_paste(&mut self, text: &str) -> Vec<Delivery> {
        let Some(path) = self.focus.clone() else {
            return Vec::new();
        };
        if !self.inputs.contains_key(path.as_str()) {
            return Vec::new();
        }
        let input = self.inputs.get_mut(&path).expect("checked above");
        // tui-input has no paste request, so insert the text at the cursor
        let value = input.value().to_string();
        let cursor = input.cursor();
        let head: String = value.chars().take(cursor).collect();
        let tail: String = value.chars().skip(cursor).collect();
        let next =
            Input::new(head + text + tail.as_str()).with_cursor(cursor + text.chars().count());
        *input = next;
        self.sync_input(&path)
    }

    fn handle_mouse(&mut self, me: &MouseEvent) -> Vec<Delivery> {
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // the status bar tab strip switches pages before drag tracking starts
                if let Some((path, _, _, _)) = self
                    .tab_hits
                    .iter()
                    .find(|(_, x0, x1, row)| me.column >= *x0 && me.column < *x1 && me.row == *row)
                    .cloned()
                {
                    return self.set_focus(Some(path));
                }
                // option rows of an open select resolve to virtual paths
                if let Some((path, idx, _, _, _)) = self
                    .select_hits
                    .iter()
                    .find(|(_, _, x0, x1, row)| {
                        me.column >= *x0 && me.column < *x1 && me.row == *row
                    })
                    .cloned()
                {
                    self.drag = DragState::Maybe {
                        start: (me.column, me.row),
                        path: format!("{path}#{idx}"),
                        allow_drag: false,
                    };
                    return Vec::new();
                }
                if let Some(path) = self.hit_test(me.column, me.row) {
                    if let Some(&id) = self.paths.get(&path)
                        && self.arena[id].get().kind == Kind::Slider
                    {
                        let now = Instant::now();
                        let double_click = self.last_down.is_some_and(|(stamp, position)| {
                            position == (me.column, me.row)
                                && now.duration_since(stamp) < Duration::from_millis(400)
                        });
                        self.last_down = Some((now, (me.column, me.row)));
                        let mut out = self.set_focus(Some(path.clone()));
                        if double_click {
                            let owner = self.arena[id].get().owner.clone();
                            out.push(Delivery {
                                target: owner,
                                event: "ui.edit".to_string(),
                                payload: json!({"path": path}),
                            });
                            self.drag = DragState::Idle;
                            return out;
                        }
                        self.drag = DragState::Slider { path: path.clone() };
                        out.extend(self.set_slider_from_column(&path, me.column));
                        return out;
                    }
                    let now = Instant::now();
                    let double_click = self.last_down.is_some_and(|(stamp, position)| {
                        position == (me.column, me.row)
                            && now.duration_since(stamp) < Duration::from_millis(400)
                    });
                    self.last_down = Some((now, (me.column, me.row)));
                    // pane drags only start from a double click held on a pane edge
                    let on_border = self.last_rects.get(&path).is_some_and(|rect| {
                        me.column <= rect.x + 1
                            || me.column + 2 >= rect.x + rect.width
                            || me.row <= rect.y + 1
                            || me.row + 2 >= rect.y + rect.height
                    });
                    self.drag = DragState::Maybe {
                        start: (me.column, me.row),
                        path,
                        allow_drag: double_click && on_border,
                    };
                }
                Vec::new()
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                match &self.drag {
                    DragState::Maybe {
                        start,
                        path,
                        allow_drag,
                    } => {
                        let (sx, sy) = *start;
                        let allow_drag = *allow_drag;
                        if allow_drag && (me.column.abs_diff(sx) > 1 || me.row.abs_diff(sy) > 1) {
                            let hit = path.clone();
                            self.begin_drag(&hit, (sx, sy), (me.column, me.row));
                        }
                    }
                    DragState::Slider { path } => {
                        let path = path.clone();
                        return self.set_slider_from_column(&path, me.column);
                    }
                    DragState::Drag { .. } => self.update_drag(me.column, me.row),
                    DragState::Idle => {}
                }
                Vec::new()
            }
            MouseEventKind::Up(MouseButton::Left) => self.end_drag(),
            // the wheel is a touchpad accident magnet: page switching is Tab-only
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => Vec::new(),
            MouseEventKind::Moved => {
                // hover tracking pauses while a drag is in progress
                if !matches!(self.drag, DragState::Idle) {
                    return Vec::new();
                }
                match self.hit_test(me.column, me.row) {
                    Some(path) => self.hover_at(&path),
                    None => self.set_hover(None),
                }
            }
            _ => Vec::new(),
        }
    }

    fn end_drag(&mut self) -> Vec<Delivery> {
        if matches!(self.drag, DragState::Drag { .. }) {
            self.dirty = true;
        }
        match std::mem::replace(&mut self.drag, DragState::Idle) {
            DragState::Drag {
                parent,
                path,
                pending,
                ..
            } => {
                self.reorder(&parent, &path, pending);
                Vec::new()
            }
            DragState::Maybe { path, .. } => self.click(&path),
            DragState::Slider { .. } => {
                self.dirty = true;
                Vec::new()
            }
            DragState::Idle => Vec::new(),
        }
    }

    fn hit_test(&self, x: u16, y: u16) -> Option<String> {
        let mut best: Option<(u32, usize, &String)> = None;
        for (path, r) in &self.last_rects {
            if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
                let area = r.width as u32 * r.height as u32;
                let depth = path.matches('/').count();
                let better = best.as_ref().is_none_or(|(best_area, best_depth, _)| {
                    area < *best_area || (area == *best_area && depth > *best_depth)
                });
                if better {
                    best = Some((area, depth, path));
                }
            }
        }
        best.map(|(_, _, p)| p.clone())
    }

    // start is the original press point (grab offset anchor), cur is the pointer now
    fn begin_drag(&mut self, hit: &str, start: (u16, u16), cur: (u16, u16)) {
        self.dirty = true;
        let Some((child, split)) = self.split_child(hit) else {
            return;
        };
        let parent = self.arena[split].get().path.clone();
        let path = self.arena[child].get().path.clone();
        // a tmss locked rule pins the pane: no pickup, the press falls back to a click
        let node = self.arena[child].get();
        let ts = crate::tmss::resolve(
            self.theme.borrow().rules(),
            &path,
            node.kind,
            self.hover.as_deref() == Some(path.as_str()),
            self.focus.as_deref() == Some(path.as_str()),
        );
        if ts.locked == Some(true) {
            return;
        }
        // the pane floats at the size it had when picked up
        let Some(rect) = self.last_rects.get(&path).copied() else {
            return;
        };
        let pending = self.compute_pending(split, child, cur.0, cur.1);
        self.drag = DragState::Drag {
            parent,
            path,
            pending,
            cursor: cur,
            grab: (
                start.0.saturating_sub(rect.x),
                start.1.saturating_sub(rect.y),
            ),
            size: (rect.width, rect.height),
        };
    }

    fn update_drag(&mut self, x: u16, y: u16) {
        self.dirty = true;
        let (parent, path) = match &self.drag {
            DragState::Drag { parent, path, .. } => (parent.clone(), path.clone()),
            _ => return,
        };
        let (Some(&split), Some(&child)) = (self.paths.get(&parent), self.paths.get(&path)) else {
            return;
        };
        let pending = self.compute_pending(split, child, x, y);
        if let DragState::Drag {
            pending: p, cursor, ..
        } = &mut self.drag
        {
            *p = pending;
            *cursor = (x, y);
        }
    }

    fn split_child(&self, path: &str) -> Option<(NodeId, NodeId)> {
        let mut node = *self.paths.get(path)?;
        while let Some(parent) = node.parent(&self.arena) {
            if self.arena[parent].get().kind == Kind::Split {
                return Some((node, parent));
            }
            node = parent;
        }
        None
    }

    fn compute_pending(&self, split: NodeId, drag: NodeId, x: u16, y: u16) -> InsertAt {
        let row = self.arena[split].get().direction != Some(Direction::Col);
        let mut sibs: Vec<(u32, String, Rect)> = split
            .children(&self.arena)
            .filter(|&c| c != drag)
            .filter_map(|c| {
                let path = &self.arena[c].get().path;
                self.last_rects.get(path).map(|r| {
                    let coord = if row { u32::from(r.x) } else { u32::from(r.y) };
                    (coord, path.clone(), *r)
                })
            })
            .collect();
        if sibs.is_empty() {
            return InsertAt::End;
        }
        sibs.sort_by_key(|(coord, _, _)| *coord);
        let pointer = if row { u32::from(x) } else { u32::from(y) };
        let mut best: Option<(u32, InsertAt)> = None;
        for (coord, path, _) in &sibs {
            let d = pointer.abs_diff(*coord);
            if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, InsertAt::Before(path.clone())));
            }
        }
        let (end_coord, _, end_rect) = sibs.last().expect("siblings checked non-empty");
        let end = end_coord
            + if row {
                u32::from(end_rect.width)
            } else {
                u32::from(end_rect.height)
            };
        let de = pointer.abs_diff(end);
        if best.as_ref().is_none_or(|(bd, _)| de < *bd) {
            best = Some((de, InsertAt::End));
        }
        best.expect("best always set above").1
    }

    fn reorder(&mut self, parent_path: &str, drag_path: &str, pending: InsertAt) {
        let (Some(&parent), Some(&drag)) = (self.paths.get(parent_path), self.paths.get(drag_path))
        else {
            return;
        };
        drag.detach(&mut self.arena);
        match pending {
            InsertAt::Before(sib) => match self.paths.get(&sib) {
                Some(&sib_id) => sib_id.insert_before(drag, &mut self.arena),
                None => parent.append(drag, &mut self.arena),
            },
            InsertAt::End => parent.append(drag, &mut self.arena),
        }
        let order: Vec<String> = parent
            .children(&self.arena)
            .map(|c| self.arena[c].get().path.clone())
            .collect();
        self.saved_order.insert(parent_path.to_string(), order);
        self.write_layout();
    }

    // the node itself if focusable, else the nearest focusable ancestor
    fn focus_target(&self, path: &str) -> Option<String> {
        let &id = self.paths.get(path)?;
        if focusable(self.arena[id].get().kind) {
            return Some(path.to_string());
        }
        id.ancestors(&self.arena)
            .find(|&a| focusable(self.arena[a].get().kind))
            .map(|a| self.arena[a].get().path.clone())
    }

    fn hover_at(&mut self, path: &str) -> Vec<Delivery> {
        self.set_hover(self.focus_target(path))
    }

    fn click(&mut self, path: &str) -> Vec<Delivery> {
        // an open select swallows the click: an option row picks, anything
        // else just closes it
        if let Some(open) = self.select_open.clone() {
            self.select_open = None;
            if path.starts_with(&format!("{open}#")) {
                let idx = path
                    .trim_start_matches(&format!("{open}#"))
                    .parse::<usize>()
                    .unwrap_or(self.select_pending);
                self.select_pending = idx;
                if let Some(&id) = self.paths.get(&open) {
                    return self.commit_select(id, &open);
                }
            }
            return Vec::new();
        }
        let Some(&id) = self.paths.get(path) else {
            return Vec::new();
        };
        let Some(target) = self.focus_target(path) else {
            return Vec::new();
        };
        let mut out = self.set_focus(Some(target));
        // Items activate too: clicking an already-focused entry must still
        // mean something (tree toggles); focus events alone only fire on change
        let kind = self.arena[id].get().kind;
        if matches!(kind, Kind::Button | Kind::Item) {
            let owner = self.arena[id].get().owner.clone();
            out.push(Delivery {
                target: owner,
                event: "ui.activate".to_string(),
                payload: json!({"path": path}),
            });
        } else if kind == Kind::Toggle {
            let owner = self.arena[id].get().owner.clone();
            let flipped = !self.arena[id].get().props.checked.unwrap_or(false);
            self.arena[id].get_mut().props.checked = Some(flipped);
            self.dirty = true;
            out.push(Delivery {
                target: owner,
                event: "ui.activate".to_string(),
                payload: json!({"path": path, "value": flipped}),
            });
        } else if kind == Kind::Select {
            self.open_select(id, path);
        }
        out
    }

    // --- lock and split resize ---

    fn toggle_lock(&mut self) {
        self.dirty = true;
        self.lock = !self.lock;
        if let Some(&id) = self.paths.get("/home/lock") {
            let text = lock_text(self);
            self.arena[id].get_mut().props.text = Some(text);
        }
        self.write_layout();
    }

    fn resize_split(&mut self, dir: Direction, grow: bool) -> Vec<Delivery> {
        if self.lock {
            return Vec::new();
        }
        let Some(focus) = self.focus.clone() else {
            return Vec::new();
        };
        let Some(mut child) = self.paths.get(&focus).copied() else {
            return Vec::new();
        };
        loop {
            let Some(parent) = child.parent(&self.arena) else {
                return Vec::new();
            };
            if self.arena[parent].get().kind == Kind::Split {
                if self.arena[parent].get().direction == Some(dir) {
                    let flex = self.arena[child].get().props.flex.unwrap_or(1.0);
                    let next = (flex + if grow { 0.1 } else { -0.1 }).max(0.1);
                    self.arena[child].get_mut().props.flex = Some(next);
                    let child_path = self.arena[child].get().path.clone();
                    self.saved_flex.insert(child_path, next);
                    self.write_layout();
                }
                return Vec::new();
            }
            child = parent;
        }
    }

    // --- ui ops (behind UiApi closures) ---

    fn create(&mut self, actor: &str, path: &str, kind: &str, json: &Json) -> bool {
        let Some(segs) = parse_path(path) else {
            return false;
        };
        let Some(kind) = parse_kind(kind) else {
            return false;
        };
        if self.paths.contains_key(path) || !self.can_create(actor, &segs, kind) {
            return false;
        }
        let Some(parent) = self.parent_of(&segs) else {
            return false;
        };
        if kind == Kind::Row && self.arena[parent].get().kind != Kind::Table {
            return false;
        }
        if kind == Kind::Popup && self.arena[parent].get().kind != Kind::Page {
            return false;
        }
        let Some((props, direction)) = parse_props(kind, json, true) else {
            return false;
        };
        if kind == Kind::Split && direction.is_none() {
            return false;
        }
        if kind == Kind::Popup {
            self.remove_popups();
        }
        let mut props = props;
        if let Some(f) = self.saved_flex.get(path) {
            props.flex = Some(*f);
        }
        let id = self.arena.new_node(NodeData {
            path: path.to_string(),
            owner: actor.to_string(),
            kind,
            props,
            direction,
        });
        self.insert_saved_order(parent, id, path);
        self.paths.insert(path.to_string(), id);
        if kind == Kind::TextInput {
            let text = self.arena[id].get().props.text.clone().unwrap_or_default();
            self.inputs.insert(path.to_string(), Input::new(text));
        }
        self.invalidate(id);
        true
    }

    fn patch(&mut self, actor: &str, path: &str, json: &Json) -> bool {
        let Some(segs) = parse_path(path) else {
            return false;
        };
        if !self.can_touch(actor, &segs) {
            return false;
        }
        let Some(&id) = self.paths.get(path) else {
            return false;
        };
        let kind = self.arena[id].get().kind;
        let Some(props) = parse_props(kind, json, false).map(|(p, _)| p) else {
            return false;
        };
        let node = self.arena[id].get_mut();
        let text_changed = props
            .text
            .as_ref()
            .is_some_and(|text| node.props.text.as_ref() != Some(text));
        let changed = node.props.apply(props);
        if text_changed && kind == Kind::TextInput {
            self.inputs.insert(
                path.to_string(),
                Input::new(node.props.text.clone().unwrap_or_default()),
            );
        }
        if changed {
            self.invalidate(id);
        }
        true
    }

    fn remove(&mut self, actor: &str, path: &str) -> bool {
        let Some(segs) = parse_path(path) else {
            return false;
        };
        if !self.can_touch(actor, &segs) {
            return false;
        }
        let Some(&id) = self.paths.get(path) else {
            return false;
        };
        self.remove_subtree(id);
        true
    }

    fn remove_owned(&mut self, owner: &str) {
        let owned: Vec<NodeId> = self
            .root
            .descendants(&self.arena)
            .filter(|&n| self.arena[n].get().owner == owner)
            .filter(|&n| {
                !n.ancestors(&self.arena)
                    .skip(1)
                    .any(|a| self.arena[a].get().owner == owner)
            })
            .collect();
        for id in owned {
            self.remove_subtree(id);
        }
    }

    fn remove_popups(&mut self) {
        let popups: Vec<NodeId> = self
            .root
            .descendants(&self.arena)
            .filter(|&n| self.arena[n].get().kind == Kind::Popup)
            .collect();
        for id in popups {
            self.remove_subtree(id);
        }
    }

    fn remove_subtree(&mut self, id: NodeId) {
        self.invalidate(id);
        let doomed: Vec<String> = id
            .descendants(&self.arena)
            .map(|d| self.arena[d].get().path.clone())
            .collect();
        if self.page.as_ref().is_some_and(|p| doomed.contains(p)) {
            self.page = None;
        }
        for path in &doomed {
            self.paths.remove(path);
            self.inputs.remove(path);
            self.last_rects.remove(path);
            if self.focus.as_deref() == Some(path.as_str()) {
                self.focus = None;
            }
            if self.hover.as_deref() == Some(path.as_str()) {
                self.hover = None;
            }
        }
        self.anim.retain(|a| !doomed.contains(&a.path));
        self.tab_hits.retain(|(path, ..)| !doomed.contains(path));
        self.select_hits.retain(|(path, ..)| !doomed.contains(path));
        if self
            .select_open
            .as_ref()
            .is_some_and(|p| doomed.contains(p))
        {
            self.select_open = None;
        }
        let cancel_drag = match &self.drag {
            DragState::Idle => false,
            DragState::Maybe { path, .. } => doomed.contains(path),
            // a slider drag keeps its path across owner rebuilds (the new node is re-resolved)
            DragState::Slider { .. } => false,
            DragState::Drag { path, parent, .. } => {
                doomed.contains(path) || doomed.contains(parent)
            }
        };
        if cancel_drag {
            self.drag = DragState::Idle;
        }
        self.pending.retain(|d| {
            !d.payload
                .get("page")
                .and_then(Json::as_str)
                .is_some_and(|p| doomed.iter().any(|s| s == p))
        });
        self.last_size = None;
        id.remove_subtree(&mut self.arena);
    }

    fn insert_saved_order(&mut self, parent: NodeId, new_id: NodeId, path: &str) {
        let parent_path = self.arena[parent].get().path.clone();
        let Some(order) = self.saved_order.get(&parent_path) else {
            parent.append(new_id, &mut self.arena);
            return;
        };
        let new_idx = order.iter().position(|p| p == path);
        let mut before = None;
        for child in parent.children(&self.arena) {
            let (Some(n), Some(c)) = (
                new_idx,
                order
                    .iter()
                    .position(|p| p == &self.arena[child].get().path),
            ) else {
                continue;
            };
            if c > n {
                before = Some(child);
                break;
            }
        }
        match before {
            Some(b) => b.insert_before(new_id, &mut self.arena),
            None => parent.append(new_id, &mut self.arena),
        }
    }

    // --- ACL ---

    fn grants(&self, actor: &str) -> Vec<String> {
        self.registry
            .borrow()
            .get(actor)
            .map(|r| r.manifest.ui_grant.clone())
            .unwrap_or_default()
    }

    fn parent_of(&self, segs: &[&str]) -> Option<NodeId> {
        match segs.len() {
            1 => Some(self.root),
            _ => self
                .paths
                .get(&path_string(&segs[..segs.len() - 1]))
                .copied(),
        }
    }

    fn can_touch(&self, actor: &str, segs: &[&str]) -> bool {
        let Some(&id) = self.paths.get(&path_string(segs)) else {
            return false;
        };
        if self.arena[id].get().owner == actor {
            return true;
        }
        self.grants(actor).iter().any(|g| grant_covers(g, segs))
    }

    fn can_create(&self, actor: &str, segs: &[&str], kind: Kind) -> bool {
        if segs.len() == 1 && kind == Kind::Page {
            return true;
        }
        let parent = &segs[..segs.len() - 1];
        let Some(pid) = self.parent_of(segs) else {
            return false;
        };
        if self.arena[pid].get().owner == actor {
            return true;
        }
        self.grants(actor).iter().any(|g| grant_covers(g, parent))
    }

    // --- focus ---

    fn set_focus(&mut self, new: Option<String>) -> Vec<Delivery> {
        if self.focus == new {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(old) = self
            .focus
            .clone()
            .filter(|old| self.paths.contains_key(old))
        {
            let node = self.arena[self.paths[&old]].get();
            out.push(Delivery {
                target: node.owner.clone(),
                event: "ui.blur".to_string(),
                payload: json!({"path": old}),
            });
        }
        if let Some(path) = &new
            && let Some(&id) = self.paths.get(path)
        {
            let node = self.arena[id].get();
            out.push(Delivery {
                target: node.owner.clone(),
                event: "ui.focus".to_string(),
                payload: json!({"path": path}),
            });
        }
        if let Some(path) = &new
            && let Some(&id) = self.paths.get(path)
            && let Some(page) = id
                .ancestors(&self.arena)
                .find(|&a| self.arena[a].get().kind == Kind::Page)
        {
            self.page = Some(self.arena[page].get().path.clone());
        }
        self.focus = new;
        self.dirty = true;
        out
    }

    fn set_hover(&mut self, new: Option<String>) -> Vec<Delivery> {
        if self.hover == new {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(old) = self
            .hover
            .clone()
            .filter(|old| self.paths.contains_key(old))
        {
            let node = self.arena[self.paths[&old]].get();
            out.push(Delivery {
                target: node.owner.clone(),
                event: "ui.unhover".to_string(),
                payload: json!({"path": old}),
            });
        }
        if let Some(path) = &new
            && let Some(&id) = self.paths.get(path)
        {
            let node = self.arena[id].get();
            out.push(Delivery {
                target: node.owner.clone(),
                event: "ui.hover".to_string(),
                payload: json!({"path": path}),
            });
        }
        self.hover = new;
        self.dirty = true;
        out
    }

    // Tab cycles whole pages only; in-page navigation is mouse/hover territory
    fn page_step(&mut self, forward: bool) -> Vec<Delivery> {
        let pages: Vec<String> = self
            .root
            .children(&self.arena)
            .filter(|&c| self.arena[c].get().kind == Kind::Page)
            .map(|c| self.arena[c].get().path.clone())
            .collect();
        if pages.len() < 2 {
            return Vec::new();
        }
        let current = self.arena[self.current_page()].get().path.clone();
        let idx = pages.iter().position(|p| *p == current).unwrap_or(0);
        let next = if forward {
            (idx + 1) % pages.len()
        } else {
            (idx + pages.len() - 1) % pages.len()
        };
        self.set_focus(Some(pages[next].clone()))
    }

    pub(crate) fn current_page(&self) -> NodeId {
        let from_focus = self
            .focus
            .as_ref()
            .and_then(|f| self.paths.get(f))
            .and_then(|&id| {
                id.ancestors(&self.arena)
                    .find(|&a| self.arena[a].get().kind == Kind::Page)
            });
        if let Some(id) = from_focus {
            return id;
        }
        if let Some(page) = &self.page
            && let Some(&id) = self.paths.get(page)
            && self.arena[id].get().kind == Kind::Page
        {
            return id;
        }
        self.root
            .descendants(&self.arena)
            .skip(1)
            .find(|&id| self.arena[id].get().kind == Kind::Page)
            .unwrap_or(self.root)
    }

    // --- layout persistence ---

    fn write_layout(&mut self) {
        self.dirty = true;
        let order: serde_json::Map<String, Json> = self
            .saved_order
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        let flex: serde_json::Map<String, Json> = self
            .saved_flex
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        self.config.borrow_mut().set(
            "system-ui",
            "layout",
            json!({"locked": self.lock, "order": order, "flex": flex}),
        );
    }

    pub(crate) fn update_anims(&mut self, rects: &HashMap<String, Rect>) {
        let now = Instant::now();
        let drag_path = match &self.drag {
            DragState::Drag { path, .. } => Some(path.clone()),
            _ => None,
        };
        self.anim
            .retain(|a| rects.get(&a.path).is_some_and(|r| *r == a.to));
        for (path, rect) in rects {
            if Some(path) == drag_path.as_ref() {
                // the floating pane follows the cursor directly, never through an anim
                continue;
            }
            if self.anim.iter().any(|a| &a.path == path) {
                continue;
            }
            if let Some(old) = self.last_rects.get(path).filter(|old| *old != rect) {
                self.anim.push(Anim {
                    path: path.clone(),
                    from: *old,
                    to: *rect,
                    start: now,
                });
            }
        }
    }
}

fn focusable(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Button
            | Kind::TextInput
            | Kind::Item
            | Kind::Tabs
            | Kind::Toggle
            | Kind::Select
            | Kind::Slider
    )
}

fn lock_text(g: &Gateway) -> String {
    if g.lock {
        crate::tmss::i18n_text(&g.i18n, "home.locked")
    } else {
        crate::tmss::i18n_text(&g.i18n, "home.unlocked")
    }
}

fn parse_path(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let segs: Vec<&str> = rest.split('/').collect();
    segs.iter()
        .all(|s| !s.is_empty() && *s != "..")
        .then_some(segs)
}

fn path_string(segs: &[&str]) -> String {
    format!("/{}", segs.join("/"))
}

fn grant_covers(grant: &str, target: &[&str]) -> bool {
    let segs: Vec<&str> = grant.split('/').filter(|s| !s.is_empty()).collect();
    segs.len() <= target.len() && segs.iter().zip(target).all(|(g, t)| g == t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use terminism_kernel::host::PluginReg;

    #[test]
    fn hidden_clocks_do_not_request_frames() {
        let rig = rig("hidden-clock");
        let ui = Gateway::ui_api(&rig.gw);
        assert!((ui.create)("p", "/visible", "Page", json!({})));
        assert!((ui.create)("p", "/hidden", "Page", json!({})));
        assert!((ui.create)("p", "/hidden/clock", "Clock", json!({})));
        assert!(!rig.gw.borrow_mut().clock_tick());
    }

    #[test]
    fn idle_and_hidden_updates_do_not_redraw() {
        let rig = rig("dirty-idle");
        let ui = Gateway::ui_api(&rig.gw);
        for page in ["/visible", "/hidden"] {
            assert!((ui.create)("p", page, "Page", json!({})));
            assert!((ui.create)(
                "p",
                &format!("{page}/label"),
                "Label",
                json!({"text":"same"})
            ));
        }
        draw_once(&rig.gw, 80, 24);
        for _ in 0..1000 {
            assert!(!rig.gw.borrow_mut().take_dirty());
        }
        assert!(rig.gw.borrow().next_frame_delay().is_none());
        assert!((ui.patch)("p", "/hidden/label", json!({"text":"changed"})));
        assert!(!rig.gw.borrow_mut().take_dirty());
        assert!((ui.patch)("p", "/visible/label", json!({"text":"same"})));
        assert!(!rig.gw.borrow_mut().take_dirty());
        assert!((ui.patch)("p", "/visible/label", json!({"text":"changed"})));
        assert!(rig.gw.borrow_mut().take_dirty());
    }

    #[test]
    fn config_and_theme_changes_invalidate_the_frame() {
        let rig = rig("shared-dirty");
        let ui = Gateway::ui_api(&rig.gw);
        assert!((ui.create)("p", "/visible", "Page", json!({})));
        draw_once(&rig.gw, 80, 24);
        rig.config
            .borrow_mut()
            .set("system-ui", "layout", json!({"locked":true}));
        assert!(rig.gw.borrow_mut().take_dirty());
        assert!(rig.gw.borrow().lock);
        assert!(!rig.gw.borrow_mut().take_dirty());
        let rules = crate::parse_tmss("Page { color: red; }").unwrap();
        rig.gw.borrow().theme.borrow_mut().replace(rules.clone());
        assert!(rig.gw.borrow_mut().take_dirty());
        rig.gw.borrow().theme.borrow_mut().replace(rules);
        assert!(!rig.gw.borrow_mut().take_dirty());
    }

    #[test]
    fn styling_an_input_preserves_its_cursor() {
        let rig = rig("input-style");
        let ui = Gateway::ui_api(&rig.gw);
        assert!((ui.create)("p", "/input", "Page", json!({})));
        assert!((ui.create)(
            "p",
            "/input/text",
            "TextInput",
            json!({"text":"abc"})
        ));
        assert!((ui.set_focus)("p", "/input/text"));
        rig.gw
            .borrow_mut()
            .handle_event(&key(KeyCode::Left, KeyModifiers::NONE));
        let before = rig.gw.borrow().inputs["/input/text"].cursor();
        assert!((ui.patch)("p", "/input/text", json!({"color":"red"})));
        assert_eq!(rig.gw.borrow().inputs["/input/text"].cursor(), before);
    }

    #[test]
    fn remove_owned_releases_the_entire_tree() {
        let rig = rig("owned-removal");
        let ui = Gateway::ui_api(&rig.gw);
        assert!((ui.create)("p", "/owned", "Page", json!({})));
        assert!((ui.create)(
            "p",
            "/owned/input",
            "TextInput",
            json!({"text":"value"})
        ));
        (ui.remove_owned)("p");
        let gw = rig.gw.borrow();
        assert!(!gw.paths.contains_key("/owned"));
        assert!(gw.inputs.is_empty());
        assert_eq!(gw.arena.iter().filter(|n| !n.is_removed()).count(), 1);
    }

    #[test]
    fn repeated_removal_reuses_arena_slots() {
        let rig = rig("arena-removal");
        let ui = Gateway::ui_api(&rig.gw);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        for _ in 0..1000 {
            assert!((ui.create)("p", "/owned", "Page", json!({})));
            assert!((ui.create)("p", "/owned/input", "TextInput", json!({})));
            assert!((ui.create)(
                "p",
                "/owned/select",
                "Select",
                json!({"titles":["a","b"]})
            ));
            assert!((ui.set_focus)("p", "/owned/input"));
            assert!((ui.set_hover)("p", "/owned/select"));
            terminal
                .draw(|frame| rig.gw.borrow_mut().draw(frame))
                .unwrap();
            {
                let mut gw = rig.gw.borrow_mut();
                gw.select_open = Some("/owned/select".into());
                gw.select_hits.push(("/owned/select".into(), 0, 0, 10, 1));
                gw.drag = DragState::Drag {
                    parent: "/owned".into(),
                    path: "/owned/input".into(),
                    pending: InsertAt::End,
                    cursor: (1, 1),
                    grab: (0, 0),
                    size: (10, 2),
                };
                gw.anim.push(Anim {
                    path: "/owned/input".into(),
                    from: Rect::new(0, 0, 10, 2),
                    to: Rect::new(0, 2, 10, 2),
                    start: Instant::now(),
                });
            }
            assert!((ui.remove)("p", "/owned"));
            let gw = rig.gw.borrow();
            assert!(gw.paths.is_empty());
            assert!(gw.inputs.is_empty() && gw.last_rects.is_empty());
            assert!(gw.focus.is_none() && gw.hover.is_none());
            assert!(gw.anim.is_empty() && matches!(gw.drag, DragState::Idle));
            assert!(gw.tab_hits.is_empty() && gw.select_hits.is_empty());
            assert!(gw.select_open.is_none() && gw.pending.is_empty());
        }
        let gw = rig.gw.borrow();
        assert_eq!(gw.arena.iter().filter(|n| !n.is_removed()).count(), 1);
        assert!(gw.arena.len() <= 4);
    }

    #[test]
    #[ignore]
    fn profile_draw() {
        for count in [100, 1000] {
            let rig = rig(&format!("profile-{count}"));
            let ui = Gateway::ui_api(&rig.gw);
            assert!((ui.create)("p", "/profile", "Page", json!({})));
            assert!((ui.create)("p", "/profile/list", "List", json!({})));
            for i in 0..count {
                assert!((ui.create)(
                    "p",
                    &format!("/profile/list/n{i}"),
                    "Item",
                    json!({"text":"profile"})
                ));
            }
            let backend = ratatui::backend::TestBackend::new(120, 40);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            for _ in 0..10 {
                terminal.draw(|f| rig.gw.borrow_mut().draw(f)).unwrap();
            }
            let mut micros = Vec::new();
            for _ in 0..100 {
                let start = Instant::now();
                terminal.draw(|f| rig.gw.borrow_mut().draw(f)).unwrap();
                micros.push(start.elapsed().as_micros());
            }
            micros.sort();
            println!(
                "nodes={count} median_us={} p95_us={}",
                micros[50], micros[95]
            );
        }
    }

    struct Rig {
        gw: Rc<RefCell<Gateway>>,
        config: Rc<RefCell<ConfigStore>>,
        registry: Rc<RefCell<PluginRegistry>>,
        sink: LogStore,
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("terminism-ui-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rig(tag: &str) -> Rig {
        rig_with(tag, "")
    }

    fn rig_with(tag: &str, tmss: &str) -> Rig {
        let (config, _) = ConfigStore::load(&tmpdir(tag).join("config.toml"));
        let config = Rc::new(RefCell::new(config));
        let registry = Rc::new(RefCell::new(PluginRegistry::new()));
        let sink = LogStore::default();
        let theme = if tmss.is_empty() {
            Vec::new()
        } else {
            crate::tmss::parse(tmss).expect("test theme parses")
        };
        let gw = Gateway::new(
            sink.clone(),
            config.clone(),
            registry.clone(),
            Rc::new(RefCell::new(crate::ThemeState::new(theme))),
            Rc::new(RefCell::new(std::collections::HashMap::new())),
        );
        Rig {
            gw,
            config,
            registry,
            sink,
        }
    }

    #[test]
    fn theme_colors_render_and_inline_wins() {
        let rig = rig_with(
            "theme-colors",
            "Button { color: green; }\n/pg/lbl { color: yellow; }\n",
        );
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        // inline red on the button must beat the theme's green
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go", "color": "red"})
        ));
        assert!(create(&gw, "p1", "/pg/lbl", "Label", json!({"text": "hi"})));
        assert!(create(
            &gw,
            "p1",
            "/pg/focus",
            "Button",
            json!({"text": "f"})
        ));
        // focus lands on /pg/btn first; a second Tab frees it from reversal
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let backend = ratatui::backend::TestBackend::new(30, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            buf.content
                .iter()
                .any(|c| c.symbol() == "o" && c.fg == ratatui::style::Color::Red),
            "inline red wins over theme green"
        );
        assert!(
            buf.content
                .iter()
                .any(|c| c.symbol() == "h" && c.fg == ratatui::style::Color::Yellow),
            "theme yellow applies where inline is absent"
        );
    }

    fn reg(rig: &Rig, id: &str, grants: &[&str]) {
        rig.registry.borrow_mut().insert(
            id.to_string(),
            PluginReg {
                manifest: {
                    let mut m =
                        terminism_kernel::manifest::parse(&format!("id = \"{id}\"\n")).unwrap();
                    m.ui_grant = grants.iter().map(|s| s.to_string()).collect();
                    m
                },
                state: terminism_kernel::host::PluginState::Active,
                logger: terminism_kernel::logger::Logger::new("init.logger", rig.sink.clone()),
                started: Instant::now(),
                kind: "rhai",
                dir: None,
            },
        );
    }

    fn create(gw: &Rc<RefCell<Gateway>>, actor: &str, path: &str, kind: &str, props: Json) -> bool {
        (Gateway::ui_api(gw).create)(actor, path, kind, props)
    }

    fn patch(gw: &Rc<RefCell<Gateway>>, actor: &str, path: &str, props: Json) -> bool {
        (Gateway::ui_api(gw).patch)(actor, path, props)
    }

    fn remove(gw: &Rc<RefCell<Gateway>>, actor: &str, path: &str) -> bool {
        (Gateway::ui_api(gw).remove)(actor, path)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    // column of a needle in a flat buffer string, immune to multibyte box glyphs
    fn col_of(text: &str, needle: &str) -> Option<usize> {
        text.find(needle).map(|b| text[..b].chars().count())
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }
    fn force_drag_start(gw: &Rc<RefCell<Gateway>>, x: u16, y: u16) {
        let path = gw.borrow().hit_test(x, y).expect("hit for drag start");
        gw.borrow_mut().begin_drag(&path, (x, y), (x, y));
    }

    fn send(gw: &Rc<RefCell<Gateway>>, ev: &Event) -> Vec<Delivery> {
        gw.borrow_mut().handle_event(ev)
    }

    fn draw_once(gw: &Rc<RefCell<Gateway>>, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn build_split(rig: &Rig, gw: &Rc<RefCell<Gateway>>) {
        reg(rig, "p1", &[]);
        assert!(create(gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            gw,
            "p1",
            "/pg/s",
            "Split",
            json!({"direction": "row"})
        ));
        for name in ["a", "b", "c"] {
            assert!(create(
                gw,
                "p1",
                &format!("/pg/s/{name}"),
                "Button",
                json!({"text": name})
            ));
        }
    }

    #[test]
    fn acl_four_rules() {
        let rig = rig("acl");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        reg(&rig, "p2", &[]);
        assert!(!create(&gw, "p1", "/", "Page", json!({})));
        assert!(!create(&gw, "p1", "/a/../b", "Page", json!({})));
        assert!(create(&gw, "p1", "/page-a", "Page", json!({})));
        assert!(patch(&gw, "p1", "/page-a", json!({"title": "A"})));
        assert!(!patch(&gw, "p2", "/page-a", json!({"title": "B"})));
        assert!(create(
            &gw,
            "p1",
            "/page-a/btn",
            "Button",
            json!({"text": "go"})
        ));
        reg(&rig, "p2", &["/page-a"]);
        assert!(patch(&gw, "p2", "/page-a/btn", json!({"text": "go!"})));
        assert!(remove(&gw, "p1", "/page-a"));
        let g = gw.borrow();
        assert!(!g.paths.contains_key("/page-a"));
        assert!(!g.paths.contains_key("/page-a/btn"));
    }

    #[test]
    fn popup_single() {
        let rig = rig("popup");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg/sec", "Section", json!({})));
        assert!(!create(&gw, "p1", "/pg/sec/pop", "Popup", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/pop1",
            "Popup",
            json!({"title": "one"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/pop2",
            "Popup",
            json!({"title": "two"})
        ));
        let g = gw.borrow();
        assert!(g.paths.contains_key("/pg/pop2"));
        assert!(!g.paths.contains_key("/pg/pop1"));
    }

    #[test]
    fn row_requires_table_parent() {
        let rig = rig("row-table");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/tbl",
            "Table",
            json!({"columns": ["a"], "widths": ["*"]})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/tbl/r1",
            "Row",
            json!({"cells": ["1"]})
        ));
        assert!(create(&gw, "p1", "/pg/sec", "Section", json!({})));
        assert!(!create(
            &gw,
            "p1",
            "/pg/sec/r2",
            "Row",
            json!({"cells": ["1"]})
        ));
        assert!(!create(&gw, "p1", "/pg/r3", "Row", json!({"cells": ["1"]})));
    }

    #[test]
    fn focus_order_and_page() {
        let rig = rig("page-step");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg1/btn", "Button", json!({})));
        assert!(create(&gw, "p1", "/pg2", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg2/btn", "Button", json!({})));
        {
            // no focus yet: the first built page is current (gateway builds none)
            let g = gw.borrow();
            assert_eq!(g.current_page(), g.paths["/pg1"]);
        }
        // Tab switches whole pages, focus lands on the page node itself
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        {
            let g = gw.borrow();
            assert_eq!(g.focus.as_deref(), Some("/pg2"));
            assert_eq!(g.current_page(), g.paths["/pg2"]);
        }
        // BackTab wraps back to the first page
        send(&gw, &key(KeyCode::BackTab, KeyModifiers::NONE));
        {
            let g = gw.borrow();
            assert_eq!(g.current_page(), g.paths["/pg1"]);
        }
    }

    #[test]
    fn focus_deliveries() {
        let rig = rig("focus-deliv");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        reg(&rig, "p2", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg1/btn", "Button", json!({})));
        assert!(create(&gw, "p2", "/pg2", "Page", json!({})));
        assert!(create(&gw, "p2", "/pg2/txt", "TextInput", json!({})));
        // Tab is page-only now; node focus cross-check uses programmatic focus
        let first = gw.borrow_mut().set_focus(Some("/pg1/btn".to_string()));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].target, "p1");
        assert_eq!(first[0].event, "ui.focus");
        assert_eq!(first[0].payload, json!({"path": "/pg1/btn"}));
        let second = gw.borrow_mut().set_focus(Some("/pg2/txt".to_string()));
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].target, "p1");
        assert_eq!(second[0].event, "ui.blur");
        assert_eq!(second[1].target, "p2");
        assert_eq!(second[1].event, "ui.focus");
        assert_eq!(second[1].payload, json!({"path": "/pg2/txt"}));
    }

    #[test]
    fn text_input_tui_input() {
        let rig = rig("text-input");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/in",
            "TextInput",
            json!({"placeholder": "type"})
        ));
        assert!(send(&gw, &key(KeyCode::Char('h'), KeyModifiers::NONE)).is_empty());
        let _ = gw.borrow_mut().set_focus(Some("/pg/in".to_string()));
        let d = send(&gw, &key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].target, "p1");
        assert_eq!(d[0].event, "ui.input");
        assert_eq!(
            d[0].payload,
            json!({"path": "/pg/in", "text": "h", "cursor": 1})
        );
        send(&gw, &key(KeyCode::Char('i'), KeyModifiers::NONE));
        // cursor-only move still reports through ui.input
        let d = send(&gw, &key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            d[0].payload,
            json!({"path": "/pg/in", "text": "hi", "cursor": 1})
        );
        let d = send(&gw, &key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            d[0].payload,
            json!({"path": "/pg/in", "text": "i", "cursor": 0})
        );
        let g = gw.borrow();
        assert_eq!(
            g.arena[g.paths["/pg/in"]].get().props.text.as_deref(),
            Some("i")
        );
        assert_eq!(g.inputs["/pg/in"].cursor(), 0);
        drop(g);
        // paste is inserted at the cursor position
        let d = send(&gw, &Event::Paste("xy".to_string()));
        assert_eq!(
            d[0].payload,
            json!({"path": "/pg/in", "text": "xyi", "cursor": 2})
        );
    }

    #[test]
    fn tabs_enter_cycles() {
        let rig = rig("tabs");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/tabs",
            "Tabs",
            json!({"titles": ["a", "b", "c"], "selected": 0})
        ));
        let _ = gw.borrow_mut().set_focus(Some("/pg/tabs".to_string()));
        let d = send(&gw, &key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].target, "p1");
        assert_eq!(d[0].event, "ui.activate");
        assert_eq!(d[0].payload, json!({"path": "/pg/tabs", "selected": 1}));
        send(&gw, &key(KeyCode::Enter, KeyModifiers::NONE));
        send(&gw, &key(KeyCode::Enter, KeyModifiers::NONE));
        let g = gw.borrow();
        assert_eq!(g.arena[g.paths["/pg/tabs"]].get().props.selected, Some(0));
    }

    #[test]
    fn split_resize_locked() {
        let rig = rig("resize");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        let _ = gw.borrow_mut().set_focus(Some("/pg/s/a".to_string()));
        send(&gw, &key(KeyCode::Right, KeyModifiers::CONTROL));
        {
            let g = gw.borrow();
            assert_eq!(g.arena[g.paths["/pg/s/a"]].get().props.flex, Some(1.1));
        }
        let stored = rig
            .config
            .borrow()
            .get("system-ui", "layout")
            .and_then(|j| j.get("flex"))
            .and_then(|f| f.get("/pg/s/a"))
            .cloned();
        assert_eq!(stored, Some(json!(1.1f32 as f64)));
        send(&gw, &key(KeyCode::Char('L'), KeyModifiers::NONE));
        send(&gw, &key(KeyCode::Right, KeyModifiers::CONTROL));
        let g = gw.borrow();
        assert_eq!(g.arena[g.paths["/pg/s/a"]].get().props.flex, Some(1.1));
    }

    #[test]
    fn drag_reorder() {
        let rig = rig("drag");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        let c_id = gw.borrow().paths["/pg/s/c"];
        // focus the page first so draw records its rects for hit testing
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        force_drag_start(&gw, 33, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 39, 5));
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 15, 5));
        let out = send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 15, 5));
        assert!(out.is_empty());
        let g = gw.borrow();
        let order: Vec<String> = g.paths["/pg/s"]
            .children(&g.arena)
            .map(|c| g.arena[c].get().path.clone())
            .collect();
        assert_eq!(order, ["/pg/s/a", "/pg/s/c", "/pg/s/b"]);
        assert_eq!(g.paths["/pg/s/c"], c_id);
        assert_eq!(g.arena[c_id].get().path, "/pg/s/c");
    }

    #[test]
    fn tmss_locked_pane_cannot_be_dragged() {
        let rig = rig_with("drag-locked", "/pg/s/b { locked: yes; }");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        // b is pinned: dragging from inside b never picks it up
        force_drag_start(&gw, 18, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 3, 5));
        assert!(
            !matches!(gw.borrow().drag, DragState::Drag { .. }),
            "locked pane must not enter drag"
        );
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 3, 5));
        {
            let g = gw.borrow();
            let order: Vec<String> = g.paths["/pg/s"]
                .children(&g.arena)
                .map(|c| g.arena[c].get().path.clone())
                .collect();
            assert_eq!(order, ["/pg/s/a", "/pg/s/b", "/pg/s/c"]);
        }
        // a is not pinned: the same gesture still drags it
        force_drag_start(&gw, 5, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 30, 5));
        assert!(matches!(gw.borrow().drag, DragState::Drag { .. }));
    }

    #[test]
    fn drag_pops_out_and_siblings_close_gap() {
        let rig = rig("drag-pop");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let seeded = draw_once(&gw, 40, 12);
        assert_eq!(col_of(&seeded, "[ c ]"), Some(27));
        // press inside c and drag anywhere: c pops out, a and b close the gap
        force_drag_start(&gw, 33, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 20, 6));
        {
            // the arena order must still be untouched before release
            let g = gw.borrow();
            let order: Vec<String> = g.paths["/pg/s"]
                .children(&g.arena)
                .map(|c| g.arena[c].get().path.clone())
                .collect();
            assert_eq!(order, ["/pg/s/a", "/pg/s/b", "/pg/s/c"]);
        }
        // grab offset is (6,5), so the floating pane starts at column 14;
        // siblings easing into the closed-gap layout get a regular anim
        let text = draw_once(&gw, 40, 12);
        assert!(
            gw.borrow().anim.iter().any(|a| a.path == "/pg/s/a"),
            "sibling reflow must animate"
        );
        assert_eq!(
            col_of(&text, "[ c ]"),
            Some(14),
            "c floats at the cursor: {text}"
        );
        assert_eq!(
            text.find("[ a ]"),
            Some(0),
            "a keeps the first slot: {text}"
        );
    }

    #[test]
    fn drag_floating_follows_cursor() {
        let rig = rig("drag-follow");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        force_drag_start(&gw, 33, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 33, 8));
        assert_eq!(
            col_of(&draw_once(&gw, 40, 12), "[ c ]"),
            Some(27),
            "first floating position"
        );
        // keep holding and move again: the pane must track the cursor
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 26, 8));
        assert_eq!(
            col_of(&draw_once(&gw, 40, 12), "[ c ]"),
            Some(20),
            "second floating position"
        );
    }

    #[test]
    fn drop_eases_from_cursor_position() {
        let rig = rig("drag-snap");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        force_drag_start(&gw, 33, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 28, 8));
        // one floating frame so last_rects records the release position,
        // then another drag event so pending refreshes against the closed layout
        draw_once(&gw, 40, 12);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 28, 8));
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 28, 8));
        // pointer x=28 sits next to b, so the drop lands between a and b
        draw_once(&gw, 40, 12);
        let g = gw.borrow();
        let anim = g
            .anim
            .iter()
            .find(|a| a.path == "/pg/s/c")
            .expect("drop must ease from the cursor position");
        assert_eq!(anim.from.x, 22, "ease starts at the floating position");
        assert_eq!(anim.to.x, 13, "ease ends at the preview slot");
    }

    #[test]
    fn drag_indicators_monochrome() {
        let rig = rig("drag-marks");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        force_drag_start(&gw, 33, 5);
        // drop target is the end slot; the pane floats left of its far border
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 30, 5));
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            buf.content
                .iter()
                .any(|c| c.symbol() == "│" && c.fg == ratatui::style::Color::Rgb(0x9A, 0xA0, 0xA6)),
            "dashed slot ring must render in fixed gray"
        );
        assert!(
            buf.content
                .iter()
                .any(|c| c.symbol() == "c" && c.fg == ratatui::style::Color::White),
            "dragged node must render with a white tint"
        );
    }

    #[test]
    fn anim_basic() {
        let rig = rig("anim");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        assert!(!gw.borrow().anim_active());
        assert!(patch(&gw, "p1", "/pg/s/a", json!({"flex": 3.0})));
        draw_once(&gw, 40, 12);
        assert!(gw.borrow().anim_active());
        let g = gw.borrow();
        let anim = g.anim.iter().find(|a| a.path == "/pg/s/a").unwrap().clone();
        assert_ne!(anim.from, anim.to);
        let target = lerp_rect(&anim, anim.start + std::time::Duration::from_secs(1));
        assert_eq!(target, anim.to);
    }

    #[test]
    fn layout_persist_roundtrip() {
        let rig = rig("persist");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        let _ = gw.borrow_mut().set_focus(Some("/pg/s/a".to_string()));
        draw_once(&gw, 40, 12);
        send(&gw, &key(KeyCode::Right, KeyModifiers::CONTROL));
        force_drag_start(&gw, 33, 5);
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 39, 5));
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 15, 5));
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 15, 5));
        send(&gw, &key(KeyCode::Char('L'), KeyModifiers::NONE));
        let layout = rig
            .config
            .borrow()
            .get("system-ui", "layout")
            .cloned()
            .unwrap();
        assert_eq!(
            layout,
            json!({
                "locked": true,
                "order": {"/pg/s": ["/pg/s/a", "/pg/s/c", "/pg/s/b"]},
                "flex": {"/pg/s/a": 1.1f32 as f64},
            })
        );
        // a fresh gateway restores order, flex weight and lock from the archive
        let gw2 = Gateway::new(
            rig.sink.clone(),
            rig.config.clone(),
            rig.registry.clone(),
            crate::Theme::default(),
            Rc::new(RefCell::new(std::collections::HashMap::new())),
        );
        build_split(&rig, &gw2);
        let g = gw2.borrow();
        let order: Vec<String> = g.paths["/pg/s"]
            .children(&g.arena)
            .map(|c| g.arena[c].get().path.clone())
            .collect();
        assert_eq!(order, ["/pg/s/a", "/pg/s/c", "/pg/s/b"]);
        assert_eq!(g.arena[g.paths["/pg/s/a"]].get().props.flex, Some(1.1));
        assert!(g.lock);
    }

    #[test]
    fn data_shapes_invalid_renders_placeholder() {
        let rig = rig("bad-data");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/ch",
            "Chart",
            json!({"data": "nope"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/bar",
            "BarChart",
            json!({"data": [{"label": "x"}]})
        ));
        // a focusable node makes the page current
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go"})
        ));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let text = draw_once(&gw, 60, 20);
        assert_eq!(text.matches("<invalid data>").count(), 2, "buffer:\n{text}");
    }

    #[test]
    fn draw_smoke() {
        let rig = rig("draw");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/tbl",
            "Table",
            json!({"columns": ["a", "b"], "widths": ["6", "*"], "title": "T"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/tbl/r1",
            "Row",
            json!({"cells": ["1", "2"]})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/ch",
            "Chart",
            json!({"data": [{"name": "s", "points": [[0.0, 0.0], [10.0, 10.0]]}]})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/tabs",
            "Tabs",
            json!({"titles": ["T1", "T2"], "selected": 0})
        ));
        assert!(create(&gw, "p1", "/pg/ok", "Button", json!({"text": "OK"})));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let text = draw_once(&gw, 60, 20);
        assert!(text.contains("[ OK ]"), "buffer:\n{text}");
        assert!(text.contains("T1"), "buffer:\n{text}");
    }

    #[test]
    fn item_focus_renders_reversed() {
        let rig = rig("item-focus");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/hint",
            "Label",
            json!({"text": "pick", "align": "center"})
        ));
        assert!(create(&gw, "p1", "/pg/list", "List", json!({"title": "L"})));
        assert!(create(
            &gw,
            "p1",
            "/pg/list/it",
            "Item",
            json!({"text": "entry"})
        ));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let reversed = buffer
            .content
            .iter()
            .any(|c| c.symbol() == "e" && c.modifier.contains(ratatui::style::Modifier::REVERSED));
        assert!(reversed, "focused item must render reversed");
        let centered = buffer.content.chunks(40).any(|row| {
            let text: String = row.iter().map(|c| c.symbol()).collect();
            text.find("pick").is_some_and(|col| col > 10)
        });
        assert!(
            centered,
            "align center must offset the label from the left edge"
        );
    }

    #[test]
    fn section_children_inside_border() {
        let rig = rig("section");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/sec",
            "Section",
            json!({"title": "S"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/sec/lbl",
            "Label",
            json!({"text": "inside"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go"})
        ));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let backend = ratatui::backend::TestBackend::new(20, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = buffer
            .content
            .chunks(20)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        assert!(
            rows[0].starts_with('┌'),
            "row 0 must be the border: {}",
            rows[0]
        );
        assert!(
            !rows[0].contains("inside"),
            "label must not paint over the top border: {}",
            rows[0]
        );
        assert!(rows.iter().skip(1).any(|r| r.contains("inside")));
    }

    #[test]
    fn label_wraps_long_text() {
        let rig = rig("wrap");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/lbl",
            "Label",
            json!({"text": "aaaa bbbb cccc dddd"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go"})
        ));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let backend = ratatui::backend::TestBackend::new(10, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = buffer
            .content
            .chunks(10)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        assert!(
            rows[0].contains("aaaa"),
            "first words on row 0: {}",
            rows[0]
        );
        assert!(
            rows[1].contains("cccc"),
            "overflow words must wrap to row 1: {rows:?}"
        );
    }

    fn hover_rig(tag: &str) -> Rc<RefCell<Gateway>> {
        let rig = rig(tag);
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg/list", "List", json!({"title": "L"})));
        assert!(create(
            &gw,
            "p1",
            "/pg/list/it1",
            "Item",
            json!({"text": "one"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/list/it2",
            "Item",
            json!({"text": "two"})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go"})
        ));
        let _ = gw.borrow_mut().set_focus(Some("/pg/list/it1".to_string()));
        draw_once(&gw, 40, 12);
        gw
    }

    #[test]
    fn hover_follows_mouse() {
        let gw = hover_rig("hover");
        let d = send(&gw, &mouse(MouseEventKind::Moved, 2, 1));
        assert!(
            d.iter()
                .any(|x| x.event == "ui.hover" && x.payload["path"] == "/pg/list/it1"),
            "{d:?}"
        );
        let d = send(&gw, &mouse(MouseEventKind::Moved, 2, 2));
        assert!(
            d.iter()
                .any(|x| x.event == "ui.unhover" && x.payload["path"] == "/pg/list/it1"),
            "{d:?}"
        );
        assert!(
            d.iter()
                .any(|x| x.event == "ui.hover" && x.payload["path"] == "/pg/list/it2"),
            "{d:?}"
        );
        // below the page subtree (status row) clears the hover
        let d = send(&gw, &mouse(MouseEventKind::Moved, 2, 11));
        assert!(
            d.iter()
                .any(|x| x.event == "ui.unhover" && x.payload["path"] == "/pg/list/it2"),
            "{d:?}"
        );
        assert!(send(&gw, &mouse(MouseEventKind::Moved, 2, 11)).is_empty());
    }

    #[test]
    fn hover_drag_suppressed() {
        let gw = hover_rig("hover-drag");
        assert!(send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), 2, 1)).is_empty());
        assert!(send(&gw, &mouse(MouseEventKind::Moved, 2, 4)).is_empty());
        // the release is a click on an already-focused item: it must still
        // activate (tree toggles), delivered instead of silence
        let out = send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 2, 4));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "ui.activate");
        assert_eq!(out[0].payload["path"], "/pg/list/it1");
    }

    #[test]
    fn arrows_deliver_ui_key_and_enter_uses_hover() {
        let gw = hover_rig("hover-key");
        // focus sits on it1; plain arrows become ui.key for the hover owner
        let d = send(&gw, &key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].event, "ui.key");
        assert_eq!(d[0].target, "p1");
        assert_eq!(d[0].payload["path"], "/pg/list/it1");
        assert_eq!(d[0].payload["key"], "down");
        // hover it2 via mouse, Enter must focus what is hovered
        send(&gw, &mouse(MouseEventKind::Moved, 2, 2));
        let d = send(&gw, &key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            d.iter()
                .any(|x| x.event == "ui.focus" && x.payload["path"] == "/pg/list/it2"),
            "{d:?}"
        );
        // hover and focus now agree; Enter on an Item does nothing
        assert!(send(&gw, &key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn textinput_keeps_plain_arrows() {
        let rig = rig("hover-input");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/in",
            "TextInput",
            json!({"text": "abc"})
        ));
        let _ = gw.borrow_mut().set_focus(Some("/pg/in".to_string()));
        let d = send(&gw, &key(KeyCode::Left, KeyModifiers::NONE));
        assert!(d.iter().any(|x| x.event == "ui.input"), "{d:?}");
    }

    #[test]
    fn set_hover_respects_owner() {
        let rig = rig("hover-set");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        reg(&rig, "p2", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/btn",
            "Button",
            json!({"text": "go"})
        ));
        assert!(gw.borrow_mut().set_hover_by("p1", "/pg/btn"));
        assert!(!gw.borrow_mut().set_hover_by("p2", "/pg/btn"));
        assert!(!gw.borrow_mut().set_hover_by("p1", "/missing"));
    }

    #[test]
    fn clock_widget_self_updates_and_drives_dirty() {
        let rig = rig("clock-widget");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        // no Clock node: no dirty signal
        assert!(!gw.borrow_mut().clock_tick());
        assert!(create(&gw, "p1", "/pg/clock", "Clock", json!({})));
        // first tick of a new second reports dirty, repeats within it do not
        let first = gw.borrow_mut().clock_tick();
        assert!(first, "first tick with a clock must be dirty");
        assert!(!gw.borrow_mut().clock_tick());
        // the widget renders the UTC clock without any props
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(gw.borrow_mut().clock_tick(), "next second is dirty again");
        let backend = ratatui::backend::TestBackend::new(20, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("UTC"), "clock must render: {text}");
    }

    #[test]
    fn resize_announced_on_first_draw_and_change() {
        let rig = rig("resize-emit");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        let draw_at = |w: u16, h: u16| {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
            gw.borrow_mut().take_pending()
        };
        let first = draw_at(40, 10);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].event, "ui.resize");
        assert_eq!(first[0].target, "p1");
        assert_eq!(first[0].payload["width"], 40);
        // same size: silent
        assert!(draw_at(40, 10).is_empty());
        // size change: announced again
        let second = draw_at(60, 10);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].payload["width"], 60);
    }

    #[test]
    fn select_menu_hugs_content_and_flips_toggle() {
        let rig = rig("select-toggle");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/sel",
            "Select",
            json!({"titles": ["a", "bb"], "selected": 0})
        ));
        assert!(create(
            &gw,
            "p1",
            "/pg/tog",
            "Toggle",
            json!({"checked": false, "text": "x"})
        ));
        let _ = gw.borrow_mut().set_focus(Some("/pg/sel".to_string()));
        draw_once(&gw, 60, 8);
        // click opens the menu; its hit zones must be content-width, not the
        // full control width
        send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), 2, 1));
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 2, 1));
        draw_once(&gw, 60, 8);
        let hits = gw.borrow().select_hits.clone();
        assert!(!hits.is_empty(), "menu must open on click");
        let (_, _, x0, x1, _) = &hits[0];
        assert!(x1 - x0 <= 6, "menu hugs content, got width {}", x1 - x0);
        // clicking the second option commits it and closes the menu
        let (_, _, ox0, _, oy) = hits[1].clone();
        send(
            &gw,
            &mouse(MouseEventKind::Down(MouseButton::Left), ox0 + 1, oy),
        );
        let out = send(
            &gw,
            &mouse(MouseEventKind::Up(MouseButton::Left), ox0 + 1, oy),
        );
        assert!(
            out.iter().any(|d| d.event == "ui.activate"
                && d.payload["path"] == "/pg/sel"
                && d.payload["selected"] == 1),
            "{out:?}"
        );
        assert!(gw.borrow().select_open.is_none(), "menu closes on commit");
        // toggle flips through click and reports the new value
        send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), 2, 5));
        let out = send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 2, 5));
        assert!(
            out.iter().any(|x| x.event == "ui.activate"
                && x.payload["path"] == "/pg/tog"
                && x.payload["value"] == true),
            "{out:?}"
        );
    }

    #[test]
    fn cjk_status_text_keeps_tab_hits_on_glyphs() {
        let rig = rig_with("status-cjk", "/pg1 { icon: \"▤\"; }\n");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        // a CJK label is two columns per glyph: hits must still land on icons
        gw.borrow().i18n.borrow_mut().insert(
            "status.keys".to_string(),
            "Tab 切页 · Enter 激活 · L 锁定 · q 退出".to_string(),
        );
        let backend = ratatui::backend::TestBackend::new(60, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let (_, x0, x1, row) = gw.borrow().tab_hits[0].clone();
        assert_eq!(x1 - x0, 3);
        let glyph: String = (x0..x1)
            .map(|x| buf.cell((x, row)).map_or(" ", |c| c.symbol()))
            .collect();
        assert_eq!(glyph, " ▤ ", "hit zone must wrap the icon itself");
    }

    #[test]
    fn status_bar_shows_hint_and_switches_via_tab_strip() {
        let rig = rig_with(
            "status-bar",
            "/pg2 { icon: \"▤\"; }\nButton { hint: \"fire\"; }\n",
        );
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg1/btn",
            "Button",
            json!({"text": "go"})
        ));
        assert!(create(&gw, "p1", "/pg2", "Page", json!({})));
        let _ = gw.borrow_mut().set_focus(Some("/pg1/btn".to_string()));
        let backend = ratatui::backend::TestBackend::new(60, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = buf
            .content
            .chunks(60)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let status = rows.last().unwrap();
        assert!(status.contains("status.keys"), "base keys: {status}");
        assert!(status.contains("fire"), "focused hint appended: {status}");
        assert!(status.contains('▤'), "page tab strip: {status}");
        // clicking the strip icon focuses that page
        let (x0, row) = {
            let g = gw.borrow();
            let (_, x0, _, y) = g
                .tab_hits
                .iter()
                .find(|(p, ..)| p == "/pg2")
                .cloned()
                .unwrap();
            (x0, y)
        };
        let d = send(
            &gw,
            &mouse(MouseEventKind::Down(MouseButton::Left), x0 + 1, row),
        );
        assert!(
            d.iter()
                .any(|x| x.event == "ui.focus" && x.payload["path"] == "/pg2"),
            "{d:?}"
        );
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg2"]);
    }

    #[test]
    fn hover_renders_gray_background() {
        let gw = hover_rig("hover-gray");
        send(&gw, &mouse(MouseEventKind::Moved, 2, 1));
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
        let gray = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.bg == ratatui::style::Color::DarkGray);
        assert!(gray, "hovered item must paint a gray background");
    }

    #[test]
    fn set_focus_navigates_pages_and_respects_ownership() {
        let rig = rig("focus-api");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        reg(&rig, "p2", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg1/btn",
            "Button",
            json!({"text": "x"})
        ));
        assert!(create(&gw, "p2", "/pg2", "Page", json!({})));
        assert!(create(
            &gw,
            "p2",
            "/pg2/btn",
            "Button",
            json!({"text": "y"})
        ));
        // any actor may focus a page, other pages' buttons are off limits
        assert!(gw.borrow_mut().set_focus_by("p2", "/pg1"));
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg1"]);
        assert!(!gw.borrow_mut().set_focus_by("p2", "/pg1/btn"));
        assert!(gw.borrow_mut().set_focus_by("p1", "/pg1/btn"));
        assert_eq!(
            gw.borrow().focus.as_deref(),
            Some("/pg1/btn"),
            "owning actor keeps button focus rights"
        );
        assert!(!gw.borrow_mut().set_focus_by("p1", "/missing"));
    }

    #[test]
    fn theme_handle_swap_recolors_render() {
        let rig = rig_with("theme-swap", "/pg/lbl { color: red; }");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg/btn", "Button", json!({"text": "f"})));
        assert!(create(&gw, "p1", "/pg/lbl", "Label", json!({"text": "hi"})));
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        let cell_fg = |want: fn(&ratatui::buffer::Cell) -> bool| {
            let backend = ratatui::backend::TestBackend::new(20, 6);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gw.borrow_mut().draw(f)).unwrap();
            terminal.backend().buffer().content.iter().any(want)
        };
        assert!(cell_fg(
            |c| c.symbol() == "h" && c.fg == ratatui::style::Color::Red
        ));
        // a styles-manager swap replaces the whole rule set at runtime
        let next = crate::tmss::parse("/pg/lbl { color: blue; }").unwrap();
        rig.gw.borrow().theme.borrow_mut().replace(next);
        assert!(cell_fg(
            |c| c.symbol() == "h" && c.fg == ratatui::style::Color::Blue
        ));
    }

    #[test]
    fn removed_focus_keeps_current_page() {
        let rig = rig("page-survive-focus");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg1/btn", "Button", json!({})));
        assert!(create(&gw, "p1", "/pg2", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg2/btn", "Button", json!({})));
        let _ = gw.borrow_mut().set_focus(Some("/pg2/btn".to_string()));
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg2"]);
        assert!(remove(&gw, "p1", "/pg2/btn"));
        {
            let g = gw.borrow();
            assert!(g.focus.is_none());
            assert_eq!(
                g.current_page(),
                g.paths["/pg2"],
                "page must survive focus removal"
            );
        }
        assert!(create(&gw, "p1", "/pg2/btn", "Button", json!({})));
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg2"]);
    }

    #[test]
    fn removed_page_falls_back_to_remaining_page() {
        let rig = rig("page-remove-fallback");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg1", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg1/btn", "Button", json!({})));
        assert!(create(&gw, "p1", "/pg2", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg2/btn", "Button", json!({})));
        let _ = gw.borrow_mut().set_focus(Some("/pg2/btn".to_string()));
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg2"]);
        assert!(remove(&gw, "p1", "/pg2"));
        assert_eq!(gw.borrow().current_page(), gw.borrow().paths["/pg1"]);
    }
    #[test]
    fn slider_adjusts_with_arrow_keys() {
        let rig = rig("slider-keys");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/slider",
            "Slider",
            json!({"value": 0.5, "text": "memory"})
        ));
        let _ = gw.borrow_mut().set_focus(Some("/pg/slider".to_string()));
        let right = send(&gw, &key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(right.len(), 1);
        assert_eq!(right[0].event, "ui.input");
        let value = right[0]
            .payload
            .get("value")
            .and_then(serde_json::Value::as_f64)
            .unwrap();
        assert!((value - 0.55).abs() < 1e-6);
        let left = send(&gw, &key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(left.len(), 1);
    }
    #[test]
    fn pane_drag_requires_double_click_on_border() {
        let rig = rig("drag-gate");
        let gw = rig.gw.clone();
        build_split(&rig, &gw);
        send(&gw, &key(KeyCode::Tab, KeyModifiers::NONE));
        draw_once(&gw, 40, 12);
        // one drag from inside c must not pick the pane up
        send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), 33, 5));
        send(&gw, &mouse(MouseEventKind::Drag(MouseButton::Left), 20, 5));
        assert!(!matches!(gw.borrow().drag, DragState::Drag { .. }));
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), 20, 5));
        // double click on c's right border then keep holding: drag starts
        let (border_x, border_y) = {
            let g = gw.borrow();
            let rect = g.last_rects["/pg/s/c"];
            (rect.x + rect.width - 1, rect.y + rect.height / 2)
        };
        send(
            &gw,
            &mouse(MouseEventKind::Down(MouseButton::Left), border_x, border_y),
        );
        send(
            &gw,
            &mouse(MouseEventKind::Up(MouseButton::Left), border_x, border_y),
        );
        send(
            &gw,
            &mouse(MouseEventKind::Down(MouseButton::Left), border_x, border_y),
        );
        send(
            &gw,
            &mouse(
                MouseEventKind::Drag(MouseButton::Left),
                border_x - 5,
                border_y,
            ),
        );
        assert!(matches!(gw.borrow().drag, DragState::Drag { .. }));
        send(
            &gw,
            &mouse(
                MouseEventKind::Up(MouseButton::Left),
                border_x - 5,
                border_y,
            ),
        );
    }

    #[test]
    fn slider_click_and_drag_updates_value() {
        let rig = rig("slider-drag");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/sl",
            "Slider",
            json!({"value": 0.0, "text": "mem"})
        ));
        draw_once(&gw, 40, 12);
        let rect = gw.borrow().last_rects["/pg/sl"];
        let y = rect.y + rect.height / 2;
        let down = send(
            &gw,
            &mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 2, y),
        );
        assert!(matches!(gw.borrow().drag, DragState::Slider { .. }));
        assert_eq!(down.iter().filter(|d| d.event == "ui.input").count(), 1);
        let middle = send(
            &gw,
            &mouse(
                MouseEventKind::Drag(MouseButton::Left),
                rect.x + rect.width / 2,
                y,
            ),
        );
        let value = middle
            .iter()
            .find(|d| d.event == "ui.input")
            .and_then(|d| d.payload.get("value"))
            .and_then(|value| value.as_f64())
            .unwrap();
        assert!(value > 0.2 && value < 0.8, "dragged value {value}");
        send(
            &gw,
            &mouse(
                MouseEventKind::Up(MouseButton::Left),
                rect.x + rect.width / 2,
                y,
            ),
        );
        assert!(matches!(gw.borrow().drag, DragState::Idle));
    }
    #[test]
    fn slider_inside_list_occupies_one_row_and_drags() {
        let rig = rig("list-slider");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(&gw, "p1", "/pg/list", "List", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/list/mem",
            "Slider",
            json!({"value": 0.25, "text": "mem 4096 MB"})
        ));
        draw_once(&gw, 40, 12);
        let rect = gw.borrow().last_rects["/pg/list/mem"];
        assert_eq!(rect.height, 1, "slider must occupy exactly one list row");
        let down = send(
            &gw,
            &mouse(
                MouseEventKind::Down(MouseButton::Left),
                rect.x + rect.width / 2,
                rect.y,
            ),
        );
        assert!(matches!(gw.borrow().drag, DragState::Slider { .. }));
        assert!(down.iter().any(|d| d.event == "ui.input"));
    }
    #[test]
    fn slider_double_click_requests_edit() {
        let rig = rig("slider-edit");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({})));
        assert!(create(
            &gw,
            "p1",
            "/pg/sl",
            "Slider",
            json!({"value": 0.5, "text": "mem|4096 MB"})
        ));
        draw_once(&gw, 40, 12);
        let rect = gw.borrow().last_rects["/pg/sl"];
        let x = rect.x + rect.width / 2;
        let y = rect.y;
        let _ = send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        send(&gw, &mouse(MouseEventKind::Up(MouseButton::Left), x, y));
        let second = send(&gw, &mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert!(
            second
                .iter()
                .any(|d| d.event == "ui.edit" && d.target == "p1")
        );
        assert!(matches!(gw.borrow().drag, DragState::Idle));
    }
    #[test]
    fn popups_overlay_and_do_not_shrink_sibling_layout() {
        let rig = rig("popup-overlay");
        let gw = rig.gw.clone();
        reg(&rig, "p1", &[]);
        assert!(create(&gw, "p1", "/pg", "Page", json!({"title": "P"})));
        assert!(create(
            &gw,
            "p1",
            "/pg/split",
            "Split",
            json!({"direction": "row"})
        ));
        assert!(create(&gw, "p1", "/pg/split/a", "Section", json!({})));
        assert!(create(&gw, "p1", "/pg/split/b", "Section", json!({})));
        assert!(create(&gw, "p1", "/pg/dlg", "Popup", json!({"title": "D"})));
        draw_once(&gw, 60, 20);
        let page = gw.borrow().last_rects["/pg"];
        assert_eq!(gw.borrow().last_rects["/pg/split"], page);
        let dialog = gw.borrow().last_rects["/pg/dlg"];
        assert!(dialog.width < page.width && dialog.height < page.height);
        assert!(dialog.x > page.x && dialog.y > page.y);
    }
}
