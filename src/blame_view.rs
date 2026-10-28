//! Git blame of the displayed source snapshot, including uncommitted edits.

use anyhow::{bail, Context, Result};
use pulldown_cmark_mdcat::{ratatui::LinkRegion, Settings};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    Frame,
};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub struct BlameLine {
    pub commit: Option<String>,
    author: String,
    date: String,
    timestamp: Option<i64>,
    pub number: usize,
    text: String,
}

pub struct BlameView {
    pub lines: Vec<BlameLine>,
}

impl BlameView {
    pub fn load(path: &Path, root: &Path, source: &str) -> Result<Self> {
        let relative = path
            .strip_prefix(root)
            .context("File is outside its Git repository")?;
        let mut child = Command::new("git")
            .args(["--no-pager", "--literal-pathspecs", "-C"])
            .arg(root)
            .args([
                "blame",
                "--line-porcelain",
                "--no-textconv",
                "--contents",
                "-",
                "--",
            ])
            .arg(relative)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Could not start git blame")?;
        let mut stdin = child.stdin.take().context("Missing git blame input")?;
        let source = source.to_owned();
        let writer = std::thread::spawn(move || stdin.write_all(source.as_bytes()));
        let output = child.wait_with_output();
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("Git blame input failed"))?;
        let output = output.context("Could not read git blame")?;
        if !output.status.success() {
            bail!(
                "Git blame failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        written?;
        Self::parse(&String::from_utf8(output.stdout).context("Git blame returned invalid UTF-8")?)
    }

    fn parse(porcelain: &str) -> Result<Self> {
        let mut records = porcelain.lines();
        let mut lines = Vec::new();
        while let Some(header) = records.next() {
            let fields = header.split_whitespace().collect::<Vec<_>>();
            anyhow::ensure!(fields.len() >= 3, "Invalid git blame header");
            let hash = fields[0];
            anyhow::ensure!(
                crate::git_links::commit_id(&format!("mdcat-commit:{hash}")).is_some(),
                "Invalid git blame commit"
            );
            let commit = (!hash.bytes().all(|b| b == b'0')).then(|| hash.to_owned());
            let number = fields[2]
                .parse::<usize>()
                .context("Invalid git blame line number")?;
            let mut author = String::new();
            let mut timestamp = None;
            let mut timezone = String::new();
            let mut text = None;
            for field in records.by_ref() {
                if let Some(value) = field.strip_prefix('\t') {
                    text = Some(value.to_owned());
                    break;
                }
                if let Some(value) = field.strip_prefix("author ") {
                    author = clean(value);
                }
                if let Some(value) = field.strip_prefix("author-time ") {
                    timestamp = value.parse::<i64>().ok();
                }
                if let Some(value) = field.strip_prefix("author-tz ") {
                    timezone = value.to_owned();
                }
            }
            let text = text.context("Missing git blame source line")?;
            anyhow::ensure!(
                number == lines.len() + 1,
                "Git blame source lines are out of order"
            );
            let date = timestamp
                .and_then(|time| {
                    let date = chrono::DateTime::from_timestamp(time, 0)?;
                    let zone = parse_timezone(&timezone)?;
                    Some(date.with_timezone(&zone).format("%Y-%m-%d").to_string())
                })
                .unwrap_or_default();
            if commit.is_none() {
                author = "Working tree".into();
            }
            lines.push(BlameLine {
                commit,
                author,
                date,
                timestamp,
                number,
                text,
            });
        }
        Ok(Self { lines })
    }

    pub fn targets(&self) -> Vec<String> {
        let mut targets = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in &self.lines {
            if let Some(id) = &line.commit {
                if seen.insert(id) {
                    targets.push(format!("mdcat-commit:{id}"));
                }
            }
        }
        targets
    }

    pub fn highlight_commit(
        &self,
        frame: &mut Frame<'_>,
        text: &Text<'_>,
        area: Rect,
        scroll: usize,
        target: Option<&str>,
        style: Style,
    ) {
        let Some(commit) = target.and_then(crate::git_links::commit_id) else {
            return;
        };
        let width = usize::from(area.width.saturating_sub(4));
        for (index, line) in self
            .lines
            .iter()
            .enumerate()
            .skip(scroll)
            .take(usize::from(area.height.saturating_sub(2)))
        {
            if line.commit.as_deref() != Some(commit) {
                continue;
            }
            let Some(rendered) = text.lines.get(index) else {
                continue;
            };
            let mut start = 0;
            for span in &rendered.spans {
                start += span.width();
                if span.content.ends_with(" │ ") {
                    break;
                }
            }
            let end = rendered.to_string().trim_end().width().min(width);
            if end > start {
                frame.buffer_mut().set_style(
                    Rect::new(
                        area.x + 2 + start as u16,
                        area.y + 1 + (index - scroll) as u16,
                        (end - start) as u16,
                        1,
                    ),
                    style,
                );
            }
        }
    }

    pub fn render(
        &self,
        settings: &Settings<'_>,
        language: &str,
        width: usize,
        tabs: u16,
    ) -> Result<(Text<'static>, Vec<LinkRegion>)> {
        let now = chrono::Utc::now().timestamp();
        let hash_style = Style::default().fg(if settings.theme.is_dark {
            Color::Rgb(145, 172, 238)
        } else {
            Color::Rgb(45, 75, 155)
        });
        let digits = self.lines.len().max(1).to_string().len();
        let author_width = self
            .lines
            .iter()
            .map(|line| line.author.width())
            .max()
            .unwrap_or(0)
            .min(16);
        let syntax = settings
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| settings.syntax_set.find_syntax_plain_text());
        let mut highlighter = settings
            .syntax_theme
            .as_ref()
            .map(|theme| syntect::easy::HighlightLines::new(syntax, theme));
        let mut rows = Vec::new();
        let mut links = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            let metadata = age_style(
                line.timestamp,
                line.commit.is_none(),
                settings.theme.is_dark,
                now,
            );
            let mut spans = Vec::new();
            if width >= digits + 14 {
                let hash = line.commit.as_ref().map_or("working", |id| &id[..8]);
                spans.push(Span::styled(format!("{:8} ", fit(hash, 8)), hash_style));
                if let Some(id) = &line.commit {
                    links.push(LinkRegion {
                        line: row,
                        start: 0,
                        end: 8,
                        target: format!("mdcat-commit:{id}"),
                    });
                }
            }
            if width >= 50 {
                let author = fit(&line.author, author_width);
                spans.push(Span::styled(
                    format!(
                        "{author}{} ",
                        " ".repeat(author_width.saturating_sub(author.width()))
                    ),
                    metadata,
                ));
            }
            if width >= 80 {
                spans.push(Span::styled(
                    format!(
                        "{} ",
                        if line.commit.is_some() {
                            line.date.as_str()
                        } else {
                            "          "
                        }
                    ),
                    metadata,
                ));
            }
            if width >= digits + 4 {
                spans.push(Span::styled(
                    format!("{:>digits$} │ ", line.number),
                    metadata,
                ));
            }
            let prefix_width = Line::from(spans.clone()).width();
            let remaining = width.saturating_sub(prefix_width);
            let source = pulldown_cmark_mdcat::expand_tabs(&line.text, tabs.max(1));
            let source = clean(&source);
            // Highlight the complete line to preserve multiline syntax state,
            // then clip the displayed text on grapheme boundaries.
            if let Some(highlighter) = highlighter.as_mut() {
                let input = format!("{source}\n");
                let mut remaining = remaining;
                for (style, text) in highlighter.highlight_line(&input, settings.syntax_set)? {
                    let full = text.trim_end_matches('\n');
                    let text = fit(full, remaining);
                    let clipped = text.len() < full.len();
                    remaining = remaining.saturating_sub(text.width());
                    spans.push(Span::styled(
                        text,
                        Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        )),
                    ));
                    if clipped {
                        break;
                    }
                }
            } else {
                spans.push(Span::raw(fit(&source, remaining)));
            }
            rows.push(Line::from(spans));
        }
        Ok((Text::from(rows), links))
    }
}

