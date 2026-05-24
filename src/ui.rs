use crate::app::{
    self, AppState, ComposerTarget, FlatKind, HoverRegion, IntraRange, LineKey, Mode,
};
use crate::diff::{FileStatus, LineKind};
use crate::syntax::Span as HSpan;
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tui_textarea::TextArea;

/// Push a hover region for a timestamp at the given absolute screen position.
/// `col_start` and `len` are character counts, not byte offsets — the title
/// rendering uses `chars().count()` so this stays consistent.
fn register_timestamp_hover(
    state: &AppState,
    row: u16,
    col_start: usize,
    len: usize,
    created_at: &DateTime<Utc>,
) {
    if len == 0 {
        return;
    }
    let col_start = col_start.min(u16::MAX as usize) as u16;
    let col_end = (col_start as usize + len).min(u16::MAX as usize) as u16;
    state.hover_regions.borrow_mut().push(HoverRegion {
        row,
        col_start,
        col_end,
        tooltip: app::format_absolute_local(created_at),
    });
}

const ADD_BG: Color = Color::Rgb(20, 60, 30);
const DEL_BG: Color = Color::Rgb(80, 25, 25);
const ADD_INTRA_BG: Color = Color::Rgb(40, 130, 60);
const DEL_INTRA_BG: Color = Color::Rgb(160, 50, 50);
const HEADER_BG: Color = Color::Rgb(40, 44, 60);
const BORDER_FG: Color = Color::Rgb(80, 90, 110);

pub fn draw(f: &mut Frame, state: &mut AppState, composer: Option<&mut TextArea<'_>>) {
    // Hover regions are repopulated each frame by the render passes that draw
    // timestamps. Clear stale entries from the previous frame so dismissed UI
    // elements (e.g. scrolled-off comments) don't keep firing tooltips.
    state.hover_regions.borrow_mut().clear();
    // Keep the composer popup sized to its content (+ 2 border rows) so the
    // user sees the full comment while editing. Capped so it never overflows
    // the viewport.
    if state.mode == Mode::Composing {
        // Count display rows after wrapping: the composer renders text wrapped
        // so its height must reflect the wrapped row count, not just the
        // newline-separated line count.
        let inner_w = composer_inner_width(state) as usize;
        let display_rows = composer
            .as_ref()
            .map(|ta| {
                ta.lines()
                    .iter()
                    .map(|l| wrapped_row_count(l, inner_w))
                    .sum::<usize>()
                    .max(1)
            })
            .unwrap_or(1);
        let desired = (display_rows as u16).saturating_add(2);
        let cap = (state.viewport_height as u16)
            .saturating_sub(2)
            .max(COMPOSER_MIN_H);
        state.composer_height = desired.clamp(COMPOSER_MIN_H, cap);
    } else {
        state.composer_height = 0;
    }
    let area = f.area();
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_topbar(f, vert[0], state);

    // horizontal split: [tree?] [body] [threads?]
    let mut constraints: Vec<Constraint> = Vec::new();
    if state.show_tree {
        constraints.push(Constraint::Length(30));
    }
    constraints.push(Constraint::Min(20));
    if state.show_threads_pane {
        constraints.push(Constraint::Length(40));
    }
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(vert[1]);

    let mut idx = 0;
    if state.show_tree {
        draw_tree(f, h_chunks[idx], state);
        idx += 1;
    }
    let body_area = h_chunks[idx];
    idx += 1;
    if state.show_threads_pane {
        draw_threads_pane(f, h_chunks[idx], state);
    }

    state.viewport_height = body_area.height as usize;
    state.body_top = body_area.y;
    state.body_x = body_area.x;
    state.body_width = body_area.width;
    // Re-flatten on first frame and on resize so wrapped-thread row counts
    // match the body width currently in use.
    if state.flat_for_body_width != state.body_width {
        state.rebuild_flat();
        state.flat_for_body_width = state.body_width;
    }
    draw_body(f, body_area, state);
    draw_footer(f, vert[2], state);

    // Stale by default; the composer/menu draw paths below populate when shown.
    state.composer_rect = None;
    state.selection_menu_rect = None;

    if state.mode == Mode::Composing {
        if let Some(ta) = composer {
            let popup = composer_rect(body_area, state);
            state.composer_rect = Some((popup.x, popup.y, popup.width, popup.height));
            draw_composer(f, popup, state, ta);
        }
    } else if state.mode == Mode::Help {
        draw_help(f, area);
    } else if state.mode == Mode::Picker {
        draw_picker(f, area, state);
    } else if state.mode == Mode::Normal {
        // Floating [copy] [comment] menu for a finished drag selection.
        if let Some(sel) = state.selection {
            if !sel.dragging && sel.start != sel.end && !state.selection_lines().is_empty() {
                if let Some(rect) = draw_selection_menu(f, body_area, state) {
                    state.selection_menu_rect = Some((rect.x, rect.y, rect.width, rect.height));
                }
            }
        }
    }

    // Tooltip is drawn last so it floats over any other content. Only in
    // Normal mode — modal popups (composer/help/picker) take precedence.
    if state.mode == Mode::Normal {
        draw_hover_tooltip(f, area, state);
    }
}

/// If the mouse is currently over a registered hover region, draw the
/// region's tooltip as a single-line floating label near the cursor. The
/// label is rendered with `Clear` so it overlays the body cleanly.
fn draw_hover_tooltip(f: &mut Frame, area: Rect, state: &AppState) {
    let Some((mx, my)) = state.hover_pos else {
        return;
    };
    let regions = state.hover_regions.borrow();
    let Some(reg) = regions
        .iter()
        .find(|r| r.row == my && mx >= r.col_start && mx < r.col_end)
    else {
        return;
    };
    // `tooltip` already has the timezone suffix baked in; pad with one space
    // on each side for breathing room inside the highlight.
    let label = format!(" {} ", reg.tooltip);
    let label_w = label.chars().count() as u16;
    if label_w == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    // Prefer placing the tooltip on the row directly below the timestamp,
    // anchored so it starts at the timestamp's left edge. If we're against
    // the bottom of the viewport, flip it above instead.
    let tip_y = if reg.row + 1 < area.y + area.height {
        reg.row + 1
    } else {
        reg.row.saturating_sub(1)
    };
    let max_x = area.x + area.width.saturating_sub(label_w);
    let tip_x = reg.col_start.min(max_x);
    let tip_w = label_w.min(area.x + area.width - tip_x);
    let popup = Rect {
        x: tip_x,
        y: tip_y,
        width: tip_w,
        height: 1,
    };
    f.render_widget(Clear, popup);
    let style = Style::default()
        .fg(Color::Rgb(20, 24, 32))
        .bg(Color::Rgb(220, 200, 110))
        .add_modifier(Modifier::BOLD);
    let p = Paragraph::new(Line::from(Span::styled(label, style)));
    f.render_widget(p, popup);
}

