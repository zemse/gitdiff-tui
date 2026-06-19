use crate::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::git::{DiffOpts, DiffSource};
use crate::syntax::{Highlighter, Span as HSpan};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
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
    // Inline rendering of a thread anchored to the Code row above. `line_idx`
    // selects which sub-row of the thread is being rendered (0 = header,
    // 1..N = body lines, N+1 = reactions if any).
    ThreadRow,
    Spacer,
}

#[derive(Debug, Clone)]
pub struct FlatLine {
    pub kind: FlatKind,
    pub file_idx: usize,
    pub hunk_idx: Option<usize>,
    pub line_idx: Option<usize>,
    pub thread_idx: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
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
    /// inside REVIEW-*.md. Empty on threads loaded from pre-thread JSON; the
    /// startup loader backfills any missing IDs.
    #[serde(default)]
    pub thread_id: String,
    /// Conversation replies — parsed back from REVIEW-*.md `<!-- replies:tID -->`
    /// blocks on launch, then preserved through subsequent submits.
    #[serde(default)]
    pub replies: Vec<Reply>,
    /// Verbatim text of the anchor line at the time the thread was created.
    /// Used at launch to re-anchor the thread when surrounding code shifts the
    /// line numbers — we search the new diff for this exact content and snap
    /// the lineno to the closest match.
    #[serde(default)]
    pub anchor_content: String,
    /// Timestamp the user last said "I've seen this — stop nagging me" on
    /// a thread whose last message wasn't theirs. Suppresses the "awaiting
    /// your reply" purple highlight in the TUI as long as no newer reply
    /// arrives. A reply with a later `created_at` automatically re-arms the
    /// highlight without needing the user to un-ack first.
    #[serde(default)]
    pub acknowledged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    /// Free-form (e.g. "you", "claude-code", "codex"). Agents pick their own.
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub const REACTION_CYCLE: &[&str] = &["👍", "👎", "🎉", "😄", "❤️", "🚀", "👀", "❓"];

/// Author handle written into REVIEW-*.md for messages sent from this TUI.
/// Agents are instructed not to claim this name when replying.
pub const LOCAL_AUTHOR: &str = "you";

/// Default author for messages written through the CLI (`gitdiff comment`,
/// `gitdiff reply`). The CLI is the agent's entry point, so its writes default
/// to this rather than `LOCAL_AUTHOR` — otherwise an agent reply lands under
/// the human's identity and `needs_attention` wrongly treats the thread as
/// already answered, suppressing the "awaiting your reply" highlight. A human
/// driving the CLI can still pass `--author you`.
pub const AGENT_AUTHOR: &str = "agent";

/// A thread "needs your attention" when it's still open and the last message
/// in the conversation isn't yours — i.e. an agent (or someone else) replied
/// and you haven't responded. Acknowledging the thread (`m`) silences the
/// nag until a *newer* reply arrives, so users can end a conversation on
/// the other side's reply without having to leave a perfunctory "thanks".
pub fn needs_attention(d: &Thread) -> bool {
    if d.resolved {
        return false;
    }
    let Some(last) = d.replies.last() else {
        return false;
    };
    if last.author == LOCAL_AUTHOR {
        return false;
    }
    match d.acknowledged_at {
        Some(ack) if ack >= last.created_at => false,
        _ => true,
    }
}

/// Whether the human (`you`) authored the thread's last message. The original
/// post counts as the human's unless an agent opened the thread via the CLI
/// (which seeds an agent reply, see `cmd_comment`).
pub fn last_message_is_human(d: &Thread) -> bool {
    match d.replies.last() {
        Some(r) => r.author == LOCAL_AUTHOR,
        None => true,
    }
}

/// A thread is "awaiting an agent response" when it's unresolved and the human
/// spoke last — the mirror of `needs_attention` (which is from the human's
/// side). `gitdiff watch` surfaces these so an agent knows its backlog.
pub fn awaiting_agent_response(d: &Thread) -> bool {
    !d.resolved && last_message_is_human(d)
}

/// What the composer is operating on. The thread editing rules are:
///   * Your message is mutable only if nothing follows it in the thread.
///   * If the last message in the thread isn't yours, the composer starts a
///     new reply instead of editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerTarget {
    /// Brand-new comment on the current selection — no thread exists yet.
    NewThread,
    /// Editing the original comment body (only when no replies exist yet).
    EditThread(usize),
    /// Editing your own last reply.
    EditReply { thread_idx: usize, reply_idx: usize },
    /// Appending a fresh reply because the thread's last message isn't yours.
    NewReply(usize),
}

impl ComposerTarget {
    /// Which thread (if any) should hide while the composer is open. Only
    /// `EditThread` hides the box — for reply targets the thread stays visible
    /// so the user can see what they're responding to, and the composer
    /// renders just below it.
    pub fn hides_thread(&self) -> Option<usize> {
        match self {
            ComposerTarget::EditThread(idx) => Some(*idx),
            _ => None,
        }
    }

