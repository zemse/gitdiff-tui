mod app;
mod cli;
mod clipboard;
mod diff;
mod git;
mod review;
mod syntax;
mod ui;

use anyhow::Result;
use app::{AppState, ComposerTarget, FlatKind, FuzzyPicker, Mode};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;
use tui_textarea::{CursorMove, TextArea};

/// Body comparison used by every reply-dedupe path: ignores trailing
/// whitespace so an in-memory body of "X\n" and its round-tripped form "X"
/// (write_review's `r.body.lines()` drops the trailing newline) compare equal.
/// Without this, dedupe missed and the same reply got re-appended on every
/// REVIEW.md import.
fn bodies_equivalent(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// True if `r` already exists in `replies` (matched on author + timestamp +
/// body modulo trailing whitespace).
fn reply_is_duplicate(replies: &[app::Reply], r: &app::Reply) -> bool {
    replies.iter().any(|x| {
        x.author == r.author
            && x.created_at == r.created_at
            && bodies_equivalent(&x.body, &r.body)
    })
}

/// Heal duplicates produced by older builds: when two replies share author +
/// timestamp and their bodies match after leading-whitespace + trailing-ws
/// normalization, the second is the lossy roundtrip copy — drop it, keep the
/// longer-bodied original.
fn heal_lossy_roundtrip_dups(replies: &mut Vec<app::Reply>) {
    fn normalize(b: &str) -> String {
        b.lines()
            .map(|l| l.trim_start())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }
    let mut keep = vec![true; replies.len()];
    for i in 0..replies.len() {
        if !keep[i] {
            continue;
        }
        for j in (i + 1)..replies.len() {
            if !keep[j] {
                continue;
            }
            let a = &replies[i];
            let b = &replies[j];
            if a.author != b.author || a.created_at != b.created_at {
                continue;
            }
            if normalize(&a.body) != normalize(&b.body) {
                continue;
            }
            // Same fingerprint after normalization → drop the shorter copy.
            if a.body.len() >= b.body.len() {
                keep[j] = false;
            } else {
                keep[i] = false;
                break; // i is gone — outer loop will skip it
            }
        }
    }
    let mut idx = 0;
    replies.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn reply(author: &str, ts_ns: u32, body: &str) -> app::Reply {
        app::Reply {
            author: author.to_string(),
            body: body.to_string(),
            created_at: chrono::Utc.timestamp_opt(1_700_000_000, ts_ns).unwrap(),
        }
    }

    #[test]
    fn dedupe_ignores_trailing_newline() {
        assert!(bodies_equivalent("hello\n", "hello"));
        assert!(bodies_equivalent("hello\n\n", "hello"));
        assert!(bodies_equivalent("a\nb\n", "a\nb"));
        assert!(!bodies_equivalent("a\nb", "a b"));
    }

    #[test]
    fn heal_removes_leading_ws_stripped_copy() {
        let original = "fn foo() {\n    let x = 1;\n}";
        let stripped = "fn foo() {\nlet x = 1;\n}";
        let mut replies = vec![
            reply("claude-code", 1_000, original),
            reply("you", 2_000, "human reply"),
            reply("claude-code", 1_000, stripped),
        ];
        heal_lossy_roundtrip_dups(&mut replies);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].body, original); // longer body kept
        assert_eq!(replies[1].author, "you");
    }

    #[test]
    fn heal_removes_trailing_newline_copy() {
        let mut replies = vec![
            reply("a", 1_000, "X\n"),
            reply("a", 1_000, "X"),
        ];
        heal_lossy_roundtrip_dups(&mut replies);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].body, "X\n"); // longer kept
    }

    #[test]
    fn heal_keeps_distinct_replies() {
        let mut replies = vec![
            reply("a", 1_000, "first"),
            reply("a", 2_000, "second"),
            reply("a", 1_000, "different"), // same ts but different content
        ];
        heal_lossy_roundtrip_dups(&mut replies);
        assert_eq!(replies.len(), 3);
    }
}

