//! Brace scopes for the mouse-following code margin guide.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub first: usize,
    pub last: usize,
}

/// Attributes are part of declarations, not lambda captures.
fn without_attributes(header: &str) -> std::borrow::Cow<'_, str> {
    if !header.contains("[[") {
        return std::borrow::Cow::Borrowed(header);
    }
    let mut header = header.to_owned();
    while let Some(start) = header.find("[[") {
        let Some(end) = header[start + 2..].find("]]") else {
            break;
        };
        header.replace_range(start..start + 2 + end + 2, " ");
    }
    std::borrow::Cow::Owned(header)
}

fn function_header(header: &str) -> bool {
    let header = without_attributes(header);
    let Some(paren) = header.find('(') else {
        return false;
    };
    let prefix = header[..paren].trim();
    let name = prefix.split_whitespace().last().unwrap_or("");
    !name.is_empty()
        && header.contains(')')
        && !prefix.contains(['=', '[', ']'])
        && !header
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|word| matches!(word, "namespace" | "class" | "struct" | "union" | "enum"))
        && !matches!(
            name,
            "if" | "for"
                | "while"
                | "switch"
                | "catch"
                | "sizeof"
                | "alignof"
                | "decltype"
                | "static_assert"
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Type,
    Enum,
    Constant,
    Macro,
    Variable,
    Namespace,
    Using,
    Other,
}

impl SymbolKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Function => "ƒ",
            Self::Type => "◇",
            Self::Enum => "≡",
            Self::Constant | Self::Macro => "◆",
            Self::Variable => "○",
            Self::Namespace => "▣",
            Self::Using | Self::Other => "·",
        }
    }

    pub fn from_lsp(kind: u64) -> Self {
        match kind {
            6 | 9 | 12 => Self::Function,
            5 | 11 | 23 | 26 => Self::Type,
            10 => Self::Enum,
            14 | 22 => Self::Constant,
            7 | 8 | 13 => Self::Variable,
            2..=4 => Self::Namespace,
            _ => Self::Other,
        }
    }
}

#[derive(Debug)]
pub struct LocalSymbol {
    pub name: String,
    pub line: usize,
    pub kind: SymbolKind,
    pub container: Option<(usize, String)>,
}

fn declaration_name(header: &str) -> Option<(String, SymbolKind)> {
    let header = without_attributes(header);
    let header = header.trim();
    let mut tokens = header.split_whitespace();
    if tokens.any(|word| word == "using") {
        let mut name = tokens.next()?;
        if matches!(name, "enum" | "namespace") {
            name = tokens.next()?;
        }
        let name = name
            .split(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '_' | ':'))
            .next()?;
        return (!name.is_empty()).then(|| (name.to_owned(), SymbolKind::Using));
    }
    if function_header(header) {
        return header
            .split('(')
            .next()?
            .split_whitespace()
            .last()
            .map(|name| {
                (
                    name.trim_start_matches(['*', '&']).to_owned(),
                    SymbolKind::Function,
                )
            });
    }
    let mut words = header.split_whitespace();
    let mut kind = words.next()?;
    if kind == "inline" {
        kind = words.next()?;
    }
    if matches!(
        kind,
        "namespace" | "class" | "struct" | "union" | "enum" | "using"
    ) {
        let mut name = words.next()?;
        if kind == "enum" && matches!(name, "class" | "struct") {
            name = words.next()?;
        }
        let name = name
            .split(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '_' | ':'))
            .next()?;
        let symbol_kind = match kind {
            "namespace" => SymbolKind::Namespace,
            "enum" => SymbolKind::Enum,
            _ => SymbolKind::Type,
        };
        return (!name.is_empty() && name != "namespace").then(|| (name.to_owned(), symbol_kind));
    }
    // Simple global/member declarations for the immediate outline. clangd fills in
    // complex declarators when its semantic results arrive.
    let declaration = header.split('=').next()?.trim();
    if declaration.contains('(') || declaration.split_whitespace().count() < 2 {
        return None;
    }
    let name = declaration
        .split_whitespace()
        .last()?
        .trim_start_matches(['*', '&'])
        .split('[')
        .next()?;
    if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        return None;
    }
    let constant = declaration
        .split_whitespace()
        .any(|word| matches!(word, "const" | "constexpr"));
    Some((
        name.to_owned(),
        if constant {
            SymbolKind::Constant
        } else {
            SymbolKind::Variable
        },
    ))
}

fn push_enum_value(
    symbols: &mut Vec<LocalSymbol>,
    header: &str,
    line: usize,
    owner: (usize, String),
) {
    let name = header.trim().split('=').next().unwrap_or("").trim();
    if !name.is_empty() && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        symbols.push(LocalSymbol {
            name: name.into(),
            line,
            kind: SymbolKind::Constant,
            container: Some(owner),
        });
    }
}

pub fn local_symbols(source: &str) -> Vec<LocalSymbol> {
    scan(source).1
}

