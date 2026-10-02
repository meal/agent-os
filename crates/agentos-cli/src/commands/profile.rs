//! `profile register` and `profile list`: the content-addressed verification profile
//! registry (`<home>/registry/<id>@<digest>/`, read-only).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_engine::job::atomic_write;
use agentos_engine::workspace::{copy_tree, workspace_digest};
use serde::Deserialize;
use serde_json::json;

use super::print;
use crate::error::CliError;
use crate::home::Home;

#[derive(Deserialize)]
struct ProfileFile {
    id: String,
    command: Vec<String>,
}

/// One plain name: no separators or traversal, no `@` (it separates id and digest).
fn check_id(id: &str) -> Result<(), CliError> {
    let mut parts = Path::new(id).components();
    let plain = matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None))
        && !id.starts_with('-')
        && !id.contains(['/', '\\', '\0', '@']);
    if plain {
        Ok(())
    } else {
        Err(CliError::usage(format!("profile id {id:?} must be one plain name without '@'")))
    }
}

/// Makes `dir` and everything under it read-only (files 0444, directories 0555).
fn make_read_only(dir: &Path) -> Result<(), CliError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            make_read_only(&path)?;
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555))?;
    Ok(())
}

pub fn register(home: &Home, dir: &Path) -> Result<(), CliError> {
    let source = dir.canonicalize().ok().filter(|p| p.is_dir()).ok_or_else(|| CliError::usage(format!("{} is not a directory", dir.display())))?;
    let raw = fs::read(source.join("profile.json")).map_err(|e| CliError::usage(format!("cannot read {}/profile.json: {e}", source.display())))?;
    let profile: ProfileFile = serde_json::from_slice(&raw).map_err(|e| CliError::usage(format!("invalid profile.json: {e}")))?;
    check_id(&profile.id)?;
    if profile.command.is_empty() {
        return Err(CliError::usage("profile.json: command must not be empty"));
    }

    let registry = home.registry_dir();
    fs::create_dir_all(&registry)?;
    // Copied and digested in a scratch directory first: the entry's name is its digest.
    let staging = tempfile::Builder::new().prefix(".register-").tempdir_in(&registry)?;
    let staged = staging.path().join("profile");
    copy_tree(&source, &staged)?;
    let digest = workspace_digest(&staged)?;
    let name = format!("{}@{digest}", profile.id);
    let entry = registry.join(&name);
    if !entry.exists() {
        make_read_only(&staged)?;
        fs::rename(&staged, &entry)?;
        let registered_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let meta = json!({ "id": profile.id, "digest": digest, "registered_ms": registered_ms });
        atomic_write(&registry.join(format!("{name}.meta.json")), meta.to_string().as_bytes())?;
    }
    print(&json!({ "id": profile.id, "digest": digest }));
    Ok(())
}

pub fn list(home: &Home) -> Result<(), CliError> {
    let mut entries = home.registry_list();
    entries.sort_by(|a, b| (&a.id, a.registered_ms, &a.digest).cmp(&(&b.id, b.registered_ms, &b.digest)));
    let listed: Vec<_> = entries.iter().map(|e| json!({ "id": e.id, "digest": e.digest, "registered_ms": e.registered_ms })).collect();
    print(&json!(listed));
    Ok(())
}
