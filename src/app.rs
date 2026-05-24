use crate::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::git::{DiffOpts, DiffSource};
use crate::syntax::{Highlighter, Span as HSpan};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatKind {
    FileHeader,
    FileFooter,
    HunkHeader,
    Code,
    ExpandedAbove,
    ExpandedBelow,
    ExpandBtnAbove,
    ExpandBtnBelow,
    // Inline rendering of a draft anchored to the Code row above. `line_idx`
    // selects which sub-row of the draft is being rendered (0 = header,
    // 1..N = body lines, N+1 = reactions if any).
    DraftRow,
    Spacer,
}

#[derive(Debug, Clone)]
pub struct FlatLine {
    pub kind: FlatKind,
    pub file_idx: usize,
    pub hunk_idx: Option<usize>,
    pub line_idx: Option<usize>,
    pub draft_idx: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub file_path: String,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    #[serde(default)]
    pub old_lineno_end: Option<usize>,
    #[serde(default)]
    pub new_lineno_end: Option<usize>,
    pub line_kind: String,
    pub diff_snippet: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub outdated: bool,
    #[serde(default)]
    pub reactions: Vec<String>,
    /// Stable identifier so agents can target a specific thread when replying
    /// inside REVIEW-*.md. Empty on drafts loaded from pre-thread JSON; the
    /// startup loader backfills any missing IDs.
    #[serde(default)]
    pub thread_id: String,
    /// Conversation replies — parsed back from REVIEW-*.md `<!-- replies:tID -->`
    /// blocks on launch, then preserved through subsequent submits.
    #[serde(default)]
    pub replies: Vec<Reply>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    /// Free-form (e.g. "you", "claude-code", "codex"). Agents pick their own.
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub const REACTION_CYCLE: &[&str] = &["👍", "👎", "🎉", "😄", "❤️", "🚀", "👀", "❓"];

impl Draft {
    pub fn anchor_label(&self) -> String {
        let a = self.new_lineno.or(self.old_lineno);
        let b = self.new_lineno_end.or(self.old_lineno_end);
        match (a, b) {
            (Some(a), Some(b)) if a != b => {
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                format!("L{lo}-{hi}")
            }
            (Some(s), _) => {
                if self.new_lineno.is_none() {
                    format!("L{s} (pre-change)")
                } else {
                    format!("L{s}")
                }
            }
            _ => "L?".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
    pub dragging: bool,
}

impl Selection {
    pub fn range(&self) -> (usize, usize) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Composing,
    Help,
    Picker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Comment => "COMMENT",
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST CHANGES",
        }
    }
    pub fn cycle(self) -> Self {
        match self {
            Verdict::Comment => Verdict::Approve,
            Verdict::Approve => Verdict::RequestChanges,
            Verdict::RequestChanges => Verdict::Comment,
        }
    }
}

pub type LineKey = (usize, usize, usize); // (file_idx, hunk_idx, line_idx)

#[derive(Debug, Default, Clone)]
pub struct Expansion {
    pub above: Vec<DiffLine>,
    pub below: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy)]
pub struct IntraRange {
    pub prefix: usize,
    pub suffix: usize,
}

pub struct AppState {
    pub source: DiffSource,
    pub source_label: String,
    pub files: Vec<FileDiff>,
    pub expanded: Vec<bool>,
    pub flat: Vec<FlatLine>,
    pub cursor: usize,
    pub scroll: usize,
    pub viewport_height: usize,
    pub drafts: Vec<Draft>,
    pub mode: Mode,
    pub status: Option<String>,
    pub should_quit: bool,
    pub total_additions: usize,
    pub total_deletions: usize,
    pub highlights: HashMap<LineKey, Vec<HSpan>>,
    pub intraline: HashMap<LineKey, IntraRange>,
    pub expansions: HashMap<(usize, usize), Expansion>,
    // Cached file content for expand-context (loaded lazily on first click).
    // None → fetch failed / unavailable. Some → use lines.len() as max bound.
    pub file_blobs: HashMap<String, Option<Vec<String>>>,
    // Hunks the user has explicitly folded (only the `@@` header visible).
    pub collapsed_hunks: HashSet<(usize, usize)>,
    pub selection: Option<Selection>,
    pub cursor_visible: bool,
    pub body_top: u16,
    pub viewed: HashSet<String>,
    pub show_tree: bool,
    pub show_drafts_pane: bool,
    pub opts: DiffOpts,
    pub tab_width: usize,
    pub verdict: Verdict,
    pub drafts_cursor: usize,
    pub picker: Option<FuzzyPicker>,
    pub body_x: u16,
    pub body_width: u16,
    // Dynamic composer popup height (rows, includes borders). Set by ui::draw
    // each frame based on the TextArea's content. 0 when not composing.
    pub composer_height: u16,
    // Index of the draft currently being edited in the composer. When Some,
    // rebuild_flat hides that draft's rows so the composer replaces (not
    // duplicates) the rendered comment.
    pub editing_draft_idx: Option<usize>,
    // Geometry of the composer popup as drawn last frame (x, y, w, h). Used to
    // map mouse coordinates into the embedded TextArea for drag-select.
    pub composer_rect: Option<(u16, u16, u16, u16)>,
    // Geometry of the floating copy/comment menu shown after a drag selection.
    // Used for hit-testing clicks on its buttons.
    pub selection_menu_rect: Option<(u16, u16, u16, u16)>,
    // `body_width` last used when computing `flat`. Drives a re-flatten on
    // resize so draft sub-row counts match the new wrap.
    pub flat_for_body_width: u16,
}

