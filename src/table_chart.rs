//! Hover previews for numeric columns in rendered Markdown tables.

use pulldown_cmark::{Event, Tag, TagEnd};
use pulldown_cmark_mdcat::ratatui::TableBounds;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::HashMap;
use tui_chart::{Axis, BarChart, BarGroup, BarOrientation};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy)]
pub struct Sort {
    pub column: usize,
    pub descending: bool,
}

fn is_table_rule(line: &Line<'_>) -> bool {
    let s = line.to_string();
    let s = s.trim();
    s.chars().count() >= 4 && s.chars().all(|c| matches!(c, '─' | '━' | '-'))
}

/// Header lines to pin when the first visible line is within a table body.
pub fn pinned_header(
    text: &Text<'_>,
    bounds: &TableBounds,
    scroll: usize,
    max_lines: usize,
) -> Option<(usize, usize)> {
    if max_lines == 0 {
        return None;
    }
    let mut first = 0;
    while first < bounds.len() {
        if bounds[first].1.is_empty() {
            first += 1;
            continue;
        }
        let mut last = first;
        while last + 1 < bounds.len()
            && bounds[last + 1].0 - bounds[last].0 <= 2
            && bounds[last + 1].1 == bounds[first].1
        {
            last += 1;
        }
        let mut end = first;
        while end < last && bounds[end + 1].0 == bounds[end].0 + 1 {
            end += 1;
        }
        if end < last && scroll > bounds[end].0 && scroll <= bounds[last].0 {
            let head = bounds[first].0;
            let tail = bounds[end].0;
            let start = head
                .checked_sub(1)
                .filter(|&line| text.lines.get(line).is_some_and(is_table_rule))
                .unwrap_or(head);
            let finish = (tail + 1 < text.lines.len() && is_table_rule(&text.lines[tail + 1]))
                .then_some(tail + 1)
                .unwrap_or(tail);
            let lines = finish - start + 1;
            return (lines <= max_lines).then_some((start, lines));
        }
        first = last + 1;
    }
    None
}

/// The table's closing rule is the only part left at the top of the viewport.
pub fn is_closing_rule_at_top(text: &Text<'_>, bounds: &TableBounds, scroll: usize) -> bool {
    if !text.lines.get(scroll).is_some_and(is_table_rule) {
        return false;
    }
    let Some(index) = bounds
        .iter()
        .position(|(row, ranges)| row.saturating_add(1) == scroll && !ranges.is_empty())
    else {
        return false;
    };
    let ranges = &bounds[index].1;
    !bounds
        .get(index + 1)
        .is_some_and(|(row, next)| *row <= scroll.saturating_add(1) && next == ranges)
}

/// Find the Markdown table and column at a rendered header position.
pub fn header_at(bounds: &TableBounds, row: usize, col: usize) -> Option<(usize, usize)> {
    let mut table = 0;
    for (i, (line, ranges)) in bounds.iter().enumerate() {
        let first = i == 0 || *line - bounds[i - 1].0 > 2 || *ranges != bounds[i - 1].1;
        if first {
            let mut end = i;
            while end + 1 < bounds.len()
                && bounds[end + 1].0 == bounds[end].0 + 1
                && bounds[end + 1].1 == *ranges
            {
                end += 1;
            }
            if row >= *line && row <= bounds[end].0 {
                return ranges
                    .iter()
                    .position(|&(left, right)| col >= left && col < right)
                    .map(|column| (table, column));
            }
            table += 1;
        }
    }
    None
}

/// Reorder rows of detected aligned text tables and move link coordinates with them.
pub fn sort_text_tables(
    text: &mut Text<'static>,
    links: &mut [pulldown_cmark_mdcat::ratatui::LinkRegion],
    bounds: &TableBounds,
    sorts: &HashMap<usize, Sort>,
) {
    let mut first = 0;
    let mut table = 0;
    while first < bounds.len() {
        let mut last = first;
        while last + 1 < bounds.len()
            && bounds[last + 1].0 - bounds[last].0 <= 2
            && bounds[last + 1].1 == bounds[first].1
        {
            last += 1;
        }
        if let Some(sort) = sorts.get(&table).filter(|_| last > first) {
            let ranges = &bounds[first].1;
            if let Some(&(left, right)) = ranges.get(sort.column) {
                let slots: Vec<_> = bounds[first + 1..=last]
                    .iter()
                    .map(|(row, _)| *row)
                    .collect();
                let mut rows: Vec<_> = slots
                    .iter()
                    .map(|&row| (row, cell(&text.lines[row], left, right)))
                    .collect();
                let numeric = rows.iter().all(|(_, value)| number(value).is_some());
                rows.sort_by(|a, b| {
                    let ordering = if numeric {
                        number(&a.1).unwrap().total_cmp(&number(&b.1).unwrap())
                    } else {
                        a.1.to_lowercase().cmp(&b.1.to_lowercase())
                    };
                    if sort.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                });
                let lines: Vec<_> = rows
                    .iter()
                    .map(|(row, _)| text.lines[*row].clone())
                    .collect();
                for (&slot, line) in slots.iter().zip(lines) {
                    text.lines[slot] = line;
                }
                for link in links.iter_mut() {
                    if let Some(index) = rows.iter().position(|(row, _)| *row == link.line) {
                        link.line = slots[index];
                    }
                }
            }
        }
        first = last + 1;
        table += 1;
    }
}

/// Sort complete Markdown row event sequences, retaining inline links and styling.
pub fn sort_markdown<'a>(events: Vec<Event<'a>>, sorts: &HashMap<usize, Sort>) -> Vec<Event<'a>> {
    if sorts.is_empty() {
        return events;
    }
    let mut output = Vec::with_capacity(events.len());
    let mut events = events.into_iter();
    let mut table_index = 0;
    while let Some(event) = events.next() {
        if !matches!(event, Event::Start(Tag::Table(_))) {
            output.push(event);
            continue;
        }
        output.push(event);
        let sort = sorts.get(&table_index).copied();
        table_index += 1;
        let mut rows: Vec<(String, Vec<Event<'a>>)> = Vec::new();
        while let Some(event) = events.next() {
            if matches!(event, Event::End(TagEnd::Table)) {
                if let Some(sort) = sort {
                    let numeric = rows.iter().all(|(key, _)| number(key).is_some());
                    rows.sort_by(|a, b| {
                        let ordering = if numeric {
                            number(&a.0).unwrap().total_cmp(&number(&b.0).unwrap())
                        } else {
                            a.0.to_lowercase().cmp(&b.0.to_lowercase())
                        };
                        if sort.descending {
                            ordering.reverse()
                        } else {
                            ordering
                        }
                    });
                }
                for (_, row) in rows.drain(..) {
                    output.extend(row);
                }
                output.push(event);
                break;
            }
            if matches!(event, Event::Start(Tag::TableRow)) {
                let mut row = vec![event];
                let mut key = String::new();
                let mut column = 0;
                let mut inside = false;
                for event in events.by_ref() {
                    match &event {
                        Event::Start(Tag::TableCell) => inside = true,
                        Event::End(TagEnd::TableCell) => {
                            inside = false;
                            column += 1;
                        }
                        Event::Text(value) | Event::Code(value)
                            if inside && sort.is_some_and(|s| s.column == column) =>
                        {
                            key.push_str(value)
                        }
                        _ => {}
                    }
                    let end = matches!(event, Event::End(TagEnd::TableRow));
                    row.push(event);
                    if end {
                        break;
                    }
                }
                rows.push((key.trim().to_owned(), row));
            } else {
                output.push(event);
            }
        }
    }
    output
}

struct Chart {
    title: String,
    rows: Vec<(String, String, Option<f64>)>,
    source_rows: Vec<(usize, usize)>,
    row_headers: Option<(usize, usize, Vec<usize>)>,
}

struct AllChart {
    columns: Vec<String>,
    rows: Vec<(String, Vec<(String, Option<f64>)>)>,
    source_rows: Vec<(usize, usize)>,
}

