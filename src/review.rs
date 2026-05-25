use crate::app::{Reply, Thread, Verdict};
use crate::git::DiffSource;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub fn threads_path(root: &Path, source: &DiffSource) -> PathBuf {
    root.join(".gitdiff")
        .join(format!("threads-{}.json", source.slug()))
}

pub fn viewed_path(root: &Path, source: &DiffSource) -> PathBuf {
    root.join(".gitdiff")
        .join(format!("viewed-{}.json", source.slug()))
}

pub fn review_path(root: &Path, source: &DiffSource) -> PathBuf {
    root.join(format!("REVIEW-{}.md", source.slug()))
}

/// Viewed entries store a hash of the file's diff content at the time the
/// user marked it viewed. On load we drop entries whose hash no longer matches
/// the current diff, so re-edits clear the tick automatically.
pub fn load_viewed(root: &Path, source: &DiffSource) -> Result<HashMap<String, String>> {
    let p = viewed_path(root, source);
    if !p.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    if let Ok(m) = serde_json::from_str::<HashMap<String, String>>(&raw) {
        return Ok(m);
    }
    // Fall back to the pre-hash array format. Entries land with empty hashes,
    // which will fail the hash check at startup and be cleared — that's the
    // intended behaviour for a one-time format upgrade.
    if let Ok(v) = serde_json::from_str::<Vec<String>>(&raw) {
        return Ok(v.into_iter().map(|p| (p, String::new())).collect());
    }
    Ok(HashMap::new())
}

pub fn save_viewed(
    root: &Path,
    source: &DiffSource,
    viewed: &HashMap<String, String>,
) -> Result<()> {
    let p = viewed_path(root, source);
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    // BTreeMap for stable key ordering in the on-disk file.
    let sorted: BTreeMap<&String, &String> = viewed.iter().collect();
    fs::write(&p, serde_json::to_string_pretty(&sorted)?)
        .with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

pub fn load_threads(root: &Path, source: &DiffSource) -> Result<Vec<Thread>> {
    let p = threads_path(root, source);
    // One-time migration from the old `drafts-{slug}.json` name. Rename the
    // file in place so subsequent runs read the canonical path. If the rename
    // fails (e.g. permissions), fall through and return empty — the user can
    // rename manually.
    if !p.exists() {
        let legacy = root
            .join(".gitdiff")
            .join(format!("drafts-{}.json", source.slug()));
        if legacy.exists() {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::rename(&legacy, &p);
        }
    }
    if !p.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&raw).with_context(|| format!("parse {}", p.display()))?)
}

