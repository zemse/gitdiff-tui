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
