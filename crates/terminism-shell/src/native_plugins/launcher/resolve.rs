//! Version JSON resolution and launch plan assembly: pure functions, unit tested.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use terminism_kernel::host::ConfigStore;

use super::settings::{DEFAULT_PLAYER_NAME, ResolvedSettings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError {
    pub kind: &'static str,
    pub message: String,
}

impl LaunchError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.kind, self.message)
    }
}

/* ---------------- version json model ---------------- */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VersionJson {
    // Minecraft spells this key "inheritsFrom", not the camelCase form
    #[serde(rename = "inheritsFrom")]
    pub inherited_from: Option<String>,
    pub main_class: Option<String>,
    pub assets: Option<String>,
    pub asset_index: Option<AssetIndex>,
    pub libraries: Option<Vec<Library>>,
    pub java_version: Option<JavaVersion>,
    pub arguments: Option<Arguments>,
    pub minecraft_arguments: Option<String>,
    pub jar: Option<String>,
    #[serde(rename = "type")]
    pub version_type: Option<String>,
    pub logging: Option<Logging>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndex {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    pub major_version: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Arguments {
    pub game: Option<Vec<Argument>>,
    pub jvm: Option<Vec<Argument>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Argument {
    Plain(String),
    Complex {
        #[serde(default)]
        rules: Vec<Rule>,
        value: StringList,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringList {
    One(String),
    Many(Vec<String>),
}

impl StringList {
    fn append_to(&self, out: &mut Vec<String>) {
        match self {
            StringList::One(item) => out.push(item.clone()),
            StringList::Many(items) => out.extend(items.iter().cloned()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub action: String,
    pub os: Option<OsRule>,
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OsRule {
    pub name: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub name: String,
    pub downloads: Option<Downloads>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub natives: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Downloads {
    pub artifact: Option<Artifact>,
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Logging {
    pub client: Option<ClientLog>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientLog {
    pub argument: Option<String>,
    pub file: LogFile,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogFile {
    pub id: String,
}

/* ---------------- rules ---------------- */

pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub fn os_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn rule_matches(rule: &Rule) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name
            && name != os_name()
        {
            return false;
        }
        if let Some(arch) = &os.arch
            && arch != os_arch()
        {
            return false;
        }
        // os.version needs a regex engine; unsupported version rules never match
        if os.version.is_some() {
            return false;
        }
    }
    if let Some(features) = &rule.features
        && features.values().any(|wanted| *wanted)
    {
        // v1 exposes no optional features, so only rules expecting false match
        return false;
    }
    true
}

pub fn rules_allow(rules: &[Rule]) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            match rule.action.as_str() {
                "allow" => allowed = true,
                "disallow" => allowed = false,
                _ => {}
            }
        }
    }
    allowed
}

pub fn collect_args(args: &[Argument]) -> Vec<String> {
    let mut out = Vec::new();
    for argument in args {
        match argument {
            Argument::Plain(text) => out.push(text.clone()),
            Argument::Complex { rules, value } => {
                if rules_allow(rules) {
                    value.append_to(&mut out);
                }
            }
        }
    }
    out
}

/* ---------------- version loading and inheritance ---------------- */

pub fn version_json_path(root: &Path, version: &str) -> PathBuf {
    root.join("versions")
        .join(version)
        .join(format!("{version}.json"))
}

pub fn version_dir(root: &Path, version: &str) -> PathBuf {
    root.join("versions").join(version)
}

fn read_json(path: &Path) -> Result<VersionJson, LaunchError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        LaunchError::new("version_invalid", format!("{}: {error}", path.display()))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        LaunchError::new("version_invalid", format!("{}: {error}", path.display()))
    })
}

fn merge_arguments(parent: Option<Arguments>, child: Option<Arguments>) -> Option<Arguments> {
    match (parent, child) {
        (None, None) => None,
        (Some(parent), None) => Some(parent),
        (None, Some(child)) => Some(child),
        (Some(parent), Some(child)) => Some(Arguments {
            game: merge_parent_then_child(parent.game, child.game),
            jvm: merge_parent_then_child(parent.jvm, child.jvm),
        }),
    }
}

fn merge_parent_then_child<T>(parent: Option<Vec<T>>, child: Option<Vec<T>>) -> Option<Vec<T>> {
    match (parent, child) {
        (None, None) => None,
        (Some(parent), None) => Some(parent),
        (None, Some(child)) => Some(child),
        (Some(parent), Some(child)) => {
            let mut merged = parent;
            merged.extend(child);
            Some(merged)
        }
    }
}

fn merge_child_then_parent<T>(parent: Option<Vec<T>>, child: Option<Vec<T>>) -> Option<Vec<T>> {
    match (parent, child) {
        (None, None) => None,
        (Some(parent), None) => Some(parent),
        (None, Some(child)) => Some(child),
        (Some(parent), Some(child)) => {
            let mut merged = child;
            merged.extend(parent);
            Some(merged)
        }
    }
}

fn merge(parent: VersionJson, child: VersionJson) -> VersionJson {
    VersionJson {
        inherited_from: None,
        main_class: child.main_class.or(parent.main_class),
        assets: child.assets.or(parent.assets),
        asset_index: child.asset_index.or(parent.asset_index),
        libraries: merge_child_then_parent(parent.libraries, child.libraries),
        java_version: child.java_version.or(parent.java_version),
        arguments: merge_arguments(parent.arguments, child.arguments),
        minecraft_arguments: child.minecraft_arguments.or(parent.minecraft_arguments),
        jar: child.jar.or(parent.jar),
        version_type: child.version_type.or(parent.version_type),
        logging: child.logging.or(parent.logging),
    }
}

pub fn load_chain(root: &Path, version: &str) -> Result<Vec<(String, VersionJson)>, LaunchError> {
    let mut chain = Vec::new();
    let mut current = version.to_string();
    for _ in 0..8 {
        let path = version_json_path(root, &current);
        if !path.is_file() {
            let kind = if chain.is_empty() {
                "version_missing"
            } else {
                "inherit_missing"
            };
            return Err(LaunchError::new(
                kind,
                format!(
                    "{} 不存在（版本 {version} 的继承链缺少 {current}）",
                    path.display()
                ),
            ));
        }
        let json = read_json(&path)?;
        let next = json.inherited_from.clone();
        chain.push((current.clone(), json));
        match next {
            Some(parent) if !parent.is_empty() => current = parent,
            _ => return Ok(chain),
        }
    }
    Err(LaunchError::new(
        "inherit_cycle",
        format!("版本 {version} 的 inheritsFrom 链超过 8 层或存在环"),
    ))
}

fn merge_chain(chain: &[(String, VersionJson)]) -> VersionJson {
    let mut iter = chain.iter().rev();
    let mut merged = iter
        .next()
        .map(|(_, json)| json.clone())
        .unwrap_or_default();
    for (_, child) in iter {
        merged = merge(merged, child.clone());
    }
    merged
}

pub fn load_version(root: &Path, version: &str) -> Result<VersionJson, LaunchError> {
    Ok(merge_chain(&load_chain(root, version)?))
}

pub fn required_java(root: &Path, version: &str) -> Result<Option<u32>, LaunchError> {
    Ok(load_version(root, version)?
        .java_version
        .and_then(|java| java.major_version))
}

/* ---------------- libraries ---------------- */

pub fn library_local_path(library: &Library) -> Option<String> {
    if let Some(artifact) = library
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.artifact.as_ref())
    {
        return Some(artifact.path.clone());
    }
    maven_to_path(&library.name)
}

pub fn maven_to_path(name: &str) -> Option<String> {
    let (coordinates, extension) = match name.split_once('@') {
        Some((coordinates, extension)) => (coordinates, extension),
        None => (name, "jar"),
    };
    let mut parts = coordinates.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    let classifier = parts.next();
    let file = match classifier {
        Some(classifier) if !classifier.is_empty() => {
            format!("{artifact}-{version}-{classifier}.{extension}")
        }
        _ => format!("{artifact}-{version}.{extension}"),
    };
    Some(format!(
        "{}/{}/{}/{}",
        group.replace('.', "/"),
        artifact,
        version,
        file
    ))
}

/* ---------------- natives / assets / logging ---------------- */

fn is_natives_dir_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "natives" || name.starts_with("natives") || name.ends_with("-natives")
}

pub fn find_natives_dir(root: &Path, version: &str) -> Option<PathBuf> {
    let dir = version_dir(root, version);
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_natives_dir_name)
        })
        .collect();
    candidates.sort();
    let platform = os_name();
    if let Some(preferred) = candidates.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().contains(platform))
    }) {
        return Some(preferred.clone());
    }
    if let Some(preferred) = candidates.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().ends_with("-natives"))
    }) {
        return Some(preferred.clone());
    }
    candidates.into_iter().next()
}