const CHART_COLORS: [Color; 9] = [
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Green,
    Color::Blue,
    Color::Red,
    Color::Rgb(230, 145, 65),
    Color::Rgb(105, 170, 130),
    Color::Rgb(180, 145, 220),
];
const CHART_IDS: &[u8] = b"123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn all_series_count(chart: &AllChart, area: Rect, no_colour: bool) -> usize {
    let limit = if no_colour {
        CHART_IDS.len()
    } else {
        usize::MAX
    };
    chart
        .rows
        .len()
        .min(usize::from(area.height.saturating_sub(2)).saturating_sub(5))
        .min(limit)
}

fn cell_lines(text: &Text<'_>, first: usize, last: usize, left: usize, right: usize) -> String {
    (first..=last)
        .filter_map(|line| text.lines.get(line))
        .map(|line| cell(line, left, right))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn logical_rows(
    bounds: &TableBounds,
    start: usize,
    last: usize,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut index = start;
    while index <= last {
        let row = bounds[index].0;
        let end = table_rows
            .iter()
            .find(|&&(first, end)| first <= row && row <= end)
            .map_or(row, |&(_, end)| end.min(bounds[last].0));
        result.push((row, end));
        index += 1;
        while index <= last && bounds[index].0 <= end {
            index += 1;
        }
    }
    result
}

impl AllChart {
    fn transpose(&self) -> Self {
        Self {
            columns: self.rows.iter().map(|(label, _)| label.clone()).collect(),
            rows: self
                .columns
                .iter()
                .enumerate()
                .map(|(column, name)| {
                    (
                        name.clone(),
                        self.rows
                            .iter()
                            .map(|(_, values)| values[column].clone())
                            .collect(),
                    )
                })
                .collect(),
            source_rows: Vec::new(),
        }
    }
}

fn row_chart_at(
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    row: usize,
    col: usize,
) -> Option<Chart> {
    let (body_first, body_last) = table_rows
        .iter()
        .copied()
        .find(|&(first, last)| row >= first && row <= last)
        .unwrap_or((row, row));
    let index = bounds.iter().position(|(line, ranges)| {
        *line == body_first
            && ranges
                .first()
                .is_some_and(|&(left, right)| col >= left && col < right)
    })?;
    let ranges = &bounds[index].1;
    let mut first = index;
    while first > 0 && bounds[first].0 - bounds[first - 1].0 <= 2 && bounds[first - 1].1 == *ranges
    {
        first -= 1;
    }
    let mut header_end = first;
    while header_end + 1 < bounds.len()
        && bounds[header_end + 1].0 == bounds[header_end].0 + 1
        && bounds[header_end + 1].1 == *ranges
    {
        header_end += 1;
    }
    if index <= header_end {
        return None;
    }
    let body_cell = |left, right| cell_lines(text, body_first, body_last, left, right);
    let title = body_cell(ranges[0].0, ranges[0].1);
    let cells: Vec<_> = ranges
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(column, &(left, right))| {
            let label = cell_lines(text, bounds[first].0, bounds[header_end].0, left, right);
            let raw = body_cell(left, right);
            let value = number(&raw);
            (!label.is_empty()).then_some((column, (label, raw, value)))
        })
        .collect();
    let (columns, rows): (Vec<_>, Vec<_>) = cells.into_iter().unzip();
    (!title.is_empty() && rows.iter().filter(|(_, _, value)| value.is_some()).count() >= 2)
        .then_some(Chart {
            title,
            rows,
            source_rows: Vec::new(),
            row_headers: Some((bounds[first].0, bounds[header_end].0, columns)),
        })
}

/// Match visible column headers to the bars in a row chart.
pub fn highlight_row_chart_headers(
    frame: &mut ratatui::Frame<'_>,
    full_area: Rect,
    body_area: Rect,
    body_scroll: usize,
    pinned: Option<(usize, usize)>,
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    chart_area: Rect,
    chart_scroll: usize,
    mouse: (u16, u16),
    no_colour: bool,
) {
    if mouse.0 < chart_area.x + 2
        || mouse.0 >= chart_area.right().saturating_sub(2)
        || mouse.1 <= chart_area.y
        || mouse.1 >= chart_area.bottom().saturating_sub(1)
        || chart_area.width.saturating_sub(2) < 24
    {
        return;
    }
    let row = chart_scroll + usize::from(mouse.1 - chart_area.y - 1);
    let col = usize::from(mouse.0 - chart_area.x - 2);
    let Some(chart) = row_chart_at(text, bounds, table_rows, row, col) else {
        return;
    };
    let Some((first, last, columns)) = &chart.row_headers else {
        return;
    };
    let safe_top = last
        .checked_sub(chart_scroll)
        .filter(|&row| row < usize::from(chart_area.height.saturating_sub(2)))
        .map_or(chart_area.y + 1, |row| chart_area.y + 2 + row as u16);
    let height = u16::try_from(chart.rows.len().saturating_add(4).max(6))
        .unwrap_or(u16::MAX)
        .min(
            chart_area
                .bottom()
                .saturating_sub(1)
                .saturating_sub(safe_top),
        );
    if height < 6 {
        return;
    }
    let visible = usize::from(height - 4);
    for line in *first..=*last {
        let (area, y) = if let Some((start, _)) =
            pinned.filter(|&(start, count)| line >= start && line < start + count)
        {
            (full_area, full_area.y + 1 + (line - start) as u16)
        } else if line >= body_scroll
            && line - body_scroll < usize::from(body_area.height.saturating_sub(2))
        {
            (body_area, body_area.y + 1 + (line - body_scroll) as u16)
        } else {
            continue;
        };
        let Some((_, ranges)) = bounds.iter().find(|(row, _)| *row == line) else {
            continue;
        };
        for (index, &column) in columns.iter().take(visible).enumerate() {
            let Some(&(left, right)) = ranges.get(column) else {
                continue;
            };
            let origin = area.x.saturating_add(2);
            let x1 = origin
                .saturating_add(u16::try_from(left).unwrap_or(u16::MAX))
                .min(area.right().saturating_sub(2));
            let x2 = origin
                .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
                .min(area.right().saturating_sub(2));
            if x2 <= x1 {
                continue;
            }
            let style = if no_colour {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(CHART_COLORS[index % CHART_COLORS.len()])
            };
            frame
                .buffer_mut()
                .set_style(Rect::new(x1, y, x2 - x1, 1), style);
            if no_colour && left > 0 {
                frame.buffer_mut()[(x1 - 1, y)]
                    .set_symbol(&(CHART_IDS[index % CHART_IDS.len()] as char).to_string());
            }
        }
    }
}

fn all_chart_at(
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    row: usize,
    col: usize,
) -> Option<AllChart> {
    let index = bounds.iter().position(|(line, ranges)| {
        *line == row
            && ranges
                .first()
                .is_some_and(|&(left, right)| col >= left && col < right)
    })?;
    let ranges = &bounds[index].1;
    let mut first = index;
    while first > 0 && bounds[first].0 - bounds[first - 1].0 <= 2 && bounds[first - 1].1 == *ranges
    {
        first -= 1;
    }
    let mut header_end = first;
    while header_end + 1 < bounds.len()
        && bounds[header_end + 1].0 == bounds[header_end].0 + 1
        && bounds[header_end + 1].1 == *ranges
    {
        header_end += 1;
    }
    if index > header_end {
        return None;
    }
    let mut last = header_end;
    while last + 1 < bounds.len()
        && bounds[last + 1].0 - bounds[last].0 <= 2
        && bounds[last + 1].1 == *ranges
    {
        last += 1;
    }
    if last < header_end + 2 {
        return None;
    }
    let rows = logical_rows(bounds, header_end + 1, last, table_rows);
    let mut columns = Vec::new();
    let mut values = vec![Vec::new(); rows.len()];
    for &(left, right) in ranges.iter().skip(1) {
        let name = cell_lines(text, bounds[first].0, bounds[header_end].0, left, right);
        let parsed: Vec<_> = rows
            .iter()
            .map(|&(first, last)| {
                let raw = cell_lines(text, first, last, left, right);
                let value = number(&raw);
                (raw, value)
            })
            .collect();
        if !name.is_empty()
            && parsed.len() == rows.len()
            && parsed.iter().filter(|(_, value)| value.is_some()).count() >= 2
        {
            columns.push(name);
            for (row, value) in values.iter_mut().zip(parsed) {
                row.push(value);
            }
        }
    }
    if columns.is_empty() {
        return None;
    }
    let source_rows = rows.clone();
    let rows = rows
        .iter()
        .zip(values)
        .map(|(&(first, last), values)| {
            (
                cell_lines(text, first, last, ranges[0].0, ranges[0].1),
                values,
            )
        })
        .collect();
    Some(AllChart {
        columns,
        rows,
        source_rows,
    })
}

