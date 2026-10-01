use std::fs;
use std::path::Path;

use agentos_engine::workspace::{copy_tree, workspace_digest};

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

#[test]
fn digest_is_independent_of_creation_order() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(a.path(), "src/x.py", "x");
    write(a.path(), "src/y.py", "y");
    write(a.path(), "z.txt", "z");
    write(b.path(), "z.txt", "z");
    write(b.path(), "src/y.py", "y");
    write(b.path(), "src/x.py", "x");
    assert_eq!(workspace_digest(a.path()).unwrap(), workspace_digest(b.path()).unwrap());
}

#[test]
fn digest_changes_with_content_and_with_rename() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "src/x.py", "x");
    let base = workspace_digest(d.path()).unwrap();
    write(d.path(), "src/x.py", "x2");
    let edited = workspace_digest(d.path()).unwrap();
    assert_ne!(base, edited);
    write(d.path(), "src/x.py", "x");
    assert_eq!(workspace_digest(d.path()).unwrap(), base);
    fs::rename(d.path().join("src/x.py"), d.path().join("src/w.py")).unwrap();
    assert_ne!(workspace_digest(d.path()).unwrap(), base);
}

#[test]
fn moving_a_file_between_directories_changes_the_digest() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(a.path(), "ab/c", "1");
    write(b.path(), "a/bc", "1");
    assert_ne!(workspace_digest(a.path()).unwrap(), workspace_digest(b.path()).unwrap());
}

#[test]
fn digest_ignores_caches_and_git_but_not_empty_files() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "src/x.py", "x");
    let base = workspace_digest(d.path()).unwrap();
    write(d.path(), "src/__pycache__/x.cpython-311.pyc", "bytecode");
    write(d.path(), "src/stray.pyc", "bytecode");
    write(d.path(), ".git/HEAD", "ref: refs/heads/main");
    assert_eq!(workspace_digest(d.path()).unwrap(), base);
    write(d.path(), "src/empty.py", "");
    assert_ne!(workspace_digest(d.path()).unwrap(), base);
}

#[test]
fn empty_directories_do_not_count() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "src/x.py", "x");
    let base = workspace_digest(d.path()).unwrap();
    fs::create_dir_all(d.path().join("empty/dir")).unwrap();
    assert_eq!(workspace_digest(d.path()).unwrap(), base);
}

#[test]
fn symlinks_are_an_error() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "src/x.py", "x");
    std::os::unix::fs::symlink("/etc", d.path().join("src/link")).unwrap();
    assert!(workspace_digest(d.path()).is_err());
}

#[test]
fn copy_tree_skips_excluded_entries_and_preserves_the_digest() {
    let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(from.path(), "src/x.py", "x");
    write(from.path(), "tests/t.py", "t");
    write(from.path(), "src/__pycache__/x.pyc", "b");
    write(from.path(), ".git/HEAD", "h");
    let dest = to.path().join("ws");
    let files = copy_tree(from.path(), &dest).unwrap();
    assert_eq!(files, vec!["src/x.py".to_string(), "tests/t.py".to_string()]);
    assert!(!dest.join(".git").exists());
    assert!(!dest.join("src/__pycache__").exists());
    assert_eq!(workspace_digest(&dest).unwrap(), workspace_digest(from.path()).unwrap());
}

#[test]
fn copy_tree_refuses_symlinks() {
    let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(from.path(), "src/x.py", "x");
    std::os::unix::fs::symlink("/etc/passwd", from.path().join("src/p")).unwrap();
    assert!(copy_tree(from.path(), &to.path().join("ws")).is_err());
}
