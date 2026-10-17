//! Game process spawning and incremental log-file reading.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::resolve::LaunchPlan;

pub struct RunningGame {
    pub child: Child,
    pub log_path: PathBuf,
    pub offset: u64,
    pub revision: u64,
    pub pid: u32,
}

pub fn spawn(plan: &LaunchPlan, log_path: &Path) -> Result<RunningGame, String> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let log = File::create(log_path).map_err(|error| error.to_string())?;
    let errors = log.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(&plan.java);
    command
        .args(&plan.jvm_args)
        .arg(&plan.main_class)
        .args(&plan.game_args)
        .current_dir(&plan.game_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("{}: {error}", plan.java.display()))?;
    let pid = child.id();
    Ok(RunningGame {
        child,
        log_path: log_path.to_path_buf(),
        offset: 0,
        revision: 0,
        pid,
    })
}

/// Reads complete new lines from offset; a partial trailing line waits for the next poll.
pub fn read_new_lines(game: &mut RunningGame) -> Vec<String> {
    read_lines(&game.log_path, &mut game.offset, &mut game.revision)
}

pub fn read_lines(path: &Path, offset: &mut u64, revision: &mut u64) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if length < *offset {
        *offset = 0;
    }
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    *offset += last_newline as u64 + 1;
    *revision += 1;
    String::from_utf8_lossy(&bytes[..last_newline])
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn tail_lines(path: &Path, count: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    lines[lines.len().saturating_sub(count)..]
        .iter()
        .map(|line| (*line).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("terminism-launch-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("game.log")
    }

    #[test]
    fn incremental_reader_keeps_partial_lines_for_next_poll() {
        let path = temp_log("incremental");
        std::fs::write(&path, "one\ntwo\npart").unwrap();
        let mut offset = 0;
        let mut revision = 0;
        let first = read_lines(&path, &mut offset, &mut revision);
        assert_eq!(first, vec!["one", "two"]);
        assert_eq!(revision, 1);
        std::fs::write(&path, "one\ntwo\npartial line\nthree\n").unwrap();
        let second = read_lines(&path, &mut offset, &mut revision);
        assert_eq!(second, vec!["partial line", "three"]);
        assert!(read_lines(&path, &mut offset, &mut revision).is_empty());
    }

    #[test]
    fn truncated_file_resets_the_offset() {
        let path = temp_log("truncate");
        std::fs::write(&path, "old line\n").unwrap();
        let mut offset = 0;
        let mut revision = 0;
        assert_eq!(
            read_lines(&path, &mut offset, &mut revision),
            vec!["old line"]
        );
        std::fs::write(&path, "new\n").unwrap();
        assert_eq!(read_lines(&path, &mut offset, &mut revision), vec!["new"]);
    }

    #[test]
    fn tail_returns_last_lines() {
        let path = temp_log("tail");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_lines(&path, 2), vec!["c", "d"]);
        assert_eq!(tail_lines(&path, 10), vec!["a", "b", "c", "d"]);
        assert!(tail_lines(&path.with_extension("missing"), 2).is_empty());
    }
}
