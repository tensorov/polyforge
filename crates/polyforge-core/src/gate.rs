//! Deterministic `evaluate_complete` stage gate.
//!
//! A stage gate decides whether a task's evidence bundle is complete enough to
//! advance. It is a pure, synchronous, deterministic function of the ledger
//! contents and the caller-supplied requirements — no async, no randomness, no
//! wall-clock in the decision path.
//!
//! Rules (north-star contract):
//! * The chain is verified **first**. Any rewind/tamper/reorder is reported as
//!   [`GateError::LedgerIntegrity`] before any state counting happens.
//! * Every required [`EvidenceState`] for the task must be present. A
//!   `ModelClaimed` entry alone is never enough for a gate that requires
//!   `Verified` or `Validated`.
//! * The model cannot self-produce `Verified`/`Validated`; those states can
//!   only be reached via a tool attestation / validation (see [`crate::promote`]).
//! * The returned [`Evaluation`] carries the counts per final state, the list of
//!   still-missing required states, and the chain tail hash so the caller can
//!   pin the verdict to the exact ledger head it was computed against.

use std::fmt;

use crate::evidence::EvidenceState;
use crate::ledger::{EvidenceEntry as LedgerEntry, Ledger, LedgerError};

/// Per-state counts of evidence entries for a task, keyed by final state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    /// Entries whose final state is `ModelClaimed`.
    pub claimed: u64,
    /// Entries whose final state is `Verified`.
    pub verified: u64,
    /// Entries whose final state is `Validated`.
    pub validated: u64,
}

impl Counts {
    fn zero() -> Self {
        Self {
            claimed: 0,
            verified: 0,
            validated: 0,
        }
    }

    fn count_for(&self, state: EvidenceState) -> u64 {
        match state {
            EvidenceState::ModelClaimed => self.claimed,
            EvidenceState::Verified => self.verified,
            EvidenceState::Validated => self.validated,
            // Refuted entries are recorded but do not advance gates in M1.
            EvidenceState::Refuted => 0,
        }
    }
}

/// The result of evaluating a stage gate for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    /// The task this gate evaluated.
    pub task_id: String,
    /// `true` iff the chain verified AND every required state is present.
    pub passed: bool,
    /// Counts of this task's entries by final state.
    pub counts: Counts,
    /// Required states that were not satisfied (e.g. `"Verified"`), in the
    /// order they were requested. Empty when `passed` is `true`.
    pub missing: Vec<String>,
    /// SHA-256 hex of the ledger head (tail) entry the verdict was computed
    /// against.
    pub chain_tail_hash: String,
}

/// Errors produced by the stage gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The ledger chain failed integrity verification. Carries the broken
    /// sequence number and the expected/found hash detail where applicable.
    LedgerIntegrity {
        seq: u64,
        expected: String,
        found: String,
    },
    /// Underlying I/O failure reading the ledger.
    Io(String),
    /// A stored ledger line is not valid JSON / not a valid entry.
    Json(String),
    /// The task has no tri-state evidence entries in the ledger at all.
    TaskNotFound { task_id: String },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::LedgerIntegrity {
                seq,
                expected,
                found,
            } => write!(
                f,
                "ledger integrity broken at seq {seq}: expected {expected}, found {found}"
            ),
            GateError::Io(msg) => write!(f, "ledger I/O error: {msg}"),
            GateError::Json(msg) => write!(f, "ledger JSON error: {msg}"),
            GateError::TaskNotFound { task_id } => {
                write!(f, "no tri-state evidence found for task {task_id}")
            }
        }
    }
}

impl std::error::Error for GateError {}

impl From<LedgerError> for GateError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::Io(msg) => GateError::Io(msg),
            LedgerError::Json(msg) => GateError::Json(msg),
            LedgerError::ChainBroken {
                seq,
                expected,
                found,
            } => GateError::LedgerIntegrity {
                seq,
                expected,
                found,
            },
            LedgerError::EmptyChain => GateError::LedgerIntegrity {
                seq: 0,
                expected: "<genesis entry>".to_string(),
                found: "<empty chain>".to_string(),
            },
        }
    }
}

