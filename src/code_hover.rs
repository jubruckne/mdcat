//! Asynchronous clangd hover support for displayed C/C++ source.
use anyhow::{Context, Result};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Target {
    pub path: PathBuf,
    pub text: Arc<str>,
    pub revision: u64,
    pub line: usize,
    pub character: usize,
    pub rect: Rect,
}
impl Target {
    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.revision == other.revision
            && self.line == other.line
            && self.character == other.character
            && self.rect == other.rect
    }
}
struct Request {
    id: u64,
    target: Target,
}
struct Client {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<(u64, String)>,
    child: Arc<Mutex<Option<Child>>>,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn send(writer: &mut impl Write, value: Value) -> Result<()> {
    let data = serde_json::to_vec(&value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", data.len())?;
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}
fn read_message(reader: &mut impl BufRead) -> Result<Value> {
    let mut size = None;
    loop {
        let mut line = String::new();
        anyhow::ensure!(reader.read_line(&mut line)? > 0, "clangd closed");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("Content-Length") {
                size = Some(value.trim().parse::<usize>()?);
            }
        }
    }
    let size = size.context("Missing LSP length")?;
    anyhow::ensure!(size <= 16 * 1024 * 1024, "LSP message too large");
    let mut data = vec![0; size];
    reader.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}
fn response(rx: &mpsc::Receiver<Value>, writer: &mut impl Write, id: u64) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if value.get("method").is_some() {
            if let Some(id) = value.get("id") {
                send(writer, json!({"jsonrpc":"2.0", "id":id, "result":null}))?;
            }
            continue;
        }
        if value["id"].as_u64() == Some(id) {
            anyhow::ensure!(value.get("error").is_none(), "clangd request failed");
            return Ok(value["result"].clone());
        }
    }
}
fn hover_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(hover_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(o) => o.get("value").map(hover_text).unwrap_or_default(),
        _ => String::new(),
    }
}
fn display_hover(value: &Value) -> (String, bool) {
    let raw = hover_text(value);
    let mut inside = false;
    let mut blocks = Vec::new();
    let mut code = Vec::new();
    for line in raw.lines() {
        if line.trim_start().starts_with("```") {
            if inside && !code.is_empty() {
                blocks.push(code.join("\n"));
                code.clear();
            }
            inside = !inside;
        } else if inside {
            code.push(line);
        }
    }
    let heading = raw.lines().next().unwrap_or("").to_ascii_lowercase();
    let expand = heading.contains("class")
        || heading.contains("struct")
        || heading.contains("union")
        || heading.contains("enum")
        || heading.contains("type-alias");
    let Some(code) = blocks.last() else {
        return (raw, expand);
    };
    let function = heading.contains("function")
        || heading.contains("method")
        || heading.contains("constructor")
        || heading.contains("destructor");
    let code = code
        .lines()
        .filter(|line| !line.trim_start().starts_with("// In "))
        .collect::<Vec<_>>()
        .join("\n");
    let display = if code.trim_start().starts_with('#') {
        code
    } else {
        pretty_declaration(&code, function)
    };
    (display, expand)
}
fn declaration_suffix(suffix: &str) -> &str {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in suffix.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        match c {
            '(' | '[' | '<' => depth += 1,
            ')' | ']' | '>' => depth = depth.saturating_sub(1),
            '=' if depth == 0
                && suffix[i + 1..].trim().trim_end_matches(';').trim() == "default" =>
            {
                return suffix[..i].trim_end()
            }
            ':' if depth == 0
                && !suffix[..i].ends_with(':')
                && !suffix[i + 1..].starts_with(':') =>
            {
                return suffix[..i].trim_end()
            }
            _ => (),
        }
    }
    suffix.trim_end_matches(';').trim_end()
}
fn pretty_declaration(code: &str, function: bool) -> String {
    let code = code.trim();
    let code = ["public", "protected", "private"]
        .into_iter()
        .find_map(|access| {
            code.strip_prefix(access)?
                .trim_start()
                .strip_prefix(':')
                .map(str::trim_start)
        })
        .unwrap_or(code);
    if function {
        let mut angle = 0usize;
        let open = code.char_indices().find_map(|(i, c)| {
            match c {
                '<' => angle += 1,
                '>' => angle = angle.saturating_sub(1),
                '(' if angle == 0 => return Some(i),
                _ => (),
            };
            None
        });
        if let Some(open) = open {
            let mut depth = 0;
            let mut close = None;
            for (i, c) in code[open..].char_indices() {
                if c == '(' {
                    depth += 1;
                }
                if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
            }
            if let Some(close) = close {
                let mut params = Vec::new();
                let mut start = open + 1;
                let mut depth = 0usize;
                let mut quote = None;
                let mut escaped = false;
                for (i, c) in code[open + 1..close].char_indices() {
                    if let Some(q) = quote {
                        if escaped {
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == q {
                            quote = None;
                        }
                        continue;
                    }
                    if c == '\'' || c == '"' {
                        quote = Some(c);
                        continue;
                    }
                    match c {
                        '(' | '<' | '[' | '{' => depth += 1,
                        ')' | '>' | ']' | '}' => depth = depth.saturating_sub(1),
                        ',' if depth == 0 => {
                            params.push(code[start..open + 1 + i].trim());
                            start = open + 2 + i;
                        }
                        _ => (),
                    }
                }
                let last = code[start..close].trim();
                if !last.is_empty() {
                    params.push(last);
                }
                if !params.is_empty() {
                    if params.len() == 1 {
                        let compact = format!(
                            "{}({}){}",
                            code[..open].trim(),
                            params[0],
                            declaration_suffix(&code[close + 1..])
                        );
                        if !compact.contains('\n')
                            && unicode_width::UnicodeWidthStr::width(compact.as_str()) <= 72
                        {
                            return compact;
                        }
                    }
                    return format!(
                        "{}(\n    {}\n){}",
                        code[..open].trim(),
                        params.join(",\n    "),
                        declaration_suffix(&code[close + 1..])
                    );
                }
                return format!(
                    "{}(){}",
                    code[..open].trim(),
                    declaration_suffix(&code[close + 1..])
                );
            }
        }
        return code.trim_end_matches(';').to_owned();
    }
    if code.ends_with(';') {
        code.to_owned()
    } else {
        format!("{code};")
    }
}
fn source_offset(source: &str, position: &Value) -> Option<usize> {
    let line = position["line"].as_u64()? as usize;
    let character = position["character"].as_u64()? as usize;
    let start = source
        .split_inclusive('\n')
        .take(line)
        .map(str::len)
        .sum::<usize>();
    let mut utf16 = 0;
    for (i, c) in source.get(start..)?.char_indices() {
        if utf16 >= character {
            return Some(start + i);
        }
        utf16 += c.len_utf16();
    }
    Some(source.len())
}
fn public_member(record: &Value, member: &Value, source: &str, public_default: bool) -> bool {
    let Some(start) = source_offset(source, &record["range"]["start"]) else {
        return false;
    };
    let Some(end) = source_offset(source, &member["range"]["start"]) else {
        return false;
    };
    let Some(text) = source.get(start..end) else {
        return false;
    };
    let tokens = regex::Regex::new(r#"(?s)//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[A-Za-z_][A-Za-z_0-9]*|[{}:]"#).expect("fixed C++ token regex");
    let mut depth = 0usize;
    let mut access = public_default;
    let mut previous = "";
    for matched in tokens.find_iter(text) {
        let token = matched.as_str();
        if token.starts_with("//")
            || token.starts_with("/*")
            || token.starts_with('"')
            || token.starts_with('\'')
        {
            continue;
        }
        match token {
            "{" => depth += 1,
            "}" => depth = depth.saturating_sub(1),
            ":" if depth == 1 => match previous {
                "public" => access = true,
                "private" | "protected" => access = false,
                _ => (),
            },
            _ => (),
        }
        previous = token;
    }
    access
}
fn rpc(
    writer: &mut impl Write,
    incoming: &mpsc::Receiver<Value>,
    next: &mut u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    *next += 1;
    send(
        writer,
        json!({"jsonrpc":"2.0","id":*next,"method":method,"params":params}),
    )?;
    response(incoming, writer, *next)
}
fn collect_symbols<'a>(symbols: &'a [Value], out: &mut Vec<&'a Value>) {
    for symbol in symbols {
        out.push(symbol);
        if let Some(children) = symbol["children"].as_array() {
            collect_symbols(children, out);
        }
    }
}
fn symbol_line(symbol: &Value) -> Option<usize> {
    symbol["selectionRange"]["start"]["line"]
        .as_u64()
        .map(|n| n as usize)
}
fn member_declaration(symbol: &Value, source: &str) -> Option<String> {
    let range = &symbol["range"];
    let start = range["start"]["line"].as_u64()? as usize;
    let end = range["end"]["line"].as_u64()? as usize;
    let mut parts = Vec::new();
    for (line, text) in source
        .lines()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start).min(30) + 1)
    {
        let from = if line == start {
            range["start"]["character"].as_u64()? as usize
        } else {
            0
        };
        let to = if line == end {
            range["end"]["character"].as_u64()? as usize
        } else {
            usize::MAX
        };
        let mut utf16 = 0;
        let mut part = String::new();
        for c in text.chars() {
            if utf16 >= from && utf16 < to {
                part.push(c);
            }
            utf16 += c.len_utf16();
        }
        parts.push(part);
    }
    let text = parts.join(" ");
    let text = if matches!(symbol["kind"].as_u64(), Some(6 | 9)) {
        text.split('{').next().unwrap_or(&text)
    } else {
        &text
    };
    Some(text.trim().chars().take(2000).collect())
}
fn member_sections(symbols: &[Value], source: &str, target_line: usize) -> String {
    let mut all = Vec::new();
    collect_symbols(symbols, &mut all);
    let lines: Vec<_> = source.lines().collect();
    let Some(first) = all.iter().find(|s| symbol_line(s) == Some(target_line)) else {
        return String::new();
    };
    let first = if first["kind"].as_u64() == Some(22) {
        all.iter()
            .find(|s| {
                s["kind"].as_u64() == Some(10)
                    && s["children"].as_array().is_some_and(|children| {
                        children
                            .iter()
                            .any(|child| symbol_line(child) == Some(target_line))
                    })
            })
            .copied()
            .unwrap_or(first)
    } else {
        first
    };
    let mut queue = std::collections::VecDeque::from([first]);
    let mut seen = std::collections::BTreeSet::new();
    let mut sections = Vec::new();
    while let Some(symbol) = queue.pop_front() {
        let Some(line) = symbol_line(symbol) else {
            continue;
        };
        if !seen.insert(line) || seen.len() > 12 {
            continue;
        }
        if let Some(children) = symbol["children"].as_array().filter(|c| !c.is_empty()) {
            let header = member_declaration(symbol, source).unwrap_or_default();
            let prefix = header.split('{').next().unwrap_or(&header);
            let enumeration =
                symbol["kind"].as_u64() == Some(10) || prefix.trim_start().starts_with("enum ");
            let public_default = enumeration
                || prefix.rfind("struct ") > prefix.rfind("class ")
                || prefix.rfind("union ") > prefix.rfind("class ");
            let members: Vec<_> = children
                .iter()
                .filter(|s| {
                    matches!(
                        s["kind"].as_u64(),
                        Some(5 | 6 | 7 | 8 | 9 | 10 | 13 | 22 | 23 | 26)
                    )
                })
                .filter(|s| enumeration || public_member(symbol, s, source, public_default))
                .filter_map(|s| member_declaration(s, source).map(|code| (s, code)))
                .filter(|(s, code)| {
                    static DELETED: std::sync::LazyLock<regex::Regex> =
                        std::sync::LazyLock::new(|| {
                            regex::Regex::new(r"=\s*delete(?:\s*\(.*\))?\s*;?\s*$")
                                .expect("fixed deleted declaration regex")
                        });
                    !matches!(s["kind"].as_u64(), Some(6 | 9)) || !DELETED.is_match(code)
                })
                .map(|(s, code)| {
                    if enumeration {
                        format!("{},", code.trim_end_matches([',', ';']))
                    } else {
                        pretty_declaration(&code, matches!(s["kind"].as_u64(), Some(6 | 9)))
                    }
                })
                .take(32)
                .collect();
            let declarations = members
                .iter()
                .map(|member| {
                    member
                        .lines()
                        .map(|line| format!("    {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !prefix.trim().is_empty() {
                sections.push(format!("{} {{\n{declarations}\n}};", prefix.trim()));
                if sections.len() >= 4 {
                    break;
                }
            }
        } else if let Some(declaration) = lines
            .get(line)
            .filter(|s| s.contains("using ") || s.contains("typedef "))
        {
            for name in declaration
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
            {
                let candidates: Vec<_> = all
                    .iter()
                    .filter(|s| s["name"].as_str() == Some(name))
                    .collect();
                if candidates.len() == 1 {
                    queue.push_back(*candidates[0]);
                }
            }
        }
    }
    sections.join("\n\n")
}
fn expand_members(
    writer: &mut impl Write,
    incoming: &mpsc::Receiver<Value>,
    next: &mut u64,
    target: &Target,
) -> Result<String> {
    let uri = url::Url::from_file_path(&target.path)
        .map_err(|_| anyhow::anyhow!("Invalid path"))?
        .to_string();
    let params = json!({"textDocument":{"uri":uri},"position":{"line":target.line,"character":target.character}});
    let root = target
        .path
        .ancestors()
        .skip(1)
        .find(|p| p.join(".git").exists() || p.join("compile_commands.json").exists())
        .or_else(|| target.path.parent())
        .context("Missing parent")?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let current_path = target
        .path
        .canonicalize()
        .unwrap_or_else(|_| target.path.clone());
    let mut sections = Vec::new();
    for method in ["textDocument/typeDefinition", "textDocument/definition"] {
        let locations = rpc(writer, incoming, next, method, params.clone())?;
        let location = if let Some(array) = locations.as_array() {
            array.first()
        } else if locations.is_object() {
            Some(&locations)
        } else {
            None
        };
        let Some(location) = location else {
            continue;
        };
        let Some(uri) = location["uri"]
            .as_str()
            .or_else(|| location["targetUri"].as_str())
        else {
            continue;
        };
        let path = url::Url::parse(uri)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Nonlocal definition"))?;
        // Keep implementation details of the C++ standard library out of member summaries.
        if !path.starts_with(&root) {
            continue;
        }
        let line = location["range"]["start"]["line"]
            .as_u64()
            .or_else(|| location["targetSelectionRange"]["start"]["line"].as_u64())
            .unwrap_or(0) as usize;
        let same = path == current_path;
        let uri = if same {
            params["textDocument"]["uri"]
                .as_str()
                .context("Missing URI")?
        } else {
            uri
        };
        let source = if same {
            target.text.to_string()
        } else {
            let metadata = std::fs::metadata(&path)?;
            if metadata.len() > 2 * 1024 * 1024 {
                continue;
            }
            std::fs::read_to_string(&path)?
        };
        if !same {
            send(
                writer,
                json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"cpp","version":1,"text":source}}}),
            )?;
        }
        let result = rpc(
            writer,
            incoming,
            next,
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        if !same {
            send(
                writer,
                json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}}),
            )?;
        }
        let result = result?;
        if let Some(symbols) = result.as_array() {
            let section = member_sections(symbols, &source, line);
            if !section.is_empty() && !sections.contains(&section) {
                sections.push(section);
            }
        }
    }
    Ok(sections.join("\n\n"))
}

