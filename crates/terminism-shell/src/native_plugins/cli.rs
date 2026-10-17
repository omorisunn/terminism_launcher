use std::collections::BTreeMap;

use serde_json::json;
use terminism_kernel::host::{CliTable, HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

// Parses `terminism <command> [subcommand] [positionals...] [--key value | --flag]`
// once at boot and exposes it three ways: kernel table, cli.ready event, and
// a [cli] config section. --locale / --theme override config through the gateway.
pub struct Cli {
    args: Vec<String>,
}

impl Cli {
    pub fn new(args: Vec<String>) -> Self {
        Self { args }
    }
}

// subcommand form: bare words fill command/subcommand/positionals, --key takes
// the next word unless it starts another flag or ends the args
pub(crate) fn parse(args: &[String]) -> CliTable {
    let mut table = CliTable::default();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(key) = arg.strip_prefix("--") {
            let value = args
                .get(i + 1)
                .filter(|next| !next.starts_with("--"))
                .cloned()
                .unwrap_or_default();
            if value.is_empty() {
                table.options.insert(key.to_string(), String::new());
            } else {
                table.options.insert(key.to_string(), value);
                i += 1;
            }
        } else {
            words.push(arg.clone());
        }
        i += 1;
    }
    if !words.is_empty() {
        table.command = words.remove(0);
    }
    if !words.is_empty() {
        table.subcommand = Some(words.remove(0));
    }
    table.positionals = words;
    table
}

impl NativePlugin for Cli {
    fn manifest(&self) -> Manifest {
        manifest(
            "cli",
            "Cli",
            &[],
            "Parses startup arguments and exposes them to plugins",
        )
    }

    fn init(&mut self, host: &mut HostHandle, logger: &Logger) {
        let table = parse(&self.args);
        let options: BTreeMap<String, Json> = table
            .options
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        logger.log(&format!(
            "cli parsed: command={:?} options={:?}",
            table.command,
            table.options.keys().collect::<Vec<_>>()
        ));
        host.publish(
            "cli.ready",
            json!({
                "command": table.command,
                "subcommand": table.subcommand,
                "positionals": table.positionals,
                "options": options,
            }),
        );
        // boot-time writes are synchronous: later clients (i18n) read the
        // store during their own init, before any queued event is drained
        let store = host.config();
        {
            let mut cfg = store.borrow_mut();
            if !table.command.is_empty() {
                cfg.set("cli", "command", json!(table.command));
            }
            if let Some(sub) = &table.subcommand {
                cfg.set("cli", "subcommand", json!(sub));
            }
            if let Some(locale) = table.options.get("locale").filter(|v| !v.is_empty()) {
                cfg.set("i18n", "locale", json!(locale));
            }
            if let Some(theme) = table.options.get("theme").filter(|v| !v.is_empty()) {
                cfg.set("styles", "theme", json!(theme));
            }
        }
        // config.updated lets managers react (styles-manager repaints, etc.)
        host.set_cli(table);
    }

    fn on_event(&mut self, _name: &str, _payload: &Json, _host: &mut HostHandle, _logger: &Logger) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_subcommands_flags_and_values() {
        let t = parse(&args(&[
            "servers", "add", "mybox", "--host", "1.2.3.4", "--debug",
        ]));
        assert_eq!(t.command, "servers");
        assert_eq!(t.subcommand.as_deref(), Some("add"));
        assert_eq!(t.positionals, ["mybox"]);
        assert_eq!(t.options.get("host").map(String::as_str), Some("1.2.3.4"));
        assert_eq!(t.options.get("debug").map(String::as_str), Some(""));
        // a flag directly before another flag takes no value
        let t = parse(&args(&["--debug", "--locale", "zh"]));
        assert_eq!(t.options.get("debug").map(String::as_str), Some(""));
        assert_eq!(t.options.get("locale").map(String::as_str), Some("zh"));
        assert!(parse(&[]).command.is_empty());
    }
}