fn draw_tree(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Files ")
        .border_style(Style::default().fg(BORDER_FG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let current_fi = state.flat.get(state.cursor).map(|fl| fl.file_idx);
    for (i, file) in state.files.iter().enumerate() {
        let viewed = state.viewed.contains_key(&file.path);
        let selected = current_fi == Some(i);
        let badge = match file.status {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Copied => "C",
        };
        let mark = if viewed { '✓' } else { ' ' };
        let path_short = shorten_path(&file.path, (inner.width as usize).saturating_sub(20));
        let path_style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(40, 70, 110))
                .add_modifier(Modifier::BOLD)
        } else if viewed {
            Style::default().fg(Color::Rgb(120, 130, 150))
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {mark} "), Style::default().fg(Color::Green)),
            Span::styled(
                format!("{badge} "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(path_short, path_style),
            Span::styled(
                format!("  +{}", file.additions),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" −{}", file.deletions),
                Style::default().fg(Color::Red),
            ),
        ]));
    }
    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn draw_threads_pane(f: &mut Frame, area: Rect, state: &AppState) {
    let title = format!(" Threads ({}) ", state.threads.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(BORDER_FG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.threads.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no threads yet — click a line or press c to comment",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
        f.render_widget(hint, inner);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, d) in state.threads.iter().enumerate() {
        let selected = i == state.threads_cursor;
        let anchor = d.anchor_label();
        let (status, status_color) = if d.resolved {
            ("✓ resolved", Color::Green)
        } else if d.outdated {
            ("! outdated", Color::Rgb(220, 160, 50))
        } else {
            ("◆ open", Color::Yellow)
        };
        let header_style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(60, 70, 100))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let path_short = shorten_path(&d.file_path, (inner.width as usize).saturating_sub(20));
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {status} "),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{path_short} · {anchor}"), header_style),
        ]));
        // body preview: first non-suggestion line
        let preview = d
            .body
            .lines()
            .find(|l| !l.trim_start().starts_with("```"))
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>();
        lines.push(Line::from(Span::styled(
            format!("    {preview}"),
            Style::default().fg(Color::Rgb(180, 180, 180)),
        )));
        if !d.reactions.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}", d.reactions.join(" ")),
                Style::default().fg(Color::Yellow),
            )));
        }
        // suggestion preview: render up to 3 lines as a mini-diff
        let suggestions = extract_suggestion_blocks(&d.body);
        for sug in suggestions.iter().take(1) {
            lines.push(Line::from(Span::styled(
                "    ┃ suggested change:",
                Style::default()
                    .fg(Color::Rgb(100, 180, 110))
                    .add_modifier(Modifier::ITALIC),
            )));
            for sline in sug.lines().take(3) {
                lines.push(Line::from(vec![
                    Span::styled("    ┃ ", Style::default().fg(Color::Rgb(100, 180, 110))),
                    Span::styled(
                        format!("+ {sline}"),
                        Style::default()
                            .fg(Color::Rgb(200, 230, 200))
                            .bg(Color::Rgb(20, 60, 30)),
                    ),
                ]));
            }
            if sug.lines().count() > 3 {
                lines.push(Line::from(Span::styled(
                    format!("    ┃   …{} more lines", sug.lines().count() - 3),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_picker(f: &mut Frame, area: Rect, state: &AppState) {
    let w = (area.width * 2 / 3)
        .max(50)
        .min(area.width.saturating_sub(4));
    let h = (area.height * 2 / 3)
        .max(12)
        .min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Jump to file ")
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 28, 40)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let picker = match &state.picker {
        Some(p) => p,
        None => return,
    };
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(" filter ")
        .border_style(Style::default().fg(Color::DarkGray));
    let input = Paragraph::new(Line::from(vec![
        Span::raw(picker.query.clone()),
        Span::styled("▎", Style::default().fg(Color::Cyan)),
    ]))
    .block(input_block);
    f.render_widget(input, parts[0]);

    let matches = state.filtered_files();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (rank, fi) in matches.iter().enumerate() {
        let file = &state.files[*fi];
        let sel = rank == picker.cursor;
        let style = if sel {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(60, 70, 100))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if sel { "▸ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(Color::Cyan)),
            Span::styled(file.path.clone(), style),
            Span::styled(
                format!("  +{}", file.additions),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" −{}", file.deletions),
                Style::default().fg(Color::Red),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), parts[1]);
}

fn extract_suggestion_blocks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if !in_block && trimmed.starts_with("```suggestion") {
            in_block = true;
            current.clear();
        } else if in_block && trimmed.starts_with("```") {
            in_block = false;
            out.push(std::mem::take(&mut current));
        } else if in_block {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    out
}

fn shorten_path(p: &str, width: usize) -> String {
    if p.chars().count() <= width {
        return p.to_string();
    }
    let take = width.saturating_sub(1);
    let skip = p.chars().count() - take;
    format!("…{}", p.chars().skip(skip).collect::<String>())
}

fn draw_topbar(f: &mut Frame, area: Rect, state: &AppState) {
    let open_threads = state.threads.iter().filter(|d| !d.resolved).count();
    let outdated = state.threads.iter().filter(|d| d.outdated).count();
    let outdated_str = if outdated > 0 {
        format!(" · {outdated} outdated")
    } else {
        String::new()
    };
    let title = format!(
        " gitdiff · {} · {} files ({}/{} viewed) · +{} −{} · {} threads{} · verdict: {} ",
        state.source_label,
        state.files.len(),
        state.viewed_count(),
        state.files.len(),
        state.total_additions,
        state.total_deletions,
        open_threads,
        outdated_str,
        state.verdict.label()
    );
    let p = Paragraph::new(Line::from(Span::styled(
        title,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let has_finished_selection = matches!(
        state.selection,
        Some(s) if !s.dragging && s.start != s.end
    );
    let hint = match state.mode {
        Mode::Normal if has_finished_selection => {
            "selection · c comment · y copy · esc clear · click elsewhere clears"
        }
        Mode::Normal => {
            "j/k · ]/[ file · }/{ hunk · n next-reply · e tree · t pick · R threads · v viewed · y yank · c comment · S submit · ? help · q quit"
        }
        Mode::Composing => match state.composer_target {
            Some(ComposerTarget::EditThread(_)) => {
                "enter save · shift-enter newline · ctrl-d delete · esc cancel"
            }
            _ => "enter save · shift-enter newline · esc cancel",
        },
        Mode::Help => "any key to close help",
        Mode::Picker => "type to filter · ↑↓ select · enter jump · esc cancel",
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
    // reserve one column on the right for the scrollbar
    let sb_w: u16 = 1;
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(sb_w),
        height: area.height,
    };
    let sb_area = Rect {
        x: area.x + area.width.saturating_sub(sb_w),
        y: area.y,
        width: sb_w,
        height: area.height,
    };

    // Compute the "gap" inserted below the selection when the composer is open
    // so the canvas grows instead of the popup overlapping text.
    let (gap_pos, gap_size) = composer_gap(state);
    let total_rows = state.flat.len() + gap_size;

    let width = content_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(content_area.height as usize);
    let from = state.scroll;
    let to = (state.scroll + content_area.height as usize).min(total_rows);

    let sel_range = state.selection.map(|s| s.range());
    for ext_i in from..to {
        // map extended-canvas index to flat[] index, accounting for the gap
        if gap_size > 0 && ext_i >= gap_pos && ext_i < gap_pos + gap_size {
            lines.push(Line::from(""));
            continue;
        }
        let i = if ext_i >= gap_pos + gap_size {
            ext_i - gap_size
        } else {
            ext_i
        };
        if i >= state.flat.len() {
            break;
        }
        let fl = &state.flat[i];
        let line = match fl.kind {
            FlatKind::FileHeader => render_file_header(state, fl.file_idx, width),
            FlatKind::FileFooter => render_file_footer(width),
            FlatKind::HunkHeader => {
                render_hunk_header(state, fl.file_idx, fl.hunk_idx.unwrap(), width)
            }
            FlatKind::Code => render_code_line(
                state,
                fl.file_idx,
                fl.hunk_idx.unwrap(),
                fl.line_idx.unwrap(),
                width,
            ),
            FlatKind::ExpandedAbove | FlatKind::ExpandedBelow => render_expanded_line(
                state,
                fl.file_idx,
                fl.hunk_idx.unwrap(),
                fl.line_idx.unwrap(),
                fl.kind == FlatKind::ExpandedAbove,
                width,
            ),
            FlatKind::ExpandBtnAbove | FlatKind::ExpandBtnBelow => render_expand_btn(
                fl.kind == FlatKind::ExpandBtnAbove,
                fl.line_idx.unwrap_or(20),
                width,
            ),
            FlatKind::ThreadRow => {
                let screen_row = content_area.y + (ext_i - from) as u16;
                render_thread_row(
                    state,
                    fl.thread_idx.unwrap_or(0),
                    fl.line_idx.unwrap_or(0),
                    width,
                    screen_row,
                    content_area.x,
                )
            }
            FlatKind::Spacer => Line::from(""),
        };
        let in_selection = sel_range.map(|(a, b)| i >= a && i <= b).unwrap_or(false);
        let is_cursor_solo = sel_range.is_none() && state.cursor_visible && i == state.cursor;
        let line = if in_selection || is_cursor_solo {
            highlight_line(line)
        } else {
            line
        };
        lines.push(line);
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, content_area);
    draw_scrollbar(
        f,
        sb_area,
        state.scroll,
        total_rows,
        content_area.height as usize,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMenuAction {
    Copy,
    Comment,
    Cancel,
}

const SELECTION_MENU_BUTTONS: &[(&str, SelectionMenuAction)] = &[
    ("copy", SelectionMenuAction::Copy),
    ("comment", SelectionMenuAction::Comment),
    ("cancel", SelectionMenuAction::Cancel),
];

const SELECTION_MENU_GAP: u16 = 2;

/// Returns `(total_width, [(x_offset_from_start, width, action), ...])` for
/// the floating copy/comment/cancel buttons. Each button reads `[ word ]`.
fn selection_menu_layout() -> (u16, Vec<(u16, u16, SelectionMenuAction)>) {
    let mut x: u16 = 0;
    let mut out = Vec::new();
    for (i, (word, action)) in SELECTION_MENU_BUTTONS.iter().enumerate() {
        if i > 0 {
            x += SELECTION_MENU_GAP;
        }
        let w = (word.len() + 4) as u16;
        out.push((x, w, *action));
        x += w;
    }
    (x, out)
}

/// Maps a screen click to a button on the floating selection menu, if any.
pub fn selection_menu_hit(state: &AppState, col: u16, row: u16) -> Option<SelectionMenuAction> {
    let (mx, my, _mw, _mh) = state.selection_menu_rect?;
    if row != my {
        return None;
    }
    let (_, buttons) = selection_menu_layout();
    for (off, w, action) in buttons {
        let bx = mx + off;
        if col >= bx && col < bx + w {
            return Some(action);
        }
    }
    None
}

fn draw_selection_menu(f: &mut Frame, body: Rect, state: &AppState) -> Option<Rect> {
    let sel = state.selection?;
    let (_, end) = sel.range();
    let (total_width, buttons) = selection_menu_layout();
    if total_width + 1 >= body.width {
        return None;
    }
    let x = body.x + body.width.saturating_sub(total_width + 1);
    // Prefer one row below the selection end; clamp to body bottom.
    let row_below = if end >= state.scroll {
        body.y
            .saturating_add((end - state.scroll) as u16)
            .saturating_add(1)
    } else {
        body.y
    };
    let max_y = body.y + body.height.saturating_sub(1);
    let y = row_below.min(max_y);
    let rect = Rect {
        x,
        y,
        width: total_width,
        height: 1,
    };

    f.render_widget(Clear, rect);
    let bg = Color::Rgb(40, 50, 70);
    let border_style = Style::default().fg(Color::Yellow).bg(bg);
    let text_style = Style::default()
        .fg(Color::Rgb(220, 230, 245))
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let gap_style = Style::default().bg(Color::Reset);

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, ((word, _), _)) in SELECTION_MENU_BUTTONS
        .iter()
        .zip(buttons.iter())
        .enumerate()
    {
        if i > 0 {
            spans.push(Span::styled(
                " ".repeat(SELECTION_MENU_GAP as usize),
                gap_style,
            ));
        }
        spans.push(Span::styled("[ ", border_style));
        spans.push(Span::styled((*word).to_string(), text_style));
        spans.push(Span::styled(" ]", border_style));
    }
    let para = Paragraph::new(Line::from(spans));
    f.render_widget(para, rect);
    Some(rect)
}

/// Returns (extended_position, height) of the composer-induced gap.
/// `(0, 0)` when the composer is not open.
///
/// For `NewThread` / `EditThread` the gap sits right after the *last* selected
/// line — the thread is hidden so the composer takes its slot. For reply
/// targets the thread stays visible and the gap is placed just below the
/// thread's bottom border so the user can still read the conversation while
/// they type.
fn composer_gap(state: &AppState) -> (usize, usize) {
    if state.mode != Mode::Composing {
        return (0, 0);
    }
    let h = state.composer_height.max(COMPOSER_MIN_H) as usize;
    // Reply targets: find the last ThreadRow of the attached thread and place
    // the gap right after it.
    if let Some(
        ComposerTarget::NewReply(idx)
        | ComposerTarget::EditReply {
            thread_idx: idx, ..
        },
    ) = state.composer_target
    {
        if let Some(last) = state.flat.iter().enumerate().rev().find_map(|(i, fl)| {
            (fl.kind == FlatKind::ThreadRow && fl.thread_idx == Some(idx)).then_some(i)
        }) {
            return (last + 1, h);
        }
    }
    let anchor = state.selection.map(|s| s.range().1).unwrap_or(state.cursor);
    (anchor + 1, h)
}

fn draw_scrollbar(f: &mut Frame, area: Rect, scroll: usize, total: usize, visible_h: usize) {
    let track_h = area.height as usize;
    if track_h == 0 || area.width == 0 {
        return;
    }
    // Paint the cell BACKGROUND rather than rely on a glyph (`█`) — many
    // terminals add line-spacing padding between rows that no character can
    // cover, but the cell bg fills the entire cell including that padding.
    // We render a literal space so there's no font glyph involved at all.
    let track_style = Style::default().bg(Color::Rgb(38, 44, 58));
    let thumb_style = Style::default().bg(Color::Rgb(150, 170, 210));

    let (thumb_top, thumb_h) = if total <= visible_h || total == 0 {
        (0, track_h)
    } else {
        let h = ((track_h * visible_h) / total).max(1).min(track_h);
        let max_scroll = total - visible_h;
        let span = track_h - h;
        let top = if max_scroll == 0 {
            0
        } else {
            (scroll * span + max_scroll / 2) / max_scroll
        };
        (top.min(span), h)
    };

    let lines: Vec<Line<'static>> = (0..track_h)
        .map(|i| {
            let in_thumb = i >= thumb_top && i < thumb_top + thumb_h;
            let style = if in_thumb { thumb_style } else { track_style };
            Line::from(Span::styled(" ", style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn render_file_header(state: &AppState, fi: usize, width: usize) -> Line<'static> {
    let f = &state.files[fi];
    let expanded = state.expanded.get(fi).copied().unwrap_or(true);
    let viewed = state.viewed.contains_key(&f.path);
    let chevron = if expanded { '▾' } else { '▸' };

    let (badge, badge_style) = match f.status {
        FileStatus::Added => ("A", Style::default().fg(Color::Black).bg(Color::Green)),
        FileStatus::Modified => ("M", Style::default().fg(Color::Black).bg(Color::Yellow)),
        FileStatus::Deleted => ("D", Style::default().fg(Color::White).bg(Color::Red)),
        FileStatus::Renamed => ("R", Style::default().fg(Color::Black).bg(Color::Magenta)),
        FileStatus::Copied => ("C", Style::default().fg(Color::Black).bg(Color::Blue)),
    };

    let header_bg = if viewed {
        Color::Rgb(28, 32, 42)
    } else {
        HEADER_BG
    };
    let bg = Style::default().bg(header_bg);
    let border = Style::default().fg(BORDER_FG).bg(header_bg);
    let path_fg = if viewed {
        Color::Rgb(120, 130, 150)
    } else {
        Color::White
    };

    let mut spans = vec![
        Span::styled("╭".to_string(), border),
        Span::styled(format!(" {chevron} "), bg.fg(Color::White)),
        Span::styled(
            format!(" {badge} "),
            badge_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", f.hunks.len()),
            bg.fg(Color::Rgb(150, 160, 180)),
        ),
    ];
    if let Some(old) = &f.old_path {
        spans.push(Span::styled(format!("{old} → "), bg.fg(Color::DarkGray)));
    }
    spans.push(Span::styled(
        f.path.clone(),
        bg.fg(path_fg).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("  ", bg));
    spans.push(Span::styled(
        format!("+{}", f.additions),
        bg.fg(Color::Green),
    ));
    spans.push(Span::styled(" ", bg));
    spans.push(Span::styled(format!("−{}", f.deletions), bg.fg(Color::Red)));
    if f.binary {
        spans.push(Span::styled(" [binary]", bg.fg(Color::DarkGray)));
    }

    // right-aligned "viewed" indicator: pad first, then the badge
    let viewed_badge = if viewed {
        " ✓ viewed "
    } else {
        " ☐ viewed "
    };
    let viewed_style = if viewed {
        bg.fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        bg.fg(Color::DarkGray)
    };

    let used = visible_width(&spans);
    let badge_w = viewed_badge.chars().count();
    let pad = width.saturating_sub(used + badge_w + 1);
    spans.push(Span::styled(" ".repeat(pad), bg));
    spans.push(Span::styled(viewed_badge.to_string(), viewed_style));
    spans.push(Span::styled("╮".to_string(), border));
    Line::from(spans)
}

fn render_file_footer(width: usize) -> Line<'static> {
    let border = Style::default().fg(BORDER_FG);
    let inner_w = width.saturating_sub(2);
    Line::from(vec![
        Span::styled("╰".to_string(), border),
        Span::styled("─".repeat(inner_w), border),
        Span::styled("╯".to_string(), border),
    ])
}

fn render_hunk_header(state: &AppState, fi: usize, hi: usize, width: usize) -> Line<'static> {
    let h = &state.files[fi].hunks[hi];
    let collapsed = state.collapsed_hunks.contains(&(fi, hi));
    let chevron = if collapsed { '▸' } else { '▾' };
    let border = Style::default().fg(BORDER_FG);
    let body = Style::default()
        .fg(Color::Rgb(120, 170, 200))
        .bg(Color::Rgb(30, 40, 60))
        .add_modifier(Modifier::ITALIC);
    let hint = if collapsed {
        format!("  ({} lines)", h.lines.len())
    } else {
        String::new()
    };
    let text = format!(" {chevron} {}{} ", h.header_text(), hint);
    let pad = width.saturating_sub(text.chars().count() + 2).max(0);
    Line::from(vec![
        Span::styled("│".to_string(), border),
        Span::styled(text, body),
        Span::styled(" ".repeat(pad), body),
        Span::styled("│".to_string(), border),
    ])
}

fn render_expand_btn(above: bool, count: usize, width: usize) -> Line<'static> {
    let border = Style::default().fg(BORDER_FG);
    let bg = Color::Rgb(28, 36, 50);
    let body = Style::default().bg(bg);
    let glyph_fg = Color::Rgb(180, 200, 230);
    let label_fg = Color::Rgb(150, 170, 200);
    let chevron = if above { "▲" } else { "▼" };
    let action = if count > 20 {
        "expand all"
    } else if above {
        "expand 20 above"
    } else {
        "expand 20 below"
    };
    let label = if count <= 20 && count > 0 && count < 20 {
        format!("  {chevron} expand {count}  ")
    } else if count > 20 {
        format!("  ▲▼ expand {count}  ")
    } else {
        format!("  {chevron} {action}  ")
    };
    let _ = (glyph_fg, label_fg);
    let pad = width.saturating_sub(label.chars().count() + 2);
    Line::from(vec![
        Span::styled("│".to_string(), border),
        Span::styled(
            label,
            body.fg(Color::Rgb(180, 200, 230))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(pad), body),
        Span::styled("│".to_string(), border),
    ])
}

fn render_code_line(
    state: &AppState,
    fi: usize,
    hi: usize,
    li: usize,
    width: usize,
) -> Line<'static> {
    let file = &state.files[fi];
    let hunk = &file.hunks[hi];
    let l = &hunk.lines[li];

    let cover = state.thread_covering_line(fi, l);
    let thread_at = cover.and_then(|(idx, is_anchor)| {
        if is_anchor {
            state.threads.get(idx)
        } else {
            None
        }
    });
    let in_range = cover.is_some();

    let (bg, sign_color) = match l.kind {
        LineKind::Added => (ADD_BG, Color::Green),
        LineKind::Deleted => (DEL_BG, Color::Red),
        LineKind::Context => (Color::Reset, Color::DarkGray),
    };
    let bg_style = Style::default().bg(bg);
    // In-range lines wear yellow borders so a multi-line thread visually frames
    // the lines it covers, joining up with the embedded thread box.
    let range_thread = cover.and_then(|(idx, _)| state.threads.get(idx));
    let border_color = match range_thread {
        Some(d) if d.resolved => Color::Green,
        Some(d) if d.outdated => Color::Rgb(220, 160, 50),
        Some(d) if app::needs_attention(d) => Color::Rgb(200, 130, 230),
        Some(_) => Color::Yellow,
        None => BORDER_FG,
    };
    let border = Style::default().fg(border_color);

    let old_g = match l.old_lineno {
        Some(n) => format!("{n:>4}"),
        None => "    ".to_string(),
    };
    let new_g = match l.new_lineno {
        Some(n) => format!("{n:>4}"),
        None => "    ".to_string(),
    };
    let sign = match l.kind {
        LineKind::Added => '+',
        LineKind::Deleted => '-',
        LineKind::Context => ' ',
    };
    // Anchor row gets the status glyph; non-anchor rows in the range get a
    // continuation bar so the eye tracks the run from the anchor down.
    let mark = match (thread_at, in_range) {
        (Some(d), _) if d.resolved => '✓',
        (Some(d), _) if d.outdated => '!',
        (Some(_), _) => '◆',
        (None, true) => '┃',
        (None, false) => ' ',
    };
    let mark_color = border_color;

    let mut spans = vec![
        Span::styled("│".to_string(), border),
        Span::styled(
            format!(" {mark} "),
            bg_style.fg(mark_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(old_g, bg_style.fg(Color::Rgb(110, 120, 140))),
        Span::styled(" ", bg_style),
        Span::styled(new_g, bg_style.fg(Color::Rgb(110, 120, 140))),
        Span::styled(
            format!(" {sign} "),
            bg_style.fg(sign_color).add_modifier(Modifier::BOLD),
        ),
    ];

    // syntax-highlighted code content (with tab expansion + intra-line emphasis)
    let key: LineKey = (fi, hi, li);
    let tab_str = " ".repeat(state.tab_width);
    let mut code_spans: Vec<Span<'static>> = Vec::new();
    if let Some(hspans) = state.highlights.get(&key) {
        for hs in hspans {
            let mut s = hspan_to_ratatui(hs, bg);
            if s.content.contains('\t') {
                let new_content = s.content.replace('\t', &tab_str);
                s = Span::styled(new_content, s.style);
            }
            code_spans.push(s);
        }
    } else {
        code_spans.push(Span::styled(l.content.replace('\t', &tab_str), bg_style));
    }

    // apply intra-line emphasis (brighter bg + bold on the differing middle)
    if let Some(intra) = state.intraline.get(&key) {
        let intra_bg = match l.kind {
            LineKind::Added => ADD_INTRA_BG,
            LineKind::Deleted => DEL_INTRA_BG,
            LineKind::Context => bg,
        };
        code_spans = apply_intraline(code_spans, *intra, intra_bg);
    }
    spans.extend(code_spans);

    let used = visible_width(&spans);
    let pad = width.saturating_sub(used + 1);
    spans.push(Span::styled(" ".repeat(pad), bg_style));
    spans.push(Span::styled("│".to_string(), border));
    Line::from(spans)
}

fn render_expanded_line(
    state: &AppState,
    fi: usize,
    hi: usize,
    li: usize,
    above: bool,
    width: usize,
) -> Line<'static> {
    let exp = match state.expansions.get(&(fi, hi)) {
        Some(e) => e,
        None => return Line::from(""),
    };
    let l = if above {
        exp.above.get(li)
    } else {
        exp.below.get(li)
    };
    let l = match l {
        Some(l) => l,
        None => return Line::from(""),
    };
    // muted bg distinguishes expanded context from regular diff body
    const EXPANDED_BG: Color = Color::Rgb(24, 28, 36);
    let bg = EXPANDED_BG;
    let bg_style = Style::default().bg(bg);
    let border = Style::default().fg(BORDER_FG);

    let old_g = l
        .old_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    let new_g = l
        .new_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    let tab_str = " ".repeat(state.tab_width);
    let content = l.content.replace('\t', &tab_str);

    let mut spans = vec![
        Span::styled("│".to_string(), border),
        Span::styled("   ", bg_style),
        Span::styled(old_g, bg_style.fg(Color::Rgb(90, 100, 120))),
        Span::styled(" ", bg_style),
        Span::styled(new_g, bg_style.fg(Color::Rgb(90, 100, 120))),
        Span::styled("   ", bg_style),
        Span::styled(content, bg_style.fg(Color::Rgb(160, 170, 190))),
    ];
    let used = visible_width(&spans);
    let pad = width.saturating_sub(used + 1);
    spans.push(Span::styled(" ".repeat(pad), bg_style));
    spans.push(Span::styled("│".to_string(), border));
    Line::from(spans)
}

/// Renders a thread as a read-only mirror of the composer Block: yellow border,
/// dark-blue interior, title on the top edge. Clicking transitions to the real
/// composer, so the visual layout is unchanged — just a cursor appears.
///
/// `screen_row` / `origin_x` describe where this sub-row lands on screen so we
/// can register hover regions over the timestamp text (the absolute date+tz
/// pops in a tooltip when the mouse is over the "5m ago" label).
fn render_thread_row(
    state: &AppState,
    thread_idx: usize,
    sub: usize,
    width: usize,
    screen_row: u16,
    origin_x: u16,
) -> Line<'static> {
    let d = match state.threads.get(thread_idx) {
        Some(d) => d,
        None => return Line::from(""),
    };
    // Threads whose last message isn't yours get a brighter purple frame so
    // they stand out as "your turn". Other states keep the yellow frame.
    let attn = app::needs_attention(d);
    const BOX_BG: Color = Color::Rgb(20, 28, 40);
    const ATTN_BOX_BG: Color = Color::Rgb(36, 24, 48);
    let box_bg = if attn { ATTN_BOX_BG } else { BOX_BG };
    let frame_fg = if attn {
        Color::Rgb(200, 130, 230)
    } else {
        Color::Yellow
    };
    let border = Style::default().fg(frame_fg).bg(box_bg);
    let bg = Style::default().bg(box_bg);
    let text = bg.fg(Color::Rgb(220, 230, 245));
    let muted = bg.fg(Color::Rgb(150, 160, 180));

    // Box must be at least 2 cols wide (for the two corner glyphs).
    let inner_w = width.saturating_sub(2);
    let text_w = inner_w.saturating_sub(1);
    // Pre-wrap body so each display row is its own bordered sub-row; this is
    // the same wrap rebuild_flat uses to size the box, so they always agree.
    let body_lines = app::wrap_body(&d.body, text_w);
    let body_count = body_lines.len().max(1);
    // Per-reply pre-wrap; each reply produces `1 + wrapped_lines` sub-rows
    // (1 header divider, then wrapped body).
    let reply_wraps: Vec<Vec<String>> = d
        .replies
        .iter()
        .map(|r| app::wrap_body(&r.body, text_w))
        .collect();
    let reply_rows: usize = reply_wraps.iter().map(|w| 1 + w.len().max(1)).sum();
    let has_react = !d.reactions.is_empty();
    let total = 2 + body_count + reply_rows + if has_react { 1 } else { 0 };
    let bottom_idx = total - 1;

    // Top edge with title — mirrors a ratatui Block's title placement.
    if sub == 0 {
        let (status_txt, status_color) = if d.resolved {
            ("✓ resolved", Color::Green)
        } else if d.outdated {
            ("! outdated", Color::Rgb(220, 160, 50))
        } else {
            ("◆ open", Color::Yellow)
        };
        let now = Utc::now();
        let ts = app::format_relative_time(&d.created_at, &now);
        let title_prefix = " ";
        let pre = " · ";
        let post = " ";
        // Columns: "┌" + " " + status_txt + " · " + ts + " " ...
        let ts_col_start = origin_x as usize
            + 1
            + title_prefix.chars().count()
            + status_txt.chars().count()
            + pre.chars().count();
        let ts_len = ts.chars().count();
        register_timestamp_hover(state, screen_row, ts_col_start, ts_len, &d.created_at);
        let title_suffix = format!("{pre}{ts}{post}");
        let title_chars = title_prefix.chars().count()
            + status_txt.chars().count()
            + title_suffix.chars().count();
        let dashes = inner_w.saturating_sub(title_chars);
        return Line::from(vec![
            Span::styled("┌".to_string(), border),
            Span::styled(title_prefix.to_string(), bg),
            Span::styled(
                status_txt.to_string(),
                bg.fg(status_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(title_suffix, muted.add_modifier(Modifier::ITALIC)),
            Span::styled("─".repeat(dashes), border),
            Span::styled("┐".to_string(), border),
        ]);
    }

    // Bottom edge.
    if sub == bottom_idx {
        return Line::from(vec![
            Span::styled("└".to_string(), border),
            Span::styled("─".repeat(inner_w), border),
            Span::styled("┘".to_string(), border),
        ]);
    }

    // Sub layout (after top edge at sub=0):
    //   1..=body_count                              → body wrapped row
    //   body_count+1..body_count+reply_rows         → reply header + wrapped body
    //   the trailing reactions row (if present)     → just before bottom_idx
    //   bottom_idx                                  → bottom edge
    let mut idx_in_body = sub.saturating_sub(1);

    if idx_in_body < body_count {
        let bi = idx_in_body;
        let txt = body_lines.get(bi).map(|s| s.as_str()).unwrap_or("");
        let label = format!(" {txt}");
        let used = label.chars().count();
        let pad = inner_w.saturating_sub(used);
        return Line::from(vec![
            Span::styled("│".to_string(), border),
            Span::styled(label, text),
            Span::styled(" ".repeat(pad), bg),
            Span::styled("│".to_string(), border),
        ]);
    }
    idx_in_body -= body_count;

    // Walk replies to find the (reply_idx, row_within_reply) for this sub.
    let now = Utc::now();
    for (ri, wrap) in reply_wraps.iter().enumerate() {
        let block = 1 + wrap.len().max(1);
        if idx_in_body < block {
            let reply = &d.replies[ri];
            if idx_in_body == 0 {
                // Reply header: `├─ @author · 5m ago ─...─┤`
                let ts = app::format_relative_time(&reply.created_at, &now);
                let prefix = format!(" ↳ @{} · ", reply.author);
                let header = format!("{prefix}{ts} ");
                // ts column: 1 ("├") + chars(prefix). Total absolute column on
                // screen needs origin_x.
                let ts_col_start = origin_x as usize + 1 + prefix.chars().count();
                let ts_len = ts.chars().count();
                register_timestamp_hover(
                    state,
                    screen_row,
                    ts_col_start,
                    ts_len,
                    &reply.created_at,
                );
                let header_chars = header.chars().count();
                let dashes = inner_w.saturating_sub(header_chars);
                return Line::from(vec![
                    Span::styled("├".to_string(), border),
                    Span::styled(
                        header,
                        muted.add_modifier(Modifier::ITALIC | Modifier::BOLD),
                    ),
                    Span::styled("─".repeat(dashes), border),
                    Span::styled("┤".to_string(), border),
                ]);
            }
            let row = idx_in_body - 1;
            let txt = wrap.get(row).map(|s| s.as_str()).unwrap_or("");
            let label = format!(" {txt}");
            let used = label.chars().count();
            let pad = inner_w.saturating_sub(used);
            return Line::from(vec![
                Span::styled("│".to_string(), border),
                Span::styled(label, text),
                Span::styled(" ".repeat(pad), bg),
                Span::styled("│".to_string(), border),
            ]);
        }
        idx_in_body -= block;
    }

    // Reactions row (only reachable when has_react == true).
    let label = format!(" {}", d.reactions.join(" "));
    let used = label.chars().count();
    let pad = inner_w.saturating_sub(used);
    Line::from(vec![
        Span::styled("│".to_string(), border),
        Span::styled(label, text),
        Span::styled(" ".repeat(pad), bg),
        Span::styled("│".to_string(), border),
    ])
}

fn apply_intraline(
    spans: Vec<Span<'static>>,
    intra: IntraRange,
    emphasis_bg: Color,
) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= intra.prefix + intra.suffix {
        return spans;
    }
    let start = intra.prefix;
    let end = total - intra.suffix;
    let mut out = Vec::with_capacity(spans.len() + 4);
    let mut col = 0usize;
    for s in spans {
        let len = s.content.chars().count();
        let span_start = col;
        let span_end = col + len;
        if span_end <= start || span_start >= end {
            out.push(s);
        } else {
            let chars: Vec<char> = s.content.chars().collect();
            let split_a = start.saturating_sub(span_start).min(len);
            let split_b = end.saturating_sub(span_start).min(len);
            if split_a > 0 {
                let pre: String = chars[..split_a].iter().collect();
                out.push(Span::styled(pre, s.style));
            }
            if split_b > split_a {
                let mid: String = chars[split_a..split_b].iter().collect();
                let bold_style = s.style.bg(emphasis_bg).add_modifier(Modifier::BOLD);
                out.push(Span::styled(mid, bold_style));
            }
            if split_b < len {
                let post: String = chars[split_b..].iter().collect();
                out.push(Span::styled(post, s.style));
            }
        }
        col = span_end;
    }
    out
}

fn hspan_to_ratatui(hs: &HSpan, bg: Color) -> Span<'static> {
    let mut style = Style::default()
        .fg(Color::Rgb(hs.fg.0, hs.fg.1, hs.fg.2))
        .bg(bg);
    if hs.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if hs.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    Span::styled(hs.text.clone(), style)
}

fn visible_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

const SELECTION_BG: Color = Color::Rgb(50, 80, 135);

/// Initial / minimum height of the inline composer popup (rows, including
/// the 2 border rows). 3 = top border + 1 content row + bottom border, which
/// matches the read-only thread view's height for a single-line comment. The
/// height grows with the body content so the full comment fits.
pub const COMPOSER_MIN_H: u16 = 3;

fn highlight_line(line: Line<'static>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            // Replace bg with a uniform selection color while preserving the
            // foreground (so syntect highlights stay readable). Reversing would
            // swap fg/bg per span and make code look like it falls into the bg.
            let style = s.style.bg(SELECTION_BG);
            Span::styled(s.content, style)
        })
        .collect();
    Line::from(spans)
}

fn composer_rect(body: Rect, state: &AppState) -> Rect {
    let (gap_pos, gap_h) = composer_gap(state);
    let h = (gap_h as u16).min(body.height.max(1));
    // gap_pos is in extended-canvas coords; subtract scroll to get screen row.
    let screen_row = gap_pos.saturating_sub(state.scroll) as u16;
    let y = (body.y + screen_row).min(body.y + body.height.saturating_sub(h));
    // composer occupies the diff body's width minus the scrollbar column
    let w = body.width.saturating_sub(1).max(20);
    Rect {
        x: body.x,
        y,
        width: w,
        height: h,
    }
}

/// Inner content width of the composer popup. Matches `composer_rect` width
/// minus 2 borders and 2 padding columns (`Padding::horizontal(1)`).
pub fn composer_inner_width(state: &AppState) -> u16 {
    let total = state.body_width.saturating_sub(1).max(20);
    total.saturating_sub(4)
}

/// Maps a click on display row/col inside the wrapped composer back to a
/// logical (row, col) the textarea understands. Clicks past the rendered text
/// snap to the end of the closest logical line.
pub fn composer_screen_to_logical(
    ta: &TextArea<'_>,
    inner_w: usize,
    local_row: u16,
    local_col: u16,
) -> (u16, u16) {
    let w = inner_w.max(1);
    let target = local_row as usize;
    let mut display_row = 0usize;
    for (li, line) in ta.lines().iter().enumerate() {
        let n = line.chars().count();
        let rows = if n == 0 { 1 } else { (n + w - 1) / w };
        if target < display_row + rows {
            let r = target - display_row;
            let start = r * w;
            let row_len = if n == 0 {
                0
            } else if r + 1 == rows {
                n - start
            } else {
                w
            };
            let col = (local_col as usize).min(row_len);
            return (li as u16, (start + col) as u16);
        }
        display_row += rows;
    }
    let lines = ta.lines();
    let last_li = lines.len().saturating_sub(1) as u16;
    let last_col = lines.last().map(|l| l.chars().count()).unwrap_or(0) as u16;
    (last_li, last_col)
}

/// How many display rows `line` occupies after wrapping into `max_w` chars.
/// Empty lines and the trailing edge of `max_w`-aligned lines still take one
/// row.
fn wrapped_row_count(line: &str, max_w: usize) -> usize {
    if max_w == 0 {
        return 1;
    }
    let n = line.chars().count();
    if n == 0 { 1 } else { (n + max_w - 1) / max_w }
}

const COMPOSER_BG: Color = Color::Rgb(20, 28, 40);

fn draw_composer(f: &mut Frame, popup: Rect, state: &AppState, ta: &mut TextArea<'_>) {
    f.render_widget(Clear, popup);

    let title = build_composer_title(state);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(COMPOSER_BG))
        // Match the read-only thread view, which leaves a 1-col gap between
        // the border and the body text.
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let max_w = inner.width as usize;
    let max_h = inner.height as usize;
    // Wrap each logical line; for each display row, remember which logical
    // line (`li`) and starting column (`start`) it represents. We need this
    // mapping to translate the textarea's logical cursor into a display
    // (row, col) and to overlay the selection.
    let mut display: Vec<String> = Vec::new();
    let mut row_map: Vec<(usize, usize)> = Vec::new();
    for (li, line) in ta.lines().iter().enumerate() {
        if line.is_empty() {
            display.push(String::new());
            row_map.push((li, 0));
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let end = (i + max_w).min(chars.len());
            display.push(chars[i..end].iter().collect());
            row_map.push((li, i));
            i = end;
        }
    }
    if display.is_empty() {
        display.push(String::new());
        row_map.push((0, 0));
    }

    // Where is the cursor on the wrapped canvas?
    let (cr, cc) = ta.cursor();
    let (cursor_row, cursor_col) = locate_cursor(&row_map, &display, cr, cc, max_w);

    // Scroll so the cursor row stays inside the visible inner area.
    let scroll = if cursor_row >= max_h {
        cursor_row + 1 - max_h
    } else {
        0
    };

    let sel = ta.selection_range();
    let sel_style = ta.selection_style();
    let text_style = ta.style();

    let from = scroll;
    let to = (scroll + max_h).min(display.len());
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(to - from);
    for di in from..to {
        lines.push(render_composer_row(
            &display[di],
            row_map[di],
            sel,
            sel_style,
            text_style,
        ));
    }
    let para = Paragraph::new(lines).style(text_style.bg(COMPOSER_BG));
    f.render_widget(para, inner);

    // Place the terminal cursor — tui-textarea's own widget normally does this;
    // since we render our own, we have to position it ourselves.
    if cursor_row >= scroll && cursor_row - scroll < max_h {
        let cx = inner.x + cursor_col.min((max_w.saturating_sub(1)).max(0)) as u16;
        let cy = inner.y + (cursor_row - scroll) as u16;
        f.set_cursor_position((cx, cy));
    }
}

/// Maps a logical `(cr, cc)` cursor onto the wrapped display canvas.
fn locate_cursor(
    row_map: &[(usize, usize)],
    display: &[String],
    cr: usize,
    cc: usize,
    max_w: usize,
) -> (usize, usize) {
    if max_w == 0 || row_map.is_empty() {
        return (0, 0);
    }
    // Walk the rows belonging to logical line `cr`; pick the one whose
    // [start, start+len] range contains `cc`. If the cursor sits past the last
    // chunk (e.g. just-typed character at end), put it at the trailing edge.
    let mut last_for_line: Option<usize> = None;
    for (di, (li, start)) in row_map.iter().enumerate() {
        if *li != cr {
            continue;
        }
        last_for_line = Some(di);
        let row_chars = display[di].chars().count();
        if cc >= *start && cc <= *start + row_chars {
            // Cursor lands inside this row, or just at its end.
            // If the cursor is exactly at start+max_w (we filled the row),
            // and there's another row for this logical line that begins where
            // we ended, prefer placing the cursor at column 0 of the next row.
            let col = cc - *start;
            if col >= max_w {
                if let Some((nli, _)) = row_map.get(di + 1) {
                    if *nli == cr {
                        return (di + 1, 0);
                    }
                }
            }
            return (di, col);
        }
    }
    if let Some(di) = last_for_line {
        let col = display[di].chars().count();
        return (di, col);
    }
    (0, 0)
}

/// Builds one display row with optional selection highlighting.
fn render_composer_row(
    text: &str,
    row_meta: (usize, usize),
    sel: Option<((usize, usize), (usize, usize))>,
    sel_style: Style,
    base_style: Style,
) -> Line<'static> {
    let (li, start) = row_meta;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    // Compute selection overlap as char offsets within this display row.
    let sel_in_row = sel.and_then(|((sr, sc), (er, ec))| {
        if li < sr || li > er {
            return None;
        }
        let abs_start = if li == sr { sc } else { 0 };
        let abs_end = if li == er { ec } else { start + len };
        let local_start = abs_start.saturating_sub(start);
        let local_end = abs_end.saturating_sub(start).min(len);
        if local_start >= local_end {
            return None;
        }
        Some((local_start, local_end))
    });
    match sel_in_row {
        None => Line::from(Span::styled(text.to_string(), base_style.bg(COMPOSER_BG))),
        Some((s, e)) => {
            let pre: String = chars[..s].iter().collect();
            let mid: String = chars[s..e].iter().collect();
            let post: String = chars[e..].iter().collect();
            Line::from(vec![
                Span::styled(pre, base_style.bg(COMPOSER_BG)),
                Span::styled(mid, sel_style),
                Span::styled(post, base_style.bg(COMPOSER_BG)),
            ])
        }
    }
}

fn build_composer_title(state: &AppState) -> String {
    let keys = state.selection_lines();
    if keys.is_empty() {
        return " Comment ".to_string();
    }
    let (fi, hi_first, li_first) = keys[0];
    let (_, hi_last, li_last) = *keys.last().unwrap();
    let file = match state.files.get(fi) {
        Some(f) => f,
        None => return " Comment ".to_string(),
    };
    let first = &file.hunks[hi_first].lines[li_first];
    let last = &file.hunks[hi_last].lines[li_last];
    let start = first.new_lineno.or(first.old_lineno).unwrap_or(0);
    let end = last.new_lineno.or(last.old_lineno).unwrap_or(start);
    let anchor = if end > start {
        format!("L{start}-{end}")
    } else {
        format!("L{start}")
    };
    let (verb, delete_hint) = match state.composer_target {
        Some(ComposerTarget::EditThread(_)) => ("Edit comment", " · ctrl-d delete"),
        Some(ComposerTarget::EditReply { .. }) => ("Edit reply", ""),
        Some(ComposerTarget::NewReply(_)) => ("Reply to thread", ""),
        _ => ("Comment", ""),
    };
    format!(
        " {verb} on {}:{}  · enter save · shift-enter newline{} · esc cancel ",
        file.path, anchor, delete_hint
    )
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = (area.width * 3 / 4).max(50);
    let h = (area.height * 3 / 4).max(16);
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
        Line::from("  ctrl-d/u     half page down/up"),
        Line::from("  g / G        top / bottom"),
        Line::from("  ]   /   [    next / prev file"),
        Line::from("  }   /   {    next / prev hunk"),
        Line::from(""),
        Line::from("  space        collapse / expand current hunk (on @@) or file (elsewhere)"),
        Line::from("  z / Z        collapse all / expand all files"),
        Line::from("  v            toggle viewed (auto-collapses)"),
        Line::from("  y            yank current file path to clipboard"),
        Line::from("  e            toggle file tree sidebar"),
        Line::from("  R            toggle threads pane"),
        Line::from("  t            fuzzy file picker"),
        Line::from(""),
        Line::from("  w            toggle ignore-whitespace"),
        Line::from("  = / -        expand / shrink context lines"),
        Line::from("  , / .        decrease / increase tab width"),
        Line::from(""),
        Line::from("  r            toggle resolved (on a commented line)"),
        Line::from("  K / 0        add reaction / clear reactions (on a thread)"),
        Line::from("  V            cycle review verdict (comment / approve / request changes)"),
        Line::from(""),
        Line::from("  c            add / edit comment on current line"),
        Line::from("  enter        save comment (composer)"),
        Line::from("  shift-enter  insert newline (composer)"),
        Line::from("  n            jump to next thread where last reply isn't yours"),
        Line::from("  x            delete comment on current line"),
        Line::from("  ctrl-d       delete comment from edit mode (in composer)"),
        Line::from("  S            submit threads → REVIEW.md at repo root"),
        Line::from("               (agents may reply inside <!-- replies:tID --> blocks;"),
        Line::from("                replies are merged back on next launch)"),
        Line::from(""),
        Line::from(
            "  mouse        click to move cursor, click header to collapse, wheel to scroll",
        ),
        Line::from(""),
        Line::from("  ?            toggle this help"),
        Line::from("  q            quit (threads auto-persist to .gitdiff/threads.json)"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}
