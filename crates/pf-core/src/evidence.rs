//! Tri-state evidence lifecycle.
//!
//! Evidence moves through exactly three states, and the transitions are
//! strictly gated:
//!
//! ```text
//!   ModelClaimed --(ToolAttestation)--> Verified --(Validation)--> Validated
//! ```
//!
//! * A [`EvidenceKind::ModelClaim`] is always created in
//!   [`EvidenceState::ModelClaimed`]. The model's only entry point is
//!   [`EvidenceEntry::new_claim`]; it can never self-issue a
//!   [`EvidenceKind::ToolAttestation`] or [`EvidenceKind::Validation`].
//! * A [`EvidenceKind::ToolAttestation`] (state [`EvidenceState::Verified`])
//!   is supplied by the toolrunner, never by the model, and requires the
//!   `tool_version`, `env_fingerprint`, `command`, `exit_code` and
//!   `stdout_hash` fields.
//! * A [`EvidenceKind::Validation`] (state [`EvidenceState::Validated`])
//!   requires `validator` + `rationale` and is the only way to reach
//!   `Validated`.
//! * Every entry is pinned to a task via `task_id` + `commit_sha` + `diff_hash`
//!   so that claims/stats reference the exact commit they were produced against
//!   (edge identity -> commit pinning).
//!
//! [`promote`] is the single deterministic transition function. It is pure:
//! no async, no randomness, no wall-clock in the decision path.

use serde::{Deserialize, Serialize};

use crate::ledger::EvidenceEntry as LedgerEntry;

/// The kind of evidence record, orthogonal to its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceKind {
    /// A model's claim about a fact. Always starts in `ModelClaimed`.
    ModelClaim,
    /// A tool's attestation of a measured fact. Always `Verified.
    ToolAttestation,
    /// A validator's judgement over evidence. Always `Validated.
    Validation,
}

/// The lifecycle state of an evidence record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceState {
    /// The model claimed it (unverified).
    ModelClaimed,
    /// A tool executor/compiler/resource verified it.
    Verified,
    /// A human/judgement validated it.
    Validated,
}

/// A single tri-state evidence record, pinned to a task and commit.
///
/// The `state` field is set exclusively by the constructors and by [`promote`];
/// the model's only constructor is [`EvidenceEntry::new_claim`], which always
/// yields [`EvidenceState::ModelClaimed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    /// Task this evidence belongs to.
    pub task_id: String,
    /// Exact commit the evidence was produced against.
    pub commit_sha: String,
    /// Diff hash of the working tree at production time.
    pub diff_hash: String,
    /// Kind of the record.
    pub kind: EvidenceKind,
    /// Lifecycle state.
    pub state: EvidenceState,
    /// Tool version (required for `ToolAttestation`).
    pub tool_version: String,
    /// Environment fingerprint (required for `ToolAttestation`).
    pub env_fingerprint: String,
    /// Command run (required for `ToolAttestation`).
    pub command: String,
    /// Tool exit code (required for `ToolAttestation`).
    pub exit_code: i32,
    /// SHA-256 of the tool's stdout (required for `ToolAttestation`).
    pub stdout_hash: String,
    /// Validator identity (required for `Validation`).
    pub validator: String,
    /// Human rationale (required for `Validation`).
    pub rationale: String,
    /// Timestamp datum (supplied by the caller, never injected).
    pub ts: String,
}

