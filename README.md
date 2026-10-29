# mdcat

Fancy `cat` for Markdown (that is, [CommonMark][]):

```
$ mdcat sample.md
```

Use `mdcat --browse sample.md` (also available as `--analyzer`) for the built-in Ratatui/Crossterm viewer.
Scroll with the arrow keys or j/k, use Page Up/Down and Home/End to navigate,
and press q to close the current document. Mouse-wheel scrolling and terminal resizing are supported.
Drag with the left mouse button to select text; hold Ctrl when starting the drag
to select a rectangular region. Releasing the button copies the selection to the
clipboard (macOS uses the system clipboard; other platforms use terminal OSC 52 support).
Escape clears the selection. Links and foldable rows activate on a click, without dragging.
Hold the pointer beyond the top or bottom edge while selecting to keep scrolling.
Each 30 ms step scrolls 1 line for up to 99 selected rows, 2 lines for 100–512
selected rows, and 4 lines for more than 512 selected rows.
Images appear as text fallbacks in this mode.

Press Ctrl+B on a tracked text file to switch the current window to Git blame. Each source
line shows its commit hash, author, date, and line number; uncommitted lines are
marked as working-tree changes. Author, date, and line number colors fade from
warm tones for recent changes through green and blue to muted tones for old changes;
working-tree changes use pink. Click a commit hash to open its diff, or use Tab
to choose a commit from the link picker. Age color bands use <1, <2, <3, <4, <7,
<10, <14, <30 days, then older changes. Hover over a hash to highlight the visible
source lines attributed to that commit. Ctrl+B, q, or Escape returns to the file.
Blame uses the original source, including local edits, even when viewing formatted
code or rendered Markdown. Narrow windows hide author/date columns to leave room
for source text. Search and mouse selection work in the blame view.

Ctrl+H switches the current window to file history, following renames. The commit
list appears on the left, with the existing side-by-side diff preview on the right.
Up/Down or a click selects a commit; a working-tree entry includes local changes.
The list shows commit messages only. Space in the list or a right-click pins a
commit as the left-side reference, highlighted in the diff's before color;
the selected commit uses the after color. Selecting another commit compares it
against the reference. Pin the same
entry again to restore comparisons against each commit's parent (HEAD for local changes).
Tab or Left/Right changes focus; arrows and the mouse wheel scroll the focused pane,
and Page Up/Down scroll the preview. Ctrl+F filters commits by hash, author, or subject.
Enter opens the current comparison at full width in a nested diff window; q returns.
On narrow terminals the list fills the window, with Enter opening the diff.
Ctrl+H, q, or Escape returns to the original file view and scroll position.
Ctrl+B and Ctrl+H can switch directly between blame and history in the same window.

In a Git repository, `mdcat --browse` (equivalent to `mdcat --browse .`) shows a directory list on the left and a
side-by-side diff preview on the right. Select with Up/Down or a click; Enter opens
the selected file or directory normally. `f` toggles all files versus changed files;
the changed view retains the directory structure and includes deleted files.
Directories with changes open in changed-paths mode automatically; clean directories
open with all files. Background polling refreshes every two seconds; `r` refreshes
immediately. Refreshing preserves your current filter, selected file, and preview
scroll position when the selected file remains available.
Polling and redraws pause while the terminal reports the tab as unfocused, and
polling resumes immediately on focus. Unchanged polls do not redraw the view.
Files that newly appear or change during polling get a yellow dot for one minute
after the marker is first drawn.
The preview always compares HEAD → working tree, showing staged and unstaged
changes together. Untracked files preview as additions;
clean files preview their contents. Enter on a deleted file opens its diff.
The diff's filename, comparison label, and change summary stay pinned while scrolling.
Tab or Left/Right changes focus; arrows select files or scroll the focused preview.
The mouse wheel scrolls the pane under the pointer without changing file selection.
Page Up/Down scroll the preview. Each pane has a scrollbar that supports
clicking and dragging when it overflows. `d` opens the current diff at full width;
`r` refreshes status. Returning from a file also refreshes the directory view.
Narrow windows show the file list alone; Enter opens files and `d` opens diffs.
Directories outside Git repositories retain the ordinary directory browser.

