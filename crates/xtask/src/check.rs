use std::path::Path;

use crate::Error;

pub(crate) fn run(dir: &Path) -> Result<String, Error> {
    if !dir.is_dir() {
        return Err(Error::NotADirectory(dir.to_path_buf()));
    }
    let descriptor = terminism_kernel::plugin::PluginDescriptor::read(dir)?;
    descriptor.check()?;
    Ok(descriptor.manifest.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn plugin_check_accepts_wasm() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/wasm-hello");
        assert_eq!(run(&dir).unwrap(), "wasm-hello");
    }
    use crate::tasks::temp_dir;

    #[test]
    fn plugin_check_ok() {
        let dir = temp_dir("plugin-ok");
        fs::write(dir.join("manifest.toml"), "id = \"test-plugin\"\n").unwrap();
        fs::write(dir.join("main.rhai"), "fn main() {}\n").unwrap();
        assert_eq!(run(&dir).unwrap(), "test-plugin");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plugin_check_bad_manifest() {
        let dir = temp_dir("plugin-bad");
        fs::write(dir.join("manifest.toml"), "id = \"Bad_Id\"\n").unwrap();
        fs::write(dir.join("main.rhai"), "").unwrap();
        assert!(run(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
