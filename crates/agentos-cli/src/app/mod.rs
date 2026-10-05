//! Shared task operations; adapters own presentation, never task state.
pub(crate) mod control;
pub(crate) mod error;
pub(crate) mod export;
pub(crate) mod queries;
#[allow(dead_code)] // Next UI form increment consumes this service.
pub(crate) mod submission;
pub(crate) mod types;
pub(crate) use error::{AppError, AppErrorKind, AppResult};