pub fn save_threads(root: &Path, source: &DiffSource, threads: &[Thread]) -> Result<()> {
    let p = threads_path(root, source);
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let raw = serde_json::to_string_pretty(threads)?;
    fs::write(&p, raw).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

/// TUI-side save that re-reads the on-disk JSON, merges any new threads /
/// replies the CLI added while the TUI was running, then writes the union.
/// Without this the TUI's stale in-memory snapshot clobbers every concurrent
/// `gitdiff comment`/`gitdiff reply` invocation.
///
/// Merge rules:
///   - Thread on disk but not in memory → appended to memory (CLI created it).
///   - Thread in both → kept in memory; any disk-only reply is appended,
///     deduped by (author, created_at, body-modulo-trailing-ws).
///   - In-memory mutations to body/resolved/reactions win (the user just
///     edited them in the TUI; the CLI rarely touches these).
///
/// Deletions made in the TUI are a known limitation: if the CLI raced a
/// reply onto a thread the user deleted, the thread comes back at next save.
/// That's a rare-enough collision to defer until we have proper locking.
pub fn save_threads_merging(
    root: &Path,
    source: &DiffSource,
    threads: &mut Vec<Thread>,
) -> Result<()> {
    let on_disk = load_threads(root, source).unwrap_or_default();
    for disk_thread in on_disk {
        match threads
            .iter_mut()
            .find(|t| t.thread_id == disk_thread.thread_id)
        {
            Some(in_mem) => {
                for dr in disk_thread.replies {
                    let dup = in_mem.replies.iter().any(|x| {
                        x.author == dr.author
                            && x.created_at == dr.created_at
                            && x.body.trim_end() == dr.body.trim_end()
                    });
                    if !dup {
                        in_mem.replies.push(dr);
                    }
                }
            }
            None => threads.push(disk_thread),
        }
    }
    save_threads(root, source, threads)
}

const AGENT_INSTRUCTIONS: &str = "<!--
INSTRUCTIONS FOR CODING AGENTS (claude-code, codex, gemini, etc.)
=================================================================

You are reading a gitdiff review file. A human wrote the comments under each
`### Lxx ... · thread:tID` heading; your job is to address them.

For each open comment:
  1. Read the comment body and the `Original` diff snippet under the heading.
  2. Make the requested code change in the actual source file, OR write a
     reply explaining why you did not.
  3. Append your reply inside the matching `<!-- replies:tID --> ... <!--
     /replies:tID -->` block, one line per reply line:
         > [@your-handle 2026-05-24T17:05:00Z] your reply text, one line.
     Multi-line reply? Emit several lines with the same author+timestamp.
  4. When the thread is fully addressed (code change shipped or no action
     needed), add a single line `<!-- resolved -->` inside the same replies
     block. gitdiff hides resolved threads from its TUI on the next launch.

Choose any `your-handle` EXCEPT `you` (that handle is reserved for the human
running gitdiff — typical agent handles are claude-code, codex, gemini). Use
ISO-8601 UTC timestamps.

EVERYTHING OUTSIDE THE `<!-- replies:* -->` BLOCKS IS OVERWRITTEN on the
human's next submit. Do not edit there — your changes will be lost. Replies
and the `<!-- resolved -->` marker are preserved across submits.
-->\n\n";

pub fn write_review(
    root: &Path,
    source: &DiffSource,
    source_label: &str,
    base_sha: Option<&str>,
    head_sha: Option<&str>,
    threads: &[Thread],
    verdict: Verdict,
) -> Result<PathBuf> {
    let p = review_path(root, source);
    let mut out = String::new();
    out.push_str(AGENT_INSTRUCTIONS);
    out.push_str("# Code Review\n\n");
    out.push_str(&format!(
        "Generated by gitdiff on {}\n",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    ));
    out.push_str(&format!("Source: {source_label}\n"));
    if let (Some(b), Some(h)) = (base_sha, head_sha) {
        out.push_str(&format!("Range: {b} .. {h}\n"));
    }
    out.push_str(&format!("Verdict: {}\n", verdict.label()));
    let _ = source;
    let open = threads.iter().filter(|d| !d.resolved).count();
    let resolved = threads.len() - open;
    let outdated = threads.iter().filter(|d| d.outdated).count();
    out.push_str(&format!(
        "Status: {open} open · {resolved} resolved · {outdated} outdated\n\n---\n\n"
    ));

    if threads.is_empty() {
        out.push_str("_No comments yet._\n");
        fs::write(&p, out)?;
        return Ok(p);
    }

    write_section(&mut out, "Open", threads.iter().filter(|d| !d.resolved));
    if resolved > 0 {
        out.push_str("---\n\n# Resolved\n\n");
        write_section(&mut out, "Resolved", threads.iter().filter(|d| d.resolved));
    }

    fs::write(&p, out).with_context(|| format!("write {}", p.display()))?;
    Ok(p)
}

fn write_section<'a>(out: &mut String, _label: &str, ds: impl Iterator<Item = &'a Thread>) {
    let mut by_file: BTreeMap<String, Vec<&Thread>> = BTreeMap::new();
    for d in ds {
        by_file.entry(d.file_path.clone()).or_default().push(d);
    }
    for (file, mut entries) in by_file {
        entries.sort_by_key(|d| d.new_lineno.unwrap_or(d.old_lineno.unwrap_or(0)));
        out.push_str(&format!("## {file}\n\n"));
        for d in entries {
            let anchor = d.anchor_label();
            let status = if d.resolved {
                "resolved"
            } else if d.outdated {
                "open · outdated"
            } else {
                "open"
            };
            let tid = if d.thread_id.is_empty() {
                "(no-id)"
            } else {
                d.thread_id.as_str()
            };
            out.push_str(&format!("### {anchor} · {status} · thread:{tid}\n\n"));
            out.push_str("Original:\n\n```diff\n");
            out.push_str(&d.diff_snippet);
            if !d.diff_snippet.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
            out.push_str(&format!(
                "**Comment** (you, {}):\n\n",
                d.created_at.format("%Y-%m-%dT%H:%M:%SZ")
            ));
            for line in d.body.lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            if !d.reactions.is_empty() {
                out.push_str(&format!("\nReactions: {}\n", d.reactions.join(" ")));
            }
            // Replies block — agents must edit only inside this delimiter pair.
            // Timestamps include sub-second precision so the parser round-trips
            // a reply back to the exact same Reply on the next launch (avoids
            // the duplicate-on-merge bug). A blank line separates consecutive
            // replies so the parser doesn't fold them into one multi-line body.
            out.push_str(&format!("\n<!-- replies:{tid} -->\n"));
            for (ri, r) in d.replies.iter().enumerate() {
                if ri > 0 {
                    out.push('\n');
                }
                let ts = r.created_at.format("%Y-%m-%dT%H:%M:%S%.6fZ");
                if r.body.is_empty() {
                    out.push_str(&format!("> [@{} {}]\n", r.author, ts));
                } else {
                    for ln in r.body.lines() {
                        out.push_str(&format!("> [@{} {}] {}\n", r.author, ts, ln));
                    }
                }
            }
            if d.resolved {
                out.push_str("<!-- resolved -->\n");
            }
            out.push_str(&format!("<!-- /replies:{tid} -->\n"));
            out.push_str("\n---\n\n");
        }
    }
}

