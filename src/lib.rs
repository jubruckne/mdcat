// Copyright 2020 Sebastian Wiesner <sebastian@swsnr.de>

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Command line application to render markdown to TTYs.
//!
//! Note that as of version 2.0.0 mdcat itself no longer contains the core rendering functions.
//! Use [`pulldown_cmark_mdcat`] instead.

#![deny(warnings, missing_docs, clippy::all)]
#![forbid(unsafe_code)]

use std::fs::File;
use std::io::stdin;
use std::io::{self, prelude::*, BufWriter};
use std::path::PathBuf;

use anyhow::{Context, Result};
use pulldown_cmark::Parser;
use pulldown_cmark_mdcat::resources::{
    DispatchingResourceHandler, FileResourceHandler, ResourceUrlHandler,
};
use pulldown_cmark_mdcat::{
    expand_tabs, markdown_options, substitute_emoji, Environment, Settings,
};
use resources::CurlResourceHandler;
use tracing::{event, instrument, Level};

use args::ResourceAccess;
use output::Output;

/// Argument parsing for mdcat.
#[allow(missing_docs)]
pub mod args;
/// User configuration file.
pub mod config;
mod frame;
mod history;

/// Output handling for mdcat.
pub mod output;
/// Resource handling for mdca.
pub mod resources;
/// Table of contents generation.
pub mod toc;

/// Default read size limit for resources.
pub static DEFAULT_RESOURCE_READ_LIMIT: u64 = 104_857_600;

/// Whether a path has a supported raster-image filename extension.
pub fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            [
                "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico",
            ]
            .iter()
            .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

struct DirectImageHandler<'a> {
    inner: &'a dyn ResourceUrlHandler,
    max_width: u32,
    max_height: u32,
}

impl ResourceUrlHandler for DirectImageHandler<'_> {
    fn read_resource(
        &self,
        url: &url::Url,
    ) -> io::Result<pulldown_cmark_mdcat::resources::MimeData> {
        let data = self.inner.read_resource(url)?;
        let image = image::load_from_memory(&data.data).map_err(io::Error::other)?;
        let resized = fit_direct_image(image, self.max_width, self.max_height);
        let mut bytes = std::io::Cursor::new(Vec::new());
        resized
            .write_to(&mut bytes, image::ImageFormat::Png)
            .map_err(io::Error::other)?;
        Ok(pulldown_cmark_mdcat::resources::MimeData {
            mime_type: Some(mime::IMAGE_PNG),
            data: bytes.into_inner(),
        })
    }
}

fn fit_direct_image(
    image: image::DynamicImage,
    max_width: u32,
    max_height: u32,
) -> image::DynamicImage {
    if image.width() <= max_width && image.height() <= max_height {
        image
    } else {
        image.resize(max_width, max_height, image::imageops::FilterType::Lanczos3)
    }
}

fn direct_image_size(
    terminal: pulldown_cmark_mdcat::TerminalSize,
) -> pulldown_cmark_mdcat::TerminalSize {
    use pulldown_cmark_mdcat::terminal::PixelSize;
    let cell = terminal
        .cell
        .filter(|cell| cell.x > 0 && cell.y > 0)
        .or_else(|| {
            terminal.pixels.and_then(|pixels| {
                let x = pixels.x / u32::from(terminal.columns.max(1));
                let y = pixels.y / u32::from(terminal.rows.max(1));
                (x > 0 && y > 0).then_some(PixelSize { x, y })
            })
        })
        .unwrap_or(PixelSize { x: 10, y: 20 });
    let columns = (terminal.columns / 2).max(1);
    let rows = (terminal.rows / 2).max(1);
    pulldown_cmark_mdcat::TerminalSize {
        columns,
        rows,
        pixels: Some(PixelSize {
            x: cell.x * u32::from(columns),
            y: cell.y * u32::from(rows),
        }),
        cell: Some(cell),
    }
}

/// Read input for `filename`.
///
/// If `filename` is `-` read from standard input, otherwise try to open and
/// read the given file.
pub fn read_input<T: AsRef<str>>(filename: T) -> Result<(PathBuf, String)> {
    let cd = std::env::current_dir()?;
    let mut buffer = String::new();

    if filename.as_ref() == "-" {
        stdin().read_to_string(&mut buffer)?;
        Ok((cd, buffer))
    } else {
        let mut source = File::open(filename.as_ref())?;
        source.read_to_string(&mut buffer)?;
        let base_dir = cd
            .join(filename.as_ref())
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(cd);
        Ok((base_dir, buffer))
    }
}

