use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub enum DiffSource {
    WorkingTree,
    Branch { base: String, head: String },
}

impl DiffSource {
    pub fn label(&self) -> String {
        match self {
            DiffSource::WorkingTree => {
                "working tree (staged + unstaged + untracked) vs HEAD".to_string()
            }
            DiffSource::Branch { base, head } => format!("{base}..{head}"),
        }
    }

    pub fn slug(&self) -> String {
        match self {
            DiffSource::WorkingTree => "working".to_string(),
            DiffSource::Branch { base, head } => sanitize(&format!("{base}..{head}")),
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn repo_root() -> Result<PathBuf> {
    let out = run(&["rev-parse", "--show-toplevel"], None)?;
    Ok(PathBuf::from(out.trim()))
}

pub fn has_working_changes(root: &Path) -> Result<bool> {
    let mut cmd = Command::new("git");
    cmd.args(["diff", "HEAD", "--quiet"]).current_dir(root);
    let status = cmd
        .status()
        .with_context(|| "failed to invoke `git diff HEAD --quiet`")?;
    let tracked = match status.code() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(anyhow!("git diff HEAD --quiet exited unexpectedly")),
    };
    if tracked {
        return Ok(true);
    }
    // No tracked changes — but brand-new untracked files (respecting .gitignore)
    // still count as something to review.
    Ok(!list_untracked(root)?.is_empty())
}

/// Untracked files honoring `.gitignore`, relative to the repo root.
fn list_untracked(root: &Path) -> Result<Vec<String>> {
    let out = run(&["ls-files", "--others", "--exclude-standard"], Some(root))?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

pub fn detect_source(root: &Path, override_range: Option<String>) -> Result<DiffSource> {
    if let Some(range) = override_range {
        let (base, head) = parse_range(&range)?;
        let head = canonicalize_head(root, &head);
        return Ok(DiffSource::Branch { base, head });
    }

    if has_working_changes(root)? {
        return Ok(DiffSource::WorkingTree);
    }

    let head = canonicalize_head(root, "HEAD");
    let base = resolve_base(root)?;
    Ok(DiffSource::Branch { base, head })
}

/// Resolve `HEAD` to the current branch name so the slug — and therefore the
/// `.gitdiff/threads-*.json` filename — is the same whether the user typed
/// `gitdiff` (auto-detect), `gitdiff upstream/master..HEAD`, or
/// `gitdiff upstream/master..<branch>`. Without this, CLI writes under one
/// slug and the TUI reads from another, hiding comments. Detached HEAD or
/// any non-`HEAD` ref is left unchanged.
fn canonicalize_head(root: &Path, head: &str) -> String {
    if head == "HEAD" {
        if let Some(branch) = current_branch(root) {
            return branch;
        }
    }
    head.to_string()
}

fn parse_range(s: &str) -> Result<(String, String)> {
    if let Some((b, h)) = s.split_once("..") {
        Ok((b.to_string(), h.to_string()))
    } else {
        Ok((s.to_string(), "HEAD".to_string()))
    }
}

fn current_branch(root: &Path) -> Option<String> {
    let out = run(&["rev-parse", "--abbrev-ref", "HEAD"], Some(root)).ok()?;
    let name = out.trim().to_string();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
}

fn resolve_base(root: &Path) -> Result<String> {
    let current = current_branch(root);
    let on_trunk = matches!(current.as_deref(), Some("main") | Some("master"));

    // Non-trunk branches: behave like a PR — base is main/master, not @{upstream}.
    // (an @{upstream} like origin/feature would diff the branch against itself)
    //
    // Probe order: `upstream/*` first so fork workflows (where `origin` points
    // at the user's fork and `upstream` at the canonical repo) diff against
    // the canonical trunk, not the fork's possibly-stale copy. Then `origin/*`
    // for the common solo workflow, then local `main`/`master` as a last resort.
    if !on_trunk {
        for candidate in [
            "upstream/main",
            "upstream/master",
            "origin/main",
            "origin/master",
            "main",
            "master",
        ] {
            if Some(candidate) == current.as_deref() {
                continue;
            }
            if run(&["rev-parse", "--verify", candidate], Some(root)).is_ok() {
                return Ok(candidate.to_string());
            }
        }
    }

    // Trunk (or no main/master nearby): fall back to @{upstream} for unpushed commits.
    if let Ok(out) = run(&["rev-parse", "--abbrev-ref", "@{upstream}"], Some(root)) {
        let t = out.trim();
        if !t.is_empty() && Some(t) != current.as_deref() {
            return Ok(t.to_string());
        }
    }

    let branch = current.as_deref().unwrap_or("HEAD");
    if on_trunk {
        Err(anyhow!(
            "nothing to review: on '{branch}' with no @{{upstream}} and no working changes — commit on a feature branch first, or pass <base>..<head>"
        ))
    } else {
        Err(anyhow!(
            "nothing to diff against: on '{branch}', but no main/master or @{{upstream}} found — pass <base>..<head> explicitly"
        ))
    }
}

#[derive(Debug, Clone)]
pub struct DiffOpts {
    pub ignore_whitespace: bool,
    pub context_lines: usize,
    /// `Some(threshold%)` enables git copy detection (`-C<n>%`).
    pub find_copies: Option<u8>,
    /// Let unchanged files be copy sources (`--find-copies-harder`).
    pub find_copies_harder: bool,
    /// Explicit `(dst, src)` pairs: diff `dst` against `src` as a copy,
    /// replacing whatever git produced for `dst`.
    pub copy_sources: Vec<(String, String)>,
}

impl Default for DiffOpts {
    fn default() -> Self {
        Self {
            ignore_whitespace: false,
            context_lines: 3,
            find_copies: None,
            find_copies_harder: false,
            copy_sources: Vec::new(),
        }
    }
}

/// Default similarity threshold for `--find-copies` / `--find-copies-harder`
/// without an explicit value, matching git's own `-C` default.
pub const DEFAULT_COPY_THRESHOLD: u8 = 50;

pub fn get_diff(root: &Path, source: &DiffSource, opts: &DiffOpts) -> Result<String> {
    let ctx = format!("-U{}", opts.context_lines);
    let copies = opts.find_copies.map(|t| format!("-C{t}%"));
    let mut base_args: Vec<&str> = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--find-renames",
        &ctx,
    ];
    if opts.ignore_whitespace {
        base_args.push("-w");
    }
    if let Some(c) = &copies {
        base_args.push(c);
        if opts.find_copies_harder {
            base_args.push("--find-copies-harder");
        }
    }
    let out = match source {
        DiffSource::WorkingTree => {
            let mut args = base_args;
            args.push("HEAD");
            working_tree_diff(root, &args)?
        }
        DiffSource::Branch { base, head } => {
            let range = format!("{}..{head}", merge_base(root, base, head));
            let mut args = base_args;
            args.push(&range);
            run(&args, Some(root))?
        }
    };
    apply_copy_sources(root, source, opts, out)
}

fn merge_base(root: &Path, base: &str, head: &str) -> String {
    run(&["merge-base", base, head], Some(root))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| base.to_string())
}

/// Swap the diff for each `--copy-source` destination with a diff against its
/// named source, marked up as a git copy so the parser shows `C src → dst`.
/// The source is read from the preimage (merge-base for a range, disk for the
/// working tree); the destination from the reviewed side.
fn apply_copy_sources(
    root: &Path,
    source: &DiffSource,
    opts: &DiffOpts,
    raw: String,
) -> Result<String> {
    if opts.copy_sources.is_empty() {
        return Ok(raw);
    }
    let ctx = format!("-U{}", opts.context_lines);
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        ctx,
    ];
    if opts.ignore_whitespace {
        args.push("-w".into());
    }
    let mut out = String::new();
    let mut replaced: Vec<&str> = Vec::new();
    for (dst, src) in &opts.copy_sources {
        let mut a = args.clone();
        let patch = match source {
            DiffSource::WorkingTree => {
                a.extend(["--no-index".into(), "--".into(), src.clone(), dst.clone()]);
                let a: Vec<&str> = a.iter().map(String::as_str).collect();
                run_no_index(&a, root)?.unwrap_or_default()
            }
            DiffSource::Branch { base, head } => {
                let mb = merge_base(root, base, head);
                a.extend([format!("{mb}:{src}"), format!("{head}:{dst}")]);
                let a: Vec<&str> = a.iter().map(String::as_str).collect();
                run(&a, Some(root))?
            }
        };
        out.push_str(&mark_as_copy(&patch, src, dst));
        replaced.push(dst);
    }
    let mut kept = String::new();
    for block in split_file_blocks(&raw) {
        let dst = crate::diff::parse(block)
            .ok()
            .and_then(|f| f.into_iter().next())
            .map(|f| f.path);
        if !dst.is_some_and(|d| replaced.contains(&d.as_str())) {
            kept.push_str(block);
        }
    }
    kept.push_str(&out);
    Ok(kept)
}

