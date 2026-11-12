# vortex

A modal, async, grammar-aware code editor for the terminal.

Vim-style modes and muscle memory, a rope-backed buffer, tree-sitter syntax
highlighting, background search/format/spell/LSP work, and split panes that
resize themselves. Nothing blocks a keystroke: everything slow runs on the
`tokio` runtime and reports back through an event queue.

```
vortex src/main.rs +120
```

## Build and run

```sh
cargo build --release
./target/release/vortex README.md      # open a file
./target/release/vortex                # scratch buffer + welcome screen
```

Useful flags:

| Flag | What it does |
| --- | --- |
| `+<LINE>` | open at a line (`vortex src/lib.rs +42`) |
| `-c, --config <PATH>` | use a specific config file |
| `--default-config` | print the annotated reference config |
| `--write-config [PATH]` | write a starter config (`~/.config/vortex/config.toml` by default) |
| `--check-config` | validate the config, exit non-zero if it is broken |
| `--list-actions` | every bindable action name |
| `--no-mouse` | leave the mouse alone |

Requirements: a Rust toolchain (edition 2024) and a terminal. `ripgrep` and
`rustfmt`/`prettier`/… are used when they are on `$PATH`, and are never
required to build or run.

## Editing

Modes are the usual four: **normal**, **insert**, **visual** (character, line,
block) and **command**. Counts work (`3dw`, `5j`), so do operators, text
objects, the `"` registers, `m`/`` ` `` marks, `Ctrl-O`/`Ctrl-I` jump list, `u`
and `Ctrl-R`, and `.` repeat.

The terminal cursor is the mode indicator: a block in normal and visual, a bar
in insert and while typing on the command line, an underline while an operator
waits for its motion. A fresh session starts in normal mode and looks like it.

`{`/`}` move between paragraph boundaries the way vim does: `}` finds the *next*
empty line even when the cursor is already standing on one, runs to the end of
the buffer when there is no empty line left, and a count that cannot be
satisfied (`3}` near the end) leaves the cursor where it started.

Movement is built for code: `w`/`W` treat punctuation and `snake_case` as real
words, `$` sticks to the last character (so `dw` at end of line behaves), `%`
jumps between matching brackets, and text objects cover brackets, quotes,
words, sentences and paragraphs. When a tree-sitter grammar exists for the
file you also get grammar objects: `af`/`if` function, `ac`/`ic` class,
`ab`/`ib` block, `as`/`is` string, `aC`/`iC` comment.

The bindings you are most likely to reach for:

| Keys | Action |
| --- | --- |
| `i` `a` `o` `O` `v` `V` `Ctrl-V` `Esc` | enter modes |
| `Space p` or `Ctrl-P` | command palette |
| `Space f` | fuzzy file picker |
| `Space s` | grep the project's contents |
| `Space b` | buffer picker (`:bn`/`:bp` cycle, `:ls` lists) |
| `Space w` or `Ctrl-S` | save; `Space q` quits |
| `Space F` | run the language formatter |
| `Space d` | diagnostics at the cursor |
| `z=` or `Space e` | spelling suggestions |
| `Space x` | clear the search highlight |
| `Space v` / `Space h` | split right / split below |
| `Space o` `Space z` `Space c` | only this pane / zoom / close pane |
| `Space n` `Space =` | split along the roomier axis / equal panes |
| `Space g` `e`·`g` | layout mode: equal, golden |
| `Space t` `n`·`c`·`w`·`s`·`f`·`m` | toggles: numbers, cursorline, wrap, spell, auto-format, mouse |
| `Ctrl-W` `h/j/k/l`, `Space ←/→/↑/↓`, `Space Tab` | move between panes |
| `Space Ctrl-H/J/K/L`, `Space +`, `Space -` | resize the focused pane |
| `/` `?` `n` `N` `#` `*` | search, next/previous, search for the word under the cursor |
| `:`, `<Count>:` | command line |
| `Ctrl-C` | cancel a pending key sequence |

Insert mode keeps the readline habits that matter while typing code:
`Ctrl-W` (delete word back), `Ctrl-U` (delete to the line start), `Ctrl-T`
(indent line), `Shift-Tab` (unindent), `Ctrl-J` (newline) and `Ctrl-O` for a
single normal-mode command. Typing is syntax-aware: `Enter` after a line that
opens a block indents the new line (and splits `{|}` into three lines), `o` on
such a line opens the new line inside the block, a closing bracket typed alone
on its line dedents to the block it closes, and `(`/`[`/`{`/quotes insert their
partner with the caret between the two. The
mouse works too: click to place the cursor, drag to select, wheel
to scroll, and click inside a picker to choose an item.

## Commands

`:help` lists everything; abbreviations work as long as they are unambiguous.

