//! polyforge-toolrunner: PolyForge tool execution.
//!
//! Allowlist + typed-args tool runner with no-shell spawning, per-command
//! environment fingerprinting, and a wall-clock timeout (default 3600s,
//! override via `PF_TOOL_TIMEOUT_SECS`) that kills the whole tool process
//! group when a run exceeds its budget. Evidence becomes `Verified` only
//! through an allowlisted tool run (see [`runner`] and [`verify`]).

pub mod runner;
pub mod verify;

pub use runner::{
    allowlist, env_fingerprint, init_executor, lookup, parse_timeout, run, run_with_timeout, spawn,
    ExecutorKind, RunOutput, RunnerError, Tool, DEFAULT_TOOL_TIMEOUT_SECS, PF_TOOL_TIMEOUT_SECS,
};
pub use verify::verify_and_append;
