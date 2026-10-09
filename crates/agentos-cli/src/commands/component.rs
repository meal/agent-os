//! `component register` and `component list`: the analyzer component registry
//! (`<home>/registry/components/<id>@<digest>/`, read-only, content-addressed).

use std::fs;
use std::path::Path;

use agentos_component::Runtime;
use serde::Deserialize;
use serde_json::json;

use super::print;
use super::registry::{check_id, register_tree};
use crate::error::CliError;
use crate::home::Home;

/// The world every analyzer implements.
pub const ANALYZER_WORLD: &str = "agentos:analyzer/analyzer@1.0.0";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentFile {
    id: String,
    world: String,
}

pub fn register(home: &Home, dir: &Path) -> Result<(), CliError> {
    let source = dir
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| CliError::usage(format!("{} is not a directory", dir.display())))?;
    let raw = fs::read(source.join("component.json")).map_err(|e| {
        CliError::usage(format!(
            "cannot read {}/component.json: {e}",
            source.display()
        ))
    })?;
    let meta: ComponentFile = serde_json::from_slice(&raw)
        .map_err(|e| CliError::usage(format!("invalid component.json: {e}")))?;
    check_id("analyzer", &meta.id)?;
    if meta.world != ANALYZER_WORLD {
        return Err(CliError::usage(format!(
            "component.json: world {:?} is not {ANALYZER_WORLD}",
            meta.world
        )));
    }
    let bytes = fs::read(source.join("component.wasm")).map_err(|e| {
        CliError::usage(format!(
            "cannot read {}/component.wasm: {e}",
            source.display()
        ))
    })?;
    let runtime = Runtime::new().map_err(CliError::other)?;
    runtime
        .check(&bytes)
        .map_err(|e| CliError::usage(format!("not an analyzer: {e}")))?;
    let registered = register_tree(&home.components_dir(), &meta.id, &source)?;
    print(&json!({ "id": registered.id, "digest": registered.digest }));
    Ok(())
}

pub fn list(home: &Home) -> Result<(), CliError> {
    let mut entries = home.component_list();
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
