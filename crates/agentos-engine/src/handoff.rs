//! Materializing a task's handoff: the verified final workspace, rebuilt from the snapshot the
//! task started from and the patches it applied.

use std::fs;
use std::path::Path;

use crate::export::Handoff;
use crate::patch::{PathsError, git, paths_of_file};
use crate::workspace::{copy_tree, has_excluded_component, purge_excluded, workspace_digest};

/// Why a handoff could not be materialized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandoffError {
    /// The recorded handoff does not rebuild the verified workspace: trying again cannot help.
    #[error("{0}")]
    Mismatch(String),
    /// The machine failed on the way (I/O, running git): a later attempt may succeed.
    #[error("{0}")]
    Io(String),
}

use HandoffError::{Io, Mismatch};

/// Copies `snapshot` to `out` (which must not exist), applies the handoff's patch and checks
/// that the result is the workspace the handoff names. Any failure removes `out`.
pub async fn materialize(snapshot: &Path, h: &Handoff, out: &Path) -> Result<(), HandoffError> {
    if fs::symlink_metadata(out).is_ok() {
        return Err(Io(format!("handoff destination {} exists", out.display())));
    }
    let built = build(snapshot, h, out).await;
    if built.is_err() {
        let _ = fs::remove_dir_all(out);
    }
    built
}

