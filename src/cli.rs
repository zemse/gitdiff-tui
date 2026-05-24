//! Non-interactive CLI built on `clap` derive. Lets coding agents
//! (claude-code, codex, gemini, …) drive gitdiff without the TUI: print the
//! diff, list/show comment threads, post new comments, reply, edit, resolve,
//! delete.
//!
//! Every subcommand reads/writes `.gitdiff/threads-{slug}.json` via
//! `review::{load_threads, save_threads}`, exactly like the TUI does, so the
//! same data survives across CLI ↔ TUI usage.

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

use crate::app::{LOCAL_AUTHOR, Reply, Thread, make_thread_id};
use crate::git::{self, DiffOpts, DiffSource};
use crate::review;

/// Top-level CLI. `args_conflicts_with_subcommands` keeps the legacy
/// `gitdiff <base>..<head>` TUI launch working without colliding with
/// subcommand parsing.
#[derive(Parser, Debug)]
#[command(
    name = "gitdiff",
    version,
    about = "Review local git changes like a GitHub PR — leave inline comments for an agent or human.",
    long_about = "Review local git changes like a GitHub PR. Comments persist to .gitdiff/threads-*.json and can be flushed to REVIEW.md for an agent to act on. With no subcommand, launches the interactive TUI; subcommands let an agent script every operation.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Optional `<base>..<head>` range for the TUI. Auto-detected if absent.
    pub range: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Print the unified diff to stdout.
    Diff {
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Lines of context around each hunk (default 3).
        #[arg(long)]
        context: Option<usize>,
        /// Ignore whitespace differences (passes `-w` to git diff).
        #[arg(long = "ignore-whitespace")]
        ignore_whitespace: bool,
    },

    /// List comment threads (open by default).
    List {
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Include resolved threads.
        #[arg(long)]
        all: bool,
        /// Emit JSON (full Thread structs).
        #[arg(long)]
        json: bool,
    },

    /// Print a single thread with all its replies.
    Show {
        /// Thread id (full `t_xxxxxxxx` or any unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Add a new comment thread anchored at `<file>:<line>`.
    Comment {
        /// File path (repo-relative).
        file: String,
        /// Anchor line number.
        line: usize,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Final line of a multi-line anchor (inclusive).
        #[arg(long)]
        end: Option<usize>,
        /// Anchor side: `new` (post-change line, default) or `old` (pre-change,
        /// for comments on deleted lines).
        #[arg(long, default_value = "new", value_parser = ["new", "old"])]
        side: String,
        #[command(flatten)]
        body: BodyInput,
        /// Author handle (default `you`). Agents pass `claude-code`,
        /// `codex`, `gemini`, …
        #[arg(long, default_value_t = LOCAL_AUTHOR.to_string())]
        author: String,
    },

    /// Reply to an existing thread.
    Reply {
        /// Thread id (full or unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        #[command(flatten)]
        body: BodyInput,
        /// Author handle (default `you`).
        #[arg(long, default_value_t = LOCAL_AUTHOR.to_string())]
        author: String,
    },

    /// Edit a thread body, or with `--reply N` the Nth reply (0-indexed).
    Edit {
        /// Thread id (full or unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Edit reply N (0-indexed) instead of the original body.
        #[arg(long)]
        reply: Option<usize>,
        #[command(flatten)]
        body: BodyInput,
    },

    /// Mark a thread as resolved.
    Resolve {
        /// Thread id (full or unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
    },

    /// Un-resolve a previously-resolved thread.
    Reopen {
        /// Thread id (full or unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
    },

    /// Delete a thread (or with `--reply N`, just that reply).
    Delete {
        /// Thread id (full or unique prefix).
        thread_id: String,
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Delete reply N (0-indexed) instead of the whole thread.
        #[arg(long)]
        reply: Option<usize>,
    },
}

/// Mutually-exclusive body source. clap enforces "exactly one" at parse time
/// via the ArgGroup attributes below.
#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct BodyInput {
    /// Body text inline.
    #[arg(long)]
    body: Option<String>,
    /// Read body from a file.
    #[arg(long = "body-file", value_name = "PATH")]
    body_file: Option<PathBuf>,
    /// Read body from stdin until EOF.
    #[arg(long = "body-stdin")]
    body_stdin: bool,
}