Click a local file link to open it in a fixed window above the current document.
Internal section links jump within the current document.
Press Tab to open a popup listing all local file destinations in the document.
The popup uses at most 60% of the screen width and height.
Select with arrow keys, j/k, or Tab/Shift+Tab, then press Enter; clicking an entry also
opens it. Directory prefixes shared by at least three links appear once as bold
tree headers; only file rows are selectable. Page Up/Down, Home/End, and the mouse
wheel navigate longer lists; a scrollbar appears only when the list overflows.
An empty list shows a compact notice that waits 250 ms and fades out over 500 ms; Escape/q
dismisses it immediately.
Keyboard selection scrolls the document to the link;
repeated links use neighboring entries to choose the occurrence. The selected link
is underlined in the document. Heading fragments are hidden in displayed Markdown
paths while remaining part of the destination.
Escape or q dismisses the picker without closing the document. Each nested window leaves
its parent's title bar visible, with inactive borders and captions dimmed. Duplicate
filenames include enough directory context to distinguish the open documents.
Git commit hashes (7 or more hex characters) become clickable when they resolve
unambiguously to a commit in the document's repository. Clicking one opens
a side-by-side diff in a nested window. Its title shows the short commit hash,
author name, and local date/time, including the year only when it differs from the current year.
A blank line precedes the commit message. Old/new line numbers and red/green changes make replacements easy to compare. Click a file header
to expand or collapse it; files start collapsed, with dividers between sections.
Each file header shows its change count and a green/red additions/deletions bar.
`n`/`p` selects files, Enter/Space toggles the selected file, and
`c`/`e` collapses/expands all files. Git `.diff` and `.patch` files use this view too.
Long lines wrap within each pane; very narrow terminals use a stacked diff. Commit links
also appear in the Tab picker; Escape/q returns to the document.
Hover over a local file link to underline it and show its full path relative to the
working directory in the footer. Escape or q closes only the deepest document window;
at the original document, it exits the viewer. Backspace or Left also returns to the
parent document and its previous scroll position. Relative paths resolve against the current
document; heading fragments and table-of-contents links are supported. Press Ctrl+F to search
the rendered document and highlight matches as you type. Press Enter to finish, Left/Right or
`n`/`N` to move between matches, and Escape to clear the search. Existing file paths written as inline
code are navigable too, resolving first against
the document directory and then the nearest Git repository root. Existing path
arguments in command examples (including fenced code blocks) are clickable and
appear in the link picker too. Opening one displays the file in a nested viewer.
Press Ctrl+N to open the current file in nano; shell aliases are honored, and the viewer reloads
the file when nano exits.
Press Ctrl+O in a C/C++, GLSL, HLSL, or Metal file to toggle a formatted view:
four-space indentation, expanded statements, and same-line opening braces.
Formatting uses `clang-format` and leaves the file on disk unchanged. Press Ctrl+O
again to restore the original view. If clang-format is outside your PATH, set
`clang_format = "/path/to/clang-format"` at the top level of mdcat's config file.
Directory links open a nested listing with links to subdirectories and files. Use Up/Down to
select an entry and Enter to open it. In a Git repository, changed and untracked entries are
shown in yellow; directories are yellow when they contain changes.
Raster image files can be opened directly or from links: browse mode fits them in the window,
using terminal graphics when available and colored character cells as a fallback. In normal mode,
image files use the terminal's configured image protocol and fit within half of its width and height.
Attach `:172` (or `#L172`) to a path to jump to source line 172; this works with
clicks and the link picker. Line links to Markdown files show their literal source.
The initial browse argument also accepts `:line`, for example
`mdcat --browse src/metal_compile_options.mm:22`.
Line numbers beyond the file's end jump to its last line.
Missing
files and unsupported destinations show an error in the viewer. Local UTF-8 text files open here, including JSON, C/C++, headers, Python, GLSL, and Metal.
Source files use syntax highlighting (C++ rules for Metal); plain text retains its literal content.
Web links are not launched.
Use an ordinary click, without Command, so Ghostty passes it to mdcat.
The normal less pager keeps its existing behavior.