#[derive(Debug, Clone)]
pub struct FuzzyPicker {
    pub query: String,
    pub cursor: usize,
}

impl AppState {
    pub fn new(
        source: DiffSource,
        source_label: String,
        files: Vec<FileDiff>,
        drafts: Vec<Draft>,
        viewed: HashSet<String>,
    ) -> Self {
        // viewed files start collapsed
        let expanded: Vec<bool> = files.iter().map(|f| !viewed.contains(&f.path)).collect();
        let flat = flatten(&files, &expanded, &HashMap::new());
        let total_additions = files.iter().map(|f| f.additions).sum();
        let total_deletions = files.iter().map(|f| f.deletions).sum();
        let highlights = precompute_highlights(&files);
        let intraline = precompute_intraline(&files);
        let mut s = Self {
            source,
            source_label,
            files,
            expanded,
            flat,
            cursor: 0,
            scroll: 0,
            viewport_height: 20,
            drafts,
            mode: Mode::Normal,
            status: None,
            should_quit: false,
            total_additions,
            total_deletions,
            highlights,
            intraline,
            expansions: HashMap::new(),
            file_blobs: HashMap::new(),
            collapsed_hunks: HashSet::new(),
            selection: None,
            cursor_visible: false,
            body_top: 1,
            viewed,
            show_tree: false,
            show_drafts_pane: false,
            opts: DiffOpts::default(),
            tab_width: 4,
            verdict: Verdict::Comment,
            drafts_cursor: 0,
            picker: None,
            body_x: 0,
            body_width: 80,
            composer_height: 0,
            editing_draft_idx: None,
            composer_rect: None,
            selection_menu_rect: None,
            flat_for_body_width: 0,
        };
        // start on first code line if possible
        if let Some(i) = s.flat.iter().position(|l| l.kind == FlatKind::Code) {
            s.cursor = i;
        }
        s
    }

