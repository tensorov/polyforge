//! pf-core: PolyForge core library.
//!
//! This crate hosts the append-only evidence ledger with SHA-256 Merkle
//! chaining. The deterministic `evaluate_complete` process lands in a later
//! todo.

pub mod ledger;

pub use ledger::{ChainState, EntryId, EvidenceEntry, Ledger, LedgerError};