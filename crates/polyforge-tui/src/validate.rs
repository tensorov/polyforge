//! Operator validation engine: promotes `Verified` -> `Validated` through the
//! single [`promote`] gatekeeper in polyforge-core. Every append re-reads the
//! ledger fresh so a stale promotion is never written.
//!
//! The engine is deliberately dumb about UI: it takes a ledger path, task ids,
//! and a rationale string, and reports per-task outcomes. The TUI layer (T8b)
//! turns outcomes into toasts; nothing here touches app state.

use std::collections::HashMap;
use std::path::Path;

use polyforge_core::evidence::{promote, EvidenceEntry as TriStateEvidence, EvidenceState};
use polyforge_core::ledger::{EvidenceEntry as LedgerEntry, Ledger};
use polyforge_core::GateError;

/// Validator identity recorded on every entry this engine appends.
pub const VALIDATOR: &str = "lazyforge-operator";

/// Why a task was skipped instead of validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The task's latest entry is still `ModelClaimed`.
    NeedsAttestation,
    /// The task's latest entry is already `Validated`.
    AlreadyValidated,
    /// The task's latest entry is `Refuted`; validation is rejected.
    Refuted,
    /// The ledger holds no entries for the task at all.
    NoEntries,
    /// The latest `Verified` identity changed between the batch snapshot and
    /// the per-task re-read: what the operator saw is no longer what is there.
    StaleState,
}

impl SkipReason {
    /// Operator-facing one-liner (plan UX wording).
    pub fn message(self) -> &'static str {
        match self {
            Self::NeedsAttestation => "needs tool attestation first",
            Self::AlreadyValidated => "already validated",
            Self::Refuted => "refuted - validation rejected",
            Self::NoEntries => "no entries for task",
            Self::StaleState => "state changed under you",
        }
    }
}

/// Result of [`validate_single`]: either an appended promotion or a skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleOutcome {
    /// `true` iff a `Validation` entry was appended to the ledger.
    pub appended: bool,
    /// Set when the task was not validated, with the reason.
    pub skip: Option<SkipReason>,
}

impl SingleOutcome {
    fn skipped(reason: SkipReason) -> Self {
        Self {
            appended: false,
            skip: Some(reason),
        }
    }

    fn appended_now() -> Self {
        Self {
            appended: true,
            skip: None,
        }
    }
}

/// Aggregate result of [`validate_bulk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkReport {
    /// Number of tasks whose promotion was appended.
    pub appended: usize,
    /// `(task_id, reason message)` for every skipped task, in input order.
    pub skipped: Vec<(String, String)>,
}

impl BulkReport {
    fn skip(&mut self, task_id: &str, reason: SkipReason) {
        self.skipped
            .push((task_id.to_string(), reason.message().to_string()));
    }
}

/// Wall-clock timestamp as an epoch-millis string (the MCP `ts` convention).
///
/// A clock running backwards before the epoch degrades to `"0"` rather than
/// panicking; the ledger treats `ts` as a caller-supplied datum.
pub fn now_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

/// Classification of a task's highest-seq ledger entry after a fresh read.
///
/// The `Verified` variant carries the reconstructed tri-state entry so the
/// caller can promote it without a second scan; every other variant makes the
/// "why not" explicit and exhaustive.
// The size gap is deliberate: the enum is stack-local and consumed within one
// function call, so boxing Verified would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum TaskLatest {
    NoEntries,
    Claimed,
    Verified(TriStateEvidence),
    Validated,
    Refuted,
}

/// Map a payload state string onto the tri-state verdict.
fn parse_state(value: &str) -> Option<EvidenceState> {
    match value {
        "ModelClaimed" => Some(EvidenceState::ModelClaimed),
        "Verified" => Some(EvidenceState::Verified),
        "Validated" => Some(EvidenceState::Validated),
        "Refuted" => Some(EvidenceState::Refuted),
        _ => None,
    }
}

