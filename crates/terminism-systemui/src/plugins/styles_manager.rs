use crate::{Rule, Theme, parse_tmss};
use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

// the factory look, applied whenever no theme plugin is active
pub(crate) const DEFAULT_TMSS: &str = "\
# terminism default theme
Button { color: white; }
Button { hint: \"Enter activate\"; }
Item { hint: \"Enter focus\"; }
/home { icon: \"⌂\"; }
/plugins { icon: \"▤\"; }
/log { icon: \"≡\"; }
/settings { icon: \"⚙\"; }
/instances { icon: \"⚘\"; }
/account { icon: \"☺\"; }
/home/icon { color: white; }
/home/info/version { color: \"#9AA0A6\"; }
/home/info/theme { color: \"#9AA0A6\"; }
/home/info/plugins { color: \"#9AA0A6\"; }
/home/info/uptime { color: \"#9AA0A6\"; }
/home/info/account { color: \"#9AA0A6\"; }
/home/info/clock { color: \"#9AA0A6\"; }
/home/empty/title { color: \"#9AA0A6\"; }
/plugins/split/detail/layout/div-a { fill: \"─\"; color: \"#9AA0A6\"; }
/plugins/split/detail/layout/div-b { fill: \"─\"; color: \"#9AA0A6\"; }
# dim border ring for every panel, accent reserved for live values
Section { color: \"#6B7280\"; }
";

pub struct StylesManager {
    theme: Theme,
    // rules loaded from .terminism/theme at boot; every activation appends to these
    base: Vec<Rule>,
    configured: String,
    active: String,
}

impl StylesManager {
    pub fn new(theme: Theme) -> Self {
        let base = theme.borrow().rules().to_vec();
        Self {
            theme,
            base,
            configured: "default-theme".to_string(),
            active: "default-theme".to_string(),
        }
    }

    // base rules first, theme rules after: ties go to the theme
    fn apply(&self, extra: &[Rule]) {
        let mut rules = self.base.clone();
        rules.extend_from_slice(extra);
        self.theme.borrow_mut().replace(rules);
    }

    fn apply_default(&self, logger: &Logger) {
        let extra = parse_tmss(DEFAULT_TMSS).unwrap_or_default();
        self.apply(&extra);
        logger.log("theme active: default-theme");
    }

    // a script theme carries its rules in theme.tmss next to its manifest
    fn activate_script_theme(&mut self, id: &str, host: &HostHandle, logger: &Logger) {
        let fallback = |this: &Self, logger: &Logger| {
            this.apply_default(logger);
            logger.log(&format!("theme fallback: {id}"));
        };
        let Some(dir) = host.plugin_dir(id) else {
            fallback(self, logger);
            self.active = "default-theme".to_string();
            return;
        };
        match std::fs::read_to_string(dir.join("theme.tmss"))
            .map_err(|e| e.to_string())
            .and_then(|text| parse_tmss(&text).map_err(|e| e.to_string()))
        {
            Ok(extra) => {
                self.apply(&extra);
                self.active = id.to_string();
                logger.log(&format!("theme active: {id}"));
            }
            Err(e) => {
                logger.log(&format!("theme load failed: {id}: {e}"));
                fallback(self, logger);
                self.active = "default-theme".to_string();
            }
        }
    }
}

impl NativePlugin for StylesManager {
    fn manifest(&self) -> Manifest {
        manifest(
            "styles-manager",
            "StylesManager",
            &[
                "plugin.loaded",
                "plugin.disabled",
                "plugin.crashed",
                "plugin.failed",
                "config.updated",
            ],
            "Activates theme plugins and falls back to the default theme",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let configured = host
            .config()
            .borrow()
            .get("styles", "theme")
            .and_then(Json::as_str)
            .unwrap_or("default-theme")
            .to_string();
        self.configured = configured.clone();
        self.apply_default(logger);
        self.active = "default-theme".to_string();
        if configured != "default-theme" {
            logger.log(&format!("theme pending: {configured}"));
        }
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        if name == "config.updated" {
            let (section, key) = (
                payload.get("section").and_then(Json::as_str),
                payload.get("key").and_then(Json::as_str),
            );
            if section == Some("styles")
                && key == Some("theme")
                && let Some(theme) = payload.get("value").and_then(Json::as_str)
                && theme != self.active
            {
                self.switch_theme(theme, host, logger);
            }
            return;
        }
        let Some(id) = payload.get("id").and_then(Json::as_str) else {
            return;
        };
        match name {
            "plugin.loaded" if id == self.configured && id != "default-theme" => {
                self.activate_script_theme(id, host, logger);
            }
            "plugin.disabled" | "plugin.crashed" | "plugin.failed"
                if id == self.active && id != "default-theme" =>
            {
                self.apply_default(logger);
                self.active = "default-theme".to_string();
                logger.log(&format!("theme fallback: {id}"));
            }
            _ => {}
        }
    }
}

impl StylesManager {
    // settings-page theme switch: default applies at once, a loaded theme
    // activates immediately, an unloaded one waits for plugin.loaded
    fn switch_theme(&mut self, theme: &str, host: &mut HostHandle, logger: &Logger) {
        self.configured = theme.to_string();
        if theme == "default-theme" {
            self.apply_default(logger);
            self.active = "default-theme".to_string();
            return;
        }
        let loaded = host.plugins().iter().any(|i| i.id == theme);
        if loaded {
            self.activate_script_theme(theme, host, logger);
        } else {
            logger.log(&format!("theme pending: {theme}"));
        }
    }
}

// reads *.tmss from the theme dir in filename order; later files override
// earlier ones at equal specificity; a broken file is skipped with a log line
pub fn load_theme(dir: &std::path::Path, logger: &Logger) -> Vec<Rule> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tmss"))
        .collect();
    files.sort();
    let mut rules = Vec::new();
    for file in files {
        match std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_tmss(&text).map_err(|e| e.to_string()))
        {
            Ok(mut parsed) => rules.append(&mut parsed),
            Err(e) => logger.log(&format!("theme skipped {}: {e}", file.display())),
        }
    }
    rules
}
