mod app;
mod clipboard;
mod diff;
mod git;
mod review;
mod syntax;
mod ui;

use anyhow::Result;
use app::{AppState, FlatKind, FuzzyPicker, Mode};
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
use std::env;
use std::io;
use std::time::Duration;
use tui_textarea::{CursorMove, TextArea};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let override_range = args.iter().find(|a| a.contains("..")).cloned();

    let root = git::repo_root()?;
    let source = git::detect_source(&root, override_range)?;
    let source_label = source.label();
    let opts = git::DiffOpts::default();
    let raw = git::get_diff(&root, &source, opts)?;
    let files = diff::parse(&raw)?;

    let drafts = review::load_drafts(&root, &source)?;
    let viewed = review::load_viewed(&root, &source).unwrap_or_default();
    let mut state = AppState::new(source.clone(), source_label, files, drafts, viewed);
    state.opts = opts;
    state.mark_outdated_drafts();
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

    let _ = review::save_drafts(&root, &state.source, &state.drafts);
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
                    handle_composing(state, &ev, &mut composer);
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
            state.show_drafts_pane = !state.show_drafts_pane;
            state.drafts_cursor = state.drafts_cursor.min(state.drafts.len().saturating_sub(1).max(0));
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
            if state.delete_draft_at_cursor() {
                state.status = Some("draft deleted".into());
            }
        }
        KeyCode::Char('S') => {
            let p = review::write_review(
                root,
                &state.source,
                &state.source_label,
                base_sha,
                head_sha,
                &state.drafts,
                state.verdict,
            )?;
            review::save_drafts(root, &state.source, &state.drafts)?;
            state.status = Some(format!("wrote {} ({})", p.display(), state.verdict.label()));
        }
        KeyCode::Char('r') => {
            if let Some(now_resolved) = state.toggle_resolved_at_cursor() {
                state.status = Some(if now_resolved {
                    "draft resolved".into()
                } else {
                    "draft re-opened".into()
                });
            } else {
                state.status = Some("no draft at cursor".into());
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
                state.status = Some("no draft at cursor".into());
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
    let Some((px, py, pw, ph)) = state.composer_rect else { return };
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
    let local_col = (m.column - inner_x) as u16;
    let local_row = (m.row - inner_y) as u16;
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            ta.cancel_selection();
            ta.move_cursor(CursorMove::Jump(local_row, local_col));
            ta.start_selection();
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Selection is anchored at the Down position; moving the cursor
            // while `is_selecting()` extends it (move_cursor passes shift=true).
            ta.move_cursor(CursorMove::Jump(local_row, local_col));
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
    if (sr, sc) == (cr, cc) { Some((er, ec)) } else { Some((sr, sc)) }
}

fn handle_mouse(
    state: &mut AppState,
    m: MouseEvent,
    composer: &mut Option<TextArea<'static>>,
    root: &std::path::Path,
) {
    // If the click is in the left sidebar (tree) or right (drafts), dispatch there.
    if state.show_tree && m.column < state.body_x {
        handle_tree_mouse(state, m);
        return;
    }
    if state.show_drafts_pane && m.column >= state.body_x + state.body_width {
        handle_drafts_mouse(state, m);
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
                FlatKind::DraftRow => {
                    state.cursor = idx;
                    state.clear_selection();
                    if let Some(di) = fl.draft_idx {
                        edit_draft(state, di, composer);
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

fn handle_drafts_mouse(state: &mut AppState, m: MouseEvent) {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let content_top = state.body_top + 1;
    if m.row < content_top {
        return;
    }
    // Each draft entry spans 3 rows in our renderer (header, preview, blank).
    let row = (m.row - content_top) as usize;
    let idx = row / 3;
    if idx >= state.drafts.len() {
        return;
    }
    state.drafts_cursor = idx;
    state.jump_to_draft(idx);
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
        let len = state.files.iter().filter(|f| {
            if picker.query.is_empty() {
                true
            } else {
                let q = picker.query.to_lowercase();
                let h = f.path.to_lowercase();
                let mut it = h.chars();
                q.chars().all(|c| it.any(|x| x == c))
            }
        }).count();
        if len == 0 {
            picker.cursor = 0;
        } else {
            picker.cursor = picker.cursor.min(len - 1);
        }
    }
}

fn edit_draft(
    state: &mut AppState,
    draft_idx: usize,
    composer: &mut Option<TextArea<'static>>,
) -> bool {
    let d = match state.drafts.get(draft_idx) {
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
    let Some(anchor_idx) = target else { return false };
    state.start_selection(anchor_idx);
    state.finish_selection();
    open_composer(state, composer);
    true
}

fn open_composer(state: &mut AppState, composer: &mut Option<TextArea<'static>>) {
    let existing = state.existing_draft_body_for_selection();
    let edit_idx = state.draft_for_selection();
    let mut ta = TextArea::default();
    let initial_lines = if let Some(b) = existing {
        let lines: Vec<String> = b.lines().map(|s| s.to_string()).collect();
        for (i, line) in lines.iter().enumerate() {
            ta.insert_str(line);
            if i + 1 < lines.len() {
                ta.insert_newline();
            }
        }
        lines.len().max(1)
    } else {
        1
    };
    *composer = Some(ta);
    state.mode = Mode::Composing;
    // Hide the draft being edited so the composer replaces (not duplicates) it.
    if edit_idx.is_some() {
        state.editing_draft_idx = edit_idx;
        state.rebuild_flat();
    }
    // Seed composer_height with the body's natural size so the gap computed on
    // this frame (before ui::draw runs again) already fits the content.
    let h = (initial_lines as u16)
        .saturating_add(2)
        .max(ui::COMPOSER_MIN_H);
    state.composer_height = h;
    // Scroll so the selection start and the inserted composer-gap fit in view.
    // Gap occupies rows [a+1, a+1+H) in extended coords (see ui::composer_gap).
    if let Some(sel) = state.selection {
        let (a, _b) = sel.range();
        let composer_h = state.composer_height as usize;
        let needed_scroll_floor = (a + 1 + composer_h).saturating_sub(state.viewport_height);
        // keep selection start visible at the top
        let target = needed_scroll_floor.min(a);
        if state.scroll < target {
            state.scroll = target;
        }
    }
}

fn close_composer(state: &mut AppState, composer: &mut Option<TextArea<'static>>) {
    *composer = None;
    state.mode = Mode::Normal;
    state.composer_height = 0;
    state.editing_draft_idx = None;
    state.clear_selection();
    state.cursor_visible = false;
    // Always rebuild so a newly-saved (or just-edited) draft renders on the
    // next frame without waiting for an unrelated trigger.
    state.rebuild_flat();
}

fn handle_composing(state: &mut AppState, ev: &Event, composer: &mut Option<TextArea<'static>>) {
    let Event::Key(key) = ev else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Esc => {
            close_composer(state, composer);
            state.status = Some("comment cancelled".into());
        }
        KeyCode::Char('s') if ctrl => {
            let body = composer
                .as_ref()
                .map(|t| t.lines().join("\n"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if body.is_empty() {
                state.status = Some("empty comment, cancelled".into());
            } else if state.add_draft_from_selection(body).is_some() {
                state.status = Some("draft saved (press S to submit to REVIEW.md)".into());
            }
            close_composer(state, composer);
        }
        KeyCode::Char('d') if ctrl => {
            if let Some(idx) = state.editing_draft_idx {
                state.drafts.remove(idx);
                close_composer(state, composer);
                state.status = Some("draft deleted".into());
            } else {
                state.status = Some("nothing to delete (new comment)".into());
            }
        }
        _ => {
            if let Some(ta) = composer {
                ta.input(*key);
            }
        }
    }
}
