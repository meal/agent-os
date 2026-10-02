//! The guest agent: request handlers, the session state machine and the fake mode that runs
//! the same code over two directories instead of a VM.

pub mod agent;
pub mod backend;
pub mod fake;
pub mod handlers;

pub use backend::{Backend, FakeBackend};

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
