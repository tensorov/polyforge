//! Runnable end-to-end demonstration of the tri-state evidence lifecycle.
//!
//! This example walks through the exact flow a PolyForge user performs:
//!
//! 1. A model appends a `ModelClaim` (state `ModelClaimed`).
//! 2. An allowlisted tool attestation is applied via [`promote`], which
//!    produces a `Verified` entry (the model can never self-produce this).
//! 3. The stage gate [`evaluate_complete`] runs against the ledger and prints
//!    a deterministic summary (passed, per-state counts, chain tail hash).
//! 4. The temp ledger (and its head-anchor sidecar) is removed.
//!
//! The ledger path is taken from the first CLI argument when present, and
//! otherwise defaults to a unique temp path
//! (`<temp>/polyforge_ledger_<pid>.jsonl`). The example panics on any error,
//! so a non-zero exit status always means something went wrong.

use polyforge_core::{
    evaluate_complete, promote, Evaluation, EvidenceState, Ledger, TriStateEvidence,
};

fn main() {
    let task_id = "demo";
    let commit_sha = "abc123";
    let diff_hash = "d1";
    let ledger_path = match std::env::args().nth(1) {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::temp_dir().join(format!("polyforge_ledger_{}.jsonl", std::process::id())),
    };

    println!("== PolyForge ledger_flow example ==");
    println!("ledger path: {}", ledger_path.display());
    println!();

    let mut ledger = Ledger::new(&ledger_path);

    // Step 1: the model records a claim (always ModelClaimed).
    let claim = TriStateEvidence::new_claim(task_id, commit_sha, diff_hash, "2026-08-07T00:00:00Z");
    let claim_id = ledger
        .append(claim.to_ledger_entry())
        .expect("failed to append model claim");
    println!("[1/3] appended ModelClaim  (seq {claim_id}, state=ModelClaimed)");
    println!();

    // Step 2: an allowlisted tool attests the claim; `promote` yields Verified.
    let attestation = TriStateEvidence::tool_attestation(
        task_id,
        commit_sha,
        diff_hash,
        "cargo-1.95.0",
        "env-polyforge-demo",
        "cargo test --workspace",
        0,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "2026-08-07T00:00:01Z",
    );
    let verified =
        promote(&claim, &attestation).expect("ModelClaimed + ToolAttestation must promote");
    let verified_id = ledger
        .append(verified.to_ledger_entry())
        .expect("failed to append tool attestation");
    println!("[2/3] applied ToolAttestation (seq {verified_id}, state=Verified)");
    println!();

    // Step 3: run the stage gate over the ledger.
    let evaluation = evaluate_complete(&ledger, task_id, &[EvidenceState::Verified])
        .expect("gate evaluation failed");
    print_evaluation(&evaluation);

    // Cleanup: remove the temp ledger and its head-anchor sidecar.
    let anchor = ledger_path.with_extension("jsonl.anchor");
    let _ = std::fs::remove_file(&ledger_path);
    let _ = std::fs::remove_file(anchor);
    println!("[cleanup] removed ledger file and anchor sidecar");
}

/// Print a human-readable summary of the gate `Evaluation`.
fn print_evaluation(evaluation: &Evaluation) {
    println!("[3/3] gate evaluation for task '{}':", evaluation.task_id);
    println!("  passed:          {}", evaluation.passed);
    println!(
        "  counts:          claimed={} verified={} validated={}",
        evaluation.counts.claimed, evaluation.counts.verified, evaluation.counts.validated
    );
    println!("  missing:         {:?}", evaluation.missing);
    println!("  chain_tail_hash: {}", evaluation.chain_tail_hash);

    assert!(
        evaluation.passed,
        "gate must pass with Verified evidence present"
    );
    assert!(
        evaluation.counts.verified >= 1,
        "expected at least one Verified entry"
    );
    assert_eq!(
        evaluation.chain_tail_hash.len(),
        64,
        "chain_tail_hash must be a real SHA-256 hex digest"
    );
    println!("  (assertions ok: passed, verified>=1, hash is 64-hex SHA-256)");
}
