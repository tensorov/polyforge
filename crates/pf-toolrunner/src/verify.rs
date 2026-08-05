//! T6: gated verify-and-append — the single choke point where a claim
//! becomes `Verified`.
//!
//! Flow: load the `ModelClaimed` entry from the ledger → verify chain
//! integrity → run the allowlisted tool → build a `ToolAttestation` from the
//! `RunOutput` → promote via [`pf_core::evidence::promote`] → append the
//! `Verified` entry back to the ledger.
//!
//! Invariants:
//! * Nothing is promoted without a successful (exit 0) tool run.
//! * The model never passes raw shell strings: args go through the runner's
//!   typed-arg validation and the binary is spawned directly (no shell).
//! * No wall-clock: the attestation reuses the claim's `ts` datum.
//! * A failed gate leaves the ledger untouched (zero partial writes).

use pf_core::evidence::{promote, EvidenceEntry, EvidenceKind, EvidenceState};
use pf_core::ledger::{EntryId, EvidenceEntry as LedgerEntry, Ledger};

use crate::runner::{run, RunnerError, Tool};

/// Verify a claim by running an allowlisted tool, then append the promoted
/// `Verified` entry to the ledger.
///
/// `claim_id` is the ledger sequence number of the `ModelClaimed` entry.
/// The tool must exit 0; any other exit is a failure and nothing is appended.
pub fn verify_and_append(
    ledger: &mut Ledger,
    task_id: &str,
    claim_id: EntryId,
    tool: &Tool,
    args: &[String],
) -> Result<EvidenceEntry, RunnerError> {
    let claim = load_claim(ledger, task_id, claim_id)?;
    ledger
        .verify_chain()
        .map_err(|e| RunnerError::Ledger(format!("{e:?}")))?;
    let output = run(tool, args)?;
    if output.exit_code != 0 {
        return Err(RunnerError::ToolFailed {
            exit_code: output.exit_code,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let attestation = output.to_attestation(
        claim.task_id.clone(),
        claim.commit_sha.clone(),
        claim.diff_hash.clone(),
        claim.ts.clone(),
    );
    let verified = promote(&claim, &attestation)
        .map_err(|e| RunnerError::Promote(format!("{e:?}")))?;
    ledger
        .append(verified.to_ledger_entry())
        .map_err(|e| RunnerError::Ledger(format!("{e:?}")))?;
    Ok(verified)
}

/// Find the `ModelClaimed` entry at `claim_id` and reconstruct the tri-state
/// record from its ledger payload.
fn load_claim(
    ledger: &Ledger,
    task_id: &str,
    claim_id: EntryId,
) -> Result<EvidenceEntry, RunnerError> {
    let entries = ledger
        .iter_entries()
        .map_err(|e| RunnerError::Ledger(format!("{e:?}")))?;
    let entry = entries
        .iter()
        .find(|e| e.seq == claim_id)
        .ok_or(RunnerError::ClaimNotFound(claim_id))?;
    if entry.kind != "ModelClaim" {
        return Err(RunnerError::ClaimNotFound(claim_id));
    }
    let claim = tri_state_from_ledger(entry)?;
    if claim.state != EvidenceState::ModelClaimed {
        return Err(RunnerError::ClaimNotFound(claim_id));
    }
    if claim.task_id != task_id {
        return Err(RunnerError::ClaimNotFound(claim_id));
    }
    Ok(claim)
}

/// Reconstruct a tri-state [`EvidenceEntry`] from a ledger entry. The payload
/// schema mirrors [`EvidenceEntry::to_ledger_entry`].
fn tri_state_from_ledger(entry: &LedgerEntry) -> Result<EvidenceEntry, RunnerError> {
    let kind = match entry.kind.as_str() {
        "ModelClaim" => EvidenceKind::ModelClaim,
        "ToolAttestation" => EvidenceKind::ToolAttestation,
        "Validation" => EvidenceKind::Validation,
        other => return Err(RunnerError::Promote(format!("unknown kind: {other}"))),
    };
    let state = match entry.payload["state"].as_str() {
        Some("ModelClaimed") => EvidenceState::ModelClaimed,
        Some("Verified") => EvidenceState::Verified,
        Some("Validated") => EvidenceState::Validated,
        _ => return Err(RunnerError::Promote("missing or invalid state".into())),
    };
    Ok(EvidenceEntry {
        task_id: entry.payload["task_id"].as_str().unwrap_or_default().to_string(),
        commit_sha: entry.payload["commit_sha"].as_str().unwrap_or_default().to_string(),
        diff_hash: entry.payload["diff_hash"].as_str().unwrap_or_default().to_string(),
        kind,
        state,
        tool_version: entry.tool_version.clone(),
        env_fingerprint: entry.env_fingerprint.clone(),
        command: entry.payload["command"].as_str().unwrap_or_default().to_string(),
        exit_code: entry.payload["exit_code"].as_i64().unwrap_or(0) as i32,
        stdout_hash: entry.payload["stdout_hash"].as_str().unwrap_or_default().to_string(),
        validator: entry.payload["validator"].as_str().unwrap_or_default().to_string(),
        rationale: entry.payload["rationale"].as_str().unwrap_or_default().to_string(),
        ts: entry.ts.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::runner::lookup;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pf-todo6-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{}-{n}.jsonl", std::process::id()))
    }

    fn claim(task_id: &str) -> EvidenceEntry {
        EvidenceEntry::new_claim(task_id, "abc123", "diff-1", "ts-1")
    }

    fn append_claim(ledger: &mut Ledger, task_id: &str) -> EntryId {
        ledger.append(claim(task_id).to_ledger_entry()).unwrap()
    }

    fn tool(name: &str) -> Tool {
        lookup(name).expect("tool on allowlist")
    }

    #[test]
    fn test_verify_promotes_claim_with_tool_attestation() {
        let path = tmp_path("promote");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");

        let verified =
            verify_and_append(&mut ledger, "T6", claim_id, &tool("cargo --version"), &[])
                .unwrap();

        assert_eq!(verified.kind, EvidenceKind::ToolAttestation);
        assert_eq!(verified.state, EvidenceState::Verified);
        assert_eq!(verified.task_id, "T6");
        assert_eq!(verified.exit_code, 0);
        assert_eq!(verified.stdout_hash.len(), 64);
        assert!(!verified.tool_version.is_empty());
        assert!(!verified.env_fingerprint.is_empty());

        let entries = ledger.iter_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].kind, "ToolAttestation");
        assert_eq!(entries[1].payload["state"], "Verified");
        assert_eq!(entries[1].payload["exit_code"], 0);
        ledger.verify_chain().unwrap();
    }

    #[test]
    fn test_verify_fails_without_claim() {
        let path = tmp_path("no-claim");
        let mut ledger = Ledger::new(&path);
        let err =
            verify_and_append(&mut ledger, "T6", 0, &tool("cargo --version"), &[]).unwrap_err();
        assert!(matches!(err, RunnerError::ClaimNotFound(0)));
        assert!(ledger.iter_entries().unwrap().is_empty());
    }

    #[test]
    fn test_verify_fails_on_tool_error() {
        let path = tmp_path("tool-error");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");
        let err = verify_and_append(
            &mut ledger,
            "T6",
            claim_id,
            &tool("cargo build"),
            &["--definitely-not-a-flag".to_string()],
        )
        .unwrap_err();
        assert!(matches!(err, RunnerError::ToolFailed { .. }));
        let entries = ledger.iter_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "ModelClaim");
    }

    #[test]
    fn test_appended_entry_is_verified_state() {
        let path = tmp_path("verified-state");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");
        verify_and_append(&mut ledger, "T6", claim_id, &tool("cargo --version"), &[]).unwrap();
        let entries = ledger.iter_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].kind, "ToolAttestation");
        assert_eq!(entries[1].payload["state"], "Verified");
        assert_eq!(entries[1].payload["task_id"], "T6");
        assert_eq!(entries[1].payload["commit_sha"], "abc123");
        assert_eq!(entries[1].payload["diff_hash"], "diff-1");
    }

    #[test]
    fn test_verify_rejects_wrong_task_id() {
        let path = tmp_path("wrong-task");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");
        let err =
            verify_and_append(&mut ledger, "OTHER", claim_id, &tool("cargo --version"), &[])
                .unwrap_err();
        assert!(matches!(err, RunnerError::ClaimNotFound(_)));
        assert_eq!(ledger.iter_entries().unwrap().len(), 1);
    }

    #[test]
    fn test_verify_rejects_non_claim_entry() {
        let path = tmp_path("non-claim");
        let mut ledger = Ledger::new(&path);
        let v = EvidenceEntry::validation("T6", "abc123", "diff-1", "oracle", "ok", "ts-1");
        ledger.append(v.to_ledger_entry()).unwrap();
        let err =
            verify_and_append(&mut ledger, "T6", 0, &tool("cargo --version"), &[]).unwrap_err();
        assert!(matches!(err, RunnerError::ClaimNotFound(0)));
    }
}