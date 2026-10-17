mod native_plugins;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use terminism_kernel::host::{ConfigStore, DataStore, HostError, PluginHost, PluginRegistry};
use terminism_kernel::logger::{LogStore, Logger};
use terminism_systemui::{
    Account, Gateway, Instances, StylesManager, SystemPages, ThemeState, UIManager, load_theme,
};

use crate::native_plugins::{
    Auth, Cli, Configuration, I18n, Init, Launcher, Loader, LoggerPlugin, Manager,
};

#[derive(Debug, thiserror::Error)]
enum ShellError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Host(#[from] HostError),
}

fn main() {
    if let Err(err) = run() {
        eprintln!("terminism: {err}");
        std::process::exit(1);
    }
}

// composition order is fixed by spec 30 section 1
fn run() -> Result<(), ShellError> {
    let home = std::env::current_exe()?.with_file_name(".terminism");
    let plugins_dir = home.join("plugins");
    let i18n_dir = home.join("i18n");
    let theme_dir = home.join("theme");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&plugins_dir)?;
    std::fs::create_dir_all(&i18n_dir)?;
    std::fs::create_dir_all(&theme_dir)?;
    let log_store = LogStore::default();
    let (config_store, parse_err) = ConfigStore::load(&home.join("config.toml"));
    let config_rc = Rc::new(RefCell::new(config_store));
    if let Some(err) = &parse_err {
        Logger::new("kernel", log_store.clone()).log(err);
    }
    let data_rc = Rc::new(RefCell::new(DataStore(HashMap::new())));
    let registry_rc = Rc::new(RefCell::new(PluginRegistry::default()));
    let theme: terminism_systemui::Theme =
        std::rc::Rc::new(std::cell::RefCell::new(ThemeState::new(load_theme(
            &home.join("theme"),
            &Logger::new("kernel", log_store.clone()),
        ))));
    let i18n: terminism_kernel::host::I18nStore = Rc::new(RefCell::new(HashMap::new()));
    let gateway = Gateway::new(
        log_store.clone(),
        config_rc.clone(),
        registry_rc.clone(),
        theme.clone(),
        i18n.clone(),
    );
    let mut host = PluginHost::new(log_store.clone(), data_rc, config_rc.clone(), registry_rc);
    host.register_native(Box::new(Init));
    host.register_native(Box::new(Manager));
    host.register_native(Box::new(Account::new()));
    host.register_native(Box::new(Auth::new()));
    host.register_native(Box::new(Instances::new()));
    host.register_native(Box::new(Launcher::new(home.clone())));
    host.register_native(Box::new(UIManager::new(log_store.clone())));
    host.register_native(Box::new(LoggerPlugin {
        log_store: log_store.clone(),
    }));
    host.register_native(Box::new(Configuration));
    host.register_native(Box::new(Cli::new(std::env::args().skip(1).collect())));
    host.register_native(Box::new(I18n {
        i18n_dir,
        plugins_dir: plugins_dir.clone(),
        store: i18n.clone(),
    }));
    host.register_native(Box::new(StylesManager::new(theme)));
    host.register_native(Box::new(SystemPages::new()));
    host.register_native(Box::new(Loader {
        plugins_dir: plugins_dir.clone(),
        seen: Default::default(),
    }));
    host.set_ui(Gateway::ui_api(&gateway));
    host.boot()?;
    let result = (|| {
        let (mut terminal, mut guard) = take_terminal()?;
        let result = main_loop(&mut terminal, &gateway, &mut host);
        let restored = guard.restore();
        result.and(restored)
    })();
    host.shutdown();
    host.drain();
    let flushed = config_rc.borrow_mut().flush();
    result?;
    flushed?;
    Ok(())
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn restore(&mut self) -> std::io::Result<()> {
        if !std::mem::take(&mut self.active) {
            return Ok(());
        }
        restore_terminal()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(err) = self.restore() {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "terminal restore failed: {err}");
        }
    }
}

type ShellTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

