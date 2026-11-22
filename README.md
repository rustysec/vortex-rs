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
in insert and while typing in the `:` popup or a search, an underline while an
operator waits for its motion. A fresh session starts in normal mode and looks
like it.

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

These are the keys the shipped starter binds — a selection, not the whole
list: `vortex --write-config` writes out every `[keymap.*]` line you can cut
down or replace, and a key with no line does nothing:

| Keys | Action |
| --- | --- |
| `i` `a` `o` `O` `v` `V` `Ctrl-V` `Esc` | enter modes |
| `Space p` | command popup, the same list as `:` |
| `Space f` | fuzzy file picker |
| `Space s` | grep the project's contents |
| `Space b` | buffer picker (`:bn`/`:bp` cycle, `:ls` lists) |
| `Space w` or `Ctrl-S` | save; `Space q` quits |
| `Space F` | run the language formatter |
| `Space d` | diagnostics at the cursor; `]d` / `[d` jump to the next/previous one |
| `K` | hover over the symbol under the cursor (language server) |
| `Space a` | quick fixes for the caret's line (language server) |
| `gd` | jump to the definition of the symbol under the cursor |
| `Space r` | list every reference to it, picker like the grep results |
| `Space m` | rename the symbol everywhere — buffers and plain files alike |
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
| `:`, `<Count>:` | command popup |
| `Esc` | cancel a pending key sequence, leave modes, close popups |

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

Every line you have edited since the last write wears a small `+` in the gutter —
the cue that there is unsaved text over there, wherever your eye happens to be.
It rides along with the line as text moves above it, yields to a diagnostic sign
on the same line (a problem outranks a diff), and clears when you write; an undo
leaves the `+` in place, because walked-back text is still unsaved text.
`:set editor.changed-signs=false` turns the marks off (the column reserves no
width at all while nothing is marked).

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
:diag[nostic] [next|prev|list] [buffer|project]   jump between or list diagnostics
:fo[rmat]                                          run the language formatter
:markdown                                          toggle in-buffer markdown decoration (same as :set markdown)
:conf[ig] path|source|write [path]                   configuration
:cd <dir>  :pwd  :hl[search] clear                   misc
:pal[ette]  :help                                    list every command
```

`:palette` (also `Space p`) opens the same popup as `:` — one list, not two that
differ slightly. `:map` prints the active bindings.

## The `:` popup

Pressing `:` — a binding you write, since vortex binds nothing itself — opens a
card over the panes listing every command alphabetically, its arguments dimmed
after the name and its help plus one line per parameter in the preview below.
Typing narrows that list as you go, so a command you have never heard of is
reachable by guessing two letters.

What `Enter` runs, in the order the evidence speaks:

- A word you typed that *names* a command wins outright: `:q` quits even while
  the filter happens to be holding `:qall`, and `:wri` writes. Typing a command
  word is a deliberate act; where the highlight landed is not.
- A line with arguments runs exactly as written — `:write notes.txt`,
  `:set tab-width=4`, `:q!`. The list keeps narrowing by the command word while
  you type them, so it never empties out from under you.
- A row you walked to with the arrows (or clicked) is a choice, and runs even if
  what you typed names a different command — that is how two guessed letters
  reach something you have never heard of.
- `:` then `Enter` on an untouched list does nothing, as in vim.

`Ctrl-P`/`Ctrl-N` walk the commands you have run, and recalling one re-narrows
the list to it, so a popup that replaced the command line kept its history.

Choosing a command from the list steps into asking for its parameters one at a
time — `:config` asks for an action, then a path — with each step offering what
could possibly go there (paths off the file system as you type them, open
buffers, option names with their current values, or the fixed set from the
command table). A command typed by hand is only asked for a parameter it cannot
run without: `:grep` needs a pattern, so it asks; `:write` does not, so it saves.
An optional parameter says so in its title and is skipped by `Enter` on an empty
row; `Esc` backs out one step instead of throwing the whole line away.

## Markdown, rendered in place

Opening a `.md` file draws it as formatted text — headings bold and coloured,
emphasis italic, `**markers**` gone from sight while the words they wrapped stay in
their own columns, `-` bullets turned into `•`, quote bars drawn as `│`, rules into
unbroken lines, and a table given real borders out of the pipes it already has.

Nothing about the buffer changes: this is only how it is painted, so writing still
writes plain markdown, and `:markdown` (or `toggle-markdown` on a key) switches back
to the raw source. Because every decorated line keeps its character count
one-for-one — table rows aside, where drawn geometry resolves through an explicit
character map either way — the cursor lands where you expect, soft wrap still breaks
where it did, and clicking a character selects that character, decorated or not.

Two things follow from keeping that promise, and both are visible:

* a concealed marker leaves its own column empty rather than closing the gap, so
  `a **b** c` paints as if the asterisks were spaces. The words cannot be pulled
  together without pulling the cursor along with them;
* a rule is only as long as it was written — `---` draws three cells of `───`, not a
  full-width line, for the same reason.

Decoration needs no grammar; markdown has none wired up in `tree-sitter` and does not
need one to be drawn this way.  Tables are the one place where painting does move
text: a table's columns grow to fit their widest cell, so even a raggedly written
grid draws straight rules and aligned content (delimiter-row colons pick the
alignment).  The cursor comes along for the ride through a per-cell character map —
clicks, selection and caret all land on the character you can see, never a padded
column over it.  The file itself is still exactly what was typed: alignment lives in
the drawing, not the text.  And whichever line the cursor sits on reveals its markup
as written, so the characters being edited are always visible — move off the row and
the clean grid comes back.

A table too wide for the pane fits rather than clips: its columns shrink toward a
shared width and long cells word-wrap down their own column across extra screen
rows, keeping every rule in line and no word past the right edge.  The row under
the cursor stays what it always is — one plain line of source, revealed for editing
— so a caret never has to chase text through a wrap.

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
optional, keys are kebab-case, and unknown keys are reported on a card over the
panes rather than refused:

```toml
[editor]
tab-width = 4
expand-tab = true
line-number = "relative"     # "absolute" | "relative" | "none"
wrap = false
markdown-render = true       # draw markdown as formatted text, in place

