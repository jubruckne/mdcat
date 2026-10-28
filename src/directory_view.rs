//! Directory browser with scoped Git status and a shared side-by-side diff preview.
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone)]
struct Change {
    index: u8,
    old: Option<PathBuf>,
}
struct Entry {
    path: PathBuf,
    directory: bool,
    depth: usize,
}
pub enum Action {
    None,
    Open(PathBuf),
    Preview,
    Close,
}
enum Preview {
    Diff(crate::diff_view::DiffView),
    Text(Text<'static>),
}
pub struct DirectoryView {
    directory: PathBuf,
    root: PathBuf,
    changes: BTreeMap<PathBuf, Change>,
    entries: Vec<Entry>,
    cursor: usize,
    changed_only: bool,
    preview: Preview,
    patch: String,
    preview_scroll: usize,
    preview_max: Cell<usize>,
    preview_focus: bool,
    wide: Cell<bool>,
    pub height: usize,
}
fn git(root: &Path, args: &[&str], paths: &[PathBuf]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(["--no-pager", "--literal-pathspecs", "-C"])
        .arg(root)
        .args(args)
        .arg("--")
        .args(paths)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .context("Could not run Git directory command")?;
    anyhow::ensure!(
        out.status.success(),
        "Git directory: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}
fn path_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}
impl DirectoryView {
    pub fn load(directory: &Path) -> Result<Self> {
        let directory = directory.canonicalize()?;
        let out = Command::new("git")
            .arg("-C")
            .arg(&directory)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        anyhow::ensure!(out.status.success(), "No Git repository for this directory");
        let root = path_bytes(out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout));
        let mut view = Self {
            directory,
            root,
            changes: BTreeMap::new(),
            entries: Vec::new(),
            cursor: 0,
            changed_only: false,
            preview: Preview::Text(Text::default()),
            patch: String::new(),
            preview_scroll: 0,
            preview_max: Cell::new(0),
            preview_focus: false,
            wide: Cell::new(false),
            height: 20,
        };
        view.read_changes()?;
        view.changed_only = !view.changes.is_empty();
        view.rebuild()?;
        view.load_preview()?;
        Ok(view)
    }
    pub fn refresh(&mut self) -> Result<()> {
        self.read_changes()?;
        self.rebuild()?;
        self.load_preview()
    }
    fn read_changes(&mut self) -> Result<()> {
        let relative = self.directory.strip_prefix(&self.root)?;
        let path = if relative.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative.to_owned()
        };
        let bytes = git(
            &self.root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            &[path],
        )?;
        self.changes.clear();
        let mut records = bytes.split(|b| *b == 0);
        while let Some(record) = records.next() {
            if record.len() < 4 {
                continue;
            }
            let path = self.root.join(path_bytes(&record[3..]));
            let old = if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
                Some(self.root.join(path_bytes(
                    records.next().context("Incomplete rename status")?,
                )))
            } else {
                None
            };
            if path.starts_with(&self.directory) {
                self.changes.insert(
                    path,
                    Change {
                        index: record[0],
                        old,
                    },
                );
            }
        }
        Ok(())
    }
    fn relevant(&self, path: &Path) -> bool {
        self.changes
            .iter()
            .any(|(changed, _)| changed.starts_with(path))
    }
    fn rebuild(&mut self) -> Result<()> {
        let selected = self.entries.get(self.cursor).map(|e| e.path.clone());
        let mut nodes = BTreeSet::new();
        for path in self.changes.keys() {
            let mut node = path.as_path();
            while node != self.directory {
                nodes.insert(node.to_owned());
                let Some(parent) = node.parent() else { break };
                node = parent;
            }
        }
        let mut entries = Vec::new();
        if self.changed_only {
            fn walk(
                dir: &Path,
                nodes: &BTreeSet<PathBuf>,
                changes: &BTreeMap<PathBuf, Change>,
                depth: usize,
                out: &mut Vec<Entry>,
            ) {
                let mut children = nodes
                    .iter()
                    .filter(|p| p.parent() == Some(dir))
                    .collect::<Vec<_>>();
                children.sort_by_key(|p| (changes.contains_key(*p), p.file_name()));
                for path in children {
                    let directory = !changes.contains_key(path);
                    out.push(Entry {
                        path: path.clone(),
                        directory,
                        depth,
                    });
                    if directory {
                        walk(path, nodes, changes, depth + 1, out);
                    }
                }
            }
            walk(&self.directory, &nodes, &self.changes, 0, &mut entries);
        } else {
            for entry in std::fs::read_dir(&self.directory)? {
                let entry = entry?;
                entries.push(Entry {
                    directory: entry.path().is_dir(),
                    path: entry.path(),
                    depth: 0,
                });
            }
            // Deleted entries remain selectable even though they are absent on disk.
            for node in &nodes {
                if node.parent() == Some(self.directory.as_path())
                    && !entries.iter().any(|e| &e.path == node)
                {
                    entries.push(Entry {
                        path: node.clone(),
                        directory: !self.changes.contains_key(node),
                        depth: 0,
                    });
                }
            }
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.path.cmp(&b.path))
            });
        }
        self.entries = entries;
        self.cursor = selected
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or_else(|| {
                self.entries
                    .iter()
                    .position(|e| !e.directory && self.relevant(&e.path))
                    .unwrap_or(0)
            });
        Ok(())
    }
    fn load_preview(&mut self) -> Result<()> {
        self.preview_scroll = 0;
        self.patch.clear();
        let Some(entry) = self.entries.get(self.cursor) else {
            self.preview = Preview::Text(Text::from("No files in this filter."));
            return Ok(());
        };
        let path = &entry.path;
        if entry.directory && path.exists() {
            let count = self
                .changes
                .iter()
                .filter(|(p, _)| p.starts_with(path))
                .count();
            self.preview = Preview::Text(Text::from(format!(
                "{}\n\n{count} changed files\nEnter to browse this directory.",
                clean(&path.strip_prefix(&self.directory)?.to_string_lossy())
            )));
            return Ok(());
        }
        let relative = path.strip_prefix(&self.root)?.to_owned();
        let change = self.changes.get(path);
        let mut paths = vec![relative.clone()];
        if let Some(old) = change.and_then(|c| c.old.as_ref()) {
            paths.push(old.strip_prefix(&self.root)?.to_owned());
        }
        let has_head = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()?
            .status
            .success();
        let untracked = change.is_some_and(|c| c.index == b'?');
        let patch = if untracked || !has_head {
            if path.exists() {
                addition(path, &relative)?
            } else {
                String::new()
            }
        } else {
            let args = [
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--find-renames",
                "HEAD",
            ];
            String::from_utf8_lossy(&git(&self.root, &args, &paths)?).into_owned()
        };
        if patch.is_empty() {
            let contents = if path.is_file() {
                match String::from_utf8(std::fs::read(path)?) {
                    Ok(text) if !text.contains('\0') => text
                        .lines()
                        .enumerate()
                        .map(|(i, line)| {
                            format!(
                                "{:>4} │ {}",
                                i + 1,
                                clean(&pulldown_cmark_mdcat::expand_tabs(line, 4))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => "Binary file. Enter to open.".into(),
                }
            } else {
                "File is deleted.".into()
            };
            self.preview = Preview::Text(Text::from(format!(
                "{} · No changes in {}\n\n{contents}",
                clean(&relative.to_string_lossy()),
                "HEAD → working tree"
            )));
        } else {
            self.patch = patch;
            let mut diff = crate::diff_view::DiffView::parse(&self.patch);
            diff.set_comparison_label("HEAD → working tree");
            diff.set_fold_markers_visible(false);
            diff.fold_all(false);
            self.preview = Preview::Diff(diff);
        }
        Ok(())
    }
    pub fn patch(&self) -> &str {
        &self.patch
    }
    fn open(&self) -> Action {
        self.entries.get(self.cursor).map_or(Action::None, |e| {
            if e.path.exists() {
                Action::Open(e.path.clone())
            } else {
                Action::Preview
            }
        })
    }
    fn move_by(&mut self, delta: isize) -> Result<()> {
        let next = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        if next != self.cursor {
            self.cursor = next;
            self.load_preview()?;
        }
        Ok(())
    }
    fn scroll_preview(&mut self, delta: isize) {
        self.preview_scroll = self
            .preview_scroll
            .min(self.preview_max.get())
            .saturating_add_signed(delta);
    }
    pub fn key(&mut self, code: KeyCode) -> Result<Action> {
        if !self.wide.get() {
            self.preview_focus = false;
        }
        match code {
            KeyCode::Enter => return Ok(self.open()),
            KeyCode::Char('d') => {
                return Ok(if self.patch.is_empty() {
                    self.open()
                } else {
                    Action::Preview
                })
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => return Ok(Action::Close),
            KeyCode::Char('f') => {
                self.changed_only = !self.changed_only;
                self.rebuild()?;
                self.load_preview()?;
            }
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Tab if self.wide.get() => self.preview_focus = !self.preview_focus,
            KeyCode::Left => self.preview_focus = false,
            KeyCode::Right if self.wide.get() => self.preview_focus = true,
            KeyCode::Up | KeyCode::Char('k') if !self.preview_focus => self.move_by(-1)?,
            KeyCode::Down | KeyCode::Char('j') if !self.preview_focus => self.move_by(1)?,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_preview(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_preview(1),
            KeyCode::PageUp => self.scroll_preview(-(self.height as isize)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_preview(self.height as isize),
            KeyCode::Home if !self.preview_focus => {
                self.cursor = 0;
                self.load_preview()?;
            }
            KeyCode::End if !self.preview_focus => {
                self.cursor = self.entries.len().saturating_sub(1);
                self.load_preview()?;
            }
            KeyCode::Home => self.preview_scroll = 0,
            KeyCode::End => self.preview_scroll = self.preview_max.get(),
            _ => (),
        }
        Ok(Action::None)
    }
    fn list_width(width: usize) -> usize {
        if width >= 90 {
            (width / 3).clamp(28, 40)
        } else {
            width
        }
    }
    fn list_start(&self) -> usize {
        self.cursor.saturating_sub(self.height.max(1) - 1)
    }
    pub fn mouse(&mut self, mouse: MouseEvent, area: Rect) -> Result<()> {
        let width = usize::from(area.width.saturating_sub(4));
        if mouse.row <= area.y
            || mouse.row >= area.bottom().saturating_sub(1)
            || mouse.column < area.x + 2
            || mouse.column >= area.right().saturating_sub(2)
        {
            return Ok(());
        }
        self.preview_focus = usize::from(mouse.column - area.x - 2) >= Self::list_width(width);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if !self.preview_focus => {
                let cursor = self.list_start() + usize::from(mouse.row - area.y - 1);
                if cursor < self.entries.len() {
                    self.cursor = cursor;
                    self.load_preview()?;
                }
            }
            MouseEventKind::ScrollDown if !self.preview_focus => self.move_by(1)?,
            MouseEventKind::ScrollUp if !self.preview_focus => self.move_by(-1)?,
            MouseEventKind::ScrollDown => self.scroll_preview(3),
            MouseEventKind::ScrollUp => self.scroll_preview(-3),
            _ => (),
        }
        Ok(())
    }
    pub fn footer(&self) -> String {
        format!(
            " {} · {} · f: all/changed · Enter: open · d: diff · Tab: focus · r: refresh ",
            if self.changed_only {
                "Changed files"
            } else {
                "All files"
            },
            "HEAD → working tree"
        )
    }
    pub fn render(&self, width: usize, dark: bool) -> Text<'static> {
        let left = Self::list_width(width);
        let split = left < width;
        self.wide.set(split);
        let right = width.saturating_sub(left + 3);
        let preview = if split {
            match &self.preview {
                Preview::Diff(diff) => diff.render(right, dark),
                Preview::Text(text) => text.clone(),
            }
        } else {
            Text::default()
        };
        self.preview_max
            .set(preview.lines.len().saturating_sub(self.height));
        let start = self.list_start();
        let offset = self.preview_scroll.min(self.preview_max.get());
        let mut rows = Vec::new();
        for row in 0..self.height {
            let (label, style) =
                self.entries
                    .get(start + row)
                    .map_or((String::new(), Style::default()), |e| {
                        let name = clean(&e.path.file_name().unwrap_or_default().to_string_lossy());
                        let style = if start + row == self.cursor {
                            crate::diff_view::after_style(dark)
                        } else if self.relevant(&e.path) {
                            Style::default().fg(if dark {
                                Color::Rgb(250, 189, 47)
                            } else {
                                Color::Rgb(150, 85, 0)
                            })
                        } else {
                            Style::default()
                        };
                        (
                            format!(
                                "{}{}{}",
                                "  ".repeat(e.depth),
                                name,
                                if e.directory { "/" } else { "" }
                            ),
                            style,
                        )
                    });
            let mut line = clip(Line::styled(label, style), left);
            line.spans.push(Span::styled(
                " ".repeat(left.saturating_sub(line.width())),
                style,
            ));
            if split {
                line.spans
                    .push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
                if let Some(part) = preview.lines.get(offset + row) {
                    line.spans.extend(clip(part.clone(), right).spans);
                }
            }
            rows.push(line);
        }
        Text::from(rows)
    }
}
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let text: String = span
            .content
            .graphemes(true)
            .take_while(|g| {
                used += g.width();
                used <= width
            })
            .collect();
        spans.push(Span::styled(text, line.style.patch(span.style)));
        if used >= width {
            break;
        }
    }
    Line::from(spans)
}
fn addition(path: &Path, relative: &Path) -> Result<String> {
    let name = clean(&relative.to_string_lossy());
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Ok(format!(
            "diff --git a/{name} b/{name}\nBinary files /dev/null and b/{name} differ\n"
        ));
    }
    let source = std::str::from_utf8(&bytes)?;
    let mut patch = format!(
        "diff --git a/{name} b/{name}\n--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{} @@\n",
        source.lines().count()
    );
    for line in source.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !source.is_empty() && !source.ends_with('\n') {
        patch.push_str("\\ No newline at end of file\n");
    }
    Ok(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn select(view: &mut DirectoryView, filename: &str) {
        view.cursor = view
            .entries
            .iter()
            .position(|e| e.path.ends_with(filename))
            .unwrap();
        view.load_preview().unwrap();
    }
    #[test]
    fn directory_combines_staged_and_unstaged_changes_and_handles_other_file_states() {
        let (dir, _) = crate::git_links::tests::repository();
        let root = dir.path();
        std::fs::write(root.join("clean.txt"), "unchanged\n").unwrap();
        std::fs::write(root.join("deleted.txt"), "remove\n").unwrap();
        std::fs::create_dir(root.join("clean-folder")).unwrap();
        std::fs::write(root.join("clean-folder/file.txt"), "clean\n").unwrap();
        command(root, &["add", "."]);
        command(root, &["commit", "-qm", "Add files"]);
        std::fs::write(root.join("example.txt"), "staged\n").unwrap();
        command(root, &["add", "example.txt"]);
        std::fs::write(root.join("example.txt"), "working\n").unwrap();
        std::fs::remove_file(root.join("deleted.txt")).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/new file.txt"), "new\n").unwrap();
        let mut view = DirectoryView::load(root).unwrap();
        assert!(view.changed_only);
        assert!(
            !DirectoryView::load(&root.join("clean-folder"))
                .unwrap()
                .changed_only
        );
        select(&mut view, "example.txt");
        assert!(view.patch.contains("-after\n+working"));
        assert!(!view.entries.iter().any(|e| e.path.ends_with("clean.txt")));
        assert!(view
            .entries
            .iter()
            .any(|e| e.path.ends_with("nested") && e.directory));
        select(&mut view, "nested/new file.txt");
        assert!(view.patch.contains("+new"));
        select(&mut view, "deleted.txt");
        assert!(view.patch.contains("-remove"));
        assert!(matches!(view.key(KeyCode::Enter).unwrap(), Action::Preview));
        view.key(KeyCode::Char('f')).unwrap();
        select(&mut view, "clean.txt");
        assert!(view.patch.is_empty());
        assert!(matches!(view.key(KeyCode::Enter).unwrap(), Action::Open(_)));
        view.refresh().unwrap();
        assert!(
            !view.changed_only,
            "refresh must preserve the user's choice"
        );
        for width in [0, 1, 20, 89, 90, 120] {
            assert!(
                view.render(width, true)
                    .lines
                    .iter()
                    .all(|line| line.width() <= width),
                "width {width}"
            );
        }
        view.key(KeyCode::Char('f')).unwrap();
        select(&mut view, "example.txt");
        let text = view.render(120, true).to_string();
        assert!(text.contains("after") && text.contains("working"));
        assert!(view.patch.starts_with("diff --git "));
        if let Preview::Diff(diff) = &view.preview {
            let rendered = diff.render(120, true);
            let header = rendered.lines[0].to_string();
            assert!(header.starts_with("example.txt "));
            let label = "(HEAD → working tree)";
            let start = header.find(label).unwrap();
            let label_center = Line::raw(&header[..start]).width() + Line::raw(label).width() / 2;
            let divider = rendered
                .lines
                .iter()
                .find_map(|line| {
                    let text = line.to_string();
                    text.find(" │ ")
                        .map(|offset| Line::raw(&text[..offset]).width() + 1)
                })
                .unwrap();
            assert_eq!(label_center, divider);
        } else {
            panic!("expected diff preview");
        }
        let mut document =
            crate::navigation::Document::load(root.to_owned(), Default::default()).unwrap();
        select(document.directory_view.as_mut().unwrap(), "deleted.txt");
        assert!(document
            .open_directory_diff()
            .unwrap()
            .source
            .contains("-remove"));
    }
    #[test]
    fn staged_renames_include_both_paths_in_preview() {
        let (dir, _) = crate::git_links::tests::repository();
        assert!(!DirectoryView::load(dir.path()).unwrap().changed_only);
        command(dir.path(), &["mv", "example.txt", "renamed name.txt"]);
        let mut view = DirectoryView::load(dir.path()).unwrap();
        select(&mut view, "renamed name.txt");
        assert!(view.patch.contains("rename from example.txt"));
        assert!(view.patch.contains("rename to renamed name.txt"));
        let row = view
            .entries
            .iter()
            .position(|e| e.path.ends_with("renamed name.txt"))
            .unwrap();
        view.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: row as u16 + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Rect::new(0, 0, 124, 28),
        )
        .unwrap();
        assert!(view.entries[view.cursor].path.ends_with("renamed name.txt"));
    }
    #[test]
    fn initial_repository_shows_combined_addition_without_head() {
        let dir = tempfile::tempdir().unwrap();
        command(dir.path(), &["init", "-q", "--template="]);
        let path = dir.path().join("first.txt");
        std::fs::write(&path, "first\n").unwrap();
        command(dir.path(), &["add", "first.txt"]);
        std::fs::write(&path, "second\n").unwrap();
        let view = DirectoryView::load(dir.path()).unwrap();
        assert!(view.patch.contains("+second"));
    }
}