fn maven_native_path(name: &str, classifier: &str) -> Option<String> {
    let (coordinates, extension) = match name.split_once('@') {
        Some((coordinates, extension)) => (coordinates, extension),
        None => (name, "jar"),
    };
    let mut parts = coordinates.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    Some(format!(
        "{}/{}/{}/{artifact}-{version}-{classifier}.{extension}",
        group.replace('.', "/"),
        artifact,
        version
    ))
}

fn library_jar_path(root: &Path, library: &Library) -> Option<PathBuf> {
    library_local_path(library).map(|relative| root.join("libraries").join(relative))
}

fn native_jar_path(root: &Path, library: &Library) -> Option<PathBuf> {
    let classifier = library.natives.as_ref()?.get(os_name())?;
    let relative = library
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.classifiers.as_ref())
        .and_then(|classifiers| classifiers.get(classifier))
        .map(|artifact| artifact.path.clone())
        .or_else(|| maven_native_path(&library.name, classifier))?;
    Some(root.join("libraries").join(relative))
}

fn extract_natives(root: &Path, version: &str, jars: &[PathBuf]) -> Result<PathBuf, LaunchError> {
    let target = version_dir(root, version).join(format!("{version}-natives"));
    std::fs::create_dir_all(&target).map_err(|error| {
        LaunchError::new("natives_extract", format!("{}: {error}", target.display()))
    })?;
    for jar in jars {
        let file = File::open(jar).map_err(|error| {
            LaunchError::new("natives_extract", format!("{}: {error}", jar.display()))
        })?;
        let mut archive = zip::ZipArchive::new(file).map_err(|error| {
            LaunchError::new("natives_extract", format!("{}: {error}", jar.display()))
        })?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|error| {
                LaunchError::new("natives_extract", format!("{}: {error}", jar.display()))
            })?;
            let Some(relative) = entry.enclosed_name() else {
                continue;
            };
            let name = relative.to_string_lossy();
            if name == "META-INF" || name.starts_with("META-INF/") {
                continue;
            }
            let output = target.join(&relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&output).map_err(|error| {
                    LaunchError::new("natives_extract", format!("{}: {error}", output.display()))
                })?;
                continue;
            }
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    LaunchError::new("natives_extract", format!("{}: {error}", parent.display()))
                })?;
            }
            let mut sink = File::create(&output).map_err(|error| {
                LaunchError::new("natives_extract", format!("{}: {error}", output.display()))
            })?;
            std::io::copy(&mut entry, &mut sink).map_err(|error| {
                LaunchError::new("natives_extract", format!("{}: {error}", output.display()))
            })?;
        }
    }
    Ok(target)
}

