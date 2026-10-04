use crate::{error::CliError, home::Home};
use agentos_engine::gc::collect;

pub fn gc(home: &Home, dry_run: bool) -> Result<(), CliError> {
    let store = home.open()?;
    let _lock = home.lock()?;
    let report = collect(&home.root, &store.db, &store.blobs, dry_run)?;
    super::print(&serde_json::to_value(report)?);
    Ok(())
}