    pub fn rebuild_flat(&mut self) {
        let mut out = Vec::new();
        for fi in 0..self.files.len() {
            out.push(FlatLine {
                kind: FlatKind::FileHeader,
                file_idx: fi,
                hunk_idx: None,
                line_idx: None,
                        draft_idx: None,
            });
            if self.expanded.get(fi).copied().unwrap_or(true) {
                for hi in 0..self.files[fi].hunks.len() {
                    // -- above button --
                    // Skip the above-button if THIS hunk is collapsed (the
                    // user has folded it; no need to offer expanding context
                    // around something they explicitly hid).
                    let hunk_collapsed = self.collapsed_hunks.contains(&(fi, hi));
                    let rem_above = if hunk_collapsed { 0 } else { self.remaining_above(fi, hi) };
                    let emit_above_btn = if hi == 0 {
                        rem_above > 0
                    } else {
                        let prev_rem = if self.collapsed_hunks.contains(&(fi, hi - 1)) {
                            0
                        } else {
                            self.remaining_below(fi, hi - 1, 0)
                        };
                        let combined = prev_rem + rem_above;
                        combined > 20 && rem_above > 0
                    };
                    if emit_above_btn {
                        let count = 20.min(rem_above);
                        out.push(FlatLine {
                            kind: FlatKind::ExpandBtnAbove,
                            file_idx: fi,
                            hunk_idx: Some(hi),
                            line_idx: Some(count),
                        draft_idx: None,
                        });
                    }

                    // -- hunk header (always FIRST inside the hunk's region,
                    //    matching GitHub's layout: `@@` at the top, then any
                    //    expanded-above context, then the code) --
                    out.push(FlatLine {
                        kind: FlatKind::HunkHeader,
                        file_idx: fi,
                        hunk_idx: Some(hi),
                        line_idx: None,
                        draft_idx: None,
                    });

                    // If this hunk is collapsed, skip the rest of its rows
                    // (expanded context, code, expanded-below, below button).
                    if self.collapsed_hunks.contains(&(fi, hi)) {
                        continue;
                    }

                    // -- expanded-above context (below the @@ header, above the
                    //    actual change region) --
                    if let Some(exp) = self.expansions.get(&(fi, hi)) {
                        for li in 0..exp.above.len() {
                            out.push(FlatLine {
                                kind: FlatKind::ExpandedAbove,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(li),
                        draft_idx: None,
                            });
                        }
                    }

                    // -- code (with inline drafts attached) --
                    for li in 0..self.files[fi].hunks[hi].lines.len() {
                        out.push(FlatLine {
                            kind: FlatKind::Code,
                            file_idx: fi,
                            hunk_idx: Some(hi),
                            line_idx: Some(li),
                            draft_idx: None,
                        });
                        // emit any drafts anchored at this code line, inline
                        let line = &self.files[fi].hunks[hi].lines[li];
                        let drafts_here: Vec<usize> = self
                            .drafts
                            .iter()
                            .enumerate()
                            .filter(|(_, d)| {
                                d.file_path == self.files[fi].path
                                    && d.old_lineno == line.old_lineno
                                    && d.new_lineno == line.new_lineno
                            })
                            .map(|(i, _)| i)
                            .collect();
                        for di in drafts_here {
                            // Skip the draft currently being edited — the composer
                            // popup renders in its place.
                            if self.editing_draft_idx == Some(di) {
                                continue;
                            }
                            // Resolved threads are hidden from the inline TUI;
                            // they still show in the side drafts pane and in
                            // REVIEW-*.md's Resolved section.
                            if self.drafts[di].resolved {
                                continue;
                            }
                            let max_w = draft_text_width(self.body_width);
                            let body_lines = wrap_body(&self.drafts[di].body, max_w)
                                .len()
                                .max(1);
                            let has_react = !self.drafts[di].reactions.is_empty();
                            // Each reply contributes: 1 header divider row +
                            // its wrapped body rows.
                            let reply_rows: usize = self.drafts[di]
                                .replies
                                .iter()
                                .map(|r| 1 + wrap_body(&r.body, max_w).len().max(1))
                                .sum();
                            // 2 border rows (top + bottom) + body + replies + optional reactions.
                            let total_rows = 2
                                + body_lines
                                + reply_rows
                                + if has_react { 1 } else { 0 };
                            for sub in 0..total_rows {
                                out.push(FlatLine {
                                    kind: FlatKind::DraftRow,
                                    file_idx: fi,
                                    hunk_idx: Some(hi),
                                    line_idx: Some(sub),
                                    draft_idx: Some(di),
                                });
                            }
                        }
                    }

                    // -- expanded-below context --
                    if let Some(exp) = self.expansions.get(&(fi, hi)) {
                        for li in 0..exp.below.len() {
                            out.push(FlatLine {
                                kind: FlatKind::ExpandedBelow,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(li),
                        draft_idx: None,
                            });
                        }
                    }

                    // -- below button --
                    // count_hint=0 so we don't show the button when the
                    // file blob can't be fetched (deleted file, binary, etc.)
                    let rem_below = self.remaining_below(fi, hi, 0);
                    let is_last = hi + 1 >= self.files[fi].hunks.len();
                    let next_above_rem = if is_last {
                        0
                    } else {
                        self.remaining_above(fi, hi + 1)
                    };
                    if is_last {
                        if rem_below > 0 {
                            let count = 20.min(rem_below);
                            out.push(FlatLine {
                                kind: FlatKind::ExpandBtnBelow,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(count),
                        draft_idx: None,
                            });
                        }
                    } else {
                        let combined = rem_below + next_above_rem;
                        if combined == 0 {
                            // gap fully closed
                        } else if combined <= 20 {
                            // merged button: load the entire remaining gap
                            // into THIS hunk's below
                            out.push(FlatLine {
                                kind: FlatKind::ExpandBtnBelow,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(combined),
                        draft_idx: None,
                            });
                        } else if rem_below > 0 {
                            let count = 20.min(rem_below);
                            out.push(FlatLine {
                                kind: FlatKind::ExpandBtnBelow,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(count),
                        draft_idx: None,
                            });
                        }
                    }
                }
            }
            out.push(FlatLine {
                kind: FlatKind::FileFooter,
                file_idx: fi,
                hunk_idx: None,
                line_idx: None,
                        draft_idx: None,
            });
            out.push(FlatLine {
                kind: FlatKind::Spacer,
                file_idx: fi,
                hunk_idx: None,
                line_idx: None,
                        draft_idx: None,
            });
        }
        self.flat = out;
    }

    pub fn toggle_hunk_collapse(&mut self, fi: usize, hi: usize) {
        let key = (fi, hi);
        if self.collapsed_hunks.contains(&key) {
            self.collapsed_hunks.remove(&key);
        } else {
            self.collapsed_hunks.insert(key);
        }
        self.rebuild_flat();
        self.cursor = self.cursor.min(self.flat.len().saturating_sub(1));
        self.ensure_cursor_visible();
    }

    pub fn toggle_collapse(&mut self, file_idx: usize) {
        if file_idx >= self.expanded.len() {
            return;
        }
        let cursor_file = self.flat.get(self.cursor).map(|fl| fl.file_idx);
        self.expanded[file_idx] = !self.expanded[file_idx];
        self.rebuild_flat();
        // try to keep cursor in the same file's header after rebuild
        if let Some(fi) = cursor_file {
            if let Some(i) = self.flat.iter().position(|fl| {
                fl.file_idx == fi && matches!(fl.kind, FlatKind::FileHeader | FlatKind::Code)
            }) {
                self.cursor = i.min(self.flat.len().saturating_sub(1));
            }
        }
        self.ensure_cursor_visible();
    }

    pub fn collapse_all(&mut self, collapsed: bool) {
        for e in self.expanded.iter_mut() {
            *e = !collapsed;
        }
        self.rebuild_flat();
        self.cursor = self.cursor.min(self.flat.len().saturating_sub(1));
        self.ensure_cursor_visible();
    }

