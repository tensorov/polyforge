//! PolyForge MCP server library.
//!
//! Exposes the PolyForge ledger + stage-gate surface as an MCP server with
//! four tools:
//!
//! - `evidence_append` — a model claims evidence (`kind=ModelClaim` only;
//!   `ToolAttestation`/`Validation` are rejected because a model cannot
//!   self-verify).
//! - `evidence_verify` — runs an allowlisted tool to verify a claim and
//!   appends the resulting `ToolAttestation` (`Verified`).
//! - `gate_evaluate` — evaluates a stage gate for a task via the
//!   deterministic `evaluate_complete` gate.
//! - `gate_report` — read-only bundle snapshot for a task (chain tail hash,
//!   gate pass status, bundle SHA-256).

pub mod server;

pub use server::PolyForgeServer;