fn main() -> Result<()> {
    // Parse argv with clap. Subcommands run inside `parse_and_dispatch` and
    // return `Ok(None)`; the no-subcommand path returns the parsed `Cli` so we
    // can pick up `cli.range` and launch the TUI below. clap exits the process
    // itself on --help / --version.
    let cli = match cli::parse_and_dispatch()? {
        None => return Ok(()),
        Some(c) => c,
    };
    let override_range = cli.range.clone();

    let root = git::repo_root()?;
    let source = git::detect_source(&root, override_range)?;
    let source_label = source.label();
    let opts = git::DiffOpts::default();
    let raw = git::get_diff(&root, &source, opts)?;
    let files = diff::parse(&raw)?;

    let mut threads = review::load_threads(&root, &source)?;
    // Backfill thread_ids on threads saved before the threads feature shipped,
    // so they survive the next round-trip with stable IDs.
    for d in threads.iter_mut() {
        if d.thread_id.is_empty() {
            d.thread_id =
                app::make_thread_id(&d.file_path, d.old_lineno, d.new_lineno, &d.created_at);
        }
    }
    // Merge any agent replies that have been added to the REVIEW-*.md file
    // since the last submit. The JSON store is canonical, so we re-save once
    // merged to make the imported replies durable.
    let mut merged_any = false;
    let review_p = review::review_path(&root, &source);
    if review_p.exists() {
        if let Ok(text) = std::fs::read_to_string(&review_p) {
            let map = review::parse_review_replies(&text);
            for d in threads.iter_mut() {
                if let Some(import) = map.get(&d.thread_id) {
                    for r in &import.replies {
                        if !reply_is_duplicate(&d.replies, r) {
                            d.replies.push(r.clone());
                            merged_any = true;
                        }
                    }
                    if import.resolved && !d.resolved {
                        d.resolved = true;
                        merged_any = true;
                    }
                }
            }
        }
    }
    // Cleanup pass for the duplicate-reply drift produced by older builds.
    // Three passes per thread:
    //   1. Collapse synthetic 'X\nX\n…' bodies. The old parser merged
    //      consecutive '> [@a T] X' lines (same author+ts) into one
    //      multi-line Reply body, and a duplicated line therefore became
    //      'X\nX'. Only collapse when *every* line is identical so real
    //      multi-line replies stay intact.
    //   2. Dedup by (author, body, timestamp-truncated-to-seconds), keeping
    //      the first (most-precise) occurrence. This catches the
    //      microsecond-vs-second-precision drift from the old write_review.
    //   3. Lossy-roundtrip dedup: drop replies whose body differs from an
    //      earlier reply (same author+ts) only by leading-whitespace
    //      stripping or trailing-newline loss. That's the dup pattern
    //      pre-parse-fix builds produced when re-importing REVIEW.md.
    let mut deduped_any = false;
    for d in threads.iter_mut() {
        let before = d.replies.len();
        for r in d.replies.iter_mut() {
            let lines: Vec<&str> = r.body.split('\n').collect();
            if lines.len() > 1 && lines.iter().all(|l| *l == lines[0]) {
                r.body = lines[0].to_string();
            }
        }
        let mut seen: std::collections::HashSet<(String, String, i64)> =
            std::collections::HashSet::new();
        d.replies
            .retain(|r| seen.insert((r.author.clone(), r.body.clone(), r.created_at.timestamp())));
        heal_lossy_roundtrip_dups(&mut d.replies);
        if d.replies.len() != before {
            deduped_any = true;
        }
    }
    if merged_any || deduped_any {
        let _ = review::save_threads(&root, &source, &threads);
    }
    // Drift-resistant re-anchor: snap threads to the current line numbers
    // when the surrounding code has moved. Also backfills `anchor_content`
    // for threads saved before that field existed.
    if app::reanchor_threads(&mut threads, &files) {
        let _ = review::save_threads(&root, &source, &threads);
    }
    let mut viewed = review::load_viewed(&root, &source).unwrap_or_default();
    // Drop viewed marks whose stored hash no longer matches the file's current
    // diff content — that's the "this file changed since you marked it viewed,
    // please look again" signal.
    {
        let current: std::collections::HashMap<String, String> = files
            .iter()
            .map(|f| (f.path.clone(), app::file_diff_hash(f)))
            .collect();
        viewed.retain(|path, stored_hash| current.get(path) == Some(stored_hash));
    }
    let mut state = AppState::new(source.clone(), source_label, files, threads, viewed);
    state.opts = opts;
    state.mark_outdated_threads();
    prefetch_file_blobs(&mut state, &root);

    let (base_sha, head_sha) = match &source {
        git::DiffSource::Branch { base, head } => {
            (git::short_sha(&root, base), git::short_sha(&root, head))
        }
        git::DiffSource::WorkingTree => (git::short_sha(&root, "HEAD"), Some("working".into())),
    };

    if state.files.is_empty() {
        eprintln!("gitdiff: no changes to review ({}).", state.source_label);
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(
        &mut terminal,
        &mut state,
        &root,
        base_sha.as_deref(),
        head_sha.as_deref(),
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    let _ = review::save_threads_merging(
        &root,
        &state.source,
        &mut state.threads,
        &mut state.suppressed_disk_replies,
        &mut state.suppressed_thread_ids,
    );
    let _ = review::save_viewed(&root, &state.source, &state.viewed);

    result
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    root: &std::path::Path,
    base_sha: Option<&str>,
    head_sha: Option<&str>,
) -> Result<()> {
    let mut composer: Option<TextArea<'static>> = None;

    loop {
        terminal.draw(|f| {
            ui::draw(f, state, composer.as_mut());
        })?;

        if !event::poll(Duration::from_millis(250))? {
            // Idle tick: pick up agent edits to REVIEW.md.
            if poll_review_file(state, root) {
                // The next iteration renders the imported replies.
            }
            continue;
        }
        let ev = event::read()?;
        // Ctrl-C exits unconditionally, regardless of current mode. We catch
        // it here so the composer's TextArea doesn't swallow it as input.
        if let Event::Key(k) = &ev {
            if k.kind == KeyEventKind::Press
                && k.modifiers.contains(KeyModifiers::CONTROL)
                && k.code == KeyCode::Char('c')
            {
                state.should_quit = true;
                return Ok(());
            }
        }
        // Track the mouse cursor so the renderer can pop a tooltip when it's
        // over a registered hover region (e.g. a comment's "5m ago" label).
        // Update regardless of mode; the renderer decides when to act on it.
        if let Event::Mouse(m) = &ev {
            state.hover_pos = Some((m.column, m.row));
        }
        match state.mode {
            Mode::Normal => {
                if let Event::Mouse(m) = ev {
                    handle_mouse(state, m, &mut composer, root);
                } else {
                    handle_normal(state, &ev, root, base_sha, head_sha, &mut composer)?;
                }
            }
            Mode::Composing => {
                if let Event::Mouse(m) = ev {
                    handle_composer_mouse(state, m, &mut composer);
                } else {
                    handle_composing(state, &ev, &mut composer, root, base_sha, head_sha);
                }
            }
            Mode::Help => {
                if matches!(ev, Event::Key(_) | Event::Mouse(_)) {
                    state.mode = Mode::Normal;
                }
            }
            Mode::Picker => handle_picker(state, &ev),
        }
        if state.should_quit {
            return Ok(());
        }
    }
}

fn reload_diff(state: &mut AppState, root: &std::path::Path) -> Result<()> {
    let raw = git::get_diff(root, &state.source, state.opts)?;
    let files = diff::parse(&raw)?;
    state.replace_files(files);
    prefetch_file_blobs(state, root);
    Ok(())
}

fn prefetch_file_blobs(state: &mut AppState, root: &std::path::Path) {
    let paths: Vec<String> = state.files.iter().map(|f| f.path.clone()).collect();
    for path in paths {
        if !state.file_blobs.contains_key(&path) {
            let blob = git::read_file_lines(root, &state.source, &path);
            state.set_file_blob(path, blob);
        }
    }
    state.rebuild_flat();
}

fn handle_normal(
    state: &mut AppState,
    ev: &Event,
    root: &std::path::Path,
    base_sha: Option<&str>,
    head_sha: Option<&str>,
    composer: &mut Option<TextArea<'static>>,
) -> Result<()> {
    let Event::Key(key) = ev else { return Ok(()) };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    state.status = None;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let half = (state.viewport_height / 2).max(1) as i32;
    // Any keyboard navigation reveals the cursor.
    let nav_keys = matches!(
        key.code,
        KeyCode::Char('j' | 'k' | 'g' | 'G' | ']' | '[' | '}' | '{')
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
    ) || (ctrl && matches!(key.code, KeyCode::Char('d' | 'u')));
    if nav_keys {
        state.cursor_visible = true;
    }

    match key.code {
        KeyCode::Esc => {
            state.cursor_visible = false;
            state.clear_selection();
            state.status = None;
        }
        KeyCode::Char('q') => state.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => state.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => state.move_cursor(-1),
        KeyCode::Char('d') if ctrl => state.move_cursor(half),
        KeyCode::Char('u') if ctrl => state.move_cursor(-half),
        KeyCode::PageDown => state.move_cursor(state.viewport_height as i32),
        KeyCode::PageUp => state.move_cursor(-(state.viewport_height as i32)),
        KeyCode::Char('g') => {
            state.cursor = 0;
            state.ensure_cursor_visible();
        }
        KeyCode::Char('G') => {
            state.cursor = state.flat.len().saturating_sub(1);
            state.ensure_cursor_visible();
        }
        KeyCode::Char(']') => state.jump_next_file(),
        KeyCode::Char('[') => state.jump_prev_file(),
        KeyCode::Char('}') => state.jump_next_hunk(),
        KeyCode::Char('{') => state.jump_prev_hunk(),
        KeyCode::Char('n') => {
            let before = state.cursor;
            if !state.jump_next_thread_needing_attention() {
                state.status = Some("no unread comments".into());
            } else if state.cursor == before {
                // Wrap landed back on the same anchor → there's only one
                // unread thread and we're already standing on it.
                state.status = Some("only unread comment — you're on it".into());
            } else {
                state.status = Some("→ next unread comment".into());
            }
        }
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char(' ') => {
            if let Some(fl) = state.flat.get(state.cursor).cloned() {
                if fl.kind == FlatKind::HunkHeader {
                    if let Some(hi) = fl.hunk_idx {
                        state.toggle_hunk_collapse(fl.file_idx, hi);
                    }
                } else {
                    state.toggle_collapse(fl.file_idx);
                }
            }
        }
        KeyCode::Char('z') => state.collapse_all(true),
        KeyCode::Char('Z') => state.collapse_all(false),
        KeyCode::Char('e') => {
            state.show_tree = !state.show_tree;
        }
        KeyCode::Char('R') => {
            state.show_threads_pane = !state.show_threads_pane;
            state.threads_cursor = state
                .threads_cursor
                .min(state.threads.len().saturating_sub(1).max(0));
        }
        KeyCode::Char('t') => {
            state.picker = Some(FuzzyPicker {
                query: String::new(),
                cursor: 0,
            });
            state.mode = Mode::Picker;
        }
        KeyCode::Char('w') => {
            state.opts.ignore_whitespace = !state.opts.ignore_whitespace;
            reload_diff(state, root)?;
            state.status = Some(format!(
                "whitespace {}",
                if state.opts.ignore_whitespace {
                    "ignored"
                } else {
                    "shown"
                }
            ));
        }
        KeyCode::Char('=') | KeyCode::Char('+') => {
            let next = match state.opts.context_lines {
                0..=3 => 10,
                4..=10 => 25,
                _ => 9999,
            };
            state.opts.context_lines = next;
            reload_diff(state, root)?;
            state.status = Some(format!("context: {}", state.opts.context_lines));
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            let next = match state.opts.context_lines {
                25.. => 10,
                4..=24 => 3,
                _ => 0,
            };
            state.opts.context_lines = next;
            reload_diff(state, root)?;
            state.status = Some(format!("context: {}", state.opts.context_lines));
        }
        KeyCode::Char('<') => {
            if let Some(fl) = state.flat.get(state.cursor).cloned() {
                if let Some(hi) = fl.hunk_idx {
                    let n = run_expand(state, root, fl.file_idx, hi, 20, true);
                    state.status = Some(format!("expanded {n} lines above"));
                }
            }
        }
        KeyCode::Char('>') => {
            if let Some(fl) = state.flat.get(state.cursor).cloned() {
                if let Some(hi) = fl.hunk_idx {
                    let n = run_expand(state, root, fl.file_idx, hi, 20, false);
                    state.status = Some(format!("expanded {n} lines below"));
                }
            }
        }
        KeyCode::Char(',') => {
            state.tab_width = state.tab_width.saturating_sub(1).max(1);
            state.status = Some(format!("tab width: {}", state.tab_width));
        }
        KeyCode::Char('.') => {
            state.tab_width = (state.tab_width + 1).min(8);
            state.status = Some(format!("tab width: {}", state.tab_width));
        }
        KeyCode::Char('y') => {
            // With an active selection, copy the selected lines' text; falls
            // back to the file path so the no-selection muscle memory still works.
            if let Some(text) = state.selection_text() {
                let n = state.selection_lines().len();
                match clipboard::copy(&text) {
                    Ok(_) => state.status = Some(format!("copied {n} line(s)")),
                    Err(e) => state.status = Some(format!("clipboard error: {e}")),
                }
                state.clear_selection();
            } else if let Some(fl) = state.flat.get(state.cursor) {
                if let Some(file) = state.files.get(fl.file_idx) {
                    match clipboard::copy(&file.path) {
                        Ok(_) => state.status = Some(format!("copied: {}", file.path)),
                        Err(e) => state.status = Some(format!("clipboard error: {e}")),
                    }
                }
            }
        }
        KeyCode::Char('v') => {
            if let Some(fl) = state.flat.get(state.cursor) {
                let fi = fl.file_idx;
                if let Some(now) = state.toggle_viewed(fi) {
                    state.status = Some(if now {
                        "marked viewed (collapsed)".into()
                    } else {
                        "marked unviewed".into()
                    });
                }
            }
        }
        KeyCode::Char('c') => {
            // Use the live selection if any (so a drag → `c` reuses the range);
            // otherwise fall back to a single-line comment on the cursor.
            if state.selection.is_some() && !state.selection_lines().is_empty() {
                open_composer(state, composer);
            } else if state.current_line().is_some() {
                state.start_selection(state.cursor);
                state.finish_selection();
                open_composer(state, composer);
            } else {
                state.status = Some("place cursor on a code line first".into());
            }
        }
        KeyCode::Char('x') => {
            if state.delete_thread_at_cursor() {
                state.status = Some("thread deleted".into());
            }
        }
        KeyCode::Char('S') => {
            let p = review::write_review(
                root,
                &state.source,
                &state.source_label,
                base_sha,
                head_sha,
                &state.threads,
                state.verdict,
            )?;
            review::save_threads_merging(
                root,
                &state.source,
                &mut state.threads,
                &mut state.suppressed_disk_replies,
                &mut state.suppressed_thread_ids,
            )?;
            stamp_review_mtime(state, root);
            state.status = Some(format!("wrote {} ({})", p.display(), state.verdict.label()));
        }
        KeyCode::Char('r') => {
            if let Some(now_resolved) = state.toggle_resolved_at_cursor() {
                state.status = Some(if now_resolved {
                    "thread resolved".into()
                } else {
                    "thread re-opened".into()
                });
            } else {
                state.status = Some("no thread at cursor".into());
            }
        }
        KeyCode::Char('V') => {
            state.verdict = state.verdict.cycle();
            state.status = Some(format!("verdict: {}", state.verdict.label()));
        }
        KeyCode::Char('K') => {
            if let Some(r) = state.add_reaction_at_cursor() {
                state.status = Some(format!("reacted: {r}"));
            } else {
                state.status = Some("no thread at cursor".into());
            }
        }
        KeyCode::Char('0') => {
            if state.clear_reactions_at_cursor() {
                state.status = Some("reactions cleared".into());
            }
        }
        _ => {}
    }
    Ok(())
}

/// Re-imports replies from REVIEW-*.md when the file has been modified since
/// we last touched it. Cheap: skipped entirely when the mtime hasn't changed.
/// Self-writes don't trigger a re-import — `stamp_review_mtime` is called
/// after every write_review to bring the cached mtime in sync.
fn poll_review_file(state: &mut AppState, root: &std::path::Path) -> bool {
    let p = review::review_path(root, &state.source);
    let mtime = match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    if state.last_review_mtime == Some(mtime) {
        return false;
    }
    let prev = state.last_review_mtime.replace(mtime);
    // First sighting (prev is None) just primes the cache — don't re-import
    // the file we just loaded at startup.
    if prev.is_none() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&p) else {
        return false;
    };
    let map = review::parse_review_replies(&text);
    let mut changed = false;
    for d in state.threads.iter_mut() {
        if let Some(import) = map.get(&d.thread_id) {
            for r in &import.replies {
                if !reply_is_duplicate(&d.replies, r) {
                    d.replies.push(r.clone());
                    changed = true;
                }
            }
            if import.resolved && !d.resolved {
                d.resolved = true;
                changed = true;
            }
        }
    }
    if changed {
        let _ = review::save_threads_merging(
            root,
            &state.source,
            &mut state.threads,
            &mut state.suppressed_disk_replies,
            &mut state.suppressed_thread_ids,
        );
        state.rebuild_flat();
        state.status = Some("imported replies from REVIEW.md".into());
    }
    changed
}

/// Updates the cached REVIEW.md mtime to whatever the file currently shows.
/// Call after any successful `write_review` so the next poll tick treats our
/// own write as expected and doesn't re-import.
fn stamp_review_mtime(state: &mut AppState, root: &std::path::Path) {
    let p = review::review_path(root, &state.source);
    if let Ok(t) = std::fs::metadata(&p).and_then(|m| m.modified()) {
        state.last_review_mtime = Some(t);
    }
}

fn handle_composer_mouse(
    state: &mut AppState,
    m: MouseEvent,
    composer: &mut Option<TextArea<'static>>,
) {
    // Scroll wheel still pans the diff under the composer so context stays
    // reachable while typing.
    match m.kind {
        MouseEventKind::ScrollDown => {
            state.scroll_by(3);
            return;
        }
        MouseEventKind::ScrollUp => {
            state.scroll_by(-3);
            return;
        }
        _ => {}
    }
    let Some((px, py, pw, ph)) = state.composer_rect else {
        return;
    };
    // Inner text area sits inside the 1-row border plus 1-col horizontal padding
    // configured in ui::draw_composer.
    if pw < 4 || ph < 3 {
        return;
    }
    let inner_x = px + 2;
    let inner_y = py + 1;
    let inner_w = pw - 4;
    let inner_h = ph - 2;
    let inside = m.column >= inner_x
        && m.column < inner_x + inner_w
        && m.row >= inner_y
        && m.row < inner_y + inner_h;
    if !inside {
        return;
    }
    let Some(ta) = composer.as_mut() else { return };
    let local_col = m.column - inner_x;
    let local_row = m.row - inner_y;
    // The composer wraps long lines, so screen (row, col) is not the same as
    // textarea (logical_row, logical_col). Translate before jumping.
    let inner_w = ui::composer_inner_width(state) as usize;
    let (jrow, jcol) = ui::composer_screen_to_logical(ta, inner_w, local_row, local_col);
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            ta.cancel_selection();
            ta.move_cursor(CursorMove::Jump(jrow, jcol));
            ta.start_selection();
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Selection is anchored at the Down position; moving the cursor
            // while `is_selecting()` extends it (move_cursor passes shift=true).
            ta.move_cursor(CursorMove::Jump(jrow, jcol));
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // If the drag never moved, drop the empty selection so the cursor
            // doesn't appear "stuck" in selection mode.
            if let Some((sr, sc)) = ta_selection_start(ta) {
                let (cr, cc) = ta.cursor();
                if sr == cr && sc == cc {
                    ta.cancel_selection();
                }
            }
        }
        _ => {}
    }
}

