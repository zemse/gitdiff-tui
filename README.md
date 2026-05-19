# gitdiff

A Rust + ratatui TUI for reviewing local git changes like a GitHub PR — without
pushing or opening a PR. Comments are written to a `REVIEW.md` at the repo root,
in a format a coding agent (or another human) can act on directly.

![gitdiff TUI](https://raw.githubusercontent.com/zemse/gitdiff-tui/main/screenshot.png)

## Features

- Auto-detects what to diff — working changes if dirty, else current branch vs.
  upstream (`@{upstream}` → `origin/main`/`master` → local `main`/`master`).
- Unified diff view with syntax highlighting (via `syntect`) and intra-line
  word-level emphasis.
- File tree sidebar (`e`), fuzzy file picker (`t`), per-file "viewed"
  checkmark (`v`) that persists across runs.
- Per-file and per-hunk collapse/expand, with `expand 20 above`/`below` buttons
  to load more file context lazily.
- Inline comments with a yellow-bordered composer: click a line (or press `c`)
  to write markdown, click an existing comment to edit, `x` to delete.
- Range comments via mouse drag or `V` (visual select), reactions (`K`),
  resolve / unresolve (`r`).
- Drafts pane (`R`) listing every open comment; `S` submits all drafts to
  `REVIEW.md`. Drafts auto-persist to `.gitdiff/drafts.json`.
- Review verdict (`V` cycles: comment / approve / request changes) shown in
  the header bar.
- Whitespace toggle (`w`), context-line +/- (`=` / `-`), tab width (`,` / `.`).
- Yank current file path (`y`).
- Mouse-driven everywhere: click to move, click headers to collapse, wheel
  to scroll.
- `ctrl-c` exits from anywhere, including the composer.

## Install

```sh
cargo install gitdiff
```

Or from source:

```sh
git clone https://github.com/zemse/gitdiff-tui
cd gitdiff-tui
cargo install --path .
```

## Use

From any git repo:

```sh
gitdiff                  # auto-detects: working changes, else branch vs upstream
gitdiff base..head       # explicit range
```

## Keybindings

Press `?` inside the app for the same list.

```
Keybindings

  j / ↓        move down one line
  k / ↑        move up one line
  ctrl-d/u     half page down/up
  g / G        top / bottom
  ]   /   [    next / prev file
  }   /   {    next / prev hunk

  space        collapse / expand current hunk (on @@) or file (elsewhere)
  z / Z        collapse all / expand all files
  v            toggle viewed (auto-collapses)
  y            yank current file path to clipboard
  e            toggle file tree sidebar
  R            toggle drafts pane
  t            fuzzy file picker

  w            toggle ignore-whitespace
  = / -        expand / shrink context lines
  , / .        decrease / increase tab width

  r            toggle resolved (on a commented line)
  K / 0        add reaction / clear reactions (on a draft)
  V            cycle review verdict (comment / approve / request changes)

  c            add / edit comment on current line
  x            delete comment on current line
  S            submit drafts → REVIEW.md at repo root

  mouse        click to move cursor, click header to collapse, wheel to scroll

  ?            toggle this help
  q            quit (drafts auto-persist to .gitdiff/drafts.json)
```
