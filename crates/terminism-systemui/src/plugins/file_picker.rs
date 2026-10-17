//! Small directory/file picker popup shared by the instance and settings pages.

use serde_json::json;
use std::path::PathBuf;

use terminism_kernel::host::HostHandle;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerMode {
    Dir,
    File,
}

pub(crate) struct Picker {
    mode: PickerMode,
    dir: PathBuf,
    entries: Vec<PathBuf>,
}

impl Picker {
    pub(crate) fn new(mode: PickerMode, start: Option<&str>) -> Self {
        let candidate = start
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let dir = candidate.canonicalize().unwrap_or(candidate);
        let mut picker = Self {
            mode,
            dir,
            entries: Vec::new(),
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        self.entries.clear();
        let Ok(read) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut all: Vec<PathBuf> = read
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        all.sort();
        for path in all {
            if path.is_dir() || (self.mode == PickerMode::File && path.is_file()) {
                self.entries.push(path);
            }
        }
    }

    pub(crate) fn show(&self, host: &HostHandle, actor: &str, base: &str, title: &str) {
        let ui = host.ui();
        let _ = (ui.remove)(actor, base);
        let _ = (ui.create)(actor, base, "Popup", json!({"title": title}));
        let _ = (ui.create)(
            actor,
            &format!("{base}/path"),
            "Label",
            json!({"text": self.dir.display().to_string(), "flex": 1.0}),
        );
        let list = format!("{base}/list");
        let _ = (ui.create)(actor, &list, "List", json!({"flex": 6.0}));
        for (index, entry) in self.entries.iter().enumerate() {
            let name = entry
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let text = if entry.is_dir() {
                format!("/ {name}")
            } else {
                format!("  {name}")
            };
            let _ = (ui.create)(
                actor,
                &format!("{list}/e-{index}"),
                "Item",
                json!({"text": text}),
            );
        }
        for (suffix, label) in [("up", ".."), ("ok", "OK"), ("cancel", "Cancel")] {
            let _ = (ui.create)(
                actor,
                &format!("{base}/{suffix}"),
                "Button",
                json!({"text": label, "flex": 1.0}),
            );
        }
    }

    /// Consumes a click on picker nodes; returns the chosen path once OK accepts it.
    pub(crate) fn activate(
        &mut self,
        host: &HostHandle,
        actor: &str,
        base: &str,
        path: &str,
        title: &str,
    ) -> Option<PathBuf> {
        if let Some(index) = path
            .strip_prefix(&format!("{base}/list/e-"))
            .and_then(|value| value.parse::<usize>().ok())
        {
            if let Some(entry) = self.entries.get(index).cloned() {
                if entry.is_dir() {
                    self.dir = entry;
                    self.reload();
                    self.show(host, actor, base, title);
                } else {
                    return Some(entry);
                }
            }
            return None;
        }
        if path == format!("{base}/up") {
            if let Some(parent) = self.dir.parent() {
                self.dir = parent.to_path_buf();
                self.reload();
                self.show(host, actor, base, title);
            }
            return None;
        }
        if path == format!("{base}/ok") {
            return Some(self.dir.clone());
        }
        None
    }

    pub(crate) fn close(host: &HostHandle, actor: &str, base: &str) {
        let _ = (host.ui().remove)(actor, base);
    }
}