/// Evaluate the stage gate for `task_id`, requiring every state in `required`.
///
/// Deterministic: the result is a pure function of the ledger contents and
/// `required`. The chain is verified first; any integrity failure short-circuits
/// to [`GateError::LedgerIntegrity`].
pub fn evaluate_complete(
    ledger: &Ledger,
    task_id: &str,
    required: &[EvidenceState],
) -> Result<Evaluation, GateError> {
    // Integrity first: any tamper/rewind/reorder aborts before counting.
    let chain = ledger.verify_chain().map_err(GateError::from)?;
    let entries = ledger.iter_entries().map_err(GateError::from)?;

    let mut counts = Counts::zero();
    for entry in &entries {
        let Some(state) = state_of(entry) else {
            continue;
        };
        let Some(entry_task) = task_of(entry) else {
            continue;
        };
        if entry_task != task_id {
            continue;
        }
        match state {
            EvidenceState::ModelClaimed => counts.claimed += 1,
            EvidenceState::Verified => counts.verified += 1,
            EvidenceState::Validated => counts.validated += 1,
            // Refuted entries are recorded but never counted toward a gate.
            EvidenceState::Refuted => {}
        }
    }

    if counts.claimed == 0 && counts.verified == 0 && counts.validated == 0 {
        return Err(GateError::TaskNotFound {
            task_id: task_id.to_string(),
        });
    }

    let mut missing = Vec::new();
    for state in required {
        if counts.count_for(*state) == 0 {
            missing.push(format!("{state:?}"));
        }
    }

    Ok(Evaluation {
        task_id: task_id.to_string(),
        passed: missing.is_empty(),
        counts,
        missing,
        chain_tail_hash: chain.head_hash,
    })
}

/// Extract the tri-state verdict from a ledger entry's payload, if present.
fn state_of(entry: &LedgerEntry) -> Option<EvidenceState> {
    match entry.payload.get("state")?.as_str()? {
        "ModelClaimed" => Some(EvidenceState::ModelClaimed),
        "Verified" => Some(EvidenceState::Verified),
        "Validated" => Some(EvidenceState::Validated),
        "Refuted" => Some(EvidenceState::Refuted),
        _ => None,
    }
}

