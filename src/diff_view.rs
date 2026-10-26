//! Side-by-side Git patches with file-level folding.
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use std::cell::RefCell;

#[derive(Clone)]
struct Cell {
    number: usize,
    text: String,
}
enum Row {
    Hunk,
    Note(String),
    Pair {
        old: Option<Cell>,
        new: Option<Cell>,
        changed: bool,
    },
}
struct File {
    name: String,
    old_name: Option<String>,
    rows: Vec<Row>,
    collapsed: bool,
    added: usize,
    removed: usize,
}
#[derive(Default)]
struct Layout {
    files: Vec<Option<usize>>,
    headers: Vec<usize>,
}
pub struct DiffView {
    metadata: Vec<String>,
    files: Vec<File>,
    layout: RefCell<Layout>,
    selected_file: Option<usize>,
}

fn flush(rows: &mut Vec<Row>, old: &mut Vec<Cell>, new: &mut Vec<Cell>) {
    let count = old.len().max(new.len());
    let mut old = std::mem::take(old).into_iter();
    let mut new = std::mem::take(new).into_iter();
    for _ in 0..count {
        rows.push(Row::Pair {
            old: old.next(),
            new: new.next(),
            changed: true,
        });
    }
}
fn hunk_starts(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    Some((
        old.split(',').next()?.parse().ok()?,
        new.split(',').next()?.parse().ok()?,
    ))
}
fn filename(line: &str) -> String {
    let name = line.trim_matches('"');
    name.strip_prefix("a/")
        .or_else(|| name.strip_prefix("b/"))
        .unwrap_or(name)
        .to_owned()
}
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    textwrap::wrap(
        text,
        textwrap::Options::new(width.max(1)).word_splitter(textwrap::WordSplitter::NoHyphenation),
    )
    .into_iter()
    .map(|line| line.into_owned())
    .collect()
}