/// Match visible row headers to their series while the combined chart is shown.
pub fn highlight_all_chart_rows(
    frame: &mut ratatui::Frame<'_>,
    body_area: Rect,
    body_scroll: usize,
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    chart_area: Rect,
    chart_scroll: usize,
    mouse: (u16, u16),
    no_colour: bool,
) {
    if mouse.0 < chart_area.x + 2
        || mouse.0 >= chart_area.right().saturating_sub(2)
        || mouse.1 <= chart_area.y
        || mouse.1 >= chart_area.bottom().saturating_sub(1)
        || chart_area.width.saturating_sub(2) < 24
    {
        return;
    }
    let row = chart_scroll + usize::from(mouse.1 - chart_area.y - 1);
    let col = usize::from(mouse.0 - chart_area.x - 2);
    let Some(chart) = all_chart_at(text, bounds, table_rows, row, col) else {
        return;
    };
    let count = all_series_count(&chart, chart_area, no_colour);
    for (index, &source_row) in chart.source_rows.iter().take(count).enumerate() {
        mark_row_header(
            frame,
            body_area,
            body_scroll,
            bounds,
            source_row,
            index,
            no_colour,
        );
    }
}

fn mark_row_header(
    frame: &mut ratatui::Frame<'_>,
    body_area: Rect,
    body_scroll: usize,
    bounds: &TableBounds,
    source_row: (usize, usize),
    index: usize,
    no_colour: bool,
) {
    for row in source_row.0..=source_row.1 {
        if row < body_scroll || row - body_scroll >= usize::from(body_area.height.saturating_sub(2))
        {
            continue;
        }
        let Some((_, ranges)) = bounds.iter().find(|(line, _)| *line == row) else {
            continue;
        };
        let Some(&(left, right)) = ranges.first() else {
            continue;
        };
        let origin = body_area.x.saturating_add(2);
        let right_edge = body_area.right().saturating_sub(2);
        let x1 = origin
            .saturating_add(u16::try_from(left).unwrap_or(u16::MAX))
            .min(right_edge);
        let x2 = origin
            .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
            .min(right_edge);
        let y = body_area.y + 1 + (row - body_scroll) as u16;
        if x2 > x1 {
            let style = if no_colour {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(CHART_COLORS[index % CHART_COLORS.len()])
            };
            frame
                .buffer_mut()
                .set_style(Rect::new(x1, y, x2 - x1, 1), style);
            if no_colour && left > 0 {
                frame.buffer_mut()[(x1 - 1, y)]
                    .set_symbol(&(CHART_IDS[index % CHART_IDS.len()] as char).to_string());
            }
        }
    }
}

/// Highlight a column from its header, or the hovered body row.
pub fn highlight_hover(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    scroll: usize,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    mouse: (u16, u16),
    no_colour: bool,
    is_dark: bool,
) {
    let (x, y) = mouse;
    if x < area.x.saturating_add(2)
        || x >= area.right().saturating_sub(2)
        || y <= area.y
        || y >= area.bottom().saturating_sub(1)
    {
        return;
    }
    let row = scroll + usize::from(y - area.y - 1);
    let col = usize::from(x - area.x - 2);
    let Some(index) = bounds.iter().position(|(line, ranges)| {
        *line == row
            && ranges
                .iter()
                .any(|&(left, right)| col >= left && col < right)
    }) else {
        return;
    };
    let ranges = &bounds[index].1;
    let column = ranges
        .iter()
        .position(|&(left, right)| col >= left && col < right)
        .unwrap();
    let mut first = index;
    let mut last = index;
    while first > 0 && bounds[first].0 - bounds[first - 1].0 <= 2 && bounds[first - 1].1 == *ranges
    {
        first -= 1;
    }
    while last + 1 < bounds.len()
        && bounds[last + 1].0 - bounds[last].0 <= 2
        && bounds[last + 1].1 == *ranges
    {
        last += 1;
    }
    let mut header_end = first;
    while header_end < last && bounds[header_end + 1].0 == bounds[header_end].0 + 1 {
        header_end += 1;
    }
    let column_hovered = index <= header_end;
    let (row_first, row_last) = table_rows
        .iter()
        .copied()
        .find(|&(first, last)| row >= first && row <= last)
        .unwrap_or((row, row));
    let style = if no_colour {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default().bg(if is_dark {
            Color::Rgb(54, 54, 54)
        } else {
            Color::Rgb(225, 225, 225)
        })
    };
    let left_edge = area.x.saturating_add(2);
    let right_edge = area.right().saturating_sub(2);
    for (line, _) in &bounds[first..=last] {
        if *line < scroll || *line - scroll >= usize::from(area.height.saturating_sub(2)) {
            continue;
        }
        let (left, right) = if column_hovered {
            ranges[column]
        } else if *line >= row_first && *line <= row_last {
            (ranges[0].0, ranges.last().unwrap().1)
        } else {
            continue;
        };
        let left = left_edge
            .saturating_add(u16::try_from(left).unwrap_or(u16::MAX))
            .min(right_edge);
        let right = left_edge
            .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
            .min(right_edge);
        if right > left {
            frame.buffer_mut().set_style(
                Rect::new(left, area.y + 1 + (*line - scroll) as u16, right - left, 1),
                style,
            );
        }
    }
}

/// Highlight the column under a pinned copy of a table header.
pub fn highlight_pinned(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    scroll: usize,
    bounds: &TableBounds,
    header: (usize, usize),
    mouse: (u16, u16),
    no_colour: bool,
    is_dark: bool,
) {
    if mouse.0 < area.x + 2 || mouse.0 >= area.right().saturating_sub(2) {
        return;
    }
    let Some(offset) = mouse
        .1
        .checked_sub(area.y + 1)
        .map(usize::from)
        .filter(|offset| *offset < header.1)
    else {
        return;
    };
    let Some(index) = bounds.iter().position(|(row, _)| *row == header.0 + offset) else {
        return;
    };
    let ranges = &bounds[index].1;
    let col = usize::from(mouse.0 - area.x - 2);
    let Some(&(left, right)) = ranges
        .iter()
        .find(|&&(left, right)| col >= left && col < right)
    else {
        return;
    };
    let mut last = index;
    while last + 1 < bounds.len()
        && bounds[last + 1].0 - bounds[last].0 <= 2
        && bounds[last + 1].1 == *ranges
    {
        last += 1;
    }
    let style = if no_colour {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default().bg(if is_dark {
            Color::Rgb(54, 54, 54)
        } else {
            Color::Rgb(225, 225, 225)
        })
    };
    let left = (area.x + 2)
        .saturating_add(u16::try_from(left).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(2));
    let right = (area.x + 2)
        .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(2));
    if right <= left {
        return;
    }
    for line in 0..header.1 {
        if bounds
            .iter()
            .any(|(row, columns)| *row == header.0 + line && columns == ranges)
        {
            frame.buffer_mut().set_style(
                Rect::new(left, area.y + 1 + line as u16, right - left, 1),
                style,
            );
        }
    }
    for (line, _) in &bounds[index..=last] {
        if *line >= scroll && *line - scroll + header.1 < usize::from(area.height.saturating_sub(2))
        {
            frame.buffer_mut().set_style(
                Rect::new(
                    left,
                    area.y + 1 + header.1 as u16 + (*line - scroll) as u16,
                    right - left,
                    1,
                ),
                style,
            );
        }
    }
}