    pub fn toggle_viewed(&mut self, file_idx: usize) -> Option<bool> {
        let file = self.files.get(file_idx)?;
        let path = file.path.clone();
        let now_viewed = if self.viewed.contains(&path) {
            self.viewed.remove(&path);
            false
        } else {
            self.viewed.insert(path);
            true
        };
        if let Some(e) = self.expanded.get_mut(file_idx) {
            *e = !now_viewed; // viewed → collapsed
        }
        self.rebuild_flat();
        self.cursor = self.cursor.min(self.flat.len().saturating_sub(1));
        self.ensure_cursor_visible();
        Some(now_viewed)
    }

    pub fn viewed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| self.viewed.contains(&f.path))
            .count()
    }

    /// Cache a file's content for accurate expansion bounds. Called lazily by
    /// the click handler before invoking expand_hunk.
    pub fn set_file_blob(&mut self, path: String, blob: Option<Vec<String>>) {
        self.file_blobs.insert(path, blob);
    }

    fn file_max_new_lineno(&self, fi: usize) -> Option<usize> {
        let path = self.files.get(fi)?.path.clone();
        self.file_blobs.get(&path).and_then(|b| b.as_ref()).map(|v| v.len())
    }

    fn above_frontier_new(&self, fi: usize, hi: usize) -> usize {
        self.expansions
            .get(&(fi, hi))
            .and_then(|e| e.above.first())
            .and_then(|l| l.new_lineno)
            .unwrap_or_else(|| self.files[fi].hunks[hi].new_start)
    }

    fn below_frontier_new(&self, fi: usize, hi: usize) -> usize {
        self.expansions
            .get(&(fi, hi))
            .and_then(|e| e.below.last())
            .and_then(|l| l.new_lineno)
            .unwrap_or_else(|| {
                let h = &self.files[fi].hunks[hi];
                h.new_start + h.new_count - 1
            })
    }

    /// Lower (smaller new_lineno) boundary for hunk `hi`'s "expand above" zone.
    fn above_lower_bound_new(&self, fi: usize, hi: usize) -> usize {
        if hi == 0 {
            1
        } else {
            // previous hunk's last covered line + 1
            let prev_below_frontier = self.below_frontier_new(fi, hi - 1);
            prev_below_frontier + 1
        }
    }

    /// Upper (larger new_lineno) boundary for hunk `hi`'s "expand below" zone.
    /// Returns None if we don't yet know the file's total length (no blob cached
    /// and this is the last hunk).
    fn below_upper_bound_new(&self, fi: usize, hi: usize) -> Option<usize> {
        let file = self.files.get(fi)?;
        if hi + 1 < file.hunks.len() {
            let next_above_frontier = self.above_frontier_new(fi, hi + 1);
            Some(next_above_frontier.saturating_sub(1))
        } else {
            self.file_max_new_lineno(fi)
        }
    }

    /// Number of lines that COULD still be loaded above hunk `hi`.
    pub fn remaining_above(&self, fi: usize, hi: usize) -> usize {
        self.above_frontier_new(fi, hi)
            .saturating_sub(self.above_lower_bound_new(fi, hi))
    }

    /// Number of lines that COULD still be loaded below hunk `hi`. Returns
    /// `count_hint` (20) when the file length isn't known yet — conservative
    /// optimistic estimate; the actual expansion is bounded later.
    pub fn remaining_below(&self, fi: usize, hi: usize, count_hint: usize) -> usize {
        match self.below_upper_bound_new(fi, hi) {
            Some(upper) => upper.saturating_sub(self.below_frontier_new(fi, hi)),
            None => count_hint, // unknown — show button optimistically
        }
    }

    /// Combined gap remaining between hunk `hi` and the next hunk. Returns None
    /// if `hi` is the last hunk in the file.
    pub fn inter_hunk_remaining(&self, fi: usize, hi: usize) -> Option<usize> {
        let file = self.files.get(fi)?;
        if hi + 1 >= file.hunks.len() {
            return None;
        }
        let lower = self.below_frontier_new(fi, hi);
        let upper = self.above_frontier_new(fi, hi + 1);
        Some(upper.saturating_sub(lower + 1))
    }

    /// Expand hunk `hi` in the given direction by up to `count` lines, sourced
    /// from `lines` (new-side file content). Returns the number actually added.
    pub fn expand_hunk(
        &mut self,
        fi: usize,
        hi: usize,
        lines: &[String],
        count: usize,
        above: bool,
    ) -> usize {
        let Some(_) = self.files.get(fi) else { return 0 };
        if hi >= self.files[fi].hunks.len() {
            return 0;
        }
        let added = if above {
            let frontier_new = self.above_frontier_new(fi, hi);
            let lower_bound_new = self.above_lower_bound_new(fi, hi);
            let available = frontier_new.saturating_sub(lower_bound_new);
            let added = count.min(available);
            if added == 0 {
                return 0;
            }
            // derive old-side frontier (same delta as the hunk's first line)
            let frontier_old = self
                .expansions
                .get(&(fi, hi))
                .and_then(|e| e.above.first())
                .and_then(|l| l.old_lineno)
                .unwrap_or_else(|| self.files[fi].hunks[hi].old_start);
            let mut prepend: Vec<DiffLine> = Vec::with_capacity(added);
            for k in 0..added {
                let nl = frontier_new - added + k;
                let ol = frontier_old - added + k;
                let content = lines
                    .get(nl.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default();
                prepend.push(DiffLine {
                    kind: LineKind::Context,
                    old_lineno: Some(ol),
                    new_lineno: Some(nl),
                    content,
                });
            }
            let exp = self.expansions.entry((fi, hi)).or_default();
            let mut new_above = prepend;
            new_above.extend(std::mem::take(&mut exp.above));
            exp.above = new_above;
            added
        } else {
            let frontier_new = self.below_frontier_new(fi, hi);
            let upper_bound_new = match self.below_upper_bound_new(fi, hi) {
                Some(b) => b,
                None => frontier_new + count, // unknown — bound by count
            };
            // also clip to actual file length if we know it
            let upper_bound_new = match self.file_max_new_lineno(fi) {
                Some(max) => upper_bound_new.min(max),
                None => upper_bound_new,
            };
            let available = upper_bound_new.saturating_sub(frontier_new);
            let added = count.min(available);
            if added == 0 {
                return 0;
            }
            let frontier_old = self
                .expansions
                .get(&(fi, hi))
                .and_then(|e| e.below.last())
                .and_then(|l| l.old_lineno)
                .unwrap_or_else(|| {
                    let h = &self.files[fi].hunks[hi];
                    h.old_start + h.old_count - 1
                });
            let mut append: Vec<DiffLine> = Vec::with_capacity(added);
            for k in 1..=added {
                let nl = frontier_new + k;
                let ol = frontier_old + k;
                if nl > lines.len() {
                    break;
                }
                let content = lines
                    .get(nl.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default();
                append.push(DiffLine {
                    kind: LineKind::Context,
                    old_lineno: Some(ol),
                    new_lineno: Some(nl),
                    content,
                });
            }
            let n = append.len();
            let exp = self.expansions.entry((fi, hi)).or_default();
            exp.below.extend(append);
            n
        };
        self.rebuild_flat();
        self.ensure_cursor_visible();
        added
    }

    pub fn replace_files(&mut self, files: Vec<FileDiff>) {
        let expanded: Vec<bool> = files.iter().map(|f| !self.viewed.contains(&f.path)).collect();
        let total_additions = files.iter().map(|f| f.additions).sum();
        let total_deletions = files.iter().map(|f| f.deletions).sum();
        let highlights = precompute_highlights(&files);
        let intraline = precompute_intraline(&files);
        self.files = files;
        self.expanded = expanded;
        self.highlights = highlights;
        self.intraline = intraline;
        // Hunk indices and file contents may have shifted; drop stale state.
        self.expansions.clear();
        self.file_blobs.clear();
        self.total_additions = total_additions;
        self.total_deletions = total_deletions;
        self.rebuild_flat();
        self.cursor = self.cursor.min(self.flat.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.flat.len().saturating_sub(1));
        self.clear_selection();
        self.ensure_cursor_visible();
    }

    pub fn jump_to_file(&mut self, file_idx: usize) {
        if let Some(i) = self
            .flat
            .iter()
            .position(|fl| fl.file_idx == file_idx && fl.kind == FlatKind::FileHeader)
        {
            self.cursor = i;
            self.ensure_cursor_visible();
        }
    }

    pub fn jump_to_draft(&mut self, draft_idx: usize) {
        // copy out the anchor data to release the immutable borrow before rebuild_flat
        let (anchor_path, anchor_old, anchor_new) = match self.drafts.get(draft_idx) {
            Some(d) => (d.file_path.clone(), d.old_lineno, d.new_lineno),
            None => return,
        };
        let Some(fi) = self.files.iter().position(|f| f.path == anchor_path) else {
            return;
        };
        if !self.expanded.get(fi).copied().unwrap_or(true) {
            self.expanded[fi] = true;
            self.rebuild_flat();
        }
        let target_idx = self.flat.iter().position(|fl| {
            if fl.file_idx != fi || fl.kind != FlatKind::Code {
                return false;
            }
            let Some(hi) = fl.hunk_idx else { return false };
            let Some(li) = fl.line_idx else { return false };
            let Some(line) = self.files[fi].hunks.get(hi).and_then(|h| h.lines.get(li)) else {
                return false;
            };
            line.old_lineno == anchor_old && line.new_lineno == anchor_new
        });
        if let Some(i) = target_idx {
            self.cursor = i;
            self.ensure_cursor_visible();
        } else {
            self.jump_to_file(fi);
        }
    }

    pub fn filtered_files(&self) -> Vec<usize> {
        let q = match &self.picker {
            Some(p) => p.query.to_lowercase(),
            None => String::new(),
        };
        if q.is_empty() {
            return (0..self.files.len()).collect();
        }
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| fuzzy_match(&f.path.to_lowercase(), &q))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current_line(&self) -> Option<(&FileDiff, &Hunk, &DiffLine)> {
        let fl = self.flat.get(self.cursor)?;
        if fl.kind != FlatKind::Code {
            return None;
        }
        let file = self.files.get(fl.file_idx)?;
        let hunk = file.hunks.get(fl.hunk_idx?)?;
        let line = hunk.lines.get(fl.line_idx?)?;
        Some((file, hunk, line))
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let len = self.flat.len();
        if len == 0 {
            return;
        }
        let mut c = self.cursor as i32 + delta;
        c = c.clamp(0, (len - 1) as i32);
        self.cursor = c as usize;
        self.ensure_cursor_visible();
    }

    pub fn jump_next_file(&mut self) {
        let cur_file = self.flat.get(self.cursor).map(|f| f.file_idx).unwrap_or(0);
        if let Some(i) = self
            .flat
            .iter()
            .position(|f| f.kind == FlatKind::FileHeader && f.file_idx > cur_file)
        {
            self.cursor = i;
            self.ensure_cursor_visible();
        }
    }

    pub fn jump_prev_file(&mut self) {
        let cur_file = self.flat.get(self.cursor).map(|f| f.file_idx).unwrap_or(0);
        if let Some(i) = self
            .flat
            .iter()
            .rposition(|f| f.kind == FlatKind::FileHeader && f.file_idx < cur_file)
        {
            self.cursor = i;
            self.ensure_cursor_visible();
        }
    }

    pub fn jump_next_hunk(&mut self) {
        let start = self.cursor + 1;
        if let Some(i) = self.flat[start.min(self.flat.len())..]
            .iter()
            .position(|f| f.kind == FlatKind::HunkHeader)
        {
            self.cursor = start + i;
            self.ensure_cursor_visible();
        }
    }

    pub fn jump_prev_hunk(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(i) = self.flat[..self.cursor]
            .iter()
            .rposition(|f| f.kind == FlatKind::HunkHeader)
        {
            self.cursor = i;
            self.ensure_cursor_visible();
        }
    }

    pub fn ensure_cursor_visible(&mut self) {
        let vh = self.viewport_height.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + vh {
            self.scroll = self.cursor + 1 - vh;
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let max_scroll = self.flat.len().saturating_sub(self.viewport_height.max(1));
        let new_scroll = (self.scroll as i32 + delta).max(0) as usize;
        self.scroll = new_scroll.min(max_scroll);
    }

    pub fn start_selection(&mut self, idx: usize) {
        if idx >= self.flat.len() {
            return;
        }
        self.selection = Some(Selection {
            start: idx,
            end: idx,
            dragging: true,
        });
        self.cursor = idx;
    }

    pub fn extend_selection(&mut self, idx: usize) {
        let Some(s) = self.selection else { return };
        let Some(start_fl) = self.flat.get(s.start).cloned() else { return };
        let Some(end_fl) = self.flat.get(idx).cloned() else { return };
        if start_fl.file_idx != end_fl.file_idx {
            return;
        }
        if end_fl.kind != FlatKind::Code {
            return;
        }
        self.selection = Some(Selection {
            start: s.start,
            end: idx,
            dragging: s.dragging,
        });
        self.cursor = idx;
    }

    pub fn finish_selection(&mut self) {
        if let Some(s) = self.selection.as_mut() {
            s.dragging = false;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn selection_lines(&self) -> Vec<LineKey> {
        let Some(s) = self.selection else { return Vec::new() };
        let (a, b) = s.range();
        (a..=b)
            .filter_map(|i| {
                let fl = self.flat.get(i)?;
                if fl.kind != FlatKind::Code {
                    return None;
                }
                Some((fl.file_idx, fl.hunk_idx?, fl.line_idx?))
            })
            .collect()
    }

    /// Plain code content of the currently selected lines (no gutter or sign
    /// prefix), joined with newlines. Returns None if nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        let keys = self.selection_lines();
        if keys.is_empty() {
            return None;
        }
        let mut out = String::new();
        for (fi, hi, li) in keys {
            if let Some(l) = self.files.get(fi).and_then(|f| f.hunks.get(hi)).and_then(|h| h.lines.get(li)) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&l.content);
            }
        }
        Some(out)
    }

    pub fn existing_draft_body_for_selection(&self) -> Option<String> {
        let keys = self.selection_lines();
        // Anchor at the *last* selected line — that's where the composer and
        // the saved draft are rendered.
        let (fi, hi, li) = *keys.last()?;
        let file = self.files.get(fi)?;
        let line = file.hunks.get(hi)?.lines.get(li)?;
        self.drafts
            .iter()
            .find(|d| {
                d.file_path == file.path
                    && d.new_lineno == line.new_lineno
                    && d.old_lineno == line.old_lineno
            })
            .map(|d| d.body.clone())
    }

    pub fn draft_for_selection(&self) -> Option<usize> {
        let keys = self.selection_lines();
        let (fi, hi, li) = *keys.last()?;
        let file = self.files.get(fi)?;
        let line = file.hunks.get(hi)?.lines.get(li)?;
        self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        })
    }

    /// Returns `(draft_idx, is_anchor)` if `line` falls inside a draft's range.
    /// `is_anchor` is true for the row the draft renders on; the anchor is the
    /// *last* line of the original selection. The two stored linenos can
    /// appear in either order across older drafts, so we treat the pair as an
    /// unordered min/max range when checking inclusion.
    pub fn draft_covering_line(&self, fi: usize, line: &DiffLine) -> Option<(usize, bool)> {
        let path = &self.files.get(fi)?.path;
        self.drafts.iter().enumerate().find_map(|(idx, d)| {
            if &d.file_path != path {
                return None;
            }
            // Hidden from inline rendering when resolved — matches rebuild_flat
            // (no DraftRow emitted), so the framing/marker must also be off.
            if d.resolved {
                return None;
            }
            if d.old_lineno == line.old_lineno && d.new_lineno == line.new_lineno {
                return Some((idx, true));
            }
            if let (Some(a), Some(b), Some(ln)) =
                (d.new_lineno, d.new_lineno_end, line.new_lineno)
            {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if ln >= lo && ln <= hi {
                    return Some((idx, false));
                }
            }
            if let (Some(a), Some(b), Some(ln)) =
                (d.old_lineno, d.old_lineno_end, line.old_lineno)
            {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if ln >= lo && ln <= hi {
                    return Some((idx, false));
                }
            }
            None
        })
    }

    pub fn draft_for_cursor(&self) -> Option<usize> {
        let (file, _, line) = self.current_line()?;
        self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        })
    }

    pub fn add_draft_from_selection(&mut self, body: String) -> Option<()> {
        let keys = self.selection_lines();
        if keys.is_empty() {
            return None;
        }
        let (fi, _, _) = keys[0];
        let file = self.files.get(fi)?.clone();
        let first_key = *keys.first()?;
        let last_key = *keys.last()?;
        let first_line = file
            .hunks
            .get(first_key.1)?
            .lines
            .get(first_key.2)?
            .clone();
        let last_line = file.hunks.get(last_key.1)?.lines.get(last_key.2)?.clone();

        let snippet = build_range_snippet(&file, &keys);
        // Use the last selected line's kind for the visual sigil — that's the
        // line the box now anchors to.
        let kind = match last_line.kind {
            LineKind::Added => "added",
            LineKind::Deleted => "deleted",
            LineKind::Context => "context",
        };
        // Anchor on the *last* selected line; store the other end of the range
        // in `_end` (it may be numerically less than the anchor).
        let other_old = if keys.len() > 1 {
            first_line.old_lineno
        } else {
            None
        };
        let other_new = if keys.len() > 1 {
            first_line.new_lineno
        } else {
            None
        };

        if let Some(idx) = self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == last_line.new_lineno
                && d.old_lineno == last_line.old_lineno
        }) {
            self.drafts[idx].body = body;
            self.drafts[idx].created_at = Utc::now();
            self.drafts[idx].old_lineno_end = other_old;
            self.drafts[idx].new_lineno_end = other_new;
            self.drafts[idx].diff_snippet = snippet;
            return Some(());
        }
        let created_at = Utc::now();
        let thread_id = make_thread_id(
            &file.path,
            last_line.old_lineno,
            last_line.new_lineno,
            &created_at,
        );
        self.drafts.push(Draft {
            file_path: file.path.clone(),
            old_lineno: last_line.old_lineno,
            new_lineno: last_line.new_lineno,
            old_lineno_end: other_old,
            new_lineno_end: other_new,
            line_kind: kind.to_string(),
            diff_snippet: snippet,
            body,
            created_at,
            resolved: false,
            outdated: false,
            reactions: Vec::new(),
            thread_id,
            replies: Vec::new(),
        });
        Some(())
    }

    pub fn add_reaction_at_cursor(&mut self) -> Option<String> {
        let idx = self.draft_for_cursor()?;
        let used: HashSet<&String> = self.drafts[idx].reactions.iter().collect();
        // pick the first reaction not yet present
        let pick = REACTION_CYCLE
            .iter()
            .find(|r| !used.contains(&r.to_string()))
            .copied()
            .unwrap_or(REACTION_CYCLE[0]);
        self.drafts[idx].reactions.push(pick.to_string());
        Some(pick.to_string())
    }

    pub fn clear_reactions_at_cursor(&mut self) -> bool {
        let Some(idx) = self.draft_for_cursor() else { return false };
        let had = !self.drafts[idx].reactions.is_empty();
        self.drafts[idx].reactions.clear();
        had
    }

    pub fn toggle_resolved_at_cursor(&mut self) -> Option<bool> {
        let idx = self.draft_for_cursor()?;
        self.drafts[idx].resolved = !self.drafts[idx].resolved;
        Some(self.drafts[idx].resolved)
    }

    pub fn mark_outdated_drafts(&mut self) {
        // a draft is outdated if its anchor line no longer exists in the parsed diff
        for d in self.drafts.iter_mut() {
            let Some(file) = self.files.iter().find(|f| f.path == d.file_path) else {
                d.outdated = true;
                continue;
            };
            let found = file.hunks.iter().any(|h| {
                h.lines.iter().any(|l| {
                    l.old_lineno == d.old_lineno && l.new_lineno == d.new_lineno
                })
            });
            d.outdated = !found;
        }
    }

    pub fn delete_draft_at_cursor(&mut self) -> bool {
        if let Some(idx) = self.draft_for_cursor() {
            self.drafts.remove(idx);
            true
        } else {
            false
        }
    }
}