pub fn log_config_path(root: &Path, json: &VersionJson) -> Option<PathBuf> {
    let id = &json.logging.as_ref()?.client.as_ref()?.file.id;
    let path = root.join("assets").join("log_configs").join(id);
    path.is_file().then_some(path)
}

/* ---------------- offline identity ---------------- */

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic offline UUID derived from the player name (not Mojang's MD5 variant).
pub fn offline_uuid(player: &str) -> String {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&fnv1a(player.as_bytes(), 0).to_be_bytes());
    bytes[8..].copy_from_slice(&fnv1a(player.as_bytes(), 0x9e37_79b9_7f4a_7c15).to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub name: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub server: Option<String>,
}

fn account_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

pub fn client_identity(config: &ConfigStore) -> Result<ClientIdentity, LaunchError> {
    let current = config
        .get("accounts", "current")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let entries = config
        .get("accounts", "list")
        .and_then(serde_json::Value::as_array);
    let entry = entries.and_then(|entries| {
        entries
            .iter()
            .find(|entry| account_field(entry, "id") == Some(current) && !current.is_empty())
    });
    let Some(entry) = entry else {
        return Ok(ClientIdentity {
            name: DEFAULT_PLAYER_NAME.to_string(),
            uuid: offline_uuid(DEFAULT_PLAYER_NAME),
            access_token: "0".to_string(),
            user_type: "legacy".to_string(),
            server: None,
        });
    };
    let name = account_field(entry, "name").unwrap_or(DEFAULT_PLAYER_NAME);
    let kind = account_field(entry, "kind").unwrap_or("offline");
    match kind {
        "offline" => Ok(ClientIdentity {
            name: name.to_string(),
            uuid: offline_uuid(name),
            access_token: "0".to_string(),
            user_type: "legacy".to_string(),
            server: None,
        }),
        "online" | "third-party" => {
            let uuid = account_field(entry, "uuid").ok_or_else(|| {
                LaunchError::new("account_unsupported", "账号缺少 UUID，请重新登录")
            })?;
            let access_token = account_field(entry, "access_token").ok_or_else(|| {
                LaunchError::new("account_unsupported", "账号缺少访问令牌，请重新登录")
            })?;
            let expires_at = entry.get("expires_at").and_then(serde_json::Value::as_u64);
            if let Some(expires_at) = expires_at {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0);
                if expires_at <= now {
                    return Err(LaunchError::new(
                        "account_expired",
                        "账号登录已过期，请在账号页刷新登录",
                    ));
                }
            }
            Ok(ClientIdentity {
                name: name.to_string(),
                uuid: uuid.to_string(),
                access_token: access_token.to_string(),
                user_type: "msa".to_string(),
                server: account_field(entry, "server").map(str::to_string),
            })
        }
        _ => Err(LaunchError::new(
            "account_unsupported",
            format!("未知账号类型: {kind}"),
        )),
    }
}

/* ---------------- plan assembly ---------------- */

