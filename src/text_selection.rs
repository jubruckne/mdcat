//! Mouse selection in rendered document coordinates (terminal cells, not bytes).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Text,
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn at(column: u16, row: u16, area: Rect, scroll: usize) -> Self {
        Self {
            row: scroll
                + usize::from(
                    row.saturating_sub(area.y + 1)
                        .min(area.height.saturating_sub(3)),
                ),
            column: usize::from(
                column
                    .saturating_sub(area.x + 2)
                    .min(area.width.saturating_sub(5)),
            ),
        }
    }
}

#[derive(Default)]
pub struct Selection {
    anchor: Option<Point>,
    head: Option<Point>,
    pub dragging: bool,
    moved: bool,
    rectangular: bool,
    next_scroll: Option<std::time::Instant>,
}

impl Selection {
    pub fn start(&mut self, point: Point, rectangular: bool) {
        *self = Self {
            anchor: Some(point),
            head: Some(point),
            dragging: true,
            moved: false,
            rectangular,
            next_scroll: None,
        };
    }

    pub fn extend(&mut self, point: Point) {
        if self.dragging {
            self.moved |= self.anchor != Some(point);
            self.head = Some(point);
        }
    }

    pub fn active(&self) -> bool {
        self.moved
    }

    pub fn auto_scroll(
        &mut self,
        pointer: Option<(u16, u16)>,
        area: Rect,
        scroll: usize,
        max_scroll: usize,
        now: std::time::Instant,
    ) -> usize {
        let Some((column, row)) = pointer else {
            return scroll;
        };
        if !self.dragging || area.height <= 2 || (row > area.y && row < area.bottom() - 1) {
            self.next_scroll = None;
            return scroll;
        }
        if self.next_scroll.is_some_and(|deadline| now < deadline) {
            return scroll;
        }
        self.next_scroll = Some(now + std::time::Duration::from_millis(30));
        let selected_rows = match (self.anchor, self.head) {
            (Some(anchor), Some(head)) => anchor.row.abs_diff(head.row).saturating_add(1),
            _ => 1,
        };
        let step = match selected_rows {
            0..=99 => 1,
            100..=512 => 2,
            _ => 4,
        };
        let scroll = if row <= area.y {
            scroll.saturating_sub(step)
        } else {
            scroll.saturating_add(step).min(max_scroll)
        };
        self.extend(Point::at(column, row, area, scroll));
        scroll
    }

    pub fn poll_timeout(
        &self,
        idle: std::time::Duration,
        now: std::time::Instant,
    ) -> std::time::Duration {
        if self.dragging {
            if let Some(deadline) = self.next_scroll {
                return idle.min(deadline.saturating_duration_since(now));
            }
        }
        idle
    }

    // Endpoints are inclusive, like terminal mouse selection. Rectangles use
    // independent row/column bounds so dragging in any direction works.
    fn range(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let (anchor, head) = (self.anchor?, self.head?);
        if !self.moved || row < anchor.row.min(head.row) || row > anchor.row.max(head.row) {
            return None;
        }
        if self.rectangular {
            return Some((
                anchor.column.min(head.column),
                anchor.column.max(head.column) + 1,
            ));
        }
        let (start, end) = (anchor.min(head), anchor.max(head));
        Some((
            if row == start.row { start.column } else { 0 },
            if row == end.row {
                end.column + 1
            } else {
                width
            },
        ))
    }

    pub fn contents(&self, text: &Text<'_>) -> String {
        let mut rows = Vec::new();
        let (Some(anchor), Some(head)) = (self.anchor, self.head) else {
            return String::new();
        };
        for row in anchor.row.min(head.row)
            ..=anchor
                .row
                .max(head.row)
                .min(text.lines.len().saturating_sub(1))
        {
            let Some(line) = text.lines.get(row) else {
                break;
            };
            let line = line.to_string();
            let Some((start, end)) = self.range(row, line.width()) else {
                continue;
            };
            let mut column = 0;
            let mut selected = String::new();
            for grapheme in line.graphemes(true) {
                let next = column + grapheme.width();
                if column < end && next > start {
                    selected.push_str(grapheme);
                }
                column = next;
            }
            if self.rectangular {
                // Preserve the shape when rows end before the rectangle does.
                selected.extend(std::iter::repeat_n(
                    ' ',
                    end.saturating_sub(column.max(start)),
                ));
            }
            rows.push(selected);
        }
        rows.join("\n")
    }