fn flatten(
    files: &[FileDiff],
    expanded: &[bool],
    _expansions: &HashMap<(usize, usize), Expansion>,
) -> Vec<FlatLine> {
    // Stub kept for backward call sites that don't yet route through AppState
    // (e.g., the bootstrap from new() before AppState is constructed).
    let mut out = Vec::new();
    for (fi, file) in files.iter().enumerate() {
        out.push(FlatLine {
            kind: FlatKind::FileHeader,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
                        draft_idx: None,
        });
        if expanded.get(fi).copied().unwrap_or(true) {
            for (hi, hunk) in file.hunks.iter().enumerate() {
                out.push(FlatLine {
                    kind: FlatKind::HunkHeader,
                    file_idx: fi,
                    hunk_idx: Some(hi),
                    line_idx: None,
                        draft_idx: None,
                });
                for (li, _) in hunk.lines.iter().enumerate() {
                    out.push(FlatLine {
                        kind: FlatKind::Code,
                        file_idx: fi,
                        hunk_idx: Some(hi),
                        line_idx: Some(li),
                        draft_idx: None,
                    });
                }
            }
        }
        out.push(FlatLine {
            kind: FlatKind::FileFooter,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
                        draft_idx: None,
        });
        out.push(FlatLine {
            kind: FlatKind::Spacer,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
                        draft_idx: None,
        });
    }
    out
}