impl BodyInput {
    fn read(&self) -> Result<String> {
        if let Some(b) = &self.body {
            return Ok(b.clone());
        }
        if let Some(p) = &self.body_file {
            return std::fs::read_to_string(p)
                .with_context(|| format!("read --body-file {}", p.display()));
        }
        // body_stdin is true; clap guaranteed exactly one source.
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .context("read body from stdin")?;
        Ok(s)
    }
}

// ----------------------------------------------------------------------------
// Shared helpers
// ----------------------------------------------------------------------------

/// Resolve a (possibly abbreviated) thread id against the loaded threads.
/// Allows the user to pass a prefix as long as it's unambiguous — mirrors
/// git's abbreviated-SHA UX.
fn find_thread(threads: &[Thread], id: &str) -> Result<usize> {
    let mut matches: Vec<usize> = threads
        .iter()
        .enumerate()
        .filter(|(_, t)| t.thread_id == id || t.thread_id.starts_with(id))
        .map(|(i, _)| i)
        .collect();
    if matches.is_empty() {
        bail!("no thread matches id '{id}'");
    }
    if matches.len() > 1 {
        let ids: Vec<&str> = matches
            .iter()
            .map(|i| threads[*i].thread_id.as_str())
            .collect();
        bail!(
            "id '{id}' is ambiguous; matches {} threads: {}",
            ids.len(),
            ids.join(", ")
        );
    }
    Ok(matches.remove(0))
}

fn resolve_source(range: Option<String>) -> Result<(PathBuf, DiffSource)> {
    let root = git::repo_root()?;
    let source = git::detect_source(&root, range)?;
    Ok((root, source))
}

// ----------------------------------------------------------------------------
// Subcommand entrypoints
// ----------------------------------------------------------------------------

fn cmd_diff(range: Option<String>, context: Option<usize>, ignore_ws: bool) -> Result<()> {
    let (root, source) = resolve_source(range)?;
    let mut opts = DiffOpts::default();
    if ignore_ws {
        opts.ignore_whitespace = true;
    }
    if let Some(c) = context {
        opts.context_lines = c;
    }
    let raw = git::get_diff(&root, &source, opts)?;
    print!("{raw}");
    Ok(())
}

fn cmd_list(range: Option<String>, include_resolved: bool, as_json: bool) -> Result<()> {
    let (root, source) = resolve_source(range)?;
    let threads = review::load_threads(&root, &source)?;
    let filtered: Vec<&Thread> = threads
        .iter()
        .filter(|t| include_resolved || !t.resolved)
        .collect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }
    if filtered.is_empty() {
        eprintln!(
            "no {}threads in {}",
            if include_resolved { "" } else { "open " },
            source.label()
        );
        return Ok(());
    }
    println!("{} thread(s) in {}:", filtered.len(), source.label());
    for t in &filtered {
        let status = if t.resolved {
            "resolved"
        } else if t.outdated {
            "outdated"
        } else {
            "open"
        };
        let attn = crate::app::needs_attention(t);
        let mark = if attn { " ★" } else { "" };
        let last_author = t
            .replies
            .last()
            .map(|r| r.author.as_str())
            .unwrap_or(LOCAL_AUTHOR);
        let preview: String = t
            .body
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(72)
            .collect();
        println!(
            "  {tid}  {status:8}  {file}:{anchor}  (@{last}, {replies} reply){mark}\n      {preview}",
            tid = t.thread_id,
            status = status,
            file = t.file_path,
            anchor = t.anchor_label(),
            last = last_author,
            replies = t.replies.len(),
            mark = mark,
            preview = preview,
        );
    }
    Ok(())
}

