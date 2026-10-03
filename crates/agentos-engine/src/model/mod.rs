//! The model provider seam: a trait, the real Anthropic Messages client, a scripted fake and a
//! recorder that turns a real session into a replayable transcript.

pub mod anthropic;
pub mod executor;
pub mod fake;
pub mod provider;

pub use executor::ModelExecutor;
