mod app;
mod diff;
mod git;
mod review;
mod ui;

use anyhow::Result;
use app::{AppState, Mode};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
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
    let raw = git::get_diff(&root, &source)?;
    let files = diff::parse(&raw)?;

    let drafts = review::load_drafts(&root)?;
    let mut state = AppState::new(source.clone(), source_label, files, drafts);

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

    let _ = review::save_drafts(&root, &state.drafts);

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
            Mode::Normal => handle_normal(state, &ev, root, base_sha, head_sha, &mut composer)?,
            Mode::Composing => handle_composing(state, &ev, &mut composer),
            Mode::Help => {
                if matches!(ev, Event::Key(_)) {
                    state.mode = Mode::Normal;
                }
            }
        }
        if state.should_quit {
            return Ok(());
        }
    }
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
        KeyCode::Char('c') => {
            if state.current_line().is_some() {
                let existing = state
                    .draft_for_cursor()
                    .and_then(|i| state.drafts.get(i))
                    .map(|d| d.body.clone());
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
            )?;
            review::save_drafts(root, &state.drafts)?;
            state.status = Some(format!("wrote {}", p.display()));
        }
        _ => {}
    }
    Ok(())
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
            } else if state.add_draft(body).is_some() {
                state.status = Some("draft saved (press S to submit to REVIEW.md)".into());
            }
            *composer = None;
            state.mode = Mode::Normal;
        }
        _ => {
            if let Some(ta) = composer {
                ta.input(*key);
            }
        }
    }
}
