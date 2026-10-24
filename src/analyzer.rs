//! Interactive Markdown viewer.

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use pulldown_cmark::Parser;
use pulldown_cmark_mdcat::ratatui::{RenderOptions as TuiRenderOptions, Renderer};
use pulldown_cmark_mdcat::resources::ResourceUrlHandler;
use pulldown_cmark_mdcat::{
    expand_tabs, markdown_options, substitute_emoji, Environment, Settings,
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Text},
    widgets::{
        Block, BorderType, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};

pub fn run(
    filename: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
    columns: Option<u16>,
    no_colour: bool,
) -> Result<()> {
    let (base_dir, input) =
        mdcat::read_input(filename).with_context(|| format!("Failed to read {filename}"))?;
    let mut query = crate::query::Query::default();
    let mut response = ResponseWindow {
        no_colour,
        animate_resize: true,
        animate_text: true,
        ..Default::default()
    };
    let document = input.clone();
    let (body, frontmatter) = pulldown_cmark_mdcat::split_frontmatter(&input);
    let with_frontmatter_table;
    let input = if options.frontmatter {
        match frontmatter
            .and_then(|(format, raw)| pulldown_cmark_mdcat::frontmatter_table_markdown(format, raw))
        {
            Some(table) => {
                with_frontmatter_table = format!("{table}\n{body}");
                with_frontmatter_table.as_str()
            }
            None => body,
        }
    } else {
        body
    };
    let input = match options.tabs {
        Some(width) => expand_tabs(input, width),
        None => input.into(),
    };
    let environment = Environment::for_local_directory(&base_dir)?;
    let response_renderer = Renderer::new(
        TuiRenderOptions::default()
            .syntax_set(settings.syntax_set)
            .theme(settings.theme.clone())
            .syntax_theme(settings.syntax_theme.clone())
            .environment(environment.clone())
            .resource_handler(resources),
    );
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        let mut text = Text::default();
        let mut rendered_width = None;
        let mut scroll = 0usize;
        let mut dragging_scrollbar = false;
        let animation_started = std::time::Instant::now();
        loop {
            query.poll();
            let size = terminal.size()?;
            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            let width = area.width.saturating_sub(4).max(1);
            let height = usize::from(area.height.saturating_sub(2));
            let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let tick = (animation_started.elapsed().as_millis() / 100) % spinner.len() as u128;
            response.refresh(area, &query, spinner[tick as usize], &response_renderer);
            response.advance_resize();
            let width = match columns {
                Some(0) => u16::MAX,
                Some(limit) => width.min(limit),
                None => width.min(80),
            };
            if rendered_width != Some(width) {
                let settings = Settings {
                    terminal_size: pulldown_cmark_mdcat::TerminalSize {
                        columns: width,
                        ..settings.terminal_size
                    },
                    terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi
                        .capabilities(),
                    syntax_set: settings.syntax_set,
                    theme: settings.theme.clone(),
                    syntax_theme: settings.syntax_theme.clone(),
                };
                let parse_options = markdown_options(options.smart_punctuation);
                let toc = if options.toc {
                    mdcat::toc::build_toc_events(&input, parse_options, None)
                } else {
                    Vec::new()
                };
                let events = toc
                    .into_iter()
                    .chain(Parser::new_ext(&input, parse_options));
                text = if options.emoji {
                    pulldown_cmark_mdcat::ratatui::push_text(
                        &settings,
                        &environment,
                        resources,
                        substitute_emoji(events),
                    )?
                } else {
                    pulldown_cmark_mdcat::ratatui::push_text(
                        &settings,
                        &environment,
                        resources,
                        events,
                    )?
                };
                if no_colour {
                    for line in &mut text.lines {
                        line.style = Style::default();
                        for span in &mut line.spans {
                            span.style = Style::default();
                        }
                    }
                }
                rendered_width = Some(width);
            }
            let max_scroll = text.lines.len().saturating_sub(height.max(1));
            scroll = scroll.min(max_scroll);
            terminal.draw(|frame| {
                // Slice with usize so documents beyond 65,535 lines remain navigable.
                let visible = Text::from(
                    text.lines
                        .iter()
                        .skip(scroll)
                        .take(height)
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                frame.render_widget(
                    Paragraph::new(visible).block(document_block(
                        filename,
                        (scroll + 1).min(text.lines.len()),
                        text.lines.len(),
                        no_colour,
                    )),
                    area,
                );

                if max_scroll > 0 && area.width >= 5 && area.height > 2 {
                    let track = ratatui::layout::Rect::new(
                        area.right() - 2,
                        area.y + 1,
                        1,
                        area.height - 2,
                    );
                    let mut state = ScrollbarState::new(max_scroll + 1)
                        .position(scroll)
                        .viewport_content_length(height);
                    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None)
                        .track_symbol(Some("│"))
                        .thumb_symbol("┃")
                        .track_style(if no_colour {
                            Style::default()
                        } else {
                            Style::default().fg(Color::Rgb(80, 80, 80))
                        })
                        .thumb_style(if no_colour {
                            Style::default()
                        } else {
                            Style::default().fg(Color::Rgb(180, 180, 180))
                        });
                    frame.render_stateful_widget(scrollbar, track, &mut state);
                }
                if response.visible {
                    response.draw(frame);
                }
                if query.open {
                    draw_prompt(frame, area, &query, no_colour);
                }
            })?;
            let refresh_ms = if response.visible
                && (response.animating_caption()
                    || response.resize_started.is_some()
                    || response.animating_answer())
            {
                15
            } else {
                100
            };
            if !event::poll(std::time::Duration::from_millis(refresh_ms))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release && query.open => {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Esc | KeyCode::Tab => query.open = false,
                        KeyCode::Enter if !query.busy() && !query.input.trim().is_empty() => {
                            query.submit(&document);
                            query.open = false;
                            response.visible = true;
                            response.initialized = false;
                            response.moved = false;
                            response.wide_started = None;
                            response.lines.clear();
                            response.scroll = 0;
                        }
                        KeyCode::Backspace => {
                            query.input.pop();
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                && query.input.chars().count()
                                    < crate::query::MAX_QUESTION_CHARS =>
                        {
                            query.input.push(c);
                        }
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release && response.visible => {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => {
                            response.visible = false;
                            response.drag = None;
                        }
                        KeyCode::Char('?') => query.open = true,
                        KeyCode::Char('l')
                            if key.modifiers.contains(KeyModifiers::CONTROL) && !query.busy() =>
                        {
                            query = crate::query::Query::default();
                            response.visible = false;
                        }
                        KeyCode::Left | KeyCode::Char('h') => response.move_by(-2, 0, area),
                        KeyCode::Right | KeyCode::Char('l') => response.move_by(2, 0, area),
                        KeyCode::Up | KeyCode::Char('k') => response.move_by(0, -1, area),
                        KeyCode::Down | KeyCode::Char('j') => response.move_by(0, 1, area),
                        KeyCode::PageUp => response.scroll = response.scroll.saturating_sub(10),
                        KeyCode::PageDown => response.scroll = response.scroll.saturating_add(10),
                        KeyCode::Home => response.scroll = 0,
                        KeyCode::End => response.scroll = usize::MAX,
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('?') => {
                        query.open = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        scroll = scroll.saturating_add(height.max(1))
                    }
                    KeyCode::PageUp | KeyCode::Char('b') => {
                        scroll = scroll.saturating_sub(height.max(1))
                    }
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => scroll = max_scroll,
                    _ => {}
                },
                Event::Mouse(_) if query.open => {}
                Event::Mouse(mouse) if response.visible => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.row == response.area.y
                            && response.area.contains((mouse.column, mouse.row).into()) =>
                    {
                        response.drag = Some((mouse.column - response.area.x, 0));
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some((x, y)) = response.drag {
                            response.moved = true;
                            response.area.x = mouse.column.saturating_sub(x);
                            response.area.y = mouse.row.saturating_sub(y);
                            response.fit(area);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => response.drag = None,
                    MouseEventKind::ScrollDown => {
                        response.scroll = response.scroll.saturating_add(3)
                    }
                    MouseEventKind::ScrollUp => response.scroll = response.scroll.saturating_sub(3),
                    _ => {}
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if max_scroll > 0
                            && area.width >= 5
                            && area.height > 2
                            && mouse.column == area.right() - 2
                            && mouse.row > area.y
                            && mouse.row < area.bottom() - 1 =>
                    {
                        dragging_scrollbar = true;
                        scroll = scrollbar_position(mouse.row, area, max_scroll);
                    }
                    MouseEventKind::Drag(MouseButton::Left) if dragging_scrollbar => {
                        scroll = scrollbar_position(mouse.row, area, max_scroll);
                    }
                    MouseEventKind::Up(MouseButton::Left) => dragging_scrollbar = false,
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    })();
    let mouse_restore = execute!(std::io::stdout(), DisableMouseCapture);
    let restore = ratatui::try_restore();
    result?;
    mouse_restore?;
    restore?;
    Ok(())
}

fn centered(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    ratatui::layout::Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw_prompt(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    query: &crate::query::Query,
    no_colour: bool,
) {
    let area = centered(area, 76, 3);
    frame.render_widget(Clear, area);
    let input_width = Text::raw(&query.input).width();
    let offset = input_width.saturating_sub(usize::from(area.width.saturating_sub(3)));
    let title = if query.busy() {
        " Query · waiting for current response · Esc close "
    } else {
        " Ask about document (500 chars) · Enter submit · Esc cancel "
    };
    frame.render_widget(
        Paragraph::new(query.input.as_str())
            .scroll((0, offset.min(u16::MAX as usize) as u16))
            .block(popup_block(no_colour).title(title)),
        area,
    );
    if area.width > 2 && area.height > 2 {
        frame.set_cursor_position((area.x + 1 + (input_width - offset) as u16, area.y + 1));
    }
}

fn scrollbar_position(row: u16, area: ratatui::layout::Rect, max_scroll: usize) -> usize {
    let height = usize::from(area.height.saturating_sub(2));
    let offset = usize::from(row.saturating_sub(area.y + 1)).min(height.saturating_sub(1));
    offset.saturating_mul(max_scroll) / height.saturating_sub(1).max(1)
}

fn document_block(filename: &str, current: usize, total: usize, no_colour: bool) -> Block<'static> {
    let filename = std::path::Path::new(filename)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(filename))
        .to_string_lossy();
    let style = if no_colour {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(180, 180, 180))
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title_style(style)
        .padding(Padding::new(1, 1, 0, 0))
        .title(format!(" {filename} "))
        .title_bottom(Line::raw(format!(" {current} / {total} ──")).right_aligned())
}

fn popup_block<'a>(no_colour: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if no_colour {
            Style::default()
        } else {
            Style::default().fg(Color::Rgb(180, 167, 235))
        })
}