pub struct LaunchContext<'a> {
    pub player_name: &'a str,
    pub version_id: &'a str,
    pub version_type: &'a str,
    pub game_dir: &'a Path,
    pub assets_root: &'a Path,
    pub assets_index: &'a str,
    pub uuid: &'a str,
    pub access_token: &'a str,
    pub user_type: &'a str,
    pub natives_dir: &'a Path,
    pub libraries_root: &'a Path,
    pub classpath: &'a str,
    pub launcher_version: &'a str,
}

fn placeholder(key: &str, context: &LaunchContext<'_>) -> Option<String> {
    Some(match key {
        "auth_player_name" | "profile_name" => context.player_name.to_string(),
        "version_name" => context.version_id.to_string(),
        "version_type" => context.version_type.to_string(),
        "game_directory" => context.game_dir.display().to_string(),
        "assets_root" | "game_assets" => context.assets_root.display().to_string(),
        "assets_index_name" => context.assets_index.to_string(),
        "auth_uuid" => context.uuid.to_string(),
        "auth_access_token" => context.access_token.to_string(),
        "auth_session" => format!("token:{}:{}", context.access_token, context.uuid),
        "user_type" => context.user_type.to_string(),
        "user_properties" => "{}".to_string(),
        "natives_directory" => context.natives_dir.display().to_string(),
        "library_directory" => context.libraries_root.display().to_string(),
        "classpath" => context.classpath.to_string(),
        "classpath_separator" => classpath_separator().to_string(),
        "launcher_name" => "terminism".to_string(),
        "launcher_version" => context.launcher_version.to_string(),
        "clientid" | "auth_xuid" | "version_separator" => String::new(),
        _ => return None,
    })
}

pub fn substitute(text: &str, context: &LaunchContext<'_>) -> Result<String, LaunchError> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(LaunchError::new(
                "placeholder_unknown",
                format!("未闭合的占位符: {text}"),
            ));
        };
        let key = &after[..end];
        let Some(value) = placeholder(key, context) else {
            return Err(LaunchError::new(
                "placeholder_unknown",
                format!("未知占位符 ${{{key}}}"),
            ));
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn classpath_separator() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub json: VersionJson,
    pub classpath: Vec<PathBuf>,
    pub natives_dir: PathBuf,
    pub assets_root: PathBuf,
    pub assets_index: String,
}

fn find_client_jar(
    root: &Path,
    version: &str,
    chain: &[(String, VersionJson)],
    merged: &VersionJson,
) -> Result<PathBuf, LaunchError> {
    if let Some(jar) = merged.jar.as_deref().filter(|jar| !jar.is_empty()) {
        let path = version_dir(root, jar).join(format!("{jar}.jar"));
        if path.is_file() {
            return Ok(path);
        }
    }
    for (id, _) in chain {
        let path = version_dir(root, id).join(format!("{id}.jar"));
        if path.is_file() {
            return Ok(path);
        }
    }
    let chain_text = chain
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>()
        .join(" -> ");
    Err(LaunchError::new(
        "client_jar_missing",
        format!("客户端 jar 不存在（版本 {version} 的继承链: {chain_text}）"),
    ))
}

pub fn resolve_version(root: &Path, version: &str) -> Result<ResolvedVersion, LaunchError> {
    let chain = load_chain(root, version)?;
    let json = merge_chain(&chain);
    let client_jar = find_client_jar(root, version, &chain, &json)?;
    let mut classpath = Vec::new();
    let mut classpath_seen = HashSet::new();
    let mut native_jars = Vec::new();
    let mut native_seen = HashSet::new();
    let mut missing = Vec::new();
    let mut missing_native = Vec::new();
    for library in json.libraries.as_deref().unwrap_or_default() {
        if !rules_allow(&library.rules) {
            continue;
        }
        let classifier_native = library
            .name
            .split(':')
            .nth(3)
            .is_some_and(|part| part.starts_with("natives"));
        if classifier_native {
            let Some(path) = library_jar_path(root, library) else {
                missing_native.push(library.name.clone());
                continue;
            };
            if path.is_file() {
                if native_seen.insert(path.clone()) {
                    native_jars.push(path);
                }
            } else {
                missing_native.push(path.display().to_string());
            }
            continue;
        }
        if let Some(path) = library_jar_path(root, library) {
            if !path.is_file() {
                missing.push(path.display().to_string());
            } else if classpath_seen.insert(path.clone()) {
                classpath.push(path);
            }
        } else if library.downloads.is_none() && library.natives.is_none() {
            missing.push(library.name.clone());
        }
        if library.natives.is_some() {
            match native_jar_path(root, library) {
                Some(path) if path.is_file() => {
                    if native_seen.insert(path.clone()) {
                        native_jars.push(path);
                    }
                }
                Some(path) => missing_native.push(path.display().to_string()),
                None => missing_native.push(library.name.clone()),
            }
        }
    }
    if !missing.is_empty() {
        let shown: Vec<String> = missing.iter().take(3).cloned().collect();
        return Err(LaunchError::new(
            "library_missing",
            format!(
                "缺少 {} 个库文件，例如: {}",
                missing.len(),
                shown.join(", ")
            ),
        ));
    }
    if !missing_native.is_empty() {
        let shown: Vec<String> = missing_native.iter().take(3).cloned().collect();
        return Err(LaunchError::new(
            "native_missing",
            format!(
                "缺少 {} 个原生库文件，例如: {}",
                missing_native.len(),
                shown.join(", ")
            ),
        ));
    }
    classpath.push(client_jar);
    let mut natives_dir = chain.iter().find_map(|(id, _)| find_natives_dir(root, id));
    if natives_dir.is_none() {
        let shared = root.join("natives");
        if shared.is_dir() {
            natives_dir = Some(shared);
        }
    }
    let natives_dir = match natives_dir {
        Some(dir) => dir,
        None if native_jars.is_empty() => {
            return Err(LaunchError::new(
                "natives_missing",
                format!(
                    "{} 下没有 natives 目录，且没有可解压的原生库",
                    version_dir(root, version).display()
                ),
            ));
        }
        None => extract_natives(root, version, &native_jars)?,
    };
    let assets_root = root.join("assets");
    let assets_index = json
        .asset_index
        .as_ref()
        .map(|index| index.id.clone())
        .or_else(|| json.assets.clone())
        .unwrap_or_else(|| "legacy".to_string());
    Ok(ResolvedVersion {
        json,
        classpath,
        natives_dir,
        assets_root,
        assets_index,
    })
}

