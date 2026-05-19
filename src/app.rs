use crate::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::git::DiffSource;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatKind {
    FileHeader,
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
    pub line_kind: String, // "added" | "deleted" | "context"
    pub diff_snippet: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

impl Draft {
    pub fn anchor_label(&self) -> String {
        match (self.new_lineno, self.old_lineno) {
            (Some(n), _) => format!("L{n}"),
            (None, Some(o)) => format!("L{o} (pre-change)"),
            _ => "L?".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Composing,
    Help,
}

pub struct AppState {
    pub source: DiffSource,
    pub source_label: String,
    pub files: Vec<FileDiff>,
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
}

impl AppState {
    pub fn new(
        source: DiffSource,
        source_label: String,
        files: Vec<FileDiff>,
        drafts: Vec<Draft>,
    ) -> Self {
        let flat = flatten(&files);
        let total_additions = files.iter().map(|f| f.additions).sum();
        let total_deletions = files.iter().map(|f| f.deletions).sum();
        let mut s = Self {
            source,
            source_label,
            files,
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
        };
        // start on first code line if possible
        if let Some(i) = s.flat.iter().position(|l| l.kind == FlatKind::Code) {
            s.cursor = i;
        }
        s
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

    pub fn draft_for_cursor(&self) -> Option<usize> {
        let (file, _, line) = self.current_line()?;
        self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        })
    }

    pub fn add_draft(&mut self, body: String) -> Option<()> {
        let (file, hunk, line) = self.current_line()?;
        let snippet = build_snippet(hunk, line);
        let kind = match line.kind {
            LineKind::Added => "added",
            LineKind::Deleted => "deleted",
            LineKind::Context => "context",
        };
        // if existing draft at this anchor, replace body
        if let Some(idx) = self.drafts.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        }) {
            self.drafts[idx].body = body;
            self.drafts[idx].created_at = Utc::now();
            return Some(());
        }
        self.drafts.push(Draft {
            file_path: file.path.clone(),
            old_lineno: line.old_lineno,
            new_lineno: line.new_lineno,
            line_kind: kind.to_string(),
            diff_snippet: snippet,
            body,
            created_at: Utc::now(),
        });
        Some(())
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

fn flatten(files: &[FileDiff]) -> Vec<FlatLine> {
    let mut out = Vec::new();
    for (fi, file) in files.iter().enumerate() {
        out.push(FlatLine {
            kind: FlatKind::FileHeader,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
        });
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
        out.push(FlatLine {
            kind: FlatKind::Spacer,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
        });
    }
    out
}

fn build_snippet(hunk: &Hunk, target: &DiffLine) -> String {
    let target_idx = hunk
        .lines
        .iter()
        .position(|l| {
            l.old_lineno == target.old_lineno
                && l.new_lineno == target.new_lineno
                && l.kind == target.kind
        })
        .unwrap_or(0);
    let start = target_idx.saturating_sub(3);
    let end = (target_idx + 4).min(hunk.lines.len());
    let mut out = String::new();
    out.push_str(&hunk.header_text());
    out.push('\n');
    for l in &hunk.lines[start..end] {
        let prefix = match l.kind {
            LineKind::Added => '+',
            LineKind::Deleted => '-',
            LineKind::Context => ' ',
        };
        out.push(prefix);
        out.push_str(&l.content);
        out.push('\n');
    }
    out
}