JSON files use a structured view with distinct colors for keys, strings, numbers,
and literals. Collapsed objects preview an identifying or first string-valued property, while collapsed
arrays show their element count inside brackets. Open arrays show a count when they have at least
ten elements. If the expanded tree exceeds
the window height on opening, deeper containers and large first-level containers start collapsed.
Select rows with Up/Down
or j/k; Enter/Space or a click folds a container, Left collapses or selects its parent,
and Right expands. `c` folds nested containers; `e` expands all. Page Up/Down and
Home/End navigate the tree. File links still open in nested windows, and Tab lists
links even inside folded sections. Backspace returns to the parent document.
Field order, duplicate keys, and original value tokens are preserved. Invalid JSON
falls back to syntax-highlighted text. Source-line links open the original source.
Text (`.txt`) and log (`.log`) files also render complete JSON objects and arrays
that occupy their own lines, including multiline fragments. Surrounding log text
stays literal, and each fragment supports the same folding controls. Malformed
or truncated fragments remain literal text.

Press `?` to open a prompt popup and ask about the document. By default this uses Apple's on-device Foundation Models.
With Apple models, opening the prompt starts background prewarming of both the title and answer
sessions, including their instructions and shared prompt prefixes. Submission reuses those sessions.
Type a question and press Enter. A small floating window appears immediately with a spinner.
When the model generates a title, it types into the caption character by character;
the window expands from 5 to 64 columns over 100 ms once the title is ready.
If a rendered answer line needs more room, it then expands to 84 columns over 80 ms.
Both widths are capped by the terminal size; height grows smoothly with the answer.
Answers are rendered as Markdown with mdcat's styling and syntax highlighting,
and revealed character by character while preserving that formatting.
Drag its title bar or use arrow keys (h/j/k/l) to move it; Page Up/Down or the mouse wheel
scroll the answer. Escape or Tab closes the popup/window, and `?` asks a follow-up.
Ctrl+L in the response window clears the conversation when idle.
The Apple provider requires Apple Silicon, macOS 26+, and Apple Intelligence with its model downloaded;
building on macOS requires Xcode 26+. Select a query model with `-m`/`--model`: `apple`, `luna`
(`gpt-6-luna`), `terra` (`gpt-6.1-terra`), or `sol` (`gpt-6.1-sol`). The OpenAI models use low
reasoning effort and require `OPENAI_API_KEY`. For example, use `mdcat --browse -m luna file.md`.
Without an explicit model, Apple is the default; `MDCAT_QUERY_PROVIDER=openai` selects Luna by
default. An explicit model takes precedence. OpenAI requests send the complete line-numbered
document, question, and recent conversation to OpenAI.
Opening the prompt also sends the document and instructions in a background OpenAI cache-prewarm
request; this is billed as a cache write. OpenAI retains the cache remotely, and a failed prewarm
falls back to a normal answer request.
The Apple provider uses selected line-numbered excerpts, so relevant text can be omitted.
Its document passage budget is 18,000 characters; both providers use up to 1,500 characters
of recent conversation and 500 characters for the question. These are character budgets rather
than token counts; very large documents can exceed the OpenAI model's context limit.

![mdcat showcase with different colour themes][sxs]

mdcat in [Ghostty], with `--theme catppuccin-mocha`, `--theme dracula`,
and `--theme nord` (left to right), using [JetBrains Mono].