pub(crate) fn age_style(timestamp: Option<i64>, working: bool, dark: bool, now: i64) -> Style {
    if working {
        return Style::default().fg(if dark {
            Color::Rgb(245, 130, 180)
        } else {
            Color::Rgb(155, 35, 95)
        });
    }
    let Some(timestamp) = timestamp else {
        return Style::default().fg(if dark {
            Color::Rgb(150, 150, 150)
        } else {
            Color::Rgb(100, 100, 100)
        });
    };
    let age = now.saturating_sub(timestamp).max(0);
    let thresholds = [1, 2, 3, 4, 7, 10, 14, 30, 99_999_999];
    let index = thresholds
        .iter()
        .position(|days| age < days * 86_400)
        .unwrap_or(thresholds.len() - 1);
    let palette = if dark {
        [
            (255, 128, 128),
            (250, 160, 120),
            (245, 181, 110),
            (235, 201, 115),
            (197, 205, 125),
            (154, 204, 150),
            (125, 199, 183),
            (133, 178, 225),
            (139, 148, 166),
        ]
    } else {
        [
            (177, 50, 55),
            (174, 70, 30),
            (160, 90, 15),
            (135, 105, 10),
            (100, 112, 20),
            (55, 115, 55),
            (20, 110, 100),
            (55, 90, 155),
            (100, 105, 115),
        ]
    };
    let (r, g, b) = palette[index];
    Style::default().fg(Color::Rgb(r, g, b))
}

