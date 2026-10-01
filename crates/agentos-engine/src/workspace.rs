use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;

use agentos_core::ids::Digest;

/// Entries never part of a workspace: VCS metadata and Python bytecode caches.
fn is_excluded(name: &OsStr) -> bool {
    name == ".git" || name == "__pycache__" || Path::new(name).extension().is_some_and(|e| e == "pyc")
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Sorted `(relative path, absolute path)` of the regular files under `root`.
fn list_files(root: &Path) -> io::Result<Vec<(String, std::path::PathBuf)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, std::path::PathBuf)>) -> io::Result<()> {
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
