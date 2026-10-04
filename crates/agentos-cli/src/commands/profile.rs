//! `profile register` and `profile list`: the content-addressed verification profile
//! registry (`<home>/registry/<id>@<digest>/`, read-only).

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use super::print;
use super::registry::{check_id, register_tree};
use crate::error::CliError;
use crate::home::Home;

#[derive(Deserialize)]
struct ProfileFile {
    id: String,
    command: Vec<String>,
}

pub fn register(home: &Home, dir: &Path) -> Result<(), CliError> {
    let source = dir
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| CliError::usage(format!("{} is not a directory", dir.display())))?;
    let raw = fs::read(source.join("profile.json")).map_err(|e| {
        CliError::usage(format!(
            "cannot read {}/profile.json: {e}",
            source.display()
        ))
    })?;
    let profile: ProfileFile = serde_json::from_slice(&raw)
        .map_err(|e| CliError::usage(format!("invalid profile.json: {e}")))?;
    check_id("profile", &profile.id)?;
    if profile.command.is_empty() {
        return Err(CliError::usage("profile.json: command must not be empty"));
    }

    let registered = register_tree(&home.registry_dir(), &profile.id, &source)?;
    print(&json!({ "id": registered.id, "digest": registered.digest }));
    Ok(())
}

pub fn list(home: &Home) -> Result<(), CliError> {
    let mut entries = home.registry_list();
    entries.sort_by(|a, b| {
        (&a.id, a.registered_ms, &a.digest).cmp(&(&b.id, b.registered_ms, &b.digest))
    });
    let listed: Vec<_> = entries
        .iter()
        .map(|e| json!({ "id": e.id, "digest": e.digest, "registered_ms": e.registered_ms }))
        .collect();
    print(&json!(listed));
    Ok(())
}