/// Render-time options for [`process_file`], bundling the flags that control how the document
/// is transformed before and during rendering.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// Frame the document, placing its first top-level H1 in the top border.
    ///
    /// Callers must shrink `settings.terminal_size` by 4 columns beforehand so wrapping still
    /// respects the requested output width.
    pub margin: bool,
    /// Render typographic punctuation (curly quotes, en/em dashes, ellipsis) instead of the
    /// literal input characters.
    pub smart_punctuation: bool,
    /// Render GitHub-style `:emoji:` shortcodes as Unicode emoji.
    pub emoji: bool,
    /// Render YAML/TOML frontmatter as a table instead of stripping it.
    ///
    /// Has no effect if the frontmatter isn't a flat key-value mapping, or fails to parse; it's
    /// stripped as usual in that case (see
    /// [`pulldown_cmark_mdcat::frontmatter_table_markdown`]).
    pub frontmatter: bool,
    /// Prepend a table of contents generated from the document's headings.
    ///
    /// On standard input (`filename` is `-`) its entries are plain text, since there is no file
    /// to link to; otherwise they link to `filename#slug`.
    pub toc: bool,
    /// If `Some`, expand literal tabs in the input to spaces using that tab stop width before
    /// parsing (see [`pulldown_cmark_mdcat::expand_tabs`]).
    pub tabs: Option<u16>,
}

/// Process a single file.
///
/// Read from `filename` and render the contents to `output` according to `render_options`.
#[instrument(skip(output, settings, resource_handler), level = "debug")]
pub fn process_file(
    filename: &str,
    settings: &Settings,
    resource_handler: &dyn ResourceUrlHandler,
    output: &mut Output,
    render_options: RenderOptions,
) -> Result<()> {
    if filename != "-" && is_image_path(std::path::Path::new(filename)) {
        let path = std::fs::canonicalize(filename)?;
        let environment =
            Environment::for_local_directory(&path.parent().context("Missing image parent")?)?;
        let url =
            url::Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("Invalid image path"))?;
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_owned();
        let events = vec![
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Paragraph),
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image {
                link_type: pulldown_cmark::LinkType::Inline,
                dest_url: url.to_string().into(),
                title: "".into(),
                id: "".into(),
            }),
            pulldown_cmark::Event::Text(label.into()),
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Image),
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Paragraph),
        ];
        let mut sink = BufWriter::new(output.writer());
        let terminal =
            pulldown_cmark_mdcat::TerminalSize::detect().unwrap_or(settings.terminal_size);
        let image_size = direct_image_size(terminal);
        let pixels = image_size
            .pixels
            .expect("direct image bounds have pixel dimensions");
        let image_handler = DirectImageHandler {
            inner: resource_handler,
            max_width: pixels.x,
            max_height: pixels.y,
        };
        let image_settings = Settings {
            terminal_capabilities:
                pulldown_cmark_mdcat::terminal::capabilities::TerminalCapabilities {
                    style: settings.terminal_capabilities.style,
                    image: settings.terminal_capabilities.image,
                    marks: settings.terminal_capabilities.marks,
                },
            terminal_size: image_size,
            syntax_set: settings.syntax_set,
            theme: settings.theme.clone(),
            syntax_theme: settings.syntax_theme.clone(),
        };
        pulldown_cmark_mdcat::push_tty(
            &image_settings,
            &environment,
            &image_handler,
            &mut sink,
            events.into_iter(),
        )?;
        sink.flush()?;
        return Ok(());
    }
    let (base_dir, input) = read_input(filename)?;
    let (body, frontmatter) = pulldown_cmark_mdcat::split_frontmatter(&input);
    let with_frontmatter_table;
    let body: &str = if render_options.frontmatter {
        match frontmatter
            .and_then(|(format, raw)| pulldown_cmark_mdcat::frontmatter_table_markdown(format, raw))
        {
            Some(table) => {
                with_frontmatter_table = format!("{table}\n{body}");
                &with_frontmatter_table
            }
            None => body,
        }
    } else {
        body
    };
    let input = match render_options.tabs {
        Some(tab_width) => expand_tabs(body, tab_width),
        None => std::borrow::Cow::Borrowed(body),
    };
    let input = input.as_ref();
    event!(
        Level::TRACE,
        "Read input, using {} as base directory",
        base_dir.display()
    );
    let options = markdown_options(render_options.smart_punctuation);
    let toc_events = if render_options.toc {
        let file_ref = (filename != "-")
            .then(|| {
                std::path::Path::new(filename)
                    .file_name()
                    .and_then(|s| s.to_str())
            })
            .flatten();
        toc::build_toc_events(input, options, file_ref)
    } else {
        Vec::new()
    };
    let parser = toc_events
        .into_iter()
        .chain(Parser::new_ext(input, options));
    let parser: Box<dyn Iterator<Item = pulldown_cmark::Event>> = if render_options.emoji {
        Box::new(substitute_emoji(parser))
    } else {
        Box::new(parser)
    };
    let env = Environment::for_local_directory(&base_dir)?;

    let ignore_broken_pipe = |error: io::Error| {
        if error.kind() == io::ErrorKind::BrokenPipe {
            event!(Level::TRACE, "Ignoring broken pipe");
            Ok(())
        } else {
            event!(Level::ERROR, ?error, "Failed to process file: {:#}", error);
            Err(error)
        }
    };

    let mut sink = BufWriter::new(output.writer());
    writeln!(sink).or_else(ignore_broken_pipe)?;
    if render_options.margin {
        let (title, body) = frame::take_title(parser);
        // Cursor-moving image/mark protocols cannot be padded into a text frame.
        let framed_settings = Settings {
            terminal_capabilities:
                pulldown_cmark_mdcat::terminal::capabilities::TerminalCapabilities {
                    style: settings.terminal_capabilities.style,
                    image: None,
                    marks: None,
                },
            terminal_size: settings.terminal_size,
            syntax_set: settings.syntax_set,
            theme: settings.theme.clone(),
            syntax_theme: settings.syntax_theme.clone(),
        };
        let mut rendered = Vec::new();
        pulldown_cmark_mdcat::push_tty(
            &framed_settings,
            &env,
            resource_handler,
            &mut rendered,
            body.into_iter(),
        )
        .and_then(|_| frame::write_frame(&mut sink, &title, &rendered, &framed_settings))
        .or_else(ignore_broken_pipe)?;
    } else {
        pulldown_cmark_mdcat::push_tty(settings, &env, resource_handler, &mut sink, parser)
            .or_else(ignore_broken_pipe)?;
    }
    if !render_options.margin {
        writeln!(sink).or_else(ignore_broken_pipe)?;
    }
    sink.flush().or_else(ignore_broken_pipe)?;
    Ok(())
}

