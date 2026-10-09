//! The guest agent: request handlers, the session state machine, the VM's PID 1 (`init`),
//! the check trampoline (`exec-check`), and the fake mode that runs the same session code
//! over two directories instead of a VM.

pub mod agent;
pub mod backend;
pub mod fake;
pub mod handlers;
pub mod init;
pub mod loopback;
pub mod proxy;
pub mod session_agent;
pub mod trampoline;

pub use backend::{Backend, FakeBackend, VmBackend};

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
