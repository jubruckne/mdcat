//! Interactive Markdown viewer.

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, LinkType, Parser, Tag, TagEnd};
use pulldown_cmark_mdcat::ratatui::{RenderOptions as TuiRenderOptions, Renderer};
use pulldown_cmark_mdcat::resources::ResourceUrlHandler;
use pulldown_cmark_mdcat::{markdown_options, substitute_emoji, Settings};
use ratatui::{
    style::{Color, Style},
    text::{Line, Text},
    widgets::{
        Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
};

pub fn run(
    filename: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
    columns: Option<u16>,
    no_colour: bool,
    model: Option<mdcat::args::QueryModel>,
) -> Result<()> {
    let working_directory = std::env::current_dir()?;
    let mut document = crate::navigation::Document::load(filename.into(), options)?;
    let mut query = crate::query::Query::new(model);
    let mut response = ResponseWindow {
        no_colour,
        animate_resize: true,
        animate_text: true,
        ..Default::default()
    };
    let mut response_renderer = Renderer::new(
        TuiRenderOptions::default()
            .syntax_set(settings.syntax_set)
            .theme(settings.theme.clone())
            .syntax_theme(settings.syntax_theme.clone())
            .environment(document.environment.clone())
            .resource_handler(resources),
    );
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        let mut text = Text::default();
        let mut rendered_width = None;
        let mut scroll = 0usize;
        let mut dragging_scrollbar = false;
        let mut links = Vec::<pulldown_cmark_mdcat::ratatui::LinkRegion>::new();
        let mut selected: Option<String> = None;
        let mut mouse_position = None;
        let mut link_picker: Option<LinkPicker> = None;
        let mut titles = Vec::new();
        let mut history: Vec<(crate::navigation::Document, usize, Option<String>)> = Vec::new();
        let mut status = String::new();
        let mut zero_match_notice: Option<(std::time::Instant, String)> = None;
        let mut pending_anchor: Option<String> = None;
        let mut search = Search::default();
        let mut image_picker: Option<ratatui_image::picker::Picker> = None;
        let mut image_state: Option<(
            std::path::PathBuf,
            ratatui_image::protocol::StatefulProtocol,
        )> = None;

        let animation_started = std::time::Instant::now();
        loop {
            query.poll();
            if let Some((started, message)) = &zero_match_notice {
                if status != *message {
                    zero_match_notice = None;
                } else if started.elapsed() >= std::time::Duration::from_millis(1500) {
                    status.clear();
                    zero_match_notice = None;
                }
            }
            if link_picker.as_ref().is_some_and(LinkPicker::expired) {
                link_picker = None;
            }
            let size = terminal.size()?;
            let bounds = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            let (first_parent, area) = document_stack_layout(bounds, history.len());
            let width = area
                .width
                .saturating_sub(if document.diff.is_some() { 5 } else { 4 })
                .max(1);
            let table_width = width;
            let height = usize::from(area.height.saturating_sub(2));
            if let Some(image) = &document.image {
                if image_state
                    .as_ref()
                    .is_none_or(|(path, _)| path != &document.path)
                {
                    let picker = image_picker.get_or_insert_with(|| {
                        pulldown_cmark_mdcat::ratatui::detect_image_picker()
                            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks())
                    });
                    image_state = Some((
                        document.path.clone(),
                        picker.new_resize_protocol(image.clone()),
                    ));
                }
            } else {
                image_state = None;
            }
            if let Some(json) = document.json.as_mut() {
                json.prepare_for_height(height);
            }
            let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let tick = (animation_started.elapsed().as_millis() / 100) % spinner.len() as u128;
            if rendered_width.is_none() {
                let paths = history
                    .iter()
                    .map(|(parent, _, _)| parent.path.as_path())
                    .chain(std::iter::once(document.path.as_path()))
                    .collect::<Vec<_>>();
                titles = document_titles(&paths);
                for (index, doc) in history
                    .iter()
                    .map(|(doc, _, _)| doc)
                    .chain(std::iter::once(&document))
                    .enumerate()
                {
                    if let Some(title) = &doc.title {
                        titles[index].clone_from(title);
                    } else if doc.is_directory {
                        titles[index] = directory_title(&doc.path, &working_directory);
                    }
                }
                response_renderer = Renderer::new(
                    response_renderer
                        .options()
                        .clone()
                        .environment(document.environment.clone()),
                );
            }
            response.refresh(area, &query, spinner[tick as usize], &response_renderer);
            response.advance_resize();
            let width = if document.diff.is_some() {
                width
            } else {
                match columns {
                    Some(0) => u16::MAX,
                    Some(limit) => width.min(limit),
                    None => width.min(80),
                }
            };
            if rendered_width != Some((width, table_width)) {
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
                (text, links) = render_document_with_table_width(
                    &document,
                    &document.input,
                    &settings,
                    resources,
                    options,
                    table_width,
                )?;
                if document.is_directory
                    && selected
                        .as_ref()
                        .is_none_or(|target| !links.iter().any(|link| &link.target == target))
                {
                    selected = links.first().map(|link| link.target.clone());
                }
                if search.pending_until.is_none() {
                    search.rebuild(&text);
                }
                if no_colour {
                    for line in &mut text.lines {
                        line.style = Style::default();
                        for span in &mut line.spans {
                            span.style = Style::default();
                        }
                    }
                }
                if let Some(anchor) = pending_anchor.take() {
                    if let Some(line) = anchor_line_with_table_width(
                        &document,
                        &anchor,
                        &settings,
                        resources,
                        options,
                        table_width,
                    )? {
                        scroll = line;
                        if let Some(source_line) = crate::navigation::source_line(&anchor) {
                            let actual = source_line.min(document.source.lines().count().max(1));
                            status = if actual == source_line {
                                format!(" Source line {actual} ")
                            } else {
                                format!(" Source line {actual} (requested {source_line}) ")
                            };
                        }
                    } else {
                        status = format!("Heading not found: #{anchor}");
                    }
                }
                rendered_width = Some((width, table_width));
            }
            let max_scroll = text.lines.len().saturating_sub(height.max(1));
            if search
                .pending_until
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                if let Some(line) = search.finish_pending(&text) {
                    scroll = line.min(max_scroll);
                    if let Some(json) = document.json.as_mut() {
                        json.select_row(line);
                    }
                }
            }
            scroll = scroll.min(max_scroll);
            let hovered = mouse_position
                .filter(|_| {
                    !query.open && !response.visible && !dragging_scrollbar && link_picker.is_none()
                })
                .and_then(|(column, row)| link_at(&links, area, scroll, column, row))
                .filter(|link| document.can_open(&link.target))
                .map(|link| link.target.as_str());
            terminal.draw(|frame| {
                // Each fixed child covers its parent's body, leaving its title bar visible.
                for (index, (_, position, _)) in history.iter().skip(first_parent).enumerate() {
                    let parent_area = ratatui::layout::Rect::new(
                        bounds.x,
                        bounds.y + index as u16,
                        bounds.width,
                        bounds.height - index as u16,
                    );
                    frame.render_widget(
                        document_block(
                            &titles[first_parent + index],
                            *position + 1,
                            0,
                            no_colour,
                            false,
                        ),
                        parent_area,
                    );
                }
                frame.render_widget(Clear, area);
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
                        &titles[history.len()],
                        (scroll + 1).min(text.lines.len()),
                        text.lines.len(),
                        no_colour,
                        true,
                    )),
                    area,
                );

                if let Some((_, protocol)) = image_state.as_mut() {
                    let image_area = ratatui::layout::Rect::new(
                        area.x.saturating_add(2),
                        area.y.saturating_add(1),
                        area.width.saturating_sub(4),
                        area.height.saturating_sub(2),
                    );
                    if image_area.width > 0 && image_area.height > 0 {
                        frame.render_stateful_widget(
                            ratatui_image::StatefulImage::default(),
                            image_area,
                            protocol,
                        );
                    }
                }

                let picker_target = link_picker.as_ref().and_then(LinkPicker::selected_target);
                if document.is_directory && !no_colour {
                    draw_directory_changes(frame, &document, &links, area, scroll);
                }
                let picker_link = picker_target.as_deref().and_then(|target| {
                    links
                        .iter()
                        .find(|link| same_local_link(&document, &link.target, target))
                });
                draw_link_highlights(
                    frame,
                    &links,
                    area,
                    scroll,
                    if document.is_directory {
                        selected.as_deref()
                    } else {
                        None
                    },
                    picker_link.map(|link| link.target.as_str()).or(hovered),
                    if no_colour {
                        None
                    } else {
                        settings
                            .theme
                            .link_style
                            .get_fg_color()
                            .map(|color| match color {
                                anstyle::Color::Rgb(anstyle::RgbColor(r, g, b)) => {
                                    Color::Rgb(r, g, b)
                                }
                                anstyle::Color::Ansi256(anstyle::Ansi256Color(index)) => {
                                    Color::Indexed(index)
                                }
                                anstyle::Color::Ansi(color) => Color::Indexed(color as u8),
                            })
                    },
                );
                if let Some(json) = document.json.as_ref() {
                    if json.cursor >= scroll && json.cursor < scroll + height {
                        let column = json.focus_column();
                        let available = usize::from(area.width.saturating_sub(4));
                        if column < available {
                            frame.buffer_mut().set_style(
                                ratatui::layout::Rect::new(
                                    area.x + 2 + column as u16,
                                    area.y + 1 + (json.cursor - scroll) as u16,
                                    1,
                                    1,
                                ),
                                Style::default().add_modifier(ratatui::style::Modifier::REVERSED),
                            );
                        }
                    }
                }
                search.highlight(
                    frame,
                    area,
                    scroll,
                    no_colour || crossterm::style::Colored::ansi_color_disabled(),
                );
                let hovered_path =
                    hovered.and_then(|target| hover_path(&document, target, &working_directory));
                let hint_width = area.width.saturating_sub(
                    4 + format!(
                        " {} / {} ──",
                        (scroll + 1).min(text.lines.len()),
                        text.lines.len()
                    )
                    .len() as u16,
                );
                let search_footer = if search.open {
                    Some(search.footer(usize::from(hint_width)))
                } else {
                    None
                };
                let hint = if let Some((hint, _)) = search_footer.as_ref() {
                    hint.as_str()
                } else if let Some(path) = hovered_path.as_deref() {
                    path
                } else if status.is_empty() {
                    if document.json.is_some() {
                        " Enter: fold · c/e: fold/expand all · Tab: links "
                    } else {
                        ""
                    }
                } else {
                    &status
                };
                if area.width > 4 {
                    let hint_style = if let Some((started, _)) = zero_match_notice
                        .as_ref()
                        .filter(|_| search_footer.is_none() && hovered_path.is_none())
                    {
                        let background = settings
                            .syntax_theme
                            .as_ref()
                            .and_then(|theme| theme.settings.background)
                            .map(|color| (color.r, color.g, color.b))
                            .unwrap_or(if settings.theme.is_dark {
                                (28, 28, 28)
                            } else {
                                (255, 255, 255)
                            });
                        zero_match_notice_style(
                            started.elapsed(),
                            background,
                            no_colour || crossterm::style::Colored::ansi_color_disabled(),
                        )
                    } else {
                        Style::default()
                    };
                    frame.render_widget(
                        Line::styled(hint, hint_style),
                        ratatui::layout::Rect::new(
                            area.x + 2,
                            area.bottom().saturating_sub(1),
                            hint_width,
                            1,
                        ),
                    );
                    if let Some((_, cursor)) = search_footer {
                        if cursor < usize::from(hint_width) {
                            frame.set_cursor_position((
                                area.x + 2 + cursor as u16,
                                area.bottom().saturating_sub(1),
                            ));
                        }
                    }
                }
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
                if let Some(picker) = &mut link_picker {
                    let background = settings
                        .syntax_theme
                        .as_ref()
                        .and_then(|theme| theme.settings.background)
                        .map(|color| (color.r, color.g, color.b))
                        .unwrap_or(if settings.theme.is_dark {
                            (28, 28, 28)
                        } else {
                            (255, 255, 255)
                        });
                    picker.draw(frame, bounds, no_colour, background);
                }
            })?;
            let refresh_ms = if link_picker
                .as_ref()
                .is_some_and(|picker| picker.targets.is_empty())
                || zero_match_notice.is_some()
                || response.visible
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
            let mut open_target = None;
            let mut go_back = false;
            let event = event::read()?;
            if let Event::Mouse(mouse) = &event {
                mouse_position = Some((mouse.column, mouse.row));
            }
            let is_mouse = matches!(event, Event::Mouse(_));
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release && link_picker.is_some() => {
                    let picker = link_picker.as_mut().unwrap();
                    let navigate = matches!(
                        key.code,
                        KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Char('j' | 'k')
                            | KeyCode::Tab
                            | KeyCode::BackTab
                            | KeyCode::PageUp
                            | KeyCode::PageDown
                            | KeyCode::Home
                            | KeyCode::End
                    );
                    let mut close = false;
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Esc | KeyCode::Char('q' | 'Q') => close = true,
                        KeyCode::Down | KeyCode::Char('j') => picker.move_by(1),
                        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            picker.move_by(-1)
                        }
                        KeyCode::Tab => picker.move_by(1),
                        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => picker.move_by(-1),
                        KeyCode::PageDown => picker.move_by(
                            isize::try_from(picker.area.height.saturating_sub(2).max(1)).unwrap(),
                        ),
                        KeyCode::PageUp => picker.move_by(
                            -isize::try_from(picker.area.height.saturating_sub(2).max(1)).unwrap(),
                        ),
                        KeyCode::Home => picker.select_edge(false),
                        KeyCode::End => picker.select_edge(true),
                        KeyCode::Enter if !picker.targets.is_empty() => {
                            open_target = picker.selected_target();
                            selected.clone_from(&open_target);
                            close = true;
                        }
                        _ => {}
                    }
                    if navigate {
                        if let Some(line) = picker.document_line(&links, &document, scroll) {
                            scroll = line.saturating_sub(1).min(max_scroll);
                        }
                    }
                    if close {
                        link_picker = None;
                    }
                }
                Event::Mouse(mouse) if link_picker.is_some() => {
                    let picker = link_picker.as_mut().unwrap();
                    match mouse.kind {
                        MouseEventKind::ScrollDown => picker.move_by(1),
                        MouseEventKind::ScrollUp => picker.move_by(-1),
                        MouseEventKind::Moved => {
                            if let Some(index) = picker.item_at(mouse.column, mouse.row) {
                                picker.state.select(Some(index));
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(index) = picker.item_at(mouse.column, mouse.row) {
                                picker.state.select(Some(index));
                                open_target = picker.selected_target();
                                selected.clone_from(&open_target);
                                link_picker = None;
                            } else if !picker.area.contains((mouse.column, mouse.row).into()) {
                                link_picker = None;
                            }
                        }
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release && query.open => {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Esc | KeyCode::Tab => query.open = false,
                        KeyCode::Enter if !query.busy() && !query.input.trim().is_empty() => {
                            query.submit(&document.source);
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
                        KeyCode::Char('?') => query.open_prompt(&document.source),
                        KeyCode::Char('l')
                            if key.modifiers.contains(KeyModifiers::CONTROL) && !query.busy() =>
                        {
                            query = crate::query::Query::new(model);
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
                Event::Key(key) if key.kind != KeyEventKind::Release && search.open => {
                    let old_input = search.input.clone();
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Esc => {
                            search.open = false;
                            search.pending_until = None;
                            search.query.clone_from(&search.original_query);
                            search.rebuild(&text);
                            search.current = search
                                .original_current
                                .filter(|index| *index < search.matches.len());
                            scroll = search.original_scroll.min(max_scroll);
                            status.clone_from(&search.original_status);
                            if let (Some(json), Some(cursor)) =
                                (document.json.as_mut(), search.original_json_cursor)
                            {
                                json.select_row(cursor);
                            }
                        }
                        KeyCode::Enter => {
                            search.open = false;
                            if search.pending_until.is_some() {
                                if let Some(line) = search.finish_pending(&text) {
                                    scroll = line.min(max_scroll);
                                    if let Some(json) = document.json.as_mut() {
                                        json.select_row(line);
                                    }
                                }
                            }
                            status = if search.query.is_empty() {
                                String::new()
                            } else {
                                format!(" {} matches for '{}' ", search.matches.len(), search.query)
                            };
                            if search.matches.is_empty() {
                                if !search.query.is_empty() {
                                    zero_match_notice =
                                        Some((std::time::Instant::now(), status.clone()));
                                }
                                search = Search::default();
                            }
                        }
                        KeyCode::Left => search.move_cursor(false),
                        KeyCode::Right => search.move_cursor(true),
                        KeyCode::Home => search.cursor = 0,
                        KeyCode::End => search.cursor = search.input.len(),
                        KeyCode::Backspace if search.cursor > 0 => {
                            let end = search.cursor;
                            search.move_cursor(false);
                            search.input.drain(search.cursor..end);
                        }
                        KeyCode::Delete if search.cursor < search.input.len() => {
                            let start = search.cursor;
                            search.move_cursor(true);
                            search.input.drain(start..search.cursor);
                            search.cursor = start;
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                && !c.is_control() =>
                        {
                            search.input.insert(search.cursor, c);
                            search.cursor += c.len_utf8();
                        }
                        _ => {}
                    }
                    if search.open && search.input != old_input {
                        search.query.clone_from(&search.input);
                        if let Some(line) = search.schedule(&text) {
                            scroll = line.min(max_scroll);
                            if let Some(json) = document.json.as_mut() {
                                json.select_row(line);
                            }
                        }
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('f')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    search.open = true;
                    search.input.clone_from(&search.query);
                    search.cursor = search.input.len();
                    search.original_query.clone_from(&search.query);
                    search.original_scroll = scroll;
                    search.original_current = search.current;
                    search.original_status.clone_from(&status);
                    search.original_json_cursor = document.json.as_ref().map(|json| json.cursor);
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('n')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if document.is_directory || document.image.is_some() || !document.path.is_file()
                    {
                        status = " No editable file in this window ".into();
                    } else {
                        let was_json_cursor = document.json.as_ref().map(|json| json.cursor);
                        let result = edit_in_nano(&mut terminal, &document.path);
                        match crate::navigation::Document::load(document.path.clone(), options) {
                            Ok(mut reloaded) => {
                                if let (Some(json), Some(cursor)) =
                                    (reloaded.json.as_mut(), was_json_cursor)
                                {
                                    json.select_row(cursor);
                                }
                                document = reloaded;
                                rendered_width = None;
                                selected = None;
                                query = crate::query::Query::new(model);
                                response.visible = false;
                            }
                            Err(error) => status = format!(" Failed to reload: {error:#} "),
                        }
                        match result {
                            Ok(exit) if exit.success() => {}
                            Ok(exit) => status = format!(" nano exited with {exit} "),
                            Err(error) => status = format!(" Could not open nano: {error:#} "),
                        }
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !search.query.is_empty()
                        && matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Right) =>
                {
                    if key.code == KeyCode::Esc {
                        search = Search::default();
                        status.clear();
                    } else if let Some(line) = search.next(key.code == KeyCode::Right) {
                        scroll = line.min(max_scroll);
                        if let Some(json) = document.json.as_mut() {
                            json.select_row(line);
                        }
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && matches!(key.code, KeyCode::Char('n' | 'N'))
                        && !search.matches.is_empty() =>
                {
                    if let Some(line) = search.next(key.code == KeyCode::Char('n')) {
                        scroll = line.min(max_scroll);
                        if let Some(json) = document.json.as_mut() {
                            json.select_row(line);
                        }
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && document.is_directory
                        && matches!(
                            key.code,
                            KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::PageUp
                                | KeyCode::PageDown
                                | KeyCode::Char('j' | 'k')
                                | KeyCode::Enter
                        ) =>
                {
                    if key.code == KeyCode::Enter {
                        open_target = selected.clone();
                    } else if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        let forward = key.code == KeyCode::PageDown;
                        if let Some(link) =
                            directory_page_selection(&links, selected.as_deref(), forward, height)
                        {
                            selected = Some(link.target.clone());
                            scroll = if forward {
                                scroll.saturating_add(height.max(1))
                            } else {
                                scroll.saturating_sub(height.max(1))
                            };
                            scroll = cursor_scroll(link.line, scroll.min(max_scroll), height)
                                .min(max_scroll);
                        }
                    } else if let Some(link) = directory_move_selection(
                        &links,
                        selected.as_deref(),
                        matches!(key.code, KeyCode::Down | KeyCode::Char('j')),
                    ) {
                        selected = Some(link.target.clone());
                        scroll = cursor_scroll(link.line, scroll, height).min(max_scroll);
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && document.diff.is_some()
                        && matches!(
                            key.code,
                            KeyCode::Enter | KeyCode::Char(' ' | 'c' | 'e' | 'n' | 'p')
                        ) =>
                {
                    let diff = document.diff.as_mut().unwrap();
                    match key.code {
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            if let Some(header) = diff.toggle(scroll, false) {
                                scroll = header;
                                rendered_width = None;
                            }
                        }
                        KeyCode::Char('c' | 'e') => {
                            scroll = diff.fold_all(key.code == KeyCode::Char('c'));
                            rendered_width = None;
                        }
                        KeyCode::Char('n' | 'p') => {
                            scroll = diff.next_file(scroll, key.code == KeyCode::Char('n'))
                        }
                        _ => {}
                    }
                    status.clear();
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && document.json.is_some()
                        && matches!(
                            key.code,
                            KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::Left
                                | KeyCode::Right
                                | KeyCode::PageUp
                                | KeyCode::PageDown
                                | KeyCode::Home
                                | KeyCode::End
                                | KeyCode::Enter
                                | KeyCode::Char(' ')
                                | KeyCode::Char('j')
                                | KeyCode::Char('k')
                                | KeyCode::Char('c')
                                | KeyCode::Char('e')
                                | KeyCode::Char('g')
                                | KeyCode::Char('G')
                                | KeyCode::Char('b')
                        ) =>
                {
                    let json = document.json.as_mut().unwrap();
                    let mut changed = false;
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => json.move_by(-1),
                        KeyCode::Down | KeyCode::Char('j') => json.move_by(1),
                        KeyCode::PageUp | KeyCode::Char('b') => {
                            json.move_by(-(height.max(1) as isize))
                        }
                        KeyCode::PageDown => json.move_by(height.max(1) as isize),
                        KeyCode::Home | KeyCode::Char('g') => json.cursor = 0,
                        KeyCode::End | KeyCode::Char('G') => json.end(),
                        KeyCode::Enter | KeyCode::Char(' ') => changed = json.toggle(json.cursor),
                        KeyCode::Left => {
                            (go_back, changed) = json_left(json, !history.is_empty());
                        }
                        KeyCode::Right => changed = json.expand(),
                        KeyCode::Char('c') => {
                            json.fold_all(true);
                            changed = true;
                        }
                        KeyCode::Char('e') => {
                            json.fold_all(false);
                            changed = true;
                        }
                        _ => {}
                    }
                    scroll = cursor_scroll(json.cursor, scroll, height);
                    if changed {
                        rendered_width = None;
                    }
                    status.clear();
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if history.is_empty() {
                            break;
                        }
                        go_back = true;
                    }
                    KeyCode::Backspace | KeyCode::Left => go_back = true,
                    KeyCode::Tab | KeyCode::BackTab => {
                        let targets = available_links(&document, options);
                        link_picker =
                            Some(LinkPicker::new(targets, &document, selected.as_deref()));
                        dragging_scrollbar = false;
                    }
                    KeyCode::Char('?') => {
                        query.open_prompt(&document.source);
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
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.column >= area.x + 2
                            && mouse.column < area.right().saturating_sub(2)
                            && mouse.row > area.y
                            && mouse.row < area.bottom().saturating_sub(1) =>
                    {
                        let row = scroll + usize::from(mouse.row - area.y - 1);
                        let folded = document
                            .diff
                            .as_mut()
                            .and_then(|diff| diff.toggle(row, true));
                        if folded.is_some() {
                            rendered_width = None;
                        } else {
                            open_target = link_at(&links, area, scroll, mouse.column, mouse.row)
                                .map(|link| link.target.clone());
                        }
                        if open_target.is_none() {
                            if let Some(json) = document.json.as_mut() {
                                let row = scroll + usize::from(mouse.row - area.y - 1);
                                // Clamp to real rows rather than a click below the document.
                                let old = json.cursor;
                                json.cursor = row;
                                if row < text.lines.len() {
                                    if json.toggle(row) {
                                        rendered_width = None;
                                    }
                                } else {
                                    json.cursor = old;
                                }
                            }
                        }
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
            if is_mouse && rendered_width.is_some() {
                if let Some(json) = document.json.as_mut() {
                    if json.cursor < scroll {
                        json.cursor = scroll;
                    }
                    if json.cursor >= scroll + height.max(1) {
                        json.cursor = scroll + height.max(1) - 1;
                    }
                    json.select_row(json.cursor);
                }
            }
            if go_back {
                if let Some((previous, position, selection)) = history.pop() {
                    document = previous;
                    if document.is_directory {
                        document.directory_changes =
                            crate::git_directory::changed_entries(&document.path);
                    }
                    scroll = position;
                    selected = selection;
                    rendered_width = None;
                    pending_anchor = None;
                    status.clear();
                    dragging_scrollbar = false;
                    query = crate::query::Query::new(model);
                    response.visible = false;
                    search = Search::default();
                }
            }
            if let Some(target) = open_target {
                if is_current_document_link(&document, &target) {
                    let url = document.environment.base_url.join(&target)?;
                    pending_anchor = url
                        .fragment()
                        .filter(|fragment| !fragment.is_empty())
                        .map(str::to_owned);
                    if pending_anchor
                        .as_deref()
                        .and_then(crate::navigation::source_line)
                        .is_some()
                    {
                        document.show_source(options);
                    }
                    if pending_anchor.is_none() {
                        scroll = 0;
                    }
                    rendered_width = None;
                    dragging_scrollbar = false;
                    status.clear();
                    continue;
                }
                let result = (|| -> Result<_> {
                    let mut next = if crate::git_links::commit_id(&target).is_some() {
                        document.open_commit(&target)?
                    } else {
                        let path = document.destination(&target)?;
                        crate::navigation::Document::load(path, options)?
                    };
                    let anchor = url::Url::parse(&target)?
                        .fragment()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    if anchor
                        .as_deref()
                        .and_then(crate::navigation::source_line)
                        .is_some()
                    {
                        next.show_source(options);
                    }
                    Ok((next, anchor))
                })();
                match result {
                    Ok((next, anchor)) => {
                        history.push((
                            std::mem::replace(&mut document, next),
                            scroll,
                            selected.take(),
                        ));
                        scroll = 0;
                        pending_anchor = anchor;
                        rendered_width = None;
                        dragging_scrollbar = false;
                        status.clear();
                        query = crate::query::Query::new(model);
                        response.visible = false;
                        search = Search::default();
                    }
                    Err(error) => status = format!(" {error:#} "),
                }
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

fn edit_in_nano(
    terminal: &mut ratatui::DefaultTerminal,
    path: &std::path::Path,
) -> Result<std::process::ExitStatus> {
    execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::try_restore()?;
    let shell = std::env::var_os("SHELL").map(std::path::PathBuf::from);
    let shell_name = shell
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str());
    let result = match (shell.as_deref(), shell_name) {
        (Some(shell), Some("zsh" | "bash")) => std::process::Command::new(shell)
            .arg("-ic")
            .arg("nano -- \"$1\"")
            .arg("mdcat-nano")
            .arg(path)
            .status(),
        (Some(shell), Some("fish")) => std::process::Command::new(shell)
            .arg("-ic")
            .arg("nano -- $argv[1]")
            .arg(path)
            .status(),
        _ => std::process::Command::new("nano")
            .arg("--")
            .arg(path)
            .status(),
    };
    *terminal = ratatui::try_init()?;
    execute!(std::io::stdout(), EnableMouseCapture)?;
    Ok(result?)
}

fn zero_match_notice_style(
    elapsed: std::time::Duration,
    background: (u8, u8, u8),
    no_colour: bool,
) -> Style {
    let fade = elapsed
        .saturating_sub(std::time::Duration::from_millis(500))
        .as_secs_f32()
        .clamp(0.0, 1.0);
    if fade == 0.0 {
        return Style::default();
    }
    if no_colour {
        return if fade >= 0.5 {
            Style::default().add_modifier(ratatui::style::Modifier::DIM)
        } else {
            Style::default()
        };
    }
    let foreground =
        if u16::from(background.0) + u16::from(background.1) + u16::from(background.2) > 384 {
            (40, 40, 40)
        } else {
            (220, 220, 220)
        };
    let blend =
        |fg: u8, bg: u8| (f32::from(fg) + (f32::from(bg) - f32::from(fg)) * fade).round() as u8;
    Style::default().fg(Color::Rgb(
        blend(foreground.0, background.0),
        blend(foreground.1, background.1),
        blend(foreground.2, background.2),
    ))
}

#[derive(Default)]
struct Search {
    open: bool,
    input: String,
    cursor: usize,
    query: String,
    matches: Vec<(usize, usize, usize)>,
    current: Option<usize>,
    original_query: String,
    original_scroll: usize,
    original_current: Option<usize>,
    original_status: String,
    original_json_cursor: Option<usize>,
    pending_until: Option<std::time::Instant>,
}

impl Search {
    fn schedule(&mut self, text: &Text<'_>) -> Option<usize> {
        let length = self.query.chars().count();
        if (1..3).contains(&length) {
            self.matches.clear();
            self.current = None;
            self.pending_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(500));
            None
        } else {
            self.finish_pending(text)
        }
    }

    fn finish_pending(&mut self, text: &Text<'_>) -> Option<usize> {
        self.pending_until = None;
        self.rebuild(text);
        self.first_at_or_after(self.original_scroll)
    }

    fn footer(&self, width: usize) -> (String, usize) {
        let prefix = " Find: ";
        let mut start = 0;
        while start < self.cursor
            && prefix.len() + Line::raw(&self.input[start..self.cursor]).width() >= width
        {
            start += self.input[start..].chars().next().unwrap().len_utf8();
        }
        let cursor = (prefix.len() + Line::raw(&self.input[start..self.cursor]).width())
            .min(width.saturating_sub(1));
        (format!("{prefix}{} ", &self.input[start..]), cursor)
    }

    fn move_cursor(&mut self, forward: bool) {
        if forward {
            if let Some(c) = self.input[self.cursor..].chars().next() {
                self.cursor += c.len_utf8();
            }
        } else if let Some(c) = self.input[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    fn rebuild(&mut self, text: &Text<'_>) {
        self.matches.clear();
        self.current = None;
        if self.query.is_empty() {
            return;
        }
        let Ok(pattern) = regex::RegexBuilder::new(&regex::escape(&self.query))
            .case_insensitive(true)
            .build()
        else {
            return;
        };
        for (line, text) in text.lines.iter().enumerate() {
            let content = text
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            for found in pattern.find_iter(&content) {
                let start = Line::raw(&content[..found.start()]).width();
                let end = start + Line::raw(found.as_str()).width();
                if end > start {
                    self.matches.push((line, start, end));
                }
            }
        }
    }

    fn first_at_or_after(&mut self, line: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let index = self
            .matches
            .iter()
            .position(|entry| entry.0 >= line)
            .unwrap_or(0);
        self.current = Some(index);
        Some(self.matches[index].0)
    }

    fn next(&mut self, forward: bool) -> Option<usize> {
        let count = self.matches.len();
        if count == 0 {
            return None;
        }
        let index = match self.current {
            Some(index) if forward => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
            None if forward => 0,
            None => count - 1,
        };
        self.current = Some(index);
        Some(self.matches[index].0)
    }

    fn highlight(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        scroll: usize,
        no_colour: bool,
    ) {
        let height = usize::from(area.height.saturating_sub(2));
        let width = usize::from(area.width.saturating_sub(4));
        let style = if no_colour {
            Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        };
        for &(line, start, end) in &self.matches {
            if line < scroll || line >= scroll + height || start >= width {
                continue;
            }
            let end = end.min(width);
            frame.buffer_mut().set_style(
                ratatui::layout::Rect::new(
                    area.x + 2 + start as u16,
                    area.y + 1 + (line - scroll) as u16,
                    (end - start) as u16,
                    1,
                ),
                style,
            );
        }
    }
}

fn cursor_scroll(cursor: usize, scroll: usize, height: usize) -> usize {
    if cursor < scroll {
        cursor
    } else if cursor >= scroll + height.max(1) {
        cursor + 1 - height.max(1)
    } else {
        scroll
    }
}

fn directory_move_selection<'a>(
    links: &'a [pulldown_cmark_mdcat::ratatui::LinkRegion],
    selected: Option<&str>,
    forward: bool,
) -> Option<&'a pulldown_cmark_mdcat::ratatui::LinkRegion> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in links {
        if seen.insert(link.target.as_str()) {
            entries.push(link);
        }
    }
    if entries.is_empty() {
        return None;
    }
    let index = selected.and_then(|target| entries.iter().position(|link| link.target == target));
    let index = match (index, forward) {
        (Some(index), true) => (index + 1).min(entries.len() - 1),
        (Some(index), false) => index.saturating_sub(1),
        (None, _) => 0,
    };
    Some(entries[index])
}

fn directory_page_selection<'a>(
    links: &'a [pulldown_cmark_mdcat::ratatui::LinkRegion],
    selected: Option<&str>,
    forward: bool,
    height: usize,
) -> Option<&'a pulldown_cmark_mdcat::ratatui::LinkRegion> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in links {
        if seen.insert(link.target.as_str()) {
            entries.push(link);
        }
    }
    let current = selected
        .and_then(|target| entries.iter().find(|link| link.target == target))
        .copied()
        .or_else(|| entries.first().copied())?;
    let destination = if forward {
        current.line.saturating_add(height.max(1))
    } else {
        current.line.saturating_sub(height.max(1))
    };
    if forward {
        entries
            .iter()
            .find(|link| link.line >= destination)
            .copied()
            .or_else(|| entries.last().copied())
    } else {
        entries
            .iter()
            .rev()
            .find(|link| link.line <= destination)
            .copied()
            .or_else(|| entries.first().copied())
    }
}

fn json_left(json: &mut crate::json_view::JsonView, has_parent: bool) -> (bool, bool) {
    if has_parent && json.cursor == 0 {
        (true, false)
    } else {
        (false, json.collapse())
    }
}

const EMPTY_LINK_NOTICE_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const EMPTY_LINK_NOTICE_FADE: std::time::Duration = std::time::Duration::from_millis(500);

struct LinkPicker {
    targets: Vec<String>,
    rows: Vec<crate::link_tree::PickerRow>,
    file_rows: Vec<usize>,
    state: ListState,
    area: ratatui::layout::Rect,
    opened: std::time::Instant,
    preview_line: Option<usize>,
}

impl LinkPicker {
    fn new(
        targets: Vec<String>,
        document: &crate::navigation::Document,
        selected: Option<&str>,
    ) -> Self {
        let labels: Vec<String> = targets
            .iter()
            .map(|target| {
                document
                    .link_label(target)
                    .expect("picker targets can be opened locally")
            })
            .collect();
        let rows = crate::link_tree::grouped_rows(&labels);
        let file_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.target.map(|_| index))
            .collect::<Vec<_>>();
        let selected_target =
            selected.and_then(|target| targets.iter().position(|item| item == target));
        let index = selected_target
            .and_then(|target| rows.iter().position(|row| row.target == Some(target)))
            .or_else(|| file_rows.first().copied());
        Self {
            targets,
            rows,
            file_rows,
            state: ListState::default().with_selected(index),
            area: Default::default(),
            opened: std::time::Instant::now(),
            preview_line: None,
        }
    }

    fn document_line(
        &mut self,
        links: &[pulldown_cmark_mdcat::ratatui::LinkRegion],
        document: &crate::navigation::Document,
        scroll: usize,
    ) -> Option<usize> {
        // The renderer includes the local hostname; picker URLs may omit it.
        // Keep fragments in the identity so distinct source-line links stay distinct.
        let identity = |target: &str| {
            if crate::git_links::commit_id(target).is_some() {
                return Some((std::path::PathBuf::from(target), None));
            }
            let path = document.destination(target).ok()?;
            let url = document.environment.base_url.join(target).ok()?;
            Some((path, url.fragment().map(str::to_owned)))
        };
        let rendered = links
            .iter()
            .filter_map(|link| identity(&link.target).map(|key| (key, link.line)))
            .collect::<Vec<_>>();
        let occurrences = self
            .targets
            .iter()
            .map(|target| {
                let key = identity(target);
                rendered
                    .iter()
                    .filter_map(|(other, line)| (key.as_ref() == Some(other)).then_some(*line))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let selected_row = self.state.selected()?;
        let target = self.rows.get(selected_row)?.target?;
        let selected = occurrences.get(target)?;
        // Use the closest available neighbor on each side in the displayed list.
        // Repeated destinations prefer the occurrence near those surrounding links,
        // with the last preview (or current viewport) breaking ambiguous ties.
        let neighbors = [
            self.rows[..selected_row].iter().rev().find_map(|row| {
                row.target.and_then(|index| {
                    (!occurrences[index].is_empty()).then_some(&occurrences[index])
                })
            }),
            self.rows[selected_row + 1..].iter().find_map(|row| {
                row.target.and_then(|index| {
                    (!occurrences[index].is_empty()).then_some(&occurrences[index])
                })
            }),
        ];
        let anchor = self.preview_line.unwrap_or(scroll);
        let line = selected.iter().copied().min_by_key(|line| {
            let context = neighbors
                .iter()
                .flatten()
                .map(|positions| {
                    positions
                        .iter()
                        .map(|other| line.abs_diff(*other))
                        .min()
                        .unwrap_or(0)
                })
                .sum::<usize>();
            (context, line.abs_diff(anchor), *line)
        })?;
        self.preview_line = Some(line);
        Some(line)
    }

    fn expired(&self) -> bool {
        self.targets.is_empty()
            && self
                .opened
                .elapsed()
                .saturating_sub(EMPTY_LINK_NOTICE_DELAY)
                >= EMPTY_LINK_NOTICE_FADE
    }

    fn move_by(&mut self, amount: isize) {
        if !self.file_rows.is_empty() {
            let current = self
                .state
                .selected()
                .and_then(|selected| self.file_rows.iter().position(|row| *row == selected))
                .unwrap_or(0);
            let next =
                (current as isize + amount).rem_euclid(self.file_rows.len() as isize) as usize;
            self.state.select(Some(self.file_rows[next]));
        }
    }

    fn select_edge(&mut self, last: bool) {
        self.state.select(
            if last {
                self.file_rows.last()
            } else {
                self.file_rows.first()
            }
            .copied(),
        );
    }

    fn selected_target(&self) -> Option<String> {
        self.state
            .selected()
            .and_then(|index| self.rows.get(index))
            .and_then(|row| row.target)
            .and_then(|index| self.targets.get(index))
            .cloned()
    }

    fn item_at(&self, column: u16, row: u16) -> Option<usize> {
        if column <= self.area.x
            || column >= self.area.right().saturating_sub(2)
            || row <= self.area.y
            || row >= self.area.bottom().saturating_sub(1)
        {
            return None;
        }
        let index = self.state.offset() + usize::from(row - self.area.y - 1);
        self.rows
            .get(index)
            .filter(|row| row.target.is_some())
            .map(|_| index)
    }

    fn draw(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        bounds: ratatui::layout::Rect,
        no_colour: bool,
        background: (u8, u8, u8),
    ) {
        let max_width = (u32::from(bounds.width) * 60 / 100).max(1) as u16;
        let max_height = (u32::from(bounds.height) * 60 / 100).max(1) as u16;
        if self.targets.is_empty() {
            const MESSAGE: &str = "No local file links in this document.";
            self.area = centered(
                bounds,
                (MESSAGE.len() as u16 + 4).min(max_width),
                3.min(max_height),
            );
            frame.render_widget(Clear, self.area);
            let remaining = (1.0
                - self
                    .opened
                    .elapsed()
                    .saturating_sub(EMPTY_LINK_NOTICE_DELAY)
                    .as_secs_f32()
                    / EMPTY_LINK_NOTICE_FADE.as_secs_f32())
            .clamp(0.0, 1.0);
            let blend = |foreground: (u8, u8, u8)| {
                let channel = |fg: u8, bg: u8| {
                    (f32::from(bg) + (f32::from(fg) - f32::from(bg)) * remaining).round() as u8
                };
                Color::Rgb(
                    channel(foreground.0, background.0),
                    channel(foreground.1, background.1),
                    channel(foreground.2, background.2),
                )
            };
            let mut block = popup_block(no_colour).padding(Padding::horizontal(1));
            let mut style = Style::default();
            if no_colour {
                if remaining < 0.5 {
                    style = style.add_modifier(ratatui::style::Modifier::DIM);
                    block = block.border_style(style);
                }
            } else {
                let foreground =
                    if u16::from(background.0) + u16::from(background.1) + u16::from(background.2)
                        > 384
                    {
                        (40, 40, 40)
                    } else {
                        (220, 220, 220)
                    };
                style = style.fg(blend(foreground));
                block = block.border_style(Style::default().fg(blend((180, 167, 235))));
            }
            frame.render_widget(Paragraph::new(MESSAGE).style(style).block(block), self.area);
            return;
        }
        let width = self
            .rows
            .iter()
            .map(|row| Line::raw(&row.label).width())
            .max()
            .unwrap_or(38)
            .saturating_add(5)
            .max(52)
            .min(usize::from(max_width));
        let height = self
            .rows
            .len()
            .max(1)
            .saturating_add(2)
            .min(usize::from(max_height));
        self.area = centered(bounds, width as u16, height as u16);
        frame.render_widget(Clear, self.area);
        let block = popup_block(no_colour)
            .padding(Padding::new(0, 1, 0, 0))
            .title(format!(" Links ({}) ", self.targets.len()));
        let items = self.rows.iter().map(|row| {
            let item = ListItem::new(row.label.as_str());
            if row.target.is_none() {
                item.style(Style::default().add_modifier(ratatui::style::Modifier::BOLD))
            } else {
                item
            }
        });
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("› ")
                .highlight_style(Style::default().add_modifier(ratatui::style::Modifier::REVERSED)),
            self.area,
            &mut self.state,
        );
        let visible = usize::from(self.area.height.saturating_sub(2));
        if self.rows.len() > visible && self.area.width >= 4 && visible > 0 {
            let track = ratatui::layout::Rect::new(
                self.area.right() - 2,
                self.area.y + 1,
                1,
                visible as u16,
            );
            let mut state = ScrollbarState::new(self.rows.len().saturating_sub(visible) + 1)
                .position(self.state.offset())
                .viewport_content_length(visible);
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
    }
}

/// Use filenames until a collision requires paths relative to the shared parent directory.
fn document_titles(paths: &[&std::path::Path]) -> Vec<String> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let peers = paths
                .iter()
                .filter(|other| other.file_name() == path.file_name())
                .collect::<Vec<_>>();
            if peers.len() == 1 {
                return path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned();
            }
            let mut common = path.parent().unwrap_or(path);
            for other in &peers {
                while !other.starts_with(common) {
                    let Some(parent) = common.parent() else {
                        break;
                    };
                    common = parent;
                }
            }
            let relative = path.strip_prefix(common).unwrap_or(path);
            let mut title = format!("./{}", relative.display());
            // Reopening the very same path (for example at another heading) has no unique path.
            if paths.iter().filter(|other| **other == *path).count() > 1 {
                let occurrence = paths[..=index]
                    .iter()
                    .filter(|other| **other == *path)
                    .count();
                title.push_str(&format!(" [{occurrence}]"));
            }
            title
        })
        .collect()
}

fn is_current_document_link(document: &crate::navigation::Document, target: &str) -> bool {
    if target.starts_with('#') {
        return true;
    }
    let Ok(path) = document.destination(target) else {
        return false;
    };
    path == document.path
        || path
            .canonicalize()
            .ok()
            .zip(document.path.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn hover_path(
    document: &crate::navigation::Document,
    target: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    if let Some(id) = crate::git_links::commit_id(target) {
        return Some(format!(" Git commit {id} "));
    }
    let path = document.destination(target).ok()?;
    let mut label = relative_path_label(&path, cwd);
    if let Some(fragment) = document.environment.base_url.join(target).ok()?.fragment() {
        if let Some(line) = crate::navigation::source_line(fragment) {
            label.push_str(&format!(":{line}"));
        } else {
            label.push('#');
            label.push_str(&percent_encoding::percent_decode_str(fragment).decode_utf8_lossy());
        }
    }
    Some(format!(" {label} "))
}

fn relative_path_label(path: &std::path::Path, cwd: &std::path::Path) -> String {
    // Resolve macOS directory aliases (for example /tmp -> /private/tmp)
    // together, so a sibling file does not get an artificial ../../tmp prefix.
    let (path, cwd) = match (path.canonicalize(), cwd.canonicalize()) {
        (Ok(path), Ok(cwd)) => (path, cwd),
        _ => (path.to_owned(), cwd.to_owned()),
    };
    let base = cwd.components().collect::<Vec<_>>();
    let components = path.components().collect::<Vec<_>>();
    let shared = base
        .iter()
        .zip(&components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = std::path::PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &components[shared..] {
        relative.push(component.as_os_str());
    }
    if shared == base.len() {
        format!("./{}", relative.display())
    } else {
        relative.display().to_string()
    }
}

fn directory_title(path: &std::path::Path, cwd: &std::path::Path) -> String {
    let path = relative_path_label(path, cwd);
    if path.ends_with('/') {
        path
    } else {
        format!("{path}/")
    }
}

fn same_local_link(document: &crate::navigation::Document, left: &str, right: &str) -> bool {
    if crate::git_links::commit_id(left).is_some() || crate::git_links::commit_id(right).is_some() {
        return left == right;
    }
    let identity = |target: &str| {
        Some((
            document.destination(target).ok()?,
            document
                .environment
                .base_url
                .join(target)
                .ok()?
                .fragment()
                .map(str::to_owned),
        ))
    };
    identity(left).is_some_and(|left| Some(left) == identity(right))
}

/// Hide heading fragments in path labels, preserving explicit source-line numbers.
fn section_path_label(label: &str, document: &crate::navigation::Document) -> Option<String> {
    let (path, fragment) = label.rsplit_once('#')?;
    if fragment.is_empty() || crate::navigation::source_line(fragment).is_some() {
        return None;
    }
    let destination = document.destination(label).ok()?;
    let extension = destination.extension()?.to_str()?.to_ascii_lowercase();
    ["md", "markdown", "mdown", "mkd", "mkdn"]
        .contains(&extension.as_str())
        .then(|| path.to_owned())
}

fn link_at(
    links: &[pulldown_cmark_mdcat::ratatui::LinkRegion],
    area: ratatui::layout::Rect,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<&pulldown_cmark_mdcat::ratatui::LinkRegion> {
    if column < area.x + 2
        || column >= area.right().saturating_sub(2)
        || row <= area.y
        || row >= area.bottom().saturating_sub(1)
    {
        return None;
    }
    let line = scroll + usize::from(row - area.y - 1);
    let column = usize::from(column - area.x - 2);
    links
        .iter()
        .find(|link| link.line == line && column >= link.start && column < link.end)
}

fn draw_directory_changes(
    frame: &mut ratatui::Frame<'_>,
    document: &crate::navigation::Document,
    links: &[pulldown_cmark_mdcat::ratatui::LinkRegion],
    area: ratatui::layout::Rect,
    scroll: usize,
) {
    let height = usize::from(area.height.saturating_sub(2));
    let width = usize::from(area.width.saturating_sub(4));
    let directory = document
        .path
        .canonicalize()
        .unwrap_or_else(|_| document.path.clone());
    for link in links {
        if link.line < scroll || link.line >= scroll + height || link.start >= width {
            continue;
        }
        let Ok(path) = document.destination(&link.target) else {
            continue;
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        if !document.directory_changes.contains(&directory.join(name)) {
            continue;
        }
        let end = link.end.min(width);
        if end > link.start {
            frame.buffer_mut().set_style(
                ratatui::layout::Rect::new(
                    area.x + 2 + link.start as u16,
                    area.y + 1 + (link.line - scroll) as u16,
                    (end - link.start) as u16,
                    1,
                ),
                Style::default().fg(Color::Yellow),
            );
        }
    }
}

fn draw_link_highlights(
    frame: &mut ratatui::Frame<'_>,
    links: &[pulldown_cmark_mdcat::ratatui::LinkRegion],
    area: ratatui::layout::Rect,
    scroll: usize,
    selected: Option<&str>,
    hovered: Option<&str>,
    commit_colour: Option<Color>,
) {
    use ratatui::style::Modifier;
    let height = usize::from(area.height.saturating_sub(2));
    for link in links {
        let mut modifiers = Modifier::empty();
        if selected == Some(&link.target) {
            modifiers |= Modifier::REVERSED;
        }
        if hovered == Some(&link.target) {
            modifiers |= Modifier::UNDERLINED;
        }
        let color = commit_colour.filter(|_| crate::git_links::commit_id(&link.target).is_some());
        if (!modifiers.is_empty() || color.is_some())
            && link.line >= scroll
            && link.line < scroll + height
        {
            let start = link.start.min(usize::from(area.width.saturating_sub(4)));
            let end = link.end.min(usize::from(area.width.saturating_sub(4)));
            if end > start {
                frame.buffer_mut().set_style(
                    ratatui::layout::Rect::new(
                        area.x + 2 + start as u16,
                        area.y + 1 + (link.line - scroll) as u16,
                        (end - start) as u16,
                        1,
                    ),
                    Style {
                        fg: color,
                        ..Style::default().add_modifier(modifiers)
                    },
                );
            }
        }
    }
}

/// Keep one title row per ancestor, reserving a usable frame for the active document.
/// When the terminal is too short, show only the nearest ancestors.
fn document_stack_layout(
    bounds: ratatui::layout::Rect,
    ancestors: usize,
) -> (usize, ratatui::layout::Rect) {
    let visible = ancestors.min(usize::from(bounds.height.saturating_sub(3)));
    let offset = visible as u16;
    (
        ancestors - visible,
        ratatui::layout::Rect::new(
            bounds.x,
            bounds.y + offset,
            bounds.width,
            bounds.height - offset,
        ),
    )
}

#[cfg(test)]
fn render_document(
    document: &crate::navigation::Document,
    input: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
) -> Result<(
    Text<'static>,
    Vec<pulldown_cmark_mdcat::ratatui::LinkRegion>,
)> {
    render_document_with_table_width(
        document,
        input,
        settings,
        resources,
        options,
        settings.terminal_size.columns,
    )
}

fn render_document_with_table_width(
    document: &crate::navigation::Document,
    input: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
    table_width: u16,
) -> Result<(
    Text<'static>,
    Vec<pulldown_cmark_mdcat::ratatui::LinkRegion>,
)> {
    if document.image.is_some() {
        return Ok((Text::default(), Vec::new()));
    }
    if let Some(diff) = document.diff.as_ref() {
        return Ok(discover_file_links(
            document,
            diff.render(
                usize::from(settings.terminal_size.columns),
                settings.theme.is_dark,
            ),
            Vec::new(),
        ));
    }
    if let Some(json) = document.json.as_ref() {
        return Ok(discover_file_links(
            document,
            json.render(&document.source, settings.theme.is_dark),
            Vec::new(),
        ));
    }
    if let Some(language) = &document.language {
        let events = vec![
            MarkdownEvent::Start(Tag::CodeBlock(CodeBlockKind::Fenced(
                language.clone().into(),
            ))),
            MarkdownEvent::Text(input.into()),
            MarkdownEvent::End(TagEnd::CodeBlock),
        ];
        let (text, links) = pulldown_cmark_mdcat::ratatui::push_linked_text(
            settings,
            &document.environment,
            resources,
            events.into_iter(),
        )?;
        return Ok(discover_file_links(document, text, links));
    }
    let parse_options = markdown_options(options.smart_punctuation);
    let toc = if options.toc {
        mdcat::toc::build_toc_events(
            &document.input,
            parse_options,
            Some(document.file_url()?.as_str()),
        )
    } else {
        Vec::new()
    };
    let mut link_depth = 0usize;
    let mut link_target = None;
    let events = toc
        .into_iter()
        .chain(Parser::new_ext(input, parse_options))
        .flat_map(|event| {
            match &event {
                MarkdownEvent::Start(Tag::Link { dest_url, .. }) => {
                    link_depth += 1;
                    link_target = Some(dest_url.to_string());
                }
                MarkdownEvent::End(TagEnd::Link) => {
                    link_depth = link_depth.saturating_sub(1);
                    link_target = None;
                }
                MarkdownEvent::Text(label) | MarkdownEvent::Code(label) if link_depth > 0 => {
                    if let Some(short) = section_path_label(label, document).filter(|_| {
                        link_target
                            .as_deref()
                            .is_some_and(|target| same_local_link(document, label, target))
                    }) {
                        return vec![match event {
                            MarkdownEvent::Code(_) => MarkdownEvent::Code(short.into()),
                            _ => MarkdownEvent::Text(short.into()),
                        }];
                    }
                }
                MarkdownEvent::Code(reference) if link_depth == 0 => {
                    if let Some(target) = document
                        .code_link(reference)
                        .or_else(|| document.commit_link(reference))
                    {
                        return vec![
                            MarkdownEvent::Start(Tag::Link {
                                link_type: LinkType::Inline,
                                dest_url: target.to_string().into(),
                                title: "".into(),
                                id: "".into(),
                            }),
                            MarkdownEvent::Code(
                                section_path_label(reference, document)
                                    .map(Into::into)
                                    .unwrap_or_else(|| reference.clone()),
                            ),
                            MarkdownEvent::End(TagEnd::Link),
                        ];
                    }
                }
                _ => {}
            }
            vec![event]
        })
        .map(|event| resolve_fragment(event, document));
    let (text, links) = if options.emoji {
        pulldown_cmark_mdcat::ratatui::push_linked_text_with_table_width(
            settings,
            &document.environment,
            resources,
            substitute_emoji(events),
            table_width,
        )?
    } else {
        pulldown_cmark_mdcat::ratatui::push_linked_text_with_table_width(
            settings,
            &document.environment,
            resources,
            events,
            table_width,
        )?
    };
    Ok(discover_file_links(document, text, links))
}

fn discover_file_links(
    document: &crate::navigation::Document,
    text: Text<'static>,
    mut links: Vec<pulldown_cmark_mdcat::ratatui::LinkRegion>,
) -> (
    Text<'static>,
    Vec<pulldown_cmark_mdcat::ratatui::LinkRegion>,
) {
    for (line_number, line) in text.lines.iter().enumerate() {
        let content = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        for (range, mut url) in document
            .file_references(&content)
            .into_iter()
            .chain(document.commits.references(&content))
        {
            // Match the terminal renderer's local file URL form.
            if url.scheme() == "file" {
                url.set_host(Some(&document.environment.hostname))
                    .expect("local hostname is valid");
            }
            let start = Line::raw(&content[..range.start]).width();
            let end = Line::raw(&content[..range.end]).width();
            // Explicit Markdown links retain their destination, even when their label is a path.
            if !links
                .iter()
                .any(|link| link.line == line_number && link.start < end && link.end > start)
            {
                links.push(pulldown_cmark_mdcat::ratatui::LinkRegion {
                    line: line_number,
                    start,
                    end,
                    target: url.to_string(),
                });
            }
        }
    }
    links.sort_by_key(|link| (link.line, link.start));
    (text, links)
}

#[cfg(test)]
fn anchor_line(
    document: &crate::navigation::Document,
    anchor: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
) -> Result<Option<usize>> {
    anchor_line_with_table_width(
        document,
        anchor,
        settings,
        resources,
        options,
        settings.terminal_size.columns,
    )
}

fn anchor_line_with_table_width(
    document: &crate::navigation::Document,
    anchor: &str,
    settings: &Settings<'_>,
    resources: &dyn ResourceUrlHandler,
    options: mdcat::RenderOptions,
    table_width: u16,
) -> Result<Option<usize>> {
    if document.language.is_some() {
        if let Some(line) = crate::navigation::source_line(anchor) {
            // Code blocks preserve one rendered row per source line. Determine the
            // theme's leading border/spacing separately from its source contents.
            let marker = "MDCAT_SOURCE_LINE_POSITION";
            let (probe, _) = render_document_with_table_width(
                document,
                &format!("{marker}\n"),
                settings,
                resources,
                options,
                table_width,
            )?;
            let offset = probe
                .lines
                .iter()
                .position(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                        .contains(marker)
                })
                .unwrap_or(0);
            let line = line.min(document.source.lines().count().max(1));
            return Ok(Some(offset + line - 1));
        }
    }
    let anchor = percent_encoding::percent_decode_str(anchor).decode_utf8_lossy();
    let Some(offset) = mdcat::toc::heading_offset(
        &document.input,
        markdown_options(options.smart_punctuation),
        &anchor,
    ) else {
        return Ok(None);
    };
    let (prefix, _) = render_document_with_table_width(
        document,
        &document.input[..offset],
        settings,
        resources,
        options,
        table_width,
    )?;
    Ok(Some(
        prefix
            .lines
            .iter()
            .rposition(|line| line.width() > 0)
            .map_or(0, |line| line + 2),
    ))
}

fn resolve_fragment<'a>(
    event: pulldown_cmark::Event<'a>,
    document: &crate::navigation::Document,
) -> pulldown_cmark::Event<'a> {
    match event {
        pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if dest_url.starts_with('#')
            || crate::navigation::split_line_reference(&dest_url).is_some() =>
        {
            let target = if let Some(url) = document.line_link(&dest_url) {
                url.to_string()
            } else {
                document
                    .file_url()
                    .and_then(|url| Ok(url.join(&dest_url)?))
                    .map(|url| url.to_string())
                    .unwrap_or_else(|_| dest_url.to_string())
            };
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
                link_type,
                dest_url: target.into(),
                title,
                id,
            })
        }
        other => other,
    }
}

fn available_links(
    document: &crate::navigation::Document,
    options: mdcat::RenderOptions,
) -> Vec<String> {
    let mut links = available_file_links(document, options);
    for (_, url) in document.commits.references(&document.source) {
        let target = url.to_string();
        if !links.contains(&target) {
            links.push(target);
        }
    }
    links
}

fn available_file_links(
    document: &crate::navigation::Document,
    options: mdcat::RenderOptions,
) -> Vec<String> {
    if let Some(json) = &document.json {
        let mut targets = Vec::new();
        for string in json.strings(&document.source) {
            let urls = if let Some(url) = document.code_link(&string) {
                vec![url]
            } else {
                document
                    .file_references(&string)
                    .into_iter()
                    .map(|(_, url)| url)
                    .collect()
            };
            for url in urls {
                let target = url.to_string();
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        return targets;
    }
    if document.language.is_some() {
        let mut targets = Vec::new();
        for (_, url) in document.file_references(&document.input) {
            let target = url.to_string();
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        return targets;
    }
    let parse_options = markdown_options(options.smart_punctuation);
    let file_url = document.file_url().ok();
    let toc = if options.toc {
        mdcat::toc::build_toc_events(
            &document.input,
            parse_options,
            file_url.as_ref().map(url::Url::as_str),
        )
    } else {
        Vec::new()
    };
    let mut targets = Vec::new();
    let mut depth = 0usize;
    for event in toc
        .into_iter()
        .chain(Parser::new_ext(&document.input, parse_options))
        .map(|event| resolve_fragment(event, document))
    {
        let url = match event {
            MarkdownEvent::Start(Tag::Link { dest_url, .. }) => {
                depth += 1;
                url::Url::parse(&dest_url)
                    .or_else(|_| document.environment.base_url.join(&dest_url))
                    .ok()
            }
            MarkdownEvent::End(TagEnd::Link) => {
                depth = depth.saturating_sub(1);
                None
            }
            MarkdownEvent::Code(reference) if depth == 0 => {
                if let Some(url) = document.code_link(&reference) {
                    Some(url)
                } else {
                    for (_, url) in document.file_references(&reference) {
                        let target = url.to_string();
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                    None
                }
            }
            MarkdownEvent::Text(text) if depth == 0 => {
                for (_, url) in document.file_references(&text) {
                    let target = url.to_string();
                    if !targets.contains(&target) {
                        targets.push(target);
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(url) = url {
            let target = url.to_string();
            if document.can_open(&target) && !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    targets
}

#[cfg(test)]
fn link_targets(
    links: &[pulldown_cmark_mdcat::ratatui::LinkRegion],
    document: &crate::navigation::Document,
) -> Vec<String> {
    let mut targets = Vec::new();
    for link in links {
        if document.can_open(&link.target) && !targets.contains(&link.target) {
            targets.push(link.target.clone());
        }
    }
    targets
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
        " Ask about document "
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

fn document_block(
    title: &str,
    current: usize,
    total: usize,
    no_colour: bool,
    active: bool,
) -> Block<'static> {
    let style = if no_colour {
        if active {
            Style::default()
        } else {
            Style::default().add_modifier(ratatui::style::Modifier::DIM)
        }
    } else {
        let brightness = if active { 180 } else { 90 };
        Style::default().fg(Color::Rgb(brightness, brightness, brightness))
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title_style(style)
        .padding(Padding::new(1, 1, 0, 0))
        .title(format!(" {title} "))
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
    fn json_left_closes_nested_document_only_at_first_line() {
        let source = r#"{"child":{"value":1}}"#;
        let mut json = crate::json_view::JsonView::parse(source).unwrap();
        let original = json.render(source, true);
        assert_eq!(json_left(&mut json, true), (true, false));
        assert_eq!(json.render(source, true), original);

        json.cursor = 1;
        assert_eq!(json_left(&mut json, true), (false, true));
        assert_eq!(json.cursor, 1);
        assert_ne!(json.render(source, true), original);

        let mut root = crate::json_view::JsonView::parse(source).unwrap();
        assert_eq!(json_left(&mut root, false), (false, true));
        assert_ne!(root.render(source, true), original);
    }

    #[test]
    fn search_matches_across_styles_and_highlights_unicode_columns() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let text = Text::from(vec![
            Line::from(vec![
                ratatui::text::Span::raw("界 "),
                ratatui::text::Span::styled(
                    "AL",
                    Style::default().add_modifier(ratatui::style::Modifier::BOLD),
                ),
                ratatui::text::Span::raw("pha alpha"),
            ]),
            Line::raw("another ALPHA"),
        ]);
        let mut search = Search {
            query: "alpha".into(),
            ..Default::default()
        };
        search.rebuild(&text);
        assert_eq!(search.matches, vec![(0, 3, 8), (0, 9, 14), (1, 8, 13)]);
        assert_eq!(search.first_at_or_after(1), Some(1));
        assert_eq!(search.next(true), Some(0));
        assert_eq!(search.next(false), Some(1));
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(text.clone()), Rect::new(2, 1, 26, 3));
                search.highlight(frame, Rect::new(0, 0, 30, 5), 0, false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(5, 1)].bg, Color::Yellow);
        assert_eq!(buffer[(11, 1)].bg, Color::Yellow);
        assert_eq!(buffer[(10, 2)].bg, Color::Yellow);
        assert_ne!(buffer[(4, 1)].bg, Color::Yellow);
    }

    #[test]
    fn search_cursor_moves_on_unicode_character_boundaries() {
        let mut search = Search {
            input: "a界b".into(),
            cursor: 5,
            ..Default::default()
        };
        search.move_cursor(false);
        assert_eq!(search.cursor, 4);
        search.move_cursor(false);
        assert_eq!(search.cursor, 1);
        search.move_cursor(true);
        assert_eq!(search.cursor, 4);
    }

    #[test]
    fn zero_match_notice_waits_then_fades_for_one_second() {
        use std::time::Duration;
        let style = |ms| zero_match_notice_style(Duration::from_millis(ms), (20, 20, 20), false);
        assert_eq!(style(0), Style::default());
        assert_eq!(style(500), Style::default());
        assert_eq!(style(1000).fg, Some(Color::Rgb(120, 120, 120)));
        assert_eq!(style(1500).fg, Some(Color::Rgb(20, 20, 20)));
        assert!(
            !zero_match_notice_style(Duration::from_millis(999), (20, 20, 20), true)
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert!(
            zero_match_notice_style(Duration::from_millis(1000), (20, 20, 20), true)
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }

    #[test]
    fn short_searches_are_delayed_but_longer_ones_run_immediately() {
        let text = Text::raw("ab abc 界界界");
        let mut search = Search {
            query: "ab".into(),
            ..Default::default()
        };
        assert_eq!(search.schedule(&text), None);
        assert!(search.pending_until.is_some());
        assert!(search.matches.is_empty());
        assert_eq!(search.finish_pending(&text), Some(0));
        assert_eq!(search.matches.len(), 2);
        search.query = "abc".into();
        assert_eq!(search.schedule(&text), Some(0));
        assert!(search.pending_until.is_none());
        assert_eq!(search.matches.len(), 1);
        search.query = "界界".into();
        assert_eq!(search.schedule(&text), None);
        assert!(search.pending_until.is_some());
        search.query.clear();
        assert_eq!(search.schedule(&text), None);
        assert!(search.pending_until.is_none());
        assert!(search.matches.is_empty());
        assert!(search.footer(20).0.starts_with(" Find: "));
        search.input = "his".into();
        search.cursor = search.input.len();
        let (footer, cursor) = search.footer(20);
        assert_eq!(footer.chars().nth(cursor), Some(' '));
    }

    #[test]
    fn image_document_decodes_and_renders_in_viewer() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sample/rust-logo-128x128.png");
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let image = document.image.expect("image document");
        assert_eq!((image.width(), image.height()), (128, 128));
        let picker = ratatui_image::picker::Picker::halfblocks();
        let mut protocol = picker.new_resize_protocol(image);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 15)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_stateful_widget(
                    ratatui_image::StatefulImage::default(),
                    frame.area(),
                    &mut protocol,
                );
            })
            .unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() != " "));
    }

    #[test]
    fn directory_links_open_as_nested_browsable_documents() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.md");
        let folder = dir.path().join("formal results");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(folder.join("nested")).unwrap();
        std::fs::write(folder.join("a [report].md"), "# Report").unwrap();
        std::fs::write(&report, "[formal results](formal%20results/)").unwrap();
        let parent = crate::navigation::Document::load(report, Default::default()).unwrap();
        let target = available_links(&parent, Default::default()).pop().unwrap();
        let listing = crate::navigation::Document::load(
            parent.destination(&target).unwrap(),
            Default::default(),
        )
        .unwrap();
        let targets = available_links(&listing, Default::default());
        assert_eq!(targets.len(), 2);
        assert_eq!(
            listing.destination(&targets[0]).unwrap(),
            folder.join("nested")
        );
        assert_eq!(
            listing.destination(&targets[1]).unwrap(),
            folder.join("a [report].md")
        );
        let nested = crate::navigation::Document::load(
            listing.destination(&targets[0]).unwrap(),
            Default::default(),
        )
        .unwrap();
        assert!(nested.input.contains("Empty directory"));
        let child = crate::navigation::Document::load(
            listing.destination(&targets[1]).unwrap(),
            Default::default(),
        )
        .unwrap();
        assert!(child.source.contains("# Report"));
    }

    #[test]
    fn directory_selection_skips_duplicate_link_fragments_and_stays_in_bounds() {
        let link = |line, target: &str| pulldown_cmark_mdcat::ratatui::LinkRegion {
            line,
            start: 0,
            end: 4,
            target: target.to_owned(),
        };
        let links = vec![link(0, "a"), link(1, "a"), link(2, "b")];
        assert_eq!(
            directory_move_selection(&links, None, true).unwrap().target,
            "a"
        );
        assert_eq!(
            directory_move_selection(&links, Some("a"), true)
                .unwrap()
                .target,
            "b"
        );
        assert_eq!(
            directory_move_selection(&links, Some("b"), true)
                .unwrap()
                .target,
            "b"
        );
        assert_eq!(
            directory_move_selection(&links, Some("b"), false)
                .unwrap()
                .target,
            "a"
        );
        assert!(directory_move_selection(&[], None, true).is_none());
        let pages = vec![
            link(0, "a"),
            link(1, "a"),
            link(3, "b"),
            link(6, "c"),
            link(9, "d"),
        ];
        assert_eq!(
            directory_page_selection(&pages, Some("a"), true, 5)
                .unwrap()
                .target,
            "c"
        );
        assert_eq!(
            directory_page_selection(&pages, Some("c"), false, 5)
                .unwrap()
                .target,
            "a"
        );
        assert_eq!(
            directory_page_selection(&pages, Some("c"), true, 50)
                .unwrap()
                .target,
            "d"
        );
    }

    #[test]
    fn directory_git_changes_color_only_the_changed_entry() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        std::fs::write(path.join("changed.md"), "changed").unwrap();
        std::fs::write(path.join("clean.md"), "clean").unwrap();
        let mut document =
            crate::navigation::Document::load(path.clone(), Default::default()).unwrap();
        document.directory_changes.insert(path.join("changed.md"));
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let (text, links) = render_document(
            &document,
            &document.input,
            &settings,
            &pulldown_cmark_mdcat::resources::NoopResourceHandler,
            Default::default(),
        )
        .unwrap();
        let changed = links
            .iter()
            .find(|link| link.target.ends_with("changed.md"))
            .unwrap();
        let clean = links
            .iter()
            .find(|link| link.target.ends_with("clean.md"))
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(text.clone()), Rect::new(2, 1, 76, 18));
                draw_directory_changes(frame, &document, &links, Rect::new(0, 0, 80, 20), 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(2 + changed.start as u16, 1 + changed.line as u16)].fg,
            Color::Yellow
        );
        assert_ne!(
            buffer[(2 + clean.start as u16, 1 + clean.line as u16)].fg,
            Color::Yellow
        );
    }

    #[test]
    fn tables_use_window_width_while_text_keeps_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(dir.path().join("target.md"), "target").unwrap();
        std::fs::write(&path, "A paragraph with many short words that should wrap at the configured text width.\n\n| Description | Destination |\n|---|---|\n| A long description that uses extra horizontal space for a table | [target](target.md) |\n\n## Later\n").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: pulldown_cmark_mdcat::TerminalSize {
                columns: 40,
                ..Default::default()
            },
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let (wide, links) = render_document_with_table_width(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
            100,
        )
        .unwrap();
        let (narrow, _) = render_document_with_table_width(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
            40,
        )
        .unwrap();
        assert!(wide.lines.iter().any(|line| line.width() > 40));
        assert!(narrow.lines.iter().all(|line| line.width() <= 40));
        assert!(wide
            .lines
            .iter()
            .filter(|line| line.to_string().contains("paragraph"))
            .all(|line| line.width() <= 40));
        assert!(links
            .iter()
            .any(|link| link.target.ends_with("target.md") && link.start > 40));
        let heading = anchor_line_with_table_width(
            &document,
            "later",
            &settings,
            &resources,
            Default::default(),
            100,
        )
        .unwrap()
        .unwrap();
        assert!(wide.lines[heading].to_string().contains("Later"));
    }

    #[test]
    fn table_click_destinations_match_all_nineteen_picker_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("README.md");
        let mut source =
            "| Dispatch | Baseline | Candidate | Diffs |\n|---|---|---|---|\n".to_owned();
        for dispatch in ["adaptive-qef", "sparse-bricks", "spiral-524288"] {
            source.push_str(&format!("| {dispatch} | [emitted]({dispatch}/baseline/emitted.glsl), [optimized]({dispatch}/baseline/optimized.glsl) | [emitted]({dispatch}/candidate/emitted.glsl), [optimized]({dispatch}/candidate/optimized.glsl) | [emitted]({dispatch}/emitted.diff), [optimized]({dispatch}/optimized.diff) |\n"));
        }
        source.push_str("\n[receipt.json](receipt.json)\n");
        std::fs::write(&path, source).unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: pulldown_cmark_mdcat::TerminalSize {
                columns: 148,
                ..Default::default()
            },
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &pulldown_cmark_mdcat::resources::NoopResourceHandler,
            Default::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 19);
        let clicked = links
            .iter()
            .map(|link| document.destination(&link.target).unwrap())
            .collect::<Vec<_>>();
        let picked = available_links(&document, Default::default())
            .iter()
            .map(|target| document.destination(target).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(clicked, picked);
    }

    #[test]
    fn structured_json_keeps_links_in_folded_sections_and_line_links_use_source() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("example.py");
        std::fs::write(&code, "print(42)\n").unwrap();
        let path = dir.path().join("data.json");
        let source = "{\n  \"nested\": {\n    \"file\": \"example.py\"\n  }\n}\n";
        std::fs::write(&path, source).unwrap();
        let mut document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        document.json.as_mut().unwrap().fold_all(true);
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        // The folded object previews its first string, making this link visible.
        assert_eq!(links.len(), 1);
        assert_eq!(available_links(&document, Default::default()).len(), 1);
        // String extraction also works when the entire JSON document is one token.
        let minified = r#"{"nested":{"file":"example.py"}}"#;
        let view = crate::json_view::JsonView::parse(minified).unwrap();
        assert!(view.strings(minified).contains(&"example.py".to_owned()));
        document.show_source(Default::default());
        assert!(document.json.is_none());
        let row = anchor_line(&document, "L3", &settings, &resources, Default::default())
            .unwrap()
            .unwrap();
        let (text, _) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        let content = text.lines[row]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(content.contains("\"file\": \"example.py\""));
    }

    #[test]
    fn grouped_picker_skips_headers_and_keeps_mouse_and_keyboard_targets_correct() {
        use ratatui::{backend::TestBackend, layout::Rect, style::Modifier, Terminal};
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "report").unwrap();
        let document = crate::navigation::Document::load(report, Default::default()).unwrap();
        let targets = [
            "group/one.glsl:172",
            "other.txt",
            "group/two.glsl",
            "group/three.glsl",
        ]
        .iter()
        .map(|path| {
            document
                .line_link(path)
                .unwrap_or_else(|| document.environment.base_url.join(path).unwrap())
                .to_string()
        })
        .collect::<Vec<_>>();
        let mut picker = LinkPicker::new(targets.clone(), &document, None);
        assert_eq!(picker.rows[0].label, "./group/");
        assert!(picker.rows[0].target.is_none());
        assert_eq!(picker.selected_target(), Some(targets[0].clone()));
        picker.move_by(1);
        assert_eq!(picker.selected_target(), Some(targets[2].clone()));
        picker.move_by(-1);
        assert_eq!(picker.selected_target(), Some(targets[0].clone()));
        picker.select_edge(true);
        assert_eq!(picker.selected_target(), Some(targets[1].clone()));
        picker.select_edge(false);
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal
            .draw(|frame| picker.draw(frame, Rect::new(0, 0, 80, 18), true, (28, 28, 28)))
            .unwrap();
        assert!(picker
            .item_at(picker.area.x + 3, picker.area.y + 1)
            .is_none());
        let row = picker
            .item_at(picker.area.x + 3, picker.area.y + 3)
            .unwrap();
        picker.state.select(Some(row));
        assert_eq!(picker.selected_target(), Some(targets[2].clone()));
        assert!(
            terminal.backend().buffer()[(picker.area.x + 3, picker.area.y + 1)]
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn source_line_links_jump_to_the_exact_rendered_source_row() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "`emitted.glsl:172` [Jump](emitted.glsl:172)\n").unwrap();
        let path = dir.path().join("emitted.glsl");
        let source = (1..=200)
            .map(|line| format!("// SOURCE_{line}\n"))
            .collect::<String>();
        std::fs::write(&path, source).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let document = crate::navigation::Document::load(report, Default::default()).unwrap();
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 2);
        assert!(links
            .iter()
            .all(|link| link.target.ends_with("emitted.glsl#L172")));
        let targets = available_links(&document, Default::default());
        assert_eq!(targets.len(), 1);
        assert!(targets[0].ends_with("emitted.glsl#L172"));
        let code = crate::navigation::Document::load(path, Default::default()).unwrap();
        let (text, _) = render_document(
            &code,
            &code.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        for requested in [1, 172, 200, 999] {
            let row = anchor_line(
                &code,
                &format!("L{requested}"),
                &settings,
                &resources,
                Default::default(),
            )
            .unwrap()
            .unwrap();
            let content = text.lines[row]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(content.trim(), format!("// SOURCE_{}", requested.min(200)));
        }
    }

    #[test]
    fn markdown_line_links_preserve_frontmatter_and_show_literal_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        let source = "---\nkey: value\n---\n# Heading\n";
        std::fs::write(&path, source).unwrap();
        let mut document = crate::navigation::Document::load(path, Default::default()).unwrap();
        document.show_source(Default::default());
        assert_eq!(document.input, source);
        assert_eq!(document.language.as_deref(), Some("md"));
    }

    #[test]
    fn shader_files_are_discovered_and_render_with_syntax_colors() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "`shader.glsl` `shader.metal`").unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let themes = two_face::theme::extra();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: Some(
                themes
                    .get(two_face::theme::EmbeddedThemeName::Dracula)
                    .clone(),
            ),
        };
        for (extension, source) in [
            ("glsl", "#version 450\nvoid main() { vec4 value = vec4(1.0); }\n"),
            ("metal", "#include <metal_stdlib>\nusing namespace metal;\nkernel void compute(device float *out [[buffer(0)]]) { out[0] = 1.0; }\n"),
        ] {
            let path = dir.path().join(format!("shader.{extension}"));
            std::fs::write(&path, source).unwrap();
            let document = crate::navigation::Document::load(path, Default::default()).unwrap();
            assert!(syntax.find_syntax_by_token(document.language.as_deref().unwrap()).is_some(), "{extension}");
            let (text, _) = render_document(&document, &document.input, &settings, &pulldown_cmark_mdcat::resources::NoopResourceHandler, Default::default()).unwrap();
            let colors = text.lines.iter().flat_map(|line| &line.spans).filter_map(|span| span.style.fg).collect::<std::collections::HashSet<_>>();
            assert!(colors.len() > 1, "{extension}");
        }
        let document = crate::navigation::Document::load(report, Default::default()).unwrap();
        assert_eq!(available_links(&document, Default::default()).len(), 2);
    }

    #[test]
    fn command_paths_in_fenced_inline_and_plain_text_are_clickable_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/ada")).unwrap();
        std::fs::write(dir.path().join("scratch/ada/measure.py"), "print(42)").unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "```sh\npython3 -B scratch/ada/measure.py\n```\n\n`python3 -B scratch/ada/measure.py`\n\npython3 -B scratch/ada/measure.py\n").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let (text, links) = render_document(
            &document,
            &document.input,
            &settings,
            &pulldown_cmark_mdcat::resources::NoopResourceHandler,
            Default::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 3);
        for link in &links {
            let line = text.lines[link.line]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(&line[link.start..link.end], "scratch/ada/measure.py");
        }
        let targets = available_links(&document, Default::default());
        assert_eq!(targets.len(), 1);
        assert!(targets[0].ends_with("/scratch/ada/measure.py"));
    }

    #[test]
    fn gruvbox_browse_distinguishes_inline_code_from_linked_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(dir.path().join("file.py"), "print(42)\n").unwrap();
        std::fs::write(&path, "`plain_code` [`linked_code`](file.py) `file.py`\n\n| Code | Link |\n| --- | --- |\n| `table_code` | [`table_link`](file.py) |\n").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: pulldown_cmark_mdcat::Theme::gruvbox_browse(),
            syntax_theme: None,
        };
        let (text, _) = render_document(
            &document,
            &document.input,
            &settings,
            &pulldown_cmark_mdcat::resources::NoopResourceHandler,
            Default::default(),
        )
        .unwrap();
        for (label, expected) in [
            ("plain_code", Color::Rgb(250, 189, 47)),
            ("table_code", Color::Rgb(250, 189, 47)),
            ("linked_code", Color::Rgb(145, 172, 238)),
            ("table_link", Color::Rgb(145, 172, 238)),
            ("file.py", Color::Rgb(145, 172, 238)),
        ] {
            let span = text
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains(label))
                .unwrap();
            assert_eq!(span.style.fg, Some(expected), "{label}");
        }
    }

    #[test]
    fn commit_links_render_in_markdown_and_open_as_nested_highlighted_diffs() {
        let (dir, id) = crate::git_links::tests::repository();
        let path = dir.path().join("report.md");
        std::fs::write(
            &path,
            format!(
                "Commit `{}`\n\nPlain {id}\n\n```text\n{id}\n```\n",
                &id[..7]
            ),
        )
        .unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: pulldown_cmark_mdcat::Theme::gruvbox_browse(),
            syntax_theme: None,
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 3);
        assert!(links
            .iter()
            .all(|link| crate::git_links::commit_id(&link.target) == Some(id.as_str())));
        let targets = available_links(&document, Default::default());
        assert_eq!(targets.len(), 1);
        assert!(document.can_open(&targets[0]));
        assert!(same_local_link(&document, &links[0].target, &targets[0]));
        let mut picker = LinkPicker::new(targets.clone(), &document, None);
        assert!(picker.document_line(&links, &document, 0).is_some());
        let mut commit = document.open_commit(&targets[0]).unwrap();
        commit.diff.as_mut().unwrap().fold_all(false);
        assert_eq!(commit.language.as_deref(), Some("diff"));
        assert_eq!(
            commit.path.file_name().unwrap().to_string_lossy(),
            format!("commit {}", &id[..12])
        );
        assert!(commit
            .title
            .as_deref()
            .unwrap()
            .starts_with(&format!("commit {} by Viewer Test - ", &id[..12])));
        assert!(!commit.title.as_deref().unwrap().contains("@"));
        assert_eq!(commit.commits.root.as_deref(), Some(dir.path()));
        assert!(commit.source.contains("Change example"));
        let (text, _) = render_document(
            &commit,
            &commit.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        assert_eq!(text.lines.first().unwrap().width(), 0);
        assert!(!text.lines.iter().any(|line| {
            let text = line.to_string();
            text.contains(&format!("commit {id}"))
                || text.starts_with("Author:")
                || text.starts_with("Date:")
        }));
        let colors = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(colors.len() > 1);
    }

    #[test]
    fn same_document_links_stay_local_and_hover_paths_are_relative_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "# Review\n").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        for target in ["#review", "report.md#review", "./report.md#review"] {
            assert!(is_current_document_link(&document, target));
        }
        assert!(!is_current_document_link(&document, "other.md#review"));
        assert!(!is_current_document_link(
            &document,
            "https://example.com/report.md#review"
        ));
        assert_eq!(
            hover_path(&document, "report.md#review", dir.path()).unwrap(),
            " ./report.md#review "
        );
        assert_eq!(
            hover_path(&document, "report.md#L12", &dir.path().join("nested")).unwrap(),
            " ../report.md:12 "
        );
    }

    #[test]
    fn section_path_labels_hide_fragments_without_changing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(
            dir.path().join("plan.md"),
            "# Review\n\nTarget paragraph.\n",
        )
        .unwrap();
        std::fs::write(&path, "[plan.md#review](plan.md#review)\n\n`plan.md#review`\n\n[Read the review](plan.md#review)\n").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let (text, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        let content = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!content.contains("#review"));
        assert!(content.contains("plan.md"));
        assert!(content.contains("Read the review"));
        assert_eq!(links.len(), 3);
        assert!(links
            .iter()
            .all(|link| link.target.ends_with("plan.md#review")));
        let targets = available_links(&document, Default::default());
        assert_eq!(targets.len(), 1);
        assert_eq!(document.link_label(&targets[0]).unwrap(), "./plan.md");
        let linked = crate::navigation::Document::load(
            document.destination(&targets[0]).unwrap(),
            Default::default(),
        )
        .unwrap();
        assert!(
            anchor_line(&linked, "review", &settings, &resources, Default::default())
                .unwrap()
                .is_some()
        );
        assert_eq!(section_path_label("plan.md#L12", &document), None);
    }

    #[test]
    fn picker_includes_table_links_and_deduplicates_repeated_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(
            &path,
            "[First](code.cpp)\n\n| File |\n| --- |\n| [Header](code.h) |\n| [Again](code.cpp) |\n",
        )
        .unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let targets = available_links(&document, Default::default());
        assert_eq!(targets.len(), 2);
        assert!(targets[0].ends_with("code.cpp"));
        assert!(targets[1].ends_with("code.h"));
    }

    #[test]
    fn linked_source_files_render_as_highlighted_code_not_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("example.py");
        std::fs::write(&source_path, "# literal comment\nprint(42)\n").unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "`example.py` [JSON](data.json)").unwrap();
        let document = crate::navigation::Document::load(report, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let themes = two_face::theme::extra();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: Some(
                themes
                    .get(two_face::theme::EmbeddedThemeName::Dracula)
                    .clone(),
            ),
        };
        let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        let targets = link_targets(&links, &document);
        assert_eq!(targets.len(), 2);
        assert!(targets[0].ends_with("example.py"));
        assert!(targets[1].ends_with("data.json"));
        let code = crate::navigation::Document::load(source_path, Default::default()).unwrap();
        let (text, links) = render_document(
            &code,
            &code.input,
            &settings,
            &resources,
            Default::default(),
        )
        .unwrap();
        assert!(links.is_empty());
        let content = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(content.contains("# literal comment"));
        assert!(content.contains("print(42)"));
        let colors = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(colors.len() > 1);
    }

    #[test]
    fn link_picker_lists_destinations_and_scrolls_selection_into_view() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "report").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let targets = (0..20)
            .map(|index| {
                document
                    .environment
                    .base_url
                    .join(&format!("child-{index}.md#heading"))
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        let mut picker = LinkPicker::new(targets.clone(), &document, None);
        assert_eq!(picker.rows[0].label, "./child-0.md");
        assert_eq!(picker.selected_target(), Some(targets[0].clone()));
        picker.move_by(-1);
        assert_eq!(picker.selected_target(), Some(targets[19].clone()));
        let mut terminal = Terminal::new(TestBackend::new(70, 12)).unwrap();
        terminal
            .draw(|frame| picker.draw(frame, Rect::new(0, 0, 70, 12), true, (28, 28, 28)))
            .unwrap();
        assert!(picker.area.width <= 42);
        assert!(picker.area.height <= 7);
        assert!(picker.state.offset() > 0);
        assert_eq!(
            terminal.backend().buffer()[(picker.area.right() - 2, picker.area.bottom() - 2)]
                .symbol(),
            "┃"
        );
        let index = picker
            .item_at(picker.area.x + 2, picker.area.y + 1)
            .unwrap();
        assert_eq!(index, picker.state.offset());
        assert!(picker.item_at(picker.area.x, picker.area.y).is_none());
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Links (20)"));
        assert!(!text.contains("Enter open"));
        picker.opened = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(!picker.expired());
        assert!(text.contains("child-19.md"));
        assert!(!text.contains("#heading"));
        picker.move_by(1);
        assert_eq!(picker.selected_target(), Some(targets[0].clone()));
    }

    #[test]
    fn picker_preview_uses_neighboring_links_to_locate_repeated_destinations() {
        use pulldown_cmark_mdcat::ratatui::LinkRegion;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "report").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let targets = ["a.md", "b.md#L12", "c.md", "hidden.md"].map(|name| {
            document
                .environment
                .base_url
                .join(name)
                .unwrap()
                .to_string()
        });
        let links = [(0, 10), (1, 2), (1, 12), (1, 80), (2, 14)].map(|(index, line)| {
            let mut url = url::Url::parse(&targets[index]).unwrap();
            url.set_host(Some(&document.environment.hostname)).unwrap();
            LinkRegion {
                line,
                start: 0,
                end: 4,
                target: url.to_string(),
            }
        });
        let mut picker = LinkPicker::new(targets.to_vec(), &document, None);
        picker.move_by(1);
        // Even when the viewport is nearer the early duplicate, surrounding links
        // identify the occurrence in the main group of references.
        assert_eq!(picker.document_line(&links, &document, 0), Some(12));
        picker.move_by(1);
        assert_eq!(picker.document_line(&links, &document, 0), Some(14));
        picker.move_by(-1);
        assert_eq!(picker.document_line(&links, &document, 0), Some(12));
        picker.select_edge(true);
        // A link hidden inside a folded JSON object has no rendered location.
        assert_eq!(picker.document_line(&links, &document, 0), None);
        picker.select_edge(false);
        assert_eq!(picker.document_line(&links, &document, 0), Some(10));
    }

    #[test]
    fn empty_link_picker_remains_safe_in_small_terminals() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "report").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let mut picker = LinkPicker::new(Vec::new(), &document, None);
        picker.move_by(1);
        picker.move_by(-1);
        assert!(picker.selected_target().is_none());
        for (width, height) in [(70, 12), (8, 3), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    picker.draw(frame, Rect::new(0, 0, width, height), true, (28, 28, 28))
                })
                .unwrap();
            assert!(picker.area.width <= width && picker.area.height <= height);
            assert!(picker.item_at(0, 0).is_none());
        }
    }

    #[test]
    fn empty_link_notice_is_compact_and_fades_before_expiring() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "report").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let mut picker = LinkPicker::new(Vec::new(), &document, None);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let bounds = Rect::new(0, 0, 80, 20);
        terminal
            .draw(|frame| picker.draw(frame, bounds, false, (28, 28, 28)))
            .unwrap();
        assert_eq!(picker.area.width, 41);
        assert_eq!(picker.area.height, 3);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("│ No local file links in this document. │"));
        assert!(!text.contains("Links (0)"));
        assert!(!picker.expired());
        let initial_border = terminal.backend().buffer()[(picker.area.x, picker.area.y)].fg;
        picker.opened = std::time::Instant::now() - std::time::Duration::from_millis(100);
        terminal
            .draw(|frame| picker.draw(frame, bounds, false, (28, 28, 28)))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(picker.area.x, picker.area.y)].fg,
            initial_border
        );
        picker.opened = std::time::Instant::now() - std::time::Duration::from_millis(500);
        terminal
            .draw(|frame| picker.draw(frame, bounds, false, (28, 28, 28)))
            .unwrap();
        let faded_border = terminal.backend().buffer()[(picker.area.x, picker.area.y)].fg;
        assert!(
            matches!((initial_border, faded_border), (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) if r2 < r1 && g2 < g1 && b2 < b1)
        );
        assert!(!picker.expired());
        picker.opened = std::time::Instant::now() - std::time::Duration::from_millis(650);
        assert!(!picker.expired());
        picker.opened = std::time::Instant::now() - std::time::Duration::from_millis(750);
        assert!(picker.expired());
    }

    #[test]
    fn duplicate_titles_use_shared_directory_context_and_shorten_when_closed() {
        use std::path::Path;
        let titles =
            |paths: &[&str]| document_titles(&paths.iter().map(Path::new).collect::<Vec<_>>());
        assert_eq!(
            titles(&[
                "/repo/report.md",
                "/repo/different/report.md",
                "/repo/notes.md"
            ]),
            ["./report.md", "./different/report.md", "notes.md"]
        );
        assert_eq!(
            titles(&[
                "/repo/scratch/ada/run/report.md",
                "/repo/scratch/grace/run/report.md"
            ]),
            ["./ada/run/report.md", "./grace/run/report.md"]
        );
        assert_eq!(titles(&["/repo/report.md"]), ["report.md"]);
        assert_eq!(
            titles(&["/repo/report.md", "/repo/report.md"]),
            ["./report.md [1]", "./report.md [2]"]
        );
    }

    #[test]
    fn directory_caption_uses_path_relative_to_working_directory() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("scratch/edsger/phase-b-to-d-formal-r2");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(
            directory_title(&path, cwd.path()),
            "./scratch/edsger/phase-b-to-d-formal-r2/"
        );
        assert_eq!(directory_title(cwd.path(), cwd.path()), "./");
        assert_eq!(directory_title(cwd.path(), &path), "../../../");
    }

    #[test]
    fn inactive_parent_border_and_title_are_darker_than_the_active_window() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    document_block("./report.md", 1, 1, false, false),
                    Rect::new(0, 0, 60, 12),
                );
                let area = Rect::new(0, 1, 60, 11);
                frame.render_widget(Clear, area);
                frame.render_widget(
                    document_block("./different/report.md", 1, 1, false, true),
                    area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in [0, 3, 50] {
            assert_eq!(buffer[(x, 0)].fg, Color::Rgb(90, 90, 90));
            assert_eq!(buffer[(x, 1)].fg, Color::Rgb(180, 180, 180));
        }
    }

    #[test]
    fn hover_uses_nested_scrolled_coordinates_and_clears_without_losing_selection() {
        use ratatui::{backend::TestBackend, layout::Rect, style::Modifier, Terminal};
        let area = Rect::new(0, 2, 40, 8);
        let links = vec![pulldown_cmark_mdcat::ratatui::LinkRegion {
            line: 8,
            start: 2,
            end: 6,
            target: "file:///tmp/report.md".into(),
        }];
        assert!(link_at(&links, area, 8, 4, 3).is_some());
        for (x, y) in [(4, 2), (4, 9), (0, 3), (8, 3)] {
            assert!(link_at(&links, area, 8, x, y).is_none());
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        for hovered in [Some(links[0].target.as_str()), None] {
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("  link").block(document_block(
                            "report.md",
                            1,
                            1,
                            true,
                            true,
                        )),
                        area,
                    );
                    draw_link_highlights(
                        frame,
                        &links,
                        area,
                        8,
                        Some(&links[0].target),
                        hovered,
                        None,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(4, 3)].modifier.contains(Modifier::UNDERLINED),
                hovered.is_some()
            );
            assert!(buffer[(4, 3)].modifier.contains(Modifier::REVERSED));
            assert!(!buffer[(3, 3)].modifier.contains(Modifier::UNDERLINED));
        }
    }

    #[test]
    fn nested_document_frames_leave_title_rows_and_fit_small_terminals() {
        use ratatui::layout::Rect;
        let bounds = Rect::new(0, 0, 80, 24);
        assert_eq!(document_stack_layout(bounds, 0), (0, bounds));
        assert_eq!(
            document_stack_layout(bounds, 2),
            (0, Rect::new(0, 2, 80, 22))
        );
        assert_eq!(
            document_stack_layout(Rect::new(3, 4, 80, 5), 10),
            (8, Rect::new(3, 6, 80, 3))
        );
        for height in 0..4 {
            let bounds = Rect::new(0, 0, 80, height);
            let (_, area) = document_stack_layout(bounds, 100_000);
            assert!(area.y >= bounds.y && area.bottom() <= bounds.bottom());
            assert!(area.height >= height.min(3));
        }
    }

    #[test]
    fn stacked_document_headers_remain_visible_above_active_content() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let bounds = Rect::new(0, 0, 60, 12);
        terminal
            .draw(|frame| {
                for (index, name) in ["report.md", "linked.md", "one_more_level.md"]
                    .into_iter()
                    .enumerate()
                {
                    let (_, area) = document_stack_layout(bounds, index);
                    frame.render_widget(Clear, area);
                    frame.render_widget(
                        Paragraph::new("ACTIVE_BODY").block(document_block(name, 1, 1, true, true)),
                        area,
                    );
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(0).contains("report.md"));
        assert!(row(1).contains("linked.md"));
        assert!(row(2).contains("one_more_level.md"));
        assert!(row(3).contains("ACTIVE_BODY"));
    }

    #[test]
    fn inline_code_paths_become_selectable_without_overriding_explicit_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/ada")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/grace")).unwrap();
        let path = dir.path().join("scratch/ada/report.md");
        std::fs::write(&path, "Report: `scratch/grace/report.md`.\n\n[Explicit `scratch/grace/report.md`](elsewhere.md)\n\n```\nscratch/grace/report.md\n```\n").unwrap();
        std::fs::write(dir.path().join("scratch/grace/report.md"), "# Grace").unwrap();
        let document = crate::navigation::Document::load(path, Default::default()).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let (_, links) = render_document(
            &document,
            &document.input,
            &settings,
            &pulldown_cmark_mdcat::resources::NoopResourceHandler,
            Default::default(),
        )
        .unwrap();
        let targets = link_targets(&links, &document);
        assert_eq!(targets.len(), 2);
        assert!(targets[0].ends_with("/scratch/grace/report.md"));
        assert!(targets[1].ends_with("/scratch/ada/elsewhere.md"));
        assert_eq!(
            links
                .iter()
                .filter(|link| link.target.ends_with("/scratch/grace/report.md"))
                .count(),
            2
        );
    }

    #[test]
    fn heading_navigation_matches_rendered_rows_with_links_and_toc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.md");
        std::fs::write(&path, "# Start\n\nA [long linked paragraph with many words](other.md).\n\n## Héllo\n\nBody.\n\n## Héllo\n\nLast.\n").unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: pulldown_cmark_mdcat::TerminalSize {
                columns: 25,
                ..Default::default()
            },
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        for toc in [false, true] {
            let options = mdcat::RenderOptions {
                toc,
                ..Default::default()
            };
            let document = crate::navigation::Document::load(path.clone(), options).unwrap();
            let resources = pulldown_cmark_mdcat::resources::NoopResourceHandler;
            let (text, _) =
                render_document(&document, &document.input, &settings, &resources, options)
                    .unwrap();
            for (anchor, occurrence) in [("h%C3%A9llo", 0), ("héllo-1", 1)] {
                let row = anchor_line(&document, anchor, &settings, &resources, options)
                    .unwrap()
                    .unwrap();
                let actual = text
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| {
                        let content = line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>();
                        content.contains("Héllo")
                            && !content.contains("1.")
                            && !content.contains("2.")
                    })
                    .nth(occurrence)
                    .unwrap()
                    .0;
                assert_eq!(
                    row, actual,
                    "anchor={anchor}, toc={toc}, lines={:?}",
                    text.lines
                );
            }
            assert!(
                anchor_line(&document, "missing", &settings, &resources, options)
                    .unwrap()
                    .is_none()
            );
        }
    }

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
