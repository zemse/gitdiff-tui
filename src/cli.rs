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

use crate::app::{
    AGENT_AUTHOR, LOCAL_AUTHOR, Reply, Thread, awaiting_agent_response, make_thread_id,
};
use crate::diff::{self, FileDiff, LineKind};
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
        /// Author handle (default `agent`). Agents may pass a specific name
        /// like `claude-code`, `codex`, `gemini`; a human driving the CLI
        /// passes `--author you`.
        #[arg(long, default_value_t = AGENT_AUTHOR.to_string())]
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
        /// Author handle (default `agent`). A human driving the CLI passes
        /// `--author you`.
        #[arg(long, default_value_t = AGENT_AUTHOR.to_string())]
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

    /// Stream new threads/replies/resolves to stdout as the JSON store
    /// changes. Lets an agent react to each human reply the moment it lands
    /// instead of polling or waiting for a batch.
    ///
    /// At startup it emits a `system` event (response etiquette for the agent)
    /// followed by an `awaiting_response` event for every thread still owed an
    /// agent reply (the human spoke last, unresolved) — this *is* the backlog,
    /// so you don't need `gitdiff list` first. After that, only deltas. Every
    /// event carries an `awaiting_response` boolean.
    ///
    /// Exits on Ctrl-C. Stdout is line-flushed so a pipeline reader sees
    /// each event immediately.
    Watch {
        /// Optional `<base>..<head>` range; auto-detected if absent.
        range: Option<String>,
        /// Only emit events whose author matches. `--author you` watches human
        /// activity (replies + brand-new threads + the awaiting-response
        /// backlog) and skips the agent's own writes.
        #[arg(long)]
        author: Option<String>,
        /// Poll interval in milliseconds. Lower = snappier, higher = cheaper.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Emit one JSON object per line (machine-readable). Default is
        /// short human-readable lines.
        #[arg(long)]
        json: bool,
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

/// Resolved anchor information for a (file, line, side) the caller wants to
/// pin a comment to. `old_lineno`/`new_lineno` mirror the matched diff line so
/// the TUI's `(old, new)` lookup later finds the same row (critical: context
/// lines have *both* fields set — storing only the requested side trivially
/// fails that match and the thread shows up as `outdated`/hidden).
struct ResolvedAnchor {
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
    content: String,
    kind: LineKind,
}

/// Walk the parsed diff for the requested side+lineno. `None` means the line
/// isn't covered by any hunk — caller falls back to one-sided storage and
/// the thread will be marked outdated until the diff catches up to it.
fn resolve_anchor(file_diff: &FileDiff, line: usize, side: &str) -> Option<ResolvedAnchor> {
    for h in &file_diff.hunks {
        for l in &h.lines {
            let hit = if side == "new" {
                l.new_lineno == Some(line)
            } else {
                l.old_lineno == Some(line)
            };
            if hit {
                return Some(ResolvedAnchor {
                    old_lineno: l.old_lineno,
                    new_lineno: l.new_lineno,
                    content: l.content.clone(),
                    kind: l.kind,
                });
            }
        }
    }
    None
}

fn kind_label(k: LineKind) -> &'static str {
    match k {
        LineKind::Added => "added",
        LineKind::Deleted => "deleted",
        LineKind::Context => "context",
    }
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

    // Look up the real diff line so context-line anchors store both
    // (old, new) fields — without this, threads on unchanged lines never
    // re-match in the TUI and get hidden as outdated.
    let raw_diff = git::get_diff(&root, &source, DiffOpts::default())?;
    let files = diff::parse(&raw_diff)?;
    let file_diff = files.iter().find(|f| f.path == file);

    let anchor = file_diff.and_then(|fd| resolve_anchor(fd, line, &side));
    let (old_lineno, new_lineno, anchor_content, line_kind) = match &anchor {
        Some(a) => (
            a.old_lineno,
            a.new_lineno,
            a.content.clone(),
            kind_label(a.kind).to_string(),
        ),
        None => {
            let (o, n) = if side == "new" {
                (None, Some(line))
            } else {
                (Some(line), None)
            };
            (o, n, String::new(), "context".to_string())
        }
    };

    let (old_end, new_end) = match end {
        Some(e) => {
            let resolved = file_diff.and_then(|fd| resolve_anchor(fd, e, &side));
            match resolved {
                Some(a) => (a.old_lineno, a.new_lineno),
                None => {
                    if side == "new" {
                        (None, Some(e))
                    } else {
                        (Some(e), None)
                    }
                }
            }
        }
        None => (None, None),
    };

    let created_at = Utc::now();
    let thread_id = make_thread_id(&file, old_lineno, new_lineno, &created_at);

    // If the author isn't the local human, record the message as the first
    // Reply so the thread's last-message-attribution flips it purple
    // ("awaiting your reply") for the human reviewer.
    let (body_field, replies) = if author == LOCAL_AUTHOR {
        (body.clone(), Vec::new())
    } else {
        (
            format!("(@{author} opened this thread)"),
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
        line_kind,
        diff_snippet: String::new(),
        body: body_field,
        created_at,
        resolved: false,
        outdated: false,
        reactions: Vec::new(),
        thread_id: thread_id.clone(),
        replies,
        anchor_content,
        acknowledged_at: None,
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
// Watch
// ----------------------------------------------------------------------------

/// Polls the threads JSON for new threads/replies/resolves and emits one
/// event per line. The expected consumer is an agent that wants to react
/// to each human reply as it lands, instead of waiting for the human to
/// finish a batch and then processing in bulk.
///
/// Implementation notes:
///   - mtime check skips the parse when the file hasn't changed.
///   - Baseline is taken at startup so existing replies don't re-emit.
///   - Stdout is flushed after every event so a piped reader sees them.
///   - Author filter is matched verbatim. Pass `--author you` to watch
///     only human messages; the agent's own replies (`--author claude-code`,
///     etc.) are skipped.
/// Response etiquette emitted to the agent at the start of every `watch`
/// stream. Keeps agents from spamming threads with low-value acknowledgements.
const WATCH_AGENT_PROMPT: &str = "You are watching review threads. Reply to a thread ONLY when you have something concrete and genuinely important to add — a real answer, a decision, a substantive update, or a necessary question. If your reply would just be a simple affirmation (\"ok\", \"done\", \"got it\", \"sounds good\"), do NOT reply at all. Silence is the correct response when nothing important needs saying.";

fn emit_system_prompt(out: &mut impl std::io::Write, as_json: bool) -> Result<()> {
    if as_json {
        let obj = serde_json::json!({ "event": "system", "prompt": WATCH_AGENT_PROMPT });
        writeln!(out, "{obj}")?;
    } else {
        writeln!(out, "[system] {WATCH_AGENT_PROMPT}")?;
    }
    Ok(())
}

/// True when `author` should pass `filter` (None = no filter = everything).
fn author_passes(filter: Option<&str>, author: &str) -> bool {
    filter.map(|f| f == author).unwrap_or(true)
}

fn cmd_watch(
    range: Option<String>,
    author_filter: Option<String>,
    interval_ms: u64,
    as_json: bool,
) -> Result<()> {
    use std::collections::HashSet;
    use std::io::Write;
    use std::time::Duration;

    let (root, source) = resolve_source(range)?;
    let threads_p = review::threads_path(&root, &source);

    type ReplyKey = (String, String, chrono::DateTime<chrono::Utc>);
    fn rkey(tid: &str, r: &Reply) -> ReplyKey {
        (tid.to_string(), r.author.clone(), r.created_at)
    }

    let mut seen_threads: HashSet<String> = HashSet::new();
    let mut seen_replies: HashSet<ReplyKey> = HashSet::new();
    let mut seen_resolved: HashSet<String> = HashSet::new();

    // Baseline pass — do NOT emit; we only stream deltas from here on.
    let baseline = review::load_threads(&root, &source).unwrap_or_default();
    for t in &baseline {
        seen_threads.insert(t.thread_id.clone());
        if t.resolved {
            seen_resolved.insert(t.thread_id.clone());
        }
        for r in &t.replies {
            seen_replies.insert(rkey(&t.thread_id, r));
        }
    }

    eprintln!(
        "gitdiff watch: {} (every {}ms; baseline {} thread(s), {} repl(ies)). Ctrl-C to exit.",
        source.label(),
        interval_ms,
        baseline.len(),
        seen_replies.len()
    );

    let interval = Duration::from_millis(interval_ms.max(50));
    let stdout = std::io::stdout();

    // Lead with the response-etiquette system prompt, then surface the current
    // backlog of threads still awaiting an agent response (the human spoke
    // last, unresolved). These predate the watch, so the delta loop below
    // would never emit them — but they're exactly what the agent needs to act
    // on. Skipped when the filter excludes the human's messages.
    {
        let mut out = stdout.lock();
        emit_system_prompt(&mut out, as_json)?;
        if author_passes(author_filter.as_deref(), LOCAL_AUTHOR) {
            for t in &baseline {
                if awaiting_agent_response(t) {
                    let last = t.replies.iter().enumerate().last();
                    emit_event(&mut out, as_json, "awaiting_response", t, last)?;
                }
            }
        }
        let _ = out.flush();
    }

    let mut last_mtime: Option<std::time::SystemTime> = std::fs::metadata(&threads_p)
        .and_then(|m| m.modified())
        .ok();

    loop {
        std::thread::sleep(interval);

        // Cheap path: file unchanged → don't even parse.
        let mtime = std::fs::metadata(&threads_p)
            .and_then(|m| m.modified())
            .ok();
        if mtime == last_mtime {
            continue;
        }
        last_mtime = mtime;

        // load_threads errors on a half-written file (the TUI/CLI can be
        // mid-write). Just skip; next tick will catch the settled state.
        let threads = match review::load_threads(&root, &source) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let mut out = stdout.lock();
        for t in &threads {
            if seen_threads.insert(t.thread_id.clone()) {
                // Unfiltered: every new thread. Filtered: only emit a new
                // thread the human just opened (no agent reply seeded yet) when
                // watching `--author you`, so an agent's human-activity stream
                // doesn't miss brand-new threads awaiting its response.
                let emit_new_thread = match author_filter.as_deref() {
                    None => true,
                    Some(f) => f == LOCAL_AUTHOR && t.replies.is_empty(),
                };
                if emit_new_thread {
                    emit_event(&mut out, as_json, "new_thread", t, None)?;
                }
            }
            for (i, r) in t.replies.iter().enumerate() {
                if !seen_replies.insert(rkey(&t.thread_id, r)) {
                    continue;
                }
                let pass = author_filter
                    .as_deref()
                    .map(|f| f == r.author)
                    .unwrap_or(true);
                if pass {
                    emit_event(&mut out, as_json, "new_reply", t, Some((i, r)))?;
                }
            }
            if t.resolved && seen_resolved.insert(t.thread_id.clone()) && author_filter.is_none() {
                emit_event(&mut out, as_json, "resolved", t, None)?;
            }
        }
        let _ = out.flush();
    }
}

fn emit_event(
    out: &mut impl std::io::Write,
    as_json: bool,
    event: &str,
    t: &Thread,
    reply: Option<(usize, &Reply)>,
) -> Result<()> {
    if as_json {
        let mut obj = serde_json::json!({
            "event": event,
            "thread_id": t.thread_id,
            "file": t.file_path,
            "anchor": t.anchor_label(),
            "awaiting_response": awaiting_agent_response(t),
        });
        if t.resolved {
            obj["resolved"] = serde_json::Value::Bool(true);
        }
        if let Some((idx, r)) = reply {
            obj["reply_index"] = serde_json::json!(idx);
            obj["author"] = serde_json::json!(r.author);
            obj["created_at"] =
                serde_json::json!(r.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string());
            obj["body"] = serde_json::json!(r.body);
        }
        writeln!(out, "{obj}")?;
    } else {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        // Flag threads the agent still owes a reply on so a human tailing the
        // stream can see the backlog at a glance.
        let flag = if awaiting_agent_response(t) {
            " ⟵ awaiting agent reply"
        } else {
            ""
        };
        match reply {
            Some((i, r)) => {
                let preview: String = r
                    .body
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                writeln!(
                    out,
                    "[{ts}] {event} {tid} {file}:{anchor} reply#{i} @{author}{flag}\n    {preview}",
                    tid = t.thread_id,
                    file = t.file_path,
                    anchor = t.anchor_label(),
                    author = r.author,
                )?;
            }
            None => {
                let preview: String = t
                    .body
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                writeln!(
                    out,
                    "[{ts}] {event} {tid} {file}:{anchor}{flag}\n    {preview}",
                    tid = t.thread_id,
                    file = t.file_path,
                    anchor = t.anchor_label(),
                )?;
            }
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
        Commands::Watch {
            range,
            author,
            interval_ms,
            json,
        } => cmd_watch(range, author, interval_ms, json)?,
    }
    Ok(None)
}

// FromArgMatches is brought into scope so the `Cli::from_arg_matches` call
// above resolves to the derived impl.
use clap::FromArgMatches;
