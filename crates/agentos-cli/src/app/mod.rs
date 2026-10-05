//! Shared task operations; adapters own presentation, never task state.
pub(crate) mod error;
#[allow(dead_code)] // The next UI increment consumes these internal read entry points.
pub(crate) mod queries;
pub(crate) mod types;
pub(crate) use error::{AppError, AppErrorKind, AppResult};