/// Rebuild the tri-state `Verified` entry behind a ledger entry.
///
/// Only `task_id`, `commit_sha`, and `diff_hash` feed the subsequent
/// `Verified -> Validated` promotion; the tool fields are carried along for
/// fidelity but never influence what gets written. A ledger entry missing an
/// identity field fails closed - it cannot be pinned, so it must not be
/// promoted.
fn verified_from_ledger(entry: &LedgerEntry) -> Result<TriStateEvidence, String> {
    let identity = |key: &str| -> Result<String, String> {
        entry
            .payload
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "ledger entry seq {} lacks payload field {key}; refusing to promote",
                    entry.seq
                )
            })
    };
    let task_id = identity("task_id")?;
    let commit_sha = identity("commit_sha")?;
    let diff_hash = identity("diff_hash")?;
    let command = entry
        .payload
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stdout_hash = entry
        .payload
        .get("stdout_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let exit_code = entry.payload.get("exit_code").and_then(|v| v.as_i64());
    Ok(TriStateEvidence::tool_attestation(
        task_id,
        commit_sha,
        diff_hash,
        entry.tool_version.clone(),
        entry.env_fingerprint.clone(),
        command,
        exit_code.unwrap_or(0) as i32,
        stdout_hash,
        entry.ts.clone(),
    ))
}

