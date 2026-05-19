use crate::app::{AppState, FlatKind, Mode};
use crate::diff::{FileStatus, LineKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_textarea::TextArea;

pub fn draw(f: &mut Frame, state: &mut AppState, composer: Option<&mut TextArea<'_>>) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    draw_header(f, chunks[0], state);
    state.viewport_height = chunks[1].height.saturating_sub(2) as usize;
    state.ensure_cursor_visible();
    draw_body(f, chunks[1], state);
    draw_footer(f, chunks[2], state);

    if state.mode == Mode::Composing {
        if let Some(ta) = composer {
            draw_composer(f, area, state, ta);
        }
    } else if state.mode == Mode::Help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, state: &AppState) {
    let title = format!(
        " gitdiff · {} · {} files · +{} −{} · {} drafts ",
        state.source_label,
        state.files.len(),
        state.total_additions,
        state.total_deletions,
        state.drafts.len()
    );
    let p = Paragraph::new(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let hint = match state.mode {
        Mode::Normal => {
            "j/k scroll · ]/[ file · }/{ hunk · c comment · x delete · S submit · ? help · q quit"
        }
        Mode::Composing => "ctrl-s save draft · esc cancel",
        Mode::Help => "any key to close help",
    };
    let status = state.status.clone().unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(hint, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default().borders(Borders::ALL).title(" Diff ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let from = state.scroll;
    let to = (state.scroll + inner.height as usize).min(state.flat.len());

    for i in from..to {
        let fl = &state.flat[i];
        let is_cursor = i == state.cursor;
        let line = match fl.kind {
            FlatKind::FileHeader => render_file_header(state, fl.file_idx),
            FlatKind::HunkHeader => render_hunk_header(state, fl.file_idx, fl.hunk_idx.unwrap()),
            FlatKind::Code => render_code_line(
                state,
                fl.file_idx,
                fl.hunk_idx.unwrap(),
                fl.line_idx.unwrap(),
            ),
            FlatKind::Spacer => Line::from(""),
        };
        let line = if is_cursor {
            highlight_line(line)
        } else {
            line
        };
        lines.push(line);
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn render_file_header(state: &AppState, fi: usize) -> Line<'static> {
    let f = &state.files[fi];
    let (badge, badge_style) = match f.status {
        FileStatus::Added => ("A", Style::default().fg(Color::Black).bg(Color::Green)),
        FileStatus::Modified => ("M", Style::default().fg(Color::Black).bg(Color::Yellow)),
        FileStatus::Deleted => ("D", Style::default().fg(Color::White).bg(Color::Red)),
        FileStatus::Renamed => ("R", Style::default().fg(Color::Black).bg(Color::Magenta)),
        FileStatus::Copied => ("C", Style::default().fg(Color::Black).bg(Color::Blue)),
    };
    let mut spans = vec![
        Span::styled(
            format!(" {badge} "),
            badge_style.add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if let Some(old) = &f.old_path {
        spans.push(Span::styled(
            format!("{} → ", old),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        f.path.clone(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("+{}", f.additions),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("−{}", f.deletions),
        Style::default().fg(Color::Red),
    ));
    if f.binary {
        spans.push(Span::styled(
            "  [binary]",
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn render_hunk_header(state: &AppState, fi: usize, hi: usize) -> Line<'static> {
    let h = &state.files[fi].hunks[hi];
    Line::from(Span::styled(
        format!("   {}", h.header_text()),
        Style::default().fg(Color::Cyan),
    ))
}

fn render_code_line(state: &AppState, fi: usize, hi: usize, li: usize) -> Line<'static> {
    let file = &state.files[fi];
    let hunk = &file.hunks[hi];
    let l = &hunk.lines[li];

    let has_draft = state.drafts.iter().any(|d| {
        d.file_path == file.path && d.old_lineno == l.old_lineno && d.new_lineno == l.new_lineno
    });

    let old_g = match l.old_lineno {
        Some(n) => format!("{:>4}", n),
        None => "    ".to_string(),
    };
    let new_g = match l.new_lineno {
        Some(n) => format!("{:>4}", n),
        None => "    ".to_string(),
    };
    let (sign, body_style, gutter_style) = match l.kind {
        LineKind::Added => (
            '+',
            Style::default().fg(Color::Green),
            Style::default().fg(Color::Green),
        ),
        LineKind::Deleted => (
            '-',
            Style::default().fg(Color::Red),
            Style::default().fg(Color::Red),
        ),
        LineKind::Context => (' ', Style::default(), Style::default().fg(Color::DarkGray)),
    };
    let mark = if has_draft { '◆' } else { ' ' };
    let mark_span = Span::styled(
        format!(" {mark} "),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    let spans = vec![
        mark_span,
        Span::styled(old_g, gutter_style),
        Span::raw(" "),
        Span::styled(new_g, gutter_style),
        Span::raw(" "),
        Span::styled(format!("{sign} "), body_style),
        Span::styled(l.content.clone(), body_style),
    ];
    Line::from(spans)
}

fn highlight_line(line: Line<'static>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let style = s.style.add_modifier(Modifier::REVERSED);
            Span::styled(s.content, style)
        })
        .collect();
    Line::from(spans)
}

fn draw_composer(f: &mut Frame, area: Rect, state: &AppState, ta: &mut TextArea<'_>) {
    let w = (area.width * 3 / 4).max(40);
    let h = (area.height / 2).max(8);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);

    let title = state
        .current_line()
        .map(|(file, _, line)| {
            let anchor = match (line.new_lineno, line.old_lineno) {
                (Some(n), _) => format!("L{n}"),
                (None, Some(o)) => format!("L{o}"),
                _ => "?".to_string(),
            };
            format!(" Comment on {}:{} ", file.path, anchor)
        })
        .unwrap_or_else(|| " Comment ".to_string());

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));
    ta.set_block(block);
    f.render_widget(&*ta, popup);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = (area.width * 3 / 4).max(50);
    let h = (area.height * 3 / 4).max(14);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            "Keybindings",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  j / ↓        move down one line"),
        Line::from("  k / ↑        move up one line"),
        Line::from("  ctrl-d       half page down"),
        Line::from("  ctrl-u       half page up"),
        Line::from("  g / G        top / bottom"),
        Line::from("  ]   /   [    next / prev file"),
        Line::from("  }   /   {    next / prev hunk"),
        Line::from(""),
        Line::from("  c            add / edit comment on current line"),
        Line::from("  x            delete comment on current line"),
        Line::from("  S            submit drafts → REVIEW.md at repo root"),
        Line::from(""),
        Line::from("  ?            toggle this help"),
        Line::from("  q            quit (drafts auto-persist to .gitdiff/drafts.json)"),
        Line::from(""),
        Line::from("In composer:"),
        Line::from("  ctrl-s       save draft"),
        Line::from("  esc          cancel"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}