impl Client {
    fn start(target: &Target) -> Result<Self> {
        let root = target
            .path
            .ancestors()
            .skip(1)
            .find(|p| p.join(".git").exists() || p.join("compile_commands.json").exists())
            .or_else(|| target.path.parent())
            .context("Missing parent")?
            .to_owned();
        let mut child = Command::new("clangd")
            .args(["--background-index=false", "--log=error"])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut writer = child.stdin.take().context("clangd stdin")?;
        let stdout = child.stdout.take().context("clangd stdout")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let (messages, incoming) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(message) = read_message(&mut reader) {
                if messages.send(message).is_err() {
                    break;
                }
            }
        });
        let (tx, requests) = mpsc::channel::<Request>();
        let (out, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let run = || -> Result<()> {
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"processId":std::process::id(),"rootUri":url::Url::from_directory_path(root).ok().map(|u| u.to_string()),"capabilities":{"textDocument":{"hover":{"contentFormat":["markdown","plaintext"]},"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}}}),
                )?;
                response(&incoming, &mut writer, 0)?;
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                )?;
                let mut opened: Option<(PathBuf, u64)> = None;
                let mut version = 0;
                let mut next_rpc = 0;
                while let Ok(mut request) = requests.recv() {
                    while let Ok(newer) = requests.try_recv() {
                        request = newer;
                    }
                    let t = &request.target;
                    let uri = url::Url::from_file_path(&t.path)
                        .map_err(|_| anyhow::anyhow!("Invalid path"))?
                        .to_string();
                    if opened.as_ref() != Some(&(t.path.clone(), t.revision)) {
                        if let Some((path, _)) = opened.take() {
                            send(
                                &mut writer,
                                json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":url::Url::from_file_path(path).ok().map(|u| u.to_string())}}}),
                            )?;
                        }
                        version += 1;
                        let language = if t.path.extension().is_some_and(|e| e == "c") {
                            "c"
                        } else {
                            "cpp"
                        };
                        send(
                            &mut writer,
                            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":language,"version":version,"text":t.text.as_ref()}}}),
                        )?;
                        opened = Some((t.path.clone(), t.revision));
                    }
                    let value = rpc(
                        &mut writer,
                        &incoming,
                        &mut next_rpc,
                        "textDocument/hover",
                        json!({"textDocument":{"uri":uri},"position":{"line":t.line,"character":t.character}}),
                    )?;
                    let (text, expand) = display_hover(&value["contents"]);
                    let _ = out.send((request.id, text.clone()));
                    if let Ok(members) = if expand {
                        expand_members(&mut writer, &incoming, &mut next_rpc, t)
                    } else {
                        Ok(String::new())
                    } {
                        if !members.is_empty() {
                            let text = if text.contains("using ") || text.contains("typedef ") {
                                format!("{text}\n\n{members}")
                            } else {
                                members
                            };
                            let _ = out.send((request.id, text));
                        }
                    }
                }
                Ok(())
            };
            let _ = run();
        });
        Ok(Self { tx, rx, child })
    }
}
fn readable_namespaces(text: &str) -> String {
    static TOKENS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?s)//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\bstd::__1::"#,
        )
        .expect("fixed tooltip namespace regex")
    });
    TOKENS
        .replace_all(text, |matched: &regex::Captures<'_>| {
            if &matched[0] == "std::__1::" {
                "std::".to_owned()
            } else {
                matched[0].to_owned()
            }
        })
        .into_owned()
}

