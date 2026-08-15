//! Evidence lifecycle.
//!
//! Evidence moves through four states, and the transitions are strictly
//! gated:
//!
//! ```text
//!   ModelClaimed --(ToolAttestation)--> Verified --(Validation)--> Validated
//!   ModelClaimed --(EvalAttestation)--> Verified
//!   ModelClaimed --(Discrepancy)-----> Refuted
//! ```
//!
//! * A [`EvidenceKind::ModelClaim`] is always created in
//!   [`EvidenceState::ModelClaimed`]. The model's only entry point is
//!   [`EvidenceEntry::new_claim`]; it can never self-issue any other kind.
//! * A [`EvidenceKind::ToolAttestation`] (state [`EvidenceState::Verified`])
//!   is supplied by the toolrunner, never by the model, and requires the
//!   `tool_version`, `env_fingerprint`, `command`, `exit_code` and
//!   `stdout_hash` fields.
//! * A [`EvidenceKind::EvalAttestation`] (state [`EvidenceState::Verified`])
//!   is an operator/eval-harness attestation carrying optional identity
//!   fields (`experiment_id`, `model_fingerprint`, `run_id`, `budget`,
//!   `eval_metadata`).
//! * A [`EvidenceKind::Discrepancy`] (state [`EvidenceState::Refuted`])
//!   records a failed verification trace: `rationale` carries the truncated
//!   stderr and `validator` the tool identity.
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
    /// An operator/eval-harness attestation of an eval run. Always `Verified`.
    EvalAttestation,
    /// A failed-verification trace. Always `Refuted`.
    Discrepancy,
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
    /// A verification trace refuted the claim.
    Refuted,
}

/// Actual git state captured at attestation time. Lives in the attestation
/// payload only — the ledger entry key stays the claim's commit/diff (see
/// [`EvidenceEntry::to_ledger_entry`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    /// `git rev-parse HEAD` at attestation time; `"none"` when not a repo.
    pub actual_commit_sha: String,
    /// `git rev-parse HEAD^{tree}` at attestation time.
    pub actual_tree_hash: String,
    /// SHA-256 of `git diff HEAD` output at attestation time.
    pub actual_diff_hash: String,
    /// `git status --porcelain` was non-empty (uncommitted changes).
    pub git_dirty: bool,
    /// A git repo was found (not-a-repo is distinct from dirty).
    pub git_repo_present: bool,
    /// The claim's commit/diff differs from the actual git state.
    pub claim_git_mismatch: bool,
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
    /// Optional eval experiment id (record-only, never enforced).
    pub experiment_id: Option<String>,
    /// Optional model fingerprint (record-only, never enforced).
    pub model_fingerprint: Option<String>,
    /// Optional eval run id (record-only, never enforced).
    pub run_id: Option<String>,
    /// Optional budget datum (record-only, never enforced).
    pub budget: Option<String>,
    /// Optional eval metadata blob (record-only, never enforced).
    pub eval_metadata: Option<serde_json::Value>,
    /// Actual git state captured at attestation time (attestation payload
    /// only; the ledger entry key stays the claim's commit/diff).
    pub git_state: Option<GitState>,
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
            experiment_id: None,
            model_fingerprint: None,
            run_id: None,
            budget: None,
            eval_metadata: None,
            git_state: None,
            ts: ts.into(),
        }
    }

    /// Toolrunner-supplied attestation: always `Verified`. Requires the
    /// tool fields; the model cannot call this.
    // Pure data constructor mirroring the 9-field EvidenceEntry record; callers stay unchanged.
    #[allow(clippy::too_many_arguments)]
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
            experiment_id: None,
            model_fingerprint: None,
            run_id: None,
            budget: None,
            eval_metadata: None,
            git_state: None,
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
            experiment_id: None,
            model_fingerprint: None,
            run_id: None,
            budget: None,
            eval_metadata: None,
            git_state: None,
            ts: ts.into(),
        }
    }

    /// Operator/eval-harness attestation: always `Verified`. Mirrors
    /// [`EvidenceEntry::tool_attestation`]'s field order plus the optional
    /// identity tail (`experiment_id`, `model_fingerprint`, `run_id`,
    /// `budget`, `eval_metadata`).
    #[allow(clippy::too_many_arguments)]
    pub fn eval_attestation(
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        tool_version: impl Into<String>,
        env_fingerprint: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        stdout_hash: impl Into<String>,
        experiment_id: Option<String>,
        model_fingerprint: Option<String>,
        run_id: Option<String>,
        budget: Option<String>,
        eval_metadata: Option<serde_json::Value>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            commit_sha: commit_sha.into(),
            diff_hash: diff_hash.into(),
            kind: EvidenceKind::EvalAttestation,
            state: EvidenceState::Verified,
            tool_version: tool_version.into(),
            env_fingerprint: env_fingerprint.into(),
            command: command.into(),
            exit_code,
            stdout_hash: stdout_hash.into(),
            validator: String::new(),
            rationale: String::new(),
            experiment_id,
            model_fingerprint,
            run_id,
            budget,
            eval_metadata,
            git_state: None,
            ts: ts.into(),
        }
    }

    /// Failed-verification trace: always `Refuted`. `rationale` carries the
    /// truncated stderr and `validator` the tool identity.
    #[allow(clippy::too_many_arguments)]
    pub fn discrepancy(
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        rationale: impl Into<String>,
        validator: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            commit_sha: commit_sha.into(),
            diff_hash: diff_hash.into(),
            kind: EvidenceKind::Discrepancy,
            state: EvidenceState::Refuted,
            tool_version: String::new(),
            env_fingerprint: String::new(),
            command: command.into(),
            exit_code,
            stdout_hash: String::new(),
            validator: validator.into(),
            rationale: rationale.into(),
            experiment_id: None,
            model_fingerprint: None,
            run_id: None,
            budget: None,
            eval_metadata: None,
            git_state: None,
            ts: ts.into(),
        }
    }

    /// Convert into a ledger entry (kind string + canonical payload). The
    /// ledger's hash covers the payload, not the model's verdict.
    pub fn to_ledger_entry(&self) -> LedgerEntry {
        let kind = match self.kind {
            EvidenceKind::ModelClaim => "ModelClaim",
            EvidenceKind::ToolAttestation => "ToolAttestation",
            EvidenceKind::EvalAttestation => "EvalAttestation",
            EvidenceKind::Discrepancy => "Discrepancy",
            EvidenceKind::Validation => "Validation",
        };
        let mut payload = serde_json::json!({
            "state": match self.state {
                EvidenceState::ModelClaimed => "ModelClaimed",
                EvidenceState::Verified => "Verified",
                EvidenceState::Validated => "Validated",
                EvidenceState::Refuted => "Refuted",
            },
            "task_id": self.task_id,
            "commit_sha": self.commit_sha,
            "diff_hash": self.diff_hash,
            "command": self.command,
            "exit_code": self.exit_code,
            "stdout_hash": self.stdout_hash,
            "validator": self.validator,
            "rationale": self.rationale,
            "experiment_id": self.experiment_id,
            "model_fingerprint": self.model_fingerprint,
            "run_id": self.run_id,
            "budget": self.budget,
            "eval_metadata": self.eval_metadata,
        });
        if let Some(gs) = &self.git_state {
            payload["actual_commit_sha"] = gs.actual_commit_sha.clone().into();
            payload["actual_tree_hash"] = gs.actual_tree_hash.clone().into();
            payload["actual_diff_hash"] = gs.actual_diff_hash.clone().into();
            payload["git_dirty"] = gs.git_dirty.into();
            payload["git_repo_present"] = gs.git_repo_present.into();
            payload["claim_git_mismatch"] = gs.claim_git_mismatch.into();
        }
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

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvidenceError::InvalidPromotion { from, via } => {
                write!(
                    f,
                    "invalid promotion: {from:?} cannot be promoted via {via:?}"
                )
            }
        }
    }
}

