//! pf-core: PolyForge core library.
//!
//! This crate hosts the append-only evidence ledger with SHA-256 Merkle
//! chaining. The deterministic `evaluate_complete` process lands in a later
//! todo.

pub mod evidence;
pub mod ledger;

pub use evidence::{EvidenceEntry as TriStateEvidence, EvidenceError, EvidenceKind, EvidenceState, promote};
pub use ledger::{ChainState, EntryId, EvidenceEntry, Ledger, LedgerError};