```
:q[!]  :qa[!]  :w[!] [path]  :wa  :wq        quit and write
:e[!] [path]                                 reload this file (`:e!` discards edits)
:sp[lit] [path]  :vs[plit] [path]  :on[ly]   panes
:b[uffer] <name>  :bn  :bp  :ls  :bd[elete][!] <name>   buffers
:se[t] name=value  :se[t] name?  :se[t] noname          options, session-only
                   # `:set so=4`, `:set scroll-margin=4` and
                   # `:set editor.scroll-margin=4` all mean the same option
:map [<lhs>]  :unmap <lhs>                              key bindings
:f[ind] [pattern]  :gr[ep] <pattern>                    search the project
:fo[rmat]                                          run the language formatter
:conf[ig] path|source|write [path]                   configuration
:cd <dir>  :pwd  :hl[search] clear                   misc
```

`:help` itself opens the palette with the command list, and `:map` prints the
active bindings.

## Dark or light

At startup vortex asks the terminal for its background colour (OSC 11) and
classifies it, then picks the palette to match: dark terminals get the bright
half of the ANSI palette, light terminals the dark half, because cyan and
yellow disappear on white. Knowing the actual background also lets the quiet
parts of the UI be measured blends of your own colours instead of invented
ones — the cursor line, selection, line-number gutter and unfocused pane
borders. The focused pane's border and label get the accent colour.

The question is asked while vortex still owns the keyboard, and it is skipped
entirely if you are already typing, so it never eats a keystroke; a terminal
that does not answer costs 120 ms once and keeps the dark palette. If the guess
is wrong, pin it:

```toml
[theme]
background = "light"          # stop asking
accent = "#3c85b4"            # or a colour name, or "" for the scheme default
```

`:set theme.background=light` changes it for the session, without a restart.

## Configuration

`~/.config/vortex/config.toml` (or `$VORTEX_CONFIG`, or `-c`). Every key is
optional, keys are kebab-case, and unknown keys are reported on the message
line rather than refused:

```toml
[editor]
tab-width = 4
expand-tab = true
line-number = "relative"     # "absolute" | "relative" | "none"
wrap = false

[status]
height = 1
show-mode = true
show-diagnostics = true

[layout]
split-mode = "equal"          # "equal" | "golden"
                              # sizing only; the split direction is per command:
                              # `space h` / `space v` choose it, `space n` takes
                              # the roomier axis (side by side while the pane is
                              # physically wider than tall — cells are ~2:1 tall,
                              # so roughly 2x rows in columns — stacked otherwise)
                              # equal:  splits keep even shares
                              # golden: focused pane takes the golden ratio of
                              # the whole window (spread over the splits on
                              # its focus path), re-balanced on every focus
                              # change; splits away from the focus take the
                              # golden ratio too (first child larger), so
                              # every pane resizes, not just the focused
                              # sub-split
min-width = 24

[search]
ripgrep = "auto"             # "auto" | path | "off"
prune = ["target", "node_modules", ".git"]
smart-case = true

[format]
on-save = true                # format before writing
on-idle = "500ms"             # format once typing pauses (never mid-insert); "off" disables
# [format.command]
# rust = "rustfmt --edition 2024 --emit stdout"

[spell]
enable = true
only-comments = true
min-len = 4

[theme]
background = "auto"           # ask the terminal; or "dark" / "light"
accent = ""                   # focused pane border and label
# "colour", "bold", "italic", "underline", "dim", "on <colour>", "#rrggbb", "color13"
keyword = "magenta bold"
search-match = "black on yellow"

[keymap.normal]
space d = "show-diagnostics"
"<A-x>" = "delete-word-after"
```

Pickers and the palette are drawn on an opaque card, so the buffer behind them
cannot show through the text.

Panes scroll independently, and each one scrolls to its *own* height: split the
window in two and the cursor turns the pane at the pane's edge, not at the edge
of the screen.

`:set` changes apply to the running session and are never written to disk;
`:config write` is how you save them. `:config source` re-reads the file. The
annotated reference is one command away: `vortex --default-config`.

Binding a key that starts with a built-in one (`wd = "close-pane"`) does not
disble the built-in: `w` still moves after a short pause, or immediately when
the next key cannot continue the sequence — the way vim's `timeoutlen` works.

Project vocabulary for the spell checker goes in `.vortex-words` (one word per
line) at the working directory.

## What runs in the background

* **Parsing.** Tree-sitter re-parses a buffer ~120 ms after you stop typing;
  highlighting and node-based text objects pick the result up when it lands.
  Highlight groups come from the grammar rather than a regex guess: a token's
  role is read from the parent's field name, so `area` in `fn area()` is a
  function and `Point` in `struct Point` is a type.
* **Searching.** `space f` and `space s` stream results — ripgrep when it is
  available, otherwise a built-in walker that honours `.gitignore`. Results
  arrive in batches so the picker fills as they come. `:grep`'s pattern is a
  ripgrep regex, taken literally while it is still too half-typed to compile
  (`look(` searches for `look(`), and the list is always the hits for the
  pattern in the prompt rather than the union of everything typed so far.