impl std::error::Error for EvidenceError {}

/// The single deterministic transition function.
///
/// Allowed transitions only:
/// * `ModelClaimed` -> `Verified` via a [`EvidenceKind::ToolAttestation`];
/// * `ModelClaimed` -> `Verified` via a [`EvidenceKind::EvalAttestation`];
/// * `ModelClaimed` -> `Refuted` via a [`EvidenceKind::Discrepancy`];
/// * `Verified` -> `Validated` via a [`EvidenceKind::Validation`].
///
/// `ModelClaimed` -> `Validated` directly is rejected.
pub fn promote(
    entry: &EvidenceEntry,
    attestation: &EvidenceEntry,
) -> Result<EvidenceEntry, EvidenceError> {
    match (entry.state, attestation.kind) {
        (EvidenceState::ModelClaimed, EvidenceKind::ToolAttestation) => {
            Ok(EvidenceEntry::tool_attestation(
                entry.task_id.clone(),
                entry.commit_sha.clone(),
                entry.diff_hash.clone(),
                attestation.tool_version.clone(),
                attestation.env_fingerprint.clone(),
                attestation.command.clone(),
                attestation.exit_code,
                attestation.stdout_hash.clone(),
                attestation.ts.clone(),
            ))
        }
        (EvidenceState::ModelClaimed, EvidenceKind::EvalAttestation) => {
            Ok(EvidenceEntry::eval_attestation(
                entry.task_id.clone(),
                entry.commit_sha.clone(),
                entry.diff_hash.clone(),
                attestation.tool_version.clone(),
                attestation.env_fingerprint.clone(),
                attestation.command.clone(),
                attestation.exit_code,
                attestation.stdout_hash.clone(),
                attestation.experiment_id.clone(),
                attestation.model_fingerprint.clone(),
                attestation.run_id.clone(),
                attestation.budget.clone(),
                attestation.eval_metadata.clone(),
                attestation.ts.clone(),
            ))
        }
        (EvidenceState::ModelClaimed, EvidenceKind::Discrepancy) => Ok(EvidenceEntry::discrepancy(
            entry.task_id.clone(),
            entry.commit_sha.clone(),
            entry.diff_hash.clone(),
            attestation.command.clone(),
            attestation.exit_code,
            attestation.rationale.clone(),
            attestation.validator.clone(),
            attestation.ts.clone(),
        )),
        (EvidenceState::Verified, EvidenceKind::Validation) => Ok(EvidenceEntry::validation(
            entry.task_id.clone(),
            entry.commit_sha.clone(),
            entry.diff_hash.clone(),
            attestation.validator.clone(),
            attestation.rationale.clone(),
            attestation.ts.clone(),
        )),
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
            "T3",
            "abc123",
            "diff-1",
            "cargo-1.95.0",
            "env-x",
            "cargo test",
            0,
            "h1",
            "ts-2",
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

    fn eval_att() -> EvidenceEntry {
        EvidenceEntry::eval_attestation(
            "T3",
            "abc123",
            "diff-1",
            "cargo-1.95.0",
            "env-x",
            "cargo test --eval",
            0,
            "h1",
            Some("exp-1".to_string()),
            Some("mf-1".to_string()),
            Some("run-1".to_string()),
            Some("0.50 usd".to_string()),
            Some(serde_json::json!({ "pass@1": 0.8 })),
            "ts-2",
        )
    }

    fn disc() -> EvidenceEntry {
        EvidenceEntry::discrepancy(
            "T3",
            "abc123",
            "diff-1",
            "cargo build",
            1,
            "error: could not compile",
            "toolrunner:cargo build",
            "ts-2",
        )
    }

    #[test]
    fn test_eval_attestation_promotes_claim_to_verified() {
        let c = claim();
        let v = promote(&c, &eval_att()).unwrap();
        assert_eq!(v.kind, EvidenceKind::EvalAttestation);
        assert_eq!(v.state, EvidenceState::Verified);
        assert_eq!(v.experiment_id.as_deref(), Some("exp-1"));
        assert_eq!(v.model_fingerprint.as_deref(), Some("mf-1"));
        assert_eq!(v.run_id.as_deref(), Some("run-1"));
        assert_eq!(v.budget.as_deref(), Some("0.50 usd"));
        assert_eq!(v.eval_metadata, Some(serde_json::json!({ "pass@1": 0.8 })));
    }

    #[test]
    fn test_discrepancy_promotes_claim_to_refuted() {
        let c = claim();
        let r = promote(&c, &disc()).unwrap();
        assert_eq!(r.kind, EvidenceKind::Discrepancy);
        assert_eq!(r.state, EvidenceState::Refuted);
        assert_eq!(r.rationale, "error: could not compile");
        assert_eq!(r.validator, "toolrunner:cargo build");
    }

    #[test]
    fn test_discrepancy_on_verified_entry_is_invalid_promotion() {
        let c = claim();
        let v = promote(&c, &attestation()).unwrap();
        let err = promote(&v, &disc()).unwrap_err();
        assert!(matches!(err, EvidenceError::InvalidPromotion { .. }));
    }

    #[test]
    fn test_identity_fields_round_trip_through_ledger_payload() {
        let le = eval_att().to_ledger_entry();
        assert_eq!(le.payload["experiment_id"], "exp-1");
        assert_eq!(le.payload["model_fingerprint"], "mf-1");
        assert_eq!(le.payload["run_id"], "run-1");
        assert_eq!(le.payload["budget"], "0.50 usd");
        assert_eq!(
            le.payload["eval_metadata"],
            serde_json::json!({ "pass@1": 0.8 })
        );

        let bare_le = claim().to_ledger_entry();
        assert_eq!(bare_le.payload["experiment_id"], serde_json::Value::Null);
        assert_eq!(
            bare_le.payload["model_fingerprint"],
            serde_json::Value::Null
        );
        assert_eq!(bare_le.payload["run_id"], serde_json::Value::Null);
        assert_eq!(bare_le.payload["budget"], serde_json::Value::Null);
        assert_eq!(bare_le.payload["eval_metadata"], serde_json::Value::Null);
    }

    #[test]
    fn test_identity_field_change_alters_chain_hash() {
        // Two entries identical except for experiment_id must produce different
        // ledger head hashes: identity fields live inside the hashed payload.
        let mut a = eval_att();
        a.experiment_id = Some("exp-A".to_string());
        let mut b = eval_att();
        b.experiment_id = Some("exp-B".to_string());

        let dir = std::env::temp_dir().join("pf-t1-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let mut la = Ledger::new(dir.join(format!("id-a-{pid}.jsonl")));
        let mut lb = Ledger::new(dir.join(format!("id-b-{pid}.jsonl")));
        la.append(a.to_ledger_entry()).unwrap();
        lb.append(b.to_ledger_entry()).unwrap();
        let ha = la.verify_chain().unwrap().head_hash;
        let hb = lb.verify_chain().unwrap().head_hash;
        assert_eq!(ha.len(), 64);
        assert_ne!(ha, hb, "identity fields must be covered by the chain hash");
    }

    #[test]
    fn test_evidence_error_display_is_non_empty() {
        // Mutant guard: Display for EvidenceError -> Ok(Default::default()).
        // The rendered message must be non-empty.
        let c = claim();
        let err = promote(&c, &validation()).unwrap_err();
        assert!(!format!("{err}").is_empty());
    }
}