impl DiffView {
    pub fn parse(source: &str) -> Self {
        let mut view = Self {
            metadata: Vec::new(),
            files: Vec::new(),
            layout: RefCell::new(Layout::default()),
            selected_file: None,
        };
        let (mut old_number, mut new_number) = (0, 0);
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        let mut in_hunk = false;
        let mut in_stats = false;
        let mut message_started = false;
        let compact_metadata = crate::git_links::commit_title(source).is_some();
        for (number, line) in source.lines().enumerate() {
            if number == 0
                && line.strip_prefix("commit ").is_some_and(|id| {
                    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                continue;
            }
            if let Some(name) = line.strip_prefix("diff --git ") {
                if let Some(file) = view.files.last_mut() {
                    flush(&mut file.rows, &mut removed, &mut added);
                }
                view.files.push(File {
                    name: name
                        .rsplit_once(" b/")
                        .map_or_else(|| name.to_owned(), |(_, new)| new.to_owned()),
                    old_name: None,
                    rows: Vec::new(),
                    collapsed: true,
                    added: 0,
                    removed: 0,
                });
                in_hunk = false;
                continue;
            }
            let Some(file) = view.files.last_mut() else {
                if compact_metadata
                    && ["Author:", "AuthorDate:", "Date:", "Commit:", "CommitDate:"]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                {
                    continue;
                }
                if view.metadata.is_empty() && line.is_empty() {
                    continue;
                }
                if line == "---" {
                    in_stats = true;
                    view.metadata.push(String::new());
                } else if !in_stats {
                    if compact_metadata && !message_started && line.starts_with("    ") {
                        if view.metadata.last().is_none_or(|line| !line.is_empty()) {
                            view.metadata.push(String::new());
                        }
                        message_started = true;
                    }
                    view.metadata.push(if line.starts_with("    ") {
                        line.trim_start().to_owned()
                    } else {
                        line.to_owned()
                    });
                }
                continue;
            };
            if let Some((old, new)) = hunk_starts(line) {
                flush(&mut file.rows, &mut removed, &mut added);
                (old_number, new_number) = (old, new);
                in_hunk = true;
                file.rows.push(Row::Hunk);
            } else if in_hunk && line.starts_with('-') {
                removed.push(Cell {
                    number: old_number,
                    text: pulldown_cmark_mdcat::expand_tabs(&line[1..], 4).into_owned(),
                });
                old_number += 1;
                file.removed += 1;
            } else if in_hunk && line.starts_with('+') {
                added.push(Cell {
                    number: new_number,
                    text: pulldown_cmark_mdcat::expand_tabs(&line[1..], 4).into_owned(),
                });
                new_number += 1;
                file.added += 1;
            } else if in_hunk && line.starts_with(' ') {
                flush(&mut file.rows, &mut removed, &mut added);
                let text = pulldown_cmark_mdcat::expand_tabs(&line[1..], 4).into_owned();
                file.rows.push(Row::Pair {
                    old: Some(Cell {
                        number: old_number,
                        text: text.clone(),
                    }),
                    new: Some(Cell {
                        number: new_number,
                        text,
                    }),
                    changed: false,
                });
                old_number += 1;
                new_number += 1;
            } else if in_hunk
                && line.starts_with("\\ No newline")
                && (!added.is_empty() || !removed.is_empty())
            {
                if let Some(cell) = added.last_mut().or_else(|| removed.last_mut()) {
                    cell.text.push_str("  ⟵ No newline at end of file");
                }
            } else {
                flush(&mut file.rows, &mut removed, &mut added);
                if !in_hunk {
                    if let Some(old) = line.strip_prefix("--- ") {
                        file.old_name = Some(filename(old));
                        continue;
                    }
                    if let Some(new) = line.strip_prefix("+++ ") {
                        file.name = if new == "/dev/null" {
                            file.old_name.clone().unwrap_or_else(|| file.name.clone())
                        } else {
                            filename(new)
                        };
                        continue;
                    }
                    if line.starts_with("index ") {
                        continue;
                    }
                }
                file.rows.push(Row::Note(line.to_owned()));
            }
        }
        if let Some(file) = view.files.last_mut() {
            flush(&mut file.rows, &mut removed, &mut added);
        }
        view
    }

    pub fn toggle(&mut self, row: usize, header_only: bool) -> Option<usize> {
        let layout = self.layout.borrow();
        let file = (if header_only {
            None
        } else {
            self.selected_file
        })
        .or_else(|| layout.files.get(row).copied().flatten())
        .or_else(|| (!header_only).then_some(0))?;
        let header = *layout.headers.get(file)?;
        if header_only && row != header {
            return None;
        }
        self.files.get_mut(file)?.collapsed ^= true;
        self.selected_file = Some(file);
        Some(header)
    }
    pub fn fold_all(&mut self, collapsed: bool) -> usize {
        self.selected_file = None;
        for file in &mut self.files {
            file.collapsed = collapsed;
        }
        self.layout.borrow().headers.first().copied().unwrap_or(0)
    }
    pub fn next_file(&mut self, row: usize, forward: bool) -> usize {
        let layout = self.layout.borrow();
        let index = if let Some(selected) = self.selected_file {
            if forward {
                (selected + 1).min(layout.headers.len().saturating_sub(1))
            } else {
                selected.saturating_sub(1)
            }
        } else if forward {
            layout
                .headers
                .iter()
                .position(|header| *header > row)
                .unwrap_or(layout.headers.len().saturating_sub(1))
        } else {
            layout
                .headers
                .iter()
                .rposition(|header| *header < row)
                .unwrap_or(0)
        };
        if let Some(header) = layout.headers.get(index) {
            self.selected_file = Some(index);
            *header
        } else {
            row
        }
    }

    pub fn render(&self, width: usize, dark: bool) -> Text<'static> {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut layout = Layout::default();
        let muted = Style::default().fg(if dark {
            Color::Rgb(146, 140, 131)
        } else {
            Color::Rgb(100, 100, 100)
        });
        let removed_style = Style::default()
            .fg(if dark {
                Color::Rgb(251, 145, 139)
            } else {
                Color::Rgb(150, 30, 30)
            })
            .bg(if dark {
                Color::Rgb(62, 35, 35)
            } else {
                Color::Rgb(255, 228, 228)
            });
        let added_style = Style::default()
            .fg(if dark {
                Color::Rgb(184, 210, 126)
            } else {
                Color::Rgb(35, 100, 30)
            })
            .bg(if dark {
                Color::Rgb(37, 51, 33)
            } else {
                Color::Rgb(228, 245, 222)
            });
        let mut push = |line: Line<'static>, file: Option<usize>| {
            lines.push(line);
            layout.files.push(file);
            lines.len() - 1
        };
        for line in &self.metadata {
            for text in wrap(line, width) {
                push(Line::raw(text), None);
            }
        }
        let max_changes = self
            .files
            .iter()
            .map(|file| file.added + file.removed)
            .max()
            .unwrap_or(1)
            .max(1);
        let max_bar = (width / 4).clamp(1, 24);
        let count_digits = max_changes.to_string().len().max(3);
        let name_width = width.saturating_sub(count_digits + 2 + max_bar);
        for (index, file) in self.files.iter().enumerate() {
            if index > 0 {
                push(Line::styled("─".repeat(width), muted), Some(index));
            }
            let renamed = file
                .old_name
                .as_deref()
                .filter(|old| *old != file.name && *old != "/dev/null");
            let name =
                renamed.map_or_else(|| file.name.clone(), |old| format!("{old} → {}", file.name));
            let total = file.added + file.removed;
            let mut cells = (total * max_bar).div_ceil(max_changes.max(max_bar));
            if file.added > 0 && file.removed > 0 {
                cells = cells.max(2);
            }
            let plus = if file.removed == 0 {
                cells
            } else if file.added == 0 {
                0
            } else {
                ((file.added * cells + total / 2) / total).clamp(1, cells - 1)
            };
            let minus = cells - plus;
            let binary = file.rows.iter().any(|row| matches!(row, Row::Note(text) if text.starts_with("Binary files ") || text == "GIT binary patch"));
            let count = if binary {
                format!(" {:>count_digits$} ", "Bin")
            } else {
                format!(" {total:>count_digits$} ")
            };
            let caption = fit(
                &format!("{} {name}", if file.collapsed { "+" } else { "−" }),
                name_width,
            );
            let spacing = " ".repeat(name_width.saturating_sub(Line::raw(&caption).width()));
            let style = Style::default()
                .fg(Color::Rgb(145, 172, 238))
                .add_modifier(Modifier::BOLD)
                .add_modifier(if self.selected_file == Some(index) {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                });
            let header = if name_width >= 3 {
                push(
                    Line::from(vec![
                        Span::styled(caption, style),
                        Span::raw(spacing),
                        Span::styled(count, muted),
                        Span::styled(
                            "+".repeat(plus),
                            Style::default().fg(added_style.fg.unwrap()),
                        ),
                        Span::styled(
                            "-".repeat(minus),
                            Style::default().fg(removed_style.fg.unwrap()),
                        ),
                        Span::raw(" ".repeat(max_bar.saturating_sub(cells))),
                    ]),
                    Some(index),
                )
            } else {
                push(
                    Line::styled(
                        fit(
                            &format!("{} {name}  {total}", if file.collapsed { "+" } else { "−" }),
                            width,
                        ),
                        style,
                    ),
                    Some(index),
                )
            };
            layout.headers.push(header);
            if file.collapsed {
                continue;
            }
            let digits = file
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Pair { old, new, .. } => Some(
                        old.as_ref()
                            .map_or(0, |cell| cell.number)
                            .max(new.as_ref().map_or(0, |cell| cell.number)),
                    ),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                .to_string()
                .len();
            for row in &file.rows {
                match row {
                    Row::Hunk => {
                        push(Line::styled("─".repeat(width), muted), Some(index));
                    }
                    Row::Note(text) => {
                        for line in wrap(text, width) {
                            push(Line::styled(line, muted), Some(index));
                        }
                    }
                    Row::Pair { old, new, changed } if width >= 24 => {
                        let left_width = (width - 3) / 2;
                        let right_width = width - left_width - 3;
                        let left = pane(
                            old.as_ref(),
                            left_width,
                            digits,
                            *changed,
                            false,
                            muted,
                            removed_style,
                        );
                        let right = pane(
                            new.as_ref(),
                            right_width,
                            digits,
                            *changed,
                            true,
                            muted,
                            added_style,
                        );
                        for row in 0..left.len().max(right.len()) {
                            let mut spans = left
                                .get(row)
                                .cloned()
                                .unwrap_or_else(|| vec![Span::raw(" ".repeat(left_width))]);
                            spans.push(Span::styled(" │ ", muted));
                            spans.extend(
                                right
                                    .get(row)
                                    .cloned()
                                    .unwrap_or_else(|| vec![Span::raw(" ".repeat(right_width))]),
                            );
                            push(Line::from(spans), Some(index));
                        }
                    }
                    Row::Pair { old, new, changed } => {
                        let cells = if *changed {
                            vec![
                                (old.as_ref(), "-", removed_style),
                                (new.as_ref(), "+", added_style),
                            ]
                        } else {
                            vec![(old.as_ref(), " ", Style::default())]
                        };
                        for (cell, marker, style) in cells {
                            if let Some(cell) = cell {
                                for text in
                                    wrap(&format!("{marker}{} {}", cell.number, cell.text), width)
                                {
                                    push(Line::styled(text, style), Some(index));
                                }
                            }
                        }
                    }
                }
            }
        }
        *self.layout.borrow_mut() = layout;
        Text::from(lines)
    }
}
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if Line::raw(format!("{out}{ch}")).width() > width {
            break;
        }
        out.push(ch);
    }
    out
}
fn pad(text: &str, width: usize) -> String {
    let text = fit(text, width);
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(Line::raw(&text).width()))
    )
}
fn pane(
    cell: Option<&Cell>,
    width: usize,
    digits: usize,
    changed: bool,
    added: bool,
    muted: Style,
    changed_style: Style,
) -> Vec<Vec<Span<'static>>> {
    let Some(cell) = cell else {
        return vec![vec![Span::raw(" ".repeat(width))]];
    };
    let gutter = (digits + 2).min(width.saturating_sub(1));
    let body_width = width.saturating_sub(gutter).max(1);
    let style = if changed {
        changed_style
    } else {
        Style::default()
    };
    wrap(&cell.text, body_width)
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let number = if index == 0 {
                format!(
                    "{:>digits$}{} ",
                    cell.number,
                    if changed {
                        if added {
                            "+"
                        } else {
                            "-"
                        }
                    } else {
                        " "
                    }
                )
            } else {
                " ".repeat(gutter)
            };
            vec![
                Span::styled(pad(&number, gutter), muted),
                Span::styled(pad(&text, body_width), style),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const PATCH: &str = "commit abcdef\nAuthor: Viewer\n\n    Example change\n---\n code.py | 5 +++--\n 3 files changed, 3 insertions(+), 3 deletions(-)\n\ndiff --git a/code.py b/code.py\nindex 0000000..1111111 100644\n--- a/code.py\n+++ b/code.py\n@@ -98,4 +98,5 @@ function\n context\n-old one\n-old two\n+new one\n+new two\n+new three\n tail\ndiff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n\\ No newline at end of file\n";
    fn contents(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn aligns_replacements_tracks_line_numbers_and_preserves_special_files() {
        let mut view = DiffView::parse(PATCH);
        view.fold_all(false);
        assert_eq!(view.files.len(), 3);
        assert_eq!((view.files[0].removed, view.files[0].added), (2, 3));
        let text = view.render(100, true);
        let body = contents(&text);
        assert!(!body
            .lines()
            .any(|line| line.starts_with("OLD") && line.contains("NEW")));
        assert!(!body.contains("files changed"));
        assert!(!body.contains("code.py | 5 +++--\n 3"));
        assert!(body
            .lines()
            .any(|line| line.starts_with("− code.py ") && line.contains("   5 +++--")));
        let pair = text
            .lines
            .iter()
            .find(|line| line.to_string().contains("old one"))
            .unwrap()
            .to_string();
        assert!(pair.contains("99-") && pair.contains("99+") && pair.contains("new one"));
        let added = text
            .lines
            .iter()
            .find(|line| line.to_string().contains("new three"))
            .unwrap()
            .to_string();
        assert!(added.contains("101+"));
        assert!(body.contains("Binary files"));
        assert!(body.contains("gone.txt") && body.contains("No newline"));
        assert!(text.lines.iter().all(|line| line.width() <= 100));
        let layout = view.layout.borrow();
        let columns = layout
            .headers
            .iter()
            .map(|row| Line::from(text.lines[*row].spans[..2].to_vec()).width())
            .collect::<Vec<_>>();
        assert!(columns.iter().all(|column| *column == columns[0]));
        assert!(layout
            .headers
            .iter()
            .all(|row| text.lines[*row].width() == 100));
    }
    #[test]
    fn folding_and_file_navigation_use_rendered_rows_and_survive_resize() {
        let mut view = DiffView::parse(PATCH);
        let initial = contents(&view.render(100, true));
        assert!(!initial.contains("old one") && !initial.contains("Binary files"));
        assert_eq!(
            initial
                .lines()
                .filter(|line| line.starts_with("───"))
                .count(),
            2
        );
        view.fold_all(false);
        view.render(100, true);
        let first = view.layout.borrow().headers[0];
        assert_eq!(view.next_file(0, true), first);
        assert!(view.toggle(first + 1, true).is_none());
        assert_eq!(view.toggle(first, true), Some(first));
        let folded = contents(&view.render(60, true));
        assert!(!folded.contains("old one"));
        assert!(folded.contains("Binary files"));
        assert!(folded.contains("+ code.py"));
        let second = view.next_file(first, true);
        assert_eq!(view.next_file(second, false), first);
        view.fold_all(true);
        let all = contents(&view.render(100, false));
        assert!(!all.contains("Binary files") && !all.contains("gone\n"));
        assert!(all.contains("image.png"));
        view.fold_all(false);
        assert!(contents(&view.render(100, false)).contains("old one"));
    }
    #[test]
    fn selected_file_is_independent_of_viewport_scroll_clamping() {
        let mut view = DiffView::parse(PATCH);
        view.fold_all(false);
        view.render(100, true);
        let first = view.next_file(0, true);
        let second = view.next_file(0, true);
        assert!(second > first);
        assert_eq!(view.toggle(0, false), Some(second));
        let text = contents(&view.render(100, true));
        assert!(text.contains("old one"));
        assert!(!text.contains("Binary files"));
    }

    #[test]
    fn long_unicode_lines_wrap_without_exceeding_pane_width() {
        let source = format!(
            "diff --git a/test b/test\n--- a/test\n+++ b/test\n@@ -1 +1 @@\n-{}\n+{}\n",
            "中文 hello ".repeat(20),
            "new ".repeat(80)
        );
        let mut view = DiffView::parse(&source);
        view.fold_all(false);
        for width in [24, 50, 120] {
            let text = view.render(width, true);
            assert!(
                text.lines.iter().all(|line| line.width() <= width),
                "width {width}"
            );
            assert!(text.lines.len() > 5);
        }
        for width in [1, 8] {
            view.render(width, true);
        }
    }
}
