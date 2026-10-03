//! `image register` and `image list`: the guest image registry
//! (`<home>/registry/images/<id>@<digest>/`, read-only, content-addressed).

use std::path::Path;

use agentos_engine::firecracker::read_image;
use serde_json::json;

use super::print;
use super::registry::{check_id, register_tree};
use crate::error::CliError;
use crate::home::Home;

pub fn register(home: &Home, dir: &Path) -> Result<(), CliError> {
    let source = dir.canonicalize().ok().filter(|p| p.is_dir()).ok_or_else(|| CliError::usage(format!("{} is not a directory", dir.display())))?;
    let image = read_image(&source).map_err(|e| CliError::usage(format!("invalid guest image: {e}")))?;
    check_id("guest image", &image.id)?;
    let registered = register_tree(&home.images_dir(), &image.id, &source)?;
    print(&json!({ "id": registered.id, "digest": registered.digest }));
    Ok(())
}

pub fn list(home: &Home) -> Result<(), CliError> {
    let mut entries = home.image_list();
    entries.sort_by(|a, b| (&a.id, a.registered_ms, &a.digest).cmp(&(&b.id, b.registered_ms, &b.digest)));
    let listed: Vec<_> = entries.iter().map(|e| json!({ "id": e.id, "digest": e.digest, "registered_ms": e.registered_ms })).collect();
    print(&json!(listed));
    Ok(())
}
