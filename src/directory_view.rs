//! Directory browser with scoped Git status and a shared side-by-side diff preview.
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, PartialEq, Eq)]
struct Change {
    index: u8,
    old: Option<PathBuf>,
    stamp: Option<(u64, Option<std::time::SystemTime>)>,
}
#[derive(Clone, PartialEq, Eq)]
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
    Unchanged,
    Diff(crate::diff_view::DiffView),
    Text(Text<'static>),
}
#[derive(Clone)]
struct GrepLink {
    path: PathBuf,
    line: Option<usize>,
    start: usize,
    end: usize,
}
type RefreshResult = Result<(Option<PathBuf>, bool, Box<DirectoryView>)>;
#[derive(Clone)]
pub struct GrepSnapshot {
    query: Option<String>,
    last: String,
    files: BTreeSet<PathBuf>,
    changed_only: bool,
    selected: Option<PathBuf>,
    preview_scroll: usize,
    grep_all: bool,
}
pub struct DirectoryView {
    directory: PathBuf,
    root: PathBuf,
    head: Option<Vec<u8>>,
    preview_stamp: Option<(u64, Option<std::time::SystemTime>)>,
    changes: BTreeMap<PathBuf, Change>,
    entries: Vec<Entry>,
    cursor: usize,
    list_scroll: usize,
    changed_only: bool,
    grep_query: Option<String>,
    grep_all: bool,
    grep_files: BTreeSet<PathBuf>,
    last_grep: String,
    grep_links: Vec<Option<GrepLink>>,
    preview: Preview,
    patch: String,
    preview_scroll: usize,
    preview_max: Cell<usize>,
    preview_focus: bool,
    dragging_scrollbar: Option<bool>,
    wide: Cell<bool>,
    pub height: usize,
    last_poll: std::time::Instant,
    pending_refresh: Option<std::sync::mpsc::Receiver<RefreshResult>>,
    recent: BTreeMap<PathBuf, Cell<Option<std::time::Instant>>>,
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
            head: None,
            preview_stamp: None,
            changes: BTreeMap::new(),
            entries: Vec::new(),
            cursor: 0,
            list_scroll: 0,
            changed_only: false,
            grep_query: None,
            grep_all: false,
            grep_files: BTreeSet::new(),
            last_grep: String::new(),
            grep_links: Vec::new(),
            preview: Preview::Text(Text::default()),
            patch: String::new(),
            preview_scroll: 0,
            preview_max: Cell::new(0),
            preview_focus: false,
            dragging_scrollbar: None,
            wide: Cell::new(false),
            height: 20,
            last_poll: std::time::Instant::now(),
            pending_refresh: None,
            recent: BTreeMap::new(),
        };
        view.read_changes()?;
        view.changed_only = !view.changes.is_empty();
        view.rebuild()?;
        view.ensure_selection_visible();
        view.load_preview()?;
        Ok(view)
    }
    pub fn refresh(&mut self) -> Result<()> {
        self.pending_refresh = None;
        self.last_poll = std::time::Instant::now();
        let selected = self.selected_path().map(Path::to_owned);
        let scroll = self.preview_scroll;
        self.read_changes()?;
        if let Some(query) = self.grep_query.clone() {
            self.run_grep(&query)?;
        }
        self.rebuild()?;
        self.load_preview()?;
        if self.selected_path() == selected.as_deref() {
            self.preview_scroll = scroll;
        }
        Ok(())
    }
    fn selected_path(&self) -> Option<&Path> {
        if self.grep_all {
            return None;
        }
        self.entries
            .get(self.cursor)
            .map(|entry| entry.path.as_path())
    }
    pub fn poll_timeout(&self) -> std::time::Duration {
        let refresh = if self.pending_refresh.is_some() {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_secs(2).saturating_sub(self.last_poll.elapsed())
        };
        self.recent
            .values()
            .filter_map(Cell::get)
            .fold(refresh, |timeout, started| {
                timeout.min(std::time::Duration::from_secs(60).saturating_sub(started.elapsed()))
            })
    }
    pub fn refresh_on_focus(&mut self) {
        self.pending_refresh = None;
        self.last_poll = std::time::Instant::now() - std::time::Duration::from_secs(2);
    }
    pub fn poll_refresh(&mut self) -> Result<bool> {
        let expired = self.expire_markers(std::time::Instant::now());
        if self.grep_query.is_some() {
            self.pending_refresh = None;
            self.last_poll = std::time::Instant::now();
            return Ok(expired);
        }
        if let Some(receiver) = &self.pending_refresh {
            match receiver.try_recv() {
                Ok(result) => {
                    self.pending_refresh = None;
                    let (selected, changed_only, next) = result?;
                    // Ignore a preview requested before the user changed selection or filter.
                    if changed_only != self.changed_only
                        || selected.as_deref() != self.selected_path()
                    {
                        return Ok(expired);
                    }
                    let text_changed = match (&self.preview, &next.preview) {
                        (_, Preview::Unchanged) => false,
                        (Preview::Text(a), Preview::Text(b)) => a != b,
                        (Preview::Diff(_), Preview::Diff(_)) => false,
                        _ => true,
                    };
                    let changed = self.entries != next.entries
                        || self.changes != next.changes
                        || self.patch != next.patch
                        || text_changed;
                    if changed {
                        for (path, change) in &next.changes {
                            if self.changes.get(path) != Some(change) {
                                self.recent.insert(path.clone(), Cell::new(None));
                            }
                        }
                        for entry in &next.entries {
                            if !entry.directory
                                && !self.entries.iter().any(|old| old.path == entry.path)
                            {
                                self.recent.insert(entry.path.clone(), Cell::new(None));
                            }
                        }
                        if self.selected_path() != next.selected_path() {
                            self.preview_scroll = 0;
                        }
                        self.changes = next.changes;
                        self.entries = next.entries;
                        self.cursor = next.cursor;
                        if !matches!(next.preview, Preview::Unchanged) {
                            self.preview = next.preview;
                        }
                        self.patch = next.patch;
                    }
                    self.preview_stamp = next.preview_stamp;
                    self.head = next.head;
                    return Ok(changed || expired);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(expired),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending_refresh = None;
                    anyhow::bail!("Directory refresh worker stopped");
                }
            }
        }
        if self.last_poll.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_poll = std::time::Instant::now();
            let mut snapshot = Self {
                directory: self.directory.clone(),
                root: self.root.clone(),
                head: self.head.clone(),
                preview_stamp: self.preview_stamp,
                changes: self.changes.clone(),
                entries: self.entries.clone(),
                cursor: self.cursor,
                list_scroll: self.list_start(),
                changed_only: self.changed_only,
                grep_query: self.grep_query.clone(),
                grep_all: self.grep_all,
                grep_files: self.grep_files.clone(),
                last_grep: self.last_grep.clone(),
                grep_links: Vec::new(),
                preview: Preview::Text(Text::default()),
                patch: self.patch.clone(),
                preview_scroll: 0,
                preview_max: Cell::new(0),
                preview_focus: false,
                dragging_scrollbar: None,
                wide: Cell::new(false),
                height: self.height,
                last_poll: self.last_poll,
                pending_refresh: None,
                recent: BTreeMap::new(),
            };
            let (sender, receiver) = std::sync::mpsc::channel();
            self.pending_refresh = Some(receiver);
            std::thread::spawn(move || {
                let selected = snapshot.selected_path().map(Path::to_owned);
                let changed_only = snapshot.changed_only;
                let result = (|| -> Result<()> {
                    let old_changes = snapshot.changes.clone();
                    let old_head = snapshot.head.clone();
                    let old_entries = snapshot.entries.clone();
                    snapshot.read_changes()?;
                    snapshot.rebuild()?;
                    if old_changes == snapshot.changes
                        && old_head == snapshot.head
                        && old_entries == snapshot.entries
                        && snapshot.preview_stamp == snapshot.selected_stamp()
                    {
                        snapshot.preview = Preview::Unchanged;
                    } else {
                        snapshot.load_preview()?;
                    }
                    Ok(())
                })()
                .map(|()| (selected, changed_only, Box::new(snapshot)));
                let _ = sender.send(result);
            });
        }
        Ok(expired)
    }
    fn expire_markers(&mut self, now: std::time::Instant) -> bool {
        let before = self.recent.len();
        self.recent.retain(|_, started| {
            started.get().is_none_or(|started| {
                now.saturating_duration_since(started) < std::time::Duration::from_secs(60)
            })
        });
        self.recent.len() != before
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
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ],
            &[path],
        )?;
        self.changes.clear();
        let mut records = bytes.split(|b| *b == 0);
        while let Some(record) = records.next() {
            if let Some(oid) = record.strip_prefix(b"# branch.oid ") {
                self.head = (oid != b"(initial)").then(|| oid.to_vec());
                continue;
            }
            let (index, path, old) = match record.first() {
                Some(b'1' | b'2' | b'u') => {
                    let fields = if record[0] == b'1' {
                        9
                    } else if record[0] == b'2' {
                        10
                    } else {
                        11
                    };
                    let parts = record.splitn(fields, |b| *b == b' ').collect::<Vec<_>>();
                    anyhow::ensure!(
                        parts.len() == fields && parts[1].len() == 2,
                        "Invalid Git status record"
                    );
                    let old = if record[0] == b'2' {
                        Some(self.root.join(path_bytes(
                            records.next().context("Incomplete rename status")?,
                        )))
                    } else {
                        None
                    };
                    (
                        parts[1][0],
                        self.root.join(path_bytes(parts[fields - 1])),
                        old,
                    )
                }
                Some(b'?') => (b'?', self.root.join(path_bytes(&record[2..])), None),
                _ => continue,
            };
            if path.starts_with(&self.directory) {
                let stamp = std::fs::metadata(&path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok()));
                self.changes.insert(path, Change { index, old, stamp });
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
        let paths: Vec<&PathBuf> = if self.grep_query.is_some() {
            self.grep_files.iter().collect()
        } else {
            self.changes.keys().collect()
        };
        for path in paths {
            let mut node = path.as_path();
            while node != self.directory {
                nodes.insert(node.to_owned());
                let Some(parent) = node.parent() else { break };
                node = parent;
            }
        }
        let mut entries = Vec::new();
        if self.changed_only || self.grep_query.is_some() {
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
            let files = if self.grep_query.is_some() {
                self.grep_files
                    .iter()
                    .map(|p| {
                        (
                            p.clone(),
                            Change {
                                index: 0,
                                old: None,
                                stamp: None,
                            },
                        )
                    })
                    .collect()
            } else {
                self.changes.clone()
            };
            walk(&self.directory, &nodes, &files, 0, &mut entries);
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
    pub fn grep_snapshot(&self) -> GrepSnapshot {
        GrepSnapshot {
            query: self.grep_query.clone(),
            last: self.last_grep.clone(),
            files: self.grep_files.clone(),
            changed_only: self.changed_only,
            selected: self.selected_path().map(Path::to_owned),
            preview_scroll: self.preview_scroll,
            grep_all: self.grep_all,
        }
    }
    pub fn restore_grep(&mut self, snapshot: &GrepSnapshot) -> Result<()> {
        self.grep_query = snapshot.query.clone();
        self.last_grep = snapshot.last.clone();
        self.grep_files = snapshot.files.clone();
        self.changed_only = snapshot.changed_only;
        self.grep_all = snapshot.grep_all;
        self.rebuild()?;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| Some(&entry.path) == snapshot.selected.as_ref())
        {
            self.cursor = index;
        }
        self.ensure_selection_visible();
        self.load_preview()?;
        self.preview_scroll = snapshot.preview_scroll;
        Ok(())
    }
    fn run_grep(&mut self, query: &str) -> Result<()> {
        let out = Command::new("rg")
            .current_dir(&self.directory)
            .args([
                "--files-with-matches",
                "--null",
                "--color=never",
                "-e",
                query,
                "--",
                ".",
            ])
            .output()
            .context("Could not run ripgrep (rg)")?;
        anyhow::ensure!(
            out.status.success() || out.status.code() == Some(1),
            "ripgrep: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        self.grep_files = out
            .stdout
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|p| {
                self.directory
                    .join(path_bytes(p.strip_prefix(b"./").unwrap_or(p)))
            })
            .collect();
        Ok(())
    }
    pub fn search_grep(&mut self, query: &str) -> Result<()> {
        self.run_grep(query)?;
        self.last_grep = query.to_owned();
        self.grep_query = Some(query.to_owned());
        self.grep_all = true;
        self.pending_refresh = None;
        self.rebuild()?;
        self.ensure_selection_visible();
        self.load_preview()
    }
    fn selected_stamp(&self) -> Option<(u64, Option<std::time::SystemTime>)> {
        let metadata = std::fs::metadata(self.selected_path()?).ok()?;
        Some((metadata.len(), metadata.modified().ok()))
    }
    fn load_preview(&mut self) -> Result<()> {
        self.preview_stamp = self.selected_stamp();
        self.preview_scroll = 0;
        self.patch.clear();
        self.grep_links.clear();
        if let Some(query) = &self.grep_query {
            let relative = if self.grep_all {
                Path::new(".")
            } else if let Some(entry) = self.entries.get(self.cursor) {
                entry.path.strip_prefix(&self.directory)?
            } else {
                self.preview = Preview::Text(Text::from("No matches."));
                return Ok(());
            };
            let out = Command::new("rg")
                .current_dir(&self.directory)
                .args(["--json", "-e", query, "--"])
                .arg(relative)
                .output()
                .context("Could not run ripgrep (rg)")?;
            anyhow::ensure!(
                out.status.success() || out.status.code() == Some(1),
                "ripgrep: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            let (text, links) = format_grep_results(&out.stdout)?;
            self.grep_links = links;
            self.preview = Preview::Text(text);
            return Ok(());
        }
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
        let untracked = change.is_some_and(|c| c.index == b'?');
        let patch = if untracked || self.head.is_none() {
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
        if self.grep_all {
            return Action::None;
        }
        self.entries.get(self.cursor).map_or(Action::None, |e| {
            if e.path.exists() {
                Action::Open(e.path.clone())
            } else {
                Action::Preview
            }
        })
    }
    fn move_by(&mut self, delta: isize) -> Result<()> {
        if self.grep_all && !self.entries.is_empty() {
            self.grep_all = false;
            self.cursor = if delta < 0 { self.entries.len() - 1 } else { 0 };
            self.ensure_selection_visible();
            return self.load_preview();
        }
        let next = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        if next != self.cursor {
            self.cursor = next;
            self.load_preview()?;
        }
        self.ensure_selection_visible();
        Ok(())
    }
    fn ensure_selection_visible(&mut self) {
        let height = self.height.max(1);
        let start = self.list_start();
        self.list_scroll = if self.cursor < start {
            self.cursor
        } else if self.cursor >= start + height {
            self.cursor + 1 - height
        } else {
            start
        };
    }
    fn scroll_list(&mut self, delta: isize) {
        self.list_scroll = self
            .list_start()
            .saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(self.height.max(1)));
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
                if self.grep_query.is_some() {
                    self.grep_query = None;
                    self.grep_all = false;
                    self.changed_only = false;
                } else if !self.changed_only {
                    self.changed_only = true;
                } else if !self.last_grep.is_empty() {
                    self.grep_query = Some(self.last_grep.clone());
                    self.grep_all = true;
                } else {
                    self.changed_only = false;
                }
                self.rebuild()?;
                self.ensure_selection_visible();
                self.load_preview()?;
            }
            KeyCode::Char('a') if self.grep_query.is_some() => {
                self.grep_all = true;
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
                self.grep_all = false;
                self.cursor = 0;
                self.ensure_selection_visible();
                self.load_preview()?;
            }
            KeyCode::End if !self.preview_focus => {
                self.grep_all = false;
                self.cursor = self.entries.len().saturating_sub(1);
                self.ensure_selection_visible();
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
        self.list_scroll
            .min(self.entries.len().saturating_sub(self.height.max(1)))
    }
    pub fn grep_link_at(
        &self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<(PathBuf, Option<usize>, Rect)> {
        if self.grep_query.is_none() {
            return None;
        }
        let width = usize::from(area.width.saturating_sub(4));
        let left = Self::list_width(width);
        if left == width || row <= area.y || row >= area.bottom().saturating_sub(1) {
            return None;
        }
        let start_x = area.x as usize + 2 + left + 2;
        let relative_column = usize::from(column).checked_sub(start_x)?;
        let right_width = width.saturating_sub(left + 2);
        if relative_column >= right_width || column >= area.right().saturating_sub(2) {
            return None;
        }
        let preview_row =
            usize::from(row - area.y - 1) + self.preview_scroll.min(self.preview_max.get());
        let link = self.grep_links.get(preview_row)?.as_ref()?;
        let end = link.end.min(right_width);
        if relative_column < link.start || relative_column >= end {
            return None;
        }
        let rect = Rect::new(
            (start_x + link.start) as u16,
            row,
            (end - link.start) as u16,
            1,
        );
        Some((self.directory.join(&link.path), link.line, rect))
    }
    pub fn grep_target_at(
        &self,
        mouse: MouseEvent,
        area: Rect,
    ) -> Option<(PathBuf, Option<usize>)> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        self.grep_link_at(mouse.column, mouse.row, area)
            .map(|(path, line, _)| (path, line))
    }
    pub fn mouse(&mut self, mouse: MouseEvent, area: Rect) -> Result<()> {
        let width = usize::from(area.width.saturating_sub(4));
        let left = Self::list_width(width);
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            self.dragging_scrollbar = None;
            return Ok(());
        }
        let list_track = area.x + 2 + left.saturating_sub(1) as u16;
        let preview_track = area.right().saturating_sub(2);
        let track_click = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && mouse.row > area.y
            && mouse.row < area.bottom().saturating_sub(1)
            && (mouse.column == list_track || (left < width && mouse.column == preview_track));
        if track_click {
            self.dragging_scrollbar = Some(left < width && mouse.column == preview_track);
        }
        if track_click || matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            if let Some(preview) = self.dragging_scrollbar {
                self.preview_focus = preview;
                let height = usize::from(area.height.saturating_sub(2));
                let row =
                    usize::from(mouse.row.saturating_sub(area.y + 1)).min(height.saturating_sub(1));
                let max = if preview {
                    self.preview_max.get()
                } else {
                    self.entries.len().saturating_sub(height)
                };
                let position = row.saturating_mul(max) / height.saturating_sub(1).max(1);
                if preview {
                    self.preview_scroll = position;
                } else if max > 0 {
                    self.list_scroll = position;
                }
                return Ok(());
            }
        }
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
                    self.grep_all =
                        self.grep_query.is_some() && !self.grep_all && self.cursor == cursor;
                    self.cursor = cursor;
                    self.load_preview()?;
                }
            }
            MouseEventKind::ScrollDown if !self.preview_focus => self.scroll_list(3),
            MouseEventKind::ScrollUp if !self.preview_focus => self.scroll_list(-3),
            MouseEventKind::ScrollDown => self.scroll_preview(3),
            MouseEventKind::ScrollUp => self.scroll_preview(-3),
            _ => (),
        }
        Ok(())
    }
    pub fn footer(&self) -> Line<'static> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        Line::from(vec![
            Span::raw(" f: "),
            Span::styled(
                "all",
                if self.changed_only || self.grep_query.is_some() {
                    Style::default()
                } else {
                    bold
                },
            ),
            Span::raw("/"),
            Span::styled(
                "changed",
                if self.changed_only && self.grep_query.is_none() {
                    bold
                } else {
                    Style::default()
                },
            ),
            Span::raw("/"),
            Span::styled(
                "grep",
                if self.grep_query.is_some() {
                    bold
                } else {
                    Style::default()
                },
            ),
            Span::raw(" · Ctrl+G: grep · a: all results · r: refresh "),
        ])
    }
    pub fn render(&self, width: usize, dark: bool) -> Text<'static> {
        let left = Self::list_width(width);
        let split = left < width;
        self.wide.set(split);
        let right = width.saturating_sub(left + 2);
        let preview = if split {
            match &self.preview {
                Preview::Diff(diff) => diff.render(right, dark),
                Preview::Text(text) => text.clone(),
                Preview::Unchanged => unreachable!("worker sentinel must not reach rendering"),
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
                        let style = if !self.grep_all && start + row == self.cursor {
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
            let mut line = clip(Line::styled(label, style), left.saturating_sub(1));
            line.spans.push(Span::styled(
                " ".repeat(left.saturating_sub(line.width())),
                style,
            ));
            if split {
                line.spans
                    .push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                let preview_row = if row == 0 && matches!(self.preview, Preview::Diff(_)) {
                    0
                } else {
                    offset + row
                };
                if let Some(part) = preview.lines.get(preview_row) {
                    line.spans.extend(clip(part.clone(), right).spans);
                }
            }
            rows.push(line);
        }
        Text::from(rows)
    }
    pub fn draw_decorations(&self, frame: &mut ratatui::Frame<'_>, area: Rect, no_colour: bool) {
        let width = usize::from(area.width.saturating_sub(4));
        let height = usize::from(area.height.saturating_sub(2));
        if width == 0 || height == 0 {
            return;
        }
        for (row, entry) in self
            .entries
            .iter()
            .skip(self.list_start())
            .take(height)
            .enumerate()
        {
            if let Some(started) = self.recent.get(&entry.path) {
                if started.get().is_none() {
                    started.set(Some(std::time::Instant::now()));
                }
                frame.buffer_mut()[(area.x + 1, area.y + 1 + row as u16)]
                    .set_symbol("·")
                    .set_style(if no_colour {
                        Style::default()
                    } else {
                        Style::default().fg(Color::Yellow)
                    });
            }
        }
        let left = Self::list_width(width);
        let tracks = [
            (
                area.x + 2 + left.saturating_sub(1) as u16,
                self.entries.len().saturating_sub(height),
                self.list_start(),
            ),
            (
                area.right().saturating_sub(2),
                self.preview_max.get(),
                self.preview_scroll,
            ),
        ];
        for (index, (x, max, position)) in tracks.into_iter().enumerate() {
            if max == 0 || (index == 1 && left == width) {
                continue;
            }
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(if no_colour {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                })
                .thumb_style(if no_colour {
                    Style::default()
                } else {
                    Style::default().fg(Color::Gray)
                });
            let mut state = ScrollbarState::new(max + 1)
                .position(position.min(max))
                .viewport_content_length(height);
            frame.render_stateful_widget(
                scrollbar,
                Rect::new(x, area.y + 1, 1, height as u16),
                &mut state,
            );
        }
    }
}
fn grep_json_bytes(value: &serde_json::Value) -> Result<Vec<u8>> {
    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        Ok(text.as_bytes().to_vec())
    } else {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(
            value
                .get("bytes")
                .and_then(|v| v.as_str())
                .context("Invalid ripgrep JSON text")?,
        )?)
    }
}

