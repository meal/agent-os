use super::print;
use crate::{error::CliError, home::Home};
use agentos_core::ids::TaskId;
use std::path::Path;
pub fn export(home: &Home, task: &TaskId, dir: &Path) -> Result<(), CliError> {
    let manifest = crate::app::export::write(home, task, dir, None)?;
    print(&serde_json::to_value(&manifest)?);
    Ok(())
}
