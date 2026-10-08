use crate::{error::CliError, home::Home};
use agentos_engine::gc::{HeldDriverLock, Options, collect};

/// `agentos gc [--dry-run]`: prints the JSON report, then exits 1 when anything was
/// refused or skipped (plain retention exits 0). Never creates a home.
pub fn gc(home: &Home, dry_run: bool, batch_size: usize) -> Result<(), CliError> {
    if !home.root.join("agentos.db").is_file() {
        return Err(CliError::usage(format!(
            "{} is not an agentos home (no agentos.db); gc never creates one",
            home.root.display()
        )));
    }
    // The driver lock first: opening the store (schema statements, the tasks directory)
    // must not race a running controller.
    let lock = home.lock()?;
    let held = HeldDriverLock::verify(&home.root, lock.file())?;
    let store = home.open()?;
    let report = collect(
        &held,
        &store.db,
        &store.blobs,
        Options {
            dry_run,
            batch_size,
        },
    )?;
    super::print(&serde_json::to_value(&report)?);
    if report.failed() {
        return Err(CliError::other(format!(
            "gc refused {} and skipped {} entries; see the report",
            report.summary.refused, report.summary.skipped
        )));
    }
    Ok(())
}