/// Create the resource handler for mdcat.
pub fn create_resource_handler(access: ResourceAccess) -> Result<DispatchingResourceHandler> {
    let mut resource_handlers: Vec<Box<dyn ResourceUrlHandler>> = vec![Box::new(
        FileResourceHandler::new(DEFAULT_RESOURCE_READ_LIMIT),
    )];
    if let ResourceAccess::Remote = access {
        let user_agent = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
        event!(
            target: "mdcat::main",
            Level::DEBUG,
            "Remote resource access permitted, creating HTTP client with user agent {}",
            user_agent
        );
        let client = CurlResourceHandler::create(DEFAULT_RESOURCE_READ_LIMIT, user_agent)
            .with_context(|| "Failed to build HTTP client".to_string())?;
        resource_handlers.push(Box::new(client));
    }
    Ok(DispatchingResourceHandler::new(resource_handlers))
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use pulldown_cmark_mdcat::terminal::PixelSize;

    #[test]
    fn direct_images_fit_half_terminal_width_and_height() {
        let bounds = direct_image_size(pulldown_cmark_mdcat::TerminalSize {
            columns: 100,
            rows: 40,
            pixels: Some(PixelSize { x: 1000, y: 800 }),
            cell: Some(PixelSize { x: 10, y: 20 }),
        });
        let pixels = bounds.pixels.unwrap();
        assert_eq!(
            (bounds.columns, bounds.rows, pixels.x, pixels.y),
            (50, 20, 500, 400)
        );
        for (width, height, expected) in [
            (1200, 300, (500, 125)),
            (300, 1200, (100, 400)),
            (50, 30, (50, 30)),
        ] {
            let resized = fit_direct_image(
                image::DynamicImage::new_rgba8(width, height),
                pixels.x,
                pixels.y,
            );
            assert_eq!((resized.width(), resized.height()), expected);
        }
    }
}