[CommonMark]: http://commonmark.org
[JetBrains Mono]: https://www.jetbrains.com/lp/mono/
[sxs]: ./screenshots/side-by-side.png

## Features

`mdcat` works best with [iTerm2], [WezTerm], [kitty], and [Ghostty], and a good terminal font with italic characters.
Then it

- nicely renders all basic CommonMark syntax, plus definition lists,
- renders footnotes and inline markup in table cells,
- highlights code blocks with syntax definitions from [bat] (TOML, TypeScript, Dockerfile, Zig, Nix, and more),
  using the colour theme of your choice — including any theme [bat] supports via `$BAT_THEME`,
- renders inline and display math, as PNGs through the iTerm2 or kitty image protocol, and as Unicode substitutions otherwise,
- renders [Mermaid][mermaid] diagrams (flowcharts, sequence diagrams, and more) in fenced ` ```mermaid ` code blocks,
  as PNGs through an image protocol, and as Unicode diagrams otherwise,
- renders [GFM alerts][gfm-alerts] (`[!NOTE]`, `[!TIP]`, `[!WARNING]`, etc.) with coloured borders and icons,
- shows [links][osc8], and also images inline in supported terminals (see above, where "Rust" is a clickable link!),
- adds jump marks for headings in [iTerm2] (jump forwards and backwards with <key>⇧⌘↓</key> and <key>⇧⌘↑</key>),
- ships nine built-in colour themes (`catppuccin-mocha`, `catppuccin-latte`, `gruvbox-dark`, `gruvbox-browse`, `gruvbox-light`,
  `dracula`, `nord`, `solarized-dark`, `solarized-light`) plus auto dark/light detection,
  (`gruvbox-browse` separates periwinkle-blue links from yellow inline code),
  and lets you fully customise colours, heading markers, and GFM alert icons/labels via
  `~/.config/mdcat/config.toml` (see [config.toml.example](./config.toml.example)),
- can render typographic punctuation (curly quotes, en/em dashes, an ellipsis) with `--smart-punctuation`,
- can watch a file and re-render it on every save with `--watch`, for a live preview while editing,
- can fuzzy-find a Markdown file to render, via `mdpick` (see below), if you have [fzf] installed.

| Terminal                               | Basic syntax | Syntax highlighting | Images | Math  | Jump marks |
| :------------------------------------- | :----------: | :-----------------: | :----: | :---: | :--------: |
| Basic ANSI¹                            |      ✓       |          ✓          |        |  ✓³   |            |
| Windows console (cmd.exe, PowerShell)⁶ |      ✓       |          ✓          |        |  ✓³   |            |
| Windows Terminal⁶                      |      ✓       |          ✓          |        |  ✓³   |            |
| [iTerm2]                               |      ✓       |          ✓          |   ✓²   |  ✓³   |     ✓      |
| [kitty]                                |      ✓       |          ✓          |   ✓²   |  ✓³   |            |
| [WezTerm]                              |      ✓       |          ✓          |   ✓²   |  ✓³   |            |
| [VSCode]                               |      ✓       |          ✓          |        |  ✓³   |            |
| [Ghostty]                              |      ✓       |          ✓          |   ✓²   |  ✓³   |            |
| [foot]⁴                                |      ✓       |          ✓          |   ✓²   | ✓³‧⁵  |            |
| [xterm]⁴                               |      ✓       |          ✓          |   ✓²   | ✓³‧⁵  |            |

1. mdcat requires that the terminal supports strikethrough formatting and [inline links][osc8].
   This includes most modern terminal emulators, such as KDE Konsole or anything based on VTE, GNOME's terminal emulation library.
   But mdcat likely won't work well on old terminals that lack these features (e.g. the Linux text console).
2. SVG images are rendered with [resvg], see [SVG support].
3. On terminals with the iTerm2, kitty, or Sixel image protocol, math is rendered as PNG images.
   Otherwise mdcat uses Unicode substitutions.
4. Uses Sixel for images, which is also supported by many other terminals.
   The capability detection logic would need to be extended to also provide the feature on other terminals, though.
5. Inline math with Sixel breaks the layout, as it causes vertical scrolling that is not taking into account.
6. mdcat doesn't detect Windows Terminal or the legacy console host specifically (there's no check for
   `$WT_SESSION`, for instance), so both fall back to the Basic ANSI tier: no image protocol and no jump marks,
   regardless of which shell (cmd.exe, PowerShell, or PowerShell 7) runs inside them.

Not supported:

- Text wrapping inside table cells.

[bat]: https://github.com/sharkdp/bat
[gfm-alerts]: https://docs.github.com/en/get-started/writing-on-github/getting-started-with-writing-and-formatting-on-github/basic-writing-and-formatting-syntax#alerts
[osc8]: https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
[iterm2]: https://www.iterm2.com
[WezTerm]: https://wezfurlong.org/wezterm/
[kitty]: https://sw.kovidgoyal.net/kitty/
[resvg]: https://github.com/RazrFalcon/resvg
[SVG support]: https://github.com/RazrFalcon/resvg#svg-support
[VSCode]: https://code.visualstudio.com/
[Ghostty]: https://mitchellh.com/ghostty
[foot]: https://codeberg.org/dnkl/foot
[xterm]: https://invisible-island.net/xterm/xterm.html
[fzf]: https://github.com/junegunn/fzf
[mermaid]: https://mermaid.js.org

## Usage

Try `mdcat --help` or read the [mdcat(1)](./mdcat.1.adoc) manpage.

To pick a colour theme:

```console
$ mdcat --theme catppuccin-mocha sample.md
$ MDCAT_THEME=dracula mdcat sample.md
$ mdcat --list-themes  # preview every built-in theme
```

To customise colours, heading markers, and GFM alert icons/labels, or set defaults for flags like
`--margin`, `--smart-punctuation`, or `--columns`, create `~/.config/mdcat/config.toml`.
See [config.toml.example](./config.toml.example) for a fully annotated example, or the
[mdcat(1)](./mdcat.1.adoc) manpage's "Configuration file" section for the reference.

To use a [bat] syntax-highlighting theme for code blocks:

```console
$ BAT_THEME="Catppuccin Mocha" mdcat sample.md
```

To get a live preview while editing:

```console
$ mdcat --watch sample.md
```

See [sample/math.md](./sample/math.md) for math examples, [sample/mermaid.md](./sample/mermaid.md) for Mermaid
diagrams, [sample/alerts.md](./sample/alerts.md) for GFM alerts, and [sample/tables.md](./sample/tables.md) for
table rendering.

To fuzzy-find and open a Markdown file below the current directory (requires [fzf]):

```console
$ mdpick
$ mdpick docs  # search below docs instead
```

### `mdless` and `mdpick`

`mdcat` looks at the name it was invoked as (`argv[0]`) to decide how to behave:

- Invoked as `mdless`, it automatically paginates output, as if `--paginate` were given.
- Invoked as `mdpick`, it skips straight to the interactive picker described above, instead of expecting a file argument.

Both are the same binary as `mdcat`, just under a different name, so you get them for free by linking or copying the binary:

```console
$ ln -s "$(command -v mdcat)" ~/.local/bin/mdless
$ ln -s "$(command -v mdcat)" ~/.local/bin/mdpick
```

(make sure `~/.local/bin`, or wherever you put the links, is on `$PATH`). A hardlink or plain copy works just as well as a symlink.

## Installation

- [Release binaries](https://github.com/BIRSAx2/mdcat/releases/) built on Github Actions.
  - These binaries are built from Git source on Github Actions; you find provenance attestations at <https://github.com/BIRSAx2/mdcat/attestations>.
- Package managers — `mdcat` is available from several package managers:

  | Package manager | Command |
  | --- | --- |
  | [Homebrew](https://formulae.brew.sh/formula/mdcat) (macOS/Linux) | `brew install mdcat` |
  | [AUR](https://aur.archlinux.org/packages/mdcat-bin) (Arch Linux) | `paru -S mdcat-bin` |
  | [Nixpkgs](https://search.nixos.org/packages?query=mdcat) | `nix-env -iA nixpkgs.mdcat` |
  | [MacPorts](https://ports.macports.org/port/mdcat/) | `sudo port install mdcat` |
  | [FreeBSD Ports](https://www.freshports.org/sysutils/mdcat/) | `pkg install mdcat` |
  | [Void Linux](https://voidlinux.org/packages/?q=mdcat) | `sudo xbps-install mdcat` |
  | [MSYS2](https://packages.msys2.org/base/mingw-w64-mdcat) (Windows) | `pacman -S mingw-w64-x86_64-mdcat` |

  See [Repology](https://repology.org/project/mdcat/versions) for the full list of packages and their versions.
- You can also build `mdcat` manually with `cargo install mdcat` (see below for details).

`mdcat` can be linked or copied to `mdless` or `mdpick` (see [`mdless` and `mdpick`](#mdless-and-mdpick) above).

## Building

Run `cargo build --release`.

Building requires `libcurl`.

## Packaging

When packaging `mdcat` you may wish to include the following additional artifacts:

- A symlink or hardlink from `mdless` to `mdcat` (see above), and, if you want to ship the picker, likewise from `mdpick` to `mdcat`. `mdpick` additionally needs [fzf] installed to work; consider depending on it in your package.
- Shell completions for relevant shells, by invoking `mdcat --completions` after building, e.g.

  ```console
  $ mdcat --completions fish > /usr/share/fish/vendor_completions.d/mdcat.fish
  $ mdcat --completions bash > /usr/share/bash-completion/completions/mdcat
  $ mdcat --completions zsh > /usr/share/zsh/site-functions/_mdcat
  # Same for mdless and mdpick if you include them
  $ mdless --completions fish > /usr/share/fish/vendor_completions.d/mdless.fish
  $ mdless --completions bash > /usr/share/bash-completion/completions/mdless
  $ mdless --completions zsh > /usr/share/zsh/site-functions/_mdless
  $ mdpick --completions fish > /usr/share/fish/vendor_completions.d/mdpick.fish
  $ mdpick --completions bash > /usr/share/bash-completion/completions/mdpick
  $ mdpick --completions zsh > /usr/share/zsh/site-functions/_mdpick
  ```

- A build of the man page `mdcat.1.adoc`, using [AsciiDoctor]:

  ```console
  $ asciidoctor -b manpage -a reproducible -o /usr/share/man/man1/mdcat.1 mdcat.1.adoc
  $ gzip /usr/share/man/man1/mdcat.1
  # If you include mdless and/or mdpick as above, you may also want to support their man pages
  $ ln -s mdcat.1.gz /usr/share/man/man1/mdless.1.gz
  $ ln -s mdcat.1.gz /usr/share/man/man1/mdpick.1.gz
  ```

[AsciiDoctor]: https://asciidoctor.org/

## Troubleshooting

`mdcat` can output extensive tracing information when asked to.
Run `mdcat` with `$MDCAT_LOG=trace` for complete tracing information, or with `$MDCAT_LOG=mdcat::render=trace` to trace only rendering.

## License

Copyright Sebastian Wiesner <sebastian@swsnr.de> and Mouhieddine Sabir <me@mouhieddine.dev>

Currently maintained by [BIRSAx2](https://github.com/BIRSAx2).

Binaries are subject to the terms of the Mozilla Public
License, v. 2.0, see [LICENSE](LICENSE).

Most of the source is subject to the terms of the Mozilla Public
License, v. 2.0, see [LICENSE](LICENSE), unless otherwise noted;
some files are subject to the terms of the Apache 2.0 license,
see <http://www.apache.org/licenses/LICENSE-2.0>
