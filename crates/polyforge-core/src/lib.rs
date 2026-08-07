//! polyforge-core: PolyForge core library.
//!
//! This crate hosts the append-only evidence ledger with SHA-256 Merkle
//! chaining and the deterministic `evaluate_complete` stage gate.

pub mod evidence;
pub mod gate;
pub mod ledger;

pub use evidence::{
    promote, EvidenceEntry as TriStateEvidence, EvidenceError, EvidenceKind, EvidenceState,
};
pub use gate::{evaluate_complete, Counts, Evaluation, GateError};
pub use ledger::{ChainState, EntryId, EvidenceEntry, Ledger, LedgerError};
