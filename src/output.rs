// Copyright 2020 Sebastian Wiesner <sebastian@swsnr.de>

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::io::{self, Write};
use std::process::*;

use anyhow::{bail, Context, Result};
use tracing::{event, Level};

/// The output for mdcat
pub enum Output {
    /// Standard output
    Stdout(std::io::Stdout),
    /// A pager
    Pager(PagerOutput),
    /// A single-file less session with persistent position history.
    Resumable(crate::history::Session),
}

/// Pager input retained for replay into terminal scrollback after the pager exits.
pub struct PagerOutput {
    child: Child,
    retained: Vec<u8>,
    closed: bool,
}

impl Write for PagerOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.retained.extend_from_slice(bytes);
        if !self.closed {
            if let Err(error) = self.child.stdin.as_mut().unwrap().write_all(bytes) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    // Keep rendering after an early quit so scrollback gets the entire document.
                    self.closed = true;
                } else {
                    return Err(error);
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        match self.child.stdin.as_mut().unwrap().flush() {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            result => result,
        }
    }
}

impl Drop for Output {
    /// Drop the output.
    ///
    /// When outputting to a pager wait for the pager to exit.
    fn drop(&mut self) {
        if let Output::Pager(ref mut pager) = *self {
            let _ = pager.child.wait();
        }
    }
}

fn parse_env_var(name: &str) -> Result<Option<Vec<String>>> {
    use std::env::VarError;
    match std::env::var(name) {
        Ok(value) => shell_words::split(&value)
            .with_context(|| format!("Failed to parse value {value} of {name}"))
            .map(Some),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(value)) => bail!("Value of {} not unicode: {:?}", name, value),
    }
}

fn pager_from_env() -> Result<Vec<String>> {
    for envvar in ["MDCAT_PAGER", "PAGER"] {
        event!(Level::TRACE, envvar, "looking for pager in environment");
        match parse_env_var(envvar) {
            // Continue looking
            Ok(None) => {}
            Ok(Some(command)) => {
                event!(Level::INFO, envvar, "Using {:?} as pager", command);
                return Ok(command);
            }
            Err(error) => {
                event!(Level::ERROR, envvar, %error, "Parsing failed: {:#}", error);
                return Err(error);
            }
        }
    }
    event!(
        Level::DEBUG,
        "Falling back to default pager less -r --mouse"
    );
    Ok(vec!["less".into(), "-r".into(), "--mouse".into()])
}

/// Get the basename of the pager command that [`Output::new`] would use, or `None` if it's empty.
pub fn resolved_pager_basename() -> Result<Option<String>> {
    Ok(pager_from_env()?.first().map(|command| {
        std::path::Path::new(command).file_stem().map_or_else(
            || command.clone(),
            |stem| stem.to_string_lossy().into_owned(),
        )
    }))
}

impl Output {
    /// Get the writer to write to the output.
    ///
    /// When outputting to a pager returns the stdin handle to the pager.
    pub fn writer(&mut self) -> &mut dyn Write {
        match self {
            Output::Stdout(handle) => handle,
            Output::Pager(pager) => pager,
            Output::Resumable(session) => &mut session.rendered,
        }
    }

    /// Wait for the pager to restore the terminal, then replay the complete document.
    pub fn finish(&mut self) -> Result<()> {
        if let Output::Resumable(session) = self {
            return session.finish();
        }
        if let Output::Pager(pager) = self {
            drop(pager.child.stdin.take());
            pager.child.wait()?;
            pager.closed = true;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&pager.retained)?;
            stdout.flush()?;
            pager.retained.clear();
        }
        Ok(())
    }

    /// Enable position history for an interactive, single-file less view.
    pub fn for_files(try_paginate: bool, filenames: &[String]) -> Result<Output> {
        if try_paginate && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            let command = pager_from_env()?;
            if command.first().is_some_and(|name| {
                std::path::Path::new(name)
                    .file_stem()
                    .is_some_and(|stem| stem == "less")
            }) {
                if let [filename] = filenames {
                    if filename != "-" {
                        if let Some(session) = crate::history::Session::new(filename, command)? {
                            return Ok(Output::Resumable(session));
                        }
                    }
                }
            }
        }
        Output::new(try_paginate)
    }

    /// Create a new output.
    ///
    /// If `try_paginate` is `true` try to output to a pager.  If stdout is not a TTY, that is, if
    /// there's no terminal to paginate on, print to stdout nonetheless.
    ///
    /// Take the pager command from `$MDCAT_PAGER` or `$PAGER`, and default to `less -r --mouse` if both are
    /// unset.  If any of the variables is empty use stdout (assuming that the user
    /// wanted to disabled paging explicitly).
    pub fn new(try_paginate: bool) -> Result<Output> {
        if try_paginate {
            match pager_from_env()?.split_first() {
                None => {
                    event!(
                        Level::WARN,
                        "Empty pager command, falling back to standard output"
                    );
                    Ok(Output::Stdout(std::io::stdout()))
                }
                Some((command, args)) => {
                    event!(
                        Level::TRACE,
                        "Paginating with command {}, args {:?}",
                        command,
                        args
                    );
                    Command::new(command)
                        .args(args)
                        .stdin(Stdio::piped())
                        .spawn()
                        .with_context(|| {
                            format!("Failed to spawn pager {command} with args {args:?}")
                        })
                        .map(|child| {
                            Output::Pager(PagerOutput {
                                child,
                                retained: Vec::new(),
                                closed: false,
                            })
                        })
                }
            }
        } else {
            Ok(Output::Stdout(std::io::stdout()))
        }
    }
}

/// Page when regular input files contain more than 2.5 terminal heights of raw lines.
/// Stdin is left unpaginated in automatic mode; explicit CLI pagination still works.
pub fn should_auto_paginate(filenames: &[String], rows: u16) -> bool {
    use std::io::{BufRead, BufReader};
    let threshold = usize::from(rows.max(1)) * 5 / 2;
    let mut lines = 0;
    for filename in filenames {
        if crate::is_image_path(std::path::Path::new(filename)) {
            continue;
        }
        if filename == "-" || !std::fs::metadata(filename).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(file) = std::fs::File::open(filename) else {
            continue;
        };
        // Avoid consuming stdin, FIFOs, or other streams during the size check.
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    lines += 1;
                    if lines > threshold {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::should_auto_paginate;

    #[test]
    fn early_pager_exit_retains_the_complete_document() {
        use std::io::Write;
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut pager = super::PagerOutput {
            child,
            retained: Vec::new(),
            closed: false,
        };
        let document = vec![b'x'; 256 * 1024];
        pager.write_all(&document).unwrap();
        pager.write_all(b"last line").unwrap();
        pager.flush().unwrap();
        assert_eq!(pager.retained.len(), document.len() + 9);
        assert!(pager.retained.ends_with(b"last line"));
        pager.child.wait().unwrap();
    }

    #[test]
    fn raw_lines_must_exceed_two_and_a_half_pages() {
        let path = std::env::temp_dir().join(format!("mdcat-auto-{}.md", std::process::id()));
        let files = vec![path.to_string_lossy().into_owned()];
        std::fs::write(&path, "line\n".repeat(60)).unwrap();
        assert!(!should_auto_paginate(&files, 24));
        std::fs::write(&path, format!("{}last line", "line\n".repeat(60))).unwrap();
        assert!(should_auto_paginate(&files, 24));
        assert!(!should_auto_paginate(&files, 30));
        assert!(should_auto_paginate(
            &[files[0].clone(), files[0].clone()],
            24
        ));
        std::fs::remove_file(path).unwrap();
    }
}