impl EvidenceEntry {
    /// The model's ONLY entry point: a claim, always in `ModelClaimed`.
    pub fn new_claim(
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            commit_sha: commit_sha.into(),
            diff_hash: diff_hash.into(),
            kind: EvidenceKind::ModelClaim,
            state: EvidenceState::ModelClaimed,
            tool_version: String::new(),
            env_fingerprint: String::new(),
            command: String::new(),
            exit_code: 0,
            stdout_hash: String::new(),
            validator: String::new(),
            rationale: String::new(),
            ts: ts.into(),
        }
    }

    /// Toolrunner-supplied attestation: always `Verified`. Requires the
    /// tool fields; the model cannot call this.
    pub fn tool_attestation(
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        tool_version: impl Into<String>,
        env_fingerprint: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        stdout_hash: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            commit_sha: commit_sha.into(),
            diff_hash: diff_hash.into(),
            kind: EvidenceKind::ToolAttestation,
            state: EvidenceState::Verified,
            tool_version: tool_version.into(),
            env_fingerprint: env_fingerprint.into(),
            command: command.into(),
            exit_code,
            stdout_hash: stdout_hash.into(),
            validator: String::new(),
            rationale: String::new(),
            ts: ts.into(),
        }
    }

    /// Validator-supplied judgement: always `Validated`. Requires
    /// `validator` + `rationale`.
    pub fn validation(
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        validator: impl Into<String>,
        rationale: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            commit_sha: commit_sha.into(),
            diff_hash: diff_hash.into(),
            kind: EvidenceKind::Validation,
            state: EvidenceState::Validated,
            tool_version: String::new(),
            env_fingerprint: String::new(),
            command: String::new(),
            exit_code: 0,
            stdout_hash: String::new(),
            validator: validator.into(),
            rationale: rationale.into(),
            ts: ts.into(),
        }
    }

    /// Convert into a ledger entry (kind string + canonical payload). The
    /// ledger's hash covers the payload, not the model's verdict.
    pub fn to_ledger_entry(&self) -> LedgerEntry {
        let kind = match self.kind {
            EvidenceKind::ModelClaim => "ModelClaim",
            EvidenceKind::ToolAttestation => "ToolAttestation",
            EvidenceKind::Validation => "Validation",
        };
        let payload = serde_json::json!({
            "state": match self.state {
                EvidenceState::ModelClaimed => "ModelClaimed",
                EvidenceState::Verified => "Verified",
                EvidenceState::Validated => "Validated",
            },
            "task_id": self.task_id,
            "commit_sha": self.commit_sha,
            "diff_hash": self.diff_hash,
            "command": self.command,
            "exit_code": self.exit_code,
            "stdout_hash": self.stdout_hash,
            "validator": self.validator,
            "rationale": self.rationale,
        });
        LedgerEntry::new(
            kind,
            payload,
            self.tool_version.clone(),
            self.env_fingerprint.clone(),
            self.ts.clone(),
        )
    }
}

/// Errors produced by the tri-state lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    /// The requested transition is not allowed by the state machine.
    InvalidPromotion {
        from: EvidenceState,
        via: EvidenceKind,
    },
}

