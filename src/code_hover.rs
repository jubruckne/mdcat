//! Asynchronous clangd hover support for displayed C/C++ source.
use anyhow::{Context, Result};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
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
#[derive(Clone)]
struct Request {
    id: u64,
    target: Target,
}
enum Work {
    Prepare(Target),
    Hover(Request),
}
enum NavigationWork {
    Target {
        request: Request,
        text: String,
        expand: bool,
    },
    IndexReady,
}
struct Client {
    tx: mpsc::Sender<Work>,
    rx: mpsc::Receiver<(u64, String)>,
    definitions: mpsc::Receiver<(u64, Option<String>)>,
    child: Arc<Mutex<Option<Child>>>,
    progress: Arc<Mutex<Option<Option<u8>>>>,
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
struct Connection {
    writer: Mutex<std::process::ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    documents: Mutex<HashMap<String, usize>>,
    next_version: AtomicU64,
}
impl Connection {
    fn send(&self, value: Value) -> Result<()> {
        send(&mut *self.writer.lock().unwrap(), value)
    }
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let result = (|| {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
            let value = rx.recv_timeout(Duration::from_secs(10))?;
            anyhow::ensure!(value.get("error").is_none(), "clangd request failed");
            Ok(value["result"].clone())
        })();
        if self.pending.lock().unwrap().remove(&id).is_some() {
            let _ =
                self.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}));
        }
        result
    }
    fn open_document(&self, uri: &str, language: &str, text: &str, replace: bool) -> Result<()> {
        let mut documents = self.documents.lock().unwrap();
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        if !documents.contains_key(uri) {
            self.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":language,"version":version,"text":text}}}))?;
        } else if replace {
            self.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]}}))?;
        }
        *documents.entry(uri.to_owned()).or_default() += 1;
        Ok(())
    }
    fn close_document(&self, uri: &str) -> Result<()> {
        let mut documents = self.documents.lock().unwrap();
        if let Some(users) = documents.get_mut(uri) {
            *users -= 1;
            if *users == 0 {
                documents.remove(uri);
                self.send(json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}}))?;
            }
        }
        Ok(())
    }
    fn warm_hover(&self, uri: &str) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"textDocument/hover","params":{"textDocument":{"uri":uri},"position":{"line":0,"character":0}}}))
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
fn expand_members(connection: &Connection, target: &Target) -> Result<String> {
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
        let locations = connection.request(method, params.clone())?;
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
            connection.open_document(uri, "cpp", &source, false)?;
        }
        let result = connection.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        if !same {
            connection.close_document(uri)?;
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

// Definition responses can contain a Location, Location[], or LocationLink[].
fn definition_target(value: &Value) -> Option<String> {
    if let Some(locations) = value.as_array() {
        return locations.iter().find_map(definition_target);
    }
    let uri = value["uri"]
        .as_str()
        .or_else(|| value["targetUri"].as_str())?;
    let mut url = url::Url::parse(uri).ok()?;
    let path = url.to_file_path().ok()?;
    if !path.is_file() {
        return None;
    }
    let start = if value.get("targetUri").is_some() {
        &value["targetSelectionRange"]["start"]
    } else {
        &value["range"]["start"]
    };
    let line = start["line"].as_u64()?.checked_add(1)?;
    url.set_fragment(Some(&format!("L{line}")));
    Some(url.to_string())
}

impl Client {
    fn request(&self, request: Request) -> Result<()> {
        self.tx.send(Work::Hover(request))?;
        Ok(())
    }

    fn start(target: &Target) -> Result<Self> {
        Self::start_command(target, Command::new("clangd"))
    }

    fn start_command(target: &Target, mut command: Command) -> Result<Self> {
        let root = target
            .path
            .ancestors()
            .skip(1)
            .find(|p| p.join(".git").exists() || p.join("compile_commands.json").exists())
            .or_else(|| target.path.parent())
            .context("Missing parent")?
            .to_owned();
        let mut child = command
            .args([
                "--background-index",
                "--background-index-priority=background",
                "-j=4",
                "--log=error",
            ])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let writer = child.stdin.take().context("clangd stdin")?;
        let stdout = child.stdout.take().context("clangd stdout")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let connection = Arc::new(Connection {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
            next_version: AtomicU64::new(1),
        });
        let progress = Arc::new(Mutex::new(None));
        let (tx, requests) = mpsc::channel::<Work>();
        let (navigation_tx, navigation_rx) = mpsc::channel::<NavigationWork>();
        let (out, rx) = mpsc::channel();
        let (definitions_out, definitions) = mpsc::channel();
        let reader_connection = connection.clone();
        let reader_navigation = navigation_tx.clone();
        let reader_progress = progress.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(message) = read_message(&mut reader) {
                if message["method"] == "$/progress"
                    && message["params"]["token"] == "backgroundIndexProgress"
                {
                    let value = &message["params"]["value"];
                    match value["kind"].as_str() {
                        Some("begin" | "report") => {
                            let mut progress = reader_progress.lock().unwrap();
                            let previous = (*progress).flatten();
                            *progress = Some(
                                value["percentage"]
                                    .as_u64()
                                    .map(|p| p.min(100) as u8)
                                    .or(previous),
                            );
                        }
                        Some("end") => {
                            *reader_progress.lock().unwrap() = None;
                            let _ = reader_navigation.send(NavigationWork::IndexReady);
                        }
                        _ => (),
                    }
                }
                if message.get("method").is_some() {
                    if let Some(id) = message.get("id") {
                        let _ =
                            reader_connection.send(json!({"jsonrpc":"2.0","id":id,"result":null}));
                    }
                } else if let Some(id) = message["id"].as_u64() {
                    if let Some(reply) = reader_connection.pending.lock().unwrap().remove(&id) {
                        let _ = reply.send(message);
                    }
                }
            }
            // Disconnect all outstanding callers if clangd exits.
            reader_connection.pending.lock().unwrap().clear();
            *reader_progress.lock().unwrap() = None;
        });
        let navigation_connection = connection.clone();
        let navigation_out = out.clone();
        std::thread::spawn(move || {
            let mut last_request: Option<Request> = None;
            while let Ok(mut work) = navigation_rx.recv() {
                while let Ok(newer) = navigation_rx.try_recv() {
                    if !matches!(newer, NavigationWork::IndexReady) {
                        work = newer;
                    }
                }
                let (request, enrichment) = match work {
                    NavigationWork::Target {
                        request,
                        text,
                        expand,
                    } => {
                        last_request = Some(request.clone());
                        (request, Some((text, expand)))
                    }
                    NavigationWork::IndexReady => {
                        let Some(request) = last_request.clone() else {
                            continue;
                        };
                        (request, None)
                    }
                };
                let t = &request.target;
                let Ok(uri) = url::Url::from_file_path(&t.path) else {
                    continue;
                };
                let destination = navigation_connection.request("textDocument/definition",
                    json!({"textDocument":{"uri":uri.as_str()},"position":{"line":t.line,"character":t.character}}))
                    .ok().and_then(|value| definition_target(&value));
                let _ = definitions_out.send((request.id, destination));
                if let Some((text, true)) = enrichment {
                    if let Ok(members) = expand_members(&navigation_connection, t) {
                        if !members.is_empty() {
                            let text = if text.contains("using ") || text.contains("typedef ") {
                                format!("{text}\n\n{members}")
                            } else {
                                members
                            };
                            let _ = navigation_out.send((request.id, text));
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            let run = || -> Result<()> {
                connection.request("initialize", json!({"processId":std::process::id(),"rootUri":url::Url::from_directory_path(root).ok().map(|u| u.to_string()),"capabilities":{"window":{"workDoneProgress":true,"implicitWorkDoneProgressCreate":true},"textDocument":{"hover":{"contentFormat":["markdown","plaintext"]},"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}}))?;
                connection.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))?;
                let mut opened: Option<(PathBuf, u64)> = None;
                while let Ok(mut work) = requests.recv() {
                    while let Ok(newer) = requests.try_recv() {
                        work = newer;
                    }
                    let (request, prepare_only) = match work {
                        Work::Prepare(target) => (Request { id: 0, target }, true),
                        Work::Hover(request) => (request, false),
                    };
                    let t = &request.target;
                    let uri = url::Url::from_file_path(&t.path)
                        .map_err(|_| anyhow::anyhow!("Invalid path"))?
                        .to_string();
                    if opened.as_ref() != Some(&(t.path.clone(), t.revision)) {
                        if let Some((path, _)) = opened.take() {
                            if let Ok(uri) = url::Url::from_file_path(path) {
                                connection.close_document(uri.as_str())?;
                            }
                        }
                        let language = if t.path.extension().is_some_and(|e| e == "c") {
                            "c"
                        } else {
                            "cpp"
                        };
                        connection.open_document(&uri, language, &t.text, true)?;
                        opened = Some((t.path.clone(), t.revision));
                    }
                    if prepare_only {
                        warm_tooltip_support();
                        connection.warm_hover(&uri)?;
                        continue;
                    }
                    let value = match connection.request("textDocument/hover", json!({"textDocument":{"uri":uri},"position":{"line":t.line,"character":t.character}})) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    let (text, expand) = display_hover(&value["contents"]);
                    let _ = out.send((request.id, text.clone()));
                    let _ = navigation_tx.send(NavigationWork::Target {
                        request,
                        text,
                        expand,
                    });
                }
                Ok(())
            };
            let _ = run();
        });
        Ok(Self {
            tx,
            rx,
            definitions,
            child,
            progress,
        })
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

static TOOLTIP_THEMES: std::sync::LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    std::sync::LazyLock::new(two_face::theme::extra);

fn warm_tooltip_support() {
    let _ = readable_namespaces("std::__1::string");
    let _ = display_hover(&json!("### function\n```cpp\nint prewarm(int value);\n```"));
    for theme in [
        two_face::theme::EmbeddedThemeName::SolarizedDark,
        two_face::theme::EmbeddedThemeName::SolarizedLight,
    ] {
        TOOLTIP_THEMES.get(theme);
    }
}

fn tooltip_text(
    source: &str,
    width: usize,
    settings: &pulldown_cmark_mdcat::Settings<'_>,
    plain: bool,
) -> Text<'static> {
    let theme = settings.syntax_theme.as_ref().unwrap_or_else(|| {
        TOOLTIP_THEMES.get(if settings.theme.is_dark {
            two_face::theme::EmbeddedThemeName::SolarizedDark
        } else {
            two_face::theme::EmbeddedThemeName::SolarizedLight
        })
    });
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
    prepared: Option<(PathBuf, u64)>,
    target: Option<Target>,
    since: Option<Instant>,
    id: u64,
    sent: bool,
    text: String,
    disabled: bool,
    destination: Option<String>,
    popup: Rect,
    offset: usize,
    styled: Text<'static>,
    styled_source: String,
    styled_width: usize,
    styled_plain: bool,
}
impl Hover {
    pub fn prepare(&mut self, path: &std::path::Path, text: Arc<str>, revision: u64) {
        if self.disabled
            || self
                .prepared
                .as_ref()
                .is_some_and(|(p, r)| p == path && *r == revision)
        {
            return;
        }
        let target = Target {
            path: path.to_owned(),
            text,
            revision,
            line: 0,
            character: 0,
            rect: Rect::default(),
        };
        if self.client.is_none() {
            match Client::start(&target) {
                Ok(client) => self.client = Some(client),
                Err(_) => self.disabled = true,
            }
        }
        if let Some(client) = &self.client {
            if client.tx.send(Work::Prepare(target)).is_ok() {
                self.prepared = Some((path.to_owned(), revision));
            }
        }
    }

    pub fn update(&mut self, target: Option<Target>) {
        if !matches!((&self.target, &target), (Some(a), Some(b)) if a.same(b))
            && !(self.target.is_none() && target.is_none())
        {
            self.id += 1;
            self.target = target;
            self.since = Some(Instant::now());
            self.sent = false;
            self.text.clear();
            self.destination = None;
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
                    let _ = c.request(Request {
                        id: self.id,
                        target: target.clone(),
                    });
                }
            }
        }
    }
    fn poll(&mut self) {
        for client in self.client.iter() {
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
            while let Ok((id, destination)) = client.definitions.try_recv() {
                if id == self.id {
                    self.destination = destination;
                }
            }
        }
    }
    pub fn indexing_status(&self) -> Option<String> {
        let progress = *self.client.as_ref()?.progress.lock().unwrap();
        progress.map(|percentage| match percentage {
            Some(percentage) => format!(" indexing {percentage}% "),
            None => " indexing… ".to_owned(),
        })
    }
    pub fn dismiss(&mut self) {
        self.id += 1;
        self.sent = true;
        self.text.clear();
        self.popup = Rect::default();
        self.destination = None;
    }
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.visible() && self.popup.contains((column, row).into())
    }
    pub fn destination_at(&self, column: u16, row: u16) -> Option<&str> {
        // A drawn popup means its symbol underline is also on screen.
        if self.popup.is_empty() || self.contains(column, row) {
            return None;
        }
        self.target
            .as_ref()
            .filter(|target| target.rect.contains((column, row).into()))?;
        self.destination.as_deref()
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
            .min(18)
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
        let visible = usize::from(height - 2);
        if lines.len() > visible {
            let track = Rect::new(rect.right() - 1, rect.y + 1, 1, height - 2);
            let mut state = ScrollbarState::new(lines.len() - visible + 1)
                .position(self.offset)
                .viewport_content_length(visible);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(if plain {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                })
                .thumb_style(if plain {
                    Style::default()
                } else {
                    Style::default().fg(Color::Gray)
                });
            frame.render_stateful_widget(scrollbar, track, &mut state);
        }
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
            .request(Request {
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
        client.request(Request { id: 2, target }).unwrap();
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
        client.request(Request { id: 1, target }).unwrap();
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
    fn definition_locations_use_local_files_and_selection_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with spaces.cpp");
        std::fs::write(&path, "int x;\n").unwrap();
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let expected = format!("{uri}#L8");
        let location = json!({"uri":uri,"range":{"start":{"line":7,"character":2}}});
        assert_eq!(
            definition_target(&location).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            definition_target(&json!([null, location])).as_deref(),
            Some(expected.as_str())
        );
        let link = json!({"targetUri":uri,"targetRange":{"start":{"line":0}},"targetSelectionRange":{"start":{"line":7}}});
        assert_eq!(
            definition_target(&json!([link])).as_deref(),
            Some(expected.as_str())
        );
        for missing in [
            Value::Null,
            json!([]),
            json!({"uri":"https://example.com/source.cpp","range":{"start":{"line":0}}}),
        ] {
            assert!(definition_target(&missing).is_none());
        }
        std::fs::remove_file(path).unwrap();
        assert!(definition_target(&location).is_none());
    }

    #[test]
    fn navigation_requires_a_drawn_underline_and_available_definition() {
        let target = Target {
            path: PathBuf::from("sample.cpp"),
            text: "int x;".into(),
            revision: 1,
            line: 0,
            character: 4,
            rect: Rect::new(4, 2, 1, 1),
        };
        let mut hover = Hover::default();
        hover.update(Some(target.clone()));
        hover.text = "int x".into();
        hover.destination = Some("file:///sample.cpp#L1".into());
        assert!(hover.destination_at(4, 2).is_none());
        hover.popup = Rect::new(4, 3, 10, 3);
        assert_eq!(hover.destination_at(4, 2), Some("file:///sample.cpp#L1"));
        assert!(hover.destination_at(3, 2).is_none());
        assert!(hover.destination_at(5, 2).is_none());
        assert!(hover.destination_at(4, 3).is_none());
        let mut next = target;
        next.character = 0;
        hover.update(Some(next));
        assert!(hover.destination_at(4, 2).is_none());
        hover.dismiss();
        assert!(hover.destination_at(4, 2).is_none());
    }

    #[test]
    fn clangd_resolves_a_function_call_to_its_body() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        let text: Arc<str> =
            "int compute();\nint caller() { return compute(); }\nint compute() { return 42; }\n"
                .into();
        std::fs::write(&path, text.as_ref()).unwrap();
        let expected = format!(
            "{}#L3",
            url::Url::from_file_path(path.canonicalize().unwrap()).unwrap()
        );
        let target = Target {
            path,
            text,
            revision: 1,
            line: 1,
            character: 23,
            rect: Rect::new(1, 1, 7, 1),
        };
        let client = Client::start(&target).unwrap();
        client.request(Request { id: 42, target }).unwrap();
        let (id, destination) = client
            .definitions
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        assert_eq!(id, 42);
        assert_eq!(destination, Some(expected));
    }

    #[test]
    fn clangd_finds_cpp_implementations_in_the_project_index() {
        if Command::new("clangd").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let header = root.join("api.h");
        let implementation = root.join("separate_implementation.cpp");
        let caller = root.join("caller.cpp");
        let header_text: Arc<str> = "int compute(int);\nint compute(double);\n".into();
        let caller_text: Arc<str> =
            "#include \"api.h\"\nint caller() { return compute(2); }\n".into();
        std::fs::write(&header, header_text.as_ref()).unwrap();
        std::fs::write(&caller, caller_text.as_ref()).unwrap();
        std::fs::write(&implementation, "#include \"api.h\"\nint compute(double x) { return 0; }\nint compute(int x) { return x; }\n").unwrap();
        let commands: Vec<_> = [&caller, &implementation].into_iter().map(|path| {
            json!({"directory":root,"file":path,"arguments":["clang++","-std=c++17","-c",path]})
        }).collect();
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::to_vec(&commands).unwrap(),
        )
        .unwrap();
        let expected = format!("{}#L3", url::Url::from_file_path(&implementation).unwrap());
        let target = Target {
            path: caller,
            text: caller_text,
            revision: 1,
            line: 1,
            character: 23,
            rect: Rect::new(1, 1, 7, 1),
        };
        let client = Client::start(&target).unwrap();
        // The implementation has never been opened: background indexing must find it.
        for (id, target) in [
            (1, target),
            (
                2,
                Target {
                    path: header,
                    text: header_text,
                    revision: 2,
                    line: 0,
                    character: 5,
                    rect: Rect::new(1, 1, 7, 1),
                },
            ),
        ] {
            client.request(Request { id, target }).unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let (reply_id, destination) = client
                    .definitions
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("clangd should find the implementation after indexing");
                if reply_id == id && destination.as_deref() == Some(&expected) {
                    break;
                }
            }
        }
    }

    #[test]
    fn one_connection_prewarms_reports_progress_and_keeps_hover_responsive() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, r#"
import json, pathlib, sys, time, threading
root = pathlib.Path(sys.argv[1])
with (root / "starts").open("a") as starts:
    starts.write("start\n")
output_lock = threading.Lock()
def emit(value):
    data = json.dumps(value).encode()
    with output_lock:
        sys.stdout.buffer.write(b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n" + data)
        sys.stdout.buffer.flush()
def progress(kind, percentage=None):
    value = {"kind": kind}
    if percentage is not None:
        value["percentage"] = percentage
    emit({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "backgroundIndexProgress", "value": value}})
def definition(request):
    progress("report", 32)
    (root / "pending").touch()
    while not (root / "release").exists():
        time.sleep(0.01)
    progress("end")
    emit({"jsonrpc": "2.0", "id": request["id"], "result": {"uri": request["params"]["textDocument"]["uri"], "range": {"start": {"line": 0}}}})
while True:
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            size = int(line.split(b":", 1)[1])
    request = json.loads(sys.stdin.buffer.read(size))
    if request["method"] == "textDocument/didOpen":
        with (root / "opened").open("a") as opened:
            opened.write(request["params"]["textDocument"]["text"] + "\n")
    if "id" not in request:
        continue
    method = request["method"]
    if method == "textDocument/definition":
        threading.Thread(target=definition, args=(request,), daemon=True).start()
        continue
    if method == "textDocument/hover":
        if request["params"]["position"] == {"line": 0, "character": 0}:
            progress("begin", 0)
            (root / "hover_warmed").touch()
        result = {"contents": "int selected;"}
    else:
        result = {}
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})
"#).unwrap();
        let path = dir.path().join("sample.cpp");
        std::fs::write(&path, "int selected;").unwrap();
        let target = Target {
            path,
            text: "int selected;".into(),
            revision: 1,
            line: 0,
            character: 4,
            rect: Rect::new(5, 2, 8, 1),
        };
        let mut command = Command::new("python3");
        command.arg("-u").arg(&script).arg(dir.path());
        let mut state = Hover {
            client: Some(Client::start_command(&target, command).unwrap()),
            ..Hover::default()
        };
        state.prepare(&target.path, target.text.clone(), target.revision);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("hover_warmed").exists()
            || state.indexing_status().as_deref() != Some(" indexing 0% ")
        {
            assert!(
                Instant::now() < deadline,
                "file and tooltip should warm before hovering"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(state.target.is_none());
        assert!(!state.visible());
        state.prepare(&target.path, target.text.clone(), target.revision);
        let client = state.client.as_ref().unwrap();
        assert!(matches!(
            client.rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let request = Request { id: 1, target };
        client.request(request.clone()).unwrap();
        assert_eq!(client.rx.recv_timeout(Duration::from_secs(5)).unwrap().0, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("opened")).unwrap(),
            "int selected;\n"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("pending").exists()
            || state.indexing_status().as_deref() != Some(" indexing 32% ")
        {
            assert!(
                Instant::now() < deadline,
                "navigation should start and report progress"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Both RPCs use the same process and pipe. The definition response is held
        // until explicitly released, and hover must still complete first.
        client.request(Request { id: 2, ..request }).unwrap();
        let (id, text) = client.rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((id, text.as_str()), (2, "int selected;"));
        assert!(matches!(
            client.definitions.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            "start\n"
        );
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert!(client
            .definitions
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .1
            .is_some());
        let (id, destination) = client
            .definitions
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(id, 2);
        assert!(destination.is_some());
        assert!(state.indexing_status().is_none());
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
        client.request(Request { id: 1, target }).unwrap();
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
        client.request(Request { id: 1, target }).unwrap();
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
        let (definition_reply, definitions) = mpsc::channel();
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
            definitions,
            child: Arc::new(Mutex::new(None)),
            progress: Arc::new(Mutex::new(None)),
        });
        reply.send((hover.id - 1, "stale".into())).unwrap();
        definition_reply
            .send((hover.id - 1, Some("file:///stale.cpp#L1".into())))
            .unwrap();
        reply
            .send((hover.id, "int x\nDocumentation".into()))
            .unwrap();
        hover.update(Some(target));
        assert_eq!(hover.text, "int x\nDocumentation");
        assert!(hover.destination.is_none());
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
    fn overflowing_popup_is_taller_and_shows_its_scroll_position() {
        let mut hover = Hover::default();
        hover.update(Some(Target {
            path: "/tmp/a.cpp".into(),
            text: "int x;".into(),
            revision: 1,
            line: 0,
            character: 4,
            rect: Rect::new(5, 2, 1, 1),
        }));
        hover.text = (0..40)
            .map(|i| format!("int member_{i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 30)).unwrap();
        let syntax = two_face::syntax::extra_newlines();
        let settings = pulldown_cmark_mdcat::Settings {
            terminal_capabilities: pulldown_cmark_mdcat::TerminalProgram::Ansi.capabilities(),
            terminal_size: Default::default(),
            syntax_set: &syntax,
            theme: Default::default(),
            syntax_theme: None,
        };
        terminal
            .draw(|frame| hover.draw(frame, frame.area(), &settings, true))
            .unwrap();
        assert_eq!(hover.popup.height, 18);
        let x = hover.popup.right() - 1;
        let top = hover.popup.y + 1;
        let bottom = hover.popup.bottom() - 2;
        assert_eq!(terminal.backend().buffer()[(x, top)].symbol(), "┃");
        assert_eq!(terminal.backend().buffer()[(x, bottom)].symbol(), "│");
        for _ in 0..20 {
            assert!(hover.scroll(x, top, true));
        }
        terminal
            .draw(|frame| hover.draw(frame, frame.area(), &settings, true))
            .unwrap();
        assert_eq!(hover.offset, 24);
        assert_eq!(terminal.backend().buffer()[(x, top)].symbol(), "│");
        assert_eq!(terminal.backend().buffer()[(x, bottom)].symbol(), "┃");
        terminal
            .draw(|frame| hover.draw(frame, Rect::new(0, 0, 60, 10), &settings, true))
            .unwrap();
        assert_eq!(hover.popup.height, 8);
        assert!(hover.popup.bottom() <= 10);
        hover.text = "int x;".into();
        terminal
            .draw(|frame| hover.draw(frame, frame.area(), &settings, true))
            .unwrap();
        assert_eq!(hover.popup.height, 3);
        assert_eq!(hover.offset, 0);
        assert_eq!(
            terminal.backend().buffer()[(hover.popup.right() - 1, hover.popup.y + 1)].symbol(),
            "│"
        );
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
