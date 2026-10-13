use std::io::IsTerminal;

use agentos_cockpit::RunError;
use agentos_store::view::ViewError;

use crate::error::CliError;
use crate::home::Home;

/// The read-only cockpit. It never calls `Home::open`, which creates directories and the
/// database; a missing home is refused instead.
pub fn cockpit(home: &Home) -> Result<(), CliError> {
    if !std::io::stdout().is_terminal() {
        return Err(CliError::usage(
            "agentos cockpit needs a terminal on stdout",
        ));
    }
    match agentos_cockpit::run(&home.root.join("agentos.db")) {
        Ok(()) => Ok(()),
        Err(e @ RunError::View(ViewError::Missing(_) | ViewError::Schema { .. })) => {
            Err(CliError::usage(e.to_string()))
        }
        Err(e) => Err(CliError::other(e.to_string())),
    }
}