fn take_terminal() -> std::io::Result<(ShellTerminal, TerminalGuard)> {
    enable_raw_mode()?;
    let guard = TerminalGuard { active: true };
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Ok((
        Terminal::new(CrosstermBackend::new(std::io::stdout()))?,
        guard,
    ))
}

fn restore_terminal() -> std::io::Result<()> {
    let raw = disable_raw_mode();
    let screen = execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    raw.and(screen)
}

fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    gateway: &Rc<RefCell<Gateway>>,
    host: &mut PluginHost,
) -> std::io::Result<()> {
    let mut next_tick = Instant::now() + Duration::from_secs(2);
    loop {
        if Instant::now() >= next_tick {
            host.tick();
            next_tick = Instant::now() + Duration::from_secs(2);
        }
        host.drain();
        let dirty = gateway.borrow_mut().take_dirty();
        if dirty {
            terminal.draw(|f| gateway.borrow_mut().draw(f))?;
            let pending = gateway.borrow_mut().take_pending();
            for d in pending {
                host.deliver(&d.target, &d.event, d.payload);
            }
            host.drain();
        }
        let mut timeout = next_tick.saturating_duration_since(Instant::now());
        if let Some(frame_delay) = gateway.borrow().next_frame_delay() {
            timeout = timeout.min(frame_delay);
        }
        if crossterm::event::poll(timeout)? {
            let ev = crossterm::event::read()?;
            if let Event::Key(k) = &ev
                && k.kind == KeyEventKind::Press
            {
                let plain_q = k.code == KeyCode::Char('q')
                    && (k.modifiers == KeyModifiers::NONE || k.modifiers == KeyModifiers::SHIFT);
                let ctrl_c =
                    k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                if plain_q || ctrl_c {
                    break;
                }
            }
            let deliveries = gateway.borrow_mut().handle_event(&ev);
            for d in deliveries {
                host.deliver(&d.target, &d.event, d.payload);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod boot_probe {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind};

    // the demo script is generated here: no example plugin ships in the repo
    fn copy_hello(dst: &std::path::Path) {
        let dir = dst.join("hello-terminism");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.toml"),
            r#"id = "hello-terminism"
name = "Hello"
version = "0.0.1"
description = "Demo plugin with a page, a counter and split panes"
provides = []
subscribe = ["plugin.loaded"]
ui_grant = []
data_grant = []
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("main.rhai"),
            r#"fn init() {}

fn on_event(name, payload) {
    if name == "plugin.loaded" && payload.id == "hello-terminism" {
        ui.create("/hello", "Page", #{title: "Hello"});
        ui.create("/hello/count", "Label", #{text: "clicks: 0"});
        ui.create("/hello/btn", "Button", #{text: "ping"});
        ui.create("/hello/split", "Split", #{direction: "row"});
        ui.create("/hello/split/a", "Label", #{text: "pane A"});
        ui.create("/hello/split/b", "Label", #{text: "pane B"});
    }
    if name == "ui.activate" && payload.path == "/hello/btn" {
        let n = data.get("clicks");
        let n = if n == () { 0 } else { n + 1 };
        data.set("clicks", n);
        ui.patch("/hello/count", #{text: "clicks: " + n});
        events.publish("hello.ping", #{count: n});
    }
}
"#,
        )
        .unwrap();
    }

    #[test]
    fn boot_builds_focusable_tree() {
        let tmp = std::env::temp_dir().join(format!("terminism-probe-{}", std::process::id()));
        let plugins_dir = tmp.join("plugins");
        copy_hello(&plugins_dir);
        // the shipped wasm guest, mirroring install-plugins
        {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let dst = plugins_dir.join("wasm-hello");
            std::fs::create_dir_all(&dst).unwrap();
            for f in ["manifest.toml", "plugin.wat"] {
                std::fs::copy(root.join("plugins/wasm-hello").join(f), dst.join(f)).unwrap();
            }
        }
        // the SDK-built rust guest, mirroring install-plugins
        {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let dst = plugins_dir.join("wasm-hello-rs");
            std::fs::create_dir_all(&dst).unwrap();
            for f in ["manifest.toml", "plugin.wasm"] {
                std::fs::copy(root.join("plugins/wasm-hello-rs").join(f), dst.join(f)).unwrap();
            }
        }
        // the shipped zh-cn language pack, mirroring install-plugins
        {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let dst = plugins_dir.join("zh-cn");
            std::fs::create_dir_all(dst.join("i18n")).unwrap();
            for f in ["manifest.toml", "main.rhai"] {
                std::fs::copy(root.join("plugins/zh-cn").join(f), dst.join(f)).unwrap();
            }
            std::fs::copy(
                root.join("plugins/zh-cn/i18n/zh.json"),
                dst.join("i18n/zh.json"),
            )
            .unwrap();
        }
        // a script theme that activates fine and then crashes on an event
        let theme_dir = plugins_dir.join("crash-theme");
        std::fs::create_dir_all(&theme_dir).unwrap();
        std::fs::write(
            theme_dir.join("manifest.toml"),
            "id = \"crash-theme\"\nprovides = [\"styles.theme\"]\n",
        )
        .unwrap();
        std::fs::write(
            theme_dir.join("theme.tmss"),
            "/home/lock { color: cyan; }\n",
        )
        .unwrap();
        std::fs::write(
            theme_dir.join("main.rhai"),
            "fn init() { } fn on_event(n, d) { if n == \"boom\" { throw \"theme died\"; } }",
        )
        .unwrap();
        std::fs::write(
            tmp.join("config.toml"),
            "[styles]\ntheme = \"crash-theme\"\n",
        )
        .unwrap();

        let log_store = LogStore::default();
        let (config_store, parse_err) = ConfigStore::load(&tmp.join("config.toml"));
        if let Some(err) = &parse_err {
            Logger::new("kernel", log_store.clone()).log(err);
        }
        let config_rc = Rc::new(RefCell::new(config_store));
        let data_rc = Rc::new(RefCell::new(DataStore(HashMap::new())));
        let registry_rc = Rc::new(RefCell::new(PluginRegistry::default()));
        let theme = terminism_systemui::Theme::default();
        let i18n: terminism_kernel::host::I18nStore = Rc::new(RefCell::new(HashMap::new()));
        // stage the repository catalogs where the i18n plugin expects them
        {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let install_dir = tmp.join("i18n");
            std::fs::create_dir_all(&install_dir).unwrap();
            std::fs::copy(root.join("i18n/en.json"), install_dir.join("en.json")).unwrap();
        }
        let gateway = Gateway::new(
            log_store.clone(),
            config_rc.clone(),
            registry_rc.clone(),
            theme.clone(),
            i18n.clone(),
        );
        let mut host = PluginHost::new(log_store.clone(), data_rc, config_rc.clone(), registry_rc);
        host.register_native(Box::new(Init));
        host.register_native(Box::new(Manager));
        host.register_native(Box::new(Account::new()));
        host.register_native(Box::new(Auth::new()));
        host.register_native(Box::new(Instances::new()));
        host.register_native(Box::new(UIManager::new(log_store.clone())));
        host.register_native(Box::new(LoggerPlugin {
            log_store: log_store.clone(),
        }));
        host.register_native(Box::new(Configuration));
        host.register_native(Box::new(Cli::new(vec![
            "servers".to_string(),
            "add".to_string(),
            "mybox".to_string(),
            "--debug".to_string(),
        ])));
        host.register_native(Box::new(I18n {
            i18n_dir: tmp.join("i18n"),
            plugins_dir: plugins_dir.clone(),
            store: i18n.clone(),
        }));
        host.register_native(Box::new(StylesManager::new(theme)));
        host.register_native(Box::new(SystemPages::new()));
        host.register_native(Box::new(Loader {
            plugins_dir: plugins_dir.clone(),
            seen: Default::default(),
        }));
        host.set_ui(Gateway::ui_api(&gateway));
        let boot_start = Instant::now();
        host.boot().unwrap();
        host.tick();
        host.drain();
        println!("boot_us={}", boot_start.elapsed().as_micros());
        let infos = host.plugin_infos();
        assert!(
            infos
                .iter()
                .find(|p| p.id == "system-pages")
                .unwrap()
                .system
        );
        assert_eq!(
            infos
                .iter()
                .find(|p| p.id == "loader")
                .unwrap()
                .parent
                .as_deref(),
            Some("manager")
        );
        assert!(!infos.iter().find(|p| p.id == "logger").unwrap().system);
        assert_eq!(
            infos
                .iter()
                .find(|p| p.id == "wasm-hello-rs")
                .unwrap()
                .state,
            "active"
        );

        let log = log_store.borrow().join("\n");
        println!("--- log ---\n{log}");

        // instance registry: a command from any UI lands in config and echoes back
        host.deliver(
            "instances",
            "instances.command",
            serde_json::json!({"action": "create", "name": "demo"}),
        );
        host.drain();
        let list = config_rc
            .borrow()
            .get("instances", "list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(list.len(), 1, "instance must be registered");
        assert_eq!(list[0]["name"], serde_json::json!("demo"));
        for name in ["first", "second"] {
            host.publish(
                "instances.command",
                serde_json::json!({"action":"create", "name":name}),
            );
        }
        host.drain();
        assert_eq!(
            config_rc
                .borrow()
                .get("instances", "list")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
        // the wasm loader picked up the wasm-hello guest
        assert!(log.contains("wasm loaded: wasm-hello"), "{log}");
        // the SDK-built guest loads and runs its init through the same ABI
        assert!(log.contains("wasm loaded: wasm-hello-rs"), "{log}");
        assert!(
            log.contains("plugin loaded"),
            "sdk guest init missing:
{log}"
        );
        // startup arguments: parsed once, written to the cli config section
        assert_eq!(
            config_rc
                .borrow()
                .get("cli", "command")
                .and_then(|v| v.as_str()),
            Some("servers"),
            "--debug must not swallow the following positional"
        );
        assert!(log.contains("cli parsed"), "{log}");
        // SystemPages owns the home widgets.
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("T_"), "cube icon missing:\n{text}");
            assert!(text.contains("version"), "info table missing:\n{text}");
            assert!(text.contains("UTC"), "live clock missing:\n{text}");
            assert!(
                text.contains("instances coming soon"),
                "empty card missing:\n{text}"
            );
            assert!(
                text.contains("Not signed in"),
                "account row missing:\n{text}"
            );
            assert!(gateway.borrow_mut().clock_tick() || text.contains("UTC"));
        }
        // the configured script theme activated at load, then crashes: fallback fires
        assert!(log.contains("theme active: crash-theme"), "{log}");
        host.deliver("crash-theme", "boom", serde_json::json!({}));
        host.drain();
        let log = log_store.borrow().join("\n");
        assert!(log.contains("theme fallback: crash-theme"), "{log}");

        // settings chain: config.set -> configuration gateway -> store write
        // -> config.updated -> styles-manager switches back to the factory look
        host.deliver(
            "configuration",
            "config.set",
            serde_json::json!({"section": "styles", "key": "theme", "value": "default-theme"}),
        );
        host.drain();
        let log = log_store.borrow().join("\n");
        assert!(
            log.contains("theme active: default-theme"),
            "config.set must re-activate the default theme:\n{log}"
        );
        assert_eq!(
            config_rc
                .borrow()
                .get("styles", "theme")
                .and_then(|v| v.as_str()),
            Some("default-theme"),
            "config.set must write through to the store"
        );

        // Tab is page-only now; drive the UI like a mouse user instead:
        // click the home nav button, then click a sidebar entry on /plugins
        let dispatch = |gw: &Rc<RefCell<Gateway>>, host: &mut PluginHost, ev: &Event| -> usize {
            let out = gw.borrow_mut().handle_event(ev);
            let hits = out.len();
            for d in out {
                host.deliver(&d.target, &d.event, d.payload);
            }
            hits
        };
        let find_text = |host: &mut PluginHost, needle: &str| -> Option<(u16, u16)> {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let pending = gateway.borrow_mut().take_pending();
            for d in pending {
                host.deliver(&d.target, &d.event, d.payload);
            }
            let buf = terminal.backend().buffer();
            for (row, line) in buf.content.chunks(80).enumerate() {
                let text: String = line.iter().map(|c| c.symbol()).collect();
                // byte offset -> character column, multibyte glyphs shift it
                if let Some(byte) = text.find(needle) {
                    let col = text[..byte].chars().count();
                    return Some((col as u16, row as u16));
                }
            }
            None
        };
        // no nav buttons on home v2: jump pages via the status bar tab strip
        let (account_x, account_y) = find_text(&mut host, "☺").expect("account tab icon rendered");
        let _ = dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: account_x,
                row: account_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains("No accounts yet"),
                "account page missing:\n{text}"
            );
            assert!(
                text.contains("Add offline account"),
                "account actions missing:\n{text}"
            );
            assert!(
                text.contains("Sign in with Microsoft"),
                "online login action missing:\n{text}"
            );
            assert!(
                text.contains("Sign in with third-party server"),
                "third-party login action missing:\n{text}"
            );
        }
        let (nav_x, nav_y) = find_text(&mut host, "▤").expect("plugins tab icon rendered");
        // settings page rides with the theme: theme stack + locale editor
        let (set_x, set_y) = find_text(&mut host, "⚙").expect("settings tab icon rendered");
        let _ = dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: set_x,
                row: set_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains("◐ Appearance"),
                "selected category missing:\n{text}"
            );
            assert!(text.contains("General"), "other category missing:\n{text}");
            assert!(text.contains("Theme"), "theme row missing:\n{text}");
            assert!(text.contains("Layout lock"), "lock row missing:\n{text}");
            assert!(text.contains("[ ] off"), "toggle glyph missing:\n{text}");
            assert!(
                text.contains("crash-theme >"),
                "select value missing:\n{text}"
            );
        }
        // click the theme row: a picker popup lists every installed theme
        let (theme_x, theme_y) = find_text(&mut host, "Theme").expect("theme row rendered");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            let _ = dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: theme_x + 2,
                    row: theme_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains("* default-theme"),
                "inline theme options missing:\n{text}"
            );
        }
        // switch to General: the Locale row appears (one stacked row)
        let (gen_x, gen_y) = find_text(&mut host, "General").expect("general category rendered");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            let _ = dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: gen_x + 2,
                    row: gen_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Locale"), "locale row missing:\n{text}");
        }
        // the locale row expands inline with the staged language packs
        let (loc_x, loc_y) = find_text(&mut host, "Locale").expect("locale row");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            let _ = dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: loc_x + 2,
                    row: loc_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains("* en"),
                "current locale option missing:\n{text}"
            );
            assert!(text.contains("zh"), "zh locale option missing:\n{text}");
        }
        // back to plugins for the sidebar walk below
        let nav_hits = dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: nav_x,
                row: nav_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                column: nav_x,
                row: nav_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        // the sidebar is 20 columns wide: the entry text is truncated there
        let (entry_x, entry_y) = find_text(&mut host, "Config").expect("sidebar entry rendered");
        let entry_hits = dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: entry_x + 2,
                row: entry_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        dispatch(
            &gateway,
            &mut host,
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                column: entry_x + 2,
                row: entry_y,
                modifiers: KeyModifiers::NONE,
            }),
        );
        let (logger_x, logger_y) = find_text(&mut host, "Logger").expect("logger entry");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: logger_x + 2,
                    row: logger_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        assert!(
            find_text(&mut host, "[ disable ]").is_none(),
            "native logger cannot be disabled"
        );
        assert!(
            find_text(&mut host, "[ reload ]").is_none(),
            "native logger cannot be reloaded"
        );
        // open the disable confirm on a user plugin, then let ticks run:
        // the page must stay put with the popup visible
        let (hello_x, hello_y) = find_text(&mut host, "Hello").expect("user entry rendered");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: hello_x + 2,
                    row: hello_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        let (off_x, off_y) = find_text(&mut host, "[ disable ]").expect("disable button");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: off_x + 2,
                    row: off_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        host.tick();
        host.drain();
        {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Confirm"), "popup missing:\n{text}");
            assert!(
                text.contains("System"),
                "page jumped away while popup open:\n{text}"
            );
        }
        // closing the confirm popup restores the full page layout
        let (no_x, no_y) = find_text(&mut host, "[ no ]").expect("popup no button");
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            dispatch(
                &gateway,
                &mut host,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: no_x + 2,
                    row: no_y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        host.tick();
        host.drain();
        let focus_moved = nav_hits + entry_hits > 0;
        // hover chain: move the mouse over the sidebar, walk with an arrow;
        // the manager's ui.key handler calls set_hover from inside delivery
        let moved = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 2,
            row: entry_y,
            modifiers: KeyModifiers::NONE,
        });
        dispatch(&gateway, &mut host, &moved);
        let down = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Down,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        dispatch(&gateway, &mut host, &down);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(focus_moved, "plugin page interactions must deliver events");

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
        let pending = gateway.borrow_mut().take_pending();
        for d in pending {
            host.deliver(&d.target, &d.event, d.payload);
        }
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        // 80 columns puts the detail pane in the Split tier
        assert!(text.contains("v0.0.1"), "detail pane missing:\n{text}");
        assert!(text.contains("State"), "info grid missing:\n{text}");
        assert!(
            text.contains("≡ Log"),
            "decorated log title missing:\n{text}"
        );
        // clicking an instance row must keep /instances (no fallback to /home)
        {
            let ui = Gateway::ui_api(&gateway);
            assert!((ui.set_focus)("system-pages", "/instances"));
            let (row_x, row_y) = find_text(&mut host, "demo").expect("instance row rendered");
            for kind in [
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            ] {
                dispatch(
                    &gateway,
                    &mut host,
                    &Event::Mouse(crossterm::event::MouseEvent {
                        kind,
                        column: row_x + 2,
                        row: row_y,
                        modifiers: KeyModifiers::NONE,
                    }),
                );
            }
            assert!(
                find_text(&mut host, "Launch").is_some(),
                "instance page must stay visible after clicking an instance row"
            );
        }
        // leaving /settings must not let layout updates steal the page back
        {
            host.deliver(
                "system-pages",
                "ui.focus",
                serde_json::json!({"path": "/settings/split/tree/c-settings.appearance"}),
            );
            host.drain();
            let ui = Gateway::ui_api(&gateway);
            assert!((ui.set_focus)("system-pages", "/instances"));
            host.deliver(
                "system-pages",
                "ui.blur",
                serde_json::json!({"path": "/settings/split/tree/c-settings.appearance"}),
            );
            config_rc.borrow_mut().set(
                "system-ui",
                "layout",
                serde_json::json!({"locked": false, "order": {}, "flex": {}}),
            );
            host.drain();
            let draw = |host: &mut PluginHost| -> String {
                for step in 0..2 {
                    let backend = ratatui::backend::TestBackend::new(80, 24);
                    let mut terminal = ratatui::Terminal::new(backend).unwrap();
                    terminal.draw(|f| gateway.borrow_mut().draw(f)).unwrap();
                    let pending = gateway.borrow_mut().take_pending();
                    for d in pending {
                        host.deliver(&d.target, &d.event, d.payload);
                    }
                    if step == 1 {
                        return terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .map(|c| c.symbol())
                            .collect();
                    }
                }
                String::new()
            };
            let text = draw(&mut host);
            assert!(
                text.contains("Launch") && !text.contains("Appearance"),
                "settings must not steal the page on layout updates:\n{text}"
            );
        }
        let ui = Gateway::ui_api(&gateway);
        assert!((ui.set_focus)("hello-terminism", "/hello"));
        host.disable("hello-terminism").unwrap();
        host.drain();
        assert!(!(ui.set_focus)("hello-terminism", "/hello"));
    }
}
