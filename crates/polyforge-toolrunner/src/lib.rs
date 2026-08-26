//! polyforge-toolrunner: PolyForge tool execution.
//!
//! Allowlist + typed-args tool runner with no-shell spawning, per-command
//! environment fingerprinting, and a wall-clock timeout (default 3600s,
//! override via `PF_TOOL_TIMEOUT_SECS`) that kills the whole tool process
//! group when a run exceeds its budget. Evidence becomes `Verified` only
//! through an allowlisted tool run (see [`runner`] and [`verify`]).

#[cfg(feature = "sandbox-container")]
pub mod container_exec;
#[cfg(feature = "sandbox-gvisor")]
pub mod gvisor_exec;
pub mod prober;
pub mod runner;
#[cfg(feature = "sandbox-mock")]
pub mod sandbox_mock;
pub mod verify;

#[cfg(feature = "sandbox-container")]
pub use container_exec::{
    ContainerExecutor, DEFAULT_SANDBOX_IMAGE, POLYFORGE_SANDBOX_IMAGE_ENV,
    POLYFORGE_SANDBOX_RUNTIME_ENV,
};
#[cfg(feature = "sandbox-gvisor")]
pub use gvisor_exec::{compose_digest, GvisorConfig, GvisorExecutor, GvisorRoute};
pub use runner::{
    allowlist, env_fingerprint, init_executor, lookup, parse_timeout, run, run_with_timeout, spawn,
    ExecutorKind, RunOutput, RunnerError, Tool, DEFAULT_TOOL_TIMEOUT_SECS, PF_TOOL_TIMEOUT_SECS,
};
#[cfg(feature = "sandbox-mock")]
pub use sandbox_mock::{executor_digest, MockSandboxExecutor, MOCK_IMAGE_ID};
pub use verify::verify_and_append;