fn precompute_intraline(files: &[FileDiff]) -> HashMap<LineKey, IntraRange> {
    let mut out = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        for (hi, h) in f.hunks.iter().enumerate() {
            // walk hunk, find runs of consecutive Deleted then Added
            let lines = &h.lines;
            let mut i = 0;
            while i < lines.len() {
                if lines[i].kind != LineKind::Deleted {
                    i += 1;
                    continue;
                }
                let del_start = i;
                while i < lines.len() && lines[i].kind == LineKind::Deleted {
                    i += 1;
                }
                let del_end = i;
                let add_start = i;
                while i < lines.len() && lines[i].kind == LineKind::Added {
                    i += 1;
                }
                let add_end = i;
                let pairs = (del_end - del_start).min(add_end - add_start);
                for k in 0..pairs {
                    let d = &lines[del_start + k].content;
                    let a = &lines[add_start + k].content;
                    if d == a {
                        continue;
                    }
                    let (prefix, suffix) = common_affixes(d, a);
                    let r = IntraRange { prefix, suffix };
                    out.insert((fi, hi, del_start + k), r);
                    out.insert((fi, hi, add_start + k), r);
                }
            }
        }
    }
    out
}

fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let prefix = ac
        .iter()
        .zip(bc.iter())
        .take_while(|(x, y)| x == y)
        .count();
    let max_suffix = ac.len().saturating_sub(prefix).min(bc.len().saturating_sub(prefix));
    let mut suffix = 0;
    while suffix < max_suffix
        && ac[ac.len() - 1 - suffix] == bc[bc.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (prefix, suffix)
}

