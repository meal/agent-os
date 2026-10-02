use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use agentos_core::workspace::{list_files, symlink_on_path, workspace_digest};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn golden_fixture_digests_are_unchanged() {
    assert_eq!(
        workspace_digest(&fixtures().join("parser-repo")).unwrap().to_string(),
        "be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8"
    );
    assert_eq!(
        workspace_digest(&fixtures().join("profiles/parser-checks-v1")).unwrap().to_string(),
        "9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c"
    );
}

#[test]
fn list_files_is_sorted_relative_and_skips_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    for p in ["src/__pycache__", "src/b", ".git"] {
        fs::create_dir_all(r.join(p)).unwrap();
    }
    for p in ["src/__pycache__/x.pyc", ".git/HEAD", "src/b/c.py", "src/a.py", "src/z.pyc"] {
        fs::write(r.join(p), "x").unwrap();
    }
    let rels: Vec<String> = list_files(r).unwrap().into_iter().map(|(rel, _)| rel).collect();
    assert_eq!(rels, ["src/a.py", "src/b/c.py"]);
    symlink("/etc", r.join("src/link")).unwrap();
    assert!(list_files(r).is_err());
}

#[test]
fn symlink_on_path_reports_the_first_link_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    fs::create_dir_all(r.join("src")).unwrap();
    symlink("/etc", r.join("src/link")).unwrap();
    assert_eq!(symlink_on_path(r, "src/link/x.py").unwrap(), Some("src/link".to_string()));
    assert_eq!(symlink_on_path(r, "src/plain.py").unwrap(), None);
    assert_eq!(symlink_on_path(r, "nope/deeper/x.py").unwrap(), None);
}
