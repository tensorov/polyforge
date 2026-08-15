//! T6: gated verify-and-append — the single choke point where a claim
//! becomes `Verified`.
//!
//! Flow: load the `ModelClaimed` entry from the ledger → verify chain
//! integrity → run the allowlisted tool → build a `ToolAttestation` from the
//! `RunOutput` → promote via [`polyforge_core::evidence::promote`] → append the
//! `Verified` entry back to the ledger.
//!
//! Invariants:
//! * Nothing is promoted to `Verified` without a successful (exit 0) tool run.
//! * A failed (non-zero exit) tool run appends a `Discrepancy`/`Refuted` trace
//!   recording tool, exit code, and truncated stderr — the failure itself is
//!   evidence — before the caller receives `ToolFailed`.
//! * The model never passes raw shell strings: args go through the runner's
//!   typed-arg validation and the binary is spawned directly (no shell).
//! * Attestations and discrepancies carry a real wall-clock `ts` (epoch
//!   millis from `SystemTime::now()`), never the claim's `ts` datum.
//! * The chain is verified before any append (zero partial writes on
//!   integrity failure).

use std::path::{Path, PathBuf};

use polyforge_core::evidence::{promote, EvidenceEntry, EvidenceKind, EvidenceState, GitState};
use polyforge_core::ledger::{EntryId, EvidenceEntry as LedgerEntry, Ledger};

use crate::runner::{run, sha256_hex, RunnerError, Tool};

/// Maximum number of stderr bytes recorded in the ledger for a failed run.
const STDERR_LEDGER_LIMIT: usize = 2048;

/// Cap a string at `limit` bytes, never splitting a UTF-8 codepoint.
fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Wall-clock timestamp datum (epoch millis), same format as the MCP
/// server's `now_ts`.
fn now_ts() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Find the nearest ancestor directory containing a `.git` entry.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(PathBuf::from)
}