    /// The existing thread this composer is attached to, if any. `NewThread`
    /// has none (the thread isn't saved yet); every other variant edits or
    /// replies to a thread already in `state.threads`.
    pub fn thread_idx(&self) -> Option<usize> {
        match self {
            ComposerTarget::NewThread => None,
            ComposerTarget::EditThread(idx)
            | ComposerTarget::NewReply(idx)
            | ComposerTarget::EditReply {
                thread_idx: idx, ..
            } => Some(*idx),
        }
    }
}

/// An entry in the thread context menu. The exact set shown depends on the
/// thread's state (see `AppState::thread_menu_items`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadMenuAction {
    /// Open the composer to reply to / edit the thread (the old click action).
    Reply,
    /// Hide the thread from the inline diff by resolving it.
    Resolve,
    /// Un-resolve a previously-resolved thread.
    Reopen,
    /// Silence the "awaiting your reply" highlight.
    MarkRead,
    /// Re-arm the highlight after a mark-read.
    MarkUnread,
    /// Add the next reaction emoji.
    React,
    /// Delete the whole thread.
    Delete,
}

/// State for the floating thread context menu opened on a thread click.
#[derive(Debug, Clone)]
pub struct ThreadMenu {
    /// Index into `state.threads` the menu acts on.
    pub thread_idx: usize,
    /// Flat-row index the menu is anchored below (the clicked thread row).
    pub anchor_flat_idx: usize,
    /// Highlighted item for keyboard navigation.
    pub cursor: usize,
}

