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
use tui_textarea::TextArea;

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
        match state.mode {
            Mode::Normal => {
                if let Event::Mouse(m) = ev {
                    handle_mouse(state, m, &mut composer);
                } else {
                    handle_normal(state, &ev, root, base_sha, head_sha, &mut composer)?;
                }
            }
            Mode::Composing => handle_composing(state, &ev, &mut composer),
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
    Ok(())
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

    match key.code {
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
            if let Some(fl) = state.flat.get(state.cursor) {
                state.toggle_collapse(fl.file_idx);
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
        KeyCode::Char(',') => {
            state.tab_width = state.tab_width.saturating_sub(1).max(1);
            state.status = Some(format!("tab width: {}", state.tab_width));
        }
        KeyCode::Char('.') => {
            state.tab_width = (state.tab_width + 1).min(8);
            state.status = Some(format!("tab width: {}", state.tab_width));
        }
        KeyCode::Char('y') => {
            if let Some(fl) = state.flat.get(state.cursor) {
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
            if state.current_line().is_some() {
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

fn handle_mouse(
    state: &mut AppState,
    m: MouseEvent,
    composer: &mut Option<TextArea<'static>>,
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
                FlatKind::FileHeader | FlatKind::FileFooter => {
                    state.cursor = idx;
                    state.clear_selection();
                    state.toggle_collapse(fl.file_idx);
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
                if !sel.dragging || sel.start == sel.end {
                    // also covers true single-click (no drag movement)
                }
                state.finish_selection();
                if !state.selection_lines().is_empty() {
                    open_composer(state, composer);
                }
            }
        }
        _ => {}
    }
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

fn open_composer(state: &mut AppState, composer: &mut Option<TextArea<'static>>) {
    let existing = state.existing_draft_body_for_selection();
    let mut ta = TextArea::default();
    if let Some(b) = existing {
        let lines: Vec<&str> = b.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            ta.insert_str(line);
            if i + 1 < lines.len() {
                ta.insert_newline();
            }
        }
    }
    *composer = Some(ta);
    state.mode = Mode::Composing;
}

fn handle_composing(state: &mut AppState, ev: &Event, composer: &mut Option<TextArea<'static>>) {
    let Event::Key(key) = ev else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Esc => {
            *composer = None;
            state.mode = Mode::Normal;
            state.clear_selection();
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
            *composer = None;
            state.mode = Mode::Normal;
            state.clear_selection();
        }
        _ => {
            if let Some(ta) = composer {
                ta.input(*key);
            }
        }
    }
}
