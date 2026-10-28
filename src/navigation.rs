//! Local document loading and destination resolution for the built-in viewer.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use pulldown_cmark_mdcat::{expand_tabs, Environment};
use url::Url;

pub struct Document {
    pub path: PathBuf,
    pub title: Option<String>,
    pub source: String,
    pub input: String,
    unformatted_input: Option<String>,
    pub environment: Environment,
    pub language: Option<String>,
    pub json: Option<crate::json_view::JsonView>,
    pub diff: Option<crate::diff_view::DiffView>,
    pub blame: Option<crate::blame_view::BlameView>,
    pub file_history: Option<crate::history_view::HistoryView>,
    pub original_view: Option<(Box<Document>, usize, Option<String>)>,
    pub image: Option<image::DynamicImage>,
    pub is_directory: bool,
    pub directory_view: Option<crate::directory_view::DirectoryView>,
    pub directory_changes: std::collections::HashSet<PathBuf>,
    project_root: Option<PathBuf>,
    pub commits: crate::git_links::GitLinks,
}

/// Parse an attached, positive, one-based source line number.
pub fn split_line_reference(reference: &str) -> Option<(&str, usize)> {
    let (path, line) = reference.rsplit_once(':')?;
    if path.is_empty()
        || path.contains(['#', '?'])
        || line.is_empty()
        || !line.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let line = line.parse::<usize>().ok().filter(|line| *line > 0)?;
    Some((path, line))
}

pub fn source_line(fragment: &str) -> Option<usize> {
    fragment
        .strip_prefix('L')?
        .parse::<usize>()
        .ok()
        .filter(|line| *line > 0)
}

impl Document {
    /// Open a viewer argument, optionally ending with a one-based `:line` suffix.
    pub fn load_argument(
        reference: &str,
        options: mdcat::RenderOptions,
    ) -> Result<(Self, Option<usize>)> {
        // A literal existing filename wins, including names containing colons.
        let (filename, line) = if std::path::Path::new(reference).exists() {
            (reference, None)
        } else {
            split_line_reference(reference)
                .map_or((reference, None), |(path, line)| (path, Some(line)))
        };
        let mut document = Self::load(filename.into(), options)?;
        if line.is_some() {
            if document.is_directory || document.image.is_some() {
                bail!("Source line references require a text file");
            }
            document.show_source(options);
        }
        Ok((document, line))
    }

