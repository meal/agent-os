use std::fmt;

use agentos_engine::export::ExportError;
use agentos_engine::runner::EngineError;
use agentos_store::db::DbError;

/// A failed command: a message for stderr and the process exit code (2 for an invalid
/// contract or invocation, 1 for anything else).
#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub message: String,
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> CliError {
        CliError {
            code: 2,
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> CliError {
        CliError {
            code: 1,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<DbError> for CliError {
    fn from(e: DbError) -> CliError {
        match e {
            DbError::NotFound(id) => CliError::other(format!("unknown task {id}")),
            e => CliError::other(e.to_string()),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> CliError {
        CliError::other(e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> CliError {
        CliError::other(e.to_string())
    }
}

impl From<EngineError> for CliError {
    fn from(e: EngineError) -> CliError {
        match e {
            EngineError::Db(e) => e.into(),
            e => CliError::other(e.to_string()),
        }
    }
}

impl From<ExportError> for CliError {
    fn from(e: ExportError) -> CliError {
        match e {
            ExportError::Db(e) => e.into(),
            e => CliError::other(e.to_string()),
        }
    }
}