/// Extract the task id from a ledger entry's payload, if present.
fn task_of(entry: &LedgerEntry) -> Option<&str> {
    entry.payload.get("task_id")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::EvidenceEntry as TriState;
    use crate::promote;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("pf-todo4-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{}-{n}.jsonl", std::process::id()))
    }

    fn claim(task: &str) -> TriState {
        TriState::new_claim(task, "abc123", "diff-1", "ts-1")
    }

    fn attestation(task: &str) -> TriState {
        TriState::tool_attestation(
            task,
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

    fn validation(task: &str) -> TriState {
        TriState::validation(task, "abc123", "diff-1", "oracle", "all green", "ts-3")
    }

    /// Build a ledger with claim + verified (+ optionally validated) for `task`.
    fn ledger_with(task: &str, validated: bool) -> Ledger {
        let path = tmp_path(task);
        let mut ledger = Ledger::new(&path);
        let c = claim(task);
        let v = promote(&c, &attestation(task)).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
        if validated {
            let d = promote(&v, &validation(task)).unwrap();
            ledger.append(d.to_ledger_entry()).unwrap();
        }
        ledger
    }

    #[test]
    fn test_gate_passes_only_with_verified_evidence() {
        // Claim + Verified, requiring Verified -> passes.
        let ledger = ledger_with("T4", false);
        let eval = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap();
        assert!(
            eval.passed,
            "Verified evidence must satisfy a Verified gate"
        );
        assert!(eval.missing.is_empty());
        assert_eq!(eval.counts.claimed, 1);
        assert_eq!(eval.counts.verified, 1);
        assert_eq!(eval.counts.validated, 0);

        // Same ledger, but requiring Validated (only Verified present) -> fails.
        let eval = evaluate_complete(&ledger, "T4", &[EvidenceState::Validated]).unwrap();
        assert!(
            !eval.passed,
            "Validated gate must not pass on Verified-only evidence"
        );
        assert_eq!(eval.missing, vec!["Validated"]);

        // A claim alone is never enough for a Verified gate.
        let path = tmp_path("claim-only");
        let mut claim_only = Ledger::new(&path);
        claim_only.append(claim("T4").to_ledger_entry()).unwrap();
        let eval = evaluate_complete(&claim_only, "T4", &[EvidenceState::Verified]).unwrap();
        assert!(
            !eval.passed,
            "ModelClaimed alone must never satisfy a Verified gate"
        );
        assert_eq!(eval.missing, vec!["Verified"]);
        assert_eq!(eval.counts.claimed, 1);
        assert_eq!(eval.counts.verified, 0);
    }

    #[test]
    fn test_gate_fails_on_missing_evidence() {
        let ledger = ledger_with("T4", false);
        let eval = evaluate_complete(
            &ledger,
            "T4",
            &[EvidenceState::Verified, EvidenceState::Validated],
        )
        .unwrap();
        assert!(!eval.passed);
        assert_eq!(eval.missing, vec!["Validated"]);
        assert_eq!(eval.counts.verified, 1);
        assert_eq!(eval.counts.validated, 0);
    }

    #[test]
    fn test_gate_fails_on_tampered_chain() {
        let path = tmp_path("tamper");
        let mut ledger = Ledger::new(&path);
        let c = claim("T4");
        let v = promote(&c, &attestation("T4")).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
        ledger.verify_chain().unwrap();

        // Tamper: rewrite the kind value of entry 0 (keeps JSON valid, breaks hash).
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        lines[0] = lines[0].replacen("ModelClaim", "ModelClaimZ", 1);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let err = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap_err();
        assert!(
            matches!(err, GateError::LedgerIntegrity { .. }),
            "tampered chain must surface as LedgerIntegrity, got {err:?}"
        );
    }

    #[test]
    fn test_evaluation_deterministic() {
        // Same ledger, same inputs -> identical Evaluation.
        let ledger = ledger_with("T4", true);
        let a = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap();
        let b = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap();
        assert_eq!(a, b);

        // Two independently-built identical ledgers -> identical Evaluation.
        let other = ledger_with("T4", true);
        let c = evaluate_complete(&other, "T4", &[EvidenceState::Verified]).unwrap();
        assert_eq!(
            a, c,
            "same evidence sequence must yield the same gate verdict"
        );
    }

    fn discrepancy(task: &str, exit_code: i32) -> TriState {
        TriState::discrepancy(
            task,
            "abc123",
            "diff-1",
            "cargo test",
            exit_code,
            "boom",
            "toolrunner:cargo",
            "ts-4",
        )
    }

    #[test]
    fn test_state_of_maps_refuted() {
        let entry = discrepancy("T4", 1).to_ledger_entry();
        assert_eq!(state_of(&entry), Some(EvidenceState::Refuted));
    }

    #[test]
    fn test_gate_claim_plus_discrepancy_never_passes() {
        // claim + Discrepancy only: the claim is tri-state evidence (no
        // TaskNotFound), but Refuted never advances the gate.
        let path = tmp_path("refuted");
        let mut ledger = Ledger::new(&path);
        let c = claim("T4");
        let d = promote(&c, &discrepancy("T4", 1)).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();

        let eval = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap();
        assert!(!eval.passed);
        assert_eq!(eval.counts.verified, 0);
        assert_eq!(eval.counts.claimed, 1);
        assert_eq!(eval.missing, vec!["Verified"]);
    }

    #[test]
    fn test_gate_discrepancy_only_is_task_not_found() {
        // A task whose only entries are Discrepancy/Refuted has no tri-state
        // evidence: refutations are recorded but never satisfy a gate.
        let path = tmp_path("refuted-only");
        let mut ledger = Ledger::new(&path);
        ledger
            .append(discrepancy("T4", 1).to_ledger_entry())
            .unwrap();

        let err = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap_err();
        assert!(
            matches!(err, GateError::TaskNotFound { .. }),
            "discrepancy-only task must be TaskNotFound, got {err:?}"
        );
    }

    #[test]
    fn test_gate_reports_tail_hash() {
        let ledger = ledger_with("T4", true);
        let chain = ledger.verify_chain().unwrap();
        let eval = evaluate_complete(&ledger, "T4", &[EvidenceState::Verified]).unwrap();
        assert_eq!(eval.chain_tail_hash, chain.head_hash);
        assert_eq!(eval.chain_tail_hash.len(), 64);
        assert!(eval
            .chain_tail_hash
            .chars()
            .all(|ch| ch.is_ascii_hexdigit()));
    }
}
