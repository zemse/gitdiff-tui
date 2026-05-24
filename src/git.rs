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
            DiffSource::WorkingTree => "working tree (staged + unstaged) vs HEAD".to_string(),
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
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(anyhow!("git diff HEAD --quiet exited unexpectedly")),
    }
}

pub fn detect_source(root: &Path, override_range: Option<String>) -> Result<DiffSource> {
    if let Some(range) = override_range {
        let (base, head) = parse_range(&range)?;
        return Ok(DiffSource::Branch { base, head });
    }

    if has_working_changes(root)? {
        return Ok(DiffSource::WorkingTree);
    }

    let head = "HEAD".to_string();
    let base = resolve_base(root)?;
    Ok(DiffSource::Branch { base, head })
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

#[derive(Debug, Clone, Copy)]
pub struct DiffOpts {
    pub ignore_whitespace: bool,
    pub context_lines: usize,
}

impl Default for DiffOpts {
    fn default() -> Self {
        Self {
            ignore_whitespace: false,
            context_lines: 3,
        }
    }
}

pub fn get_diff(root: &Path, source: &DiffSource, opts: DiffOpts) -> Result<String> {
    let ctx = format!("-U{}", opts.context_lines);
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
    match source {
        DiffSource::WorkingTree => {
            let mut args = base_args;
            args.push("HEAD");
            run(&args, Some(root))
        }
        DiffSource::Branch { base, head } => {
            let merge_base = run(&["merge-base", base, head], Some(root))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| base.clone());
            let range = format!("{merge_base}..{head}");
            let mut args = base_args;
            args.push(&range);
            run(&args, Some(root))
        }
    }
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
