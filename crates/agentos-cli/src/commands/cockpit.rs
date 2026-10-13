use std::io::IsTerminal;

use agentos_store::view::{View, ViewError};

use crate::error::CliError;
use crate::home::Home;

/// The read-only cockpit. It never calls `Home::open`, which creates directories and the
/// database; a missing home is refused instead. The database is opened before the terminal
/// check, so a refusal is reported the same with or without a terminal.
pub fn cockpit(home: &Home) -> Result<(), CliError> {
    let view = View::open(&home.root.join("agentos.db")).map_err(|e| match e {
        ViewError::Missing(_) | ViewError::Open { .. } | ViewError::Schema { .. } => {
            CliError::usage(e.to_string())
        }
        e => CliError::other(e.to_string()),
    })?;
    if !std::io::stdout().is_terminal() {
        return Err(CliError::usage(
            "agentos cockpit needs a terminal on stdout",
        ));
    }
    agentos_cockpit::run(view).map_err(|e| CliError::other(e.to_string()))
}
