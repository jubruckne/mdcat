//! Add viewer-only comparisons to Before/After tables.
use pulldown_cmark::{Alignment, Event, LinkType, Tag, TagEnd};
use url::Url;

use crate::navigation::Document;

pub fn paths(target: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let url = Url::parse(target).ok()?;
    if url.scheme() != "mdcat-diff" {
        return None;
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    if pairs.len() != 2 || pairs[0].0 != "before" || pairs[1].0 != "after" {
        return None;
    }
    let before = Url::parse(&pairs[0].1).ok()?.to_file_path().ok()?;
    let after = Url::parse(&pairs[1].1).ok()?.to_file_path().ok()?;
    Some((before, after))
}

fn target(document: &Document, before: &str, after: &str) -> Option<String> {
    let before = document.destination(before).ok()?.canonicalize().ok()?;
    let after = document.destination(after).ok()?.canonicalize().ok()?;
    if !before.is_file() || !after.is_file() || before == after || before.parent() != after.parent()
    {
        return None;
    }
    let mut url = Url::parse("mdcat-diff:?").ok()?;
    url.query_pairs_mut()
        .append_pair("before", Url::from_file_path(before).ok()?.as_str())
        .append_pair("after", Url::from_file_path(after).ok()?.as_str());
    Some(url.into())
}

fn cells<'a, 'b>(events: &'b [Event<'a>]) -> Vec<&'b [Event<'a>]> {
    let mut result = Vec::new();
    let mut start = None;
    for (i, event) in events.iter().enumerate() {
        match event {
            Event::Start(Tag::TableCell) => start = Some(i + 1),
            Event::End(TagEnd::TableCell) => {
                if let Some(start) = start.take() {
                    result.push(&events[start..i]);
                }
            }
            _ => {}
        }
    }
    result
}

fn header(cell: &[Event<'_>], expected: &str) -> bool {
    cell.iter()
        .filter_map(|e| match e {
            Event::Text(s) | Event::Code(s) => Some(s.as_ref()),
            _ => None,
        })
        .collect::<String>()
        .trim()
        .eq_ignore_ascii_case(expected)
}

fn link<'a>(cell: &'a [Event<'_>]) -> Option<&'a str> {
    let mut links = cell.iter().filter_map(|e| match e {
        Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
        _ => None,
    });
    let first = links.next()?;
    links.next().is_none().then_some(first)
}

pub fn augment<'a>(events: Vec<Event<'a>>, document: &Document) -> Vec<Event<'a>> {
    let mut output = Vec::new();
    let mut iter = events.into_iter();
    while let Some(event) = iter.next() {
        let Event::Start(Tag::Table(mut alignments)) = event else {
            output.push(event);
            continue;
        };
        let mut table = Vec::new();
        for event in iter.by_ref() {
            let end = matches!(event, Event::End(TagEnd::Table));
            table.push(event);
            if end {
                break;
            }
        }
        let head_end = table
            .iter()
            .position(|e| matches!(e, Event::End(TagEnd::TableHead)));
        let eligible = head_end
            .map(|end| cells(&table[..end]))
            .is_some_and(|head| {
                head.len() >= 2
                    && header(head[head.len() - 2], "Before")
                    && header(head[head.len() - 1], "After")
            });
        let mut row_targets = Vec::new();
        let mut start = None;
        if eligible {
            for (i, event) in table.iter().enumerate() {
                if matches!(event, Event::Start(Tag::TableRow)) {
                    start = Some(i);
                }
                if matches!(event, Event::End(TagEnd::TableRow)) {
                    if let Some(start) = start.take() {
                        let row = cells(&table[start..i]);
                        let value = if row.len() == alignments.len() {
                            link(row[row.len() - 2])
                                .zip(link(row[row.len() - 1]))
                                .and_then(|(a, b)| target(document, a, b))
                        } else {
                            None
                        };
                        row_targets.push((i, value));
                    }
                }
            }
        }
        if row_targets.iter().any(|(_, target)| target.is_some()) {
            alignments.push(Alignment::None);
            output.push(Event::Start(Tag::Table(alignments)));
            let mut rows = row_targets.into_iter().peekable();
            for (i, event) in table.into_iter().enumerate() {
                if Some(i) == head_end {
                    output.extend([
                        Event::Start(Tag::TableCell),
                        Event::Text("(diff)".into()),
                        Event::End(TagEnd::TableCell),
                    ]);
                }
                if rows.peek().is_some_and(|(index, _)| *index == i) {
                    let (_, target) = rows.next().unwrap();
                    output.push(Event::Start(Tag::TableCell));
                    if let Some(target) = target {
                        output.extend([
                            Event::Start(Tag::Link {
                                link_type: LinkType::Inline,
                                dest_url: target.into(),
                                title: "".into(),
                                id: "".into(),
                            }),
                            Event::Text("(diff)".into()),
                            Event::End(TagEnd::Link),
                        ]);
                    }
                    output.push(Event::End(TagEnd::TableCell));
                }
                output.push(event);
            }
        } else {
            output.push(Event::Start(Tag::Table(alignments)));
            output.extend(table);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    #[test]
    fn adds_diff_only_for_valid_rows_and_opens_comparison() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("before a.txt"), "old\n").unwrap();
        std::fs::write(dir.path().join("after a.txt"), "new\n").unwrap();
        let path = dir.path().join("report.md");
        let input = "| Stage | Before | After |\n|---|---|---|\n| one | [old](before%20a.txt) | [new](after%20a.txt) |\n| missing | [old](before%20a.txt) | [new](missing.txt) |\n";
        std::fs::write(&path, input).unwrap();
        let document = Document::load(path, Default::default()).unwrap();
        let events = augment(
            Parser::new_ext(input, Options::ENABLE_TABLES).collect(),
            &document,
        );
        let targets: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) if paths(dest_url).is_some() => {
                    Some(dest_url.to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(targets.len(), 1);
        assert!(document.can_open(&targets[0]));
        let diff = document.open_file_diff(&targets[0]).unwrap();
        assert!(diff.source.contains("-old"));
        assert!(diff.source.contains("+new"));
        assert!(diff.diff.is_some());
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Text(text) if text.as_ref() == "(diff)"))
                .count(),
            2
        );
    }

    #[test]
    fn leaves_unrelated_or_invalid_tables_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "").unwrap();
        let document = Document::load(path, Default::default()).unwrap();
        for input in [
            "| Before | After |\n|---|---|\n| [x](missing) | [y](also-missing) |\n",
            "| Before | Result |\n|---|---|\n| a | b |\n",
        ] {
            let original: Vec<_> = Parser::new_ext(input, Options::ENABLE_TABLES).collect();
            assert_eq!(augment(original.clone(), &document), original);
        }
    }
}