fn tooltip_width(source: &str, available: u16) -> u16 {
    let widest = source
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    widest
        .saturating_add(2)
        .max(3)
        .min(usize::from(available.saturating_sub(4).min(80))) as u16
}

fn tooltip_text(
    source: &str,
    width: usize,
    settings: &pulldown_cmark_mdcat::Settings<'_>,
    plain: bool,
) -> Text<'static> {
    let fallback_themes = two_face::theme::extra();
    let fallback = fallback_themes.get(if settings.theme.is_dark {
        two_face::theme::EmbeddedThemeName::SolarizedDark
    } else {
        two_face::theme::EmbeddedThemeName::SolarizedLight
    });
    let theme = settings.syntax_theme.as_ref().unwrap_or(fallback);
    let syntax = settings
        .syntax_set
        .find_syntax_by_extension("cpp")
        .unwrap_or_else(|| settings.syntax_set.find_syntax_plain_text());
    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut rows = Vec::new();
    for line in source.lines() {
        let input = format!("{line}\n");
        let mut ranges = Vec::new();
        let mut offset = 0;
        if !plain {
            if let Ok(regions) = highlighter.highlight_line(&input, settings.syntax_set) {
                for (style, text) in regions {
                    let end = offset + text.len();
                    let mut rendered = Style::default().fg(Color::Rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    ));
                    if style
                        .font_style
                        .contains(syntect::highlighting::FontStyle::BOLD)
                    {
                        rendered = rendered.add_modifier(Modifier::BOLD);
                    }
                    if style
                        .font_style
                        .contains(syntect::highlighting::FontStyle::ITALIC)
                    {
                        rendered = rendered.add_modifier(Modifier::ITALIC);
                    }
                    ranges.push((offset..end, rendered));
                    offset = end;
                }
            }
        }
        let indent = " ".repeat(line.len() - line.trim_start().len());
        let mut consumed = 0;
        for wrapped in textwrap::wrap(
            line,
            textwrap::Options::new(width).subsequent_indent(&indent),
        ) {
            let content = wrapped.trim_start();
            let padding = wrapped.len() - content.len();
            let start = line[consumed..]
                .find(content)
                .map_or(consumed, |i| consumed + i);
            let end = (start + content.len()).min(line.len());
            let mut spans = vec![Span::raw(" ".repeat(padding))];
            if ranges.is_empty() {
                spans.push(Span::raw(content.to_owned()));
            } else {
                for (range, style) in &ranges {
                    let a = range.start.max(start);
                    let b = range.end.min(end);
                    if a < b {
                        spans.push(Span::styled(line[a..b].to_owned(), *style));
                    }
                }
            }
            rows.push(Line::from(spans));
            consumed = end;
        }
    }
    Text::from(rows)
}