/// tui-textarea 0.7 doesn't expose `selection_start` directly; reconstruct it
/// by reading the field through the `selection_range` helper if the cursor is
/// not at the anchor. Returns None when no selection is active.
fn ta_selection_start(ta: &TextArea<'_>) -> Option<(usize, usize)> {
    if !ta.is_selecting() {
        return None;
    }
    let (cr, cc) = ta.cursor();
    let ((sr, sc), (er, ec)) = ta.selection_range()?;
    // selection_range returns the lower position first. The anchor is whichever
    // endpoint isn't the current cursor.
    if (sr, sc) == (cr, cc) {
        Some((er, ec))
    } else {
        Some((sr, sc))
    }
}

fn handle_mouse(
    state: &mut AppState,
    m: MouseEvent,
    composer: &mut Option<TextArea<'static>>,
    root: &std::path::Path,
) {
    // If the click is in the left sidebar (tree) or right (threads), dispatch there.
    if state.show_tree && m.column < state.body_x {
        handle_tree_mouse(state, m);
        return;
    }
    if state.show_threads_pane && m.column >= state.body_x + state.body_width {
        handle_threads_mouse(state, m);
        return;
    }
    // Selection-menu hit-test runs before anything else so the buttons stay
    // clickable even while a selection is active.
    if let MouseEventKind::Down(MouseButton::Left) = m.kind {
        if let Some(action) = ui::selection_menu_hit(state, m.column, m.row) {
            match action {
                ui::SelectionMenuAction::Copy => {
                    if let Some(text) = state.selection_text() {
                        let n = state.selection_lines().len();
                        match clipboard::copy(&text) {
                            Ok(_) => state.status = Some(format!("copied {n} line(s)")),
                            Err(e) => state.status = Some(format!("clipboard error: {e}")),
                        }
                    }
                    state.clear_selection();
                }
                ui::SelectionMenuAction::Comment => open_composer(state, composer),
                ui::SelectionMenuAction::Cancel => {
                    state.clear_selection();
                    state.status = Some("selection cleared".into());
                }
            }
            return;
        }
        // A click outside the menu drops a finished selection.
        if let Some(sel) = state.selection {
            if !sel.dragging {
                state.clear_selection();
            }
        }
    }
    let body_top = state.body_top;
    let row_to_idx = |row: u16| -> Option<usize> {
        if row < body_top {
            return None;
        }
        let offset = (row - body_top) as usize;
        let idx = state.scroll + offset;
        if idx >= state.flat.len() {
            return None;
        }
        Some(idx)
    };
    match m.kind {
        MouseEventKind::ScrollDown => state.scroll_by(3),
        MouseEventKind::ScrollUp => state.scroll_by(-3),
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(idx) = row_to_idx(m.row) else { return };
            let fl = state.flat[idx].clone();
            match fl.kind {
                FlatKind::FileHeader => {
                    state.cursor = idx;
                    state.clear_selection();
                    // Right-aligned " ✓ viewed " / " ☐ viewed " badge is 10
                    // chars wide; the closing `╮` is the very last column.
                    let rel_col = m.column.saturating_sub(state.body_x);
                    let badge_start = state.body_width.saturating_sub(11);
                    let badge_end = state.body_width.saturating_sub(1);
                    if rel_col >= badge_start && rel_col < badge_end {
                        if let Some(now) = state.toggle_viewed(fl.file_idx) {
                            state.status = Some(if now {
                                "marked viewed (collapsed)".into()
                            } else {
                                "marked unviewed".into()
                            });
                        }
                    } else {
                        state.toggle_collapse(fl.file_idx);
                    }
                }
                FlatKind::FileFooter => {
                    state.cursor = idx;
                    state.clear_selection();
                    state.toggle_collapse(fl.file_idx);
                }
                FlatKind::HunkHeader => {
                    state.cursor = idx;
                    state.clear_selection();
                    if let Some(hi) = fl.hunk_idx {
                        state.toggle_hunk_collapse(fl.file_idx, hi);
                    }
                }
                FlatKind::ThreadRow => {
                    state.cursor = idx;
                    state.clear_selection();
                    if let Some(ti) = fl.thread_idx {
                        edit_thread(state, ti, composer);
                    }
                }
                FlatKind::ExpandBtnAbove | FlatKind::ExpandBtnBelow => {
                    state.cursor = idx;
                    state.clear_selection();
                    let above = fl.kind == FlatKind::ExpandBtnAbove;
                    let count = fl.line_idx.unwrap_or(20);
                    if let Some(hi) = fl.hunk_idx {
                        let n = run_expand(state, root, fl.file_idx, hi, count, above);
                        state.status = Some(format!(
                            "expanded {n} lines {}",
                            if above { "above" } else { "below" }
                        ));
                    }
                }
                FlatKind::Code => {
                    state.start_selection(idx);
                }
                _ => {
                    state.cursor = idx;
                    state.clear_selection();
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(idx) = row_to_idx(m.row) else { return };
            state.extend_selection(idx);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(sel) = state.selection {
                let single_click = sel.start == sel.end;
                state.finish_selection();
                if single_click {
                    // No drag → treat as a cursor move on the clicked line,
                    // not a 1-line selection. The floating menu won't appear.
                    state.cursor = sel.start;
                    state.cursor_visible = true;
                    state.clear_selection();
                } else {
                    // Multi-line drag → keep the selection so the menu (drawn
                    // by ui::draw) offers copy / comment.
                    state.cursor = sel.range().1;
                }
            }
        }
        _ => {}
    }
}

fn run_expand(
    state: &mut AppState,
    root: &std::path::Path,
    fi: usize,
    hi: usize,
    count: usize,
    above: bool,
) -> usize {
    // ensure the file blob is cached so expand_hunk knows the file length
    let path = state.files[fi].path.clone();
    if !state.file_blobs.contains_key(&path) {
        let blob = git::read_file_lines(root, &state.source, &path);
        state.set_file_blob(path.clone(), blob);
    }
    let lines = match state.file_blobs.get(&path).and_then(|b| b.as_ref()) {
        Some(v) => v.clone(),
        None => Vec::new(),
    };
    state.expand_hunk(fi, hi, &lines, count, above)
}

fn handle_tree_mouse(state: &mut AppState, m: MouseEvent) {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    // Tree pane: 1-line border on top + 1-line title, content starts at body_top+1.
    let content_top = state.body_top + 1;
    if m.row < content_top {
        return;
    }
    let row = (m.row - content_top) as usize;
    if row >= state.files.len() {
        return;
    }
    state.jump_to_file(row);
}

fn handle_threads_mouse(state: &mut AppState, m: MouseEvent) {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let content_top = state.body_top + 1;
    if m.row < content_top {
        return;
    }
    // Each thread entry spans 3 rows in our renderer (header, preview, blank).
    let row = (m.row - content_top) as usize;
    let idx = row / 3;
    if idx >= state.threads.len() {
        return;
    }
    state.threads_cursor = idx;
    state.jump_to_thread(idx);
}

fn handle_picker(state: &mut AppState, ev: &Event) {
    let Event::Key(key) = ev else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }
    let close = |state: &mut AppState| {
        state.picker = None;
        state.mode = Mode::Normal;
    };
    let Some(picker) = state.picker.as_mut() else {
        state.mode = Mode::Normal;
        return;
    };
    match key.code {
        KeyCode::Esc => close(state),
        KeyCode::Enter => {
            let matches = state.filtered_files();
            let target = matches.get(state.picker.as_ref().unwrap().cursor).copied();
            close(state);
            if let Some(fi) = target {
                state.jump_to_file(fi);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.cursor = picker.cursor.saturating_add(1);
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            picker.cursor = picker.cursor.saturating_add(1);
        }
        KeyCode::Up => {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.cursor = 0;
        }
        KeyCode::Char(c) => {
            picker.query.push(c);
            picker.cursor = 0;
        }
        _ => {}
    }
    // clamp cursor to matches len after update
    if let Some(picker) = state.picker.as_mut() {
        let len = state
            .files
            .iter()
            .filter(|f| {
                if picker.query.is_empty() {
                    true
                } else {
                    let q = picker.query.to_lowercase();
                    let h = f.path.to_lowercase();
                    let mut it = h.chars();
                    q.chars().all(|c| it.any(|x| x == c))
                }
            })
            .count();
        if len == 0 {
            picker.cursor = 0;
        } else {
            picker.cursor = picker.cursor.min(len - 1);
        }
    }
}

fn edit_thread(
    state: &mut AppState,
    thread_idx: usize,
    composer: &mut Option<TextArea<'static>>,
) -> bool {
    let d = match state.threads.get(thread_idx) {
        Some(d) => d.clone(),
        None => return false,
    };
    let Some(fi) = state.files.iter().position(|f| f.path == d.file_path) else {
        return false;
    };
    let target = state.flat.iter().position(|fl| {
        if fl.kind != FlatKind::Code || fl.file_idx != fi {
            return false;
        }
        let Some(hi) = fl.hunk_idx else { return false };
        let Some(li) = fl.line_idx else { return false };
        let Some(line) = state.files[fi].hunks.get(hi).and_then(|h| h.lines.get(li)) else {
            return false;
        };
        line.old_lineno == d.old_lineno && line.new_lineno == d.new_lineno
    });
    let Some(anchor_idx) = target else {
        return false;
    };
    state.start_selection(anchor_idx);
    state.finish_selection();
    open_composer(state, composer);
    true
}

fn open_composer(state: &mut AppState, composer: &mut Option<TextArea<'static>>) {
    let target = state.decide_composer_target();
    let body = state.initial_composer_body(target);
    let mut ta = TextArea::default();
    let initial_lines = if body.is_empty() {
        1
    } else {
        let lines: Vec<String> = body.lines().map(|s| s.to_string()).collect();
        for (i, line) in lines.iter().enumerate() {
            ta.insert_str(line);
            if i + 1 < lines.len() {
                ta.insert_newline();
            }
        }
        lines.len().max(1)
    };
    *composer = Some(ta);
    state.mode = Mode::Composing;
    state.composer_target = Some(target);
    state.editing_thread_idx = target.hides_thread();
    if state.editing_thread_idx.is_some() {
        // Hide the thread being edited/replied to so the composer takes its slot.
        state.rebuild_flat();
    }
    // Seed composer_height with the body's natural size so the gap computed on
    // this frame (before ui::draw runs again) already fits the content.
    let h = (initial_lines as u16)
        .saturating_add(2)
        .max(ui::COMPOSER_MIN_H);
    state.composer_height = h;
    // Scroll so the composer-gap fits in view. For new/edit-thread the gap is
    // anchored at the selection end; for reply targets it sits below the
    // thread's last row.
    let composer_h = state.composer_height as usize;
    let gap_pos: usize = match target {
        ComposerTarget::NewReply(idx)
        | ComposerTarget::EditReply {
            thread_idx: idx, ..
        } => state
            .flat
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, fl)| {
                (fl.kind == FlatKind::ThreadRow && fl.thread_idx == Some(idx)).then_some(i + 1)
            })
            .unwrap_or_else(|| {
                state
                    .selection
                    .map(|s| s.range().1 + 1)
                    .unwrap_or(state.cursor)
            }),
        _ => state
            .selection
            .map(|s| s.range().1 + 1)
            .unwrap_or(state.cursor),
    };
    let needed_scroll_floor = (gap_pos + composer_h).saturating_sub(state.viewport_height);
    // Keep the selection (or the head of the thread) visible at the top if possible.
    let keep_top = state.selection.map(|s| s.range().0).unwrap_or(state.cursor);
    let scroll_target = needed_scroll_floor.min(keep_top);
    if state.scroll < scroll_target {
        state.scroll = scroll_target;
    }
}