/// Ignore comments and literals so their braces cannot change the scope stack.
pub fn scopes(source: &str) -> Vec<Scope> {
    scan(source).0
}

fn scan(source: &str) -> (Vec<Scope>, Vec<LocalSymbol>) {
    let bytes = source.as_bytes();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut containers: Vec<Option<((usize, String), SymbolKind)>> = Vec::new();
    let mut header = String::new();
    let mut header_start = None;
    let mut result = Vec::new();
    let mut symbols = Vec::new();
    let mut row = 0;
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i] == b'#' {
            // Directives such as #if defined(...) are not declaration headers.
            header.clear();
            header_start = None;
            let directive = source[i + 1..].lines().next().unwrap_or("").trim_start();
            if let Some(rest) = directive
                .strip_prefix("define")
                .filter(|rest| rest.starts_with(char::is_whitespace))
            {
                let name: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                    .collect();
                if !name.is_empty() {
                    symbols.push(LocalSymbol {
                        name,
                        line: row,
                        kind: SymbolKind::Macro,
                        container: None,
                    });
                }
            }
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\\') {
                    break;
                }
                i += 1;
            }
        } else if bytes[i..].starts_with(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if bytes[i..].starts_with(b"R\"") {
            // C++ raw strings may have a custom delimiter of up to 16 characters.
            let delimiter_start = i + 2;
            if let Some(open) = bytes[delimiter_start..]
                .iter()
                .take(17)
                .position(|&b| b == b'(')
            {
                let delimiter = &source[delimiter_start..delimiter_start + open];
                let ending = format!("){delimiter}\"");
                let body = delimiter_start + open + 1;
                i = source[body..]
                    .find(&ending)
                    .map_or(bytes.len(), |end| body + end + ending.len());
            } else {
                i += 1;
            }
        } else if bytes[i] == b'\''
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
        {
            // C++ digit separators are not character literals.
            i += 1;
        } else if matches!(bytes[i], b'"' | b'\'') {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else {
            match bytes[i] {
                b'{' => {
                    let mut new_container = None;
                    if !stack.iter().any(|(_, function)| *function) {
                        if let Some((name, kind)) = declaration_name(&header) {
                            let group = matches!(kind, SymbolKind::Type | SymbolKind::Enum);
                            if group {
                                new_container =
                                    Some(((header_start.unwrap_or(row), name.clone()), kind));
                            }
                            symbols.push(LocalSymbol {
                                name,
                                line: header_start.unwrap_or(row),
                                kind,
                                container: if matches!(kind, SymbolKind::Type | SymbolKind::Enum) {
                                    None
                                } else {
                                    containers
                                        .iter()
                                        .rev()
                                        .flatten()
                                        .next()
                                        .map(|(owner, _)| owner.clone())
                                },
                            });
                        }
                    }
                    let function =
                        !stack.iter().any(|(_, function)| *function) && function_header(&header);
                    containers.push(new_container);
                    stack.push((
                        if function {
                            header_start.unwrap_or(row)
                        } else {
                            row
                        },
                        function,
                    ));
                    header.clear();
                    header_start = None;
                }
                b',' if containers
                    .last()
                    .is_some_and(|container| matches!(container, Some((_, SymbolKind::Enum)))) =>
                {
                    if let Some((owner, _)) = containers.last().cloned().flatten() {
                        push_enum_value(&mut symbols, &header, header_start.unwrap_or(row), owner);
                    }
                    header.clear();
                    header_start = None;
                }
                b'}' => {
                    if let Some((owner, SymbolKind::Enum)) = containers.pop().flatten() {
                        push_enum_value(&mut symbols, &header, header_start.unwrap_or(row), owner);
                    }
                    if let Some((first, function)) = stack.pop() {
                        if function && first < row {
                            result.push(Scope { first, last: row });
                        }
                    }
                    header.clear();
                    header_start = None;
                }
                b';' => {
                    if !stack.iter().any(|(_, function)| *function) {
                        if let Some((name, kind)) = declaration_name(&header) {
                            symbols.push(LocalSymbol {
                                name,
                                line: header_start.unwrap_or(row),
                                kind,
                                container: if matches!(kind, SymbolKind::Type | SymbolKind::Enum) {
                                    None
                                } else {
                                    containers
                                        .iter()
                                        .rev()
                                        .flatten()
                                        .next()
                                        .map(|(owner, _)| owner.clone())
                                },
                            });
                        }
                    }
                    header.clear();
                    header_start = None;
                }
                b':' if matches!(header.trim(), "public" | "private" | "protected") => {
                    header.clear();
                    header_start = None;
                }
                byte if byte.is_ascii() => {
                    if !byte.is_ascii_whitespace() && header_start.is_none() {
                        header_start = Some(row);
                    }
                    header.push(char::from(byte));
                }
                _ => {}
            }
            i += 1;
        }
        row += bytes[start..i].iter().filter(|&&b| b == b'\n').count();
    }
    (result, symbols)
}