    pub fn load(path: PathBuf, options: mdcat::RenderOptions) -> Result<Self> {
        // Resolve relative links from the displayed path, including symlink paths.
        let path = std::env::current_dir()?.join(path);
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_dir() {
            return Self::load_directory(path);
        }
        if !metadata.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if mdcat::is_image_path(&path) {
            let image = image::ImageReader::open(&path)
                .with_context(|| format!("Failed to open {}", path.display()))?
                .with_guessed_format()?
                .decode()
                .with_context(|| format!("Failed to decode image {}", path.display()))?;
            let environment =
                Environment::for_local_directory(&path.parent().context("Missing image parent")?)?;
            let project_root = path
                .ancestors()
                .skip(1)
                .find(|dir| dir.join(".git").exists())
                .map(PathBuf::from);
            return Ok(Self {
                path,
                title: None,
                source: String::new(),
                input: String::new(),
                unformatted_input: None,
                blame: None,
                file_history: None,
                original_view: None,
                environment,
                language: None,
                json: None,
                diff: None,
                image: Some(image),
                is_directory: false,
                directory_view: None,
                directory_changes: Default::default(),
                commits: crate::git_links::GitLinks::new(project_root.clone()),
                project_root,
            });
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if source.contains('\0') {
            bail!("{} contains binary data", path.display());
        }
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let json = if extension == "json" {
            crate::json_view::JsonView::parse(&source)
        } else if matches!(extension.as_str(), "txt" | "log") {
            crate::json_view::JsonView::parse_embedded(&source)
        } else {
            None
        };
        let markdown = ["md", "markdown", "mdown", "mkd", "mkdn"].contains(&extension.as_str());
        let language = if markdown {
            None
        } else {
            Some(
                match extension.as_str() {
                    "h" | "hpp" | "hh" | "hxx" | "cc" | "cxx" | "metal" => "cpp",
                    "py" => "python",
                    "txt" | "log" => "",
                    other => other,
                }
                .to_owned(),
            )
        };
        let (body, frontmatter) = if markdown {
            pulldown_cmark_mdcat::split_frontmatter(&source)
        } else {
            (source.as_str(), None)
        };
        let mut input = if options.frontmatter && markdown {
            frontmatter
                .and_then(|(format, raw)| {
                    pulldown_cmark_mdcat::frontmatter_table_markdown(format, raw)
                })
                .map_or_else(|| body.to_owned(), |table| format!("{table}\n{body}"))
        } else {
            body.to_owned()
        };
        if let Some(width) = options.tabs {
            input = expand_tabs(&input, width).into_owned();
        }
        let environment =
            Environment::for_local_directory(&path.parent().context("Missing parent")?)?;
        let project_root: Option<PathBuf> = path
            .ancestors()
            .skip(1)
            .find(|dir| dir.join(".git").exists())
            .map(PathBuf::from);
        let diff = if matches!(extension.as_str(), "diff" | "patch")
            && source.lines().any(|line| line.starts_with("diff --git "))
        {
            Some(crate::diff_view::DiffView::parse(&source))
        } else {
            None
        };
        let title = diff
            .as_ref()
            .and_then(|_| crate::git_links::commit_title(&source));
        let commits = crate::git_links::GitLinks::new(project_root.clone());
        commits.references(&source);
        Ok(Self {
            unformatted_input: None,
            blame: None,
            file_history: None,
            original_view: None,
            commits,
            diff,
            image: None,
            is_directory: false,
            directory_view: None,
            directory_changes: Default::default(),
            title,
            project_root,
            language,
            json,
            path,
            source,
            input,
            environment,
        })
    }

    fn load_directory(path: PathBuf) -> Result<Self> {
        let mut entries = std::fs::read_dir(&path)
            .with_context(|| format!("Failed to read directory {}", path.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by(|a, b| {
            let a_dir = a.path().is_dir();
            let b_dir = b.path().is_dir();
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.file_name().cmp(&b.file_name()))
        });
        let mut input = String::new();
        for entry in entries {
            let entry_path = entry.path();
            let is_dir = entry_path.is_dir();
            let url = if is_dir {
                Url::from_directory_path(&entry_path)
            } else {
                Url::from_file_path(&entry_path)
            };
            let Ok(url) = url else { continue };
            let label = entry
                .file_name()
                .to_string_lossy()
                .chars()
                .map(|c| match c {
                    '\n' | '\r' | '\t' => " ".to_owned(),
                    '\\' | '[' | ']' => format!("\\{c}"),
                    c if c.is_control() => "�".to_owned(),
                    c => c.to_string(),
                })
                .collect::<String>();
            input.push_str(&format!(
                "- [{label}{}]({url})\n",
                if is_dir { "/" } else { "" }
            ));
        }
        if input.is_empty() {
            input.push_str("Empty directory.\n");
        }
        let environment = Environment::for_local_directory(&path)?;
        let project_root = path
            .ancestors()
            .find(|dir| dir.join(".git").exists())
            .map(PathBuf::from);
        let commits = crate::git_links::GitLinks::new(project_root.clone());
        let directory_changes = crate::git_directory::changed_entries(&path);
        let directory_view = crate::directory_view::DirectoryView::load(&path).ok();
        Ok(Self {
            unformatted_input: None,
            blame: None,
            file_history: None,
            original_view: None,
            path,
            title: None,
            source: input.clone(),
            input,
            environment,
            language: None,
            json: None,
            diff: None,
            image: None,
            is_directory: true,
            directory_view,
            directory_changes,
            project_root,
            commits,
        })
    }

    pub fn open_directory_diff(&self) -> Result<Self> {
        let patch = self
            .directory_view
            .as_ref()
            .context("No directory comparison")?
            .patch();
        let mut next = Self::load_directory(self.path.clone())?;
        next.directory_view = None;
        next.is_directory = false;
        next.title = Some("Directory comparison".into());
        next.path = self
            .commits
            .root
            .as_ref()
            .unwrap_or(&self.path)
            .join(".git/directory comparison");
        next.source = patch.to_owned();
        next.input = next.source.clone();
        let mut diff = crate::diff_view::DiffView::parse(patch);
        diff.set_comparison_label("HEAD → working tree");
        diff.fold_all(false);
        next.diff = Some(diff);
        next.language = Some("diff".into());
        Ok(next)
    }

    pub fn open_history(&self) -> Result<Self> {
        if self.is_directory || self.image.is_some() || !self.path.is_file() {
            bail!("History requires a tracked text file");
        }
        let root = self
            .commits
            .root
            .as_ref()
            .context("No Git repository for this file")?;
        let view = crate::history_view::HistoryView::load(root, &self.path)?;
        let mut next = Self::load(self.path.clone(), Default::default())?;
        next.title = Some(format!(
            "History: {}",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        ));
        next.json = None;
        next.diff = None;
        next.file_history = Some(view);
        Ok(next)
    }

    pub fn open_history_diff(&self) -> Result<Self> {
        let view = self.file_history.as_ref().context("No history view")?;
        let mut next = Self::load(self.path.clone(), Default::default())?;
        next.title = Some("History diff".into());
        // This is a generated comparison, not the working file's contents.
        // Match commit views so source-only actions cannot blame the patch text.
        if let Some(root) = &self.commits.root {
            next.path = root.join(".git/history comparison");
        }
        next.source = view.patch().to_owned();
        next.input = next.source.clone();
        let mut diff = crate::diff_view::DiffView::parse(&next.source);
        diff.fold_all(false);
        next.diff = Some(diff);
        next.json = None;
        next.language = Some("diff".into());
        Ok(next)
    }

    pub fn open_blame(&self) -> Result<Self> {
        if self.is_directory || self.image.is_some() || !self.path.is_file() {
            bail!("Git blame requires a tracked text file");
        }
        let root = self
            .commits
            .root
            .as_ref()
            .context("No Git repository for this file")?;
        let blame = crate::blame_view::BlameView::load(&self.path, root, &self.source)?;
        Ok(Self {
            path: self.path.clone(),
            title: Some(format!(
                "Blame: {}",
                self.path.file_name().unwrap_or_default().to_string_lossy()
            )),
            source: self.source.clone(),
            input: self.source.clone(),
            unformatted_input: None,
            environment: self.environment.clone(),
            language: Some(self.language.clone().unwrap_or_else(|| "md".into())),
            json: None,
            diff: None,
            blame: Some(blame),
            file_history: None,
            original_view: None,
            image: None,
            is_directory: false,
            directory_view: None,
            directory_changes: Default::default(),
            project_root: self.project_root.clone(),
            commits: crate::git_links::GitLinks::new(Some(root.clone())),
        })
    }

    pub fn commit_link(&self, reference: &str) -> Option<Url> {
        self.commits
            .references(reference)
            .into_iter()
            .find(|(range, _)| range.start == 0 && range.end == reference.len())
            .map(|(_, url)| url)
    }

    pub fn can_open(&self, target: &str) -> bool {
        crate::git_links::commit_id(target).is_some() && self.commits.root.is_some()
            || self.destination(target).is_ok()
    }

    pub fn open_commit(&self, target: &str) -> Result<Self> {
        let (root, source) = self.commits.show(target)?;
        let id = crate::git_links::commit_id(target).context("Invalid Git commit")?;
        Ok(Self {
            path: root.join(format!(".git/commit {}", &id[..12])),
            title: crate::git_links::commit_title(&source),
            input: source.clone(),
            unformatted_input: None,
            blame: None,
            file_history: None,
            original_view: None,
            diff: Some(crate::diff_view::DiffView::parse(&source)),
            image: None,
            is_directory: false,
            directory_view: None,
            directory_changes: Default::default(),
            source,
            environment: Environment::for_local_directory(&root)?,
            language: Some("diff".into()),
            json: None,
            project_root: Some(root.clone()),
            commits: crate::git_links::GitLinks::new(Some(root)),
        })
    }

    /// Recognize existing local file paths written as inline code in project reports.
    /// Document-relative paths take precedence over repository-relative references.
    pub fn code_link(&self, reference: &str) -> Option<Url> {
        let (filename, fragment) = reference
            .split_once('#')
            .map_or((reference, None), |(path, fragment)| (path, Some(fragment)));
        let (filename, line) = if fragment.is_none() {
            split_line_reference(filename)
                .map_or((filename, None), |(path, line)| (path, Some(line)))
        } else {
            (filename, None)
        };
        if filename.is_empty() {
            return None;
        }
        let relative = self.path.parent()?.join(filename);
        let path = if relative.is_file() {
            relative
        } else {
            self.project_root.as_ref()?.join(filename)
        };
        if !path.is_file() {
            return None;
        }
        let mut url = Url::from_file_path(path).ok()?;
        if let Some(line) = line {
            url.set_fragment(Some(&format!("L{line}")));
        } else {
            url.set_fragment(fragment);
        }
        self.destination(url.as_str()).ok()?;
        Some(url)
    }

    pub fn line_link(&self, reference: &str) -> Option<Url> {
        let (path, line) = split_line_reference(reference)?;
        if let Some(url) = self.code_link(reference) {
            return Some(url);
        }
        let mut url = self.environment.base_url.join(path).ok()?;
        if url.scheme() != "file" {
            return None;
        }
        url.set_fragment(Some(&format!("L{line}")));
        Some(url)
    }

    pub fn show_source(&mut self, options: mdcat::RenderOptions) {
        if let Some(input) = self.unformatted_input.take() {
            self.input = input;
        }
        self.json = None;
        self.diff = None;
        if self.language.is_none() {
            self.language = Some("md".into());
            self.input = options.tabs.map_or_else(
                || self.source.clone(),
                |width| expand_tabs(&self.source, width).into_owned(),
            );
        }
    }

    pub fn can_format(&self) -> bool {
        self.language.is_some()
            && self.json.is_none()
            && self.diff.is_none()
            && self.blame.is_none()
            && self.file_history.is_none()
            && self.image.is_none()
            && !self.is_directory
            && crate::code_format::supports(&self.path)
    }

    pub fn is_formatted(&self) -> bool {
        self.unformatted_input.is_some()
    }

    /// Toggle a formatted presentation without changing the source or file.
    pub fn toggle_formatting(&mut self) -> Result<bool> {
        if let Some(input) = self.unformatted_input.take() {
            self.input = input;
            return Ok(false);
        }
        if !self.can_format() {
            bail!("Formatting is available for C/C++, GLSL, HLSL, and Metal source files");
        }
        let formatted = crate::code_format::format(&self.source)?;
        self.unformatted_input = Some(std::mem::replace(&mut self.input, formatted));
        Ok(true)
    }

    /// Find existing path arguments in commands without interpreting or running the command.
    /// Returned ranges refer to the original text, excluding surrounding quotes/punctuation.
    pub fn file_references(&self, text: &str) -> Vec<(std::ops::Range<usize>, Url)> {
        let mut references = Vec::new();
        let mut cursor = 0;
        while cursor < text.len() {
            let c = text[cursor..].chars().next().unwrap();
            if c.is_whitespace() {
                cursor += c.len_utf8();
                continue;
            }
            let quoted = matches!(c, '\'' | '"');
            let start = cursor + if quoted { c.len_utf8() } else { 0 };
            let end = text[start..]
                .char_indices()
                .find(|(_, next)| {
                    if quoted {
                        *next == c || *next == '\n'
                    } else {
                        next.is_whitespace()
                    }
                })
                .map_or(text.len(), |(offset, _)| start + offset);
            cursor = end;
            if cursor < text.len() {
                cursor += text[cursor..].chars().next().unwrap().len_utf8();
            }
            let raw = &text[start..end];
            let value = raw
                .trim_start_matches(['`', '(', '[', '{'])
                .trim_end_matches(['`', ')', ']', '}', ',', ';', '.']);
            let Some(offset) = raw.find(value) else {
                continue;
            };
            if value.is_empty()
                || value.starts_with('-')
                || (!value.contains('/') && std::path::Path::new(value).extension().is_none())
            {
                continue;
            }
            if let Some(url) = self.code_link(value) {
                references.push((start + offset..start + offset + value.len(), url));
            }
        }
        references
    }

    pub fn destination(&self, target: &str) -> Result<PathBuf> {
        let mut url = self.environment.base_url.join(target)?;
        if url.scheme() != "file" {
            bail!("Only local file links can be opened in this viewer");
        }
        if let Some(host) = url.host_str() {
            if host != "localhost" && !host.eq_ignore_ascii_case(&self.environment.hostname) {
                bail!("Cannot open a file on another host: {host}");
            }
            url.set_host(Some("localhost"))
                .map_err(|_| anyhow::anyhow!("Invalid file URL"))?;
        }
        if url.query().is_some() {
            bail!("File links with query parameters are not supported");
        }
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Invalid file URL"))?;
        Ok(path)
    }

    pub fn link_label(&self, target: &str) -> Result<String> {
        if let Some(id) = crate::git_links::commit_id(target) {
            return Ok(format!("commit {}", &id[..12]));
        }
        let path = self.destination(target)?;
        let base = self.project_root.as_deref().or_else(|| self.path.parent());
        let relative = base.and_then(|base| path.strip_prefix(base).ok());
        let mut label = relative.map_or_else(
            || path.display().to_string(),
            |path| format!("./{}", path.display()),
        );
        if let Some(fragment) = Url::parse(target)?
            .fragment()
            .filter(|fragment| !fragment.is_empty())
        {
            if let Some(line) = source_line(fragment) {
                label.push_str(&format!(":{line}"));
            }
        }
        Ok(label)
    }

    pub fn file_url(&self) -> Result<Url> {
        Url::from_file_path(&self.path).map_err(|_| anyhow::anyhow!("Invalid document path"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_arguments_open_original_source_at_attached_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("options.mm", "// first\nvoid main() {}\n"),
            ("report.md", "---\nkey: value\n---\n# Heading\n"),
            ("data.json", "{\n\"nested\": {\"value\": 1}\n}\n"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, source).unwrap();
            let (document, line) =
                Document::load_argument(&format!("{}:2", path.display()), Default::default())
                    .unwrap();
            assert_eq!(line, Some(2));
            assert_eq!(document.path, path);
            assert_eq!(document.input, source);
            assert!(document.json.is_none());
            assert!(document.language.is_some());
            for invalid in ["0", "-2", "999999999999999999999999999"] {
                assert!(Document::load_argument(
                    &format!("{}:{invalid}", path.display()),
                    Default::default(),
                )
                .is_err());
            }
        }
        #[cfg(not(windows))]
        {
            // Existing colon-containing names keep their literal meaning.
            let path = dir.path().join("literal:22");
            std::fs::write(&path, "literal file\n").unwrap();
            let (document, line) =
                Document::load_argument(path.to_str().unwrap(), Default::default()).unwrap();
            assert_eq!(document.path, path);
            assert_eq!(line, None);
        }
        assert!(Document::load_argument(
            &format!("{}:2", dir.path().display()),
            Default::default(),
        )
        .is_err());
    }

    #[test]
    fn attached_line_numbers_resolve_to_file_and_source_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "report").unwrap();
        let file = dir.path().join("emitted.glsl");
        std::fs::write(&file, "void main() {}\n").unwrap();
        let doc = Document::load(report, Default::default()).unwrap();
        let url = doc.code_link("emitted.glsl:172").unwrap();
        assert_eq!(doc.destination(url.as_str()).unwrap(), file);
        assert_eq!(url.fragment(), Some("L172"));
        assert_eq!(doc.link_label(url.as_str()).unwrap(), "./emitted.glsl:172");
        let command = "cat emitted.glsl:172";
        let references = doc.file_references(command);
        assert_eq!(&command[references[0].0.clone()], "emitted.glsl:172");
        assert_eq!(references[0].1, url);
        for invalid in [
            "emitted.glsl:0",
            "emitted.glsl:-2",
            "emitted.glsl:99999999999999999999999999",
            "#heading:12",
        ] {
            assert!(split_line_reference(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn command_arguments_find_existing_paths_and_preserve_their_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/ada")).unwrap();
        let report = dir.path().join("report.md");
        std::fs::write(&report, "report").unwrap();
        std::fs::write(dir.path().join("scratch/ada/measure.py"), "print(42)").unwrap();
        std::fs::write(dir.path().join("a file.py"), "pass").unwrap();
        let doc = Document::load(report, Default::default()).unwrap();
        for command in [
            "python3 -B scratch/ada/measure.py",
            "python3 -B ./scratch/ada/measure.py",
            "python3 -B 'a file.py'",
            "python3 -B \"a file.py\"",
        ] {
            let references = doc.file_references(command);
            assert_eq!(references.len(), 1, "{command}");
            let (range, url) = &references[0];
            let argument = &command[range.clone()];
            assert!(argument.ends_with(".py"));
            assert!(!argument.starts_with("python"));
            assert!(doc.destination(url.as_str()).unwrap().is_file());
        }
        assert!(doc
            .file_references("python3 -B missing.py --version")
            .is_empty());
    }

    #[test]
    fn text_and_source_files_preserve_content_and_binary_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for extension in ["txt", "json", "c", "cpp", "h", "py", "sh"] {
            let path = dir.path().join(format!("example.{extension}"));
            let source = "---\n# literal text, not a heading\n[also not a link](other.md)\n```\n";
            std::fs::write(&path, source).unwrap();
            let doc = Document::load(
                path.clone(),
                mdcat::RenderOptions {
                    frontmatter: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(doc.input, source);
            assert!(doc.language.is_some());
            assert_eq!(
                doc.destination(Url::from_file_path(&path).unwrap().as_str())
                    .unwrap(),
                path
            );
        }
        let path = dir.path().join("binary.dat");
        std::fs::write(&path, b"abc\0xyz").unwrap();
        assert!(Document::load(path.clone(), Default::default()).is_err());
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(Document::load(path, Default::default()).is_err());
    }

    #[test]
    fn code_paths_resolve_from_document_then_repository_and_ignore_non_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/ada")).unwrap();
        std::fs::create_dir_all(dir.path().join("scratch/grace")).unwrap();
        let path = dir.path().join("scratch/ada/report.md");
        std::fs::write(&path, "report").unwrap();
        let sibling = dir.path().join("scratch/grace/report.md");
        std::fs::write(&sibling, "grace").unwrap();
        let local = dir.path().join("scratch/ada/hello world.md");
        std::fs::write(&local, "hello").unwrap();
        let doc = Document::load(path, Default::default()).unwrap();
        for reference in [
            "scratch/grace/report.md#review",
            "../grace/report.md#review",
        ] {
            let url = doc.code_link(reference).unwrap();
            assert_eq!(doc.destination(url.as_str()).unwrap(), sibling);
            assert_eq!(url.fragment(), Some("review"));
        }
        assert_eq!(
            doc.destination(doc.code_link("hello world.md").unwrap().as_str())
                .unwrap(),
            local
        );
        for reference in [
            "missing.md",
            "scratch/ada",
            "let x = 1",
            "https://example.com/report.md",
        ] {
            assert!(doc.code_link(reference).is_none(), "{reference}");
        }
    }

    #[test]
    fn resolves_relative_and_encoded_paths_without_changing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.md");
        std::fs::write(&path, "[next](sub/hello%20world.md)").unwrap();
        let doc = Document::load(path, Default::default()).unwrap();
        assert_eq!(
            doc.destination("sub/hello%20world.md#heading").unwrap(),
            dir.path().join("sub/hello world.md")
        );
        let mut url = Url::from_file_path(dir.path().join("NEXT.MD")).unwrap();
        url.set_host(Some(&doc.environment.hostname)).unwrap();
        assert_eq!(
            doc.destination(url.as_str()).unwrap(),
            dir.path().join("NEXT.MD")
        );
        for target in [
            "https://example.com/test.md",
            "file://other-host/tmp/a.md",
            "index.md?x=1",
        ] {
            assert!(doc.destination(target).is_err(), "{target}");
        }
    }
}
