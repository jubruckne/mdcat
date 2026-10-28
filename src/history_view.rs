//! Per-file history with a selectable reference and the shared diff renderer.
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

struct Entry {
    id: Option<String>,
    author: String,
    date: String,
    subject: String,
    path: PathBuf,
}

pub struct HistoryView {
    root: PathBuf,
    entries: Vec<Entry>,
    visible: Vec<usize>,
    cursor: usize,
    reference: Option<usize>,
    preview: crate::diff_view::DiffView,
    patch: String,
    preview_scroll: usize,
    preview_max: std::cell::Cell<usize>,
    wide: std::cell::Cell<bool>,
    pub height: usize,
    preview_focus: bool,
    pub filtering: bool,
    filter: String,
}

pub enum Action {
    None,
    Open,
    Close,
}

fn git(root: &Path, args: &[&str], paths: &[PathBuf]) -> Result<String> {
    let output = Command::new("git")
        .args(["--no-pager", "--literal-pathspecs", "-C"])
        .arg(root)
        .args(args)
        .arg("--")
        .args(paths)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .context("Could not run Git history command")?;
    anyhow::ensure!(
        output.status.success(),
        "Git history: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("Git history returned invalid UTF-8")
}

impl HistoryView {
    pub fn load(root: &Path, path: &Path) -> Result<Self> {
        let path = path
            .strip_prefix(root)
            .context("File is outside its Git repository")?
            .to_owned();
        let log = git(
            root,
            &[
                "log",
                "--follow",
                "-z",
                "--format=%H%x00%an%x00%aI%x00%s",
                "--name-only",
            ],
            std::slice::from_ref(&path),
        )?;
        let fields = log.split('\0').collect::<Vec<_>>();
        let mut entries = Vec::new();
        let mut index = 0;
        while index < fields.len() {
            if fields[index].is_empty() {
                index += 1;
                continue;
            }
            anyhow::ensure!(index + 4 < fields.len(), "Incomplete Git history record");
            let id = fields[index];
            anyhow::ensure!(
                crate::git_links::commit_id(&format!("mdcat-commit:{id}")).is_some(),
                "Invalid history commit"
            );
            entries.push(Entry {
                id: Some(id.to_owned()),
                author: clean(fields[index + 1]),
                date: fields[index + 2].to_owned(),
                subject: clean(fields[index + 3]),
                path: PathBuf::from(
                    fields[index + 4]
                        .strip_prefix('\n')
                        .unwrap_or(fields[index + 4]),
                ),
            });
            index += 5;
        }
        anyhow::ensure!(
            !entries.is_empty(),
            "No history for this file (it may be untracked)"
        );
        let working = git(
            root,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD"],
            std::slice::from_ref(&path),
        )?;
        if !working.is_empty() {
            entries.insert(
                0,
                Entry {
                    id: None,
                    author: "Working tree".into(),
                    date: String::new(),
                    subject: "Local changes against HEAD".into(),
                    path,
                },
            );
        }
        let mut view = Self {
            root: root.to_owned(),
            visible: (0..entries.len()).collect(),
            entries,
            cursor: 0,
            reference: None,
            preview: crate::diff_view::DiffView::parse(""),
            patch: String::new(),
            preview_scroll: 0,
            preview_max: std::cell::Cell::new(0),
            wide: std::cell::Cell::new(false),
            height: 20,
            preview_focus: false,
            filtering: false,
            filter: String::new(),
        };
        view.refresh()?;
        Ok(view)
    }

    fn selected(&self) -> Option<usize> {
        self.visible.get(self.cursor).copied()
    }
    fn label(&self, index: usize) -> String {
        let e = &self.entries[index];
        format!(
            "{} · {} · {} · {}",
            e.id.as_ref().map_or("Working tree", |id| &id[..8]),
            e.date.get(..10).unwrap_or(""),
            e.author,
            e.subject
        )
    }

    fn refresh(&mut self) -> Result<()> {
        self.preview_scroll = 0;
        let Some(selected) = self.selected() else {
            self.patch.clear();
            self.preview = crate::diff_view::DiffView::parse("No matching commits");
            return Ok(());
        };
        let entry = &self.entries[selected];
        let mut paths = vec![entry.path.clone()];
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
        ];
        let source = if let Some(reference) = self.reference {
            let base = &self.entries[reference];
            if !paths.contains(&base.path) {
                paths.push(base.path.clone());
            }
            match (&base.id, &entry.id) {
                (Some(base), Some(target)) => {
                    args.extend([base.as_str(), target.as_str()]);
                    git(&self.root, &args, &paths)?
                }
                (Some(base), None) => {
                    args.push(base);
                    git(&self.root, &args, &paths)?
                }
                (None, Some(target)) => {
                    args.extend(["-R", target]);
                    git(&self.root, &args, &paths)?
                }
                (None, None) => String::new(),
            }
        } else if let Some(id) = &entry.id {
            git(
                &self.root,
                &[
                    "show",
                    "--format=%s%n%n%b",
                    "--date=iso-strict",
                    "--patch",
                    "--root",
                    "--first-parent",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--find-renames",
                    id,
                ],
                &paths,
            )?
        } else {
            args.push("HEAD");
            git(&self.root, &args, &paths)?
        };
        let label = self.reference.map_or_else(
            || {
                if entry.id.is_some() {
                    "Parent".into()
                } else {
                    "HEAD".into()
                }
            },
            |i| self.label(i),
        );
        self.patch = format!(
            "Reference: {label}\nSelected: {}\n\n{source}",
            self.label(selected)
        );
        if !source.contains("diff --git ") {
            self.patch
                .push_str("\nNo changes between these revisions.\n");
        }
        self.preview = crate::diff_view::DiffView::parse(&self.patch);
        self.preview.fold_all(false);
        Ok(())
    }

    fn move_by(&mut self, delta: isize) -> Result<()> {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.visible.len().saturating_sub(1));
        self.refresh()
    }
    fn pin(&mut self) -> Result<()> {
        let selected = self.selected();
        self.reference = if self.reference == selected {
            None
        } else {
            selected
        };
        self.refresh()
    }
    fn filter(&mut self) -> Result<()> {
        let query = self.filter.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| self.label(*i).to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.cursor = 0;
        self.refresh()
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('h') {
            return Ok(Action::Close);
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.filter()?;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.filter()?;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.filter.push(c);
                    self.filter()?;
                }
                _ => (),
            }
            return Ok(Action::None);
        }
        if !self.wide.get() {
            self.preview_focus = false;
        }
        match key.code {
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filtering = true
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return Ok(Action::Close),
            KeyCode::Enter if self.selected().is_some() => return Ok(Action::Open),
            KeyCode::Tab if self.wide.get() => self.preview_focus = !self.preview_focus,
            KeyCode::Left => self.preview_focus = false,
            KeyCode::Right if self.wide.get() => self.preview_focus = true,
            KeyCode::Char(' ') if !self.preview_focus => self.pin()?,
            KeyCode::Up | KeyCode::Char('k') if !self.preview_focus => self.move_by(-1)?,
            KeyCode::Down | KeyCode::Char('j') if !self.preview_focus => self.move_by(1)?,
            KeyCode::Up | KeyCode::Char('k') => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_add(1)
            }
            KeyCode::PageUp => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_sub(self.height.saturating_sub(1).max(1))
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_add(self.height.saturating_sub(1).max(1))
            }
            KeyCode::Home if !self.preview_focus => {
                self.cursor = 0;
                self.refresh()?;
            }
            KeyCode::End if !self.preview_focus => {
                self.cursor = self.visible.len().saturating_sub(1);
                self.refresh()?;
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
    fn header_rows(&self) -> usize {
        usize::from(self.filtering || !self.filter.is_empty())
    }
    fn list_start(&self) -> usize {
        self.cursor.saturating_sub(
            self.height
                .saturating_sub(self.header_rows())
                .max(1)
                .saturating_sub(1),
        )
    }
    pub fn mouse(&mut self, mouse: MouseEvent, area: Rect) -> Result<()> {
        let width = usize::from(area.width.saturating_sub(4));
        let column = usize::from(mouse.column.saturating_sub(area.x + 2));
        if mouse.row <= area.y
            || mouse.row >= area.bottom().saturating_sub(1)
            || mouse.column < area.x + 2
            || column >= width
        {
            return Ok(());
        }
        self.preview_focus = column >= Self::list_width(width);
        match mouse.kind {
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right))
                if !self.preview_focus =>
            {
                let row = usize::from(mouse.row - area.y - 1);
                if row >= self.header_rows() {
                    let index = self.list_start() + row - self.header_rows();
                    if index < self.visible.len() {
                        self.cursor = index;
                        if button == MouseButton::Right {
                            self.pin()?;
                        } else {
                            self.refresh()?;
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown if !self.preview_focus => self.move_by(1)?,
            MouseEventKind::ScrollUp if !self.preview_focus => self.move_by(-1)?,
            MouseEventKind::ScrollDown => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_add(3)
            }
            MouseEventKind::ScrollUp => {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview_max.get())
                    .saturating_sub(3)
            }
            _ => (),
        }
        Ok(())
    }
    pub fn patch(&self) -> &str {
        &self.patch
    }
    pub fn render(&self, width: usize, dark: bool) -> Text<'static> {
        let left = Self::list_width(width);
        let split = left < width;
        self.wide.set(split);
        let right = width.saturating_sub(left + 3);
        let preview = if split {
            self.preview.render(right, dark)
        } else {
            Text::default()
        };
        self.preview_max
            .set(preview.lines.len().saturating_sub(self.height));
        let offset = self.preview_scroll.min(self.preview_max.get());
        let start = self.list_start();
        let mut rows = Vec::new();
        for row in 0..self.height {
            let mut line = if row < self.header_rows() {
                Line::raw(format!("Find: {}", self.filter))
            } else if let Some(index) = self.visible.get(start + row - self.header_rows()) {
                let style = if self.reference == Some(*index) {
                    crate::diff_view::before_style(dark)
                } else if self.selected() == Some(*index) {
                    crate::diff_view::after_style(dark)
                } else {
                    Style::default()
                };
                Line::styled(self.entries[*index].subject.clone(), style)
            } else {
                Line::default()
            };
            let padding_style = line.style;
            line = clipped(line, left);
            {
                let used = line.width();
                line.spans.push(Span::styled(
                    " ".repeat(left.saturating_sub(used)),
                    padding_style,
                ));
            }
            if split {
                line.spans
                    .push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
                if let Some(diff) = preview.lines.get(offset + row) {
                    line.spans.extend(
                        diff.spans
                            .iter()
                            .map(|span| span.clone().style(diff.style.patch(span.style))),
                    );
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
fn clipped(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let content: String = span
            .content
            .graphemes(true)
            .take_while(|g| {
                used += g.width();
                used <= width
            })
            .collect();
        spans.push(Span::styled(content, line.style.patch(span.style)));
        if used >= width {
            break;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn history_previews_file_changes_and_pins_commits_or_working_tree_as_reference() {
        let (dir, id) = crate::git_links::tests::repository();
        let path = dir.path().join("example.txt");
        std::fs::write(&path, "local\n").unwrap();
        let mut view = HistoryView::load(dir.path(), &path).unwrap();
        assert_eq!(view.entries.len(), 3);
        assert_eq!(view.entries[0].id, None);
        assert_eq!(view.entries[1].id.as_deref(), Some(id.as_str()));
        assert!(view.patch.contains("-after\n+local"));
        view.key(key(KeyCode::Down)).unwrap();
        view.key(key(KeyCode::Char(' '))).unwrap();
        assert_eq!(view.reference, Some(1));
        assert!(view.patch.contains("No changes between"));
        view.key(key(KeyCode::Down)).unwrap();
        assert!(view.patch.contains("-after\n+before"));
        let list = view.render(80, true);
        assert_eq!(
            list.lines[0].to_string().trim_end(),
            "Local changes against HEAD"
        );
        assert_eq!(list.lines[1].to_string().trim_end(), "Change example");
        assert_eq!(list.lines[2].to_string().trim_end(), "Initial text");
        assert!(list.lines[1]
            .spans
            .iter()
            .all(|span| span.style == crate::diff_view::before_style(true)));
        assert!(list.lines[2]
            .spans
            .iter()
            .all(|span| span.style == crate::diff_view::after_style(true)));
        view.cursor = 0;
        view.pin().unwrap();
        view.key(key(KeyCode::Down)).unwrap();
        assert!(view.patch.contains("-local\n+after"));
        let area = Rect::new(0, 0, 124, 20);
        view.height = 18;
        view.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 4,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            area,
        )
        .unwrap();
        assert_eq!(view.reference, Some(1));
        view.pin().unwrap();
        assert_eq!(view.reference, None);
        assert!(view.patch.contains("-before\n+after"));
        for width in [0, 1, 15, 89, 90, 120] {
            let text = view.render(width, true);
            assert!(
                text.lines.iter().all(|line| line.width() <= width),
                "width {width}"
            );
        }
        let rendered = view.render(120, true);
        assert!(rendered
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style == crate::diff_view::after_style(true)));
        let text = rendered.to_string();
        assert!(text.contains("before") && text.contains("after"));
        view.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .unwrap();
        for c in "Initial".chars() {
            view.key(key(KeyCode::Char(c))).unwrap();
        }
        assert_eq!(view.visible.len(), 1);
        assert_eq!(
            view.entries[view.selected().unwrap()].subject,
            "Initial text"
        );
        view.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(view.visible.len(), 3);
    }

    #[test]
    fn follows_renames_and_full_diff_uses_historical_file_path() {
        let (dir, _) = crate::git_links::tests::repository();
        for args in [
            vec!["mv", "example.txt", "renamed file.txt"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-m",
                "Rename",
            ],
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let path = dir.path().join("renamed file.txt");
        let mut view = HistoryView::load(dir.path(), &path).unwrap();
        assert_eq!(view.entries.len(), 3);
        assert_eq!(view.entries[0].path, PathBuf::from("renamed file.txt"));
        assert_eq!(view.entries[1].path, PathBuf::from("example.txt"));
        view.key(key(KeyCode::Down)).unwrap();
        assert!(view.patch.contains("-before\n+after"));
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let mut history = document.open_history().unwrap();
        history
            .file_history
            .as_mut()
            .unwrap()
            .key(key(KeyCode::Down))
            .unwrap();
        let diff = history.open_history_diff().unwrap();
        assert!(diff.diff.is_some());
        assert!(diff.source.contains("-before\n+after"));
    }
}