[status]
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
case-sensitive = false        # check "Hello" as typed rather than as "hello"

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

The `:` popup, search lists and messages are all drawn on opaque cards, so the
buffer behind them cannot show through the text.

Panes scroll independently, and each one scrolls to its *own* height: split the
window in two and the cursor turns the pane at the pane's edge, not at the edge
of the screen.

With `[editor] wrap` off (the default), a line longer than the pane scrolls
sideways instead of losing the caret: moving right along a long line pulls the
column window along with it — keeping `editor.sidescroll-margin` columns of
context ahead — and `z h` / `z l` nudge that window left or right without moving
the cursor. Turning wrap on draws everything by wrapping, so the column window
stays parked at the first column until you turn it back off.

`:set` changes apply to the running session and are never written to disk;
`:config write` is how you save them. `:config source` re-reads the file. The
annotated reference is one command away: `vortex --default-config`.

Binding a key that starts with another one you bound (`wd = "close-pane"` next to
`w = "word-forward"`) does not disable the shorter: `w` still moves after a short
pause, or immediately when the next key cannot continue the sequence — the way
vim's `timeoutlen` works.

Nothing is bound until `[keymap.*]` says so.  Vortex ships no built-ins it would
apply behind your back; what you get from `vortex --write-config` is a vim-style
starter written out as ordinary lines, and the only keys that work with an empty
configuration are insert-mode text input — printable characters, backspace, enter
and the arrows — because those write text rather than run commands.

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
* **Formatting.** When a language server is attached for the buffer, its own
  formatter is asked first — it knows the language's real formatting rules and
  can format a range. The external formatters (`rustfmt`, `prettier`, `gofmt`,
  `clang-format`, `taplo`, `ruff` …) are the fallback for languages with no
  server, and for a server that refuses. Triggered by `:format`, `space F`,
  `[format] on-save` or `on-idle`. Only the changed middle of the file is
  rewritten, so undo stays granular and the cursor keeps its place. An automatic
  run never lands while a gesture is open in its buffer — typing, an armed `d`
  waiting for its motion, a visual selection, an open prompt; the timer starts
  when the gesture ends — and a result describing text you have edited since it
  started is dropped instead of applied. `on-save` runs the formatter before the
  bytes go out: the write waits for the result, but it always happens, so a
  formatter that fails or never answers cannot lose a save. Automatic formatting
  is not an undo step of its own either:
  `u` walks back your edits and takes the formatting that followed each one with
  it. `:format` is an edit you asked for, so it undoes on its own.
