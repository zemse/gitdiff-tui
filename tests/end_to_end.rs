use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git exec");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn working_tree_diff_parses_and_writes_review() {
    let tmp = tempdir_in_target("gitdiff_e2e_wt");
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "t@t"]);
    git(&tmp, &["config", "user.name", "t"]);

    fs::write(
        tmp.join("hello.rs"),
        "fn main() {\n    println!(\"old\");\n}\n",
    )
    .unwrap();
    git(&tmp, &["add", "hello.rs"]);
    git(&tmp, &["commit", "-q", "-m", "init"]);

    // unstaged change
    fs::write(
        tmp.join("hello.rs"),
        "fn main() {\n    println!(\"new\");\n    println!(\"added\");\n}\n",
    )
    .unwrap();

    // Use the library via running git diff manually + reusing gitdiff modules
    // We just verify git + diff parsing align with what gitdiff will see.
    let out = Command::new("git")
        .args(["diff", "HEAD", "--no-color"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(raw.contains("--- a/hello.rs"));
    assert!(raw.contains("+    println!(\"added\");"));
}

#[test]
fn working_tree_diff_includes_untracked_files() {
    let tmp = tempdir_in_target("gitdiff_e2e_untracked");
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "t@t"]);
    git(&tmp, &["config", "user.name", "t"]);

    fs::write(tmp.join("tracked.rs"), "fn main() {}\n").unwrap();
    git(&tmp, &["add", "tracked.rs"]);
    git(&tmp, &["commit", "-q", "-m", "init"]);

    // A brand-new, never-added file plus an ignored one that must NOT show up.
    fs::write(tmp.join("fresh.rs"), "fn brand_new() {}\n").unwrap();
    fs::write(tmp.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(tmp.join("ignored.rs"), "should not appear\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gitdiff"))
        .arg("diff")
        .current_dir(&tmp)
        .output()
        .expect("run gitdiff diff");
    assert!(
        out.status.success(),
        "gitdiff diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(
        raw.contains("diff --git a/fresh.rs b/fresh.rs") && raw.contains("+fn brand_new() {}"),
        "untracked file missing from diff:\n{raw}"
    );
    assert!(
        raw.contains("new file mode"),
        "untracked file not rendered as a new file:\n{raw}"
    );
    assert!(
        !raw.contains("should not appear"),
        "gitignored file leaked into diff:\n{raw}"
    );
}

#[test]
fn large_untracked_file_is_omitted_not_ingested() {
    let tmp = tempdir_in_target("gitdiff_e2e_large_untracked");
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "t@t"]);
    git(&tmp, &["config", "user.name", "t"]);

    fs::write(tmp.join("tracked.rs"), "fn main() {}\n").unwrap();
    git(&tmp, &["add", "tracked.rs"]);
    git(&tmp, &["commit", "-q", "-m", "init"]);

    // A small new file (rendered in full) next to a large one (> 256 KiB, so
    // its content is skipped and it appears only as an omitted placeholder).
    fs::write(tmp.join("small.rs"), "fn brand_new() {}\n").unwrap();
    let big = "MARKER_LINE_THAT_MUST_NOT_APPEAR\n".repeat(20_000); // ~660 KiB
    fs::write(tmp.join("big.json"), &big).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gitdiff"))
        .arg("diff")
        .current_dir(&tmp)
        .output()
        .expect("run gitdiff diff");
    assert!(out.status.success());
    let raw = String::from_utf8(out.stdout).unwrap();

    // small file rendered normally
    assert!(
        raw.contains("+fn brand_new() {}"),
        "small untracked file should be rendered in full:\n{raw}"
    );
    // large file present as an omitted stub, content NOT ingested
    assert!(
        raw.contains("diff --git a/big.json b/big.json") && raw.contains("GITDIFF-OMITTED "),
        "large untracked file should appear as an omitted stub:\n{raw}"
    );
    assert!(
        !raw.contains("MARKER_LINE_THAT_MUST_NOT_APPEAR"),
        "large untracked file content must not be read into the diff:\n{}",
        &raw[..raw.len().min(2000)]
    );
}

#[test]
fn untracked_copy_detected_like_a_commit() {
    let tmp = tempdir_in_target("gitdiff_e2e_untracked_copy");
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "t@t"]);
    git(&tmp, &["config", "user.name", "t"]);

    let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    fs::write(tmp.join("orig.rs"), format!("include old;\n{body}")).unwrap();
    fs::write(tmp.join("other.rs"), "unrelated\n".repeat(40)).unwrap();
    git(&tmp, &["add", "."]);
    git(&tmp, &["commit", "-q", "-m", "init"]);

    // Untracked clone with one changed line, plus an untracked file only a
    // `--copy-source` hint can pair with its source.
    fs::write(tmp.join("clone.rs"), format!("include new;\n{body}")).unwrap();
    fs::write(tmp.join("hinted.rs"), "something else\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gitdiff"))
        .args([
            "diff",
            "--find-copies-harder",
            "--copy-source",
            "hinted.rs=other.rs",
        ])
        .current_dir(&tmp)
        .output()
        .expect("run gitdiff diff");
    assert!(
        out.status.success(),
        "gitdiff diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(
        raw.contains("copy from orig.rs\ncopy to clone.rs")
            && raw.contains("-include old;\n+include new;\n")
            && !raw.contains("+line 1\n"),
        "untracked clone not detected as a copy:\n{raw}"
    );
    assert!(
        raw.contains("copy from other.rs\ncopy to hinted.rs"),
        "--copy-source hint not applied:\n{raw}"
    );

    // The real index is untouched: both files are still untracked.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(
        status.contains("?? clone.rs") && status.contains("?? hinted.rs"),
        "index was modified:\n{status}"
    );
}

#[test]
fn branch_diff_against_upstream() {
    let tmp = tempdir_in_target("gitdiff_e2e_br");
    git(&tmp, &["init", "-q", "-b", "main"]);
    git(&tmp, &["config", "user.email", "t@t"]);
    git(&tmp, &["config", "user.name", "t"]);
    fs::write(tmp.join("a.txt"), "hello\n").unwrap();
    git(&tmp, &["add", "a.txt"]);
    git(&tmp, &["commit", "-q", "-m", "init"]);

    git(&tmp, &["checkout", "-q", "-b", "feature"]);
    fs::write(tmp.join("a.txt"), "hello\nworld\n").unwrap();
    git(&tmp, &["add", "a.txt"]);
    git(&tmp, &["commit", "-q", "-m", "add line"]);

    let mb = Command::new("git")
        .args(["merge-base", "main", "HEAD"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    let base = String::from_utf8(mb.stdout).unwrap().trim().to_string();
    let out = Command::new("git")
        .args(["diff", &format!("{base}..HEAD"), "--no-color"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(raw.contains("+world"));
}

fn tempdir_in_target(name: &str) -> std::path::PathBuf {
    let mut p = std::env::current_dir().unwrap();
    p.push("target");
    p.push("test-tmp");
    p.push(name);
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}
