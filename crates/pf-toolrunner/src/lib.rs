//! pf-toolrunner: PolyForge tool execution.
//!
//! Allowlist + typed-args tool runner with no-shell spawning and per-command
//! environment fingerprinting. Evidence becomes `Verified` only through an
//! allowlisted tool run (see [`runner`] and [`verify`]).

pub mod runner;
pub mod verify;

pub use runner::{RunOutput, RunnerError, Tool, allowlist, env_fingerprint, lookup, run, spawn};
pub use verify::verify_and_append;