fn precompute_highlights(files: &[FileDiff]) -> HashMap<LineKey, Vec<HSpan>> {
    let hl = Highlighter::global();
    let mut out = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        for (hi, h) in f.hunks.iter().enumerate() {
            for (li, l) in h.lines.iter().enumerate() {
                let spans = hl.highlight(&f.path, &l.content);
                out.insert((fi, hi, li), spans);
            }
        }
    }
    out
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut it = haystack.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Generates a stable thread identifier for a freshly created draft. Combines
/// the anchor location with the creation timestamp so re-creating a draft on
/// the same line yields a fresh ID (and therefore a fresh thread).
pub fn make_thread_id(
    file_path: &str,
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
    created_at: &DateTime<Utc>,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    file_path.hash(&mut h);
    old_lineno.hash(&mut h);
    new_lineno.hash(&mut h);
    created_at.timestamp_micros().hash(&mut h);
    format!("t_{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

/// Splits `body` into display rows that fit within `max_w` characters.
/// Empty `body` yields a single empty row. `max_w == 0` falls back to one
/// row per logical line.
pub fn wrap_body(body: &str, max_w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        if max_w == 0 {
            out.push(line.to_string());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let end = (i + max_w).min(chars.len());
            out.push(chars[i..end].iter().collect());
            i = end;
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Usable text width inside a rendered draft box for the given diff body
/// width (subtracts 2 borders and 1 leading-padding column).
pub fn draft_text_width(body_width: u16) -> usize {
    (body_width as usize).saturating_sub(3)
}

fn build_range_snippet(file: &FileDiff, keys: &[LineKey]) -> String {
    use std::collections::BTreeMap;
    let mut by_hunk: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (_, hi, li) in keys {
        by_hunk.entry(*hi).or_default().push(*li);
    }
    let mut out = String::new();
    for (hi, mut lis) in by_hunk {
        lis.sort_unstable();
        let h = match file.hunks.get(hi) {
            Some(h) => h,
            None => continue,
        };
        let first = *lis.first().unwrap_or(&0);
        let last = *lis.last().unwrap_or(&0);
        let start = first.saturating_sub(2);
        let end = (last + 3).min(h.lines.len());
        out.push_str(&h.header_text());
        out.push('\n');
        for l in &h.lines[start..end] {
            let prefix = match l.kind {
                LineKind::Added => '+',
                LineKind::Deleted => '-',
                LineKind::Context => ' ',
            };
            out.push(prefix);
            out.push_str(&l.content);
            out.push('\n');
        }
    }
    out
}