async fn build(snapshot: &Path, h: &Handoff, out: &Path) -> Result<(), HandoffError> {
    copy_tree(snapshot, out).map_err(|e| Io(format!("cannot copy snapshot: {e}")))?;
    let base = workspace_digest(out).map_err(|e| Io(format!("cannot digest workspace: {e}")))?;
    if base != h.base {
        return Err(Mismatch(format!(
            "handoff base mismatch: expected {}, snapshot is {base}",
            h.base
        )));
    }
    if !h.patch.is_empty() {
        // The patch file lives next to, never inside, the workspace.
        let parent = out.parent().filter(|p| !p.as_os_str().is_empty());
        let scratch = tempfile::tempdir_in(parent.unwrap_or(Path::new(".")))
            .map_err(|e| Io(format!("scratch dir: {e}")))?;
        let patch_file = scratch.path().join("handoff.patch");
        fs::write(&patch_file, &h.patch).map_err(|e| Io(format!("cannot write patch: {e}")))?;
        // The same screening as the fixture executor: unacceptable patch kinds (symlink modes
        // and the like) and paths the workspace digest does not cover, which could otherwise
        // leave unaccounted files in `out`. `symlink_on_path` is not needed: a snapshot cannot
        // contain symlinks, and a patch that creates one is refused by this screening.
        let paths = paths_of_file(&patch_file, scratch.path())
            .await
            .map_err(|e| match e {
                PathsError::Git(m) => Io(m),
                PathsError::Refused(m) => Mismatch(m),
            })?;
        if let Some(p) = paths.iter().find(|p| has_excluded_component(p)) {
            return Err(Mismatch(format!(
                "path excluded from the workspace digest: {p}"
            )));
        }
        // A planted `.git` would make `git apply` honour its repo-local configuration.
        purge_excluded(out).map_err(|e| Io(format!("cannot clean workspace: {e}")))?;
        for args in [&["apply", "--check"][..], &["apply"][..]] {
            let res = git(out)
                .args(args)
                .arg(&patch_file)
                .output()
                .await
                .map_err(|e| Io(format!("cannot run git: {e}")))?;
            if !res.status.success() {
                return Err(Mismatch(format!(
                    "patch does not apply: {}",
                    String::from_utf8_lossy(&res.stderr).trim()
                )));
            }
        }
    }
    // Belt and braces: nothing the digest ignores may remain in `out`.
    let leftovers = purge_excluded(out).map_err(|e| Io(format!("cannot clean workspace: {e}")))?;
    if let Some(p) = leftovers.first() {
        return Err(Mismatch(format!("patch left excluded path {p}")));
    }
    let result = workspace_digest(out).map_err(|e| Io(format!("cannot digest workspace: {e}")))?;
    if result != h.final_digest {
        return Err(Mismatch(format!(
            "handoff result mismatch: expected {}, got {result}",
            h.final_digest
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use agentos_core::ids::Digest;

    use super::{HandoffError, materialize};
    use crate::export::Handoff;
    use crate::workspace::workspace_digest;

    const PATCH: &str = "--- a/src/a.py\n+++ b/src/a.py\n@@ -1,2 +1,2 @@\n x = 1\n-y = 2\n+y = 3\n";

    fn tree(root: &Path, second: &str) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.py"), format!("x = 1\n{second}\n")).unwrap();
    }

    /// A snapshot, and the handoff that turns it into the tree with `y = 3`.
    fn setup(dir: &Path) -> (std::path::PathBuf, Handoff) {
        let snap = dir.join("snap");
        tree(&snap, "y = 2");
        let expected = dir.join("expected");
        tree(&expected, "y = 3");
        let h = Handoff {
            base: workspace_digest(&snap).unwrap(),
            final_digest: workspace_digest(&expected).unwrap(),
            patch: PATCH.as_bytes().to_vec(),
        };
        (snap, h)
    }

    #[tokio::test]
    async fn materialize_produces_the_final_workspace_and_leaves_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, h) = setup(dir.path());
        let out = dir.path().join("out");

        materialize(&snap, &h, &out).await.unwrap();

        assert_eq!(workspace_digest(&out).unwrap(), h.final_digest);
        assert_eq!(workspace_digest(&snap).unwrap(), h.base);
        assert_eq!(
            fs::read_to_string(out.join("src/a.py")).unwrap(),
            "x = 1\ny = 3\n"
        );
    }

    #[tokio::test]
    async fn an_empty_patch_gives_a_copy_of_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, mut h) = setup(dir.path());
        h.patch.clear();
        h.final_digest = h.base;
        let out = dir.path().join("out");

        materialize(&snap, &h, &out).await.unwrap();

        assert_eq!(workspace_digest(&out).unwrap(), h.base);
    }

    #[tokio::test]
    async fn a_wrong_final_digest_is_an_error_and_leaves_no_out() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, mut h) = setup(dir.path());
        h.final_digest = Digest::of(b"something else");
        let out = dir.path().join("out");

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Mismatch(_)), "{err:?}");
        let err = err.to_string();

        assert!(err.contains("handoff result mismatch"), "{err}");
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn a_snapshot_that_is_not_the_base_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, h) = setup(dir.path());
        fs::write(snap.join("src/a.py"), "x = 9\n").unwrap();
        let out = dir.path().join("out");

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Mismatch(_)), "{err:?}");
        let err = err.to_string();

        assert!(err.contains("handoff base mismatch"), "{err}");
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn a_patch_that_does_not_apply_is_an_error_and_leaves_no_out() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, mut h) = setup(dir.path());
        h.patch = PATCH.replace("-y = 2", "-y = 7").into_bytes();
        let out = dir.path().join("out");

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Mismatch(_)), "{err:?}");
        let err = err.to_string();

        assert!(err.contains("patch does not apply"), "{err}");
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn a_snapshot_that_cannot_be_read_is_an_io_error_not_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, h) = setup(dir.path());
        fs::remove_dir_all(&snap).unwrap();
        let out = dir.path().join("out");

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("cannot copy snapshot"), "{err}");
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn an_existing_out_is_refused_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, h) = setup(dir.path());
        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("keep"), "mine").unwrap();

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Io(_)), "{err:?}");
        let err = err.to_string();

        assert!(err.contains("exists"), "{err}");
        assert_eq!(fs::read_to_string(out.join("keep")).unwrap(), "mine");
    }

    #[tokio::test]
    async fn a_planted_git_dir_in_the_snapshot_does_not_survive_the_patch() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, _) = setup(dir.path());
        let planted = dir.path().join("planted");
        tree(&planted, "y = 2");
        fs::create_dir(planted.join(".git")).unwrap();
        fs::write(planted.join(".git/config"), "[core]\n").unwrap();
        // `.git` is excluded from the digest, so the base is unchanged by it.
        let h = Handoff {
            base: workspace_digest(&snap).unwrap(),
            final_digest: setup(&dir.path().join("x")).1.final_digest,
            patch: PATCH.as_bytes().to_vec(),
        };
        let out = dir.path().join("out");

        materialize(&planted, &h, &out).await.unwrap();

        assert!(!out.join(".git").exists());
    }

    /// A git-format patch creating `path`: an operation the screening accepts, so only the
    /// exclusion guard stands between it and the workspace.
    fn add_file_patch(path: &str) -> Vec<u8> {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn a_patch_adding_digest_excluded_files_is_refused() {
        for path in ["src/__pycache__/x.pyc", "src/m.pyc"] {
            let dir = tempfile::tempdir().unwrap();
            let (snap, mut h) = setup(dir.path());
            h.patch = add_file_patch(path);
            let out = dir.path().join("out");

            let err = materialize(&snap, &h, &out).await.unwrap_err();

            assert!(matches!(err, HandoffError::Mismatch(_)), "{path}: {err:?}");
            assert!(
                err.to_string()
                    .contains(&format!("path excluded from the workspace digest: {path}")),
                "{path}: {err}"
            );
            assert!(!out.exists(), "{path}");
        }
    }

    #[tokio::test]
    async fn a_patch_creating_a_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (snap, mut h) = setup(dir.path());
        h.patch = b"diff --git a/ln b/ln\nnew file mode 120000\n--- /dev/null\n+++ b/ln\n@@ -0,0 +1 @@\n+target\n\\ No newline at end of file\n".to_vec();
        let out = dir.path().join("out");

        let err = materialize(&snap, &h, &out).await.unwrap_err();

        assert!(matches!(err, HandoffError::Mismatch(_)), "{err:?}");
        assert!(!out.exists());
    }
}