/// Recognize aligned text tables bounded by horizontal rules, including fenced examples.
pub fn add_text_tables(text: &Text<'_>, bounds: &mut TableBounds) {
    fn columns(line: &Line<'_>) -> Vec<(usize, usize)> {
        let s = line.to_string();
        let mut starts = Vec::new();
        let mut pos = 0;
        let mut spaces = 2;
        for ch in s.chars() {
            if ch == ' ' {
                spaces += 1;
            } else {
                if spaces >= 2 {
                    starts.push(pos);
                }
                spaces = 0;
            }
            pos += ch.width().unwrap_or(0);
        }
        starts
            .iter()
            .enumerate()
            .map(|(i, &start)| (start, starts.get(i + 1).copied().unwrap_or(pos)))
            .collect()
    }
    let lines = &text.lines;
    for start in 0..lines.len().saturating_sub(4) {
        if !is_table_rule(&lines[start]) || !is_table_rule(&lines[start + 2]) {
            continue;
        }
        let mut ranges = columns(&lines[start + 1]);
        if ranges.len() < 2 {
            continue;
        }
        if let Some(last) = ranges.last_mut() {
            last.1 = last.1.max(lines[start].width());
        }
        let Some(end) = (start + 3..lines.len()).find(|&row| is_table_rule(&lines[row])) else {
            continue;
        };
        if end < start + 5
            || (start + 1..end).any(|row| bounds.iter().any(|(existing, _)| *existing == row))
        {
            continue;
        }
        bounds.push((start + 1, ranges.clone()));
        bounds.extend((start + 3..end).map(|row| (row, ranges.clone())));
    }
    bounds.sort_by_key(|(row, _)| *row);
}

fn cell(line: &Line<'_>, start: usize, end: usize) -> String {
    let mut column = 0;
    let mut result = String::new();
    for ch in line.spans.iter().flat_map(|span| span.content.chars()) {
        let width = ch.width().unwrap_or(0);
        if column >= start && column + width <= end {
            result.push(ch);
        }
        column += width;
    }
    result.trim().to_owned()
}

