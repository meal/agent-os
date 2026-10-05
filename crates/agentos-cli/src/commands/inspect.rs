use agentos_core::ids::TaskId;
use serde_json::json;

use super::print;
use crate::error::CliError;
use crate::home::Home;

pub fn status(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let shown = crate::app::queries::status(home, task, None)?;
    print(&serde_json::to_value(shown)?);
    Ok(())
}

pub fn events(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    for e in store.db.events(task)? {
        print(&json!({ "seq": e.seq, "type": e.event_type, "payload": e.payload, "ts": e.ts }));
    }
    Ok(())
}
