use std::path::Path;

use agentos_core::ids::TaskId;
use agentos_engine::export::export_bundle;

use super::print;
use crate::error::CliError;
use crate::home::Home;

pub fn export(home: &Home, task: &TaskId, dir: &Path) -> Result<(), CliError> {
    let store = home.open()?;
    let manifest = export_bundle(&store.db, &store.blobs, task, dir)?;
    print(&serde_json::to_value(&manifest)?);
    Ok(())
}