/// Run a git command in `root`, returning trimmed stdout on success.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Capture the actual git state of the repo containing `start_path`.
///
/// Fails open when no repo is found: `git_repo_present` is false,
/// `actual_commit_sha` is `"none"`, and `git_dirty` is false.
fn git_state_from(start_path: &Path) -> GitState {
    let Some(root) = find_git_root(start_path) else {
        return GitState {
            actual_commit_sha: "none".into(),
            actual_tree_hash: String::new(),
            actual_diff_hash: String::new(),
            git_dirty: false,
            git_repo_present: false,
            claim_git_mismatch: false,
        };
    };
    let actual_commit_sha = git_output(&root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let actual_tree_hash = git_output(&root, &["rev-parse", "HEAD^{tree}"]).unwrap_or_default();
    let diff = git_output(&root, &["diff", "HEAD"]).unwrap_or_default();
    let actual_diff_hash = sha256_hex(diff.as_bytes());
    let git_dirty = git_output(&root, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    GitState {
        actual_commit_sha,
        actual_tree_hash,
        actual_diff_hash,
        git_dirty,
        git_repo_present: true,
        claim_git_mismatch: false,
    }
}

/// Git state for the repo being verified: process CWD first, then
/// `CARGO_MANIFEST_DIR` ancestors as fallback. `claim_git_mismatch` is
/// computed against the claim being verified.
fn current_git_state(claim: &EvidenceEntry) -> GitState {
    let mut gs = std::env::current_dir()
        .ok()
        .map(|cwd| git_state_from(&cwd))
        .filter(|g| g.git_repo_present)
        .or_else(|| {
            std::env::var("CARGO_MANIFEST_DIR")
                .ok()
                .map(|m| git_state_from(Path::new(&m)))
        })
        .unwrap_or_else(|| GitState {
            actual_commit_sha: "none".into(),
            actual_tree_hash: String::new(),
            actual_diff_hash: String::new(),
            git_dirty: false,
            git_repo_present: false,
            claim_git_mismatch: false,
        });
    gs.claim_git_mismatch = gs.git_repo_present
        && (claim.commit_sha != gs.actual_commit_sha || claim.diff_hash != gs.actual_diff_hash);
    gs
}

/// Verify a claim by running an allowlisted tool, then append the promoted
/// `Verified` entry to the ledger.
///
/// `claim_id` is the ledger sequence number of the `ModelClaimed` entry.
/// The tool must exit 0; any other exit is a failure that appends a
/// `Discrepancy`/`Refuted` trace before the caller receives `ToolFailed`.
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
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let discrepancy = EvidenceEntry::discrepancy(
            claim.task_id.clone(),
            claim.commit_sha.clone(),
            claim.diff_hash.clone(),
            output.command.clone(),
            output.exit_code,
            truncate(&stderr, STDERR_LEDGER_LIMIT),
            format!("toolrunner:{}", tool.name),
            now_ts(),
        );
        let mut refuted =
            promote(&claim, &discrepancy).map_err(|e| RunnerError::Promote(format!("{e:?}")))?;
        refuted.git_state = Some(current_git_state(&claim));
        ledger
            .append(refuted.to_ledger_entry())
            .map_err(|e| RunnerError::Ledger(format!("{e:?}")))?;
        return Err(RunnerError::ToolFailed {
            exit_code: output.exit_code,
            stderr: truncate(&stderr, STDERR_LEDGER_LIMIT),
        });
    }
    let attestation = output.to_attestation(
        claim.task_id.clone(),
        claim.commit_sha.clone(),
        claim.diff_hash.clone(),
        now_ts(),
    );
    let mut verified =
        promote(&claim, &attestation).map_err(|e| RunnerError::Promote(format!("{e:?}")))?;
    verified.git_state = Some(current_git_state(&claim));
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
        task_id: entry.payload["task_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        commit_sha: entry.payload["commit_sha"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        diff_hash: entry.payload["diff_hash"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        kind,
        state,
        tool_version: entry.tool_version.clone(),
        env_fingerprint: entry.env_fingerprint.clone(),
        command: entry.payload["command"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        exit_code: entry.payload["exit_code"].as_i64().unwrap_or(0) as i32,
        stdout_hash: entry.payload["stdout_hash"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        validator: entry.payload["validator"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        rationale: entry.payload["rationale"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        experiment_id: entry.payload["experiment_id"].as_str().map(str::to_string),
        model_fingerprint: entry.payload["model_fingerprint"]
            .as_str()
            .map(str::to_string),
        run_id: entry.payload["run_id"].as_str().map(str::to_string),
        budget: entry.payload["budget"].as_str().map(str::to_string),
        eval_metadata: entry.payload.get("eval_metadata").cloned(),
        git_state: entry.payload.get("git_repo_present").map(|_| GitState {
            actual_commit_sha: entry.payload["actual_commit_sha"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            actual_tree_hash: entry.payload["actual_tree_hash"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            actual_diff_hash: entry.payload["actual_diff_hash"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            git_dirty: entry.payload["git_dirty"].as_bool().unwrap_or(false),
            git_repo_present: entry.payload["git_repo_present"].as_bool().unwrap_or(false),
            claim_git_mismatch: entry.payload["claim_git_mismatch"]
                .as_bool()
                .unwrap_or(false),
        }),
        ts: entry.ts.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

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

    // Run truncate on a worker thread so a non-terminating mutant (the
    // `-=` -> `/=` division mutation) fails the test instead of hanging it.
    fn truncate_bounded(s: &str, limit: usize) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = s.to_string();
        std::thread::spawn(move || {
            tx.send(truncate(&s, limit)).unwrap();
        });
        rx.recv_timeout(Duration::from_millis(250))
            .expect("truncate must terminate")
    }

    #[test]
    fn test_failed_run_appends_discrepancy_entry() {
        let path = tmp_path("discrepancy");
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
        assert_eq!(entries.len(), 2, "failed run must append a Discrepancy");
        assert_eq!(entries[1].kind, "Discrepancy");
        assert_eq!(entries[1].payload["state"], "Refuted");
        assert_eq!(entries[1].payload["task_id"], "T6");
        assert_eq!(entries[1].payload["validator"], "toolrunner:cargo build");
        let rationale = entries[1].payload["rationale"].as_str().unwrap();
        assert!(rationale.len() <= 2048, "stderr must be truncated ~2KB");
        assert!(!rationale.is_empty());
        ledger.verify_chain().unwrap();
    }

    #[test]
    fn test_failed_run_without_claim_appends_nothing() {
        let path = tmp_path("discrepancy-no-claim");
        let mut ledger = Ledger::new(&path);
        let err = verify_and_append(
            &mut ledger,
            "T6",
            0,
            &tool("cargo build"),
            &["--definitely-not-a-flag".to_string()],
        )
        .unwrap_err();
        assert!(matches!(err, RunnerError::ClaimNotFound(0)));
        assert!(ledger.iter_entries().unwrap().is_empty());
    }

    #[test]
    fn test_verify_promotes_claim_with_tool_attestation() {
        let path = tmp_path("promote");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");

        let verified =
            verify_and_append(&mut ledger, "T6", claim_id, &tool("cargo --version"), &[]).unwrap();

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

    // T6: the attestation ts must be a real wall-clock epoch-millis value,
    // never the claim's `ts` datum ("ts-1" in fixtures).
    #[test]
    fn test_attestation_ts_is_wallclock_epoch_millis() {
        let path = tmp_path("wallclock-att");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6");

        let verified =
            verify_and_append(&mut ledger, "T6", claim_id, &tool("cargo --version"), &[]).unwrap();

        assert_ne!(
            verified.ts, "ts-1",
            "attestation must not reuse the claim ts datum"
        );
        let ts: u64 = verified
            .ts
            .parse()
            .expect("attestation ts must be numeric epoch millis");
        assert!(
            ts > 1_700_000_000_000,
            "epoch millis must be a plausible modern timestamp, got {ts}"
        );
    }

    // T6: the discrepancy ts (failed-run trace) must also be wall-clock.
    #[test]
    fn test_discrepancy_ts_is_wallclock_epoch_millis() {
        let path = tmp_path("wallclock-disc");
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
        assert_eq!(entries[1].kind, "Discrepancy");
        assert_ne!(
            entries[1].ts, "ts-1",
            "discrepancy must not reuse the claim ts datum"
        );
        let ts: u64 = entries[1]
            .ts
            .parse()
            .expect("discrepancy ts must be numeric epoch millis");
        assert!(
            ts > 1_700_000_000_000,
            "epoch millis must be a plausible modern timestamp, got {ts}"
        );
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
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "ModelClaim");
        assert_eq!(entries[1].kind, "Discrepancy");
        assert_eq!(entries[1].payload["state"], "Refuted");
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
        let err = verify_and_append(
            &mut ledger,
            "OTHER",
            claim_id,
            &tool("cargo --version"),
            &[],
        )
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

    // Mutant 15 (verify.rs:29:5, `end -= 1` -> `end += 1`): the loop must
    // decrement towards a char boundary; the ASCII case returns "hel".
    #[test]
    fn test_truncate_ascii_caps_at_limit() {
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("hello", 10), "hello");
    }

    // Mutant 16 (verify.rs:29:5, `end -= 1` -> `end *= 1`): with the
    // multiplier mutation the boundary never recedes, so the whole string is
    // returned; truncation must still cap it.
    #[test]
    fn test_truncate_ascii_never_returns_longer_than_limit() {
        let s = truncate("hello", 3);
        assert_eq!(s, "hel");
        assert!(s.len() <= 3);
    }

    // Mutant 17 (verify.rs:34:5, delete `!`): with the negated boundary check
    // the loop keeps decrementing to 0 and returns the empty string, so a
    // non-empty UTF-8 prefix is required.
    #[test]
    fn test_truncate_utf8_preserves_codepoints() {
        assert_eq!(truncate_bounded("héllo", 2), "h");
        assert_eq!(truncate_bounded("héllo", 3), "hé");
    }

    // Mutant 18 (verify.rs:34:5, `end -= 1` -> `end += 1`): with the increment
    // mutation the boundary never reaches a char boundary from a non-boundary
    // start, so the loop runs until a panic on slicing; the correct result
    // terminates immediately.
    #[test]
    fn test_truncate_utf8_terminates_with_prefix() {
        let s = truncate_bounded("héllo", 2);
        assert_eq!(s, "h");
        assert_eq!(s.len(), 1);
    }

    // Mutant 19 (verify.rs:34:5, `end -= 1` -> `end /= 1`): with the division
    // mutation the boundary never changes and the loop spins forever; the
    // watchdog thread fails the test instead of hanging it.
    #[test]
    fn test_truncate_utf8_loop_terminates() {
        assert_eq!(truncate_bounded("héllo", 2), "h");
    }

    // Mutant 20 (verify.rs:123:5, tri_state_from_ledger -> Ok(Verified)): a
    // claim entry whose payload carries no validation must reconstruct with
    // its true state, never a fabricated Verified.
    #[test]
    fn test_tri_state_from_ledger_keeps_claim_state() {
        let path = tmp_path("tri-claim");
        let mut ledger = Ledger::new(&path);
        append_claim(&mut ledger, "T6");
        let raw = ledger.iter_entries().unwrap().remove(0);
        let restored = tri_state_from_ledger(&raw).unwrap();
        assert_eq!(restored.kind, EvidenceKind::ModelClaim);
        assert_eq!(restored.state, EvidenceState::ModelClaimed);
        assert_eq!(restored.task_id, "T6");
        assert_eq!(restored.commit_sha, "abc123");
    }

    // Mutant 21 (verify.rs:123:5, tri_state_from_ledger -> Ok(Validated)): a
    // claim entry must never reconstruct as Validated without a validation
    // entry in the chain.
    #[test]
    fn test_tri_state_from_ledger_never_fabricates_validated() {
        let path = tmp_path("tri-fabricate");
        let mut ledger = Ledger::new(&path);
        append_claim(&mut ledger, "T6");
        let raw = ledger.iter_entries().unwrap().remove(0);
        let restored = tri_state_from_ledger(&raw).unwrap();
        assert_eq!(restored.state, EvidenceState::ModelClaimed);
        assert_ne!(restored.state, EvidenceState::Validated);
    }

    // Mutant 20 (verify.rs:132:9, delete match arm Some("Verified")): a
    // Verified entry must round-trip; with the arm deleted the match falls
    // through to the error arm and the unwrap panics.
    #[test]
    fn test_tri_state_from_ledger_round_trips_verified() {
        let v = EvidenceEntry::tool_attestation(
            "T6",
            "abc123",
            "diff-1",
            "cargo 1.0",
            "fp",
            "cargo --version",
            0,
            "h".repeat(64),
            "ts-1",
        );
        let raw = v.to_ledger_entry();
        let restored = tri_state_from_ledger(&raw).unwrap();
        assert_eq!(restored.kind, EvidenceKind::ToolAttestation);
        assert_eq!(restored.state, EvidenceState::Verified);
        assert_eq!(restored.command, "cargo --version");
    }

    // Mutant 21 (verify.rs:133:9, delete match arm Some("Validated")): a
    // Validated entry must round-trip; with the arm deleted the match falls
    // through to the error arm and the unwrap panics.
    #[test]
    fn test_tri_state_from_ledger_round_trips_validated() {
        let v = EvidenceEntry::validation("T6", "abc123", "diff-1", "oracle", "ok", "ts-1");
        let raw = v.to_ledger_entry();
        let restored = tri_state_from_ledger(&raw).unwrap();
        assert_eq!(restored.kind, EvidenceKind::Validation);
        assert_eq!(restored.state, EvidenceState::Validated);
        assert_eq!(restored.validator, "oracle");
    }

    // T3: git-state introspection. Create a throwaway git repo with one
    // committed file and return its path.
    fn temp_git_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-todo3-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t3@test"]);
        run(&["config", "user.name", "t3"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "init"]);
        dir
    }

    fn git_head(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // T3: a clean temp repo reports its real HEAD and tree hash, present=true,
    // dirty=false.
    #[test]
    fn test_git_state_from_reports_real_head() {
        let dir = temp_git_repo("head");
        let gs = git_state_from(&dir);
        assert!(gs.git_repo_present);
        assert_eq!(gs.actual_commit_sha, git_head(&dir));
        assert_eq!(gs.actual_commit_sha.len(), 40);
        assert_eq!(gs.actual_tree_hash.len(), 40);
        assert!(!gs.git_dirty);
    }

    // T3: an uncommitted change flips the dirty marker.
    #[test]
    fn test_git_state_from_marks_dirty_worktree() {
        let dir = temp_git_repo("dirty");
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let gs = git_state_from(&dir);
        assert!(gs.git_repo_present);
        assert!(gs.git_dirty);
    }

    // T3: a non-repo directory fails open: present=false, sha="none",
    // dirty=false (not-a-repo is distinct from dirty).
    #[test]
    fn test_git_state_from_non_repo_fails_open() {
        let dir = std::env::temp_dir().join(format!("pf-todo3-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gs = git_state_from(&dir);
        assert!(!gs.git_repo_present);
        assert_eq!(gs.actual_commit_sha, "none");
        assert!(!gs.git_dirty);
    }

    // T3 acceptance: a claim pinned to "abc123" verified inside a real git
    // repo (this workspace) must record the ACTUAL HEAD in the attestation
    // payload and flag claim_git_mismatch=true, while the ledger entry key
    // stays the claim's key (promote untouched).
    #[test]
    fn test_verify_payload_carries_actual_git_state() {
        let path = tmp_path("git-att");
        let mut ledger = Ledger::new(&path);
        let claim_id = append_claim(&mut ledger, "T6"); // commit "abc123"

        let verified =
            verify_and_append(&mut ledger, "T6", claim_id, &tool("cargo --version"), &[]).unwrap();
        let returned = verified.git_state.as_ref().unwrap();
        assert!(returned.git_repo_present);
        assert_eq!(returned.actual_commit_sha.len(), 40);

        let entries = ledger.iter_entries().unwrap();
        let payload = &entries[1].payload;
        assert_eq!(entries[1].kind, "ToolAttestation");
        // Actual git state is recorded in the payload.
        let actual = payload["actual_commit_sha"].as_str().unwrap();
        assert_eq!(actual.len(), 40, "actual_commit_sha must be a real sha");
        assert_ne!(actual, "abc123");
        assert_eq!(payload["git_repo_present"].as_bool(), Some(true));
        assert_eq!(payload["claim_git_mismatch"].as_bool(), Some(true));
        // The ledger entry key stays the claim's key (promote unchanged).
        assert_eq!(payload["commit_sha"], "abc123");
        // Round-trip through tri_state_from_ledger restores the git state.
        let restored = tri_state_from_ledger(&entries[1]).unwrap();
        assert!(restored.git_state.is_some());
        assert_eq!(restored.git_state.unwrap().actual_commit_sha, actual);
    }
}
