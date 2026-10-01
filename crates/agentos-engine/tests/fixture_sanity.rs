use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {program}: {e}"))
}

fn unittest(repo: &Path) -> Output {
    run(repo, "python3", &["-m", "unittest"])
}

fn protected_check(repo: &Path) -> Output {
    let profile = fixtures().join("profiles/parser-checks-v1");
    let script = profile.join("check_parser.py");
    run(
        &profile,
        "python3",
        &[script.to_str().unwrap(), repo.to_str().unwrap()],
    )
}

#[test]
fn fixture_fails_before_patch_and_passes_after() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("parser-repo");
    copy_dir(&fixtures().join("parser-repo"), &repo);

    assert!(!unittest(&repo).status.success(), "visible tests should fail on the seeded bug");
    assert!(!protected_check(&repo).status.success(), "protected check should fail on the seeded bug");

    let patch = fixtures().join("parser-repo.fix.patch");
    let applied = run(&repo, "git", &["apply", patch.to_str().unwrap()]);
    assert!(
        applied.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&applied.stderr)
    );

    let ut = unittest(&repo);
    assert!(ut.status.success(), "unittest after fix: {}", String::from_utf8_lossy(&ut.stderr));
    let chk = protected_check(&repo);
    assert!(
        chk.status.success(),
        "check_parser after fix: {}{}",
        String::from_utf8_lossy(&chk.stdout),
        String::from_utf8_lossy(&chk.stderr)
    );
}

#[test]
fn fix_patch_touches_only_src() {
    let patch = fs::read_to_string(fixtures().join("parser-repo.fix.patch")).unwrap();
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|l| l.strip_prefix("+++ ").or_else(|| l.strip_prefix("--- ")))
        .filter(|p| *p != "/dev/null")
        .collect();
    assert!(!files.is_empty());
    for f in files {
        let path = f.split('\t').next().unwrap();
        assert!(
            path.starts_with("a/src/") || path.starts_with("b/src/"),
            "patch touches non-src path: {path}"
        );
    }
}

#[test]
fn profile_is_protected() {
    let raw = fs::read_to_string(fixtures().join("profiles/parser-checks-v1/profile.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["id"], "parser-checks-v1");
    assert_eq!(v["command"], serde_json::json!(["python3", "check_parser.py"]));
    assert_eq!(v["protected"], true);
}
