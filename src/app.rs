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
    Spacer,
}

#[derive(Debug, Clone)]
pub struct FlatLine {
    pub kind: FlatKind,
    pub file_idx: usize,
    pub hunk_idx: Option<usize>,
    pub line_idx: Option<usize>,
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
}

pub const REACTION_CYCLE: &[&str] = &["👍", "👎", "🎉", "😄", "❤️", "🚀", "👀", "❓"];

impl Draft {
    pub fn anchor_label(&self) -> String {
        let start = self.new_lineno.or(self.old_lineno);
        let end = self.new_lineno_end.or(self.old_lineno_end);
        match (start, end) {
            (Some(s), Some(e)) if e > s => format!("L{s}-{e}"),
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
    pub selection: Option<Selection>,
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
        let flat = flatten(&files, &expanded);
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
            selection: None,
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
        };
        // start on first code line if possible
        if let Some(i) = s.flat.iter().position(|l| l.kind == FlatKind::Code) {
            s.cursor = i;
        }
        s
    }

    pub fn toggle_collapse(&mut self, file_idx: usize) {
        if file_idx >= self.expanded.len() {
            return;
        }
        let cursor_file = self.flat.get(self.cursor).map(|fl| fl.file_idx);
        self.expanded[file_idx] = !self.expanded[file_idx];
        self.flat = flatten(&self.files, &self.expanded);
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
        self.flat = flatten(&self.files, &self.expanded);
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
        self.flat = flatten(&self.files, &self.expanded);
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
        self.total_additions = total_additions;
        self.total_deletions = total_deletions;
        self.flat = flatten(&self.files, &self.expanded);
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
        let Some(d) = self.drafts.get(draft_idx) else { return };
        let Some(fi) = self.files.iter().position(|f| f.path == d.file_path) else { return };
        // expand the file so the line is visible
        if !self.expanded.get(fi).copied().unwrap_or(true) {
            self.expanded[fi] = true;
            self.flat = flatten(&self.files, &self.expanded);
        }
        // find the line matching the draft anchor
        let target_idx = self.flat.iter().position(|fl| {
            if fl.file_idx != fi || fl.kind != FlatKind::Code {
                return false;
            }
            let Some(hi) = fl.hunk_idx else { return false };
            let Some(li) = fl.line_idx else { return false };
            let Some(line) = self.files[fi].hunks.get(hi).and_then(|h| h.lines.get(li)) else {
                return false;
            };
            line.old_lineno == d.old_lineno && line.new_lineno == d.new_lineno
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

    pub fn existing_draft_body_for_selection(&self) -> Option<String> {
        let keys = self.selection_lines();
        let (fi, hi, li) = *keys.first()?;
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
        let kind = match first_line.kind {
            LineKind::Added => "added",
            LineKind::Deleted => "deleted",
            LineKind::Context => "context",
        };
        let end_old = if keys.len() > 1 {
            last_line.old_lineno
        } else {
            None
        };
        let end_new = if keys.len() > 1 {
            last_line.new_lineno
        } else {
            None
        };

        if let Some(idx) = self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == first_line.new_lineno
                && d.old_lineno == first_line.old_lineno
        }) {
            self.drafts[idx].body = body;
            self.drafts[idx].created_at = Utc::now();
            self.drafts[idx].old_lineno_end = end_old;
            self.drafts[idx].new_lineno_end = end_new;
            self.drafts[idx].diff_snippet = snippet;
            return Some(());
        }
        self.drafts.push(Draft {
            file_path: file.path.clone(),
            old_lineno: first_line.old_lineno,
            new_lineno: first_line.new_lineno,
            old_lineno_end: end_old,
            new_lineno_end: end_new,
            line_kind: kind.to_string(),
            diff_snippet: snippet,
            body,
            created_at: Utc::now(),
            resolved: false,
            outdated: false,
            reactions: Vec::new(),
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

fn flatten(files: &[FileDiff], expanded: &[bool]) -> Vec<FlatLine> {
    let mut out = Vec::new();
    for (fi, file) in files.iter().enumerate() {
        out.push(FlatLine {
            kind: FlatKind::FileHeader,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
        });
        if expanded.get(fi).copied().unwrap_or(true) {
            for (hi, hunk) in file.hunks.iter().enumerate() {
                out.push(FlatLine {
                    kind: FlatKind::HunkHeader,
                    file_idx: fi,
                    hunk_idx: Some(hi),
                    line_idx: None,
                });
                for (li, _) in hunk.lines.iter().enumerate() {
                    out.push(FlatLine {
                        kind: FlatKind::Code,
                        file_idx: fi,
                        hunk_idx: Some(hi),
                        line_idx: Some(li),
                    });
                }
            }
        }
        out.push(FlatLine {
            kind: FlatKind::FileFooter,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
        });
        out.push(FlatLine {
            kind: FlatKind::Spacer,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
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