#[derive(Default)]
struct ResponseWindow {
    visible: bool,
    no_colour: bool,
    lines: Vec<Line<'static>>,
    cached_markdown: String,
    cached_width: u16,
    rendered: Text<'static>,
    full_chars: usize,
    animate_text: bool,
    revealed_chars: usize,
    reveal_updated: Option<std::time::Instant>,
    title: String,
    title_started: Option<std::time::Instant>,
    wide_started: Option<std::time::Instant>,
    bounds: ratatui::layout::Rect,
    area: ratatui::layout::Rect,
    initialized: bool,
    moved: bool,
    desired_width: usize,
    animate_resize: bool,
    target_area: ratatui::layout::Rect,
    resize_from: ratatui::layout::Rect,
    resize_started: Option<std::time::Instant>,
    scroll: usize,
    drag: Option<(u16, u16)>,
}

impl ResponseWindow {
    fn fit(&mut self, bounds: ratatui::layout::Rect) {
        self.bounds = bounds;
        let max_width = bounds
            .width
            .saturating_sub(4)
            .clamp(5, 84)
            .min(bounds.width);
        let width = self.desired_width.max(5).min(usize::from(max_width)) as u16;
        let max_height = bounds.height.saturating_sub(4).max(3).min(bounds.height);
        let height = self
            .lines
            .len()
            .max(1)
            .saturating_add(2)
            .min(usize::from(max_height)) as u16;
        let target = if !self.moved {
            centered(bounds, width, height)
        } else {
            ratatui::layout::Rect::new(
                self.area
                    .x
                    .clamp(bounds.x, bounds.right().saturating_sub(width)),
                self.area
                    .y
                    .clamp(bounds.y, bounds.bottom().saturating_sub(height)),
                width,
                height,
            )
        };
        if !self.initialized || !self.animate_resize {
            self.area = target;
            self.target_area = target;
            self.resize_started = None;
            self.initialized = true;
        } else if self.target_area != target {
            self.resize_from = self.area.intersection(bounds);
            self.area = self.resize_from;
            self.target_area = target;
            self.resize_started = Some(std::time::Instant::now());
        }
    }

