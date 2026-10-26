//! Resolve commit references against a document's local repository.
use anyhow::{bail, Context, Result};
use chrono::Datelike;
use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    ops::Range,
    path::PathBuf,
    process::{Command, Stdio},
};

pub struct GitLinks {
    pub root: Option<PathBuf>,
    cache: RefCell<HashMap<String, Option<String>>>,
}

pub fn commit_id(target: &str) -> Option<&str> {
    let id = target.strip_prefix("mdcat-commit:")?;
    ((id.len() == 40 || id.len() == 64) && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

pub fn commit_title(source: &str) -> Option<String> {
    let mut lines = source.lines();
    let id = lines.next()?.strip_prefix("commit ")?;
    commit_id(&format!("mdcat-commit:{id}"))?;
    let header = lines
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let author = header
        .iter()
        .find_map(|line| line.strip_prefix("Author:"))?
        .trim();
    let author = author.rsplit_once(" <").map_or(author, |(name, _)| name);
    let date = header
        .iter()
        .find_map(|line| {
            line.strip_prefix("AuthorDate:")
                .or_else(|| line.strip_prefix("Date:"))
        })?
        .trim();
    let date = chrono::DateTime::parse_from_rfc3339(date)
        .or_else(|_| chrono::DateTime::parse_from_str(date, "%a %b %e %H:%M:%S %Y %z"))
        .ok()?;
    let local = date.with_timezone(&chrono::Local).fixed_offset();
    Some(format_title(id, author, local, chrono::Local::now().year()))
}

fn format_title(
    id: &str,
    author: &str,
    date: chrono::DateTime<chrono::FixedOffset>,
    current_year: i32,
) -> String {
    let year = if date.year() == current_year {
        String::new()
    } else {
        format!(" {}", date.year())
    };
    format!(
        "commit {} by {author} - {}{year} {}",
        &id[..12],
        date.format("%a %b %-d"),
        date.format("%H:%M:%S")
    )
}

fn candidates(text: &str) -> Vec<(Range<usize>, String)> {
    let mut found = Vec::new();
    let mut start = None;
    for (index, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if ch.is_alphanumeric() || ch == '_' {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            let token = &text[start..index];
            if ((7..=40).contains(&token.len()) || token.len() == 64)
                && token.bytes().all(|b| b.is_ascii_hexdigit())
            {
                found.push((start..index, token.to_ascii_lowercase()));
            }
        }
    }
    found
}

impl GitLinks {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn command(&self) -> Option<Command> {
        let mut command = Command::new("git");
        command
            .arg("--no-pager")
            .arg("-C")
            .arg(self.root.as_ref()?)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .stderr(Stdio::null());
        Some(command)
    }

    pub fn references(&self, text: &str) -> Vec<(Range<usize>, url::Url)> {
        if self.root.is_none() {
            return Vec::new();
        }
        let found = candidates(text);
        let mut missing = Vec::new();
        {
            let cache = self.cache.borrow();
            for (_, hash) in &found {
                if !cache.contains_key(hash) && !missing.contains(hash) {
                    missing.push(hash.clone());
                }
            }
        }
        if !missing.is_empty() {
            // Batch resolution avoids one subprocess per hash in large reports.
            let resolved = (|| -> Option<_> {
                let mut child = self
                    .command()?
                    .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .ok()?;
                let mut stdin = child.stdin.take()?;
                let input = missing
                    .iter()
                    .map(|hash| format!("{hash}^{{commit}}\n"))
                    .collect::<String>();
                // Drain stdout while feeding stdin, even for very large documents.
                let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
                let output = child.wait_with_output().ok();
                let written = writer.join().ok()?.ok();
                written?;
                let output = output?;
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
            })();
            let mut cache = self.cache.borrow_mut();
            let mut lines = resolved.as_deref().unwrap_or("").lines();
            for hash in missing {
                let value = lines
                    .next()
                    .and_then(|line| line.strip_suffix(" commit"))
                    .filter(|id| {
                        id.starts_with(&hash) && commit_id(&format!("mdcat-commit:{id}")).is_some()
                    })
                    .map(str::to_owned);
                cache.insert(hash, value);
            }
        }
        let cache = self.cache.borrow();
        found
            .into_iter()
            .filter_map(|(range, hash)| {
                let id = cache.get(&hash)?.as_ref()?;
                Some((range, url::Url::parse(&format!("mdcat-commit:{id}")).ok()?))
            })
            .collect()
    }

    pub fn show(&self, target: &str) -> Result<(PathBuf, String)> {
        let id = commit_id(target).context("Invalid Git commit link")?;
        let root = self
            .root
            .as_ref()
            .context("No Git repository for this document")?;
        let output = self
            .command()
            .context("No Git repository")?
            .args([
                "show",
                "--format=medium",
                "--date=iso-strict",
                "--patch",
                "--root",
                "--first-parent",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-decorate",
                id,
                "--",
            ])
            .output()
            .context("Failed to read Git commit")?;
        if !output.status.success() {
            bail!("Could not read Git commit {id}");
        }
        Ok((
            root.clone(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn repository() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args([
                    "-c",
                    "user.name=Viewer Test",
                    "-c",
                    "user.email=viewer@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "--quiet", "--template="]);
        std::fs::write(dir.path().join("example.txt"), "before\n").unwrap();
        git(&["add", "example.txt"]);
        git(&["commit", "--quiet", "-m", "Initial text"]);
        std::fs::write(dir.path().join("example.txt"), "after\n").unwrap();
        git(&["commit", "--quiet", "-am", "Change example"]);
        let id = git(&["rev-parse", "HEAD"]);
        git(&["config", "diff.external", "false"]);
        git(&["branch", "deadbeef"]);
        (dir, id)
    }

    #[test]
    fn commit_captions_use_local_dates_and_omit_only_the_current_year() {
        let id = "0123456789abcdef0123456789abcdef01234567";
        let original = chrono::DateTime::parse_from_rfc3339("2026-10-26T23:03:18-04:00").unwrap();
        let local = original.with_timezone(&chrono::FixedOffset::east_opt(3600).unwrap());
        assert_eq!(
            format_title(id, "julia", local, 2026),
            "commit 0123456789ab by julia - Tue Oct 27 04:03:18"
        );
        assert_eq!(
            format_title(id, "julia", local, 2027),
            "commit 0123456789ab by julia - Tue Oct 27 2026 04:03:18"
        );
        let source = format!("commit {id}\nMerge: abcdef0 abcdef1\nAuthor: julia <test@example.invalid>\nDate: 2026-10-26T23:03:18-04:00\n\n    Message\n");
        let title = commit_title(&source).unwrap();
        assert!(title.starts_with("commit 0123456789ab by julia - "));
        assert!(!title.contains("@"));
    }

    #[test]
    fn resolves_only_real_commits_and_shows_metadata_and_patch() {
        let (dir, id) = repository();
        let git = GitLinks::new(Some(dir.path().to_owned()));
        let text = format!(
            "`{}` and {id}. invalid deadbeef, identifier x{id}, sha {}",
            &id[..7],
            "f".repeat(64)
        );
        let found = git.references(&text);
        assert_eq!(found.len(), 2);
        for (range, url) in &found {
            assert!(id.starts_with(&text[range.clone()]));
            assert_eq!(commit_id(url.as_str()), Some(id.as_str()));
        }
        let (_, diff) = git.show(found[0].1.as_str()).unwrap();
        assert!(diff.contains("Author: Viewer Test"));
        assert!(diff.contains("Date:"));
        assert!(!diff.contains("Commit:") && !diff.contains("CommitDate:"));
        assert!(diff.contains("Change example"));
        assert!(diff.contains("diff --git a/example.txt b/example.txt"));
        assert!(diff.contains("-before\n+after"));
        let tree = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        assert!(git
            .references(String::from_utf8_lossy(&tree.stdout).trim())
            .is_empty());
        assert!(GitLinks::new(None).references(&text).is_empty());
    }
}