* **Formatting.** External formatters (`rustfmt`, `prettier`, `gofmt`,
  `clang-format`, `taplo`, `ruff` …), triggered by `:format`, `space F`,
  `[format] on-save` or `on-idle`. Only the changed middle of the file is
  rewritten, so undo stays granular and the cursor keeps its place. An automatic
  run never lands while insert mode is open — the timer starts when you leave it
  — and a result describing text you have edited since it started is dropped
  instead of applied. Automatic formatting is not an undo step of its own either:
  `u` walks back your edits and takes the formatting that followed each one with
  it. `:format` is an edit you asked for, so it undoes on its own.
* **Spelling.** A self-contained English checker for comments and strings,
  with suggestions (`z=`, `space E`). Identifiers, `camelCase`, paths and
  numbers are skipped, so it stays quiet on code.
* **LSP.** Off by default — `[lsp] enable = true` turns it on, which also
  implies well-known servers (rust-analyzer, pylsp, gopls, clangd) when those
  binaries are installed, or name your own under `[lsp.servers.<language>]`.
  One server per project root, `initialize` handshake,
  `didOpen`/`didChange`/`didSave` sync, diagnostics in the gutter and status
  line, and insert-mode completion: typing pauses, the server is asked for
  candidates at the caret, and a popup opens next to it. Diagnostics arrive both
  pushed (the compiler's view of the saved file) and pulled
  (`textDocument/diagnostic`, the server's view of the edited buffer, refreshed
  when the server asks), so an unsaved error shows up without a save. `Tab`/`Enter`
  accept, the arrows move, `Esc` closes it, and typing narrows the list with a
  fuzzy filter. A slow answer for a caret that has since moved is dropped, and
  `[lsp] completion = false` turns the popup off while keeping diagnostics.
  Hover and rename are not wired up yet, which is why formatting is external.

## Architecture

```
src/
  main.rs         CLI, config loading, runtime setup
  app.rs          the only place that touches the terminal, files, processes
  editor.rs       editing state: modes, operators, registers, jumplist, panes
  buffer.rs       rope, undo history, file I/O, dirty/mtime tracking
  selection.rs    multi-span selections, mapping across edits
  input/          keys, keymap trie, motions, text objects, operators
  layout.rs       pane tree, equal/golden sizing
  ui.rs           pure renderer over a ratatui Buffer
  status.rs       configurable status line and message row
  prompt.rs       command line, search, fuzzy pickers
  syntax.rs       tree-sitter parsing and highlight spans
  indent.rs       syntax-aware indentation while typing
  completion.rs   the completion popup's candidates and prefix filtering
  search.rs       ripgrep driver plus ignore-crate fallback
  format.rs       formatter discovery, diffing, cursor restoration
  spell.rs        dictionary, tokenizer, suggestions
  lsp/            JSON-RPC framing, the client and completion requests
  config.rs       TOML config, defaults, `:set` plumbing
  theme.rs        style parsing and defaults
```

`Editor` never touches the terminal or the filesystem. It takes key events and
returns `Intent`s ("open a prompt", "parse this buffer", "write this file");
`App` carries them out. That split is what makes the editing core testable in
memory, and the renderer a pure function over a snapshot, so both can be tested
against an in-memory buffer instead of a tty.

Long work goes through one event queue: input, resize, paste, mouse and async
results are the same kind of thing, drained by a `tokio::select!` alongside a
30 ms tick that drives idle timers (reparse, spell, format, key-sequence
timeout).

## Development

```sh
cargo test --all-targets   # unit tests (buffer, motions, keymap, config, ui, …)
cargo clippy --all-targets
cargo fmt
cargo build --release
python3 scripts/smoke.py target/release/vortex       # modes, editing, search, panes, prompts
python3 scripts/smoke_code.py target/release/vortex  # highlighting, spell, format, reload
python3 scripts/smoke_theme.py target/release/vortex # colour detection: light, dark, silent
```

Rendering is tested against `ratatui::buffer::Buffer`, editing against
in-memory buffers. End-to-end behaviour is checked by driving the real binary
on a pty and reading the screen back (`scripts/ptyui.py` is a ~200-line
terminal for tests) — far more effective than mocking a tty. Every
`assert_eq!`-style failure prints the last screen, which is what you want when
the bug is "the file was empty" or "the leader key did nothing".

## Limitations worth knowing

* LSP covers diagnostics (pushed and pulled) and insert-mode completion; hover,
  rename and formatting are not wired up (formatting is external on purpose).
* The spell dictionary is embedded and modest. It favours false negatives over
  nagging; add words to `.vortex-words` when it matters.
* No tab page concept — buffers, not tabs.
* Block-wise visual operations are limited to insert/replace/delete on the
  block's columns.