#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub java: PathBuf,
    pub jvm_args: Vec<String>,
    pub main_class: String,
    pub game_args: Vec<String>,
    pub game_dir: PathBuf,
}

pub fn build_plan(
    root: &Path,
    version: &str,
    game_dir: &Path,
    settings: &ResolvedSettings,
    java: PathBuf,
    identity: &ClientIdentity,
    authlib_jar: Option<&Path>,
) -> Result<LaunchPlan, LaunchError> {
    let resolved = resolve_version(root, version)?;
    let json = &resolved.json;
    let main_class = json
        .main_class
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LaunchError::new("version_invalid", "版本 JSON 缺少 mainClass"))?;
    let classpath_text = resolved
        .classpath
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(classpath_separator());
    let libraries_root = root.join("libraries");
    let context = LaunchContext {
        player_name: &identity.name,
        version_id: version,
        version_type: json.version_type.as_deref().unwrap_or("release"),
        game_dir,
        assets_root: &resolved.assets_root,
        assets_index: &resolved.assets_index,
        uuid: &identity.uuid,
        access_token: &identity.access_token,
        user_type: &identity.user_type,
        natives_dir: &resolved.natives_dir,
        libraries_root: &libraries_root,
        classpath: &classpath_text,
        launcher_version: env!("CARGO_PKG_VERSION"),
    };
    let mut jvm_args = Vec::new();
    for extra in &settings.jvm_args {
        jvm_args.push(substitute(extra, &context)?);
    }
    let version_jvm = json
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.jvm.as_deref())
        .map(collect_args)
        .unwrap_or_default();
    if version_jvm.is_empty() {
        jvm_args.push(format!(
            "-Djava.library.path={}",
            resolved.natives_dir.display()
        ));
        jvm_args.push("-cp".to_string());
        jvm_args.push(classpath_text.clone());
    } else {
        for argument in version_jvm {
            jvm_args.push(substitute(&argument, &context)?);
        }
        if !jvm_args
            .iter()
            .any(|argument| argument.starts_with("-Djava.library.path="))
        {
            jvm_args.push(format!(
                "-Djava.library.path={}",
                resolved.natives_dir.display()
            ));
        }
        if !jvm_args
            .iter()
            .any(|argument| argument == "-cp" || argument == "-classpath")
        {
            jvm_args.push("-cp".to_string());
            jvm_args.push(classpath_text.clone());
        }
    }
    jvm_args.push(format!("-Xmx{}M", settings.memory_mb));
    if let Some(log_config) = log_config_path(root, json) {
        let argument = json
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            .and_then(|client| client.argument.as_deref())
            .unwrap_or("-Dlog4j.configurationFile=${path}");
        let argument = argument.replace("${path}", &log_config.display().to_string());
        jvm_args.push(argument);
    }
    let mut game_args = json
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.game.as_deref())
        .map(collect_args)
        .unwrap_or_default();
    if game_args.is_empty()
        && let Some(legacy) = &json.minecraft_arguments
    {
        game_args = legacy.split_whitespace().map(str::to_string).collect();
    }
    if game_args.is_empty() {
        return Err(LaunchError::new(
            "version_invalid",
            "版本 JSON 没有 game 参数",
        ));
    }
    let mut substituted = Vec::with_capacity(game_args.len());
    for argument in game_args {
        substituted.push(substitute(&argument, &context)?);
    }
    if let Some(server) = &identity.server {
        let jar = authlib_jar.ok_or_else(|| {
            LaunchError::new(
                "authlib_missing",
                "第三方账号需要 authlib-injector，请在设置中选择 jar 路径",
            )
        })?;
        if !jar.is_file() {
            return Err(LaunchError::new(
                "authlib_missing",
                format!("authlib-injector jar 不存在: {}", jar.display()),
            ));
        }
        jvm_args.insert(0, format!("-javaagent:{}={server}", jar.display()));
    }
    for extra in &settings.game_args {
        substituted.push(substitute(extra, &context)?);
    }
    Ok(LaunchPlan {
        java,
        jvm_args,
        main_class,
        game_args: substituted,
        game_dir: game_dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_inherited_from_and_main_class() {
        let json: VersionJson =
            serde_json::from_str(r#"{"inheritsFrom":"base","id":"x","mainClass":"a.B"}"#).unwrap();
        assert_eq!(json.inherited_from.as_deref(), Some("base"));
        assert_eq!(json.main_class.as_deref(), Some("a.B"));
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "terminism-launch-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn scaffold(root: &Path) -> ResolvedSettings {
        let dir = root.join("versions/1.20.1");
        write(
            &dir.join("1.20.1.json"),
            r#"{
                "id": "1.20.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {"id": "5"},
                "javaVersion": {"majorVersion": 17},
                "libraries": [
                    {"name": "g:a:1.0", "downloads": {"artifact": {"path": "g/a/1.0/a-1.0.jar"}}}
                ],
                "arguments": {
                    "jvm": ["-Djava.library.path=${natives_directory}", "-cp", "${classpath}"],
                    "game": ["--username", "${auth_player_name}", "--version", "${version_name}", "--gameDir", "${game_directory}"]
                }
            }"#,
        );
        write(&dir.join("1.20.1.jar"), "jar");
        fs::create_dir_all(dir.join("natives")).unwrap();
        write(&root.join("libraries/g/a/1.0/a-1.0.jar"), "lib");
        write(&root.join("assets/indexes/5.json"), "{}");
        ResolvedSettings {
            java_path: Some("java".into()),
            memory_mb: 4096,
            jvm_args: Vec::new(),
            game_args: Vec::new(),
        }
    }

    fn identity() -> ClientIdentity {
        ClientIdentity {
            name: "Player".into(),
            uuid: offline_uuid("Player"),
            access_token: "0".into(),
            user_type: "legacy".into(),
            server: None,
        }
    }

    #[test]
    fn builds_plan_with_classpath_and_substitution() {
        let root = tmp("plan");
        let settings = scaffold(&root);
        let game_dir = root.join("versions/1.20.1");
        let identity = identity();
        let plan = build_plan(
            &root,
            "1.20.1",
            &game_dir,
            &settings,
            PathBuf::from("java"),
            &identity,
            None,
        )
        .unwrap();
        assert_eq!(plan.main_class, "net.minecraft.client.main.Main");
        assert_eq!(plan.game_args[1], "Player");
        assert_eq!(plan.game_args[3], "1.20.1");
        assert!(plan.jvm_args.iter().any(|argument| argument == "-Xmx4096M"));
        let classpath_arg = plan
            .jvm_args
            .iter()
            .position(|argument| argument == "-cp")
            .expect("classpath flag");
        let classpath = &plan.jvm_args[classpath_arg + 1];
        assert!(classpath.contains("a-1.0.jar"));
        assert!(classpath.ends_with("1.20.1.jar"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_minecraft_arguments_are_split() {
        let root = tmp("legacy");
        let dir = root.join("versions/1.8.9");
        write(
            &dir.join("1.8.9.json"),
            r#"{
                "id": "1.8.9",
                "mainClass": "net.minecraft.client.main.Main",
                "minecraftArguments": "--username ${auth_player_name} --version ${version_name}",
                "libraries": []
            }"#,
        );
        write(&dir.join("1.8.9.jar"), "jar");
        fs::create_dir_all(dir.join("natives")).unwrap();
        let settings = ResolvedSettings {
            java_path: None,
            memory_mb: 2048,
            jvm_args: Vec::new(),
            game_args: Vec::new(),
        };
        let identity = identity();
        let plan = build_plan(
            &root,
            "1.8.9",
            &dir,
            &settings,
            PathBuf::from("java"),
            &identity,
            None,
        )
        .unwrap();
        assert_eq!(
            plan.game_args,
            vec!["--username", "Player", "--version", "1.8.9"]
        );
        assert!(
            plan.jvm_args
                .iter()
                .any(|argument| argument.starts_with("-Djava.library.path="))
        );
        assert!(plan.jvm_args.iter().any(|argument| argument == "-cp"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn inheritance_merges_libraries_before_parent_and_arguments() {
        let root = tmp("inherit");
        let dir = root.join("versions/base");
        write(
            &dir.join("base.json"),
            r#"{
                "id": "base",
                "mainClass": "base.Main",
                "libraries": [{"name": "p:base:1.0"}],
                "arguments": {"game": ["--base"], "jvm": ["-Dbase=1"]}
            }"#,
        );
        write(&dir.join("base.jar"), "jar");
        write(
            &root.join("versions/child/child.json"),
            r#"{
                "id": "child",
                "inheritsFrom": "base",
                "libraries": [{"name": "p:child:1.0"}],
                "arguments": {"game": ["--child"]}
            }"#,
        );
        write(&root.join("versions/child/child.jar"), "jar");
        fs::create_dir_all(root.join("versions/child/natives")).unwrap();
        let merged = load_version(&root, "child").unwrap();
        assert_eq!(merged.main_class.as_deref(), Some("base.Main"));
        let names: Vec<&str> = merged
            .libraries
            .as_ref()
            .unwrap()
            .iter()
            .map(|library| library.name.as_str())
            .collect();
        assert_eq!(names, vec!["p:child:1.0", "p:base:1.0"]);
        let game = merged
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.game.as_ref())
            .unwrap();
        assert_eq!(collect_args(game), vec!["--base", "--child"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rules_filter_foreign_platforms() {
        let rules: Vec<Rule> =
            serde_json::from_str(r#"[{"action":"allow","os":{"name":"windows"}}]"#).unwrap();
        assert_eq!(rules_allow(&rules), cfg!(windows));
        let none: Vec<Rule> = Vec::new();
        assert!(rules_allow(&none));
    }

    #[test]
    fn identity_placeholders_use_context_values() {
        let context = LaunchContext {
            player_name: "Alex",
            version_id: "1",
            version_type: "release",
            game_dir: Path::new("/tmp"),
            assets_root: Path::new("/tmp/assets"),
            assets_index: "5",
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5",
            access_token: "secret",
            user_type: "msa",
            natives_dir: Path::new("/tmp/natives"),
            libraries_root: Path::new("/tmp/libraries"),
            classpath: "cp",
            launcher_version: "0.0.1",
        };
        assert_eq!(substitute("${auth_player_name}", &context).unwrap(), "Alex");
        assert_eq!(
            substitute("${auth_uuid}", &context).unwrap(),
            "069a79f4-44e9-4726-a5be-fca90e38aaf5"
        );
        assert_eq!(
            substitute("${auth_access_token}", &context).unwrap(),
            "secret"
        );
        assert_eq!(substitute("${user_type}", &context).unwrap(), "msa");
    }

    #[test]
    fn third_party_account_injects_authlib_agent() {
        let root = tmp("authlib");
        let settings = scaffold(&root);
        let game_dir = root.join("versions/1.20.1");
        let jar = root.join("authlib-injector.jar");
        write(&jar, "jar");
        let identity = ClientIdentity {
            name: "Notch".into(),
            uuid: offline_uuid("Notch"),
            access_token: "token".into(),
            user_type: "msa".into(),
            server: Some("https://example.com/api/yggdrasil".into()),
        };
        let plan = build_plan(
            &root,
            "1.20.1",
            &game_dir,
            &settings,
            PathBuf::from("java"),
            &identity,
            Some(&jar),
        )
        .unwrap();
        assert_eq!(
            plan.jvm_args[0],
            format!(
                "-javaagent:{}={}",
                jar.display(),
                "https://example.com/api/yggdrasil"
            )
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn third_party_account_without_jar_is_rejected() {
        let root = tmp("authlib-missing");
        let settings = scaffold(&root);
        let game_dir = root.join("versions/1.20.1");
        let identity = ClientIdentity {
            name: "Notch".into(),
            uuid: offline_uuid("Notch"),
            access_token: "token".into(),
            user_type: "msa".into(),
            server: Some("https://example.com/api/yggdrasil".into()),
        };
        let error = build_plan(
            &root,
            "1.20.1",
            &game_dir,
            &settings,
            PathBuf::from("java"),
            &identity,
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind, "authlib_missing");
        let _ = fs::remove_dir_all(&root);
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        for (name, data) in entries {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(data).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn inherited_profile_uses_parent_jar_and_natives() {
        let root = tmp("inherit-jar");
        let parent = root.join("versions/1.20.1");
        write(
            &parent.join("1.20.1.json"),
            r#"{"id":"1.20.1","mainClass":"net.minecraft.client.main.Main","libraries":[],"arguments":{"game":["--username","${auth_player_name}"],"jvm":["-cp","${classpath}"]}}"#,
        );
        write(&parent.join("1.20.1.jar"), "jar");
        fs::create_dir_all(parent.join("natives")).unwrap();
        let child = root.join("versions/fabric-loader-0.15.0-1.20.1");
        write(
            &child.join("fabric-loader-0.15.0-1.20.1.json"),
            r#"{"id":"fabric-loader-0.15.0-1.20.1","inheritsFrom":"1.20.1","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.15.0"}],"arguments":{"game":["--username","${auth_player_name}"],"jvm":["-cp","${classpath}"]}}"#,
        );
        write(
            &root.join("libraries/net/fabricmc/fabric-loader/0.15.0/fabric-loader-0.15.0.jar"),
            "jar",
        );
        let resolved = resolve_version(&root, "fabric-loader-0.15.0-1.20.1").unwrap();
        assert_eq!(
            resolved.json.main_class.as_deref(),
            Some("net.fabricmc.loader.impl.launch.knot.KnotClient")
        );
        assert!(
            resolved
                .classpath
                .iter()
                .any(|path| path.ends_with("1.20.1.jar"))
        );
        assert!(resolved.natives_dir.ends_with("1.20.1/natives"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pcl_style_profile_uses_natives_suffix_and_library_directory() {
        let root = tmp("pcl-style");
        let name = "1.20.1-Forge_47.4.22";
        let dir = root.join("versions").join(name);
        write(
            &dir.join(format!("{name}.json")),
            r#"{"id":"1.20.1-Forge_47.4.22","mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher","libraries":[],"arguments":{"game":["--username","${auth_player_name}"],"jvm":["-DlibraryDirectory=${library_directory}","-p","${library_directory}/cpw/mods/bootstraplauncher/1.1.2/bootstraplauncher-1.1.2.jar"]}}"#,
        );
        write(&dir.join(format!("{name}.jar")), "jar");
        fs::create_dir_all(dir.join(format!("{name}-natives"))).unwrap();
        let settings = ResolvedSettings {
            java_path: None,
            memory_mb: 4096,
            jvm_args: Vec::new(),
            game_args: Vec::new(),
        };
        let identity = identity();
        let plan = build_plan(
            &root,
            name,
            &dir,
            &settings,
            PathBuf::from("java"),
            &identity,
            None,
        )
        .unwrap();
        let libraries = root.join("libraries").display().to_string();
        assert!(
            plan.jvm_args
                .iter()
                .any(|argument| argument == &format!("-DlibraryDirectory={libraries}")),
            "{:?}",
            plan.jvm_args
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_map_library_extracts_classifier_jar() {
        let root = tmp("native-extract");
        let name = "1.20.1-native";
        let dir = root.join("versions").join(name);
        write(
            &dir.join(format!("{name}.json")),
            r#"{"id":"1.20.1-native","mainClass":"net.minecraft.client.main.Main","libraries":[{"name":"org.lwjgl:lwjgl:3.3.1","downloads":{"artifact":{"path":"org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1.jar"},"classifiers":{"natives-windows":{"path":"org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-natives-windows.jar"}}},"natives":{"windows":"natives-windows"}}],"arguments":{"game":["--username","${auth_player_name}"],"jvm":["-cp","${classpath}"]}}"#,
        );
        write(&dir.join(format!("{name}.jar")), "jar");
        write(
            &root.join("libraries/org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1.jar"),
            "jar",
        );
        write_zip(
            &root.join("libraries/org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-natives-windows.jar"),
            &[("windows/x64/lwjgl.dll", b"dll")],
        );
        let resolved = resolve_version(&root, name).unwrap();
        assert!(
            resolved
                .classpath
                .iter()
                .any(|path| path.ends_with("lwjgl-3.3.1.jar"))
        );
        assert!(
            !resolved
                .classpath
                .iter()
                .any(|path| path.ends_with("natives-windows.jar")),
            "native jar must not be on the classpath"
        );
        assert!(resolved.natives_dir.join("windows/x64/lwjgl.dll").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let context = LaunchContext {
            player_name: "Steve",
            version_id: "1",
            version_type: "release",
            game_dir: Path::new("/tmp"),
            assets_root: Path::new("/tmp/assets"),
            assets_index: "5",
            uuid: "uuid",
            access_token: "token",
            user_type: "msa",
            natives_dir: Path::new("/tmp/natives"),
            libraries_root: Path::new("/tmp/libraries"),
            classpath: "cp",
            launcher_version: "0.0.1",
        };
        assert_eq!(substitute("${version_name}", &context).unwrap(), "1");
        let error = substitute("${nope}", &context).unwrap_err();
        assert_eq!(error.kind, "placeholder_unknown");
    }

    #[test]
    fn maven_coordinates_map_to_paths() {
        assert_eq!(
            maven_to_path("org.lwjgl:lwjgl:3.3.3").as_deref(),
            Some("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar")
        );
        assert_eq!(
            maven_to_path("org.lwjgl:lwjgl:3.3.3:natives-windows").as_deref(),
            Some("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar")
        );
    }

    #[test]
    fn offline_uuid_is_stable_and_formatted() {
        let uuid = offline_uuid("Steve");
        assert_eq!(uuid, offline_uuid("Steve"));
        assert_ne!(uuid, offline_uuid("Alex"));
        assert_eq!(uuid.len(), 36);
        assert_eq!(
            uuid.chars().filter(|character| *character == '-').count(),
            4
        );
    }
}