impl Thread {
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

/// A diff-reload-stable handle to a flat row. Captured before `replace_files`
/// re-flattens and resolved against the new `flat` afterwards so scroll/cursor
/// stay pinned to the same code. See `AppState::row_anchor`.
#[derive(Clone)]
struct RowAnchor {
    path: String,
    old: Option<usize>,
    new: Option<usize>,
    kind: FlatKind,
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
    /// A context menu (reply / resolve / mark read / react / delete) floating
    /// over a thread the user clicked. Replaces the old "click goes straight
    /// into the composer" behavior.
    ThreadMenu,
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

/// On-screen region that should reveal a tooltip when the mouse hovers over it.
/// Currently used by comment headers to expose the absolute timestamp behind
/// the "5m ago" relative label.
#[derive(Debug, Clone)]
pub struct HoverRegion {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub tooltip: String,
}

/// Compact "5s ago" / "12m ago" / "3h ago" / "4d ago" style label.
/// Anchored to `now` so callers can pass an injected clock in tests.
pub fn format_relative_time(dt: &DateTime<Utc>, now: &DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(*dt).num_seconds();
    if secs < 0 {
        // Clock skew or a future-dated record — fall back to a tiny label
        // rather than confusing the reader with "-3s ago".
        return "just now".to_string();
    }
    if secs < 5 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

/// `2026-05-24 17:05 IST` (or with a `+05:30` offset suffix on platforms where
/// `%Z` doesn't expand). Always in the user's local timezone so the displayed
/// wall-clock matches what their other tools show.
pub fn format_absolute_local(dt: &DateTime<Utc>) -> String {
    let local = dt.with_timezone(&Local);
    let tz = local.format("%Z").to_string();
    if tz.is_empty() || tz == "%Z" {
        local.format("%Y-%m-%d %H:%M %:z").to_string()
    } else {
        format!("{} {tz}", local.format("%Y-%m-%d %H:%M"))
    }
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
    pub threads: Vec<Thread>,
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
    pub viewed: HashMap<String, String>,
    pub show_tree: bool,
    pub show_threads_pane: bool,
    pub opts: DiffOpts,
    pub tab_width: usize,
    pub verdict: Verdict,
    pub threads_cursor: usize,
    pub picker: Option<FuzzyPicker>,
    pub body_x: u16,
    pub body_width: u16,
    // Dynamic composer popup height (rows, includes borders). Set by ui::draw
    // each frame based on the TextArea's content. 0 when not composing.
    pub composer_height: u16,
    // Index of the thread currently being edited in the composer. When Some,
    // rebuild_flat hides that thread's rows so the composer replaces (not
    // duplicates) the rendered comment. Derived from `composer_target` each
    // time the composer opens; cleared on close.
    pub editing_thread_idx: Option<usize>,
    // What the open composer is targeting (new thread, edit, reply). Set in
    // `open_composer`, consulted by the save path and the composer chrome.
    pub composer_target: Option<ComposerTarget>,
    // Geometry of the composer popup as drawn last frame (x, y, w, h). Used to
    // map mouse coordinates into the embedded TextArea for drag-select.
    pub composer_rect: Option<(u16, u16, u16, u16)>,
    // Geometry of the floating copy/comment menu shown after a drag selection.
    // Used for hit-testing clicks on its buttons.
    pub selection_menu_rect: Option<(u16, u16, u16, u16)>,
    // `body_width` last used when computing `flat`. Drives a re-flatten on
    // resize so thread sub-row counts match the new wrap.
    pub flat_for_body_width: u16,
    // mtime of `.gitdiff/threads-*.json` the last time we read or wrote it.
    // The TUI's idle tick compares it against the file's current mtime and
    // merges in fresh CLI writes (e.g. an agent ran `gitdiff reply` in
    // another shell) when they differ.
    pub last_threads_mtime: Option<std::time::SystemTime>,
    // Hash of the raw diff the last time we (re)loaded it. The idle tick
    // recomputes the diff's hash and, when it differs, reloads files in place
    // so edits/commits to the reviewed code re-render without restarting. None
    // until the first poll primes it.
    pub last_diff_fingerprint: Option<u64>,
    // The floating thread context menu, when open (Mode::ThreadMenu).
    pub thread_menu: Option<ThreadMenu>,
    // Geometry (x, y, w, h) of the thread menu as drawn last frame, for
    // hit-testing clicks on its items and detecting clicks outside it.
    pub thread_menu_rect: Option<(u16, u16, u16, u16)>,
    // Last reported mouse position. Drives hover tooltips on comment
    // timestamps. None until the user moves the mouse.
    pub hover_pos: Option<(u16, u16)>,
    // Screen regions registered during the current frame's render that should
    // pop a tooltip when the mouse is over them. Cleared at the start of every
    // frame, then re-populated by render passes. RefCell so the render
    // functions can keep their `&AppState` signature.
    pub hover_regions: RefCell<Vec<HoverRegion>>,
    // (thread_id, author, old_created_at) of replies the user has edited or
    // deleted locally since the last successful save. `save_threads_merging`
    // consults this so the on-disk version of an edited reply (still carrying
    // the pre-edit ts + body) isn't reanimated as a "new disk addition" and
    // re-appended after the edited copy. Cleared after every successful save.
    pub suppressed_disk_replies: HashSet<(String, String, chrono::DateTime<chrono::Utc>)>,
    // thread_ids the user has deleted locally since the last successful save.
    // Without this, the merge would see the still-on-disk thread, find no
    // memory match, and resurrect it as a "CLI addition".
    pub suppressed_thread_ids: HashSet<String>,
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
        threads: Vec<Thread>,
        viewed: HashMap<String, String>,
    ) -> Self {
        // viewed files start collapsed
        let expanded: Vec<bool> = files
            .iter()
            .map(|f| !viewed.contains_key(&f.path))
            .collect();
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
            threads,
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
            show_threads_pane: false,
            opts: DiffOpts::default(),
            tab_width: 4,
            verdict: Verdict::Comment,
            threads_cursor: 0,
            picker: None,
            body_x: 0,
            body_width: 80,
            composer_height: 0,
            editing_thread_idx: None,
            composer_target: None,
            composer_rect: None,
            selection_menu_rect: None,
            flat_for_body_width: 0,
            last_threads_mtime: None,
            last_diff_fingerprint: None,
            thread_menu: None,
            thread_menu_rect: None,
            hover_pos: None,
            hover_regions: RefCell::new(Vec::new()),
            suppressed_disk_replies: HashSet::new(),
            suppressed_thread_ids: HashSet::new(),
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
                thread_idx: None,
            });
            if self.expanded.get(fi).copied().unwrap_or(true) {
                for hi in 0..self.files[fi].hunks.len() {
                    // -- above button --
                    // Skip the above-button if THIS hunk is collapsed (the
                    // user has folded it; no need to offer expanding context
                    // around something they explicitly hid).
                    let hunk_collapsed = self.collapsed_hunks.contains(&(fi, hi));
                    let rem_above = if hunk_collapsed {
                        0
                    } else {
                        self.remaining_above(fi, hi)
                    };
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
                            thread_idx: None,
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
                        thread_idx: None,
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
                                thread_idx: None,
                            });
                        }
                    }

                    // -- code (with inline threads attached) --
                    for li in 0..self.files[fi].hunks[hi].lines.len() {
                        out.push(FlatLine {
                            kind: FlatKind::Code,
                            file_idx: fi,
                            hunk_idx: Some(hi),
                            line_idx: Some(li),
                            thread_idx: None,
                        });
                        // emit any threads anchored at this code line, inline
                        let line = &self.files[fi].hunks[hi].lines[li];
                        let threads_here: Vec<usize> = self
                            .threads
                            .iter()
                            .enumerate()
                            .filter(|(_, d)| {
                                d.file_path == self.files[fi].path
                                    && d.old_lineno == line.old_lineno
                                    && d.new_lineno == line.new_lineno
                            })
                            .map(|(i, _)| i)
                            .collect();
                        for ti in threads_here {
                            // Skip the thread currently being edited — the composer
                            // popup renders in its place.
                            if self.editing_thread_idx == Some(ti) {
                                continue;
                            }
                            // Resolved threads are hidden from the inline TUI;
                            // they still show in the side threads pane and in
                            // REVIEW-*.md's Resolved section.
                            if self.threads[ti].resolved {
                                continue;
                            }
                            let max_w = thread_text_width(self.body_width);
                            let body_lines = wrap_body(&self.threads[ti].body, max_w).len().max(1);
                            let has_react = !self.threads[ti].reactions.is_empty();
                            // Each reply contributes: 1 header divider row +
                            // its wrapped body rows.
                            let reply_rows: usize = self.threads[ti]
                                .replies
                                .iter()
                                .map(|r| 1 + wrap_body(&r.body, max_w).len().max(1))
                                .sum();
                            // 2 border rows (top + bottom) + body + replies + optional reactions.
                            let total_rows =
                                2 + body_lines + reply_rows + if has_react { 1 } else { 0 };
                            for sub in 0..total_rows {
                                out.push(FlatLine {
                                    kind: FlatKind::ThreadRow,
                                    file_idx: fi,
                                    hunk_idx: Some(hi),
                                    line_idx: Some(sub),
                                    thread_idx: Some(ti),
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
                                thread_idx: None,
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
                                thread_idx: None,
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
                                thread_idx: None,
                            });
                        } else if rem_below > 0 {
                            let count = 20.min(rem_below);
                            out.push(FlatLine {
                                kind: FlatKind::ExpandBtnBelow,
                                file_idx: fi,
                                hunk_idx: Some(hi),
                                line_idx: Some(count),
                                thread_idx: None,
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
                thread_idx: None,
            });
            out.push(FlatLine {
                kind: FlatKind::Spacer,
                file_idx: fi,
                hunk_idx: None,
                line_idx: None,
                thread_idx: None,
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
        let now_viewed = if self.viewed.contains_key(&path) {
            self.viewed.remove(&path);
            false
        } else {
            // Stamp the current diff-content hash so a later edit clears the
            // tick automatically on next launch.
            let hash = file_diff_hash(file);
            self.viewed.insert(path, hash);
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
            .filter(|f| self.viewed.contains_key(&f.path))
            .count()
    }

    /// Cache a file's content for accurate expansion bounds. Called lazily by
    /// the click handler before invoking expand_hunk.
    pub fn set_file_blob(&mut self, path: String, blob: Option<Vec<String>>) {
        self.file_blobs.insert(path, blob);
    }

    fn file_max_new_lineno(&self, fi: usize) -> Option<usize> {
        let path = self.files.get(fi)?.path.clone();
        self.file_blobs
            .get(&path)
            .and_then(|b| b.as_ref())
            .map(|v| v.len())
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
        let Some(_) = self.files.get(fi) else {
            return 0;
        };
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
                let content = lines.get(nl.saturating_sub(1)).cloned().unwrap_or_default();
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
                let content = lines.get(nl.saturating_sub(1)).cloned().unwrap_or_default();
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

    /// A stable handle to a flat row that survives a diff reload: the file it
    /// belongs to plus, for code rows, the (old, new) line numbers. Used to
    /// keep the viewport pinned to the same code when `replace_files`
    /// re-flattens after the underlying diff changed.
    fn row_anchor(&self, idx: usize) -> Option<RowAnchor> {
        let fl = self.flat.get(idx)?;
        let path = self.files.get(fl.file_idx)?.path.clone();
        let (old, new) = if fl.kind == FlatKind::Code {
            let line = self.files[fl.file_idx]
                .hunks
                .get(fl.hunk_idx?)?
                .lines
                .get(fl.line_idx?)?;
            (line.old_lineno, line.new_lineno)
        } else {
            (None, None)
        };
        Some(RowAnchor {
            path,
            old,
            new,
            kind: fl.kind,
        })
    }

    /// Find the flat index that best matches a previously-captured anchor in
    /// the freshly rebuilt `flat`. Returns None if the file or line vanished.
    fn find_row_anchor(&self, a: &RowAnchor) -> Option<usize> {
        let fi = self.files.iter().position(|f| f.path == a.path)?;
        if a.kind == FlatKind::Code {
            self.flat.iter().position(|fl| {
                if fl.file_idx != fi || fl.kind != FlatKind::Code {
                    return false;
                }
                let (Some(hi), Some(li)) = (fl.hunk_idx, fl.line_idx) else {
                    return false;
                };
                self.files[fi]
                    .hunks
                    .get(hi)
                    .and_then(|h| h.lines.get(li))
                    .is_some_and(|l| l.old_lineno == a.old && l.new_lineno == a.new)
            })
        } else {
            self.flat
                .iter()
                .position(|fl| fl.file_idx == fi && fl.kind == a.kind)
        }
    }

    pub fn replace_files(&mut self, files: Vec<FileDiff>) {
        // Capture where the viewport and cursor are pinned *before* we drop the
        // old flat, so we can restore them to the same code afterwards even
        // when line numbers shifted. Keeping the cursor's on-screen row lets us
        // fall back gracefully when its exact line was deleted.
        let scroll_anchor = self.row_anchor(self.scroll);
        let cursor_anchor = self.row_anchor(self.cursor);
        let cursor_row = self.cursor.saturating_sub(self.scroll);

        let expanded: Vec<bool> = files
            .iter()
            .map(|f| !self.viewed.contains_key(&f.path))
            .collect();
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
        let last = self.flat.len().saturating_sub(1);
        // Restore the viewport to the same code it was showing. We deliberately
        // do NOT call ensure_cursor_visible here: the scroll offset is pinned
        // to its anchor so a live reload doesn't yank the view to chase the
        // cursor — preserving the scroll position is the whole point.
        self.scroll = scroll_anchor
            .and_then(|a| self.find_row_anchor(&a))
            .unwrap_or_else(|| self.scroll.min(last));
        self.cursor = cursor_anchor
            .and_then(|a| self.find_row_anchor(&a))
            .unwrap_or_else(|| (self.scroll + cursor_row).min(last));
        self.clamp_scroll();
        self.clear_selection();
    }

    /// Clamp `scroll` to the valid range without moving it to follow the
    /// cursor. Used by reloads that want to keep the viewport pinned.
    fn clamp_scroll(&mut self) {
        let max_scroll = self.flat.len().saturating_sub(self.viewport_height.max(1));
        self.scroll = self.scroll.min(max_scroll);
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

    pub fn jump_to_thread(&mut self, thread_idx: usize) {
        // copy out the anchor data to release the immutable borrow before rebuild_flat
        let (anchor_path, anchor_old, anchor_new) = match self.threads.get(thread_idx) {
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

    /// Jumps the cursor to the next thread whose latest message isn't yours,
    /// wrapping to the top if there is none after the cursor. Returns whether
    /// it actually moved.
    pub fn jump_next_thread_needing_attention(&mut self) -> bool {
        // Build the list of flat indices that hold a thread anchor needing
        // attention, in document order. Then find the first one strictly
        // after the cursor; wrap to the first overall if none.
        let mut anchors: Vec<usize> = Vec::new();
        for (i, fl) in self.flat.iter().enumerate() {
            if fl.kind != FlatKind::ThreadRow || fl.line_idx != Some(0) {
                continue;
            }
            let Some(ti) = fl.thread_idx else { continue };
            if let Some(d) = self.threads.get(ti) {
                if needs_attention(d) {
                    anchors.push(i);
                }
            }
        }
        if anchors.is_empty() {
            return false;
        }
        let target = anchors
            .iter()
            .copied()
            .find(|&i| i > self.cursor)
            .unwrap_or(anchors[0]);
        self.cursor = target;
        self.cursor_visible = true;
        self.ensure_cursor_visible();
        true
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
        let Some(start_fl) = self.flat.get(s.start).cloned() else {
            return;
        };
        let Some(end_fl) = self.flat.get(idx).cloned() else {
            return;
        };
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
        let Some(s) = self.selection else {
            return Vec::new();
        };
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
            if let Some(l) = self
                .files
                .get(fi)
                .and_then(|f| f.hunks.get(hi))
                .and_then(|h| h.lines.get(li))
            {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&l.content);
            }
        }
        Some(out)
    }

    pub fn thread_for_selection(&self) -> Option<usize> {
        let keys = self.selection_lines();
        let (fi, hi, li) = *keys.last()?;
        let file = self.files.get(fi)?;
        let line = file.hunks.get(hi)?.lines.get(li)?;
        self.threads.iter().position(|d| {
            // Skip resolved threads: they're hidden from the inline diff, so a
            // comment on their old line is a brand-new thread, not an edit of
            // the hidden one (otherwise the "new" comment inherits `resolved`
            // and is invisible too).
            !d.resolved
                && d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        })
    }

    /// Returns `(thread_idx, is_anchor)` if `line` falls inside a thread's range.
    /// `is_anchor` is true for the row the thread renders on; the anchor is the
    /// *last* line of the original selection. The two stored linenos can
    /// appear in either order across older threads, so we treat the pair as an
    /// unordered min/max range when checking inclusion.
    pub fn thread_covering_line(&self, fi: usize, line: &DiffLine) -> Option<(usize, bool)> {
        let path = &self.files.get(fi)?.path;
        self.threads.iter().enumerate().find_map(|(idx, d)| {
            if &d.file_path != path {
                return None;
            }
            // Hidden from inline rendering when resolved — matches rebuild_flat
            // (no ThreadRow emitted), so the framing/marker must also be off.
            if d.resolved {
                return None;
            }
            if d.old_lineno == line.old_lineno && d.new_lineno == line.new_lineno {
                return Some((idx, true));
            }
            if let (Some(a), Some(b), Some(ln)) = (d.new_lineno, d.new_lineno_end, line.new_lineno)
            {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if ln >= lo && ln <= hi {
                    return Some((idx, false));
                }
            }
            if let (Some(a), Some(b), Some(ln)) = (d.old_lineno, d.old_lineno_end, line.old_lineno)
            {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if ln >= lo && ln <= hi {
                    return Some((idx, false));
                }
            }
            None
        })
    }

    pub fn thread_for_cursor(&self) -> Option<usize> {
        let (file, _, line) = self.current_line()?;
        self.threads.iter().position(|d| {
            d.file_path == file.path
                && d.new_lineno == line.new_lineno
                && d.old_lineno == line.old_lineno
        })
    }

    /// Picks the composer target for the current selection. The thread
    /// editing rules: you can only edit a message if it's yours and nothing
    /// follows it; otherwise the composer is a brand-new message.
    pub fn decide_composer_target(&self) -> ComposerTarget {
        let Some(idx) = self.thread_for_selection() else {
            return ComposerTarget::NewThread;
        };
        let d = &self.threads[idx];
        if d.replies.is_empty() {
            // The original comment is still the thread's last message. You wrote
            // it, so you can edit it.
            ComposerTarget::EditThread(idx)
        } else {
            let last = d.replies.len() - 1;
            if d.replies[last].author == LOCAL_AUTHOR {
                ComposerTarget::EditReply {
                    thread_idx: idx,
                    reply_idx: last,
                }
            } else {
                ComposerTarget::NewReply(idx)
            }
        }
    }

    /// Initial composer text for `target` — the existing body for edits, or
    /// empty for new messages.
    pub fn initial_composer_body(&self, target: ComposerTarget) -> String {
        match target {
            ComposerTarget::NewThread | ComposerTarget::NewReply(_) => String::new(),
            ComposerTarget::EditThread(idx) => self
                .threads
                .get(idx)
                .map(|d| d.body.clone())
                .unwrap_or_default(),
            ComposerTarget::EditReply {
                thread_idx,
                reply_idx,
            } => self
                .threads
                .get(thread_idx)
                .and_then(|d| d.replies.get(reply_idx))
                .map(|r| r.body.clone())
                .unwrap_or_default(),
        }
    }

    /// Commits `body` to whatever the open composer is targeting.
    pub fn save_composer(&mut self, target: ComposerTarget, body: String) -> bool {
        match target {
            ComposerTarget::NewThread => self.add_thread_from_selection(body).is_some(),
            ComposerTarget::EditThread(idx) => {
                if let Some(d) = self.threads.get_mut(idx) {
                    d.body = body;
                    d.created_at = Utc::now();
                    true
                } else {
                    false
                }
            }
            ComposerTarget::EditReply {
                thread_idx,
                reply_idx,
            } => {
                // Suppress the pre-edit identity so the merge-save doesn't
                // see the stale on-disk version and re-append it as a fresh
                // reply (which is exactly what made an "edit" look like a
                // new reply post-merging-save landed).
                if let Some(d) = self.threads.get(thread_idx) {
                    if let Some(r) = d.replies.get(reply_idx) {
                        self.suppressed_disk_replies.insert((
                            d.thread_id.clone(),
                            r.author.clone(),
                            r.created_at,
                        ));
                    }
                }
                if let Some(r) = self
                    .threads
                    .get_mut(thread_idx)
                    .and_then(|d| d.replies.get_mut(reply_idx))
                {
                    r.body = body;
                    r.created_at = Utc::now();
                    true
                } else {
                    false
                }
            }
            ComposerTarget::NewReply(idx) => {
                if let Some(d) = self.threads.get_mut(idx) {
                    d.replies.push(Reply {
                        author: LOCAL_AUTHOR.to_string(),
                        body,
                        created_at: Utc::now(),
                    });
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn add_thread_from_selection(&mut self, body: String) -> Option<()> {
        let keys = self.selection_lines();
        if keys.is_empty() {
            return None;
        }
        let (fi, _, _) = keys[0];
        let file = self.files.get(fi)?.clone();
        let first_key = *keys.first()?;
        let last_key = *keys.last()?;
        let first_line = file.hunks.get(first_key.1)?.lines.get(first_key.2)?.clone();
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

        // Only fold into an existing *open* thread on this line. A resolved
        // thread is hidden, so overwriting it would make the new comment
        // inherit `resolved` and vanish — match the renderer/selection logic
        // and start a fresh thread instead.
        if let Some(idx) = self.threads.iter().position(|d| {
            !d.resolved
                && d.file_path == file.path
                && d.new_lineno == last_line.new_lineno
                && d.old_lineno == last_line.old_lineno
        }) {
            self.threads[idx].body = body;
            self.threads[idx].created_at = Utc::now();
            self.threads[idx].old_lineno_end = other_old;
            self.threads[idx].new_lineno_end = other_new;
            self.threads[idx].diff_snippet = snippet;
            return Some(());
        }
        let created_at = Utc::now();
        let thread_id = make_thread_id(
            &file.path,
            last_line.old_lineno,
            last_line.new_lineno,
            &created_at,
        );
        self.threads.push(Thread {
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
            anchor_content: last_line.content.clone(),
            acknowledged_at: None,
        });
        Some(())
    }

    pub fn add_reaction_at_cursor(&mut self) -> Option<String> {
        let idx = self.thread_for_cursor()?;
        self.add_reaction(idx)
    }

    pub fn clear_reactions_at_cursor(&mut self) -> bool {
        let Some(idx) = self.thread_for_cursor() else {
            return false;
        };
        let had = !self.threads[idx].reactions.is_empty();
        self.threads[idx].reactions.clear();
        had
    }

    pub fn toggle_resolved_at_cursor(&mut self) -> Option<bool> {
        let idx = self.thread_for_cursor()?;
        self.threads[idx].resolved = !self.threads[idx].resolved;
        Some(self.threads[idx].resolved)
    }

    /// Toggle "I've seen this" on the thread at the cursor. Returns
    /// `Some(true)` when the thread is now silenced, `Some(false)` when the
    /// ack was cleared (re-arming the purple highlight), or `None` when
    /// there's no thread at the cursor. Acking a thread whose last reply
    /// is already yours is a no-op — there was nothing to silence.
    pub fn toggle_acknowledged_at_cursor(&mut self) -> Option<bool> {
        let idx = self.thread_for_cursor()?;
        self.toggle_acknowledged(idx)
    }

    pub fn mark_outdated_threads(&mut self) {
        // a thread is outdated if its anchor line no longer exists in the parsed diff
        for d in self.threads.iter_mut() {
            let Some(file) = self.files.iter().find(|f| f.path == d.file_path) else {
                d.outdated = true;
                continue;
            };
            let found = file.hunks.iter().any(|h| {
                h.lines
                    .iter()
                    .any(|l| l.old_lineno == d.old_lineno && l.new_lineno == d.new_lineno)
            });
            d.outdated = !found;
        }
    }

    pub fn delete_thread_at_cursor(&mut self) -> bool {
        if let Some(idx) = self.thread_for_cursor() {
            self.delete_thread(idx);
            true
        } else {
            false
        }
    }

    /// Delete the thread at `ti`, recording its id in the suppression set so a
    /// concurrent CLI write doesn't resurrect it on the next merge.
    pub fn delete_thread(&mut self, ti: usize) {
        if let Some(d) = self.threads.get(ti) {
            self.suppressed_thread_ids.insert(d.thread_id.clone());
            self.threads.remove(ti);
        }
    }

    /// Set the resolved flag on the thread at `ti`. Returns the new value.
    pub fn set_resolved(&mut self, ti: usize, resolved: bool) -> Option<bool> {
        let d = self.threads.get_mut(ti)?;
        d.resolved = resolved;
        Some(resolved)
    }

    /// Toggle "I've seen this" on the thread at `ti` (index variant of
    /// `toggle_acknowledged_at_cursor`). See that method for semantics.
    pub fn toggle_acknowledged(&mut self, ti: usize) -> Option<bool> {
        let d = self.threads.get_mut(ti)?;
        let last = d.replies.last()?;
        if last.author == LOCAL_AUTHOR {
            return None;
        }
        let last_ts = last.created_at;
        if matches!(d.acknowledged_at, Some(ack) if ack >= last_ts) {
            d.acknowledged_at = None;
            Some(false)
        } else {
            d.acknowledged_at = Some(Utc::now());
            Some(true)
        }
    }

    /// Add the next unused reaction emoji to the thread at `ti`.
    pub fn add_reaction(&mut self, ti: usize) -> Option<String> {
        let used: HashSet<&String> = self.threads.get(ti)?.reactions.iter().collect();
        let pick = REACTION_CYCLE
            .iter()
            .find(|r| !used.contains(&r.to_string()))
            .copied()
            .unwrap_or(REACTION_CYCLE[0]);
        self.threads[ti].reactions.push(pick.to_string());
        Some(pick.to_string())
    }

    /// The context-menu entries for the thread at `ti`, in display order. The
    /// resolve/reopen and mark-read/unread entries flip based on thread state;
    /// mark-read only appears when there's something to silence.
    pub fn thread_menu_items(&self, ti: usize) -> Vec<(String, ThreadMenuAction)> {
        let Some(d) = self.threads.get(ti) else {
            return Vec::new();
        };
        let mut items: Vec<(String, ThreadMenuAction)> =
            vec![("reply / edit".into(), ThreadMenuAction::Reply)];
        if d.resolved {
            items.push(("reopen".into(), ThreadMenuAction::Reopen));
        } else {
            items.push(("resolve (hide)".into(), ThreadMenuAction::Resolve));
        }
        if needs_attention(d) {
            items.push(("mark read".into(), ThreadMenuAction::MarkRead));
        } else if d.acknowledged_at.is_some() {
            items.push(("mark unread".into(), ThreadMenuAction::MarkUnread));
        }
        items.push(("react".into(), ThreadMenuAction::React));
        items.push(("delete".into(), ThreadMenuAction::Delete));
        items
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
            thread_idx: None,
        });
        if expanded.get(fi).copied().unwrap_or(true) {
            for (hi, hunk) in file.hunks.iter().enumerate() {
                out.push(FlatLine {
                    kind: FlatKind::HunkHeader,
                    file_idx: fi,
                    hunk_idx: Some(hi),
                    line_idx: None,
                    thread_idx: None,
                });
                for (li, _) in hunk.lines.iter().enumerate() {
                    out.push(FlatLine {
                        kind: FlatKind::Code,
                        file_idx: fi,
                        hunk_idx: Some(hi),
                        line_idx: Some(li),
                        thread_idx: None,
                    });
                }
            }
        }
        out.push(FlatLine {
            kind: FlatKind::FileFooter,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
            thread_idx: None,
        });
        out.push(FlatLine {
            kind: FlatKind::Spacer,
            file_idx: fi,
            hunk_idx: None,
            line_idx: None,
            thread_idx: None,
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
    let prefix = ac.iter().zip(bc.iter()).take_while(|(x, y)| x == y).count();
    let max_suffix = ac
        .len()
        .saturating_sub(prefix)
        .min(bc.len().saturating_sub(prefix));
    let mut suffix = 0;
    while suffix < max_suffix && ac[ac.len() - 1 - suffix] == bc[bc.len() - 1 - suffix] {
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

/// Drift-resistant re-anchor: when surrounding code has changed and the
/// stored anchor line no longer sits at the same lineno, search the current
/// diff for the verbatim `anchor_content` and snap the thread to the closest
/// match. Backfills `anchor_content` from the current anchor line for threads
/// loaded from older JSON that don't have it yet. Returns whether anything
/// changed (so main.rs can re-save the JSON).
pub fn reanchor_threads(threads: &mut [Thread], files: &[FileDiff]) -> bool {
    let mut changed = false;
    for d in threads.iter_mut() {
        let Some(file) = files.iter().find(|f| f.path == d.file_path) else {
            continue;
        };
        let anchor_now = file.hunks.iter().find_map(|h| {
            h.lines
                .iter()
                .find(|l| l.old_lineno == d.old_lineno && l.new_lineno == d.new_lineno)
        });
        // Backfill: a thread saved before the anchor_content field existed
        // adopts the current anchor's content as its cue.
        if d.anchor_content.is_empty() {
            if let Some(l) = anchor_now {
                d.anchor_content = l.content.clone();
                changed = true;
            } else {
                // CLI-created threads stored only one side (e.g. `--side new`
                // gave `(None, Some(line))`), so the strict `(old, new)`
                // match above misses context lines whose diff row is
                // `(Some, Some)`. Fall back to matching by the populated
                // side alone; if it's unique enough we adopt the diff row's
                // full pair and snap the thread to it.
                let target = d.new_lineno.or(d.old_lineno);
                let by_new = d.new_lineno.is_some();
                if let Some(l) = file.hunks.iter().find_map(|h| {
                    h.lines.iter().find(|l| {
                        if by_new {
                            l.new_lineno == target
                        } else {
                            l.old_lineno == target
                        }
                    })
                }) {
                    d.old_lineno = l.old_lineno;
                    d.new_lineno = l.new_lineno;
                    d.anchor_content = l.content.clone();
                    changed = true;
                }
            }
            continue;
        }
        // Same lineno still holds the same text → nothing to do.
        if anchor_now
            .map(|l| l.content == d.anchor_content)
            .unwrap_or(false)
        {
            continue;
        }
        // Otherwise scan all hunks for an exact content match; pick the
        // candidate whose lineno is closest to the stored anchor.
        let stored_ln = d.new_lineno.or(d.old_lineno).unwrap_or(0) as i64;
        let mut best: Option<(i64, Option<usize>, Option<usize>)> = None;
        for h in &file.hunks {
            for l in &h.lines {
                if l.content != d.anchor_content {
                    continue;
                }
                let ln = l.new_lineno.or(l.old_lineno).unwrap_or(0) as i64;
                let dist = (ln - stored_ln).abs();
                if best.map(|(b, _, _)| dist < b).unwrap_or(true) {
                    best = Some((dist, l.old_lineno, l.new_lineno));
                }
            }
        }
        if let Some((_, new_old, new_new)) = best {
            if new_old != d.old_lineno || new_new != d.new_lineno {
                d.old_lineno = new_old;
                d.new_lineno = new_new;
                changed = true;
            }
        }
    }
    changed
}

/// Hashes a file's diff content (all hunks + line numbers + kinds) so we can
/// detect when the diff for that file has changed. Used to expire the
/// per-file "viewed" tick automatically.
pub fn file_diff_hash(file: &FileDiff) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    file.path.hash(&mut h);
    for hk in &file.hunks {
        hk.old_start.hash(&mut h);
        hk.old_count.hash(&mut h);
        hk.new_start.hash(&mut h);
        hk.new_count.hash(&mut h);
        for l in &hk.lines {
            (l.kind as u8).hash(&mut h);
            l.content.hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

/// Generates a stable thread identifier for a freshly created thread. Combines
/// the anchor location with the creation timestamp so re-creating a thread on
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

/// Usable text width inside a rendered thread box for the given diff body
/// width (subtracts 2 borders and 1 leading-padding column).
pub fn thread_text_width(body_width: u16) -> usize {
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