/// Collect a possibly wrapped function definition from its first signature row.
pub fn definition(source: &str, first_row: usize) -> String {
    let mut parts = Vec::new();
    for line in source.lines().skip(first_row) {
        if let Some((header, _)) = line.split_once('{') {
            if !header.trim().is_empty() {
                parts.push(header.trim());
            }
            break;
        }
        parts.push(line.trim());
    }
    parts.join(" ").trim().to_owned()
}

pub fn at(scopes: &[Scope], row: usize) -> Option<Scope> {
    scopes
        .iter()
        .filter(|scope| scope.first <= row && row <= scope.last)
        .max_by_key(|scope| scope.last - scope.first)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributed_function_bodies_do_not_leak_calls_into_the_outline() {
        let source = "namespace app_internal {\n[[noreturn]] void emergency_exit_direct_sdf_timeout() {\n const char message[] = \"timeout\";\n const ssize_t written = ::write(STDERR_FILENO, message, sizeof(message) - 1); (void)written;\n std::_Exit(EXIT_FAILURE);\n}\n[[nodiscard(\"check result\")]]\nint value() {\n return external::read();\n}\n}\n";
        let symbols = local_symbols(source);
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(
            names,
            ["app_internal", "emergency_exit_direct_sdf_timeout", "value"]
        );
        assert_eq!(at(&scopes(source), 4), Some(Scope { first: 1, last: 5 }));
    }

    #[test]
    fn local_outline_includes_types_prototypes_and_single_line_functions() {
        let source = "#include <external.h>\nnamespace demo {\nstruct Thing {\n void declared(int value);\n void defined() { if (ok) { work(); } }\n};\n// void fake();\nusing Alias = Thing;\n}\n";
        let symbols = local_symbols(source);
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, ["demo", "Thing", "declared", "defined", "Alias"]);
        assert_eq!(
            (symbols[2].line, symbols[2].kind),
            (3, SymbolKind::Function)
        );
    }

    #[test]
    fn scope_starts_at_first_signature_row_including_a_separate_brace() {
        for brace in [") {", ")\n{"] {
            let source = format!("namespace test {{\n// A comment.\nvoid record_image_barriers(\n    int commands,\n    int source{brace}\n    work();\n}}\n}}\n");
            let scopes = scopes(&source);
            let scope = at(&scopes, 2).unwrap();
            assert_eq!(scope.first, 2);
            assert_eq!(at(&scopes, 3), Some(scope));
            assert_eq!(at(&scopes, scope.last - 1), Some(scope));
            assert_eq!(
                definition(&source, scope.first),
                "void record_image_barriers( int commands, int source)"
            );
        }
    }

    #[test]
    fn definition_keeps_wrapped_signatures() {
        let source = "namespace test {\n\nvoid Example::render(\n    int width,\n    int height) {\n work();\n}\n}\n";
        assert_eq!(
            definition(source, 2),
            "void Example::render( int width, int height)"
        );
    }

    #[test]
    fn preprocessor_conditions_do_not_turn_namespaces_into_functions() {
        let source = "#if defined(__APPLE__)\n#include <pthread.h>\n#endif\nnamespace shader_works {\nnamespace {\nvoid first() {\n if (ok) {\n  work();\n }\n}\n}\nvoid second() {\n work();\n}\n}\n";
        let scopes = scopes(source);
        assert_eq!(at(&scopes, 7), Some(Scope { first: 5, last: 9 }));
        assert_eq!(
            at(&scopes, 12),
            Some(Scope {
                first: 11,
                last: 13
            })
        );
        assert_eq!(at(&scopes, 3), None);
    }

    #[test]
    fn selects_functions_inside_namespaces_and_classes() {
        let source = "namespace outer {\nnamespace inner {\nclass Example {\n void method() {\n  for (;;) {\n   if (ok) {\n    work();\n   }\n  }\n }\n void second() {\n  work();\n }\n};\n}\n}\n";
        let scopes = scopes(source);
        assert_eq!(at(&scopes, 6), Some(Scope { first: 3, last: 9 }));
        assert_eq!(
            at(&scopes, 11),
            Some(Scope {
                first: 10,
                last: 12
            })
        );
        assert_eq!(at(&scopes, 1), None);
        assert_eq!(at(&scopes, 13), None);
    }

    #[test]
    fn selects_function_and_ignores_literal_and_comment_braces() {
        let source = "void f() {\n for (;;) {\n  if (ok) {\n   use(\"}\"); // {\n   /* } */ use(R\"tag({})tag\");\n  }\n }\n}\n";
        let scopes = scopes(source);
        assert_eq!(at(&scopes, 1), Some(Scope { first: 0, last: 7 }));
        assert_eq!(at(&scopes, 3), Some(Scope { first: 0, last: 7 }));
        assert_eq!(at(&scopes, 7), Some(Scope { first: 0, last: 7 }));
        assert_eq!(at(&scopes, 8), None);
    }
}
