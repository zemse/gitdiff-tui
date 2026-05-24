# gitdiff

A Rust + ratatui TUI for reviewing local git changes like a GitHub PR — without
pushing or opening a PR. Comments are written to a `REVIEW.md` at the repo root,
in a format a coding agent (or another human) can act on directly.

![gitdiff TUI — comment thread with an agent reply](https://raw.githubusercontent.com/zemse/gitdiff-tui/main/screenshot.png)

## Features

- Auto-detects what to diff — working changes if dirty, else current branch vs.
  the first trunk that exists, probed in order:
  `upstream/main` → `upstream/master` → `origin/main` → `origin/master` →
  local `main` → local `master`. (Fork workflows with `upstream` as the
  canonical remote diff against canonical, not your fork's possibly-stale copy.)
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
- Threads pane (`R`) listing every open comment; `S` submits all threads to
  `REVIEW.md`. Threads auto-persist to `.gitdiff/threads.json`.
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

## CLI subcommands (for agents and scripting)

The same comment store the TUI uses is exposed as non-interactive subcommands
so a coding agent (claude-code, codex, gemini, …) can read the diff, list
threads, post comments, reply, and resolve — without launching a terminal UI.
Every subcommand accepts an optional `<base>..<head>` argument anywhere; if
omitted, the same auto-detection as the TUI is used.

```sh
gitdiff diff [<range>]                     # print the unified diff to stdout
gitdiff list [<range>] [--all] [--json]    # list open threads (--all incl. resolved)
gitdiff show <tid> [<range>] [--json]      # print one thread with replies
gitdiff comment <file> <line> --body ...   # add a new thread
gitdiff reply <tid> --body ...             # reply to an existing thread
gitdiff edit <tid> [--reply N] --body ...  # edit a thread body or its Nth reply
gitdiff resolve <tid> | reopen <tid>       # toggle resolved
gitdiff delete <tid> [--reply N]           # delete a thread or just one reply
```

Body input takes `--body <text>`, `--body-file <path>`, or `--body-stdin`
(read from stdin). Thread ids accept any unique prefix (git SHA-style).
Agents should pass `--author claude-code` (or their handle); the human
reviewer then sees the thread highlighted as "awaiting your reply" in the
TUI. Run `gitdiff --help` (or `gitdiff <subcommand> --help`) for the full
clap-generated reference plus a live "what diff would I auto-detect right
now" trailer.

Example agent loop:

```sh
gitdiff list --json                                          # see open threads
gitdiff show t_a1b2c3 --json                                 # read one thread
# ... agent makes the requested code change ...
gitdiff reply t_a1b2c3 --author claude-code \
    --body 'addressed in commit c0ffee1'
gitdiff resolve t_a1b2c3
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
  R            toggle threads pane
  t            fuzzy file picker

  w            toggle ignore-whitespace
  = / -        expand / shrink context lines
  , / .        decrease / increase tab width

  r            toggle resolved (on a commented line)
  K / 0        add reaction / clear reactions (on a thread)
  V            cycle review verdict (comment / approve / request changes)

  c            add / edit comment on current line
  x            delete comment on current line
  S            submit threads → REVIEW.md at repo root

  mouse        click to move cursor, click header to collapse, wheel to scroll

  ?            toggle this help
  q            quit (threads auto-persist to .gitdiff/threads.json)
```
