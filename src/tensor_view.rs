//! A viewport over mapped tensor values, with spreadsheet-style navigation.
use crate::tensor_stats::{Axis, Hover, Statistics, Target};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mdcat::gguf_view::TensorData;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use std::{sync::Arc, time::Instant};

const COLUMN_GAP: usize = 2;
pub struct TensorView {
    pub data: Arc<TensorData>,
    hover: Hover,
    hover_anchor: Option<(usize, usize)>,
    row: u64,
    column: u64,
    plane: u64,
    top: u64,
    left: u64,
    width: usize,
    height: usize,
    goto: Option<String>,
    quantized: bool,
    message: String,
}
impl TensorView {
    pub fn open(path: &std::path::Path, index: usize) -> Result<Self> {
        Ok(Self {
            data: Arc::new(TensorData::open(path, index)?),
            hover: Hover::default(),
            hover_anchor: None,
            row: 0,
            column: 0,
            plane: 0,
            top: 0,
            left: 0,
            width: 80,
            height: 20,
            goto: None,
            quantized: false,
            message: String::new(),
        })
    }
    fn data_dimensions(&self) -> (u64, u64, u64) {
        let s = &self.data.shape;
        match s.len() {
            0 => (1, 1, 1),
            1 => (s[0], 1, 1),
            n => (s[n - 2], s[n - 1], s[..n - 2].iter().product()),
        }
    }
    fn value_dimensions(&self) -> (u64, u64, u64) {
        if self.quantized && self.data.shape.len() == 1 {
            (1, self.data.shape[0], 1)
        } else {
            self.data_dimensions()
        }
    }
    fn dimensions(&self) -> (u64, u64, u64) {
        let (rows, cols, planes) = self.value_dimensions();
        if self.quantized {
            let layout = self.data.quantization().unwrap();
            (
                rows,
                cols / layout.block_len as u64 * (layout.block_len + layout.metadata.len()) as u64,
                planes,
            )
        } else {
            (rows, cols, planes)
        }
    }
    fn logical_column(&self, column: u64) -> u64 {
        if !self.quantized {
            return column;
        }
        let layout = self.data.quantization().unwrap();
        let fields = (layout.block_len + layout.metadata.len()) as u64;
        column / fields * layout.block_len as u64
            + (column % fields).saturating_sub(layout.metadata.len() as u64)
    }
    fn flat_index(&self) -> u64 {
        let (rows, cols, _) = self.value_dimensions();
        (self.plane * rows + self.row) * cols + self.logical_column(self.column)
    }
    fn select_flat(&mut self, index: u64) {
        let (rows, cols, _) = self.value_dimensions();
        self.plane = index / (rows * cols);
        self.row = index / cols % rows;
        let col = index % cols;
        self.column = if self.quantized {
            let layout = self.data.quantization().unwrap();
            let block = layout.block_len as u64;
            col / block * (block + layout.metadata.len() as u64)
                + layout.metadata.len() as u64
                + col % block
        } else {
            col
        };
        self.left = 0;
        self.reveal();
    }
    fn header(&self, column: u64) -> String {
        if !self.quantized {
            return column.to_string();
        }
        let layout = self.data.quantization().unwrap();
        let fields = (layout.block_len + layout.metadata.len()) as u64;
        let block = column / fields;
        let field = (column % fields) as usize;
        match layout.metadata.get(field) {
            Some(label) => match label.strip_prefix("Subscale") {
                Some(index) => format!("B{block}.{index} Scale"),
                None => format!("B{block} {label}"),
            },
            None => self.logical_column(column).to_string(),
        }
    }
    fn cell_text(&self, row: u64, col: u64) -> String {
        let (rows, cols, _) = self.value_dimensions();
        if !self.quantized {
            return self.value_text((self.plane * rows + row) * cols + col);
        }
        let layout = self.data.quantization().unwrap();
        let fields = (layout.block_len + layout.metadata.len()) as u64;
        let block = (self.plane * rows + row) * (cols / layout.block_len as u64) + col / fields;
        self.data
            .quantized_field(block, (col % fields) as usize)
            .unwrap_or_else(|_| "ERR".into())
    }
    fn block_style(&self, col: u64, dark: bool) -> Style {
        let layout = self.data.quantization().unwrap();
        let fields = (layout.block_len + layout.metadata.len()) as u64;
        let alternate = (col / fields) % 2 == 1;
        let metadata = col % fields < layout.metadata.len() as u64;
        let (background, foreground) = match (dark, alternate, metadata) {
            (true, false, false) => (Color::Rgb(51, 73, 79), Color::Rgb(191, 226, 230)),
            (true, false, true) => (Color::Rgb(32, 51, 57), Color::Rgb(175, 213, 219)),
            (true, true, false) => (Color::Rgb(72, 59, 83), Color::Rgb(225, 204, 237)),
            (true, true, true) => (Color::Rgb(48, 36, 60), Color::Rgb(212, 185, 228)),
            (false, false, false) => (Color::Rgb(225, 243, 245), Color::Rgb(34, 67, 73)),
            (false, false, true) => (Color::Rgb(177, 212, 218), Color::Rgb(26, 55, 61)),
            (false, true, false) => (Color::Rgb(241, 229, 248), Color::Rgb(73, 45, 88)),
            (false, true, true) => (Color::Rgb(211, 185, 226), Color::Rgb(57, 33, 71)),
        };
        Style::default().bg(background).fg(foreground)
    }
    fn gutter(&self) -> usize {
        self.dimensions()
            .0
            .saturating_sub(1)
            .to_string()
            .len()
            .max(3)
            + 2
    }
    fn separator_style(&self, previous: u64, current: u64, dark: bool) -> Style {
        if self.quantized {
            let layout = self.data.quantization().unwrap();
            let fields = (layout.block_len + layout.metadata.len()) as u64;
            if previous / fields == current / fields {
                return self.block_style(current, dark);
            }
        }
        Style::default()
    }
    fn value_text(&self, index: u64) -> String {
        self.data
            .formatted_value(index)
            .unwrap_or_else(|_| "ERR".into())
    }
    fn columns(&self) -> Vec<(u64, usize)> {
        let (rows, cols, _) = self.dimensions();
        let mut result = Vec::new();
        let mut used = self.gutter();
        for col in self.left..cols {
            let width = (self.top
                ..rows.min(self.top + self.height.saturating_sub(1).max(1) as u64))
                .map(|row| self.cell_text(row, col).len())
                .max()
                .unwrap_or(1)
                .max(self.header(col).len());
            let separator = if result.is_empty() { 0 } else { COLUMN_GAP };
            if !result.is_empty() && used + separator + width > self.width {
                break;
            }
            result.push((col, width));
            used += separator + width;
            if used >= self.width {
                break;
            }
        }
        result
    }
    fn visible(&self) -> (usize, usize) {
        (
            self.height.saturating_sub(1).max(1),
            self.columns().len().max(1),
        )
    }
    pub fn caption(&self) -> String {
        let shape = self
            .data
            .shape
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" × ");
        format!("{} · {} · {shape}", self.data.name, self.data.kind)
    }
    pub fn coordinates(&self) -> String {
        let mut index = self.flat_index();
        let mut indices = Vec::new();
        for dimension in self.data.shape.iter().rev() {
            indices.push(index % dimension);
            index /= dimension;
        }
        indices.reverse();
        format!("{indices:?}")
    }
    fn reveal(&mut self) {
        let (rows, _) = self.visible();
        self.top = self
            .top
            .min(self.row)
            .max(self.row.saturating_sub(rows as u64 - 1));
        let approximate_columns = self
            .width
            .saturating_sub(self.gutter())
            .checked_div(1 + COLUMN_GAP)
            .unwrap_or(0)
            .max(1) as u64;
        self.left = self
            .left
            .min(self.column)
            .max(self.column.saturating_sub(approximate_columns - 1));
        while self
            .columns()
            .last()
            .is_some_and(|(col, _)| *col < self.column)
        {
            self.left += 1;
        }
    }
    pub fn resize(&mut self, width: usize, height: usize) -> bool {
        if (self.width, self.height) == (width, height) {
            return false;
        }
        self.width = width;
        self.height = height;
        self.reveal();
        true
    }
    pub fn is_editing(&self) -> bool {
        self.goto.is_some()
    }
    pub fn footer(&self) -> String {
        if let Some(input) = &self.goto {
            return format!(" Go to row,column (zero-based): {input} ▏ · Enter: go · Esc: cancel ");
        }
        if !self.message.is_empty() {
            return format!(" {} ", self.message);
        }
        if self.data.quantization().is_some() {
            format!(
                " {} · g: {} · Ctrl+G: go to cell · [ ]: slice · q: back ",
                if self.quantized {
                    "Quantized codes"
                } else {
                    "Dequantized"
                },
                if self.quantized {
                    "dequantize"
                } else {
                    "quantize"
                }
            )
        } else {
            " ↑↓←→: cells · PgUp/Dn: rows · Ctrl+G: go to cell · q: back ".into()
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let (rows, cols, planes) = self.dimensions();
        if let Some(input) = &mut self.goto {
            match key.code {
                KeyCode::Esc => self.goto = None,
                KeyCode::Enter => {
                    let parts = input
                        .split(',')
                        .map(|s| s.trim().parse::<u64>())
                        .collect::<Vec<_>>();
                    if parts.len() == 2
                        && parts[0]
                            .as_ref()
                            .is_ok_and(|r| *r < self.data_dimensions().0)
                        && parts[1]
                            .as_ref()
                            .is_ok_and(|c| *c < self.data_dimensions().1)
                    {
                        let (r, c, _) = self.data_dimensions();
                        let index = (self.plane * r + *parts[0].as_ref().unwrap()) * c
                            + *parts[1].as_ref().unwrap();
                        self.goto = None;
                        self.select_flat(index);
                    } else {
                        self.message = "Cell is out of range; use row,column".into();
                        self.goto = None;
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == ',' || c == ' ' => input.push(c),
                _ => (),
            }
            return true;
        }
        self.message.clear();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.row = self.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.row = (self.row + 1).min(rows - 1),
            KeyCode::Left | KeyCode::Char('h') => self.column = self.column.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.column = (self.column + 1).min(cols - 1),
            KeyCode::PageUp => self.row = self.row.saturating_sub(self.visible().0 as u64),
            KeyCode::PageDown => {
                self.row = self
                    .row
                    .saturating_add(self.visible().0 as u64)
                    .min(rows - 1)
            }
            KeyCode::Home => {
                self.column = 0;
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.row = 0;
                }
            }
            KeyCode::End => {
                self.column = cols - 1;
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.row = rows - 1;
                }
            }
            KeyCode::Char('[') => self.plane = self.plane.saturating_sub(1),
            KeyCode::Char(']') => self.plane = (self.plane + 1).min(planes - 1),
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.goto = Some(String::new())
            }
            KeyCode::Char('g') => {
                if self.data.quantization().is_none() {
                    self.message = format!(
                        "{} is stored directly; no quantization blocks",
                        self.data.kind
                    );
                    return true;
                }
                let index = self.flat_index();
                self.quantized = !self.quantized;
                self.select_flat(index);
                return true;
            }
            _ => return false,
        }
        self.reveal();
        true
    }
    fn column_at(&self, x: usize) -> Option<u64> {
        if x < self.gutter() {
            return None;
        }
        let mut offset = self.gutter();
        self.columns()
            .into_iter()
            .enumerate()
            .find_map(|(index, (col, width))| {
                if index > 0 {
                    offset += COLUMN_GAP;
                }
                let start = offset;
                offset += width;
                (x >= start && x < offset).then_some(col)
            })
    }
    pub fn click(&mut self, x: usize, y: usize) {
        if y < 1 || x < self.gutter() {
            return;
        }
        let (rows, cols, _) = self.dimensions();
        let column = self.column_at(x);
        self.row = (self.top + (y - 1) as u64).min(rows - 1);
        if let Some(col) = column {
            self.column = col.min(cols - 1);
        }
        self.reveal();
    }
    fn statistics_target(&self, mouse: Option<(usize, usize)>) -> Option<Target> {
        let (x, y) = mouse?;
        if x >= self.width || y >= self.height {
            return None;
        }
        let (rows, columns, planes) = self.value_dimensions();
        let axis = if y == 0 && x < self.gutter() {
            Axis::Tensor { planes }
        } else if y == 0 {
            Axis::Column(self.column_at(x)?)
        } else if x < self.gutter() && (self.top + (y - 1) as u64) < rows {
            Axis::Row(self.top + (y - 1) as u64)
        } else {
            return None;
        };
        Some(Target {
            axis,
            plane: if matches!(axis, Axis::Tensor { .. }) {
                0
            } else {
                self.plane
            },
            quantized: self.quantized,
            rows,
            columns,
        })
    }
    pub fn update_hover(&mut self, mouse: Option<(usize, usize)>) {
        let target = self.statistics_target(mouse);
        self.hover_anchor = mouse;
        self.hover.update(target, &self.data, Instant::now());
    }
    pub fn hover_needs_tick(&self) -> bool {
        self.hover.needs_tick()
    }
    pub fn clear_hover(&mut self) {
        self.update_hover(None);
    }

    pub fn wheel(&mut self, down: bool) {
        let (rows, _, _) = self.dimensions();
        self.row = if down {
            self.row.saturating_add(3).min(rows - 1)
        } else {
            self.row.saturating_sub(3)
        };
        self.reveal();
    }
    fn histogram_lines(
        stats: &Statistics,
        label: &str,
        height: u16,
        integer: bool,
        dark: bool,
        plain: bool,
        series: usize,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![Line::raw(label.to_owned())];
        if stats.finite == 0 {
            lines.push(Line::raw("No finite values"));
            return lines;
        }
        let maximum = stats.histogram.iter().copied().max().unwrap_or(1).max(1) as f64;
        let symbols = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let colors = if dark {
            [(166, 215, 224), (210, 188, 232), (235, 196, 153)]
        } else {
            [(45, 112, 130), (119, 75, 155), (140, 98, 48)]
        };
        let (r, g, b) = colors[series % colors.len()];
        let style = if plain {
            Style::default()
        } else {
            Style::default().fg(Color::Rgb(r, g, b))
        };
        let range = Line::raw(if integer {
            format!("{:.0} ← range → {:.0}", stats.minimum, stats.maximum)
        } else {
            format!("{:.6} ← range → {:.6}", stats.minimum, stats.maximum)
        });
        let plot_width = stats.histogram.len() * 2;
        let plot_indent = 2 + range.width().saturating_sub(plot_width) / 2;
        for level in (0..height).rev() {
            let mut plot = " ".repeat(plot_indent);
            for count in stats.histogram {
                let units = (count as f64 / maximum * f64::from(height * 8)).round() as u16;
                let filled = units.saturating_sub(level * 8).min(8) as usize;
                plot.push(symbols[filled]);
                plot.push(symbols[filled]);
            }
            lines.push(Line::styled(plot, style));
        }
        let range_indent = 2 + plot_width.saturating_sub(range.width()) / 2;
        lines.push(Line::raw(format!("{:range_indent$}{range}", "")));
        lines
    }
    fn statistics_lines(
        stats: &Statistics,
        quantized: bool,
        aggregate: bool,
        dark: bool,
        plain: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if stats.finite != stats.total {
            lines.push(Line::raw(format!(
                "{} finite · {} NaN/Inf excluded",
                stats.finite,
                stats.total - stats.finite
            )));
        }
        if !quantized && stats.finite > 0 {
            lines.push(Line::raw(format!("Min   {:.6}", stats.minimum)));
            lines.push(Line::raw(format!("Max   {:.6}", stats.maximum)));
            lines.push(Line::raw(format!("Mean  {:.6}", stats.mean)));
        }
        let multiple = quantized && !stats.metadata.is_empty();
        let height = if multiple { 3 } else { 4 };
        lines.extend(Self::histogram_lines(
            stats,
            if quantized && aggregate {
                "Codes"
            } else {
                "Histogram · 16 equal-width bins"
            },
            height,
            quantized,
            dark,
            plain,
            0,
        ));
        if quantized {
            for (series, (label, metadata)) in stats.metadata.iter().enumerate() {
                lines.push(Line::raw(format!(
                    "  {}",
                    "─".repeat(stats.histogram.len() * 2)
                )));
                lines.push(Line::default());
                lines.extend(Self::histogram_lines(
                    metadata,
                    &format!("{label} · {} values", metadata.total),
                    height,
                    true,
                    dark,
                    plain,
                    series + 1,
                ));
            }
        }
        lines
    }

    pub fn draw_hover(
        &self,
        frame: &mut ratatui::Frame<'_>,
        bounds: ratatui::layout::Rect,
        dark: bool,
        plain: bool,
    ) {
        use ratatui::{
            layout::Rect,
            widgets::{Block, Clear, Paragraph},
        };
        if !self.hover.visible(Instant::now()) || bounds.width < 20 || bounds.height < 6 {
            return;
        }
        let Some(target) = self.hover.target() else {
            return;
        };
        let Some((mouse_x, mouse_y)) = self.hover_anchor else {
            return;
        };
        let label = match target.axis {
            Axis::Tensor { .. } => "Tensor".to_owned(),
            Axis::Row(row) => format!("Row {row}"),
            Axis::Column(col) => {
                let header = self.header(col);
                if header.parse::<u64>().is_ok() {
                    format!("Column {header}")
                } else {
                    header
                }
            }
        };
        let lines = match self.hover.result() {
            None => vec![Line::raw(match self.hover.progress_percent() {
                100 if target.quantized && !matches!(target.axis, Axis::Column(_)) => {
                    "Calculating scale histograms…".to_owned()
                }
                percent => format!("Calculating statistics… {percent}%"),
            })],
            Some(Err(error)) => vec![Line::raw(format!("Statistics unavailable: {error}"))],
            Some(Ok(stats)) => Self::statistics_lines(
                stats,
                target.quantized,
                !matches!(target.axis, Axis::Column(_)),
                dark,
                plain,
            ),
        };
        let title = Line::raw(format!(" {label} · {} values ", target.count()));
        let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
        let width = (content_width + 2)
            .max(title.width())
            .saturating_add(2)
            .min(usize::from(bounds.width.saturating_sub(2))) as u16;
        let height = (lines.len() as u16 + 2).min(bounds.height.saturating_sub(2));
        let anchor_x = bounds.x.saturating_add(2).saturating_add(mouse_x as u16);
        let anchor_y = bounds.y.saturating_add(1).saturating_add(mouse_y as u16);
        let x = anchor_x
            .min(bounds.right().saturating_sub(width + 1))
            .max(bounds.x + 1);
        let y = if anchor_y.saturating_add(height + 1) < bounds.bottom() {
            anchor_y + 1
        } else {
            anchor_y
                .saturating_sub(height)
                .clamp(bounds.y + 1, bounds.bottom() - height - 1)
        };
        let area = Rect::new(x, y, width, height);
        let style = if plain {
            Style::default()
        } else {
            Style::default()
                .bg(if dark {
                    Color::Rgb(27, 32, 39)
                } else {
                    Color::Rgb(250, 251, 253)
                })
                .fg(if dark {
                    Color::Rgb(225, 229, 235)
                } else {
                    Color::Rgb(40, 49, 61)
                })
        };
        let border = if plain {
            style
        } else {
            style.fg(if dark {
                Color::Rgb(169, 197, 216)
            } else {
                Color::Rgb(87, 115, 139)
            })
        };
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).style(style).block(
                Block::bordered()
                    .style(style)
                    .border_style(border)
                    .title(title),
            ),
            area,
        );
    }

    pub fn render(&self, dark: bool) -> Text<'static> {
        let (rows, _, _) = self.dimensions();
        let mut lines = Vec::new();
        let vr = self.height.saturating_sub(1).max(1);
        let columns = self.columns();
        let gutter = self.gutter();
        let mut header = vec![Span::raw(" ".repeat(gutter))];
        for (index, &(col, cell_width)) in columns.iter().enumerate() {
            if index > 0 {
                header.push(Span::styled(
                    " ".repeat(COLUMN_GAP),
                    self.separator_style(columns[index - 1].0, col, dark),
                ));
            }
            header.push(Span::styled(
                format!("{:>cell_width$}", self.header(col)),
                if self.quantized {
                    self.block_style(col, dark).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                },
            ));
        }
        lines.push(Line::from(header));
        for row in self.top..rows.min(self.top + vr as u64) {
            let mut spans = vec![Span::styled(
                format!("{row:>width$}  ", width = gutter - 2),
                Style::default().fg(Color::DarkGray),
            )];
            for (index, &(col, cell_width)) in columns.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(
                        " ".repeat(COLUMN_GAP),
                        self.separator_style(columns[index - 1].0, col, dark),
                    ));
                }
                let display = self.cell_text(row, col);
                let mut style = if self.quantized {
                    self.block_style(col, dark)
                } else {
                    Style::default()
                };
                if (row, col) == (self.row, self.column) {
                    style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
                }
                spans.push(Span::styled(format!("{display:>cell_width$}"), style));
            }
            lines.push(Line::from(spans));
        }
        Text::from(lines)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn quantized_fixture(vector: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        quantized_fixture_kind(vector, 8)
    }
    pub(crate) fn quantized_fixture_kind(
        vector: bool,
        kind: u32,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quantized.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(7u64.to_le_bytes());
        bytes.extend(b"weights");
        bytes.extend((if vector { 1u32 } else { 2u32 }).to_le_bytes());
        bytes.extend(64u64.to_le_bytes());
        if !vector {
            bytes.extend(2u64.to_le_bytes());
        }
        bytes.extend(kind.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.resize(bytes.len().next_multiple_of(32), 0);
        for block in 0..if vector { 2 } else { 4 } {
            bytes.extend(
                half::f16::from_f32((block + 1) as f32)
                    .to_bits()
                    .to_le_bytes(),
            );
            if kind == 3 {
                bytes.extend(
                    half::f16::from_f32(-((block + 1) as f32))
                        .to_bits()
                        .to_le_bytes(),
                );
                bytes.extend((0..16).map(|q| q as u8 | (q as u8) << 4));
            } else {
                bytes.extend((0..32).map(|q| (q as i8 - 16) as u8));
            }
        }
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn iq4_xs_values_raw_blocks_and_statistics() {
        use std::sync::atomic::AtomicBool;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iq4_xs.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(7u64.to_le_bytes());
        bytes.extend(b"weights");
        bytes.extend(2u32.to_le_bytes());
        bytes.extend(256u64.to_le_bytes());
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(23u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.resize(bytes.len().next_multiple_of(32), 0);
        let scales = [0u8, 1, 15, 31, 32, 33, 47, 63];
        for block in 0..2 {
            bytes.extend(
                half::f16::from_f32(if block == 0 { 1.0 } else { -0.5 })
                    .to_bits()
                    .to_le_bytes(),
            );
            let codes: Vec<_> = if block == 0 {
                scales.to_vec()
            } else {
                scales.iter().rev().copied().collect()
            };
            let high = codes.iter().enumerate().fold(0u16, |bits, (i, code)| {
                bits | (u16::from(code >> 4) << (2 * i))
            });
            bytes.extend(high.to_le_bytes());
            for pair in codes.chunks_exact(2) {
                bytes.push((pair[0] & 15) | (pair[1] & 15) << 4);
            }
            for group in 0..8 {
                for j in 0..16 {
                    bytes.push(((j + group) % 16) | (((15 - j + 2 * group) % 16) << 4));
                }
            }
        }
        std::fs::write(&path, &bytes).unwrap();
        let mut view = TensorView::open(&path, 0).unwrap();
        let data = &view.data;
        // Reference outputs for scale -32 and each of the 16 nonlinear codes.
        let reference = [
            4064., 3328., 2656., 2080., 1568., 1120., 704., 320., -32., -416., -800., -1216.,
            -1696., -2208., -2848., -3616.,
        ];
        for block in 0..2u64 {
            assert_eq!(
                data.quantized_number(block, 0).unwrap(),
                if block == 0 { 15360. } else { 47104. }
            );
            for group in 0..8usize {
                let scale = scales[if block == 0 { group } else { 7 - group }];
                assert_eq!(
                    data.quantized_number(block, group + 1).unwrap(),
                    f64::from(scale)
                );
                for j in 0..32 {
                    let code = if j < 16 {
                        (j + group) % 16
                    } else {
                        (31 - j + 2 * group) % 16
                    };
                    let index = block * 256 + (group * 32 + j) as u64;
                    let expected = reference[code] * (f64::from(scale) - 32.) / -32.
                        * if block == 0 { 1. } else { -0.5 };
                    assert_eq!(data.number(index).unwrap(), expected, "index {index}");
                    assert_eq!(
                        data.quantized_number(block, 9 + group * 32 + j).unwrap(),
                        code as f64
                    );
                }
            }
        }
        assert!(data.value(512).is_err());
        assert!(data.quantized_number(2, 0).is_err());
        assert!(data.quantized_number(0, 265).is_err());
        let target = Target {
            axis: Axis::Row(0),
            plane: 0,
            quantized: true,
            rows: 2,
            columns: 256,
        };
        let stats = Statistics::calculate(target, data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(stats.histogram, [16; 16]);
        assert_eq!(stats.metadata.len(), 2);
        assert_eq!(
            (stats.metadata[0].0, stats.metadata[0].1.total),
            ("Scales", 1)
        );
        assert_eq!(
            (stats.metadata[1].0, stats.metadata[1].1.total),
            ("Subscales", 8)
        );
        assert_eq!(stats.metadata[1].1.histogram.iter().sum::<u64>(), 8);
        view.resize(120, 12);
        view.column = 255;
        let toggle = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        view.key(toggle);
        assert_eq!(view.dimensions(), (2, 265, 1));
        assert_eq!(view.coordinates(), "[0, 255]");
        assert_eq!(view.header(0), "B0 Scale");
        assert_eq!(view.header(1), "B0.0 Scale");
        assert_eq!(view.header(8), "B0.7 Scale");
        assert_eq!(view.header(9), "0");
        assert_eq!(view.header(264), "255");
        view.key(toggle);
        assert_eq!(view.column, 255);
        drop(view);
        bytes.pop();
        std::fs::write(&path, bytes).unwrap();
        assert!(TensorData::open(&path, 0).is_err());
    }

    #[test]
    fn statistics_hover_tracks_headers_rows_and_scrolled_quantized_blocks() {
        let (_dir, path) = quantized_fixture(false);
        let mut view = TensorView::open(&path, 0).unwrap();
        view.resize(120, 8);
        view.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        let target = view.statistics_target(Some((view.gutter(), 0))).unwrap();
        assert_eq!(target.axis, Axis::Column(0));
        assert!(target.quantized);
        assert_eq!((target.rows, target.columns), (2, 64));
        assert_eq!(
            view.statistics_target(Some((0, 2))).unwrap().axis,
            Axis::Row(1)
        );
        assert!(view.statistics_target(Some((view.gutter(), 1))).is_none());
        view.left = 33;
        let target = view.statistics_target(Some((view.gutter(), 0))).unwrap();
        assert_eq!(target.axis, Axis::Column(33));
        assert_eq!(view.header(33), "B1 Scale");
        assert_eq!(view.header(34), "32");
    }

    #[test]
    fn quantized_tooltips_only_show_separate_code_scale_and_offset_histograms() {
        use std::sync::atomic::AtomicBool;
        let (_dir, path) = quantized_fixture_kind(false, 3);
        let data = TensorData::open(&path, 0).unwrap();
        let target = Target {
            axis: Axis::Row(0),
            plane: 0,
            quantized: true,
            rows: 2,
            columns: 64,
        };
        let stats = Statistics::calculate(target, &data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(stats.metadata.len(), 2);
        assert_eq!(stats.metadata[0].0, "Scales");
        assert_eq!(stats.metadata[1].0, "Offsets");
        assert_eq!(stats.metadata[0].1.histogram.iter().sum::<u64>(), 2);
        assert_eq!(stats.metadata[1].1.histogram.iter().sum::<u64>(), 2);
        assert_eq!(data.quantized_field(0, 0).unwrap(), "15360");
        assert_eq!(data.quantized_field(0, 1).unwrap(), "48128");
        let lines = TensorView::statistics_lines(&stats, true, true, true, false);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Codes"));
        assert!(text.contains("Scales · 2 values"));
        assert!(text.contains("Offsets · 2 values"));
        assert!(!text.contains("Current slice"));
        assert!(!lines.iter().any(|line| ["Min ", "Max ", "Mean "]
            .iter()
            .any(|label| line.to_string().starts_with(label))));
    }

    #[test]
    fn quantized_blocks_show_scales_codes_and_stable_selection() {
        let (_dir, path) = quantized_fixture(false);
        let mut view = TensorView::open(&path, 0).unwrap();
        view.resize(1000, 10);
        view.row = 1;
        view.column = 35;
        let toggle = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        view.key(toggle);
        assert!(view.quantized);
        assert_eq!(view.coordinates(), "[1, 35]");
        assert_eq!(view.column, 37);
        assert_eq!(view.header(0), "B0 Scale");
        assert_eq!(view.header(32), "31");
        assert_eq!(view.header(33), "B1 Scale");
        assert_eq!(view.header(34), "32");
        assert_eq!(view.cell_text(0, 0), "15360");
        assert_eq!(view.cell_text(0, 1), "-16");
        assert_eq!(view.cell_text(0, 32), "15");
        assert_eq!(view.cell_text(0, 33), "16384");
        let header = view.render(true).lines[0].to_string();
        assert!(header.contains("B0 Scale"));
        assert!(header.contains("B1 Scale"));
        for dark in [false, true] {
            assert_ne!(view.block_style(0, dark).bg, view.block_style(1, dark).bg);
            assert_eq!(view.block_style(1, dark).bg, view.block_style(32, dark).bg);
            assert_ne!(view.block_style(1, dark).bg, view.block_style(34, dark).bg);
        }
        view.key(toggle);
        assert!(!view.quantized);
        assert_eq!(view.coordinates(), "[1, 35]");
        assert_eq!(view.column, 35);
        view.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(view.is_editing());
    }

    #[test]
    fn quantized_vector_toggle_and_horizontal_scrolling_keep_coordinates() {
        let (_dir, path) = quantized_fixture(true);
        let mut view = TensorView::open(&path, 0).unwrap();
        view.resize(55, 8);
        view.row = 35;
        let toggle = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        view.key(toggle);
        assert_eq!(view.coordinates(), "[35]");
        assert_eq!(view.dimensions(), (1, 66, 1));
        view.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(view.coordinates(), "[63]");
        assert_eq!(view.columns().last().unwrap().0, 65);
        view.key(toggle);
        assert_eq!(view.coordinates(), "[63]");
        assert_eq!(view.row, 63);
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tensor.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(7u64.to_le_bytes());
        bytes.extend(b"weights");
        bytes.extend(3u32.to_le_bytes());
        for dim in [4u64, 3, 2] {
            bytes.extend(dim.to_le_bytes());
        }
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.resize(bytes.len().next_multiple_of(32), 0);
        for value in 0..24 {
            bytes.extend((value as f32).to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }
    #[test]
    fn tensor_grid_uses_fixed_decimals_and_fits_wide_columns() {
        let (_dir, path) = fixture();
        let mut bytes = std::fs::read(&path).unwrap();
        let payload = bytes.len() - 24 * 4;
        bytes[payload..payload + 4].copy_from_slice(&0.12345678f32.to_le_bytes());
        bytes[payload + 4..payload + 8].copy_from_slice(&1e12f32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let mut view = TensorView::open(&path, 0).unwrap();
        view.resize(80, 12);
        assert_eq!(view.value_text(0), "0.123457");
        assert_eq!(view.value_text(1), format!("{:.6}", 1e12f32));
        let text = view.render(true);
        assert!(text.lines[1].to_string().contains("0.123457"));
        assert!(text.lines[1].to_string().contains("999999995904.000000"));
        assert!(!text
            .lines
            .iter()
            .any(|line| line.to_string().contains(" = ")));
        assert!(!text.lines[0].to_string().contains("F32"));
        assert_eq!(view.columns()[0].1, "8.000000".len());
        assert_eq!(view.columns()[1].1, "999999995904.000000".len());
        assert!(text.lines[1]
            .to_string()
            .contains("0.123457  999999995904.000000"));
    }

    #[test]
    fn tensor_grid_navigates_cells_slices_and_mouse_selection() {
        let (_dir, path) = fixture();
        let mut view = TensorView::open(&path, 0).unwrap();
        view.resize(70, 12);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        view.key(key(KeyCode::Right));
        view.key(key(KeyCode::Down));
        assert_eq!(view.coordinates(), "[0, 1, 1]");
        assert_eq!(view.value_text(5), "5.000000");
        view.key(key(KeyCode::Char(']')));
        assert_eq!(view.coordinates(), "[1, 1, 1]");
        assert_eq!(view.value_text(17), "17.000000");
        view.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        for c in "2,3".chars() {
            view.key(key(KeyCode::Char(c)));
        }
        view.key(key(KeyCode::Enter));
        assert_eq!(view.coordinates(), "[1, 2, 3]");
        assert_eq!(view.value_text(23), "23.000000");
        view.key(key(KeyCode::Home));
        view.key(key(KeyCode::Up));
        view.click(view.gutter() + view.columns()[0].1 + COLUMN_GAP, 1);
        assert_eq!(view.coordinates(), "[1, 0, 1]");
        assert_eq!(view.value_text(13), "13.000000");
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        assert!(document.can_open("mdcat-tensor:0"));
        let explorer = document.open_tensor(0, Default::default()).unwrap();
        assert!(explorer.tensor.is_some());
        assert!(explorer.json.is_none());
        assert_eq!(explorer.title.as_deref(), Some("weights · F32 · 2 × 3 × 4"));
    }
}
