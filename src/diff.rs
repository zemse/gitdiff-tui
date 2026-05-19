use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub header_context: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    pub fn header_text(&self) -> String {
        let ctx = if self.header_context.is_empty() {
            String::new()
        } else {
            format!(" {}", self.header_context)
        };
        format!(
            "@@ -{},{} +{},{} @@{}",
            self.old_start, self.old_count, self.new_start, self.new_count, ctx
        )
    }
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
}

pub fn parse(input: &str) -> Result<Vec<FileDiff>> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut iter = input.lines().peekable();

    while let Some(line) = iter.peek().copied() {
        if !line.starts_with("diff --git ") {
            iter.next();
            continue;
        }
        let header = iter.next().unwrap();
        let (a, b) = parse_diff_header(header)?;

        let mut old_path = Some(a);
        let mut new_path = b;
        let mut status = FileStatus::Modified;
        let mut binary = false;
        let mut hunks: Vec<Hunk> = Vec::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;

        // metadata lines until first hunk or next file
        while let Some(&l) = iter.peek() {
            if l.starts_with("diff --git ") {
                break;
            }
            if l.starts_with("@@ ") {
                break;
            }
            iter.next();
            if l.starts_with("new file mode") {
                status = FileStatus::Added;
                old_path = None;
            } else if l.starts_with("deleted file mode") {
                status = FileStatus::Deleted;
            } else if l.starts_with("rename from ") {
                status = FileStatus::Renamed;
                old_path = Some(l.trim_start_matches("rename from ").to_string());
            } else if l.starts_with("rename to ") {
                new_path = l.trim_start_matches("rename to ").to_string();
            } else if l.starts_with("copy from ") {
                status = FileStatus::Copied;
                old_path = Some(l.trim_start_matches("copy from ").to_string());
            } else if l.starts_with("copy to ") {
                new_path = l.trim_start_matches("copy to ").to_string();
            } else if l.starts_with("--- ") {
                let p = l.trim_start_matches("--- ");
                if p != "/dev/null" {
                    old_path = Some(strip_ab_prefix(p).to_string());
                } else {
                    old_path = None;
                }
            } else if l.starts_with("+++ ") {
                let p = l.trim_start_matches("+++ ");
                if p != "/dev/null" {
                    new_path = strip_ab_prefix(p).to_string();
                }
            } else if l.starts_with("Binary files ")
                || l.contains(" differ") && l.contains("Binary")
            {
                binary = true;
            }
        }

        // hunks
        while let Some(&l) = iter.peek() {
            if l.starts_with("diff --git ") {
                break;
            }
            if !l.starts_with("@@ ") {
                iter.next();
                continue;
            }
            let hunk_header = iter.next().unwrap();
            let (old_start, old_count, new_start, new_count, ctx) = parse_hunk_header(hunk_header)?;
            let mut h = Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                header_context: ctx,
                lines: Vec::new(),
            };
            let mut old_n = old_start;
            let mut new_n = new_start;
            while let Some(&body) = iter.peek() {
                if body.starts_with("diff --git ") || body.starts_with("@@ ") {
                    break;
                }
                iter.next();
                if body.starts_with("\\ ") {
                    // "\ No newline at end of file"
                    continue;
                }
                let (kind, content) = if let Some(rest) = body.strip_prefix('+') {
                    (LineKind::Added, rest.to_string())
                } else if let Some(rest) = body.strip_prefix('-') {
                    (LineKind::Deleted, rest.to_string())
                } else if let Some(rest) = body.strip_prefix(' ') {
                    (LineKind::Context, rest.to_string())
                } else if body.is_empty() {
                    (LineKind::Context, String::new())
                } else {
                    // tolerate unknown — treat as context
                    (LineKind::Context, body.to_string())
                };
                let (ol, nl) = match kind {
                    LineKind::Added => {
                        let n = new_n;
                        new_n += 1;
                        additions += 1;
                        (None, Some(n))
                    }
                    LineKind::Deleted => {
                        let o = old_n;
                        old_n += 1;
                        deletions += 1;
                        (Some(o), None)
                    }
                    LineKind::Context => {
                        let o = old_n;
                        let n = new_n;
                        old_n += 1;
                        new_n += 1;
                        (Some(o), Some(n))
                    }
                };
                h.lines.push(DiffLine {
                    kind,
                    old_lineno: ol,
                    new_lineno: nl,
                    content,
                });
            }
            hunks.push(h);
        }

        let final_path = if status == FileStatus::Deleted {
            old_path.clone().unwrap_or(new_path.clone())
        } else {
            new_path
        };
        files.push(FileDiff {
            path: final_path,
            old_path: if status == FileStatus::Renamed || status == FileStatus::Copied {
                old_path
            } else {
                None
            },
            status,
            hunks,
            additions,
            deletions,
            binary,
        });
    }

    Ok(files)
}