    pub fn highlight(&self, frame: &mut Frame<'_>, text: &Text<'_>, area: Rect, scroll: usize) {
        let width = usize::from(area.width.saturating_sub(4));
        for (index, line) in text
            .lines
            .iter()
            .enumerate()
            .skip(scroll)
            .take(usize::from(area.height.saturating_sub(2)))
        {
            let Some((mut start, mut end)) = self.range(index, line.width()) else {
                continue;
            };
            // Expand to whole graphemes, including wide characters and emoji.
            let line = line.to_string();
            // Renderers may pad rows to the viewport width. Do not highlight
            // that padding or empty cells after the actual text.
            let line = line.trim_end();
            let mut column = 0;
            for grapheme in line.graphemes(true) {
                let next = column + grapheme.width();
                if column < start && next > start {
                    start = column;
                }
                if column < end && next > end {
                    end = next;
                }
                column = next;
            }
            start = start.min(width);
            end = end.min(width).min(column);
            if end > start {
                frame.buffer_mut().set_style(
                    Rect::new(
                        area.x + 2 + start as u16,
                        area.y + 1 + (index - scroll) as u16,
                        (end - start) as u16,
                        1,
                    ),
                    Style::default().add_modifier(Modifier::REVERSED),
                );
            }
        }
    }
}