fn number(value: &str) -> Option<f64> {
    let token = value.split_whitespace().next()?.replace('−', "-");
    let token = token.trim_end_matches('%').replace(',', "");
    let value = token.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

fn display_value(raw: &str, value: Option<f64>) -> String {
    if value.is_some() {
        raw.to_owned()
    } else if raw.is_empty() {
        "∅".to_owned()
    } else {
        format!("∅ {raw}")
    }
}

fn chart_at(
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    row: usize,
    col: usize,
) -> Option<Chart> {
    let index = bounds.iter().position(|(line, _)| *line == row)?;
    let ranges = &bounds[index].1;
    let column = ranges.iter().position(|&(a, b)| col >= a && col < b)?;
    if column == 0 {
        return None;
    }
    let mut first = index;
    let mut last = index;
    while first > 0 && bounds[first].0 - bounds[first - 1].0 <= 2 && bounds[first - 1].1 == *ranges
    {
        first -= 1;
    }
    while last + 1 < bounds.len()
        && bounds[last + 1].0 - bounds[last].0 <= 2
        && bounds[last + 1].1 == *ranges
    {
        last += 1;
    }
    if first == last {
        return None;
    }
    let mut header_end = first;
    while header_end < last && bounds[header_end + 1].0 == bounds[header_end].0 + 1 {
        header_end += 1;
    }
    if header_end == last {
        return None;
    }
    let title = cell_lines(
        text,
        bounds[first].0,
        bounds[header_end].0,
        ranges[column].0,
        ranges[column].1,
    );
    let source_rows = logical_rows(bounds, header_end + 1, last, table_rows);
    let rows: Vec<_> = source_rows
        .iter()
        .map(|&(first, last)| {
            let label = cell_lines(text, first, last, ranges[0].0, ranges[0].1);
            let raw = cell_lines(text, first, last, ranges[column].0, ranges[column].1);
            let value = number(&raw);
            (label, raw, value)
        })
        .collect();
    (rows.iter().filter(|(_, _, value)| value.is_some()).count() >= 2 && !title.is_empty())
        .then_some(Chart {
            title,
            rows,
            source_rows,
            row_headers: None,
        })
}

/// Draw a column-header chart on the same screen lines as the table rows.
pub fn draw_column_aligned(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    scroll: usize,
    body_area: Rect,
    body_scroll: usize,
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    mouse: (u16, u16),
    no_colour: bool,
) -> bool {
    if mouse.0 < area.x + 2
        || mouse.0 >= area.right().saturating_sub(2)
        || mouse.1 <= area.y
        || mouse.1 >= area.bottom().saturating_sub(1)
    {
        return false;
    }
    let row = scroll + usize::from(mouse.1 - area.y - 1);
    let col = usize::from(mouse.0 - area.x - 2);
    if !header_at(bounds, row, col).is_some_and(|(_, column)| column > 0) {
        return false;
    }
    let Some(chart) = chart_at(text, bounds, table_rows, row, col) else {
        return false;
    };
    let body_height = usize::from(body_area.height.saturating_sub(2));
    let Some(first_line) = chart
        .source_rows
        .iter()
        .filter_map(|&(first, last)| {
            (last >= body_scroll && first < body_scroll + body_height)
                .then_some(first.max(body_scroll))
        })
        .next()
    else {
        return false;
    };
    let Some(last_line) = chart
        .source_rows
        .iter()
        .filter_map(|&(first, last)| {
            (last >= body_scroll && first < body_scroll + body_height)
                .then_some(last.min(body_scroll + body_height - 1))
        })
        .last()
    else {
        return false;
    };
    let screen_y = body_area.y + 1 + (first_line - body_scroll) as u16;
    let top = screen_y.saturating_sub(1);
    let slots = (last_line - first_line + 1).min(usize::from(
        area.bottom()
            .saturating_sub(1)
            .saturating_sub(top)
            .saturating_sub(4),
    ));
    if slots == 0 {
        return false;
    }
    let header_right = bounds
        .iter()
        .find(|(line, _)| *line == row)
        .and_then(|(_, ranges)| ranges.first())
        .map(|&(_, right)| {
            body_area
                .x
                .saturating_add(2)
                .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
        })
        .unwrap_or(area.x + 2);
    let min_left = header_right.saturating_add(1);
    let available = area.right().saturating_sub(1).saturating_sub(min_left);
    let value_width = chart
        .rows
        .iter()
        .map(|(_, raw, value)| display_value(raw, *value).width())
        .max()
        .unwrap_or(0)
        .saturating_add(if no_colour { 3 } else { 1 })
        .min(30);
    let desired = (24 + value_width + 2).max(chart.title.width() + 2).max(20);
    let width = available
        .min(70)
        .min(u16::try_from(desired).unwrap_or(u16::MAX));
    if width < 20 {
        return false;
    }
    let left = mouse
        .0
        .saturating_add(2)
        .max(min_left)
        .min(area.right().saturating_sub(1 + width));
    let rect = Rect::new(left, top, width, slots as u16 + 4);
    let mut groups = Vec::with_capacity(slots);
    let mut values = Vec::with_capacity(slots);
    let mut min = 0.0_f64;
    let mut max = 0.0_f64;
    for (_, _, value) in &chart.rows {
        if let Some(value) = value {
            min = min.min(*value);
            max = max.max(*value);
        }
    }
    for offset in 0..slots {
        let line = first_line + offset;
        let entry = chart
            .source_rows
            .iter()
            .enumerate()
            .find(|(_, &(first, last))| first.max(body_scroll) == line && last >= line);
        if let Some((index, &source)) = entry {
            let (_, raw, value) = &chart.rows[index];
            let color = if no_colour {
                Color::Reset
            } else {
                CHART_COLORS[index % CHART_COLORS.len()]
            };
            groups.push(BarGroup::new("", [value.unwrap_or(f64::NAN)]).bar_color(0, color));
            values.push(Line::raw(if no_colour {
                format!(
                    " {} {}",
                    CHART_IDS[index % CHART_IDS.len()] as char,
                    display_value(raw, *value)
                )
            } else {
                format!(" {}", display_value(raw, *value))
            }));
            mark_row_header(
                frame,
                body_area,
                body_scroll,
                bounds,
                source,
                index,
                no_colour,
            );
        } else {
            groups.push(BarGroup::new("", [f64::NAN]));
            values.push(Line::default());
        }
    }
    if min == max {
        (min, max) = (0.0, 1.0);
    }
    frame.render_widget(Clear, rect);
    let border = if no_colour {
        Style::default()
    } else {
        Style::default().fg(Color::Cyan)
    };
    let title = if slots < last_line - first_line + 1 {
        format!("{} · partial", chart.title)
    } else {
        chart.title
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border),
        rect,
    );
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    let value_width = u16::try_from(value_width)
        .unwrap_or(u16::MAX)
        .min(inner.width.saturating_sub(3));
    let plot = Rect::new(inner.x, inner.y, inner.width - value_width, inner.height);
    let mut value_axis = Axis::new().grid(false).bounds(min, max);
    let mut category_axis = Axis::new();
    if no_colour {
        value_axis = value_axis
            .style(Style::default())
            .label_style(Style::default());
        category_axis = category_axis
            .style(Style::default())
            .label_style(Style::default());
    }
    frame.render_widget(
        BarChart::new(groups)
            .orientation(BarOrientation::Horizontal)
            .bar_width(1)
            .bar_gap(0)
            .group_gap(0)
            .value_axis(value_axis)
            .category_axis(category_axis),
        plot,
    );
    if value_width > 0 {
        frame.render_widget(
            Paragraph::new(values),
            Rect::new(plot.right(), plot.y, value_width, slots as u16),
        );
    }
    true
}

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    scroll: usize,
    text: &Text<'_>,
    bounds: &TableBounds,
    table_rows: &pulldown_cmark_mdcat::ratatui::TableRows,
    mouse: (u16, u16),
    no_colour: bool,
) {
    let (x, y) = mouse;
    if x < area.x + 2
        || x >= area.right().saturating_sub(2)
        || y <= area.y
        || y >= area.bottom().saturating_sub(1)
    {
        return;
    }
    let row = scroll + usize::from(y - area.y - 1);
    let col = usize::from(x - area.x - 2);
    if let Some(chart) = all_chart_at(text, bounds, table_rows, row, col) {
        // Leave the row headers visible beside the tooltip whenever the window has room.
        let header_right = bounds
            .iter()
            .find(|(line, _)| *line == row)
            .and_then(|(_, ranges)| ranges.first())
            .map(|&(_, right)| {
                area.x
                    .saturating_add(2)
                    .saturating_add(u16::try_from(right).unwrap_or(u16::MAX))
            });
        let chart_area = header_right
            .filter(|&right| area.right().saturating_sub(right) >= 27)
            .map_or(area, |right| {
                Rect::new(
                    right.saturating_sub(1),
                    area.y,
                    area.right().saturating_sub(right).saturating_add(1),
                    area.height,
                )
            });
        let chart_mouse = if chart_area == area {
            mouse
        } else {
            (chart_area.x, mouse.1)
        };
        draw_all(frame, chart_area, chart_mouse, &chart, no_colour);
        return;
    }
    let Some(chart) = row_chart_at(text, bounds, table_rows, row, col)
        .or_else(|| chart_at(text, bounds, table_rows, row, col))
    else {
        return;
    };
    let safe_top = chart
        .row_headers
        .as_ref()
        .and_then(|(_, last, _)| last.checked_sub(scroll))
        .filter(|&row| row < usize::from(area.height.saturating_sub(2)))
        .map_or(area.y + 1, |row| area.y + 2 + row as u16);
    let height = u16::try_from(chart.rows.len().saturating_add(4).max(6))
        .unwrap_or(u16::MAX)
        .min(area.bottom().saturating_sub(1).saturating_sub(safe_top));
    let visible_rows = usize::from(height.saturating_sub(4));
    let title = if chart.rows.len() > visible_rows {
        format!(
            "{} (first {} of {})",
            chart.title,
            visible_rows,
            chart.rows.len()
        )
    } else {
        chart.title.clone()
    };
    let label_width = chart
        .rows
        .iter()
        .take(visible_rows)
        .map(|(label, _, _)| label.width())
        .max()
        .unwrap_or(0)
        .min(16);
    let value_width = chart
        .rows
        .iter()
        .take(visible_rows)
        .map(|(_, raw, value)| display_value(raw, *value).width())
        .max()
        .unwrap_or(0)
        .saturating_add(if no_colour && chart.row_headers.is_some() {
            2
        } else {
            0
        })
        .min(30);
    // The widget needs a label gutter and axis, plus room for the bars and values.
    let desired_width = (label_width + 1 + 24 + 1 + value_width + 2)
        .max(title.width() + 2)
        .max(24);
    let width = area
        .width
        .saturating_sub(2)
        .min(70)
        .min(u16::try_from(desired_width).unwrap_or(u16::MAX));
    if width < 24 || height < 6 {
        return;
    }
    let left = (x.saturating_add(2))
        .min(area.right().saturating_sub(width + 1))
        .max(area.x + 1);
    let top = if y.saturating_add(height) < area.bottom() {
        y + 1
    } else {
        y.saturating_sub(height)
    }
    .max(safe_top)
    .min(area.bottom().saturating_sub(1 + height));
    let rect = Rect::new(left, top, width, height);
    frame.render_widget(Clear, rect);
    let style = if no_colour {
        Style::default()
    } else {
        Style::default().fg(Color::Cyan)
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(style),
        rect,
    );
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    let value_width = u16::try_from(value_width + 1).unwrap_or(u16::MAX);
    let value_width = value_width.min(inner.width.saturating_sub((label_width + 3) as u16));
    let plot = Rect::new(inner.x, inner.y, inner.width - value_width, inner.height);
    let groups = chart
        .rows
        .iter()
        .take(visible_rows)
        .enumerate()
        .map(|(index, (label, _, value))| {
            let group = BarGroup::new(label, [value.unwrap_or(f64::NAN)]);
            if chart.row_headers.is_some() && !no_colour {
                group.bar_color(0, CHART_COLORS[index % CHART_COLORS.len()])
            } else {
                group
            }
        })
        .collect();
    let color = if no_colour { Color::Reset } else { Color::Cyan };
    let min = chart
        .rows
        .iter()
        .filter_map(|row| row.2)
        .fold(0.0_f64, f64::min);
    let max = chart
        .rows
        .iter()
        .filter_map(|row| row.2)
        .fold(0.0_f64, f64::max);
    let (min, max) = if min == max { (0.0, 1.0) } else { (min, max) };
    let mut value_axis = Axis::new().grid(false).bounds(min, max);
    let mut category_axis = Axis::new();
    if no_colour {
        value_axis = value_axis
            .style(Style::default())
            .label_style(Style::default());
        category_axis = category_axis
            .style(Style::default())
            .label_style(Style::default());
    }
    let bars = BarChart::new(groups)
        .orientation(BarOrientation::Horizontal)
        .bar_width(1)
        .group_gap(0)
        .colors(vec![color])
        .value_axis(value_axis)
        .category_axis(category_axis);
    frame.render_widget(bars, plot);
    if value_width > 0 {
        let values = chart
            .rows
            .iter()
            .take(visible_rows)
            .enumerate()
            .map(|(index, (_, raw, value))| {
                Line::raw(if no_colour && chart.row_headers.is_some() {
                    format!(
                        " {} {}",
                        CHART_IDS[index % CHART_IDS.len()] as char,
                        display_value(raw, *value)
                    )
                } else {
                    format!(" {}", display_value(raw, *value))
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(values),
            Rect::new(plot.right(), inner.y, value_width, visible_rows as u16),
        );
    }
}

fn draw_all(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    mouse: (u16, u16),
    chart: &AllChart,
    no_colour: bool,
) {
    let count = all_series_count(chart, area, no_colour);
    let chart = chart.transpose();
    let available_height = usize::from(area.height.saturating_sub(2));
    let available_width = usize::from(area.width.saturating_sub(2)).min(160);
    if count == 0 || available_width < 24 {
        return;
    }
    let value_width = chart
        .rows
        .iter()
        .flat_map(|(_, values)| {
            values
                .iter()
                .take(count)
                .map(|(raw, value)| display_value(raw, *value).width())
        })
        .max()
        .unwrap_or(0)
        .saturating_add(if no_colour { 2 } else { 0 })
        .min(30);
    let panel_width = (24 + 1 + value_width)
        .max(30)
        .min(available_width.saturating_sub(2));
    let per_row = ((available_width.saturating_sub(1)) / (panel_width + 1))
        .max(1)
        .min(chart.rows.len());
    let panel_height = count + 3; // title, bars, axis and tick labels
    let panel_rows = available_height.saturating_sub(2) / panel_height;
    let visible = chart.rows.len().min(panel_rows * per_row);
    if visible == 0 {
        return;
    }
    let displayed_rows = visible.div_ceil(per_row);
    let height = u16::try_from(2 + displayed_rows * panel_height).unwrap();
    let title = if visible < chart.rows.len() || count < chart.columns.len() {
        format!(
            "Separate scales · columns {visible}/{} · rows {count}/{}",
            chart.rows.len(),
            chart.columns.len()
        )
    } else {
        "All numeric columns · separate scales".to_owned()
    };
    let content_width = (per_row * panel_width + per_row.saturating_sub(1)).max(title.width());
    let width = u16::try_from((content_width + 2).min(available_width)).unwrap();
    let left = mouse
        .0
        .saturating_add(2)
        .min(area.right().saturating_sub(width + 1))
        .max(area.x + 1);
    let top = if mouse.1.saturating_add(height) < area.bottom() {
        mouse.1 + 1
    } else {
        mouse.1.saturating_sub(height).max(area.y + 1)
    };
    let rect = Rect::new(left, top, width, height);
    frame.render_widget(Clear, rect);
    let border = if no_colour {
        Style::default()
    } else {
        Style::default().fg(Color::Cyan)
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border),
        rect,
    );
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    let actual_panel_width =
        ((usize::from(inner.width) - per_row.saturating_sub(1)) / per_row).min(panel_width);
    let colors: Vec<_> = (0..count)
        .map(|i| {
            if no_colour {
                Color::Reset
            } else {
                CHART_COLORS[i % CHART_COLORS.len()]
            }
        })
        .collect();
    for (index, (label, values)) in chart.rows.iter().take(visible).enumerate() {
        let x = inner.x + ((index % per_row) * (actual_panel_width + 1)) as u16;
        let y = inner.y + ((index / per_row) * panel_height) as u16;
        let panel = Rect::new(x, y, actual_panel_width as u16, panel_height as u16);
        frame.render_widget(
            Paragraph::new(label.as_str()),
            Rect::new(panel.x, panel.y, panel.width, 1),
        );
        let values_width = u16::try_from(value_width + 1)
            .unwrap_or(u16::MAX)
            .min(panel.width.saturating_sub(3));
        let plot = Rect::new(
            panel.x,
            panel.y + 1,
            panel.width - values_width,
            (count + 2) as u16,
        );
        let (min, max) = values
            .iter()
            .filter_map(|(_, value)| *value)
            .fold((0.0_f64, 0.0_f64), |(min, max), value| {
                (min.min(value), max.max(value))
            });
        let (min, max) = if min == max { (0.0, 1.0) } else { (min, max) };
        let mut value_axis = Axis::new().grid(false).bounds(min, max);
        let mut category_axis = Axis::new();
        if no_colour {
            value_axis = value_axis
                .style(Style::default())
                .label_style(Style::default());
            category_axis = category_axis
                .style(Style::default())
                .label_style(Style::default());
        }
        let group = BarGroup::new(
            "",
            values
                .iter()
                .take(count)
                .map(|(_, value)| value.unwrap_or(f64::NAN)),
        );
        frame.render_widget(
            BarChart::new(vec![group])
                .orientation(BarOrientation::Horizontal)
                .bar_width(1)
                .bar_gap(0)
                .group_gap(0)
                .colors(colors.clone())
                .value_axis(value_axis)
                .category_axis(category_axis),
            plot,
        );
        if values_width > 0 {
            let lines: Vec<_> = values
                .iter()
                .take(count)
                .enumerate()
                .map(|(index, (raw, value))| {
                    Line::raw(if no_colour {
                        format!(
                            " {} {}",
                            CHART_IDS[index] as char,
                            display_value(raw, *value)
                        )
                    } else {
                        format!(" {}", display_value(raw, *value))
                    })
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines),
                Rect::new(plot.right(), plot.y, values_width, count as u16),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_chart_aligns_with_visible_wrapped_rows_and_colors_headers() {
        use ratatui::{backend::TestBackend, Terminal};
        let text = Text::from(vec![
            Line::raw(" Name       Time"),
            Line::raw(" ─────────────────────"),
            Line::raw(" Long       0.69"),
            Line::raw(" name       "),
            Line::raw(" ─────────────────────"),
            Line::raw(" Beta       inactive"),
            Line::raw(" Gamma      5.07"),
        ]);
        let ranges = vec![(1, 10), (12, 20)];
        let bounds = vec![
            (0, ranges.clone()),
            (2, ranges.clone()),
            (3, ranges.clone()),
            (5, ranges.clone()),
            (6, ranges),
        ];
        let rows = vec![(0, 0), (2, 3), (5, 5), (6, 6)];
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                assert!(draw_column_aligned(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &rows,
                    (15, 1),
                    false
                ));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen_row = |y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(screen_row(3).contains("0.69"));
        assert!(!screen_row(4).contains("0.69"));
        assert!(screen_row(6).contains("∅ inactive"));
        assert!(screen_row(7).contains("5.07"));
        assert_eq!(buffer[(3, 3)].fg, CHART_COLORS[0]);
        assert_eq!(buffer[(3, 4)].fg, CHART_COLORS[0]);
        assert_eq!(buffer[(3, 6)].fg, CHART_COLORS[1]);
        assert_eq!(buffer[(3, 7)].fg, CHART_COLORS[2]);
        assert!(!screen_row(3).contains("Long"));
        terminal
            .draw(|frame| {
                assert!(draw_column_aligned(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    Rect::new(0, 3, 100, 17),
                    5,
                    &text,
                    &bounds,
                    &rows,
                    (15, 1),
                    true
                ));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen_row = |y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(screen_row(4).contains("2 ∅ inactive"));
        assert!(screen_row(5).contains("3 5.07"));
        assert_eq!(buffer[(2, 4)].symbol(), "2");
        assert_eq!(buffer[(2, 5)].symbol(), "3");
    }

    #[test]
    fn small_charts_fit_five_columns_and_seven_rows_in_a_medium_terminal() {
        use ratatui::{backend::TestBackend, Terminal};
        let chart = AllChart {
            columns: (0..5).map(|i| format!("Metric{i}")).collect(),
            rows: (0..7)
                .map(|row| {
                    (
                        format!("Input{row}"),
                        (0..5)
                            .map(|column| {
                                let value = (row + 1) as f64 * 10_f64.powi(column);
                                (format!("{value}"), Some(value))
                            })
                            .collect(),
                    )
                })
                .collect(),
            source_rows: Vec::new(),
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| draw_all(frame, Rect::new(0, 0, 80, 40), (3, 1), &chart, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = (0..40)
            .flat_map(|y| (0..80).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        for name in &chart.columns {
            assert!(rendered.contains(name), "missing {name}: {rendered}");
        }
        assert!(
            !rendered.contains("Input0"),
            "the tooltip should not include a legend"
        );
        assert!(rendered.contains("separate scales"));
    }

    #[test]
    fn charts_require_two_numbers_and_keep_missing_values_distinct_from_zero() {
        use ratatui::{backend::TestBackend, Terminal};
        let text = Text::from(vec![
            Line::raw(" Name     Time         Other"),
            Line::raw(" ──────────────────────────────"),
            Line::raw(" Alpha    0.69         4"),
            Line::raw(" Beta     inactive     2"),
            Line::raw(" Gamma    0            1"),
            Line::raw(" Delta                 inactive"),
        ]);
        let ranges = vec![(1, 8), (10, 20), (23, 31)];
        let bounds = vec![
            (0, ranges.clone()),
            (2, ranges.clone()),
            (3, ranges.clone()),
            (4, ranges.clone()),
            (5, ranges),
        ];
        let chart = chart_at(&text, &bounds, &Vec::new(), 0, 12).unwrap();
        assert_eq!(
            chart.rows.iter().map(|row| row.2).collect::<Vec<_>>(),
            [Some(0.69), None, Some(0.0), None]
        );
        let all = all_chart_at(&text, &bounds, &Vec::new(), 0, 2).unwrap();
        assert_eq!(all.columns, ["Time", "Other"]);
        assert_eq!(all.rows[1].1[0].1, None);
        assert_eq!(all.rows[2].1[0].1, Some(0.0));
        assert!(row_chart_at(&text, &bounds, &Vec::new(), 2, 2).is_some());
        assert!(row_chart_at(&text, &bounds, &Vec::new(), 3, 2).is_none());
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (13, 1),
                    false,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = (0..20)
            .flat_map(|y| (0..100).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        assert!(rendered.contains("∅ inactive"), "{rendered}");
        assert!(rendered.contains("∅"), "{rendered}");
        assert!(rendered.contains("0.69"), "{rendered}");
    }

    #[test]
    fn charts_count_wrapped_lines_as_one_row() {
        let text = Text::from(vec![
            Line::raw(" Name       Time"),
            Line::raw(" ─────────────────────"),
            Line::raw(" Long       0.69"),
            Line::raw(" name       "),
            Line::raw(" ─────────────────────"),
            Line::raw(" Beta       inactive"),
            Line::raw(" Gamma      5.07"),
        ]);
        let ranges = vec![(1, 10), (12, 20)];
        let bounds = vec![
            (0, ranges.clone()),
            (2, ranges.clone()),
            (3, ranges.clone()),
            (5, ranges.clone()),
            (6, ranges),
        ];
        let logical = vec![(0, 0), (2, 3), (5, 5), (6, 6)];
        let chart = chart_at(&text, &bounds, &logical, 0, 13).unwrap();
        assert_eq!(chart.rows.len(), 3);
        assert_eq!(chart.rows[0].0, "Long name");
        assert_eq!(chart.rows[1].2, None);
        let all = all_chart_at(&text, &bounds, &logical, 0, 2).unwrap();
        assert_eq!(all.rows.len(), 3);
        assert_eq!(all.rows[0].0, "Long name");
    }

    #[test]
    fn table_headers_pin_only_while_scrolling_inside_their_table() {
        let ranges = vec![(1, 6), (8, 13)];
        let bounds = vec![
            (2, ranges.clone()),
            (3, ranges.clone()),
            (5, ranges.clone()),
            (6, ranges.clone()),
            (7, ranges.clone()),
            (8, ranges.clone()),
            (9, ranges.clone()),
            (10, ranges.clone()),
            (12, Vec::new()),
            (15, ranges.clone()),
            (17, ranges.clone()),
            (18, ranges.clone()),
            (19, ranges.clone()),
            (20, ranges),
        ];
        let mut text = Text::from(vec![Line::raw("content"); 22]);
        for row in [1, 4, 11, 14, 16, 21] {
            text.lines[row] = Line::raw("──────────────────");
        }
        assert_eq!(pinned_header(&text, &bounds, 3, 4), None);
        assert_eq!(pinned_header(&text, &bounds, 4, 4), Some((1, 4)));
        assert_eq!(pinned_header(&text, &bounds, 6, 4), Some((1, 4)));
        assert_eq!(pinned_header(&text, &bounds, 7, 4), Some((1, 4)));
        assert_eq!(pinned_header(&text, &bounds, 9, 4), Some((1, 4)));
        assert_eq!(pinned_header(&text, &bounds, 10, 4), Some((1, 4)));
        assert_eq!(pinned_header(&text, &bounds, 11, 4), None);
        assert_eq!(pinned_header(&text, &bounds, 17, 4), Some((14, 3)));
        assert_eq!(pinned_header(&text, &bounds, 17, 2), None);
        assert_eq!(pinned_header(&text, &bounds, 17, 0), None);
        assert!(is_closing_rule_at_top(&text, &bounds, 11));
        assert!(is_closing_rule_at_top(&text, &bounds, 21));
        assert!(!is_closing_rule_at_top(&text, &bounds, 4));
        assert!(!is_closing_rule_at_top(&text, &bounds, 16));
    }

    #[test]
    fn top_left_charts_all_numeric_columns_and_skips_text() {
        use ratatui::{backend::TestBackend, Terminal};
        let text = Text::from(vec![
            Line::raw(" Item     Delta      SD         Note"),
            Line::raw(" ─────────────────────────────────────"),
            Line::raw(" Alpha    −2.5 ms    0.1 ms     yes"),
            Line::raw(" Beta     +1.0 ms    0.2 ms     no"),
        ]);
        let ranges = vec![(1, 8), (10, 19), (21, 30), (32, 38)];
        let bounds = vec![(0, ranges.clone()), (2, ranges.clone()), (3, ranges)];
        let chart = all_chart_at(&text, &bounds, &Vec::new(), 0, 2).unwrap();
        assert_eq!(chart.columns, ["Delta", "SD"]);
        assert_eq!(chart.rows[0].1[0].1, Some(-2.5));
        assert_eq!(chart.rows[1].1[1].1, Some(0.2));
        let transposed = chart.transpose();
        assert_eq!(transposed.columns, ["Alpha", "Beta"]);
        assert_eq!(transposed.rows[0].0, "Delta");
        assert_eq!(
            transposed.rows[0]
                .1
                .iter()
                .map(|(_, value)| *value)
                .collect::<Vec<_>>(),
            [Some(-2.5), Some(1.0)]
        );
        assert_eq!(transposed.rows[1].0, "SD");
        assert_eq!(
            transposed.rows[1]
                .1
                .iter()
                .map(|(_, value)| *value)
                .collect::<Vec<_>>(),
            [Some(0.1), Some(0.2)]
        );
        assert!(all_chart_at(&text, &bounds, &Vec::new(), 2, 2).is_none());
        let row_chart = row_chart_at(&text, &bounds, &Vec::new(), 2, 2).unwrap();
        assert_eq!(row_chart.title, "Alpha");
        assert_eq!(
            row_chart
                .rows
                .iter()
                .map(|(name, _, value)| (name.as_str(), *value))
                .collect::<Vec<_>>(),
            [("Delta", Some(-2.5)), ("SD", Some(0.1)), ("Note", None)]
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (4, 1),
                    false,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = (0..20)
            .flat_map(|y| (0..100).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        for expected in ["separate scales", "−2.5 ms", "0.2 ms", "Delta", "SD"] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        terminal
            .draw(|frame| {
                highlight_all_chart_rows(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    Rect::new(0, 0, 100, 20),
                    0,
                    (4, 1),
                    false,
                );
                draw(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (4, 1),
                    false,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(3, 3)].fg, CHART_COLORS[0]);
        assert_eq!(buffer[(3, 4)].fg, CHART_COLORS[1]);
        assert_ne!(buffer[(12, 3)].fg, CHART_COLORS[0]);
        terminal
            .draw(|frame| {
                highlight_all_chart_rows(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    Rect::new(0, 0, 100, 20),
                    0,
                    (4, 1),
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 3)].symbol(), "1");
        assert_eq!(buffer[(2, 4)].symbol(), "2");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (4, 3),
                    false,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = (0..20)
            .flat_map(|y| (0..100).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        for expected in ["Alpha", "Delta", "SD", "−2.5 ms", "0.1 ms"] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
    }

    #[test]
    fn row_chart_colors_match_visible_and_pinned_column_headers() {
        use ratatui::{backend::TestBackend, Terminal};
        let text = Text::from(vec![
            Line::raw(" Item     Delta      SD         Note"),
            Line::raw(" ─────────────────────────────────────"),
            Line::raw(" Alpha    −2.5 ms    0.1 ms     yes"),
            Line::raw(" Beta     +1.0 ms    0.2 ms     no"),
        ]);
        let ranges = vec![(1, 8), (10, 19), (21, 30), (32, 38)];
        let bounds = vec![(0, ranges.clone()), (2, ranges.clone()), (3, ranges)];
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (4, 3),
                    false,
                );
                highlight_row_chart_headers(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    Rect::new(0, 0, 100, 20),
                    0,
                    None,
                    &text,
                    &bounds,
                    &Vec::new(),
                    Rect::new(0, 0, 100, 20),
                    0,
                    (4, 3),
                    false,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(12, 1)].fg, CHART_COLORS[0]);
        assert_eq!(buffer[(23, 1)].fg, CHART_COLORS[1]);
        assert_eq!(buffer[(34, 1)].fg, CHART_COLORS[2]);
        for color in [CHART_COLORS[0], CHART_COLORS[1]] {
            assert!(
                (2..20).any(|y| (0..100).any(|x| buffer[(x, y)].fg == color
                    && matches!(
                        buffer[(x, y)].symbol(),
                        "█" | "▏" | "▎" | "▍" | "▌" | "▋" | "▊" | "▉"
                    ))),
                "missing bar color {color:?}"
            );
        }
        terminal
            .draw(|frame| {
                highlight_row_chart_headers(
                    frame,
                    Rect::new(0, 0, 100, 20),
                    Rect::new(0, 3, 100, 17),
                    2,
                    Some((0, 1)),
                    &text,
                    &bounds,
                    &Vec::new(),
                    Rect::new(0, 3, 100, 17),
                    2,
                    (4, 4),
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(11, 1)].symbol(), "1");
        assert_eq!(buffer[(22, 1)].symbol(), "2");
        assert_eq!(buffer[(33, 1)].symbol(), "3");
    }

    #[test]
    fn chart_tooltip_fits_labels_and_values() {
        use ratatui::{backend::TestBackend, Terminal};
        let text = Text::from(vec![
            Line::raw(" Input         Pair SD"),
            Line::raw(" ─────────────────────────"),
            Line::raw(" Spiral 512k   0.00826 ms"),
            Line::raw(" Spiral 1m     0.06802 ms"),
        ]);
        let ranges = vec![(1, 13), (15, 26)];
        let bounds = vec![(0, ranges.clone()), (2, ranges.clone()), (3, ranges)];
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    0,
                    &text,
                    &bounds,
                    &Vec::new(),
                    (18, 3),
                    false,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let border = (0..120).find(|&x| buffer[(x, 4)].symbol() == "┐").unwrap();
        let left = (0..120).find(|&x| buffer[(x, 4)].symbol() == "┌").unwrap();
        assert!(border - left < 60, "tooltip should fit its content");
        let value_row: String = (left..border).map(|x| buffer[(x, 5)].symbol()).collect();
        assert!(value_row.contains("0.00826 ms"), "{value_row}");
        assert!(value_row.ends_with("0.00826 ms"), "{value_row}");
    }

    #[test]
    fn hover_highlights_header_column_or_body_row() {
        use ratatui::{backend::TestBackend, Terminal};
        let ranges = vec![(1, 6), (8, 13)];
        let bounds = vec![
            (3, ranges.clone()),
            (5, ranges.clone()),
            (6, ranges.clone()),
            (10, ranges),
        ];
        let mut terminal = Terminal::new(TestBackend::new(30, 9)).unwrap();
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_style(Rect::new(0, 0, 30, 9), Style::default().fg(Color::Yellow));
                highlight_hover(
                    frame,
                    Rect::new(0, 0, 30, 9),
                    3,
                    &bounds,
                    &Vec::new(),
                    (11, 3),
                    false,
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let bg = Color::Rgb(54, 54, 54);
        assert_eq!(buffer[(4, 3)].bg, bg); // hovered row, first column
        assert_ne!(buffer[(11, 1)].bg, bg); // same column, header
        assert_ne!(buffer[(11, 4)].bg, bg); // same column, next row
        assert_ne!(buffer[(4, 4)].bg, bg); // different row and column
        assert_ne!(buffer[(11, 8)].bg, bg); // another table / border
        assert_eq!(buffer[(11, 3)].fg, Color::Yellow);
        terminal
            .draw(|frame| {
                highlight_hover(
                    frame,
                    Rect::new(0, 0, 30, 9),
                    3,
                    &bounds,
                    &Vec::new(),
                    (11, 1),
                    false,
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(11, 1)].bg, bg); // header column
        assert_eq!(buffer[(11, 3)].bg, bg); // same column, data row
        assert_eq!(buffer[(11, 4)].bg, bg);
        assert_ne!(buffer[(4, 1)].bg, bg); // other header column
        assert_ne!(buffer[(4, 3)].bg, bg); // other data column
        terminal
            .draw(|frame| {
                highlight_hover(
                    frame,
                    Rect::new(0, 0, 30, 9),
                    3,
                    &bounds,
                    &Vec::new(),
                    (4, 3),
                    false,
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 3)].bg, bg); // row header
        assert_eq!(buffer[(11, 3)].bg, bg); // same row
        assert_ne!(buffer[(4, 4)].bg, bg); // other row
        terminal
            .draw(|frame| {
                highlight_pinned(
                    frame,
                    Rect::new(0, 0, 30, 9),
                    5,
                    &bounds,
                    (3, 1),
                    (11, 1),
                    false,
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(11, 1)].bg, bg);
        assert_eq!(buffer[(11, 3)].bg, bg);
    }

    #[test]
    fn sorts_markdown_rows_numerically_and_preserves_links() {
        let input = "| Name | Delta |\n|---|---:|\n| [A](a.md) | −2.9 ms |\n| [B](b.md) | 10 ms |\n| [C](c.md) | -1 ms |\n";
        let events = || {
            pulldown_cmark::Parser::new_ext(input, pulldown_cmark::Options::ENABLE_TABLES).collect()
        };
        let mut sorts = HashMap::new();
        sorts.insert(
            0,
            Sort {
                column: 1,
                descending: false,
            },
        );
        let sorted = sort_markdown(events(), &sorts);
        let destinations: Vec<_> = sorted
            .iter()
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(destinations, ["a.md", "c.md", "b.md"]);
        sorts.insert(
            0,
            Sort {
                column: 1,
                descending: true,
            },
        );
        let sorted = sort_markdown(events(), &sorts);
        let destinations: Vec<_> = sorted
            .iter()
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(destinations, ["b.md", "c.md", "a.md"]);
    }

    #[test]
    fn extracts_signed_values_from_hovered_column() {
        let text = Text::from(vec![
            Line::raw(" Input   Delta            SD"),
            Line::raw(" ─────────────────────────────────"),
            Line::raw(" A       −1.763 ms        0.351 ms"),
            Line::raw(" B       +0.0130 ms       0.011 ms"),
        ]);
        let bounds = vec![
            (0, vec![(1, 6), (9, 24), (26, 35)]),
            (2, vec![(1, 6), (9, 24), (26, 35)]),
            (3, vec![(1, 6), (9, 24), (26, 35)]),
        ];
        let chart = chart_at(&text, &bounds, &Vec::new(), 2, 10).unwrap();
        assert_eq!(chart.title, "Delta");
        assert_eq!(
            chart.rows.iter().map(|row| row.2).collect::<Vec<_>>(),
            [Some(-1.763), Some(0.013)]
        );
        assert!(chart_at(&text, &bounds, &Vec::new(), 2, 2).is_none());
    }

    #[test]
    fn detects_aligned_text_tables_with_horizontal_rules() {
        let mut text = Text::from(vec![
            Line::raw("──────────────────────────────────────────────────"),
            Line::raw(" Input               Delta              Pair SD"),
            Line::raw("──────────────────────────────────────────────────"),
            Line::raw(" Spiral 512k         −1.763 ms          0.351 ms"),
            Line::raw(" Spiral 1m           −2.922 ms          0.712 ms"),
            Line::raw("──────────────────────────────────────────────────"),
        ]);
        let mut bounds = Vec::new();
        add_text_tables(&text, &mut bounds);
        let chart = chart_at(&text, &bounds, &Vec::new(), 3, 22).unwrap();
        assert_eq!(chart.title, "Delta");
        assert_eq!(chart.rows[0].0, "Spiral 512k");
        assert_eq!(chart.rows[1].2, Some(-2.922));
        let mut sorts = HashMap::new();
        sorts.insert(
            0,
            Sort {
                column: 1,
                descending: false,
            },
        );
        sort_text_tables(&mut text, &mut [], &bounds, &sorts);
        assert!(text.lines[3].to_string().contains("Spiral 1m"));
        assert!(text.lines[4].to_string().contains("Spiral 512k"));
    }
}