fn close_composer(state: &mut AppState, composer: &mut Option<TextArea<'static>>) {
    *composer = None;
    state.mode = Mode::Normal;
    state.composer_height = 0;
    state.editing_thread_idx = None;
    state.composer_target = None;
    state.clear_selection();
    state.cursor_visible = false;
    // Always rebuild so a newly-saved (or just-edited) thread renders on the
    // next frame without waiting for an unrelated trigger.
    state.rebuild_flat();
}

fn handle_composing(
    state: &mut AppState,
    ev: &Event,
    composer: &mut Option<TextArea<'static>>,
    root: &std::path::Path,
    base_sha: Option<&str>,
    head_sha: Option<&str>,
) {
    let Event::Key(key) = ev else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Save shortcut: plain Enter (or ctrl-s as legacy alias). Shift/Alt+Enter
    // and ctrl+Enter all insert a literal newline so multi-line comments stay
    // possible.
    let save_now = matches!(key.code, KeyCode::Enter) && !shift && !alt && !ctrl
        || matches!(key.code, KeyCode::Char('s')) && ctrl;

    match key.code {
        KeyCode::Esc => {
            close_composer(state, composer);
            state.status = Some("comment cancelled".into());
        }
        _ if save_now => {
            let body = composer
                .as_ref()
                .map(|t| t.lines().join("\n"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let target = state.composer_target.unwrap_or(ComposerTarget::NewThread);
            if body.is_empty() {
                state.status = Some("empty comment, cancelled".into());
            } else if state.save_composer(target, body) {
                // Persist to the JSON store on every save so a crash before
                // submit doesn't lose work.
                let _ = review::save_threads_merging(
            root,
            &state.source,
            &mut state.threads,
            &mut state.suppressed_disk_replies,
            &mut state.suppressed_thread_ids,
        );
                let is_reply = matches!(
                    target,
                    ComposerTarget::NewReply(_) | ComposerTarget::EditReply { .. }
                );
                // Replies auto-flush to REVIEW.md so an agent watching the file
                // sees them immediately — no need to wait for `S`.
                let mut suffix = "press S to submit".to_string();
                if is_reply {
                    if let Ok(p) = review::write_review(
                        root,
                        &state.source,
                        &state.source_label,
                        base_sha,
                        head_sha,
                        &state.threads,
                        state.verdict,
                    ) {
                        suffix = format!("REVIEW.md updated → {}", p.display());
                        stamp_review_mtime(state, root);
                    }
                }
                let msg = match target {
                    ComposerTarget::NewThread => "thread saved",
                    ComposerTarget::EditThread(_) => "comment edited",
                    ComposerTarget::EditReply { .. } => "reply edited",
                    ComposerTarget::NewReply(_) => "reply added",
                };
                state.status = Some(format!("{msg} ({suffix})"));
            }
            close_composer(state, composer);
        }
        KeyCode::Char('d') if ctrl => {
            // ctrl-d only deletes when editing the original thread. Reply
            // edits/new-reply paths leave it as a no-op to avoid accidentally
            // nuking a thread someone replied to.
            match state.composer_target {
                Some(ComposerTarget::EditThread(idx)) => {
                    state
                        .suppressed_thread_ids
                        .insert(state.threads[idx].thread_id.clone());
                    state.threads.remove(idx);
                    close_composer(state, composer);
                    state.status = Some("thread deleted".into());
                }
                Some(ComposerTarget::EditReply { .. } | ComposerTarget::NewReply(_)) => {
                    state.status = Some("ctrl-d disabled inside a thread".into());
                }
                _ => {
                    state.status = Some("nothing to delete (new comment)".into());
                }
            }
        }
        _ => {
            if let Some(ta) = composer {
                ta.input(*key);
            }
        }
    }
}
