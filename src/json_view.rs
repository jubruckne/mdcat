//! A foldable JSON view that retains source tokens, field order, and duplicate keys.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

#[derive(Debug)]
enum Kind {
    Object(Vec<(Range<usize>, usize)>),
    Array(Vec<usize>),
    String,
    Number,
    Literal,
    Text,
}

struct Node {
    range: Range<usize>,
    parent: Option<usize>,
    kind: Kind,
}

#[derive(Clone)]
struct Row {
    node: usize,
    depth: usize,
    key: Option<Range<usize>>,
    closing: bool,
    comma: bool,
}

pub struct JsonView {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    rows: Vec<Row>,
    collapsed: HashSet<usize>,
    pub cursor: usize,
    initial_layout_pending: bool,
}

impl JsonView {
    pub fn parse(source: &str) -> Option<Self> {
        // Bound the recursive tree builder independently of serde's iterative skip parser.
        let (mut depth, mut string, mut escape) = (0usize, false, false);
        for byte in source.bytes() {
            if string {
                if escape {
                    escape = false;
                } else if byte == b'\\' {
                    escape = true;
                } else if byte == b'"' {
                    string = false;
                }
            } else {
                match byte {
                    b'"' => string = true,
                    b'{' | b'[' => {
                        depth += 1;
                        if depth > 128 {
                            return None;
                        }
                    }
                    b'}' | b']' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        // Validate without building a map, which would discard duplicate fields.
        serde_json::from_str::<serde::de::IgnoredAny>(source).ok()?;
        let mut parser = Parser {
            source: source.as_bytes(),
            cursor: 0,
            nodes: Vec::new(),
        };
        parser.node(None);
        let mut view = Self {
            nodes: parser.nodes,
            roots: vec![0],
            rows: Vec::new(),
            collapsed: HashSet::new(),
            cursor: 0,
            initial_layout_pending: true,
        };
        view.rebuild(0);
        Some(view)
    }

    /// Recognize complete, line-delimited JSON containers in otherwise literal text.
    /// Requiring a container at the start of a line avoids interpreting ordinary
    /// numbers, quoted prose, or bracketed log prefixes as JSON values.
    pub fn parse_embedded(source: &str) -> Option<Self> {
        let mut view = Self {
            nodes: Vec::new(),
            roots: Vec::new(),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            cursor: 0,
            initial_layout_pending: true,
        };
        let mut offset = 0;
        let mut found = false;
        while offset < source.len() {
            let rest = &source[offset..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let line = &rest[..line_end];
            let indent = line.len() - line.trim_start_matches([' ', '\t', '\r']).len();
            let start = offset + indent;
            let fragment = if matches!(source.as_bytes().get(start), Some(b'{' | b'[')) {
                let mut stream = serde_json::Deserializer::from_str(&source[start..])
                    .into_iter::<serde::de::IgnoredAny>();
                if stream.next().is_some_and(|value| value.is_ok()) {
                    let end = start + stream.byte_offset();
                    let tail = &source[end..];
                    let tail_end = tail.find('\n').unwrap_or(tail.len());
                    if tail[..tail_end].trim().is_empty() {
                        Self::parse(&source[start..end]).map(|json| (json, end + tail_end))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((json, end)) = fragment {
                let base = view.nodes.len();
                view.roots.push(base);
                for mut node in json.nodes {
                    node.range = node.range.start + start..node.range.end + start;
                    node.parent = node.parent.map(|id| id + base);
                    match &mut node.kind {
                        Kind::Object(members) => {
                            for (key, id) in members {
                                *key = key.start + start..key.end + start;
                                *id += base;
                            }
                        }
                        Kind::Array(items) => {
                            for id in items {
                                *id += base;
                            }
                        }
                        _ => {}
                    }
                    view.nodes.push(node);
                }
                offset = end;
                found = true;
            } else {
                view.roots.push(view.nodes.len());
                view.nodes.push(Node {
                    range: offset..offset + line_end,
                    parent: None,
                    kind: Kind::Text,
                });
                offset += line_end;
            }
            if source.as_bytes().get(offset) == Some(&b'\n') {
                offset += 1;
            }
        }
        if !found {
            return None;
        }
        view.rebuild(0);
        Some(view)
    }

    fn children(&self, node: usize) -> Vec<(Option<Range<usize>>, usize)> {
        match &self.nodes[node].kind {
            Kind::Object(members) => members
                .iter()
                .map(|(key, node)| (Some(key.clone()), *node))
                .collect(),
            Kind::Array(items) => items.iter().map(|node| (None, *node)).collect(),
            _ => Vec::new(),
        }
    }

    fn count(&self, node: usize) -> Option<usize> {
        match &self.nodes[node].kind {
            Kind::Object(members) => Some(members.len()),
            Kind::Array(items) => Some(items.len()),
            _ => None,
        }
    }

    fn append(&mut self, node: usize, depth: usize, key: Option<Range<usize>>, comma: bool) {
        self.rows.push(Row {
            node,
            depth,
            key,
            closing: false,
            comma,
        });
        if self.count(node).is_some_and(|count| count > 0) && !self.collapsed.contains(&node) {
            let children = self.children(node);
            let count = children.len();
            for (index, (key, child)) in children.into_iter().enumerate() {
                self.append(child, depth + 1, key, index + 1 < count);
            }
            self.rows.push(Row {
                node,
                depth,
                key: None,
                closing: true,
                comma,
            });
        }
    }

    fn rebuild(&mut self, selected: usize) {
        self.rows.clear();
        for root in self.roots.clone() {
            self.append(root, 0, None, false);
        }
        self.cursor = self
            .rows
            .iter()
            .position(|row| row.node == selected && !row.closing)
            .unwrap_or(0);
    }

    pub fn move_by(&mut self, amount: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(amount)
            .min(self.rows.len().saturating_sub(1));
    }

    pub fn select_row(&mut self, row: usize) {
        self.cursor = row.min(self.rows.len().saturating_sub(1));
    }

    pub fn end(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
    }

    pub fn toggle(&mut self, row: usize) -> bool {
        let Some(row) = self.rows.get(row) else {
            return false;
        };
        let node = row.node;
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if self.count(node).is_none_or(|count| count == 0) {
            return false;
        }
        if !self.collapsed.remove(&node) {
            self.collapsed.insert(node);
        }
        self.rebuild(node);
        true
    }

    pub fn collapse(&mut self) -> bool {
        let node = self.rows[self.cursor].node;
        if self.count(node).is_some_and(|count| count > 0) && !self.collapsed.contains(&node) {
            self.collapsed.insert(node);
            self.rebuild(node);
            true
        } else {
            if let Some(parent) = self.nodes[node].parent {
                self.cursor = self
                    .rows
                    .iter()
                    .position(|row| row.node == parent && !row.closing)
                    .unwrap_or(0);
            }
            false
        }
    }

    pub fn expand(&mut self) -> bool {
        let node = self.rows[self.cursor].node;
        if self.collapsed.remove(&node) {
            self.rebuild(node);
            true
        } else {
            if self.count(node).is_some_and(|count| count > 0) {
                self.move_by(1);
            }
            false
        }
    }

    pub fn fold_all(&mut self, collapse: bool) {
        let selected = self.rows[self.cursor].node;
        self.collapsed.clear();
        if collapse {
            // Keep the root open so its fields remain discoverable.
            for node in 0..self.nodes.len() {
                if self.nodes[node].parent.is_some()
                    && self.count(node).is_some_and(|count| count > 0)
                {
                    self.collapsed.insert(node);
                }
            }
        }
        self.rebuild(selected);
    }

    /// Apply the initial fold policy once, after the viewer knows its available height.
    pub fn prepare_for_height(&mut self, height: usize) {
        if !self.initial_layout_pending || height == 0 {
            return;
        }
        self.initial_layout_pending = false;
        if self.rows.len() <= height {
            return;
        }
        for node in 1..self.nodes.len() {
            let mut depth = 0;
            let mut parent = self.nodes[node].parent;
            while let Some(index) = parent {
                depth += 1;
                parent = self.nodes[index].parent;
            }
            if self
                .count(node)
                .is_some_and(|count| count > 0 && (depth >= 2 || (depth == 1 && count > 8)))
            {
                self.collapsed.insert(node);
            }
        }
        self.rebuild(0);
    }

    pub fn focus_column(&self) -> usize {
        self.rows[self.cursor].depth * 2
    }

    pub fn strings(&self, source: &str) -> Vec<String> {
        let mut strings = Vec::new();
        for node in &self.nodes {
            if matches!(node.kind, Kind::Text) {
                strings.push(source[node.range.clone()].to_owned());
            }
            if matches!(node.kind, Kind::String) {
                strings.push(
                    serde_json::from_str(&source[node.range.clone()])
                        .expect("validated JSON string"),
                );
            }
            if let Kind::Object(members) = &node.kind {
                for (key, _) in members {
                    strings.push(
                        serde_json::from_str(&source[key.clone()]).expect("validated JSON key"),
                    );
                }
            }
        }
        strings
    }

    // Choose one varying string field per array, based on its first four items.
    // Using a shared field keeps previews consistent across all rows in the array.
    fn varying_string_fields(&self, source: &str) -> HashMap<usize, String> {
        let mut fields = HashMap::new();
        for (array_id, node) in self.nodes.iter().enumerate() {
            let Kind::Array(items) = &node.kind else {
                continue;
            };
            let sample = &items[..items.len().min(4)];
            if sample.len() < 2 {
                continue;
            }
            let Kind::Object(first) = &self.nodes[sample[0]].kind else {
                continue;
            };
            for (range, value) in first {
                if !matches!(self.nodes[*value].kind, Kind::String) {
                    continue;
                }
                let Ok(name) = serde_json::from_str::<String>(&source[range.clone()]) else {
                    continue;
                };
                let Ok(first_value) =
                    serde_json::from_str::<String>(&source[self.nodes[*value].range.clone()])
                else {
                    continue;
                };
                let differs = sample.iter().skip(1).any(|id| {
                    let Kind::Object(members) = &self.nodes[*id].kind else {
                        return true;
                    };
                    let matching = members.iter().find(|(key, _)| {
                        serde_json::from_str::<String>(&source[key.clone()])
                            .is_ok_and(|key| key == name)
                    });
                    matching.is_none_or(|(_, other)| {
                        !matches!(self.nodes[*other].kind, Kind::String)
                            || serde_json::from_str::<String>(
                                &source[self.nodes[*other].range.clone()],
                            )
                            .map_or(true, |value| value != first_value)
                    })
                });
                if differs {
                    fields.insert(array_id, name);
                    break;
                }
            }
        }
        fields
    }

    pub fn render(&self, source: &str, dark: bool) -> Text<'static> {
        let key = Style::default()
            .fg(if dark {
                Color::Rgb(125, 190, 225)
            } else {
                Color::Rgb(0, 80, 150)
            })
            .add_modifier(Modifier::BOLD);
        let string = Style::default().fg(if dark {
            Color::Rgb(170, 205, 120)
        } else {
            Color::Rgb(45, 110, 35)
        });
        let number = Style::default().fg(if dark {
            Color::Rgb(230, 175, 100)
        } else {
            Color::Rgb(145, 75, 0)
        });
        let literal = Style::default().fg(if dark {
            Color::Rgb(205, 155, 220)
        } else {
            Color::Rgb(125, 45, 150)
        });
        let muted = Style::default().fg(if dark {
            Color::Rgb(145, 145, 145)
        } else {
            Color::Rgb(100, 100, 100)
        });
        let varying_fields = self.varying_string_fields(source);
        Text::from(
            self.rows
                .iter()
                .map(|row| {
                    let node = &self.nodes[row.node];
                    if matches!(node.kind, Kind::Text) {
                        return Line::raw(source[node.range.clone()].to_owned());
                    }
                    let mut spans = vec![Span::raw("  ".repeat(row.depth))];
                    let count = self.count(row.node);
                    if row.closing {
                        spans.push(Span::raw(if matches!(node.kind, Kind::Object(_)) {
                            "}"
                        } else {
                            "]"
                        }));
                    } else {
                        let folded = self.collapsed.contains(&row.node);
                        if let Some(range) = &row.key {
                            spans.push(Span::styled(source[range.clone()].to_owned(), key));
                            spans.push(Span::raw(": "));
                        }
                        if let Some(count) = count {
                            let object = matches!(node.kind, Kind::Object(_));
                            if folded {
                                if let Kind::Object(members) = &node.kind {
                                    let scalar = |id: usize| {
                                        !matches!(
                                            self.nodes[id].kind,
                                            Kind::Object(_) | Kind::Array(_)
                                        )
                                    };
                                    let preferred =
                                        ["name", "key", "id", "index"].iter().find_map(|name| {
                                            members.iter().find(|(range, id)| {
                                                scalar(*id)
                                                    && serde_json::from_str::<String>(
                                                        &source[range.clone()],
                                                    )
                                                    .is_ok_and(|key| key.eq_ignore_ascii_case(name))
                                            })
                                        });
                                    let varying = self.nodes[row.node]
                                        .parent
                                        .and_then(|parent| varying_fields.get(&parent))
                                        .and_then(|name| {
                                            members.iter().find(|(range, id)| {
                                                matches!(self.nodes[*id].kind, Kind::String)
                                                    && serde_json::from_str::<String>(
                                                        &source[range.clone()],
                                                    )
                                                    .is_ok_and(|key| key == *name)
                                            })
                                        });
                                    let preview = preferred.or(varying).or_else(|| {
                                        members.iter().find(|(_, id)| {
                                            matches!(self.nodes[*id].kind, Kind::String)
                                        })
                                    });
                                    if let Some((range, id)) = preview {
                                        let value = &self.nodes[*id];
                                        let value_style = match value.kind {
                                            Kind::String => string,
                                            Kind::Number => number,
                                            _ => literal,
                                        };
                                        spans.push(Span::raw("{ "));
                                        spans.push(Span::styled(
                                            source[range.clone()].to_owned(),
                                            key,
                                        ));
                                        spans.push(Span::raw(": "));
                                        spans.push(Span::styled(
                                            source[value.range.clone()].to_owned(),
                                            value_style,
                                        ));
                                        spans.push(Span::raw(", … }"));
                                    } else {
                                        spans.push(Span::raw("{…}"));
                                    }
                                } else {
                                    spans.push(Span::raw("["));
                                    spans.push(Span::styled(count.to_string(), muted));
                                    spans.push(Span::raw("]"));
                                }
                            } else {
                                spans.push(Span::raw(match (object, count == 0) {
                                    (true, true) => "{}",
                                    (false, true) => "[]",
                                    (true, false) => "{",
                                    (false, false) => "[",
                                }));
                            }
                            if (folded || count == 0) && row.comma {
                                spans.push(Span::raw(","));
                            }
                            if !object && !folded && count >= 10 {
                                spans.push(Span::styled(format!(" ({count})"), muted));
                            }
                        } else {
                            let style = match node.kind {
                                Kind::String => string,
                                Kind::Number => number,
                                _ => literal,
                            };
                            spans.push(Span::styled(source[node.range.clone()].to_owned(), style));
                        }
                    }
                    if row.comma && (row.closing || count.is_none()) {
                        spans.push(Span::raw(","));
                    }
                    Line::from(spans)
                })
                .collect::<Vec<_>>(),
        )
    }
}

struct Parser<'a> {
    source: &'a [u8],
    cursor: usize,
    nodes: Vec<Node>,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .source
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }

    fn string(&mut self) -> Range<usize> {
        let start = self.cursor;
        self.cursor += 1;
        loop {
            match self.source[self.cursor] {
                b'\\' => self.cursor += 2,
                b'"' => {
                    self.cursor += 1;
                    return start..self.cursor;
                }
                _ => self.cursor += 1,
            }
        }
    }

    fn node(&mut self, parent: Option<usize>) -> usize {
        self.whitespace();
        let start = self.cursor;
        let id = self.nodes.len();
        self.nodes.push(Node {
            range: start..start,
            parent,
            kind: Kind::Literal,
        });
        let kind = match self.source[self.cursor] {
            b'{' => {
                self.cursor += 1;
                self.whitespace();
                let mut members = Vec::new();
                while self.source[self.cursor] != b'}' {
                    let key = self.string();
                    self.whitespace();
                    self.cursor += 1; // colon
                    let child = self.node(Some(id));
                    members.push((key, child));
                    self.whitespace();
                    if self.source[self.cursor] != b',' {
                        break;
                    }
                    self.cursor += 1;
                    self.whitespace();
                }
                self.cursor += 1;
                Kind::Object(members)
            }
            b'[' => {
                self.cursor += 1;
                self.whitespace();
                let mut items = Vec::new();
                while self.source[self.cursor] != b']' {
                    items.push(self.node(Some(id)));
                    self.whitespace();
                    if self.source[self.cursor] != b',' {
                        break;
                    }
                    self.cursor += 1;
                }
                self.cursor += 1;
                Kind::Array(items)
            }
            b'"' => {
                self.string();
                Kind::String
            }
            c => {
                while self
                    .source
                    .get(self.cursor)
                    .is_some_and(|c| !c.is_ascii_whitespace() && !b",]}".contains(c))
                {
                    self.cursor += 1;
                }
                if c == b'-' || c.is_ascii_digit() {
                    Kind::Number
                } else {
                    Kind::Literal
                }
            }
        };
        self.nodes[id].range = start..self.cursor;
        self.nodes[id].kind = kind;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(view: &JsonView, source: &str) -> String {
        view.render(source, true)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn embedded_containers_keep_log_context_and_independent_folds() {
        let source = "stdout:\n\n{\"kind\":\"plan\",\"items\":[{\"id\":1},{\"id\":2}]}\nstderr: failed\n  [\n    {\"file\":\"example.py\"}\n  ]\nend\n";
        let mut view = JsonView::parse_embedded(source).unwrap();
        let expanded = content(&view, source);
        assert!(expanded.starts_with("stdout:\n\n{\n"));
        assert!(expanded.contains("\"kind\": \"plan\""));
        assert!(expanded.contains("stderr: failed\n[\n"));
        assert!(expanded.ends_with("\nend"));
        assert!(view.strings(source).contains(&"example.py".to_owned()));
        assert!(view.strings(source).contains(&"stderr: failed".to_owned()));
        view.select_row(0);
        assert!(!view.toggle(0));
        view.fold_all(true);
        let folded = content(&view, source);
        assert!(folded.contains("\"items\": [2]"));
        assert!(folded.contains("stderr: failed\n[\n  { \"file\": \"example.py\", … }\n]\nend"));
        let root = view.roots[2];
        view.select_row(2);
        assert!(view.toggle(2));
        assert!(view.collapsed.contains(&root));
        assert!(content(&view, source).contains("stderr: failed\n["));
        view.fold_all(false);
        assert_eq!(content(&view, source), expanded);
    }

    #[test]
    fn embedded_detection_leaves_prose_invalid_and_partial_json_literal() {
        let source =
            "[mvk-error] failed\n{broken}\n42\n\"quoted\"\nprefix {\"a\":1}\n{\"a\":1} trailing\n";
        assert!(JsonView::parse_embedded(source).is_none());
        let source = format!("{source}{{\"valid\":true}}\n{{\"truncated\": [\n");
        let view = JsonView::parse_embedded(&source).unwrap();
        let rendered = content(&view, &source);
        assert!(rendered.starts_with("[mvk-error] failed\n{broken}\n42\n"));
        assert!(rendered.contains("{\"a\":1} trailing\n{\n  \"valid\": true\n}"));
        assert!(rendered.ends_with("{\"truncated\": ["));
        assert!(
            JsonView::parse_embedded(&format!("{}0{}", "[".repeat(129), "]".repeat(129))).is_none()
        );
    }

    #[test]
    fn preserves_order_duplicate_keys_escapes_and_numeric_spelling() {
        let source = r#"{"z":"same","a":"same","z":1e+02,"list":[true,null,{"ユニ":"a\"b"}]}"#;
        let view = JsonView::parse(source).unwrap();
        assert_eq!(view.count(0), Some(4));
        let rendered = content(&view, source);
        assert!(!rendered.contains("(4)"));
        assert!(
            rendered.find("\"z\": \"same\"").unwrap() < rendered.find("\"a\": \"same\"").unwrap()
        );
        assert_eq!(rendered.matches("\"z\":").count(), 2);
        assert!(rendered.contains("1e+02"));
        assert!(rendered.contains(r#""ユニ": "a\"b""#));
        assert_eq!(view.nodes[0].range, 0..source.len());
    }

    #[test]
    fn keys_and_identical_string_values_have_distinct_colors_on_both_backgrounds() {
        let source = r#"{"same":"same"}"#;
        let view = JsonView::parse(source).unwrap();
        for dark in [true, false] {
            let text = view.render(source, dark);
            let strings = text
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.content == "\"same\"")
                .collect::<Vec<_>>();
            assert_eq!(strings.len(), 2);
            assert_ne!(strings[0].style.fg, strings[1].style.fg);
            assert!(strings[0].style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn toggles_nested_containers_counts_and_bulk_folding_without_losing_data() {
        let source = r#"{"config":{"mode":"x","items":[1,2,3]},"empty":[]}"#;
        let mut view = JsonView::parse(source).unwrap();
        let expanded = content(&view, source);
        let expanded_rows = view.rows.len();
        let config = view
            .rows
            .iter()
            .position(|row| {
                row.key
                    .as_ref()
                    .is_some_and(|range| &source[range.clone()] == "\"config\"")
            })
            .unwrap();
        assert!(view.toggle(config));
        let folded = content(&view, source);
        assert!(folded.contains("\"config\": { \"mode\": \"x\", … },"));
        assert!(!folded.contains("\"items\""));
        assert!(folded.contains("\"empty\": []"));
        assert!(!folded.contains("(0)"));
        assert!(view.expand());
        assert_eq!(content(&view, source), expanded);
        view.fold_all(true);
        assert!(view.rows.len() < expanded_rows);
        view.fold_all(false);
        assert_eq!(content(&view, source), expanded);
        let array = view
            .rows
            .iter()
            .position(|row| {
                row.key
                    .as_ref()
                    .is_some_and(|range| &source[range.clone()] == "\"items\"")
            })
            .unwrap();
        view.select_row(array);
        assert!(view.collapse());
        assert!(content(&view, source).contains("[3]"));
        assert!(!view.collapse()); // Move to the parent container.
        assert_eq!(
            &source[view.rows[view.cursor].key.clone().unwrap()],
            "\"config\""
        );
    }

    #[test]
    fn counts_large_open_containers_and_embeds_folded_counts() {
        let source = r#"{"cases":{"a":0,"b":0,"c":0,"d":0,"e":0,"f":0,"g":0,"h":0,"i":0,"j":0,"k":0},"items":[0,1,2,3,4,5,6,7,8,9],"nine":[0,1,2,3,4,5,6,7,8]}"#;
        let mut view = JsonView::parse(source).unwrap();
        let expanded = content(&view, source);
        assert!(expanded.contains("\"cases\": {\n"));
        assert!(expanded.contains("\"items\": [ (10)"));
        assert!(expanded.contains("\"nine\": [\n"));
        view.fold_all(true);
        let folded = content(&view, source);
        assert!(folded.contains("\"cases\": {…},"));
        assert!(folded.contains("\"items\": [10],"));
        assert!(folded.contains("\"nine\": [9]"));
        assert!(!folded.contains("(11)"));
    }

    #[test]
    fn initial_folding_depends_on_height_depth_and_container_size() {
        let source = r#"{"small":{"nested":{"x":1},"values":[1,2]},"large":[1,2,3,4,5,6,7,8,9],"eight":[1,2,3,4,5,6,7,8]}"#;
        let mut fits = JsonView::parse(source).unwrap();
        let expanded = content(&fits, source);
        fits.prepare_for_height(fits.rows.len());
        assert_eq!(content(&fits, source), expanded);

        let mut overflowing = JsonView::parse(source).unwrap();
        overflowing.prepare_for_height(5);
        let folded = content(&overflowing, source);
        assert!(folded.contains("\"small\": {\n"));
        assert!(folded.contains("\"nested\": {…},"));
        assert!(folded.contains("\"values\": [2]"));
        assert!(folded.contains("\"large\": [9],"));
        assert!(folded.contains("\"eight\": [\n"));
        overflowing.fold_all(false);
        overflowing.prepare_for_height(5);
        assert_eq!(content(&overflowing, source), expanded);
    }

    #[test]
    fn folded_objects_preview_identifier_or_first_string() {
        let source = r#"[{"sequence":10,"values":{"x":1},"scope":"sample","case":"test"},{"other":1,"id":"abc","name":"example"},{"items":[1],"child":{},"enabled":true},{"only":[1]},{"index":15,"name":{}}]"#;
        let mut view = JsonView::parse(source).unwrap();
        view.fold_all(true);
        let folded = content(&view, source);
        assert!(folded.contains("{ \"scope\": \"sample\", … },"));
        assert!(folded.contains("{ \"name\": \"example\", … },"));
        assert!(folded.contains("{ \"index\": 15, … }"));
        assert!(folded.contains("{…}"));
    }

    #[test]
    fn array_object_previews_prefer_a_string_that_varies_in_first_four_items() {
        let source = r#"[{"scope":"same","case":"first","mode":"before"},{"scope":"same","case":"first","mode":"after"},{"scope":"same","case":"second","mode":"before"},{"scope":"same","case":"second","mode":"after"},{"scope":"different","case":"third"}]"#;
        let mut view = JsonView::parse(source).unwrap();
        view.fold_all(true);
        let folded = content(&view, source);
        assert!(folded.contains("{ \"case\": \"first\", … },"));
        assert!(folded.contains("{ \"case\": \"second\", … },"));
        assert!(folded.contains("{ \"case\": \"third\", … }"));
        assert!(!folded.contains("\"scope\""));
    }

    #[test]
    fn validates_input_and_handles_scalar_roots_and_empty_containers() {
        for source in [
            "null",
            "true",
            "false",
            "123.4e-2",
            "\"hello\"",
            "{}",
            "[]",
            " [ 1, 2, { \"x\": [ ] } ] ",
        ] {
            let mut view = JsonView::parse(source).unwrap();
            assert!(!content(&view, source).is_empty());
            view.move_by(999);
            view.move_by(-999);
            assert_eq!(view.cursor, 0);
            view.select_row(usize::MAX);
            assert!(view.cursor < view.rows.len());
            assert!(!view.toggle(usize::MAX));
        }
        for source in ["", "{bad}", "[1,]", "{\"x\":}", "1 2"] {
            assert!(JsonView::parse(source).is_none());
        }
        let too_deep = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        assert!(JsonView::parse(&too_deep).is_none());
    }
}
