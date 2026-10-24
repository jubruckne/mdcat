//! Resume single-file less sessions without storing source filenames in history.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{atomic::AtomicBool, Arc};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

#[derive(Default, Deserialize, Serialize)]
struct History {
    #[serde(default)]
    positions: BTreeMap<String, usize>,
}

fn read_history(path: &Path) -> Result<History> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(History::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_position(path: &Path, key: &str, line: usize) -> Result<()> {
    let parent = path.parent().context("History path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("history.lock"))?;
    lock.lock()?;
    // Re-read under the lock so concurrent sessions merge their entries.
    let mut history = read_history(path)?;
    history.positions.insert(key.to_owned(), line.max(1));
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(toml::to_string(&history)?.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn file_key(path: &Path) -> Result<String> {
    let absolute = path.canonicalize()?;
    Ok(Sha256::digest(absolute.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Rendered input for an interactive less session, retained for scrollback replay.
pub struct Session {
    /// The complete rendered document.
    pub(crate) rendered: Vec<u8>,
    command: Vec<String>,
    key: String,
    history_path: PathBuf,
    start_line: usize,
}

struct InterruptGuard(signal_hook::SigId);

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

impl Session {
    pub(crate) fn new(filename: &str, command: Vec<String>) -> Result<Option<Self>> {
        if !std::fs::metadata(filename).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(None);
        }
        let Some(config) = crate::config::config_path() else {
            return Ok(None);
        };
        let history_path = config.with_file_name("history.toml");
        let key = file_key(Path::new(filename))?;
        let start_line = match read_history(&history_path) {
            Ok(history) => history.positions.get(&key).copied().unwrap_or(1).max(1),
            Err(error) => {
                eprintln!("Warning: could not load pager history: {error:#}");
                1
            }
        };
        Ok(Some(Self {
            rendered: Vec::new(),
            command,
            key,
            history_path,
            start_line,
        }))
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        let mut less_history = NamedTempFile::new()?;
        less_history.write_all(b".less-history-file:\n")?;
        less_history.flush()?;
        let lines = self
            .rendered
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            .max(1);
        let start_line = self.start_line.min(lines);
        // The terminal sends SIGINT to both mdcat and less. Keep mdcat alive while
        // less exits cleanly with -K and writes its final position.
        let _interrupt = InterruptGuard(signal_hook::flag::register(
            signal_hook::consts::SIGINT,
            Arc::new(AtomicBool::new(false)),
        )?);
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .args([
                "--save-marks",
                "--quit-on-intr",
                "--jump-target=1",
                "--prompt=smdcat\\: lines %lt-%lb?e (END).",
            ])
            .arg(format!("+{start_line}g"))
            .env("LESSHISTFILE", less_history.path());
        let mut document = None;
        if cfg!(unix) && Path::new("/dev/stdin").exists() {
            // An explicit filename makes less save the final mark even for a pipe.
            // --force is required because /dev/stdin is not a regular file.
            command
                .arg("--force")
                .arg("/dev/stdin")
                .stdin(Stdio::piped());
        } else {
            // Retain the named-file fallback where /dev/stdin is unavailable.
            let mut file = NamedTempFile::new()?;
            file.write_all(&self.rendered)?;
            file.flush()?;
            command.arg(file.path());
            document = Some(file);
        }
        let mut child = command.spawn().context("Failed to run less")?;
        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&self.rendered)
        } else {
            Ok(())
        };
        child.wait()?;
        drop(document);
        if let Err(error) = write_result {
            // An early q/Ctrl+C can close the pipe before all input was consumed.
            if error.kind() != io::ErrorKind::BrokenPipe {
                return Err(error.into());
            }
        }
        if let Some(line) = final_line(
            &std::fs::read_to_string(less_history.path())?,
            &self.rendered,
        ) {
            if let Err(error) = save_position(&self.history_path, &self.key, line) {
                eprintln!("Warning: could not save pager history: {error:#}");
            }
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(&self.rendered)?;
        stdout.flush()?;
        self.rendered.clear();
        Ok(())
    }
}

fn final_line(history: &str, rendered: &[u8]) -> Option<usize> {
    // less's final-position mark: m ' <screen-row> <byte-offset> <input-name>.
    let offset: usize = history
        .lines()
        .find(|line| line.starts_with("m ' "))?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(
        1 + rendered[..offset.min(rendered.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_roundtrips_and_merges_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        save_position(&path, "first", 40).unwrap();
        save_position(&path, "second", 70).unwrap();
        save_position(&path, "first", 50).unwrap();
        let history = read_history(&path).unwrap();
        assert_eq!(history.positions["first"], 50);
        assert_eq!(history.positions["second"], 70);
    }

    #[test]
    fn keys_use_canonical_full_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("same.md"), "").unwrap();
        std::fs::write(b.join("same.md"), "").unwrap();
        let key = file_key(&a.join("same.md")).unwrap();
        assert_eq!(key.len(), 64);
        assert_ne!(key, file_key(&b.join("same.md")).unwrap());
        assert_eq!(key, file_key(&a.join("../a/same.md")).unwrap());
    }

    #[test]
    fn corrupt_history_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        std::fs::write(&path, "invalid = [").unwrap();
        assert!(save_position(&path, "key", 20).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "invalid = [");
    }

    #[test]
    fn final_mark_maps_byte_offset_to_rendered_line() {
        let body = "first\n日本語\nlast\n".as_bytes();
        assert_eq!(final_line(".mark\nm ' 1 16 /tmp/input", body), Some(3));
        assert_eq!(final_line(".mark\nm ' 1 0 /tmp/input", body), Some(1));
        assert_eq!(final_line(".mark\n", body), None);
    }
}