pub fn copy(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    #[cfg(target_os = "macos")]
    {
        use anyhow::Context;
        let mut child = std::process::Command::new("/usr/bin/pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("Could not start pbcopy")?;
        let result = child.stdin.take().unwrap().write_all(text.as_bytes());
        let exit = child.wait()?;
        result?;
        anyhow::ensure!(exit.success(), "pbcopy exited with {exit}");
    }
    #[cfg(not(target_os = "macos"))]
    {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "\x1b]52;c;{encoded}\x07")?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(start: (usize, usize), end: (usize, usize), rectangular: bool) -> Selection {
        let mut selection = Selection::default();
        selection.start(
            Point {
                row: start.0,
                column: start.1,
            },
            rectangular,
        );
        selection.extend(Point {
            row: end.0,
            column: end.1,
        });
        selection
    }

    #[test]
    fn continuous_selection_is_direction_independent_and_preserves_blank_lines() {
        let text = Text::from("abcdef\n\nuvwxyz");
        assert_eq!(
            select((0, 2), (2, 3), false).contents(&text),
            "cdef\n\nuvwx"
        );
        assert_eq!(
            select((2, 3), (0, 2), false).contents(&text),
            "cdef\n\nuvwx"
        );
    }

    #[test]
    fn rectangles_use_independent_bounds_and_pad_short_rows() {
        let text = Text::from("abcdef\nx\nuvwxyz");
        for (start, end) in [
            ((0, 2), (2, 4)),
            ((2, 4), (0, 2)),
            ((0, 4), (2, 2)),
            ((2, 2), (0, 4)),
        ] {
            assert_eq!(select(start, end, true).contents(&text), "cde\n   \nwxy");
        }
    }

    #[test]
    fn selection_keeps_wide_and_combining_graphemes_whole() {
        let text = Text::from("a界e\u{301}👩‍💻z");
        assert_eq!(
            select((0, 2), (0, 4), false).contents(&text),
            "界e\u{301}👩‍💻"
        );
        assert_eq!(select((0, 3), (0, 4), true).contents(&text), "e\u{301}👩‍💻");
    }

    #[test]
    fn rectangle_highlight_uses_only_selected_columns_in_scrolled_nested_view() {
        use ratatui::{backend::TestBackend, Terminal};

        let text = Text::from("abcdef\nghijkl\nmnopqr\nstuvwx");
        let selection = select((0, 2), (2, 3), true);
        let mut terminal = Terminal::new(TestBackend::new(12, 8)).unwrap();
        let area = Rect::new(0, 2, 12, 5);
        terminal
            .draw(|frame| selection.highlight(frame, &text, area, 1))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..8 {
            for x in 0..12 {
                assert_eq!(
                    buffer[(x, y)].modifier.contains(Modifier::REVERSED),
                    (4..6).contains(&x) && (3..5).contains(&y),
                    "unexpected selection at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn highlights_stop_at_text_end_in_both_modes_including_padded_rows() {
        use ratatui::{backend::TestBackend, Terminal};

        let text = Text::from("abcdef\n界e\u{301}   \n\nxy");
        for rectangular in [false, true] {
            let selection = select((0, 1), (3, 8), rectangular);
            let mut terminal = Terminal::new(TestBackend::new(16, 7)).unwrap();
            let area = Rect::new(0, 0, 16, 7);
            terminal
                .draw(|frame| selection.highlight(frame, &text, area, 0))
                .unwrap();
            let buffer = terminal.backend().buffer();
            // A boundary inside the wide glyph expands to include it whole.
            let ranges = if rectangular {
                [(3, 8), (2, 5), (0, 0), (3, 4)]
            } else {
                [(3, 8), (2, 5), (0, 0), (2, 4)]
            };
            for (row, (start, end)) in ranges.into_iter().enumerate() {
                for x in 0..16 {
                    assert_eq!(
                        buffer[(x, row as u16 + 1)]
                            .modifier
                            .contains(Modifier::REVERSED),
                        (start..end).contains(&x),
                        "unexpected highlight at ({x}, {row}), rectangular={rectangular}"
                    );
                }
            }
        }
    }

    #[test]
    fn stationary_pointer_scrolls_on_ticks_in_both_modes_and_stops_on_release() {
        let area = Rect::new(0, 2, 20, 6);
        let now = std::time::Instant::now();
        for rectangular in [false, true] {
            let mut selection = select((10, 2), (13, 3), rectangular);
            let pointer = Some((5, 7));
            let mut scroll = selection.auto_scroll(pointer, area, 10, 100, now);
            assert_eq!(scroll, 11);
            scroll = selection.auto_scroll(
                pointer,
                area,
                scroll,
                100,
                now + std::time::Duration::from_millis(29),
            );
            assert_eq!(scroll, 11);
            for (tick, expected) in [(1, 12), (2, 13), (3, 14)] {
                scroll = selection.auto_scroll(
                    pointer,
                    area,
                    scroll,
                    100,
                    now + std::time::Duration::from_millis(tick * 30),
                );
                assert_eq!(scroll, expected);
                assert_eq!(
                    selection.head.unwrap(),
                    Point {
                        row: scroll + 3,
                        column: 3
                    }
                );
            }
            selection.dragging = false;
            assert_eq!(
                selection.auto_scroll(
                    pointer,
                    area,
                    scroll,
                    100,
                    now + std::time::Duration::from_secs(1)
                ),
                scroll
            );
        }
    }

    #[test]
    fn polling_wakes_for_scroll_deadline_without_delaying_other_refreshes() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let mut selection = select((0, 2), (3, 3), false);
        selection.auto_scroll(Some((5, 7)), Rect::new(0, 2, 20, 6), 0, 100, now);
        let later = now + Duration::from_millis(15);
        assert_eq!(
            selection.poll_timeout(Duration::from_millis(100), later),
            Duration::from_millis(15)
        );
        assert_eq!(
            selection.poll_timeout(Duration::from_millis(15), later),
            Duration::from_millis(15)
        );
        assert_eq!(
            selection.poll_timeout(Duration::from_millis(100), now + Duration::from_millis(30)),
            Duration::ZERO
        );
        selection.dragging = false;
        assert_eq!(
            selection.poll_timeout(Duration::from_millis(100), later),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn auto_scroll_stops_inside_viewport_and_clamps_at_both_document_ends() {
        let area = Rect::new(0, 2, 20, 6);
        let now = std::time::Instant::now();
        let mut selection = select((0, 2), (3, 3), false);
        assert_eq!(
            selection.auto_scroll(Some((5, 7)), area, 100, 100, now),
            100
        );
        assert_eq!(selection.auto_scroll(Some((5, 4)), area, 20, 100, now), 20);
        assert_eq!(selection.auto_scroll(Some((5, 2)), area, 20, 100, now), 18);
        assert_eq!(
            selection.auto_scroll(
                Some((5, 2)),
                area,
                0,
                100,
                now + std::time::Duration::from_secs(1)
            ),
            0
        );
    }

    #[test]
    fn scroll_speed_tiers_cover_boundaries_in_both_modes_and_directions() {
        let area = Rect::new(0, 2, 20, 6);
        let now = std::time::Instant::now();
        for rectangular in [false, true] {
            for (rows, step) in [(1, 1), (99, 1), (100, 2), (512, 2), (513, 4), (10_000, 4)] {
                let mut down = select((0, 2), (rows - 1, 3), rectangular);
                assert_eq!(
                    down.auto_scroll(Some((5, 7)), area, rows, 20_000, now),
                    rows + step
                );
                let mut up = select((rows - 1, 2), (0, 3), rectangular);
                assert_eq!(
                    up.auto_scroll(Some((5, 2)), area, 1000, 20_000, now),
                    1000 - step
                );
            }
        }
    }

    #[test]
    fn selection_supports_documents_beyond_u16_rows() {
        let text = Text::from(vec![ratatui::text::Line::raw("abcdef"); 70_001]);
        assert_eq!(
            select((69_999, 1), (70_000, 2), true).contents(&text),
            "bc\nbc"
        );
    }

    #[test]
    fn click_does_not_select_and_coordinates_account_for_nested_scrolled_views() {
        let mut selection = Selection::default();
        selection.start(Point { row: 0, column: 0 }, false);
        assert!(!selection.active());
        assert_eq!(selection.contents(&Text::from("abc")), "");
        let area = Rect::new(0, 3, 20, 10);
        assert_eq!(
            Point::at(5, 6, area, 100),
            Point {
                row: 102,
                column: 3
            }
        );
        assert_eq!(
            Point::at(50, 50, area, 100),
            Point {
                row: 107,
                column: 15
            }
        );
    }
}
