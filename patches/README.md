# Local mdcat patches

`0001-config-defaults-paginate.patch` adds `[defaults] paginate = "auto"` (also accepts `true` or `false`).
Explicit `--paginate` / `--no-pager` wins; without the setting, command defaults remain unchanged.

Base: BIRSAx2/mdcat 2.18.0, commit e3c81340a15a486344e0e07072aed68044e516e1.
For 2.19.0, use `0001-config-defaults-paginate-2.19.patch` and
`0004-document-frame-2.19.patch` instead of their unversioned variants.
Application was checked against the release sources. Apply only one variant of each.
`0002-default-pager-mouse.patch` enables mouse-wheel scrolling in the default
`less -r --mouse` pager. Apply it after either version of patch 0001.
Custom `$MDCAT_PAGER` / `$PAGER` commands still take precedence.

`0003-h1-rule.patch` adds `[theme] h1_rule = true` for main headings such as
`── Shader Works ──────────`, filling the configured width without a background.
Apply it after patches 0001 and 0002.

`0004-document-frame.patch` makes `margin = true` frame each document with rounded
borders, using its first top-level H1 as the top-border title. Four columns are
reserved for borders and padding. Additional H1s stay in the body; long titles
continue on rows inside the frame. Framed output uses text instead of inline images.

`0005-pager-scrollback.patch` retains rendered pager input and prints it after the
pager exits, so the complete document remains in terminal scrollback, including
after an early quit. This retains the rendered document in memory until replay.

Apply these after patch 0003, in order.

`0006-less-position-history.patch` resumes interactive single-file `less` views.
It hashes the canonical absolute source filename with SHA-256 and saves the top
rendered line in `history.toml` beside `config.toml` on `q` or Ctrl+C. History is
merged under `history.lock` and replaced atomically. On Unix, rendered output is
piped to `less -f /dev/stdin`, allowing native position marks without writing the
document to disk. A tiny temporary less-history file is cleaned up after exit;
platforms without `/dev/stdin` use a temporary rendered-file fallback. Other pagers, stdin, unpaginated
views, and combined multi-file views keep their previous behavior.

Apply this after patch 0005. Delete history entries to reset a saved position.

The patches are already applied to this working tree. Keep this directory when updating.

On a clean future checkout, from the repository root:

```sh
git apply --check patches/0001-config-defaults-paginate.patch
git apply patches/0001-config-defaults-paginate.patch
git apply --check patches/0002-default-pager-mouse.patch
git apply patches/0002-default-pager-mouse.patch
git apply --check patches/0003-h1-rule.patch
git apply patches/0003-h1-rule.patch
git apply --check patches/0004-document-frame.patch
git apply patches/0004-document-frame.patch
git apply --check patches/0005-pager-scrollback.patch
git apply patches/0005-pager-scrollback.patch
git apply --check patches/0006-less-position-history.patch
git apply patches/0006-less-position-history.patch
cargo test -p mdcat --lib
cargo test -p pulldown-cmark-mdcat --lib
cargo check -p mdcat --bin mdcat
cargo install --path . --locked
```

On this machine, prefix Cargo commands with `OPENSSL_DIR=/Users/julia/openssl`
if OpenSSL discovery fails.

If the check fails, review upstream changes and port the patch manually before rebuilding.
If upstream adds equivalent support, drop this patch instead.

After installing, add to `~/.config/mdcat/config.toml`:

```toml
[defaults]
paginate = "auto"
```

Merge the key into an existing `[defaults]` table rather than adding a duplicate table.

Automatic mode counts raw input lines, then pages above 2.5 terminal heights.
Multiple files count together; stdin is excluded from the automatic line count.
Redirected output and watch mode do not automatically paginate.
