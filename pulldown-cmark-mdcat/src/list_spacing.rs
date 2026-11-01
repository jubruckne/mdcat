//! Add breathing room to tight numbered lists using their actual rendered height.
use std::io::{Result, Write};

use pulldown_cmark::{Event, Tag, TagEnd};

#[derive(Default)]
struct List {
    ordered: bool,
    loose: bool,
    items: Vec<(usize, usize)>,
    prefix: String,
}

#[derive(Default)]
pub(crate) struct ListSpacing {
    lists: Vec<List>,
    buffer: Vec<u8>,
    gaps: Vec<(usize, String)>,
    quote_depth: u16,
}

impl ListSpacing {
    pub(crate) fn write_event<W: Write>(
        &mut self,
        writer: &mut W,
        settings: &crate::Settings<'_>,
        event: &Event<'_>,
        output: &[u8],
    ) -> Result<()> {
        match event {
            Event::Start(Tag::BlockQuote(_)) => self.quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                self.quote_depth = self.quote_depth.saturating_sub(1)
            }
            Event::Start(Tag::List(start)) => {
                if let Some(parent) = self.lists.last_mut() {
                    parent.loose = true;
                }
                self.lists.push(List {
                    ordered: start.is_some(),
                    prefix: crate::render::quote_line_prefix(
                        &settings.terminal_capabilities,
                        &settings.theme,
                        self.quote_depth,
                        None,
                    )
                    .0,
                    ..List::default()
                });
            }
            Event::Start(Tag::Item) => {
                if let Some(list) = self.lists.last_mut() {
                    list.items.push((self.buffer.len(), self.buffer.len()));
                }
            }
            Event::Start(
                Tag::Paragraph | Tag::CodeBlock(_) | Tag::Table(_) | Tag::Heading { .. },
            ) => {
                if let Some(list) = self.lists.last_mut() {
                    list.loose = true;
                }
            }
            _ => {}
        }
        if self.lists.is_empty() {
            writer.write_all(output)?;
        } else {
            self.buffer.extend_from_slice(output);
        }
        match event {
            Event::End(TagEnd::Item) => {
                if let Some(item) = self.lists.last_mut().and_then(|list| list.items.last_mut()) {
                    item.1 = self.buffer.len();
                }
            }
            Event::End(TagEnd::List(_)) => {
                if let Some(list) = self.lists.pop() {
                    if list.ordered
                        && !list.loose
                        && list.items.iter().any(|&(start, end)| {
                            self.buffer[start..end]
                                .iter()
                                .filter(|&&byte| byte == b'\n')
                                .count()
                                > 1
                        })
                    {
                        self.gaps.extend(
                            list.items
                                .iter()
                                .skip(1)
                                .map(|&(start, _)| (start, list.prefix.clone())),
                        );
                    }
                }
                if self.lists.is_empty() {
                    self.gaps.sort_by_key(|(offset, _)| *offset);
                    self.gaps.dedup_by_key(|(offset, _)| *offset);
                    let mut previous = 0;
                    for (offset, prefix) in self.gaps.drain(..) {
                        writer.write_all(&self.buffer[previous..offset])?;
                        writeln!(writer, "{prefix}")?;
                        previous = offset;
                    }
                    writer.write_all(&self.buffer[previous..])?;
                    self.buffer.clear();
                }
            }
            _ => {}
        }
        Ok(())
    }
}