fn format_grep_results(output: &[u8]) -> Result<(Text<'static>, Vec<Option<GrepLink>>)> {
    let events: Vec<serde_json::Value> = output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice)
        .collect::<std::result::Result<_, _>>()?;
    let number_width = events
        .iter()
        .filter(|event| event["type"] == "match")
        .filter_map(|event| event["data"]["line_number"].as_u64())
        .map(|number| number.to_string().len())
        .max()
        .unwrap_or(1);
    let mut rows = Vec::new();
    let mut links = Vec::new();
    let mut previous_path: Option<Vec<u8>> = None;
    for event in events {
        if event["type"] != "match" {
            continue;
        }
        let data = &event["data"];
        let path = grep_json_bytes(&data["path"])?;
        if previous_path.as_ref() != Some(&path) {
            if !rows.is_empty() {
                rows.push(Line::default());
                links.push(None);
            }
            let label = clean(&String::from_utf8_lossy(&path));
            links.push(Some(GrepLink {
                path: path_bytes(&path),
                line: None,
                start: 0,
                end: unicode_width::UnicodeWidthStr::width(label.as_str()),
            }));
            rows.push(Line::styled(label, Style::default().fg(Color::Magenta)));
            previous_path = Some(path.clone());
        }
        let line_number = data["line_number"].as_u64().unwrap_or(0) as usize;
        let digits = line_number.to_string().len();
        links.push(Some(GrepLink {
            path: path_bytes(&path),
            line: Some(line_number),
            start: number_width.saturating_sub(digits),
            end: number_width,
        }));
        let bytes = grep_json_bytes(&data["lines"])?;
        let content = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        let mut spans = vec![
            Span::styled(
                format!(
                    "{:>number_width$}",
                    data["line_number"].as_u64().unwrap_or(0)
                ),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(": "),
        ];
        let mut offset = 0;
        if let Some(matches) = data["submatches"].as_array() {
            for matched in matches {
                let start = matched["start"].as_u64().unwrap_or(0) as usize;
                let end = matched["end"].as_u64().unwrap_or(0) as usize;
                if start < offset || end < start || end > content.len() {
                    continue;
                }
                spans.push(Span::raw(clean(&String::from_utf8_lossy(
                    &content[offset..start],
                ))));
                spans.push(Span::styled(
                    clean(&String::from_utf8_lossy(&content[start..end])),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                offset = end;
            }
        }
        spans.push(Span::raw(clean(&String::from_utf8_lossy(
            &content[offset..],
        ))));
        rows.push(Line::from(spans));
    }
    if rows.is_empty() {
        rows.push(Line::raw("No matches."));
        links.push(None);
    }
    Ok((Text::from(rows), links))
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
        view.grep_all = false;
        view.cursor = view
            .entries
            .iter()
            .position(|e| e.path.ends_with(filename))
            .unwrap();
        view.load_preview().unwrap();
    }
    fn background_refresh(view: &mut DirectoryView) -> bool {
        view.last_poll = std::time::Instant::now() - std::time::Duration::from_secs(2);
        assert!(!view.poll_refresh().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let changed = view.poll_refresh().unwrap();
            if view.pending_refresh.is_none() {
                return changed;
            }
            assert!(std::time::Instant::now() < deadline, "refresh timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn grep_click_targets_follow_preview_scroll() {
        let (dir, _) = crate::git_links::tests::repository();
        let path = dir.path().join("match space.txt");
        std::fs::write(&path, "hit\n".repeat(20)).unwrap();
        let mut view = DirectoryView::load(dir.path()).unwrap();
        view.height = 4;
        view.search_grep("hit").unwrap();
        view.render(120, true);
        let area = Rect::new(0, 0, 124, 6);
        let x = 2 + DirectoryView::list_width(120) as u16 + 2;
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let expected = view.directory.join("match space.txt");
        let (hover_file, hover_line, rect) = view.grep_link_at(x, 1, area).unwrap();
        assert_eq!(hover_file, expected);
        assert_eq!(hover_line, None);
        assert_eq!(rect.x, x);
        assert!(rect.width > 1);
        let (file, line) = view.grep_target_at(click(x, 1), area).unwrap();
        assert_eq!(file, expected);
        assert_eq!(line, None);
        let (_, hover_line, rect) = view.grep_link_at(x + 1, 2, area).unwrap();
        assert_eq!(hover_line, Some(1));
        assert_eq!(rect, Rect::new(x + 1, 2, 1, 1));
        let (file, line) = view.grep_target_at(click(x + 1, 2), area).unwrap();
        assert_eq!(file, expected);
        assert_eq!(line, Some(1));
        assert!(view.grep_target_at(click(x + 5, 2), area).is_none());
        view.preview_scroll = 10;
        let (_, line) = view.grep_target_at(click(x, 1), area).unwrap();
        assert_eq!(line, Some(10));
        assert!(view.grep_target_at(click(3, 1), area).is_none());
    }
    #[test]
    fn grep_line_numbers_align_across_files() {
        let records = concat!(
            "{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"a\"},\"lines\":{\"text\":\"hit\\n\"},\"line_number\":2,\"submatches\":[]}}\n",
            "{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"b\"},\"lines\":{\"text\":\"hit\\n\"},\"line_number\":1144,\"submatches\":[]}}\n"
        );
        let text = format_grep_results(records.as_bytes())
            .unwrap()
            .0
            .to_string();
        assert!(text.contains("   2: hit"));
        assert!(text.contains("1144: hit"));
    }
    #[test]
    fn grep_shows_matching_tree_and_lines_and_cycles_filters() {
        let (dir, _) = crate::git_links::tests::repository();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/match.txt"), "first\nneedle here\n").unwrap();
        std::fs::write(dir.path().join("nested/other.txt"), "needle again\n").unwrap();
        let mut view = DirectoryView::load(dir.path()).unwrap();
        std::fs::write(dir.path().join("root-match.txt"), "needle root\n").unwrap();
        let snapshot = view.grep_snapshot();
        view.search_grep("needle").unwrap();
        assert!(view.selected_path().is_none());
        let Preview::Text(all) = &view.preview else {
            panic!("expected grep preview")
        };
        assert!(all.to_string().contains("needle root") && all.to_string().contains("needle here"));
        assert!(matches!(view.key(KeyCode::Enter).unwrap(), Action::None));
        view.key(KeyCode::Down).unwrap();
        assert!(view.selected_path().is_some());
        view.key(KeyCode::Char('a')).unwrap();
        assert!(view.selected_path().is_none());
        select(&mut view, "nested");
        let Preview::Text(preview) = &view.preview else {
            panic!("expected grep preview")
        };
        assert!(preview
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content == "needle"
                && span.style.fg == Some(Color::Red)
                && span.style.add_modifier.contains(Modifier::BOLD)));
        let preview = preview.to_string();
        assert!(preview.contains("nested/match.txt\n2: needle here"));
        assert!(preview.contains("nested/other.txt\n1: needle again"));
        view.restore_grep(&snapshot).unwrap();
        assert!(view.grep_query.is_none());
        view.search_grep("needle").unwrap();
        assert!(view
            .entries
            .iter()
            .any(|e| e.directory && e.path.ends_with("nested")));
        select(&mut view, "nested/match.txt");
        assert!(view
            .render(120, true)
            .to_string()
            .contains("2: needle here"));
        view.key(KeyCode::Char('f')).unwrap();
        assert!(!view.changed_only && view.grep_query.is_none());
        view.key(KeyCode::Char('f')).unwrap();
        assert!(view.changed_only && view.grep_query.is_none());
        view.key(KeyCode::Char('f')).unwrap();
        assert_eq!(view.grep_query.as_deref(), Some("needle"));
        assert!(view.search_grep("[").is_err());
        assert_eq!(view.grep_query.as_deref(), Some("needle"));
        view.search_grep("no-such-match-xyz").unwrap();
        assert!(view.entries.is_empty());
    }
    #[test]
    fn polling_markers_start_on_render_and_expire_after_one_minute() {
        let (dir, _) = crate::git_links::tests::repository();
        let mut view = DirectoryView::load(dir.path()).unwrap();
        select(&mut view, "example.txt");
        assert!(view.recent.is_empty());
        std::fs::write(dir.path().join("example.txt"), "new edit\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "new file\n").unwrap();
        assert!(background_refresh(&mut view));
        let path = view.directory.join("example.txt");
        let new_path = view.directory.join("new.txt");
        assert!(view.recent[&path].get().is_none());
        assert!(view.recent[&new_path].get().is_none());
        // Time before the first draw does not consume the marker's lifetime.
        assert!(
            !view.expire_markers(std::time::Instant::now() + std::time::Duration::from_secs(120))
        );
        let text = view.render(120, true);
        let selected_row = view.cursor - view.list_start();
        assert!(text.lines[selected_row]
            .to_string()
            .starts_with("example.txt"));
        assert!(
            view.recent[&path].get().is_none(),
            "layout must not start the timer"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(124, 22)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(text),
                    Rect::new(2, 1, 120, 20),
                );
                view.draw_decorations(frame, Rect::new(0, 0, 124, 22), false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1 + selected_row as u16)].symbol(), "·");
        assert_eq!(buffer[(1, 1 + selected_row as u16)].fg, Color::Yellow);
        assert_eq!(buffer[(2, 1 + selected_row as u16)].symbol(), "e");
        let started = view.recent[&path].get().unwrap();
        assert!(!background_refresh(&mut view));
        assert_eq!(view.recent[&path].get(), Some(started));
        assert!(!view.expire_markers(started + std::time::Duration::from_secs(59)));
        assert!(view.expire_markers(started + std::time::Duration::from_secs(61)));
        assert!(!view.recent.contains_key(&path));
        assert!(!view.render(120, true).to_string().contains('·'));
    }
    #[test]
    fn pane_scrollbars_render_and_support_clicks_and_dragging() {
        let (dir, _) = crate::git_links::tests::repository();
        let root = dir.path();
        std::fs::write(root.join("example.txt"), "long preview\n".repeat(100)).unwrap();
        for index in 0..20 {
            std::fs::write(root.join(format!("new-{index:02}.txt")), "new\n").unwrap();
        }
        let mut view = DirectoryView::load(root).unwrap();
        view.height = 8;
        select(&mut view, "example.txt");
        let area = Rect::new(0, 0, 124, 10);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(124, 10)).unwrap();
        let text = view.render(120, true);
        view.preview_scroll = 10;
        let scrolled = view.render(120, true);
        assert_eq!(
            text.lines[0], scrolled.lines[0],
            "diff header must stay pinned"
        );
        assert_ne!(text.lines[1], scrolled.lines[1], "diff body must scroll");
        view.preview_scroll = 0;
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(text),
                    Rect::new(2, 1, 120, 8),
                );
                view.draw_decorations(frame, area, true);
            })
            .unwrap();
        for x in [41, 122] {
            let symbols = (1..9)
                .map(|y| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>();
            assert!(symbols.contains('┃') && symbols.contains('│'), "{symbols}");
        }
        assert_eq!(
            terminal.backend().buffer()[(42, 1)].symbol(),
            "│",
            "divider must sit directly beside the list scrollbar"
        );
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let selected = view.cursor;
        let patch = view.patch.clone();
        view.mouse(mouse(MouseEventKind::ScrollDown, 4, 3), area)
            .unwrap();
        assert_eq!(view.list_start(), 3);
        assert_eq!(view.cursor, selected);
        assert_eq!(view.patch, patch);
        assert_eq!(view.preview_scroll, 0);
        view.mouse(mouse(MouseEventKind::ScrollUp, 4, 3), area)
            .unwrap();
        assert_eq!(view.list_start(), 0);
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 122, 8), area)
            .unwrap();
        assert_eq!(view.preview_scroll, view.preview_max.get());
        view.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 122, 0), area)
            .unwrap();
        assert_eq!(view.preview_scroll, 0);
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 122, 0), area)
            .unwrap();
        assert!(view.dragging_scrollbar.is_none());
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 41, 8), area)
            .unwrap();
        assert_eq!(view.cursor, 0, "scrollbar must not change selection");
        assert_eq!(view.list_start(), view.entries.len() - view.height);
        view.key(KeyCode::Down).unwrap();
        assert_eq!(view.cursor, 1);
        assert!(view.cursor >= view.list_start() && view.cursor < view.list_start() + view.height);
    }
    #[test]
    fn background_poll_detects_head_changes_with_unchanged_status_and_file() {
        let (dir, _) = crate::git_links::tests::repository();
        let root = dir.path();
        std::fs::write(root.join("example.txt"), "working\n").unwrap();
        let mut view = DirectoryView::load(root).unwrap();
        assert!(view.patch.contains("-after"));
        let changes = view.changes.clone();
        let stamp = view.preview_stamp;
        command(root, &["reset", "--mixed", "HEAD~1"]);
        assert!(background_refresh(&mut view));
        assert!(view.changes == changes);
        assert_eq!(view.preview_stamp, stamp);
        assert!(view.patch.contains("-before"));
        assert!(!background_refresh(&mut view));
    }
    #[test]
    fn background_poll_preserves_selection_filter_and_scroll_and_detects_changes() {
        let (dir, _) = crate::git_links::tests::repository();
        let root = dir.path();
        let mut view = DirectoryView::load(root).unwrap();
        select(&mut view, "example.txt");
        view.preview_scroll = 3;
        assert!(!background_refresh(&mut view));
        assert_eq!(view.preview_scroll, 3);
        std::fs::write(root.join("example.txt"), "polling update\n").unwrap();
        std::fs::write(root.join("new.txt"), "new file\n").unwrap();
        assert!(background_refresh(&mut view));
        assert!(!view.changed_only);
        assert!(view.selected_path().unwrap().ends_with("example.txt"));
        assert_eq!(view.preview_scroll, 3);
        assert!(view.patch.contains("+polling update"));
        std::fs::write(root.join("example.txt"), "another edit\n").unwrap();
        assert!(background_refresh(&mut view));
        assert!(view.patch.contains("+another edit"));
        assert!(view.entries.iter().any(|e| e.path.ends_with("new.txt")));
        command(root, &["add", "."]);
        command(root, &["commit", "-qm", "Commit polled changes"]);
        assert!(background_refresh(&mut view));
        assert!(view.patch.is_empty());
        std::fs::remove_file(root.join("new.txt")).unwrap();
        assert!(background_refresh(&mut view));
        assert!(view.changes.contains_key(&view.directory.join("new.txt")));
        view.key(KeyCode::Char('f')).unwrap();
        select(&mut view, "new.txt");
        command(root, &["add", "."]);
        command(root, &["commit", "-qm", "Remove selected file"]);
        assert!(background_refresh(&mut view));
        assert!(view.changed_only);
        assert!(view.entries.is_empty());
        assert_eq!(view.preview_scroll, 0);
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
