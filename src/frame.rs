//! Document frames for margin output.

use std::io::{self, Write};

use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};
use pulldown_cmark_mdcat::Settings;
use textwrap::core::display_width;

pub(crate) fn take_title<'a>(events: impl Iterator<Item = Event<'a>>) -> (String, Vec<Event<'a>>) {
    let mut events = events.peekable();
    let mut body = Vec::new();
    let mut title = String::new();
    let mut depth = 0usize;
    let mut found = false;
    while let Some(event) = events.next() {
        if !found
            && depth == 0
            && matches!(
                &event,
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H1,
                    ..
                })
            )
        {
            found = true;
            for part in events.by_ref() {
                match part {
                    Event::End(TagEnd::Heading(HeadingLevel::H1)) => break,
                    Event::Text(text) | Event::Code(text) | Event::InlineMath(text) => {
                        title.push_str(&text)
                    }
                    Event::SoftBreak | Event::HardBreak => title.push(' '),
                    _ => {}
                }
            }
        } else {
            match &event {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            body.push(event);
        }
    }
    (title, body)
}

fn write_row(writer: &mut impl Write, line: &str, width: usize, ansi: bool) -> io::Result<()> {
    write!(writer, "│ {line}")?;
    if ansi {
        write!(writer, "\x1b[0m")?;
    }
    writeln!(
        writer,
        "{} │",
        " ".repeat(width.saturating_sub(display_width(line)))
    )
}

pub(crate) fn write_frame(
    writer: &mut impl Write,
    title: &str,
    body: &[u8],
    settings: &Settings,
) -> io::Result<()> {
    let width = usize::from(settings.terminal_size.columns).max(1);
    let inside = width + 2;
    let ansi = settings.terminal_capabilities.style.is_some();
    let title = title.trim();
    let title_lines = textwrap::wrap(title, width);
    if title.is_empty() {
        writeln!(writer, "╭{}╮", "─".repeat(inside))?;
    } else {
        let first = &title_lines[0];
        write!(writer, "╭ ")?;
        if ansi {
            write!(
                writer,
                "{}",
                settings.theme.h1_text_style.bg_color(None).render()
            )?;
        }
        write!(writer, "{first}")?;
        if ansi {
            write!(writer, "\x1b[0m")?;
        }
        writeln!(
            writer,
            "{}╮",
            "─".repeat(inside.saturating_sub(1 + display_width(first)))
        )?;
        for line in title_lines.iter().skip(1) {
            write_row(writer, line, width, ansi)?;
        }
    }
    write_row(writer, "", width, ansi)?;
    let body = String::from_utf8_lossy(body);
    for line in body
        .trim_matches('\n')
        .split('\n')
        .filter(|_| !body.trim_matches('\n').is_empty())
    {
        for wrapped in textwrap::wrap(line, width) {
            write_row(writer, &wrapped, width, ansi)?;
        }
    }
    writeln!(writer, "╰{}╯", "─".repeat(inside))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::Parser;
    use pulldown_cmark_mdcat::{
        terminal::{TerminalProgram, TerminalSize},
        Theme,
    };
    use syntect::parsing::SyntaxSet;

    #[test]
    fn title_is_first_top_level_h1_only() {
        let (title, body) = take_title(Parser::new(
            "> # Quoted\n\n# Shader **Works**\n\nBody\n\n# Later",
        ));
        assert_eq!(title, "Shader Works");
        let text: String = body
            .iter()
            .filter_map(|event| {
                if let Event::Text(text) = event {
                    Some(text.as_ref())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "QuotedBodyLater");
    }

    #[test]
    fn frames_align_with_unicode_colors_and_long_lines() {
        let settings = Settings {
            terminal_capabilities: TerminalProgram::Dumb.capabilities(),
            terminal_size: TerminalSize::default().with_max_columns(20),
            syntax_set: &SyntaxSet::default(),
            theme: Theme::default(),
            syntax_theme: None,
        };
        for title in [
            "Shader Works",
            "日本語",
            "An exceptionally long document title",
            "",
        ] {
            let mut output = Vec::new();
            write_frame(
                &mut output,
                title,
                "Text\n\n日本語\n\x1b[31mred\x1b[0m\nA very long body line that should wrap"
                    .as_bytes(),
                &settings,
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.starts_with('╭'));
            assert!(output.lines().last().unwrap().ends_with('╯'));
            for line in output.lines() {
                assert_eq!(display_width(line), 24, "{line:?}");
            }
        }
    }
}
