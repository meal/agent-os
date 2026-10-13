use std::path::Path;

use agentos_core::broker::Resource;
use agentos_core::contract::Capability;
use agentos_core::ids::TaskId;
use agentos_engine::export::{Manifest, export_bundle};
use agentos_store::db::DbError;

use super::print;
use crate::error::CliError;
use crate::home::{Home, Store};

pub fn export(home: &Home, task: &TaskId, dir: &Path) -> Result<(), CliError> {
    let store = home.open()?;
    let manifest = export_task(&store, task, dir)?;
    print(&serde_json::to_value(&manifest)?);
    Ok(())
}

/// [`export`] without the output: the manifest of the bundle written to `dir`.
pub(crate) fn export_task(store: &Store, task: &TaskId, dir: &Path) -> Result<Manifest, CliError> {
    // The broker decides, and the decision is journaled, before anything is read or written.
    // A task that is not finished cannot be exported whatever it holds: the export itself
    // says so (it also covers a task that was never approved, which has no handles).
    if store.db.task(task)?.state.is_terminal() {
        match store
            .db
            .authorize(task, Capability::ArtifactExport, &Resource::Task)
        {
            Ok(_) => {}
            Err(DbError::CapabilityDenied { reason, .. }) => {
                return Err(CliError::other(format!(
                    "export denied: capability artifact.export is not usable ({reason})"
                )));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(export_bundle(&store.db, &store.blobs, task, dir)?)
}