fn fit(text: &str, width: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|g| {
            used += g.width();
            used <= width
        })
        .collect()
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

fn parse_timezone(zone: &str) -> Option<chrono::FixedOffset> {
    let bytes = zone.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let hours = zone[1..3].parse::<i32>().ok()?;
    let minutes = zone[3..].parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    chrono::FixedOffset::east_opt(
        (hours * 3600 + minutes * 60) * if bytes[0] == b'-' { -1 } else { 1 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_bands_use_strict_day_boundaries_in_both_themes() {
        let now = 1_800_000_000;
        for dark in [false, true] {
            let mut lower = 0;
            let mut previous = None;
            for upper in [1, 2, 3, 4, 7, 10, 14, 30, 99_999_999] {
                let start = age_style(Some(now - lower * 86_400), false, dark, now);
                let end = age_style(Some(now - upper * 86_400 + 1), false, dark, now);
                assert_eq!(start, end, "same age band {lower}..{upper} days");
                if let Some(previous) = previous {
                    assert_ne!(start, previous, "new band at {lower} days");
                }
                previous = Some(start);
                lower = upper;
            }
        }
    }

    #[test]
    fn age_colors_distinguish_recent_old_and_working_lines_in_both_themes() {
        let now = 1_800_000_000;
        for dark in [false, true] {
            let recent = age_style(Some(now), false, dark, now);
            let old = age_style(Some(now - 1095 * 86_400), false, dark, now);
            let working = age_style(None, true, dark, now);
            assert_ne!(recent, old);
            assert_ne!(recent, working);
            assert_ne!(old, working);
            assert_eq!(recent, age_style(Some(now + 86_400), false, dark, now));
            assert_eq!(
                old,
                age_style(Some(now - 10_000 * 86_400), false, dark, now)
            );
        }
    }

    #[test]
    fn hover_highlights_all_matching_source_rows_and_clears_when_pointer_leaves() {
        use ratatui::{backend::TestBackend, Terminal};

        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let zero = "0".repeat(40);
        let records = [(&a, "first"), (&b, "second"), (&a, "third"), (&zero, "working")]
            .into_iter().enumerate().map(|(index, (hash, source))| format!(
                "{hash} 1 {} 1\nauthor julia\nauthor-time 0\nauthor-tz +0000\nfilename test.txt\n\t{source}\n", index + 1
            )).collect::<String>();
        let view = BlameView::parse(&records).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let (text, _) = view.render(&settings, "", 80, 4).unwrap();
        let prefix = text.lines[0].to_string().find(" │ ").unwrap() + " │ ".len();
        // The separator is Unicode: convert the byte prefix to terminal cells.
        let start = text.lines[0].to_string()[..prefix].width();
        let area = Rect::new(0, 2, 84, 6);
        let target = format!("mdcat-commit:{a}");
        for scroll in [0, 1] {
            let mut terminal = Terminal::new(TestBackend::new(84, 8)).unwrap();
            terminal
                .draw(|frame| {
                    view.highlight_commit(
                        frame,
                        &text,
                        area,
                        scroll,
                        Some(&target),
                        Style::default().bg(Color::Blue),
                    )
                })
                .unwrap();
            for y in 0..8 {
                for x in 0..84 {
                    let row = usize::from(y).checked_sub(3).map(|row| row + scroll);
                    let expected = matches!(row, Some(0 | 2))
                        && (start + 2..start + 7).contains(&usize::from(x));
                    assert_eq!(
                        terminal.backend().buffer()[(x, y)].bg == Color::Blue,
                        expected,
                        "({x}, {y}), scroll {scroll}"
                    );
                }
            }
            terminal
                .draw(|frame| {
                    view.highlight_commit(
                        frame,
                        &text,
                        area,
                        scroll,
                        None,
                        Style::default().bg(Color::Blue),
                    )
                })
                .unwrap();
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg != Color::Blue));
        }
    }

    #[test]
    fn blame_tracks_committed_and_unsaved_source_snapshot_without_changing_file() {
        let (dir, id) = crate::git_links::tests::repository();
        let path = dir.path().join("example.txt");
        let view = BlameView::load(&path, dir.path(), "after\n追加\n").unwrap();
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.lines[0].commit.as_deref(), Some(id.as_str()));
        assert_eq!(view.lines[0].author, "Viewer Test");
        assert_eq!(view.lines[0].date.len(), 10);
        assert_eq!(view.lines[1].commit, None);
        assert_eq!(view.lines[1].author, "Working tree");
        assert_eq!(view.lines[1].text, "追加");
        assert_eq!(view.targets(), vec![format!("mdcat-commit:{id}")]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
        assert!(BlameView::load(&path, dir.path(), "")
            .unwrap()
            .lines
            .is_empty());
    }

    #[test]
    fn untracked_file_returns_git_error() {
        let (dir, _) = crate::git_links::tests::repository();
        let path = dir.path().join("untracked.txt");
        std::fs::write(&path, "new\n").unwrap();
        let error = BlameView::load(&path, dir.path(), "new\n").err().unwrap();
        assert!(error.to_string().contains("Git blame failed"));
    }

    #[test]
    fn parser_handles_author_timezone_sha256_and_rejects_incomplete_records() {
        let hash = "a".repeat(64);
        let record = format!("{hash} 1 1 1\nauthor A界\nauthor-time 0\nauthor-tz -0200\nsummary subject\nfilename strange name.txt\n\tprint(\"界\")\n");
        let view = BlameView::parse(&record).unwrap();
        assert_eq!(view.lines[0].date, "1969-12-31");
        assert_eq!(view.lines[0].author, "A界");
        assert_eq!(view.lines[0].commit.as_deref(), Some(hash.as_str()));
        assert!(BlameView::parse(&format!("{hash} 1 1\nauthor Missing content\n")).is_err());
        assert!(BlameView::parse("not a header\n").is_err());
    }

    #[test]
    fn rendering_fits_narrow_views_and_preserves_line_numbers_and_commit_hitboxes() {
        let (dir, id) = crate::git_links::tests::repository();
        let view = BlameView::load(
            &dir.path().join("example.txt"),
            dir.path(),
            "after\n\t界e\u{301}👩‍💻\n",
        )
        .unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: Some(
                two_face::theme::extra()
                    .get(two_face::theme::EmbeddedThemeName::InspiredGithub)
                    .clone(),
            ),
        };
        for width in [0, 1, 6, 15, 16, 30, 50, 80, 120] {
            let (text, links) = view.render(&settings, "python", width, 4).unwrap();
            assert_eq!(text.lines.len(), 2);
            assert!(
                text.lines.iter().all(|line| line.width() <= width),
                "width {width}"
            );
            for link in links {
                assert_eq!(link.target, format!("mdcat-commit:{id}"));
                assert_eq!(link.line, 0);
                assert_eq!(&text.lines[0].to_string()[link.start..link.end], &id[..8]);
            }
            if width >= 80 {
                for line in &text.lines {
                    assert_eq!(line.spans[1].style, line.spans[2].style);
                    assert_eq!(line.spans[2].style, line.spans[3].style);
                }
                assert!(text.lines[0].to_string().contains("Viewer Test"));
                assert!(text.lines[0].to_string().ends_with("1 │ after"));
                assert!(text.lines[1].to_string().contains("Working tree"));
                assert!(text.lines[1].to_string().ends_with("2 │     界e\u{301}👩‍💻"));
            }
        }
    }
}
