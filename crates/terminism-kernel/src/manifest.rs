use serde::Deserialize;

pub(crate) const RESERVED_PREFIXES: [&str; 3] = ["plugin.", "ui.", "shell."];

#[derive(Debug, Clone)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub provides: Vec<String>,
    pub subscribe: Vec<String>,
    pub ui_grant: Vec<String>,
    pub data_grant: Vec<String>,
    // startup option keys this plugin may read through cli.get/cli.flag
    pub cli_grant: Vec<String>,
    // runtime kind: "rhai" (default) or "wasm"
    pub kind: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("toml parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid plugin id: {0}")]
    InvalidId(String),
    #[error("invalid plugin kind: {0}")]
    InvalidKind(String),
}

#[derive(Deserialize)]
struct RawManifest {
    id: String,
    name: Option<String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    subscribe: Vec<String>,
    #[serde(default)]
    ui_grant: Vec<String>,
    #[serde(default)]
    data_grant: Vec<String>,
    #[serde(default)]
    cli_grant: Vec<String>,
    #[serde(default)]
    kind: Option<String>,
}

pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
    let raw: RawManifest = toml::from_str(text)?;
    if !id_valid(&raw.id) {
        return Err(ManifestError::InvalidId(raw.id));
    }
    let name = raw.name.unwrap_or_else(|| raw.id.clone());
    Ok(Manifest {
        id: raw.id,
        name,
        version: raw.version,
        description: raw.description,
        provides: raw.provides,
        subscribe: raw.subscribe,
        ui_grant: raw.ui_grant,
        data_grant: raw.data_grant,
        cli_grant: raw.cli_grant,
        kind: {
            let kind = raw.kind.unwrap_or_else(|| "rhai".to_string());
            if kind != "rhai" && kind != "wasm" {
                return Err(ManifestError::InvalidKind(kind));
            }
            kind
        },
    })
}

pub fn native(id: &str, name: &str, subscribe: &[&str], description: &str) -> Manifest {
    Manifest {
        id: id.to_string(),
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: description.to_string(),
        provides: Vec::new(),
        subscribe: subscribe.iter().map(|s| (*s).to_string()).collect(),
        ui_grant: Vec::new(),
        data_grant: Vec::new(),
        cli_grant: Vec::new(),
        kind: "rhai".to_string(),
    }
}

fn id_valid(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parse_ok() {
        let text = "id = \"hello-terminism\"\nname = \"Hello\"\nversion = \"0.0.1\"\ndescription = \"demo\"\nprovides = []\nsubscribe = [\"timer.tick\"]\nui_grant = []\ndata_grant = []\n";
        let m = parse(text).unwrap();
        assert_eq!(m.id, "hello-terminism");
        assert_eq!(m.name, "Hello");
        assert_eq!(m.version, "0.0.1");
        assert_eq!(m.description, "demo");
        assert_eq!(m.provides, Vec::<String>::new());
        assert_eq!(m.subscribe, ["timer.tick"]);
        assert_eq!(m.ui_grant, Vec::<String>::new());
        assert_eq!(m.data_grant, Vec::<String>::new());
        let minimal = parse("id = \"minimal\"\n").unwrap();
        assert_eq!(minimal.name, "minimal");
        assert_eq!(minimal.version, "");
        assert_eq!(minimal.description, "");
        assert!(minimal.provides.is_empty());
        assert!(minimal.subscribe.is_empty());
    }

    #[test]
    fn manifest_bad_id() {
        for id in ["", "-lead", "trail-", "Upper", "with space", "under_score"] {
            let text = format!("id = \"{id}\"\n");
            assert!(
                matches!(parse(&text), Err(ManifestError::InvalidId(_))),
                "id: {id}"
            );
        }
    }

    #[test]
    fn manifest_builtin_subscribe_ok() {
        for event in ["plugin.loaded", "ui.focus", "shell.quit"] {
            let text = format!("id = \"x\"\nsubscribe = [\"{event}\"]\n");
            assert!(parse(&text).is_ok(), "event: {event}");
        }
    }
}