/// Rewrite a two-path patch's header into git's copy form. An identical pair
/// yields no patch from git, so emit a bare 100% copy header instead.
fn mark_as_copy(patch: &str, src: &str, dst: &str) -> String {
    let header = format!("diff --git a/{src} b/{dst}\n");
    let body = patch.split_once('\n').map(|(_, rest)| rest);
    match body {
        Some(rest) if patch.starts_with("diff --git ") => {
            format!("{header}copy from {src}\ncopy to {dst}\n{rest}")
        }
        _ => format!("{header}similarity index 100%\ncopy from {src}\ncopy to {dst}\n"),
    }
}

/// Split a multi-file patch into per-file chunks, each starting at its
/// `diff --git` line. Anything before the first header is dropped.
fn split_file_blocks(raw: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    let mut pos = 0;
    for line in raw.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(pos);
        }
        pos += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &s)| &raw[s..starts.get(i + 1).copied().unwrap_or(raw.len())])
        .collect()
}

/// `git diff HEAD` plus every untracked file. `git diff HEAD` only covers
/// tracked files, so untracked ones are marked intent-to-add (`git add -N`)
/// in a throwaway copy of the index and the diff runs against that. They then
/// show up as ordinary new files, and rename/copy detection treats them
/// exactly as it would in a commit. The real index is never touched.
fn working_tree_diff(root: &Path, args: &[&str]) -> Result<String> {
    let mut intent: Vec<String> = Vec::new();
    let mut stubs = String::new();
    for path in list_untracked(root)? {
        // Skip reading/diffing brand-new files above the size cap. A large
        // untracked blob (multi-MB JSON dumps, logs, build artifacts) would
        // otherwise be read into memory, parsed, and syntax-highlighted line
        // by line at startup — enough to freeze the TUI for minutes. Emit a
        // stub the parser turns into an `omitted` placeholder instead.
        match std::fs::symlink_metadata(root.join(&path)) {
            Ok(meta) if meta.is_file() && meta.len() > MAX_UNTRACKED_RENDER_BYTES => {
                stubs.push_str(&omitted_stub(&path, meta.len()));
            }
            // Directories here are nested repos; adding them would record a
            // gitlink, so leave them out like `git diff` itself does.
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => intent.push(path),
            Err(_) => {}
        }
    }
    let mut out = if intent.is_empty() {
        run(args, Some(root))?
    } else {
        let index = TempIndex::new(root)?;
        let mut list = intent.join("\0");
        list.push('\0');
        index.run(
            &[
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(&list),
        )?;
        index.run(args, None)?
    };
    out.push_str(&stubs);
    Ok(out)
}

/// A private copy of the repo's index, deleted on drop. Per-process name so a
/// TUI poll and a concurrent CLI call never share one.
struct TempIndex<'a> {
    root: &'a Path,
    path: PathBuf,
}