fn strip_ab_prefix(p: &str) -> &str {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
}

fn parse_diff_header(line: &str) -> Result<(String, String)> {
    // "diff --git a/foo b/foo"
    let rest = line
        .strip_prefix("diff --git ")
        .ok_or_else(|| anyhow!("not a diff header: {line}"))?;
    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
    if parts.len() != 2 {
        return Err(anyhow!("malformed diff header: {line}"));
    }
    Ok((
        strip_ab_prefix(parts[0]).to_string(),
        strip_ab_prefix(parts[1]).to_string(),
    ))
}

fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize, String)> {
    // "@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@ [ctx]"
    let rest = line
        .strip_prefix("@@ ")
        .ok_or_else(|| anyhow!("not a hunk header: {line}"))?;
    let (range_part, ctx) = rest
        .split_once(" @@")
        .ok_or_else(|| anyhow!("malformed hunk header: {line}"))?;
    let mut iter = range_part.split_whitespace();
    let old = iter
        .next()
        .ok_or_else(|| anyhow!("missing old range: {line}"))?;
    let new = iter
        .next()
        .ok_or_else(|| anyhow!("missing new range: {line}"))?;
    let (old_start, old_count) = parse_range(old.trim_start_matches('-'))?;
    let (new_start, new_count) = parse_range(new.trim_start_matches('+'))?;
    Ok((
        old_start,
        old_count,
        new_start,
        new_count,
        ctx.trim().to_string(),
    ))
}

fn parse_range(s: &str) -> Result<(usize, usize)> {
    if let Some((a, b)) = s.split_once(',') {
        Ok((a.parse()?, b.parse()?))
    } else {
        Ok((s.parse()?, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/foo.rs b/src/foo.rs
index abc1234..def5678 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,4 +1,5 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"added\");
 }
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+fn brand_new() {}
+
";

    #[test]
    fn parses_two_files() {
        let files = parse(SAMPLE).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/foo.rs");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[1].path, "src/new.rs");
        assert_eq!(files[1].status, FileStatus::Added);
        assert_eq!(files[1].additions, 2);
    }

    #[test]
    fn line_numbers_track_correctly() {
        let files = parse(SAMPLE).unwrap();
        let h = &files[0].hunks[0];
        // context line "fn main() {": old 1, new 1
        assert_eq!(h.lines[0].kind, LineKind::Context);
        assert_eq!(h.lines[0].old_lineno, Some(1));
        assert_eq!(h.lines[0].new_lineno, Some(1));
        // deletion "println!(\"old\");": old 2, no new
        assert_eq!(h.lines[1].kind, LineKind::Deleted);
        assert_eq!(h.lines[1].old_lineno, Some(2));
        assert_eq!(h.lines[1].new_lineno, None);
        // addition "println!(\"new\");": no old, new 2
        assert_eq!(h.lines[2].kind, LineKind::Added);
        assert_eq!(h.lines[2].new_lineno, Some(2));
    }

    #[test]
    fn parses_rename() {
        let raw = "\
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
index 1111111..2222222 100644
--- a/old.txt
+++ b/new.txt
@@ -1,2 +1,2 @@
 keep
-old
+new
";
        let files = parse(raw).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
    }
}
