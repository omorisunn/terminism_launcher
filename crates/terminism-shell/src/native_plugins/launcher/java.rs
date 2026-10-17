//! Java discovery: candidate paths and runtime major version probing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCandidate {
    pub path: PathBuf,
    pub major: Option<u32>,
}

impl JavaCandidate {
    pub fn valid(&self) -> bool {
        self.major.is_some()
    }
}

fn java_binary() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Parses the runtime major from `java -version` output; Java 8 reports 1.8 as 8.
pub fn parse_major(text: &str) -> Option<u32> {
    let start = text.find("version \"")? + "version \"".len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    let version = &rest[..end];
    if let Some(legacy) = version.strip_prefix("1.") {
        let major = legacy.split('.').next()?;
        return major.parse().ok();
    }
    let major = version.split('.').next()?;
    major.parse().ok()
}

pub fn probe(path: &Path) -> JavaCandidate {
    let major = Command::new(path)
        .arg("-version")
        .output()
        .ok()
        .and_then(|output| {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            parse_major(&text)
        });
    JavaCandidate {
        path: path.to_path_buf(),
        major,
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if path.is_file() && seen.insert(path.clone()) {
        paths.push(path);
    }
}

fn push_bin(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, home: &Path) {
    push_unique(paths, seen, home.join("bin").join(java_binary()));
}

fn push_children(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut homes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    homes.sort();
    for home in homes {
        push_bin(paths, seen, &home);
    }
}

/// Candidate java executables from the environment and the usual Windows install roots.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let home = PathBuf::from(home);
        push_bin(&mut paths, &mut seen, &home);
    }
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            push_unique(&mut paths, &mut seen, entry.join(java_binary()));
        }
    }
    if cfg!(windows) {
        for base in [
            "C:/Program Files/Java",
            "C:/Program Files/Microsoft",
            "C:/Program Files/Eclipse Adoptium",
            "C:/Program Files/Zulu",
            "C:/Program Files/BellSoft",
            "C:/Program Files/Amazon Corretto",
        ] {
            push_children(&mut paths, &mut seen, Path::new(base));
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            push_children(&mut paths, &mut seen, &Path::new(&home).join(".jdks"));
        }
    } else {
        for base in ["/usr/lib/jvm", "/Library/Java/JavaVirtualMachines"] {
            push_children(&mut paths, &mut seen, Path::new(base));
        }
    }
    paths
}

/// Probes every candidate once; valid entries come first, sorted by major descending.
pub fn discover() -> Vec<JavaCandidate> {
    let mut candidates: Vec<JavaCandidate> =
        search_paths().iter().map(|path| probe(path)).collect();
    candidates.sort_by(|left, right| {
        right
            .major
            .unwrap_or(0)
            .cmp(&left.major.unwrap_or(0))
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.dedup_by(|left, right| left.path == right.path);
    candidates
}

pub fn find_for_major(candidates: &[JavaCandidate], major: Option<u32>) -> Option<&JavaCandidate> {
    match major {
        Some(required) => candidates
            .iter()
            .find(|candidate| candidate.major == Some(required)),
        None => candidates.iter().find(|candidate| candidate.valid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_parsing_handles_legacy_and_modern() {
        assert_eq!(
            parse_major("openjdk version \"21.0.10\" 2025-01-21"),
            Some(21)
        );
        assert_eq!(parse_major("java version \"1.8.0_402\"\n"), Some(8));
        assert_eq!(parse_major("no version line"), None);
    }

    #[test]
    fn candidate_selection_prefers_required_major() {
        let candidates = vec![
            JavaCandidate {
                path: PathBuf::from("a"),
                major: Some(17),
            },
            JavaCandidate {
                path: PathBuf::from("b"),
                major: Some(21),
            },
            JavaCandidate {
                path: PathBuf::from("c"),
                major: None,
            },
        ];
        assert_eq!(
            find_for_major(&candidates, Some(21)).unwrap().path,
            PathBuf::from("b")
        );
        assert_eq!(find_for_major(&candidates, Some(8)), None);
        assert_eq!(
            find_for_major(&candidates, None).unwrap().path,
            PathBuf::from("a")
        );
    }
}
