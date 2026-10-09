//! The content-addressed, read-only registry layout shared by `profile` and `image`:
//! `<registry>/<id>@<digest>/` (files 0444, directories 0555) plus the sibling
//! `<id>@<digest>.meta.json` holding the registration time (outside the digest).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_core::ids::Digest;
use agentos_engine::job::atomic_write;
use agentos_engine::workspace::{copy_tree, workspace_digest};
use serde_json::json;

use crate::error::CliError;
use crate::home::RegistryEntry;

/// What `register_tree` reports: the id and the digest of the registered bytes.
pub struct Registered {
    pub id: String,
    pub digest: Digest,
}

/// One plain name: no separators or traversal, no `@` (it separates id and digest), no leading `-`.
/// `kind` names the thing in the message (`profile`, `guest image`).
pub fn check_id(kind: &str, id: &str) -> Result<(), CliError> {
    let mut parts = Path::new(id).components();
    let plain = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) && !id.starts_with('-')
        && !id.contains(['/', '\\', '\0', '@']);
    if plain {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "{kind} id {id:?} must be one plain name without '@'"
        )))
    }
}

/// Makes `dir` and everything under it read-only (files 0444, directories 0555).
/// Removes the write bits of everything under `dir` and of `dir` itself.
fn make_read_only(dir: &Path) -> Result<(), CliError> {
    make_contents_read_only(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555))?;
    Ok(())
}

/// Removes the write bits of everything under `dir`, but not of `dir`: a directory moved to
/// another parent needs its own write bit (its `..` changes), which only root does without.
fn make_contents_read_only(dir: &Path) -> Result<(), CliError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            make_read_only(&path)?;
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        }
    }
    Ok(())
}

/// Copies `source` into `registry` as `<id>@<digest>/`, read-only, and records the registration
/// time. The same bytes twice change nothing. The tree is copied and digested in a scratch
/// directory first: the entry's name is its digest.
pub fn register_tree(registry: &Path, id: &str, source: &Path) -> Result<Registered, CliError> {
    check_id("registry entry", id)?;
    fs::create_dir_all(registry)?;
    let staging = tempfile::Builder::new()
        .prefix(".register-")
        .tempdir_in(registry)?;
    let staged = staging.path().join("entry");
    copy_tree(source, &staged)?;
    let digest = workspace_digest(&staged)?;
    let name = format!("{id}@{digest}");
    let entry = registry.join(&name);
    if !entry.exists() {
        make_contents_read_only(&staged)?;
        fs::rename(&staged, &entry)?;
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o555))?;
        let registered_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let meta = json!({ "id": id, "digest": digest, "registered_ms": registered_ms });
        atomic_write(
            &registry.join(format!("{name}.meta.json")),
            meta.to_string().as_bytes(),
        )?;
    }
    Ok(Registered {
        id: id.to_string(),
        digest,
    })
}

/// Every complete entry of `registry` (`<id>@<64 hex>` directories for which `has_marker`
/// holds), unordered. Missing registry: none.
pub fn list_entries(registry: &Path, has_marker: impl Fn(&Path) -> bool) -> Vec<RegistryEntry> {
    let Ok(entries) = fs::read_dir(registry) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((id, digest)) = name.split_once('@') else {
            continue;
        };
        if digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            || !has_marker(&entry.path())
        {
            continue;
        }
        let meta = fs::read(registry.join(format!("{name}.meta.json")))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let registered_ms = meta.and_then(|m| m["registered_ms"].as_i64()).unwrap_or(0);
        out.push(RegistryEntry {
            id: id.to_string(),
            digest: digest.to_string(),
            dir: entry.path(),
            registered_ms,
        });
    }
    out
}
