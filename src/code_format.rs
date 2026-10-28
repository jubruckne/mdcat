//! Display-only formatting for C-family source, including GLSL and Metal.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const STYLE: &str = "{BasedOnStyle: LLVM, IndentWidth: 4, ContinuationIndentWidth: 4, UseTab: Never, BreakBeforeBraces: Attach, ColumnLimit: 100, AllowShortBlocksOnASingleLine: Never, AllowShortFunctionsOnASingleLine: None, AllowShortIfStatementsOnASingleLine: Never, AllowShortLoopsOnASingleLine: false}";

pub fn supports(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "c" | "h"
                    | "cpp"
                    | "cc"
                    | "cxx"
                    | "hpp"
                    | "hh"
                    | "hxx"
                    | "glsl"
                    | "vert"
                    | "frag"
                    | "comp"
                    | "geom"
                    | "tesc"
                    | "tese"
                    | "metal"
                    | "hlsl"
            )
        })
}

pub fn format(source: &str) -> Result<String> {
    let config = mdcat::config::config_path()
        .map(|path| mdcat::config::load(&path))
        .transpose()?
        .flatten();
    let executable = config
        .and_then(|config| config.clang_format)
        .unwrap_or_else(|| PathBuf::from("clang-format"));
    format_with(&executable, source)
}

fn format_with(executable: &Path, source: &str) -> Result<String> {
    // A temporary stdin file avoids pipe deadlocks on large generated shaders.
    // The formatter never receives the original file's path or an in-place flag.
    let mut input = tempfile::tempfile()?;
    input.write_all(source.as_bytes())?;
    input.seek(SeekFrom::Start(0))?;
    let output = Command::new(executable)
        .arg("--assume-filename=code.cpp")
        .arg(format!("--style={STYLE}"))
        .stdin(Stdio::from(input))
        .output()
        .with_context(|| {
            format!(
                "Could not run {}; install clang-format or set clang_format in mdcat's config",
                executable.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "clang-format failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let formatted =
        String::from_utf8(output.stdout).context("Formatter returned non-UTF-8 text")?;
    if !source.trim().is_empty() && formatted.trim().is_empty() {
        bail!("Formatter returned empty output");
    }
    Ok(formatted)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn formatter_errors_do_not_become_display_content() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("formatter");
        std::fs::write(&executable, "#!/bin/sh\necho failure >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(format_with(&executable, "void main() {}")
            .unwrap_err()
            .to_string()
            .contains("failure"));
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(format_with(&executable, "void main() {}").is_err());
        assert!(format_with(&dir.path().join("missing"), "void main() {}").is_err());
    }
}