/// Per-thread payload imported back from a REVIEW-*.md `<!-- replies:tID -->`
/// block.
#[derive(Debug, Default, Clone)]
pub struct ThreadImport {
    pub replies: Vec<Reply>,
    /// True when the agent placed a `<!-- resolved -->` marker inside the block.
    pub resolved: bool,
}

/// Scans a REVIEW-*.md file and extracts all `<!-- replies:tID --> ... <!-- /replies:tID -->`
/// blocks. Returns a thread_id → import map for merging back into the JSON
/// thread store on launch.
pub fn parse_review_replies(text: &str) -> HashMap<String, ThreadImport> {
    let mut out: HashMap<String, ThreadImport> = HashMap::new();
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("<!-- replies:") {
        let abs_start = cursor + start;
        let head = &text[abs_start..];
        let Some(close_idx) = head.find(" -->") else {
            break;
        };
        let tid = head["<!-- replies:".len()..close_idx].trim().to_string();
        let body_start = abs_start + close_idx + " -->".len();
        let end_marker = format!("<!-- /replies:{tid} -->");
        let Some(rel_end) = text[body_start..].find(&end_marker) else {
            cursor = body_start;
            continue;
        };
        let body_end = body_start + rel_end;
        let block = &text[body_start..body_end];
        let mut replies = Vec::new();
        let mut resolved = false;
        let mut current: Option<Reply> = None;
        for raw in block.lines() {
            let line = raw.trim_end();
            // Resolve marker. Accept variants for forgiving parsing.
            let trimmed = line.trim();
            if trimmed == "<!-- resolved -->"
                || trimmed == "<!--resolved-->"
                || trimmed.eq_ignore_ascii_case("<!-- resolved -->")
            {
                resolved = true;
                continue;
            }
            if !line.starts_with("> [@") {
                if line.trim().is_empty() {
                    if let Some(r) = current.take() {
                        replies.push(r);
                    }
                }
                continue;
            }
            // Parse: > [@handle 2026-05-24T17:05:00Z] body...
            // Header lookup uses the trim_end'd line (`]` and timestamp are
            // ASCII). The body uses the *raw* line so leading whitespace
            // inside the body survives — write_review emits exactly one
            // separator space after `]`, and code-block lines carry real
            // 4-space indentation we must preserve. trim_start() would
            // strip both, making the next roundtrip's body differ from the
            // in-memory original and bypassing the dedupe check → the same
            // reply gets re-imported every time the TUI sees the file.
            let rest = &line[3..]; // drop "> ["
            let Some(close_br) = rest.find(']') else {
                continue;
            };
            let header = &rest[1..close_br]; // skip leading '@'
            // Indices are the same in `line` and `raw` because trim_end
            // only differs in the tail; slice the raw form so trailing
            // characters survive untouched.
            let after_br = &raw[close_br + 4..]; // past "> [" + "]"
            let body_part = after_br.strip_prefix(' ').unwrap_or(after_br);
            let mut it = header.splitn(2, ' ');
            let author = it.next().unwrap_or("").trim().to_string();
            let ts_str = it.next().unwrap_or("").trim();
            let created_at = parse_ts(ts_str).unwrap_or_else(Utc::now);
            match current.as_mut() {
                Some(r) if r.author == author && r.created_at == created_at => {
                    r.body.push('\n');
                    r.body.push_str(body_part);
                }
                _ => {
                    if let Some(r) = current.take() {
                        replies.push(r);
                    }
                    current = Some(Reply {
                        author,
                        body: body_part.to_string(),
                        created_at,
                    });
                }
            }
        }
        if let Some(r) = current.take() {
            replies.push(r);
        }
        if !replies.is_empty() || resolved {
            let entry = out.entry(tid).or_default();
            entry.replies.extend(replies);
            entry.resolved = entry.resolved || resolved;
        }
        cursor = body_end + end_marker.len();
    }
    out
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}