* **Spelling.** A self-contained English checker for comments and string
  values, with suggestions (`z=`, `Space e`). Identifiers, mapping keys in
  JSON/YAML/TOML, `camelCase`, paths and numbers are skipped, so it stays quiet
  on code.
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
  The server also answers hover (`K`), quick fixes (`Space a`), go-to-definition
  (`gd`), find-references (`Space r`) and rename across the workspace
  (`Space m`).

## Architecture

```
src/
  main.rs         CLI, config loading, runtime setup
  app.rs          the only place that touches the terminal, files, processes
  editor.rs       editing state: modes, operators, registers, jumplist, panes
  buffer.rs       rope, undo history, file I/O, dirty/mtime tracking
  selection.rs    multi-span selections, mapping across edits
  input/          modal key handling: key notation, keymap trie, motions,
                  operators, text objects
  command.rs      the `:` command line: parsing, the command table, completion, `:set`
  action.rs       the action vocabulary: one name per binding and per command
  layout.rs       pane tree, equal/golden sizing
  ui.rs           pure renderer over a ratatui Buffer
  status.rs       the one-row status area (transient text is a card in ui.rs)
  prompt.rs       command line, search, fuzzy pickers
  completion.rs   the completion popup's candidates and prefix filtering
  syntax.rs       tree-sitter parsing and highlight spans
  indent.rs       syntax-aware indentation while typing
  markdown.rs     in-buffer markdown decoration
  search.rs       ripgrep driver plus ignore-crate fallback
  format.rs       formatter discovery, diffing, cursor restoration
  spell.rs        dictionary, tokenizer, suggestions
  fuzzy.rs        path-aware fuzzy matcher for the pickers
  git.rs          git branch probe for the status row
  lang.rs         language registry: detection, tree-sitter grammars, comment tokens
  diagnostic.rs   diagnostics: errors, warnings and hints, decorating text
  lsp/            JSON-RPC framing, the client and its requests
  config.rs       TOML config, defaults, `:set` plumbing
  theme.rs        style parsing and defaults
  term.rs         terminal setup and teardown
  event.rs        terminal input plus background-job results, one channel
  error.rs        error and result types
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
python3 scripts/smoke_markdown.py target/release/vortex # in-buffer markdown decoration
```

Rendering is tested against `ratatui::buffer::Buffer`, editing against
in-memory buffers. End-to-end behaviour is checked by driving the real binary
on a pty and reading the screen back (`scripts/ptyui.py` is a ~200-line
terminal for tests) — far more effective than mocking a tty. Every
`assert_eq!`-style failure prints the last screen, which is what you want when
the bug is "the file was empty" or "the leader key did nothing".

## Limitations worth knowing

* LSP covers diagnostics (pushed and pulled), insert-mode completion, hover,
  quick fixes, go-to-definition, find-references and rename across the
  workspace.  Formatting prefers the language formatter (the server is used
  when it has one).
* The spell dictionary is embedded and modest. It favours false negatives over
  nagging; add words to `.vortex-words` when it matters.
* No tab page concept — buffers, not tabs.
* Block-wise visual operations are limited to insert/replace/delete on the
  block's columns.