    fn advance_resize(&mut self) {
        if let Some(start) = self.resize_started {
            let elapsed = start.elapsed();
            self.area = resize_frame(self.resize_from, self.target_area, elapsed);
            if elapsed.as_millis() >= 180 {
                self.resize_started = None;
            }
        }
        if self.animate_resize {
            let width = phased_width(
                self.title_started
                    .map(|start| start.elapsed())
                    .filter(|_| !self.title.is_empty()),
                self.wide_started.map(|start| start.elapsed()),
            );
            self.area.width = width.min(
                self.bounds
                    .width
                    .saturating_sub(4)
                    .clamp(5, 84)
                    .min(self.bounds.width),
            );
            if !self.moved {
                self.area.x = self.bounds.x + self.bounds.width.saturating_sub(self.area.width) / 2;
                self.area.y =
                    self.bounds.y + self.bounds.height.saturating_sub(self.area.height) / 2;
            } else {
                self.area.x = self.area.x.clamp(
                    self.bounds.x,
                    self.bounds.right().saturating_sub(self.area.width),
                );
                self.area.y = self.area.y.clamp(
                    self.bounds.y,
                    self.bounds.bottom().saturating_sub(self.area.height),
                );
            }
        }
    }

    fn move_by(&mut self, x: i16, y: i16, bounds: ratatui::layout::Rect) {
        self.moved = true;
        self.area.x = self.area.x.saturating_add_signed(x);
        self.area.y = self.area.y.saturating_add_signed(y);
        self.fit(bounds);
    }