/// Fresh fail-closed read of `task_id`'s latest ledger verdict.
///
/// Chain integrity is verified first: any tamper/rewind aborts before any
/// classification. The task's highest-seq entry decides the variant; an
/// unrecognized state string fails closed rather than guessing.
fn scan_latest(ledger: &mut Ledger, task_id: &str) -> Result<TaskLatest, String> {
    ledger
        .verify_chain()
        .map_err(|e| format!("{}", GateError::from(e)))?;
    let entries = ledger
        .iter_entries()
        .map_err(|e| format!("{}", GateError::from(e)))?;
    let latest = entries
        .iter()
        .filter(|entry| entry.payload.get("task_id").and_then(|v| v.as_str()) == Some(task_id))
        .max_by_key(|entry| entry.seq);
    let Some(latest) = latest else {
        return Ok(TaskLatest::NoEntries);
    };
    let state = parse_state(
        latest
            .payload
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
    .ok_or_else(|| {
        format!(
            "ledger entry seq {} carries an unrecognized state; refusing to classify",
            latest.seq
        )
    })?;
    match state {
        EvidenceState::ModelClaimed => Ok(TaskLatest::Claimed),
        EvidenceState::Validated => Ok(TaskLatest::Validated),
        EvidenceState::Refuted => Ok(TaskLatest::Refuted),
        EvidenceState::Verified => Ok(TaskLatest::Verified(verified_from_ledger(latest)?)),
    }
}

/// The task's latest entry as a tri-state entry, iff that entry is `Verified`.
///
/// Fail-closed wrapper over [`scan_latest`]: integrity or parse failures are
/// errors; anything else yields `None` and the caller classifies by state.
pub fn latest_verified(
    ledger: &mut Ledger,
    task_id: &str,
) -> Result<Option<TriStateEvidence>, String> {
    match scan_latest(ledger, task_id)? {
        TaskLatest::Verified(entry) => Ok(Some(entry)),
        _ => Ok(None),
    }
}

/// Build the operator `Validation` record and append its promoted form.
///
/// The promoted entry copies the verified entry's identity triple
/// (`task_id` / `commit_sha` / `diff_hash`) and stamps `now_millis()` as its
/// timestamp datum.
fn append_validation(
    ledger: &mut Ledger,
    verified: &TriStateEvidence,
    rationale: &str,
) -> Result<(), String> {
    let validation = TriStateEvidence::validation(
        verified.task_id.clone(),
        verified.commit_sha.clone(),
        verified.diff_hash.clone(),
        VALIDATOR,
        rationale,
        now_millis(),
    );
    let promoted = promote(verified, &validation).map_err(|e| format!("{e}"))?;
    ledger
        .append(promoted.to_ledger_entry())
        .map_err(|e| format!("{}", GateError::from(e)))?;
    Ok(())
}

/// True when the freshly-read `Verified` identity differs from the batch
/// snapshot (`None` snapshot means no earlier belief existed: never stale).
fn identity_changed(snapshot: Option<(&str, &str)>, verified: &TriStateEvidence) -> bool {
    match snapshot {
        None => false,
        Some((commit_sha, diff_hash)) => {
            commit_sha != verified.commit_sha || diff_hash != verified.diff_hash
        }
    }
}

/// Validate one task against the ledger at `ledger_path`.
///
/// Opens the ledger fresh, classifies the task's latest state, and on
/// `Verified` appends the promoted `Validation` entry. Skips carry the reason;
/// integrity failures surface as `Err` and write nothing.
pub fn validate_single(
    ledger_path: &Path,
    task_id: &str,
    rationale: &str,
) -> Result<SingleOutcome, String> {
    let mut ledger = Ledger::new(ledger_path);
    match scan_latest(&mut ledger, task_id)? {
        TaskLatest::NoEntries => Ok(SingleOutcome::skipped(SkipReason::NoEntries)),
        TaskLatest::Claimed => Ok(SingleOutcome::skipped(SkipReason::NeedsAttestation)),
        TaskLatest::Validated => Ok(SingleOutcome::skipped(SkipReason::AlreadyValidated)),
        TaskLatest::Refuted => Ok(SingleOutcome::skipped(SkipReason::Refuted)),
        TaskLatest::Verified(verified) => {
            append_validation(&mut ledger, &verified, rationale)?;
            Ok(SingleOutcome::appended_now())
        }
    }
}

/// Validate many tasks, re-reading the ledger immediately before each append.
///
/// Per task the ledger is reopened fresh and the latest state re-classified:
/// a task whose state moved between the batch snapshot and its turn is
/// skipped with the current reason (never promoted from a stale belief). A
/// task still `Verified` but under a different commit/diff key than the
/// batch-start snapshot is skipped as [`SkipReason::StaleState`]. One bad
/// task never aborts the batch; only integrity/read failures do.
pub fn validate_bulk(
    ledger_path: &Path,
    tasks: &[String],
    rationale: &str,
) -> Result<BulkReport, String> {
    // Batch-start snapshot: each requested task's latest Verified identity,
    // standing in for what the operator saw when they hit "validate all".
    let mut snapshot: HashMap<&str, (String, String)> = HashMap::new();
    {
        let mut ledger = Ledger::new(ledger_path);
        for task_id in tasks {
            if let TaskLatest::Verified(verified) = scan_latest(&mut ledger, task_id)? {
                snapshot.insert(
                    task_id.as_str(),
                    (verified.commit_sha.clone(), verified.diff_hash.clone()),
                );
            }
        }
    }

    let mut report = BulkReport {
        appended: 0,
        skipped: Vec::new(),
    };
    for task_id in tasks {
        // Fresh reopen + immediate re-read right before this task's append.
        let mut ledger = Ledger::new(ledger_path);
        match scan_latest(&mut ledger, task_id)? {
            TaskLatest::NoEntries => report.skip(task_id, SkipReason::NoEntries),
            TaskLatest::Claimed => report.skip(task_id, SkipReason::NeedsAttestation),
            TaskLatest::Validated => report.skip(task_id, SkipReason::AlreadyValidated),
            TaskLatest::Refuted => report.skip(task_id, SkipReason::Refuted),
            TaskLatest::Verified(verified) => {
                let snap = snapshot
                    .get(task_id.as_str())
                    .map(|(commit, diff)| (commit.as_str(), diff.as_str()));
                if identity_changed(snap, &verified) {
                    report.skip(task_id, SkipReason::StaleState);
                } else {
                    append_validation(&mut ledger, &verified, rationale)?;
                    report.appended += 1;
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Pid-suffixed tempdir path so parallel test runs never collide.
    fn tmp_ledger_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pf-tui-validate-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{}-{n}.jsonl", std::process::id()))
    }

    fn claim(task: &str) -> TriStateEvidence {
        TriStateEvidence::new_claim(task, "abc123", "diff-1", "ts-1")
    }

    fn attestation(task: &str) -> TriStateEvidence {
        TriStateEvidence::tool_attestation(
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

    fn discrepancy(task: &str) -> TriStateEvidence {
        TriStateEvidence::discrepancy(
            task,
            "abc123",
            "diff-1",
            "cargo build",
            1,
            "boom",
            "toolrunner:cargo build",
            "ts-4",
        )
    }

    /// Seed claim + tool attestation: the task ends `Verified`.
    fn seed_verified(path: &Path, task: &str) {
        let mut ledger = Ledger::new(path);
        let c = claim(task);
        let v = promote(&c, &attestation(task)).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
    }

    fn entry_count(path: &Path) -> usize {
        Ledger::new(path).iter_entries().unwrap().len()
    }

    fn entries_for(path: &Path, task: &str) -> Vec<LedgerEntry> {
        Ledger::new(path)
            .iter_entries()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.payload.get("task_id").and_then(|v| v.as_str()) == Some(task))
            .collect()
    }

    #[test]
    fn single_happy_path_appends_validation_with_copied_identity() {
        let path = tmp_ledger_path("single-happy");
        seed_verified(&path, "t-ok");
        let before = entry_count(&path);

        let outcome = validate_single(&path, "t-ok", "looks good").unwrap();
        assert_eq!(
            outcome,
            SingleOutcome {
                appended: true,
                skip: None
            }
        );

        let entries = entries_for(&path, "t-ok");
        assert_eq!(entries.len(), before + 1, "exactly one new entry");
        let last = entries.last().unwrap();
        assert_eq!(last.kind, "Validation");
        assert_eq!(last.payload["state"], "Validated");
        // Identity copy: commit/diff equal the Verified entry's pinning.
        assert_eq!(last.payload["commit_sha"], "abc123");
        assert_eq!(last.payload["diff_hash"], "diff-1");
        assert_eq!(last.payload["validator"], VALIDATOR);
        assert_eq!(last.payload["rationale"], "looks good");
        // MCP ts convention: epoch-millis string.
        let ts_millis: u64 = last.ts.parse().expect("ts must be epoch millis digits");
        assert!(ts_millis > 0, "epoch-millis ts must be positive");
        // The chain still verifies with the new entry.
        Ledger::new(&path).verify_chain().unwrap();
    }

    #[test]
    fn single_on_model_claimed_skips_and_writes_nothing() {
        let path = tmp_ledger_path("single-claimed");
        let mut ledger = Ledger::new(&path);
        ledger.append(claim("t-c").to_ledger_entry()).unwrap();
        let before = entry_count(&path);

        let outcome = validate_single(&path, "t-c", "r").unwrap();
        assert!(!outcome.appended);
        assert_eq!(outcome.skip, Some(SkipReason::NeedsAttestation));
        assert_eq!(
            outcome.skip.map(SkipReason::message),
            Some("needs tool attestation first")
        );
        assert_eq!(entry_count(&path), before, "skip must not touch the ledger");
    }

    #[test]
    fn single_on_already_validated_skips() {
        let path = tmp_ledger_path("single-validated");
        seed_verified(&path, "t-v");
        let outcome = validate_single(&path, "t-v", "first pass").unwrap();
        assert!(outcome.appended);
        let before = entry_count(&path);

        let second = validate_single(&path, "t-v", "second pass").unwrap();
        assert!(!second.appended);
        assert_eq!(second.skip, Some(SkipReason::AlreadyValidated));
        assert_eq!(entry_count(&path), before, "no double validation");
    }

    #[test]
    fn single_on_refuted_skips_with_rejection_message() {
        let path = tmp_ledger_path("single-refuted");
        let mut ledger = Ledger::new(&path);
        let c = claim("t-r");
        let d = promote(&c, &discrepancy("t-r")).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();
        let before = entry_count(&path);

        let outcome = validate_single(&path, "t-r", "r").unwrap();
        assert!(!outcome.appended);
        assert_eq!(outcome.skip, Some(SkipReason::Refuted));
        assert_eq!(
            outcome.skip.map(SkipReason::message),
            Some("refuted - validation rejected")
        );
        assert_eq!(entry_count(&path), before);
    }

    #[test]
    fn single_on_unknown_task_skips_no_entries() {
        let path = tmp_ledger_path("single-unknown");
        seed_verified(&path, "t-real");

        let outcome = validate_single(&path, "ghost", "r").unwrap();
        assert!(!outcome.appended);
        assert_eq!(outcome.skip, Some(SkipReason::NoEntries));
        assert_eq!(
            outcome.skip.map(SkipReason::message),
            Some("no entries for task")
        );
    }

    #[test]
    fn bulk_validates_every_verified_task_in_order() {
        let path = tmp_ledger_path("bulk-all");
        for task in ["t1", "t2", "t3"] {
            seed_verified(&path, task);
        }
        let before = entry_count(&path);

        let tasks: Vec<String> = ["t1", "t2", "t3"].iter().map(|s| s.to_string()).collect();
        let report = validate_bulk(&path, &tasks, "batch ok").unwrap();
        assert_eq!(report.appended, 3);
        assert!(
            report.skipped.is_empty(),
            "no skips expected, got {:?}",
            report.skipped
        );

        let entries = Ledger::new(&path).iter_entries().unwrap();
        assert_eq!(entries.len(), before + 3);
        let validations: Vec<&LedgerEntry> =
            entries.iter().filter(|e| e.kind == "Validation").collect();
        assert_eq!(validations.len(), 3);
        for (i, task) in ["t1", "t2", "t3"].iter().enumerate() {
            assert_eq!(validations[i].payload["task_id"], *task);
            assert_eq!(validations[i].payload["state"], "Validated");
        }
        Ledger::new(&path).verify_chain().unwrap();
    }

    #[test]
    fn bulk_never_promotes_from_a_stale_belief() {
        let path = tmp_ledger_path("bulk-stale");
        seed_verified(&path, "t1");
        seed_verified(&path, "t2");
        // The operator's list was built while t2 looked Verified...
        let tasks: Vec<String> = ["t1", "t2"].iter().map(|s| s.to_string()).collect();

        // ...then a concurrent writer lands a NEWER bare claim on t2.
        let mut ledger = Ledger::new(&path);
        ledger
            .append(
                TriStateEvidence::new_claim("t2", "zzz999", "diff-9", "ts-late").to_ledger_entry(),
            )
            .unwrap();

        let report = validate_bulk(&path, &tasks, "r").unwrap();
        assert_eq!(report.appended, 1, "only t1 may be promoted");
        assert_eq!(
            report.skipped,
            vec![("t2".to_string(), "needs tool attestation first".to_string())]
        );

        // t1 got its Validation; t2 has NO Validation entry at all: the stale
        // belief produced no promotion.
        let t1 = entries_for(&path, "t1");
        assert_eq!(t1.last().unwrap().kind, "Validation");
        let t2 = entries_for(&path, "t2");
        assert!(
            t2.iter().all(|e| e.kind != "Validation"),
            "no stale promotion for t2"
        );
        assert_eq!(t2.last().unwrap().payload["state"], "ModelClaimed");
        Ledger::new(&path).verify_chain().unwrap();
    }

    #[test]
    fn bulk_carries_the_caller_rationale_verbatim() {
        let path = tmp_ledger_path("bulk-rationale");
        for task in ["t1", "t2"] {
            seed_verified(&path, task);
        }
        let tasks: Vec<String> = ["t1", "t2"].iter().map(|s| s.to_string()).collect();
        let rationale = "operator reviewed CI logs 42";
        let report = validate_bulk(&path, &tasks, rationale).unwrap();
        assert_eq!(report.appended, 2);
        for task in ["t1", "t2"] {
            let last = entries_for(&path, task).pop().unwrap();
            assert_eq!(last.payload["rationale"], rationale);
        }
    }

    #[test]
    fn bulk_reports_each_skip_reason_across_mixed_tasks() {
        let path = tmp_ledger_path("bulk-mixed");
        seed_verified(&path, "t-good");
        let mut ledger = Ledger::new(&path);
        ledger.append(claim("t-claim").to_ledger_entry()).unwrap();
        let c = claim("t-ref");
        let d = promote(&c, &discrepancy("t-ref")).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();

        let tasks: Vec<String> = ["t-good", "t-claim", "t-ref", "t-ghost"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let report = validate_bulk(&path, &tasks, "r").unwrap();
        assert_eq!(report.appended, 1);
        assert_eq!(
            report.skipped,
            vec![
                (
                    "t-claim".to_string(),
                    "needs tool attestation first".to_string()
                ),
                (
                    "t-ref".to_string(),
                    "refuted - validation rejected".to_string()
                ),
                ("t-ghost".to_string(), "no entries for task".to_string()),
            ]
        );
    }

    #[test]
    fn corrupt_ledger_fails_closed_without_appending() {
        let path = tmp_ledger_path("corrupt-fail-closed");
        seed_verified(&path, "t-x");
        // Byte-level tamper keeps JSON valid but breaks the hash chain.
        let content = std::fs::read_to_string(&path).unwrap();
        let tampered = content.replacen("ModelClaim", "ModelClaimZ", 1);
        std::fs::write(&path, tampered).unwrap();

        let err = validate_single(&path, "t-x", "r").unwrap_err();
        assert!(
            err.contains("integrity"),
            "integrity failure must surface, got: {err}"
        );
        // And the bulk path refuses too.
        let tasks = vec!["t-x".to_string()];
        assert!(validate_bulk(&path, &tasks, "r").is_err());
    }

    #[test]
    fn latest_verified_reflects_the_task_state() {
        let path = tmp_ledger_path("latest-verified");
        seed_verified(&path, "t-v");
        let mut ledger = Ledger::new(&path);
        let verified = latest_verified(&mut ledger, "t-v")
            .unwrap()
            .expect("Verified task must yield its entry");
        assert_eq!(verified.state, EvidenceState::Verified);
        assert_eq!(verified.commit_sha, "abc123");
        assert_eq!(verified.diff_hash, "diff-1");

        ledger.append(claim("t-bare").to_ledger_entry()).unwrap();
        assert_eq!(latest_verified(&mut ledger, "t-bare").unwrap(), None);
        assert_eq!(latest_verified(&mut ledger, "ghost").unwrap(), None);
    }

    #[test]
    fn identity_changed_flags_only_a_key_drift() {
        let same = TriStateEvidence::new_claim("t", "c1", "d1", "ts");
        let drifted = TriStateEvidence::new_claim("t", "c2", "d2", "ts");
        assert!(!identity_changed(Some(("c1", "d1")), &same));
        assert!(identity_changed(Some(("c1", "d1")), &drifted));
        // Commit drift alone counts; so does diff drift alone.
        assert!(identity_changed(
            Some(("c1", "d1")),
            &TriStateEvidence::new_claim("t", "c1", "dX", "ts")
        ));
        assert!(identity_changed(
            Some(("c1", "d1")),
            &TriStateEvidence::new_claim("t", "cX", "d1", "ts")
        ));
        // No snapshot: nothing to be stale against.
        assert!(!identity_changed(None, &drifted));
    }

    #[test]
    fn now_millis_is_epoch_millis_digits() {
        let ts = now_millis();
        let parsed: u64 = ts.parse().expect("epoch-millis string parses");
        // Sanity window: comfortably past 2026-01-01 (~1767225600000 ms).
        assert!(
            parsed > 1_700_000_000_000,
            "plausible epoch millis, got {parsed}"
        );
    }

    #[test]
    fn unreadable_ledger_surfaces_io_error() {
        // Mutant guard: a dropped map_err in scan_latest would turn an I/O
        // failure into a silent Ok(None) classification. The error must
        // propagate out of both entry points.
        let path = tmp_ledger_path("io-error");
        let long = path.with_file_name("z".repeat(300));
        let mut ledger = Ledger::new(&long);
        assert!(latest_verified(&mut ledger, "any").is_err());
        let tasks = vec!["any".to_string()];
        assert!(validate_bulk(&long, &tasks, "r").is_err());
    }
}