fn cmd_show(thread_id: String, range: Option<String>, as_json: bool) -> Result<()> {
    let (root, source) = resolve_source(range)?;
    let threads = review::load_threads(&root, &source)?;
    let idx = find_thread(&threads, &thread_id)?;
    let t = &threads[idx];
    if as_json {
        println!("{}", serde_json::to_string_pretty(t)?);
        return Ok(());
    }
    let status = if t.resolved {
        "resolved"
    } else if t.outdated {
        "outdated"
    } else {
        "open"
    };
    println!("Thread {}", t.thread_id);
    println!("  Status:  {status}");
    println!("  File:    {} {}", t.file_path, t.anchor_label());
    println!("  Author:  @{}", LOCAL_AUTHOR);
    println!("  Created: {}", t.created_at.format("%Y-%m-%dT%H:%M:%SZ"));
    if !t.reactions.is_empty() {
        println!("  React:   {}", t.reactions.join(" "));
    }
    println!();
    println!("{}", t.body);
    for (i, r) in t.replies.iter().enumerate() {
        println!();
        println!(
            "--- reply {i} by @{} at {} ---",
            r.author,
            r.created_at.format("%Y-%m-%dT%H:%M:%SZ")
        );
        println!("{}", r.body);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_comment(
    file: String,
    line: usize,
    range: Option<String>,
    end: Option<usize>,
    side: String,
    body_input: &BodyInput,
    author: String,
) -> Result<()> {
    let body = body_input.read()?;
    let (root, source) = resolve_source(range)?;
    let mut threads = review::load_threads(&root, &source)?;

    let created_at = Utc::now();
    let (old_lineno, new_lineno) = if side == "new" {
        (None, Some(line))
    } else {
        (Some(line), None)
    };
    let (old_end, new_end) = match (end, side.as_str()) {
        (Some(e), "new") => (None, Some(e)),
        (Some(e), _) => (Some(e), None),
        (None, _) => (None, None),
    };
    let thread_id = make_thread_id(&file, old_lineno, new_lineno, &created_at);

    // If the author isn't the local human, record the message as the first
    // Reply so the thread's last-message-attribution flips it purple
    // ("awaiting your reply") for the human reviewer.
    let (body_field, replies) = if author == LOCAL_AUTHOR {
        (body.clone(), Vec::new())
    } else {
        (
            format!("(agent {author} opened this thread)"),
            vec![Reply {
                author: author.clone(),
                body,
                created_at,
            }],
        )
    };

    threads.push(Thread {
        file_path: file,
        old_lineno,
        new_lineno,
        old_lineno_end: old_end,
        new_lineno_end: new_end,
        line_kind: "context".to_string(),
        diff_snippet: String::new(),
        body: body_field,
        created_at,
        resolved: false,
        outdated: false,
        reactions: Vec::new(),
        thread_id: thread_id.clone(),
        replies,
        anchor_content: String::new(),
    });
    review::save_threads(&root, &source, &threads)?;
    println!("{thread_id}");
    Ok(())
}

fn cmd_reply(
    thread_id: String,
    range: Option<String>,
    body_input: &BodyInput,
    author: String,
) -> Result<()> {
    let body = body_input.read()?;
    let (root, source) = resolve_source(range)?;
    let mut threads = review::load_threads(&root, &source)?;
    let idx = find_thread(&threads, &thread_id)?;
    threads[idx].replies.push(Reply {
        author,
        body,
        created_at: Utc::now(),
    });
    review::save_threads(&root, &source, &threads)?;
    println!("{}", threads[idx].thread_id);
    Ok(())
}

fn cmd_edit(
    thread_id: String,
    range: Option<String>,
    reply: Option<usize>,
    body_input: &BodyInput,
) -> Result<()> {
    let body = body_input.read()?;
    let (root, source) = resolve_source(range)?;
    let mut threads = review::load_threads(&root, &source)?;
    let idx = find_thread(&threads, &thread_id)?;
    match reply {
        Some(ri) => {
            let n = threads[idx].replies.len();
            let r = threads[idx]
                .replies
                .get_mut(ri)
                .ok_or_else(|| anyhow!("reply index {ri} out of range (thread has {n})"))?;
            r.body = body;
            r.created_at = Utc::now();
        }
        None => {
            threads[idx].body = body;
            threads[idx].created_at = Utc::now();
        }
    }
    review::save_threads(&root, &source, &threads)?;
    println!("{}", threads[idx].thread_id);
    Ok(())
}

fn cmd_set_resolved(thread_id: String, range: Option<String>, resolved: bool) -> Result<()> {
    let (root, source) = resolve_source(range)?;
    let mut threads = review::load_threads(&root, &source)?;
    let idx = find_thread(&threads, &thread_id)?;
    threads[idx].resolved = resolved;
    review::save_threads(&root, &source, &threads)?;
    println!("{}", threads[idx].thread_id);
    Ok(())
}

fn cmd_delete(thread_id: String, range: Option<String>, reply: Option<usize>) -> Result<()> {
    let (root, source) = resolve_source(range)?;
    let mut threads = review::load_threads(&root, &source)?;
    let idx = find_thread(&threads, &thread_id)?;
    match reply {
        Some(ri) => {
            let n = threads[idx].replies.len();
            if ri >= n {
                bail!("reply index {ri} out of range (thread has {n})");
            }
            threads[idx].replies.remove(ri);
            review::save_threads(&root, &source, &threads)?;
            println!("{}", threads[idx].thread_id);
        }
        None => {
            let tid = threads[idx].thread_id.clone();
            threads.remove(idx);
            review::save_threads(&root, &source, &threads)?;
            println!("{tid}");
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Help trailer ("what would I auto-detect right now?")
// ----------------------------------------------------------------------------

/// Probes the current working directory and produces a short multi-line block
/// describing what `gitdiff` (no args) would do here. Appended to clap's
/// generated `--help` so the user can sanity-check the default before running
/// anything destructive.
fn build_default_diff_trailer() -> String {
    use std::fmt::Write;
    let mut s = String::new();
    s.push_str("DEFAULT DIFF (auto-detected from current working dir)\n");
    match git::repo_root() {
        Err(e) => {
            let _ = writeln!(s, "    (not inside a git repo: {e})");
        }
        Ok(root) => match git::detect_source(&root, None) {
            Err(e) => {
                let _ = writeln!(s, "    (could not pick a default: {e})");
            }
            Ok(source) => {
                let label = source.label();
                let threads_p = review::threads_path(&root, &source);
                let n = review::load_threads(&root, &source)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let _ = writeln!(s, "    Range:        {label}");
                let _ = writeln!(
                    s,
                    "    Threads file: {} ({n} thread(s))",
                    threads_p.display()
                );
            }
        },
    }
    s
}

// ----------------------------------------------------------------------------
// Dispatch
// ----------------------------------------------------------------------------

/// Parse argv with clap, applying the live default-diff trailer to `--help`
/// output. Returns:
/// - `Ok(None)` when no subcommand was given (caller proceeds with TUI launch);
///   the optional `<base>..<head>` is in `cli.range`.
/// - `Ok(Some(()))` when a subcommand ran successfully.
/// - `Err(_)` when a subcommand failed.
///
/// Exits the process on `--help` / `--version` (clap's default behaviour).
pub fn parse_and_dispatch() -> Result<Option<Cli>> {
    let trailer = build_default_diff_trailer();
    let mut cmd = Cli::command().after_help(trailer);
    let matches = cmd.get_matches_mut(); // clap exits on --help/--version
    let cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow!("clap parse error: {e}"))?;

    let Some(command) = cli.command else {
        return Ok(Some(Cli {
            range: cli.range,
            command: None,
        }));
    };

    match command {
        Commands::Diff {
            range,
            context,
            ignore_whitespace,
        } => cmd_diff(range, context, ignore_whitespace)?,
        Commands::List { range, all, json } => cmd_list(range, all, json)?,
        Commands::Show {
            thread_id,
            range,
            json,
        } => cmd_show(thread_id, range, json)?,
        Commands::Comment {
            file,
            line,
            range,
            end,
            side,
            body,
            author,
        } => cmd_comment(file, line, range, end, side, &body, author)?,
        Commands::Reply {
            thread_id,
            range,
            body,
            author,
        } => cmd_reply(thread_id, range, &body, author)?,
        Commands::Edit {
            thread_id,
            range,
            reply,
            body,
        } => cmd_edit(thread_id, range, reply, &body)?,
        Commands::Resolve { thread_id, range } => cmd_set_resolved(thread_id, range, true)?,
        Commands::Reopen { thread_id, range } => cmd_set_resolved(thread_id, range, false)?,
        Commands::Delete {
            thread_id,
            range,
            reply,
        } => cmd_delete(thread_id, range, reply)?,
    }
    Ok(None)
}

// FromArgMatches is brought into scope so the `Cli::from_arg_matches` call
// above resolves to the derived impl.
use clap::FromArgMatches;