    fn refresh(
        &mut self,
        bounds: ratatui::layout::Rect,
        query: &crate::query::Query,
        spinner: char,
        renderer: &Renderer<'_>,
    ) {
        if self.title != query.title {
            self.title.clone_from(&query.title);
            self.title_started = Some(std::time::Instant::now());
            self.wide_started = None;
        }
        let max_width = bounds
            .width
            .saturating_sub(4)
            .clamp(5, 84)
            .min(bounds.width);
        let columns = max_width.saturating_sub(2).max(1);
        if query.response.is_empty() {
            self.cached_markdown.clear();
            self.rendered = Text::default();
            self.full_chars = 0;
            self.revealed_chars = 0;
            self.reveal_updated = None;
            self.lines = if query.busy() {
                vec![Line::raw(format!(" {spinner} "))]
            } else {
                Vec::new()
            };
        } else {
            if self.cached_markdown != query.response || self.cached_width != columns {
                self.rendered = Renderer::new(renderer.options().clone().width(columns))
                    .text_from_str(&query.response)
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "Unable to render response Markdown");
                        Text::raw(query.response.clone())
                    });
                // The terminal renderer adds outer blank lines; window borders supply that space.
                while self
                    .rendered
                    .lines
                    .first()
                    .is_some_and(|line| line.to_string().trim().is_empty())
                {
                    self.rendered.lines.remove(0);
                }
                while self
                    .rendered
                    .lines
                    .last()
                    .is_some_and(|line| line.to_string().trim().is_empty())
                {
                    self.rendered.lines.pop();
                }
                if self.no_colour {
                    for line in &mut self.rendered.lines {
                        line.style = Style::default();
                        for span in &mut line.spans {
                            span.style = Style::default();
                        }
                    }
                }
                self.full_chars = self
                    .rendered
                    .lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.chars().count())
                            .sum::<usize>()
                            + 1
                    })
                    .sum();
                self.cached_markdown.clone_from(&query.response);
                self.cached_width = columns;
            }
            if self.animate_text {
                let now = std::time::Instant::now();
                let updated = self.reveal_updated.get_or_insert(now);
                let steps = (now.duration_since(*updated).as_millis() / 15) as usize;
                self.revealed_chars = self
                    .revealed_chars
                    .saturating_add(steps)
                    .min(self.full_chars);
                if self.revealed_chars == self.full_chars {
                    *updated = now;
                } else if steps > 0 {
                    *updated += std::time::Duration::from_millis(steps as u64 * 15);
                }
            } else {
                self.revealed_chars = self.full_chars;
            }
            self.lines = reveal_lines(&self.rendered, self.revealed_chars);
        }
        let long_line = self.rendered.lines.iter().any(|line| line.width() > 62);
        let standard_complete = !self.animate_resize
            || self
                .title_started
                .is_some_and(|start| start.elapsed().as_millis() >= 100);
        if long_line && standard_complete && self.wide_started.is_none() {
            self.wide_started = Some(std::time::Instant::now());
        }
        self.desired_width = if self.wide_started.is_some() {
            84
        } else if !self.title.is_empty() || self.full_chars > 0 {
            64
        } else {
            5
        };
        self.fit(bounds);
    }

    fn animating_answer(&self) -> bool {
        self.animate_text && self.revealed_chars < self.full_chars
    }

    fn caption(&self, elapsed: std::time::Duration) -> String {
        self.title
            .chars()
            .take((elapsed.as_millis() / 40) as usize)
            .collect()
    }

    fn animating_caption(&self) -> bool {
        self.title_started.is_some_and(|start| {
            start.elapsed().as_millis() < self.title.chars().count() as u128 * 40
        })
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        frame.render_widget(Clear, self.area);
        let height = usize::from(self.area.height.saturating_sub(2));
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(height));
        let text = Text::from(
            self.lines
                .iter()
                .skip(self.scroll)
                .take(height)
                .cloned()
                .collect::<Vec<_>>(),
        );
        frame.render_widget(
            Paragraph::new(text).block(if self.title.is_empty() {
                popup_block(self.no_colour)
            } else {
                popup_block(self.no_colour).title(format!(
                    " {} ",
                    self.caption(
                        self.title_started
                            .map(|start| start.elapsed())
                            .unwrap_or_default()
                    )
                ))
            }),
            self.area,
        );
    }
}

