use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::ids::Digest;

/// Entries never part of a workspace: VCS metadata and Python bytecode caches. This one rule
/// drives both the digest and the patch denial in [`has_excluded_component`], so a patch can
/// never write a file the digest (and so the verified evidence) does not cover.
fn is_excluded(name: &OsStr) -> bool {
    name == ".git" || name == "__pycache__" || name.as_encoded_bytes().ends_with(b".pyc")
}

/// True if any component of the repo-relative `rel` is excluded from [`workspace_digest`].
pub fn has_excluded_component(rel: &str) -> bool {
    Path::new(rel).components().any(|c| is_excluded(c.as_os_str()))
}

/// Repo-relative paths of the excluded entries (`.git`, `__pycache__`, `*.pyc`) anywhere
/// under `root`, without descending into them or following symlinks.
pub fn excluded_entries(root: &Path) -> io::Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if is_excluded(&entry.file_name()) {
                out.push(path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned());
            } else if entry.file_type()?.is_dir() {
                walk(root, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

/// Removes every excluded entry under `root`; returns what was removed. The digest cannot
/// see these entries, so none may survive into an effect whose result it vouches for: a
/// bytecode cache could stand in for changed source, a `.git` could configure `git apply`.
pub fn purge_excluded(root: &Path) -> io::Result<Vec<String>> {
    let found = excluded_entries(root)?;
    for rel in &found {
        let path = root.join(rel);
        // A symlink named `.git` is removed as a link, never followed.
        if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(found)
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Sorted `(relative path, absolute path)` of the regular files under `root`.
pub fn list_files(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if is_excluded(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(invalid(format!("symlink in workspace: {}", path.display())));
            } else if ty.is_dir() {
                walk(root, &path, out)?;
            } else if ty.is_file() {
                let rel = path.strip_prefix(root).map_err(|e| invalid(e.to_string()))?;
                let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
                let rel = parts.ok_or_else(|| invalid(format!("non-UTF-8 path: {}", path.display())))?;
                out.push((rel.join("/"), path));
            } else {
                return Err(invalid(format!("unsupported file type: {}", path.display())));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

/// BLAKE3 over the sorted `(relative path, file digest)` listing of the regular files under
/// `dir`, skipping `.git`, `__pycache__` and `*.pyc`. Symlinks are an error. Paths use `/`
/// separators and are length-prefixed, so no two listings encode the same bytes.
pub fn workspace_digest(dir: &Path) -> io::Result<Digest> {
    let mut listing = Vec::new();
    for (rel, path) in list_files(dir)? {
        listing.extend_from_slice(&(rel.len() as u64).to_le_bytes());
        listing.extend_from_slice(rel.as_bytes());
        listing.extend_from_slice(Digest::of(&fs::read(path)?).as_bytes());
    }
    Ok(Digest::of(&listing))
}

/// Copies the regular files of `from` into `to` with the same exclusions as
/// [`workspace_digest`]; returns the sorted relative paths copied.
pub fn copy_tree(from: &Path, to: &Path) -> io::Result<Vec<String>> {
    let files = list_files(from)?;
    fs::create_dir_all(to)?;
    for (rel, src) in &files {
        let dest = to.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dest)?;
    }
    Ok(files.into_iter().map(|(rel, _)| rel).collect())
}

/// The first prefix of `rel` (inside `ws`) that is a symlink, if any.
pub fn symlink_on_path(ws: &Path, rel: &str) -> io::Result<Option<String>> {
    let mut cur = ws.to_path_buf();
    for comp in Path::new(rel).components() {
        let Component::Normal(name) = comp else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unexpected component in {rel}")));
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Ok(Some(cur.strip_prefix(ws).unwrap_or(&cur).display().to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}
