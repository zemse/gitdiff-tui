use crate::app::{Thread, Verdict};
use crate::git::DiffSource;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
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
///   - Thread on disk but not in memory → appended to memory (CLI created it),
///     unless its id appears in `suppressed_threads` (user deleted it in TUI).
///   - Thread in both → kept in memory; any disk-only reply is appended,
///     deduped by (author, created_at, body-modulo-trailing-ws). A disk
///     reply whose (thread_id, author, created_at) appears in
///     `suppressed_replies` is skipped — that's the pre-edit ghost of a
///     reply the user just modified, and re-appending it would resurrect
///     the original alongside the edited copy (visible as "my edit posted
///     a fresh reply instead of replacing my own").
///   - In-memory mutations to body/resolved/reactions win (the user just
///     edited them in the TUI; the CLI rarely touches these).
///
/// Both suppression sets are cleared after a successful save: the disk now
/// reflects the post-edit state, so keeping stale identities around could
/// drop a legitimate future CLI write that happens to collide.
pub fn save_threads_merging(
    root: &Path,
    source: &DiffSource,
    threads: &mut Vec<Thread>,
    suppressed_replies: &mut HashSet<(String, String, DateTime<Utc>)>,
    suppressed_threads: &mut HashSet<String>,
) -> Result<()> {
    let on_disk = load_threads(root, source).unwrap_or_default();
    for disk_thread in on_disk {
        if suppressed_threads.contains(&disk_thread.thread_id) {
            continue;
        }
        let tid = disk_thread.thread_id.clone();
        match threads.iter_mut().find(|t| t.thread_id == tid) {
            Some(in_mem) => {
                for dr in disk_thread.replies {
                    let key = (tid.clone(), dr.author.clone(), dr.created_at);
                    if suppressed_replies.contains(&key) {
                        continue;
                    }
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
    save_threads(root, source, threads)?;
    suppressed_replies.clear();
    suppressed_threads.clear();
    Ok(())
}

/// Renders the current thread store as a human-readable Markdown artifact at
/// the repo root. One-way only — agents drive every mutation through the CLI
/// (`gitdiff reply`/`comment`/`resolve`/…) and the TUI polls the JSON store
/// directly, so this file is never parsed back in. Safe to commit, paste into
/// a PR, or hand to a reviewer.
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
            if !d.replies.is_empty() {
                out.push('\n');
                for (ri, r) in d.replies.iter().enumerate() {
                    if ri > 0 {
                        out.push('\n');
                    }
                    let ts = r.created_at.format("%Y-%m-%dT%H:%M:%SZ");
                    out.push_str(&format!("**Reply** ({}, {}):\n\n", r.author, ts));
                    for ln in r.body.lines() {
                        out.push_str("> ");
                        out.push_str(ln);
                        out.push('\n');
                    }
                }
            }
            out.push_str("\n---\n\n");
        }
    }
}