/// The single deterministic transition function.
///
/// Allowed transitions only:
/// * `ModelClaimed` -> `Verified` via a [`EvidenceKind::ToolAttestation`];
/// * `Verified` -> `Validated` via a [`EvidenceKind::Validation`].
///
/// `ModelClaimed` -> `Validated` directly is rejected.
pub fn promote(entry: &EvidenceEntry, attestation: &EvidenceEntry) -> Result<EvidenceEntry, EvidenceError> {
    match (entry.state, attestation.kind) {
        (EvidenceState::ModelClaimed, EvidenceKind::ToolAttestation) => Ok(
            EvidenceEntry::tool_attestation(
                entry.task_id.clone(),
                entry.commit_sha.clone(),
                entry.diff_hash.clone(),
                attestation.tool_version.clone(),
                attestation.env_fingerprint.clone(),
                attestation.command.clone(),
                attestation.exit_code,
                attestation.stdout_hash.clone(),
                attestation.ts.clone(),
            ),
        ),
        (EvidenceState::Verified, EvidenceKind::Validation) => Ok(
            EvidenceEntry::validation(
                entry.task_id.clone(),
                entry.commit_sha.clone(),
                entry.diff_hash.clone(),
                attestation.validator.clone(),
                attestation.rationale.clone(),
                attestation.ts.clone(),
            ),
        ),
        (from, via) => Err(EvidenceError::InvalidPromotion { from, via }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    fn claim() -> EvidenceEntry {
        EvidenceEntry::new_claim("T3", "abc123", "diff-1", "ts-1")
    }

    fn attestation() -> EvidenceEntry {
        EvidenceEntry::tool_attestation(
            "T3", "abc123", "diff-1", "cargo-1.95.0", "env-x", "cargo test", 0, "h1", "ts-2",
        )
    }

    fn validation() -> EvidenceEntry {
        EvidenceEntry::validation("T3", "abc123", "diff-1", "oracle", "all green", "ts-3")
    }

    #[test]
    fn test_model_claim_starts_model_claimed() {
        let c = claim();
        assert_eq!(c.kind, EvidenceKind::ModelClaim);
        assert_eq!(c.state, EvidenceState::ModelClaimed);
    }

    #[test]
    fn test_claim_cannot_self_verify() {
        // A claim cannot promote itself to Verified (no tool attestation).
        let c = claim();
        let err = promote(&c, &c).unwrap_err();
        assert!(matches!(err, EvidenceError::InvalidPromotion { .. }));
        // A claim cannot be promoted via a Validation either.
        let err = promote(&c, &validation()).unwrap_err();
        assert!(matches!(err, EvidenceError::InvalidPromotion { .. }));
    }

    #[test]
    fn test_tool_attestation_promotes_to_verified() {
        let c = claim();
        let v = promote(&c, &attestation()).unwrap();
        assert_eq!(v.kind, EvidenceKind::ToolAttestation);
        assert_eq!(v.state, EvidenceState::Verified);
        assert_eq!(v.tool_version, "cargo-1.95.0");
        assert_eq!(v.env_fingerprint, "env-x");
        assert_eq!(v.command, "cargo test");
        assert_eq!(v.exit_code, 0);
        assert_eq!(v.stdout_hash, "h1");
    }

    #[test]
    fn test_validation_promotes_to_validated() {
        let c = claim();
        let v = promote(&c, &attestation()).unwrap();
        let d = promote(&v, &validation()).unwrap();
        assert_eq!(d.kind, EvidenceKind::Validation);
        assert_eq!(d.state, EvidenceState::Validated);
        assert_eq!(d.validator, "oracle");
        assert_eq!(d.rationale, "all green");
    }

    #[test]
    fn test_no_direct_claim_to_validated() {
        let c = claim();
        let err = promote(&c, &validation()).unwrap_err();
        assert!(matches!(err, EvidenceError::InvalidPromotion { .. }));
    }

    #[test]
    fn test_evidence_pinned_to_task_and_commit() {
        let c = claim();
        assert_eq!(c.task_id, "T3");
        assert_eq!(c.commit_sha, "abc123");
        assert_eq!(c.diff_hash, "diff-1");
        // Promotion preserves the pinning.
        let v = promote(&c, &attestation()).unwrap();
        assert_eq!(v.task_id, "T3");
        assert_eq!(v.commit_sha, "abc123");
        assert_eq!(v.diff_hash, "diff-1");
        let d = promote(&v, &validation()).unwrap();
        assert_eq!(d.task_id, "T3");
        assert_eq!(d.commit_sha, "abc123");
        assert_eq!(d.diff_hash, "diff-1");
    }

    #[test]
    fn test_ledger_integration_chain_ok() {
        let dir = std::env::temp_dir().join("pf-todo3-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ledger-{}.jsonl", std::process::id()));
        let mut ledger = Ledger::new(&path);
        let c = claim();
        let v = promote(&c, &attestation()).unwrap();
        let d = promote(&v, &validation()).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();
        let state = ledger.verify_chain().unwrap();
        assert_eq!(state.entry_count, 3);
        assert_eq!(state.head_hash.len(), 64);
    }
}