impl<'a> TempIndex<'a> {
    fn new(root: &'a Path) -> Result<Self> {
        let git_path = |p: &str| -> Result<PathBuf> {
            let out = run(&["rev-parse", "--git-path", p], Some(root))?;
            Ok(root.join(out.trim()))
        };
        let real = git_path("index")?;
        let path = git_path(&format!("gitdiff-index-{}", std::process::id()))?;
        if real.exists() {
            std::fs::copy(&real, &path)
                .with_context(|| format!("failed to copy index to {}", path.display()))?;
        }
        Ok(Self { root, path })
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("git")
            .args(args)
            .current_dir(self.root)
            .env("GIT_INDEX_FILE", &self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to invoke `git {}`", args.join(" ")))?;
        let mut pipe = child.stdin.take().expect("stdin is piped");
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes())?;
        }
        drop(pipe);
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8(out.stdout).context("git output not utf-8")
    }
}

impl Drop for TempIndex<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Untracked files larger than this are shown as a collapsed "not rendered"
/// placeholder rather than having their full content ingested and highlighted.
const MAX_UNTRACKED_RENDER_BYTES: u64 = 256 * 1024;

/// A synthetic "new file" diff carrying no content, plus a `GITDIFF-OMITTED`
/// marker line the parser reads to flag the file as omitted (size in bytes).
/// Shaped like git's own new-file header so the diff parser needs no special
/// casing beyond recognizing the marker.
fn omitted_stub(path: &str, bytes: u64) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\nGITDIFF-OMITTED {bytes}\n"
    )
}

/// Read the "new side" content of a file as a Vec of lines. For the working
/// tree source we read straight from disk; for branch comparisons we use
/// `git show <head>:<path>`. Returns None if the file can't be fetched
/// (deletion, binary, missing).
pub fn read_file_lines(root: &Path, source: &DiffSource, path: &str) -> Option<Vec<String>> {
    let raw = match source {
        DiffSource::WorkingTree => std::fs::read_to_string(root.join(path)).ok()?,
        DiffSource::Branch { head, .. } => {
            run(&["show", &format!("{head}:{path}")], Some(root)).ok()?
        }
    };
    Some(raw.lines().map(|s| s.to_string()).collect())
}

pub fn short_sha(root: &Path, refname: &str) -> Option<String> {
    run(&["rev-parse", "--short", refname], Some(root))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Like [`run`] but for `git diff --no-index`, which exits with code 1 (not 0)
/// when the two inputs differ — the normal, expected case here. Returns the
/// captured stdout (lossy UTF-8, to tolerate binary files) for exit 0 or 1, and
/// an error only for a genuine failure. Output for a binary file is the
/// "Binary files ... differ" line, which the parser flags as binary.
fn run_no_index(args: &[&str], cwd: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to invoke `git {}`", args.join(" ")))?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned())),
        _ => Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn run(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke `git {}`", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).context("git output not utf-8")
}