#[derive(Default)]
pub struct Hover {
    client: Option<Client>,
    target: Option<Target>,
    since: Option<Instant>,
    id: u64,
    sent: bool,
    text: String,
    disabled: bool,
    popup: Rect,
    offset: usize,
    styled: Text<'static>,
    styled_source: String,
    styled_width: usize,
    styled_plain: bool,
}
impl Hover {
    pub fn update(&mut self, target: Option<Target>) {
        if !matches!((&self.target, &target), (Some(a), Some(b)) if a.same(b))
            && !(self.target.is_none() && target.is_none())
        {
            self.id += 1;
            self.target = target;
            self.since = Some(Instant::now());
            self.sent = false;
            self.text.clear();
            self.offset = 0;
        }
        self.poll();
        if !self.sent
            && !self.disabled
            && self
                .since
                .is_some_and(|s| s.elapsed() >= Duration::from_millis(450))
        {
            if let Some(target) = &self.target {
                self.sent = true;
                if self.client.is_none() {
                    match Client::start(target) {
                        Ok(c) => self.client = Some(c),
                        Err(_) => self.disabled = true,
                    }
                }
                if let Some(c) = &self.client {
                    let _ = c.tx.send(Request {
                        id: self.id,
                        target: target.clone(),
                    });
                }
            }
        }
    }
    fn poll(&mut self) {
        if let Some(client) = &self.client {
            while let Ok((id, text)) = client.rx.try_recv() {
                if id == self.id {
                    let text: String = text
                        .chars()
                        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                        .take(16000)
                        .collect();
                    self.text = readable_namespaces(&text);
                }
            }
        }
    }
    pub fn dismiss(&mut self) {
        self.id += 1;
        self.sent = true;
        self.text.clear();
        self.popup = Rect::default();
    }
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.visible() && self.popup.contains((column, row).into())
    }
    pub fn visible(&self) -> bool {
        !self.text.is_empty()
    }
    pub fn scroll(&mut self, column: u16, row: u16, down: bool) -> bool {
        if !self.visible() || !self.popup.contains((column, row).into()) {
            return false;
        }
        self.offset = if down {
            self.offset.saturating_add(3)
        } else {
            self.offset.saturating_sub(3)
        };
        true
    }
    pub fn draw(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        bounds: Rect,
        settings: &pulldown_cmark_mdcat::Settings<'_>,
        plain: bool,
    ) {
        self.poll();
        self.popup = Rect::default();
        if self.text.is_empty() || bounds.width < 15 || bounds.height < 5 {
            return;
        }
        let Some(target) = &self.target else {
            return;
        };
        frame.buffer_mut().set_style(
            target.rect,
            Style::default().add_modifier(Modifier::UNDERLINED),
        );
        let width = tooltip_width(&self.text, bounds.width);
        let content_width = usize::from(width - 2);
        if self.styled_source != self.text
            || self.styled_width != content_width
            || self.styled_plain != plain
        {
            self.styled = tooltip_text(&self.text, content_width, settings, plain);
            self.styled_source.clone_from(&self.text);
            self.styled_width = content_width;
            self.styled_plain = plain;
        }
        let lines = &self.styled.lines;
        let height = (lines.len() as u16)
            .saturating_add(2)
            .min(14)
            .min(bounds.height - 2);
        let x = target.rect.x.min(bounds.right() - width);
        let y = if target.rect.y + height + 1 < bounds.bottom() {
            target.rect.y + 1
        } else {
            target.rect.y.saturating_sub(height).max(bounds.y)
        };
        let rect = Rect::new(x, y, width, height);
        self.popup = rect;
        self.offset = self
            .offset
            .min(lines.len().saturating_sub(usize::from(height - 2)));
        let text = Text::from(
            lines
                .iter()
                .skip(self.offset)
                .take(usize::from(height - 2))
                .cloned()
                .collect::<Vec<_>>(),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
            rect,
        );
    }
}

