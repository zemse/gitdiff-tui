use crate::app::{AppState, FlatKind, IntraRange, LineKey, Mode};
use crate::diff::{FileStatus, LineKind};
use crate::syntax::Span as HSpan;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_textarea::TextArea;

const ADD_BG: Color = Color::Rgb(20, 60, 30);
const DEL_BG: Color = Color::Rgb(80, 25, 25);
const ADD_INTRA_BG: Color = Color::Rgb(40, 130, 60);
const DEL_INTRA_BG: Color = Color::Rgb(160, 50, 50);
const HEADER_BG: Color = Color::Rgb(40, 44, 60);
const BORDER_FG: Color = Color::Rgb(80, 90, 110);

pub fn draw(f: &mut Frame, state: &mut AppState, composer: Option<&mut TextArea<'_>>) {
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

    // horizontal split: [tree?] [body] [drafts?]
    let mut constraints: Vec<Constraint> = Vec::new();
    if state.show_tree {
        constraints.push(Constraint::Length(30));
    }
    constraints.push(Constraint::Min(20));
    if state.show_drafts_pane {
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
    if state.show_drafts_pane {
        draw_drafts_pane(f, h_chunks[idx], state);
    }

    state.viewport_height = body_area.height as usize;
    state.body_top = body_area.y;
    state.body_x = body_area.x;
    state.body_width = body_area.width;
    draw_body(f, body_area, state);
    draw_footer(f, vert[2], state);

    if state.mode == Mode::Composing {
        if let Some(ta) = composer {
            let popup = composer_rect(body_area, state);
            draw_composer(f, popup, state, ta);
        }
    } else if state.mode == Mode::Help {
        draw_help(f, area);
    } else if state.mode == Mode::Picker {
        draw_picker(f, area, state);
    }
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
        let viewed = state.viewed.contains(&file.path);
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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(path_short, path_style),
            Span::styled(format!("  +{}", file.additions), Style::default().fg(Color::Green)),
            Span::styled(format!(" −{}", file.deletions), Style::default().fg(Color::Red)),
        ]));
    }
    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn draw_drafts_pane(f: &mut Frame, area: Rect, state: &AppState) {
    let title = format!(" Drafts ({}) ", state.drafts.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(BORDER_FG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.drafts.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no drafts yet — click a line or press c to comment",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
        f.render_widget(hint, inner);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, d) in state.drafts.iter().enumerate() {
        let selected = i == state.drafts_cursor;
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
                Style::default().fg(status_color).add_modifier(Modifier::BOLD),
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
                Style::default().fg(Color::Rgb(100, 180, 110)).add_modifier(Modifier::ITALIC),
            )));
            for sline in sug.lines().take(3) {
                lines.push(Line::from(vec![
                    Span::styled(
                        "    ┃ ",
                        Style::default().fg(Color::Rgb(100, 180, 110)),
                    ),
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
    let w = (area.width * 2 / 3).max(50).min(area.width.saturating_sub(4));
    let h = (area.height * 2 / 3).max(12).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect { x, y, width: w, height: h };
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
            Span::styled(format!("  +{}", file.additions), Style::default().fg(Color::Green)),
            Span::styled(format!(" −{}", file.deletions), Style::default().fg(Color::Red)),
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
    let open_drafts = state.drafts.iter().filter(|d| !d.resolved).count();
    let outdated = state.drafts.iter().filter(|d| d.outdated).count();
    let outdated_str = if outdated > 0 {
        format!(" · {outdated} outdated")
    } else {
        String::new()
    };
    let title = format!(
        " gitdiff · {} · {} files ({}/{} viewed) · +{} −{} · {} drafts{} · verdict: {} ",
        state.source_label,
        state.files.len(),
        state.viewed_count(),
        state.files.len(),
        state.total_additions,
        state.total_deletions,
        open_drafts,
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
    let hint = match state.mode {
        Mode::Normal => {
            "j/k · ]/[ file · }/{ hunk · e tree · t pick · R drafts · v viewed · y yank · c comment · S submit · ? help · q quit"
        }
        Mode::Composing => "ctrl-s save draft · esc cancel",
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

    let width = content_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(content_area.height as usize);
    let from = state.scroll;
    let to = (state.scroll + content_area.height as usize).min(state.flat.len());

    let sel_range = state.selection.map(|s| s.range());
    for i in from..to {
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
            FlatKind::Spacer => Line::from(""),
        };
        let in_selection = sel_range.map(|(a, b)| i >= a && i <= b).unwrap_or(false);
        let is_cursor_solo = sel_range.is_none() && i == state.cursor;
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
        state.flat.len(),
        content_area.height as usize,
    );
}

fn draw_scrollbar(f: &mut Frame, area: Rect, scroll: usize, total: usize, visible_h: usize) {
    let track_h = area.height as usize;
    if track_h == 0 || area.width == 0 {
        return;
    }
    let track_style = Style::default().fg(BORDER_FG);
    let thumb_style = Style::default().fg(Color::Rgb(150, 170, 210));

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
            if in_thumb {
                Line::from(Span::styled("█", thumb_style))
            } else {
                Line::from(Span::styled("│", track_style))
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn render_file_header(state: &AppState, fi: usize, width: usize) -> Line<'static> {
    let f = &state.files[fi];
    let expanded = state.expanded.get(fi).copied().unwrap_or(true);
    let viewed = state.viewed.contains(&f.path);
    let chevron = if expanded { '▾' } else { '▸' };

    let (badge, badge_style) = match f.status {
        FileStatus::Added => ("A", Style::default().fg(Color::Black).bg(Color::Green)),
        FileStatus::Modified => ("M", Style::default().fg(Color::Black).bg(Color::Yellow)),
        FileStatus::Deleted => ("D", Style::default().fg(Color::White).bg(Color::Red)),
        FileStatus::Renamed => ("R", Style::default().fg(Color::Black).bg(Color::Magenta)),
        FileStatus::Copied => ("C", Style::default().fg(Color::Black).bg(Color::Blue)),
    };

    let header_bg = if viewed { Color::Rgb(28, 32, 42) } else { HEADER_BG };
    let bg = Style::default().bg(header_bg);
    let border = Style::default().fg(BORDER_FG).bg(header_bg);
    let path_fg = if viewed { Color::Rgb(120, 130, 150) } else { Color::White };

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
    spans.push(Span::styled(
        format!("−{}", f.deletions),
        bg.fg(Color::Red),
    ));
    if f.binary {
        spans.push(Span::styled(" [binary]", bg.fg(Color::DarkGray)));
    }

    // right-aligned "viewed" indicator: pad first, then the badge
    let viewed_badge = if viewed { " ✓ viewed " } else { " ☐ viewed " };
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
    let border = Style::default().fg(BORDER_FG);
    let body = Style::default()
        .fg(Color::Rgb(120, 170, 200))
        .bg(Color::Rgb(30, 40, 60))
        .add_modifier(Modifier::ITALIC);
    let text = format!(" {} ", h.header_text());
    let pad = width
        .saturating_sub(text.chars().count() + 2)
        .max(0);
    Line::from(vec![
        Span::styled("│".to_string(), border),
        Span::styled(text, body),
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

    let draft_at = state.drafts.iter().find(|d| {
        d.file_path == file.path && d.old_lineno == l.old_lineno && d.new_lineno == l.new_lineno
    });
    let has_draft = draft_at.is_some();

    let (bg, sign_color) = match l.kind {
        LineKind::Added => (ADD_BG, Color::Green),
        LineKind::Deleted => (DEL_BG, Color::Red),
        LineKind::Context => (Color::Reset, Color::DarkGray),
    };
    let bg_style = Style::default().bg(bg);
    let border = Style::default().fg(BORDER_FG);

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
    let mark = match draft_at {
        Some(d) if d.resolved => '✓',
        Some(d) if d.outdated => '!',
        Some(_) => '◆',
        None => ' ',
    };
    let mark_color = match draft_at {
        Some(d) if d.resolved => Color::Green,
        Some(d) if d.outdated => Color::Rgb(180, 130, 50),
        Some(_) => Color::Yellow,
        None => Color::Yellow,
    };
    let _ = has_draft;

    let mut spans = vec![
        Span::styled("│".to_string(), border),
        Span::styled(
            format!(" {mark} "),
            bg_style.fg(mark_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            old_g,
            bg_style.fg(Color::Rgb(110, 120, 140)),
        ),
        Span::styled(" ", bg_style),
        Span::styled(
            new_g,
            bg_style.fg(Color::Rgb(110, 120, 140)),
        ),
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
                let bold_style = s
                    .style
                    .bg(emphasis_bg)
                    .add_modifier(Modifier::BOLD);
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
    let h: u16 = 7;
    let sel = state.selection;
    let (_a, b) = sel.map(|s| s.range()).unwrap_or((state.cursor, state.cursor));
    let scroll = state.scroll;
    let screen_row = (b.saturating_sub(scroll)) as u16;
    let anchor_y = body.y + screen_row;
    let space_below = (body.y + body.height).saturating_sub(anchor_y + 1);
    let y = if space_below >= h {
        anchor_y + 1
    } else {
        // place above selection start
        let (a, _) = sel.map(|s| s.range()).unwrap_or((state.cursor, state.cursor));
        let start_screen = (a.saturating_sub(scroll)) as u16;
        let start_y = body.y + start_screen;
        start_y.saturating_sub(h)
    };
    // clamp x/width to body
    let w = body.width.min(body.width).max(20);
    Rect {
        x: body.x,
        y,
        width: w,
        height: h,
    }
}

fn draw_composer(f: &mut Frame, popup: Rect, state: &AppState, ta: &mut TextArea<'_>) {
    f.render_widget(Clear, popup);

    let title = build_composer_title(state);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Rgb(20, 28, 40)));
    ta.set_block(block);
    f.render_widget(&*ta, popup);
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
    format!(" Comment on {}:{}  · ctrl-s save · esc cancel ", file.path, anchor)
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
        Line::from("  space        collapse / expand current file"),
        Line::from("  z / Z        collapse all / expand all"),
        Line::from("  v            toggle viewed (auto-collapses)"),
        Line::from("  y            yank current file path to clipboard"),
        Line::from("  e            toggle file tree sidebar"),
        Line::from("  R            toggle drafts pane"),
        Line::from("  t            fuzzy file picker"),
        Line::from(""),
        Line::from("  w            toggle ignore-whitespace"),
        Line::from("  = / -        expand / shrink context lines"),
        Line::from("  , / .        decrease / increase tab width"),
        Line::from(""),
        Line::from("  r            toggle resolved (on a commented line)"),
        Line::from("  K / 0        add reaction / clear reactions (on a draft)"),
        Line::from("  V            cycle review verdict (comment / approve / request changes)"),
        Line::from(""),
        Line::from("  c            add / edit comment on current line"),
        Line::from("  x            delete comment on current line"),
        Line::from("  S            submit drafts → REVIEW.md at repo root"),
        Line::from(""),
        Line::from("  mouse        click to move cursor, click header to collapse, wheel to scroll"),
        Line::from(""),
        Line::from("  ?            toggle this help"),
        Line::from("  q            quit (drafts auto-persist to .gitdiff/drafts.json)"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}
