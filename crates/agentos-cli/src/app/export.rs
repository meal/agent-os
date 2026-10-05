use super::{AppError, AppErrorKind, AppResult, queries};
use crate::home::Home;
use agentos_core::{broker::Resource, contract::Capability, ids::TaskId};
use agentos_engine::export::{Manifest, export_bundle_bounded};
use agentos_store::db::DbError;
use std::path::Path;
pub(crate) fn write(
    home: &Home,
    task: &TaskId,
    out: &Path,
    max_bytes: Option<u64>,
) -> AppResult<Manifest> {
    let store = home.open()?;
    if max_bytes.is_some() {
        store.db.contract_bounded(task, queries::CONTRACT_LIMIT)?;
    }
    if store.db.task(task)?.state.is_terminal() {
        match store
            .db
            .authorize(task, Capability::ArtifactExport, &Resource::Task)
        {
            Ok(_) => {}
            Err(DbError::CapabilityDenied { reason, .. }) => {
                return Err(AppError::new(
                    AppErrorKind::Forbidden,
                    format!("export denied: capability artifact.export is not usable ({reason})"),
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(export_bundle_bounded(
        &store.db,
        &store.blobs,
        task,
        out,
        max_bytes,
    )?)
}