fn reveal_lines(text: &Text<'static>, mut remaining: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in &text.lines {
        if remaining == 0 {
            break;
        }
        let mut visible = line.clone();
        visible.spans.clear();
        for span in &line.spans {
            if remaining == 0 {
                break;
            }
            let content = span.content.chars().take(remaining).collect::<String>();
            remaining = remaining.saturating_sub(content.chars().count());
            let mut visible_span = span.clone();
            visible_span.content = content.into();
            visible.spans.push(visible_span);
        }
        lines.push(visible);
        remaining = remaining.saturating_sub(1); // newline
    }
    lines
}

fn phased_width(
    title_elapsed: Option<std::time::Duration>,
    wide_elapsed: Option<std::time::Duration>,
) -> u16 {
    if let Some(elapsed) = wide_elapsed {
        64 + (20 * elapsed.as_millis().min(80) / 80) as u16
    } else if let Some(elapsed) = title_elapsed {
        5 + (59 * elapsed.as_millis().min(100) / 100) as u16
    } else {
        5
    }
}

fn resize_frame(
    from: ratatui::layout::Rect,
    to: ratatui::layout::Rect,
    elapsed: std::time::Duration,
) -> ratatui::layout::Rect {
    let progress = elapsed.as_millis().min(180) as i64;
    let lerp = |start: u16, end: u16| {
        (i64::from(start) + (i64::from(end) - i64::from(start)) * progress / 180) as u16
    };
    ratatui::layout::Rect::new(
        lerp(from.x, to.x),
        lerp(from.y, to.y),
        lerp(from.width, to.width),
        lerp(from.height, to.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_expands_in_two_timed_phases() {
        use std::time::Duration;
        assert_eq!(phased_width(None, None), 5);
        assert_eq!(phased_width(Some(Duration::ZERO), None), 5);
        assert_eq!(phased_width(Some(Duration::from_millis(50)), None), 34);
        assert_eq!(phased_width(Some(Duration::from_millis(100)), None), 64);
        assert_eq!(phased_width(Some(Duration::from_secs(1)), None), 64);
        assert_eq!(
            phased_width(Some(Duration::from_secs(1)), Some(Duration::ZERO)),
            64
        );
        assert_eq!(
            phased_width(
                Some(Duration::from_secs(1)),
                Some(Duration::from_millis(40))
            ),
            74
        );
        assert_eq!(
            phased_width(
                Some(Duration::from_secs(1)),
                Some(Duration::from_millis(80))
            ),
            84
        );
    }
    #[test]
    fn response_markdown_keeps_styling_lists_and_code_blocks() {
        let mut window = ResponseWindow::default();
        let mut query = crate::query::Query::default();
        query.response = "**Important** and `code`.\n\n- Item\n\n```rust\nfn main() {}\n```".into();
        window.refresh(
            ratatui::layout::Rect::new(0, 0, 120, 30),
            &query,
            '⠋',
            &Renderer::default(),
        );
        let plain = window
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("Important"));
        assert!(plain.contains("Item"));
        assert!(plain.contains("fn main()"));
        assert!(!plain.contains("**") && !plain.contains("```") && !plain.contains("`code`"));
        assert!(window
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("Important")
                && span
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)));
        assert!(window
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn response_typewriter_reveals_styled_unicode_without_markdown_markers() {
        let mut window = ResponseWindow {
            animate_text: true,
            ..Default::default()
        };
        let mut query = crate::query::Query::default();
        query.response = "**短い text**".into();
        let bounds = ratatui::layout::Rect::new(0, 0, 120, 30);
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert!(window.animating_answer());
        window.lines = reveal_lines(&window.rendered, 2);
        assert_eq!(window.lines[0].to_string(), "短い");
        assert!(window.lines[0].spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        window.reveal_updated = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert!(!window.animating_answer());
        assert_eq!(window.lines[0].to_string(), "短い text");
    }

    #[test]
    fn resize_animation_has_intermediate_frames_and_finishes_exactly() {
        let small = ratatui::layout::Rect::new(57, 13, 5, 3);
        let large = ratatui::layout::Rect::new(18, 5, 84, 20);
        assert_eq!(resize_frame(small, large, std::time::Duration::ZERO), small);
        let middle = resize_frame(small, large, std::time::Duration::from_millis(90));
        assert!(middle.width > small.width && middle.width < large.width);
        assert!(middle.height > small.height && middle.height < large.height);
        assert_eq!(
            resize_frame(small, large, std::time::Duration::from_millis(180)),
            large
        );
        assert_eq!(
            resize_frame(large, small, std::time::Duration::from_secs(1)),
            small
        );
    }

    #[test]
    fn caption_reveals_one_unicode_character_every_40_ms() {
        let window = ResponseWindow {
            title: "短い summary".into(),
            ..Default::default()
        };
        assert_eq!(window.caption(std::time::Duration::ZERO), "");
        assert_eq!(window.caption(std::time::Duration::from_millis(40)), "短");
        assert_eq!(window.caption(std::time::Duration::from_millis(80)), "短い");
        assert_eq!(
            window.caption(std::time::Duration::from_secs(10)),
            window.title
        );
    }

    #[test]
    fn response_starts_small_and_expands_to_title_and_answer_width() {
        let mut window = ResponseWindow::default();
        let bounds = ratatui::layout::Rect::new(0, 0, 120, 30);
        let mut query = crate::query::Query::default();
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert_eq!((window.area.width, window.area.height), (5, 3));
        query.title = "Secret Ingredient".into();
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert_eq!(window.area.width, 64);
        query.response = "A detailed answer with several paragraphs. ".repeat(30);
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert!(window.area.width > Text::raw(&query.title).width() as u16 + 4);
        assert!(window.area.width <= 84);
        assert!(window.area.height > 3);
        assert!(window.area.right() <= bounds.right());
        assert!(window.area.bottom() <= bounds.bottom());
    }

    #[test]
    fn response_height_fits_wrapped_content_and_caps_at_screen() {
        let mut window = ResponseWindow::default();
        let bounds = ratatui::layout::Rect::new(0, 0, 120, 30);
        let mut query = crate::query::Query::default();
        query.question = "Question?".into();
        query.response = "Short answer.".into();
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert_eq!(window.area.height, 3); // answer and two borders
        assert!(!window
            .lines
            .iter()
            .any(|line| line.to_string().contains("Question?")));
        query.response = "A long answer that wraps. ".repeat(200);
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert_eq!(window.area.height, 26);
        assert!(window.lines.len() > usize::from(window.area.height - 2));
        query.response = "Short again.".into();
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        assert_eq!(window.area.height, 3);
        let narrow = ratatui::layout::Rect::new(0, 0, 20, 20);
        window.refresh(narrow, &query, '⠋', &Renderer::default());
        assert_eq!(usize::from(window.area.height), window.lines.len() + 2);
    }

    #[test]
    fn response_stays_within_bounds_when_moved_and_resized() {
        let mut window = ResponseWindow::default();
        let bounds = ratatui::layout::Rect::new(0, 0, 120, 30);
        window.fit(bounds);
        let centered = window.area;
        window.move_by(2, 1, bounds);
        assert_eq!(window.area.x, centered.x + 2);
        assert_eq!(window.area.y, centered.y + 1);
        window.move_by(i16::MAX, i16::MAX, bounds);
        assert!(bounds.contains((window.area.right() - 1, window.area.bottom() - 1).into()));
        for (width, height) in [(30, 8), (1, 1), (0, 0), (120, 30)] {
            let bounds = ratatui::layout::Rect::new(0, 0, width, height);
            window.fit(bounds);
            assert!(window.area.right() <= bounds.right());
            assert!(window.area.bottom() <= bounds.bottom());
        }
    }

    #[test]
    fn moving_response_restores_document_underneath() {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut window = ResponseWindow::default();
        let bounds = ratatui::layout::Rect::new(0, 0, 120, 30);
        window.fit(bounds);
        let mut query = crate::query::Query::default();
        query.response = "A document answer.".into();
        query.title = "Cardamom recipe summary".into();
        window.refresh(bounds, &query, '⠋', &Renderer::default());
        let original = window.area;
        window.title_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
        let draw = |frame: &mut ratatui::Frame<'_>, window: &mut ResponseWindow| {
            frame.render_widget(
                Paragraph::new(".".repeat(120 * 30)).wrap(ratatui::widgets::Wrap { trim: false }),
                bounds,
            );
            window.draw(frame);
        };
        terminal.draw(|frame| draw(frame, &mut window)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(original.x, original.y)].symbol(),
            "╭"
        );
        let buffer = terminal.backend().buffer();
        let caption = (original.x..original.right())
            .map(|x| buffer[(x, original.y)].symbol())
            .collect::<String>();
        assert!(caption.contains("Cardamom recipe summary"));
        assert!(!caption.contains("arrows move"));
        assert_eq!(
            buffer[(original.x, original.y)].fg,
            Color::Rgb(180, 167, 235)
        );
        assert_eq!(buffer[(original.x, original.bottom() - 1)].symbol(), "╰");
        for x in original.x + 1..original.right() - 1 {
            assert_eq!(buffer[(x, original.bottom() - 1)].symbol(), "─");
        }
        window.move_by(2, 1, bounds);
        terminal.draw(|frame| draw(frame, &mut window)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(original.x, original.y)].symbol(), ".");
        assert_eq!(buffer[(window.area.x, window.area.y)].symbol(), "╭");
    }
}
