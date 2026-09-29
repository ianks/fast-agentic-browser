//! fab: a fast agentic browser tool. An LLM sends high-level
//! instructions; Jev (a System One decision model) turns them into typed
//! browser actions over a compact DOM representation.

pub mod agent;
pub mod backend;
pub mod config;
pub mod dataset;
pub mod decide;
pub mod direct;
pub mod domain;
pub mod dvm;
pub mod jev;
pub mod license;
pub mod llm;
pub mod paths;
pub mod script;
pub mod secrets;
pub mod shape;
pub mod snapshot;
pub mod spans;

pub use agent::{ActResult, ExtractResult, Gate, Pages, Session, Timings};
pub use config::{ExecMode, Knobs};

pub mod program_machine;

pub mod secret_machine;