pub fn target_at(
    path: &std::path::Path,
    source: Arc<str>,
    revision: u64,
    first_row: usize,
    text: &ratatui::text::Text<'_>,
    viewport: (usize, Rect),
    pointer: (u16, u16),
) -> Option<Target> {
    let (scroll, area) = viewport;
    let (column, row) = pointer;
    if column < area.x + 2
        || column >= area.right().saturating_sub(2)
        || row <= area.y
        || row >= area.bottom().saturating_sub(1)
    {
        return None;
    }
    let rendered_row = scroll + usize::from(row - area.y - 1);
    let line = rendered_row.checked_sub(first_row)?;
    let contents = source.lines().nth(line)?;
    if contents.trim().is_empty() {
        return None;
    }
    let rendered = text.lines.get(rendered_row)?.to_string();
    let prefix = rendered.find(contents)?;
    use unicode_width::UnicodeWidthStr;
    let padding = rendered[..prefix].width();
    let column = usize::from(column - area.x - 2).checked_sub(padding)?;
    let mut visual = 0;
    let mut hit = None;
    for (byte, c) in contents.char_indices() {
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if column >= visual && column < visual + width {
            hit = Some(byte);
            break;
        }
        visual += width;
    }
    let hit = hit?;
    let identifier = |c: char| c == '_' || c.is_alphanumeric();
    if !identifier(contents[hit..].chars().next()?) {
        return None;
    }
    let start = contents[..hit]
        .char_indices()
        .rev()
        .find(|(_, c)| !identifier(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let end = contents[hit..]
        .char_indices()
        .find(|(_, c)| !identifier(*c))
        .map_or(contents.len(), |(i, _)| hit + i);
    if contents[start..end].chars().next()?.is_numeric() {
        return None;
    }
    let character = contents[..start].encode_utf16().count();
    Some(Target {
        path: path.to_owned(),
        text: source.clone(),
        revision,
        line,
        character,
        rect: Rect::new(
            area.x + 2 + (padding + contents[..start].width()) as u16,
            row,
            (contents[start..end].width() as u16).min(
                area.right()
                    .saturating_sub(2 + area.x + 2 + (padding + contents[..start].width()) as u16),
            ),
            1,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarations_are_formatted_as_cpp() {
        assert_eq!(
            pretty_declaration(
                "document_control(\n    const document_control& other\n)",
                true
            ),
            "document_control(const document_control& other)"
        );
        assert_eq!(
            pretty_declaration(
                "document_control(const document_control& other) = default;",
                true
            ),
            "document_control(const document_control& other)"
        );
        assert_eq!(
            pretty_declaration("generator() = default;", true),
            "generator()"
        );
        assert_eq!(
            pretty_declaration("generator() noexcept =default", true),
            "generator() noexcept"
        );
        assert_eq!(
            readable_namespaces("std::__1::vector<std::__1::string> counts;"),
            "std::vector<std::string> counts;"
        );
        assert_eq!(
            readable_namespaces("const char *s = \"std::__1::vector\"; // std::__1::string"),
            "const char *s = \"std::__1::vector\"; // std::__1::string"
        );
        assert_eq!(
            readable_namespaces("mystd::__1::vector x;"),
            "mystd::__1::vector x;"
        );
        assert_eq!(
            pretty_declaration("foo(int x, int y): _x(x), _y(y)", true),
            "foo(\n    int x,\n    int y\n)"
        );
        assert_eq!(pretty_declaration("foo() : _x{0}, _y(0)", true), "foo()");
        assert_eq!(
            pretty_declaration("foo() noexcept(noexcept(ns::f())) : _x(0)", true),
            "foo() noexcept(noexcept(ns::f()))"
        );
        assert_eq!(
            pretty_declaration("auto f() -> ns::Type", true),
            "auto f() -> ns::Type"
        );
        assert_eq!(
            pretty_declaration(
                "public: const diagram_layout_hints *hints = nullptr;",
                false
            ),
            "const diagram_layout_hints *hints = nullptr;"
        );
        assert_eq!(
            pretty_declaration("protected: void reset()", true),
            "void reset()"
        );
        let (variable, expand) = display_hover(
            &json!({"kind":"markdown", "value":"### variable counts\nType: noise\n```cpp\nstd::unordered_map<std::string, std::size_t> counts\n```"}),
        );
        assert_eq!(
            variable,
            "std::unordered_map<std::string, std::size_t> counts;"
        );
        assert!(!expand);
        let (function, _) = display_hover(
            &json!({"kind":"markdown", "value":"### function generate\n```cpp\nresponse generator::generate(const std::string &instructions, const std::string &input, int max_tokens, bool use_chat_template)\n```"}),
        );
        assert_eq!(function, "response generator::generate(\n    const std::string &instructions,\n    const std::string &input,\n    int max_tokens,\n    bool use_chat_template\n)");
        assert_eq!(
            pretty_declaration(
                "void f(std::pair<int, int> x, const char *s = \"a,b\")",
                true
            ),
            "void f(\n    std::pair<int, int> x,\n    const char *s = \"a,b\"\n)"
        );
        let (macro_code, _) =
            display_hover(&json!({"value":"### macro MAX\n```cpp\n#define MAX 32\n```"}));
        assert_eq!(macro_code, "#define MAX 32");
    }
    #[test]
    fn clangd_class_summaries_hide_nonpublic_members_and_show_enums() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        let text: Arc<str> = "class generator {\n int hidden;\npublic:\n int generate(int max_tokens, bool use_chat_template);\n generator& operator=(const generator&) = delete;\n generator() = default;\nprotected:\n int protected_field;\n};\nenum class Mode : int {\n Fast = 1,\n Slow = 2\n};\n".into();
        std::fs::write(&path, text.as_ref()).unwrap();
        let target = Target {
            path,
            text,
            revision: 1,
            line: 0,
            character: 6,
            rect: Rect::new(1, 1, 6, 1),
        };
        let client = Client::start(&target).unwrap();
        client
            .tx
            .send(Request {
                id: 1,
                target: target.clone(),
            })
            .unwrap();
        let _ = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let (_, result) = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            result.starts_with("class generator {\n")
                && !result.contains("public:")
                && result.contains("generate(")
                && !result.contains("hidden")
                && !result.contains("protected_field")
                && !result.contains("operator=")
                && result.contains("generator()")
                && !result.contains("= default"),
            "{result}"
        );
        let mut target = target;
        target.line = target
            .text
            .lines()
            .position(|line| line.starts_with("enum class Mode"))
            .unwrap();
        target.character = 11;
        client.tx.send(Request { id: 2, target }).unwrap();
        let _ = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let (_, result) = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            result.contains("enum class Mode : int {")
                && result.contains("Fast = 1,")
                && result.contains("Slow = 2,"),
            "{result}"
        );
    }
    #[test]
    fn tooltip_syntax_survives_wrapping_and_plain_mode() {
        assert_eq!(tooltip_width("int x;", 100), 8);
        assert_eq!(tooltip_width("struct Foo {\n    int count;\n};", 100), 16);
        assert_eq!(tooltip_width("界界", 100), 6);
        assert_eq!(tooltip_width(&"x".repeat(200), 100), 80);
        assert_eq!(tooltip_width(&"x".repeat(200), 40), 36);
        let syntax = two_face::syntax::extra_newlines();
        let settings = pulldown_cmark_mdcat::Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        let source = "class generator {\n    const char *name = \"hello long string value\";\n};";
        let colored = tooltip_text(source, 22, &settings, false);
        let plain = tooltip_text(source, 22, &settings, true);
        assert_eq!(colored.to_string(), plain.to_string());
        assert!(colored
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg.is_some()));
        assert!(plain
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style == Style::default()));
        assert!(colored.lines.len() > 3);
        assert!(colored.lines[1].to_string().starts_with("    "));
    }
    #[test]
    fn clangd_variable_type_keeps_original_spelling() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        let text: Arc<str> = "namespace diagram { struct diagram_layout_hints {}; struct Owner { const diagram_layout_hints *hints = nullptr; }; }\n".into();
        std::fs::write(&path, text.as_ref()).unwrap();
        let target = Target {
            path,
            character: text.find("hints =").unwrap(),
            text,
            revision: 1,
            line: 0,
            rect: Rect::new(1, 1, 6, 1),
        };
        let client = Client::start(&target).unwrap();
        client.tx.send(Request { id: 1, target }).unwrap();
        let (_, result) = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert_eq!(result, "const diagram_layout_hints *hints = nullptr;");
    }
    #[test]
    fn framing_and_hover_formats() {
        let mut data = Vec::new();
        send(&mut data, json!({"id":1,"result":"é"})).unwrap();
        assert_eq!(
            read_message(&mut std::io::Cursor::new(data)).unwrap()["result"],
            "é"
        );
        assert_eq!(
            hover_text(&json!([{"language":"cpp","value":"int selected"},"Docs"])),
            "int selected\n\nDocs"
        );
    }
    #[test]
    fn stale_replies_and_dismissal() {
        let mut hover = Hover::default();
        let target = Target {
            path: "/tmp/a.cpp".into(),
            text: "int x;".into(),
            revision: 1,
            line: 0,
            character: 4,
            rect: Rect::new(4, 2, 1, 1),
        };
        hover.update(Some(target.clone()));
        hover.text = "int x".into();
        hover.dismiss();
        hover.update(Some(target.clone()));
        assert!(!hover.visible());
        assert!(hover.sent);
        let mut next = target;
        next.revision += 1;
        hover.update(Some(next));
        assert!(!hover.sent);
    }
    #[test]
    fn actual_clangd_hover_and_formatted_buffer() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        std::fs::write(&path, "int unrelated;").unwrap();
        let text: Arc<str> = "struct Selection { int member_id; };\nint f() {\n    Selection selected;\n    return selected.member_id;\n}\n".into();
        let target = Target {
            path,
            text,
            revision: 1,
            line: 3,
            character: 11,
            rect: Rect::new(1, 1, 8, 1),
        };
        let client = Client::start(&target).unwrap();
        client.tx.send(Request { id: 1, target }).unwrap();
        let (_, result) = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            result.contains("selected") && result.contains("Selection"),
            "{result}"
        );
    }
    #[test]
    fn clangd_expands_project_aliases_to_members() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        std::fs::write(dir.path().join("types.h"), "struct Diagnostic {\n int code;\n const char *message;\n};\nusing diagnostics = Diagnostic;\ntemplate<class T> using result = diagnostics;\n").unwrap();
        let text: Arc<str> = "#include \"types.h\"\nresult<int> f();\n".into();
        std::fs::write(&path, text.as_ref()).unwrap();
        let target = Target {
            path,
            text,
            revision: 1,
            line: 1,
            character: 0,
            rect: Rect::new(1, 1, 6, 1),
        };
        let client = Client::start(&target).unwrap();
        client.tx.send(Request { id: 1, target }).unwrap();
        let _ = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let (_, result) = client.rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            result.contains("struct Diagnostic {")
                && result.contains("int code")
                && result.contains("const char *message"),
            "{result}"
        );
    }
    #[test]
    fn popup_fits_terminal_and_ignores_late_response() {
        let (reply, rx) = mpsc::channel();
        let (tx, _) = mpsc::channel();
        let mut hover = Hover::default();
        let target = Target {
            path: "/tmp/a.cpp".into(),
            text: "int x;".into(),
            revision: 1,
            line: 0,
            character: 4,
            rect: Rect::new(35, 8, 1, 1),
        };
        hover.update(Some(target.clone()));
        hover.sent = true;
        hover.client = Some(Client {
            tx,
            rx,
            child: Arc::new(Mutex::new(None)),
        });
        reply.send((hover.id - 1, "stale".into())).unwrap();
        reply
            .send((hover.id, "int x\nDocumentation".into()))
            .unwrap();
        hover.update(Some(target));
        assert_eq!(hover.text, "int x\nDocumentation");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = pulldown_cmark_mdcat::Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        terminal
            .draw(|frame| hover.draw(frame, Rect::new(0, 0, 40, 10), &settings, false))
            .unwrap();
        assert!(hover.popup.right() <= 40 && hover.popup.bottom() <= 10);
        let top = (hover.popup.x..hover.popup.right())
            .map(|x| terminal.backend().buffer()[(x, hover.popup.y)].symbol())
            .collect::<String>();
        assert!(!top.contains("Symbol") && !top.contains("Esc"));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "D"));
    }
    #[test]
    fn unicode_screen_position_to_utf16() {
        let source: Arc<str> = "/* 😀 */ int selected;".into();
        let text = ratatui::text::Text::from("  /* 😀 */ int selected;");
        let target = target_at(
            std::path::Path::new("/tmp/a.cpp"),
            source,
            1,
            0,
            &text,
            (0, Rect::new(0, 0, 60, 10)),
            (17, 1),
        )
        .unwrap();
        assert_eq!(target.character, 13);
    }
}
