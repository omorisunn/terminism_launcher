use std::path::{Path, PathBuf};

use crate::manifest::{self, Manifest, ManifestError};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("missing plugin entry for {id} in {}", dir.display())]
    MissingEntry { id: String, dir: PathBuf },
    #[error("invalid plugin: {0}")]
    Invalid(String),
}

pub struct PluginDescriptor {
    pub manifest: Manifest,
    pub dir: PathBuf,
    pub entry: PathBuf,
}

impl PluginDescriptor {
    pub fn read(dir: &Path) -> Result<Self, PluginError> {
        let manifest = manifest::parse(&std::fs::read_to_string(dir.join("manifest.toml"))?)?;
        let entries: &[&str] = if manifest.kind == "wasm" {
            &["plugin.wasm", "plugin.wat"]
        } else {
            &["main.rhai"]
        };
        let entry = entries
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| dir.join(entries[0]));
        Ok(Self {
            manifest,
            dir: dir.to_path_buf(),
            entry,
        })
    }

    pub fn check_entry(&self) -> Result<(), PluginError> {
        if self.entry.is_file() {
            Ok(())
        } else {
            Err(PluginError::MissingEntry {
                id: self.manifest.id.clone(),
                dir: self.dir.clone(),
            })
        }
    }

    pub fn check(&self) -> Result<(), PluginError> {
        self.check_entry()?;
        if self.manifest.kind == "wasm" {
            crate::wasm::check(&std::fs::read(&self.entry)?).map_err(PluginError::Invalid)
        } else {
            rhai::Engine::new()
                .compile(std::fs::read_to_string(&self.entry)?)
                .map(|_| ())
                .map_err(|e| PluginError::Invalid(e.to_string()))
        }
    }
}
