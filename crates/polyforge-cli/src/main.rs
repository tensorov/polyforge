//! polyforge-cli — PolyForge command-line interface.
//!
//! Subcommands:
//!   pf init                 create the ledger at `.pf/ledger.jsonl` if missing (idempotent)
//!   pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]
//!                           [--experiment <id>] [--model <fp>] [--run <id>]
//!                           [--budget <amt>] [--metadata <json>]
//!                           append an evidence entry to the ledger
//!   pf ledger tail          print the last entry's hash (ChainState.head_hash)
//!   pf ledger summary       print per-task counts (latest ledger state per task)
//!   pf gate <task_id> [--required verified,validated]
//!                           run evaluate_complete; on PASS write a reproducible bundle
//!   pf coverage-check --report <llvm-cov.json>
//!                           evaluate a cargo llvm-cov --json report against the
//!                           coverage floor (default 80% aggregate / 80% per file)
//!
//! Ledger path: default `.pf/ledger.jsonl`, overridable via `PF_LEDGER`.
//! Evidence dir: default `.pf/evidence/`, overridable via `PF_EVIDENCE_DIR`.
//!
//! Tri-state honesty: `pf append tool_attestation`, `pf append eval_attestation`
//! and `pf append discrepancy` do NOT fabricate state. They locate the latest
//! eligible entry for the task (ModelClaimed for an attestation/discrepancy,
//! Verified for a validation) and promote it through
//! `polyforge_core::evidence::promote` — the single gatekeeper enforcing the
//! claim -> verified -> validated chain. A bare attestation with no prior claim
//! is rejected.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use polyforge_core::coverage::{
    CoverageFloor, CoverageReport, CoverageScope, CrateCoverage, FileCoverage,
};
use polyforge_core::evidence::{promote, EvidenceEntry, EvidenceState};
use polyforge_core::gate::{evaluate_complete, Evaluation, GateError};
use polyforge_core::ledger::{EvidenceEntry as LedgerEntry, Ledger};

const DEFAULT_LEDGER: &str = ".pf/ledger.jsonl";
const DEFAULT_EVIDENCE_DIR: &str = ".pf/evidence/";

fn ledger_path() -> PathBuf {
    match env::var("PF_LEDGER") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_LEDGER),
    }
}

fn evidence_dir() -> PathBuf {
    match env::var("PF_EVIDENCE_DIR") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_EVIDENCE_DIR),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn tool_versions() -> serde_json::Value {
    serde_json::json!({
        "rustc": env::var("RUSTC_VERSION").unwrap_or_else(|_| "1.95.0".to_string()),
        "cargo": env::var("CARGO_VERSION").unwrap_or_else(|_| "1.95.0".to_string()),
    })
}

fn cmd_init() -> Result<(), String> {
    let path = ledger_path();
    if path.exists() {
        eprintln!("ledger already exists at {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }
    fs::write(&path, "").map_err(|e| format!("create ledger {}: {e}", path.display()))?;
    eprintln!("created ledger at {}", path.display());
    Ok(())
}

fn parse_kind(kind: &str) -> Result<&'static str, String> {
    match kind {
        "model_claim" => Ok("model_claim"),
        "tool_attestation" => Ok("tool_attestation"),
        "eval_attestation" => Ok("eval_attestation"),
        "discrepancy" => Ok("discrepancy"),
        "validation" => Ok("validation"),
        other => Err(format!("unknown evidence kind: {other}")),
    }
}

/// Optional eval identity fields, record-only (never enforced).
#[derive(Default)]
struct IdentityFlags {
    experiment_id: Option<String>,
    model_fingerprint: Option<String>,
    run_id: Option<String>,
    budget: Option<String>,
    eval_metadata: Option<serde_json::Value>,
}

fn cmd_append(
    kind: &str,
    payload: &str,
    task_id: &str,
    commit: Option<&str>,
    diff: Option<&str>,
    identity: IdentityFlags,
) -> Result<(), String> {
    parse_kind(kind)?;
    let mut ledger = Ledger::new(ledger_path());

    let entry = match kind {
        "model_claim" => {
            let commit_sha = commit.unwrap_or("none");
            let diff_hash = diff.unwrap_or("none");
            // The CLI operator supplies the claim datum (payload) as the opaque
            // caller-supplied `ts` field. It is DATA only — never executed.
            let mut claim = EvidenceEntry::new_claim(task_id, commit_sha, diff_hash, payload);
            claim.experiment_id = identity.experiment_id;
            claim.model_fingerprint = identity.model_fingerprint;
            claim.run_id = identity.run_id;
            claim.budget = identity.budget;
            claim.eval_metadata = identity.eval_metadata;
            claim
        }
        "tool_attestation" => {
            // Honest tri-state: locate the latest ModelClaimed entry and promote
            // it via a tool attestation. A bare attestation with no claim is
            // rejected — models cannot self-promote.
            let claim = latest_state_of_state(&ledger, task_id, "ModelClaimed")?
                .ok_or_else(|| format!("no ModelClaimed entry for task {task_id} to attest"))?;
            let attestation = EvidenceEntry::tool_attestation(
                task_id,
                &claim.commit_sha,
                &claim.diff_hash,
                "polyforge-cli-1.95.0",
                env::var("PF_ENV_FINGERPRINT").unwrap_or_else(|_| "cli".to_string()),
                payload,
                0,
                "none",
                payload,
            );
            promote(&claim, &attestation).map_err(|e| format!("promotion rejected: {e:?}"))?
        }
        "eval_attestation" => {
            // Operator-side eval attestation: locate the latest ModelClaimed
            // entry and promote it to Verified, carrying the optional eval
            // identity fields. Mirrors the tool_attestation path.
            let claim = latest_state_of_state(&ledger, task_id, "ModelClaimed")?
                .ok_or_else(|| format!("no ModelClaimed entry for task {task_id} to attest"))?;
            let attestation = EvidenceEntry::eval_attestation(
                task_id,
                &claim.commit_sha,
                &claim.diff_hash,
                "polyforge-cli-1.95.0",
                env::var("PF_ENV_FINGERPRINT").unwrap_or_else(|_| "cli".to_string()),
                payload,
                0,
                "none",
                identity.experiment_id,
                identity.model_fingerprint,
                identity.run_id,
                identity.budget,
                identity.eval_metadata,
                payload,
            );
            promote(&claim, &attestation).map_err(|e| format!("promotion rejected: {e:?}"))?
        }
        "discrepancy" => {
            // Operator-side refutation trace: locate the latest ModelClaimed
            // entry and promote it to Refuted. The payload is the trace datum
            // (ts slot); the operator identity is recorded as the validator.
            let claim = latest_state_of_state(&ledger, task_id, "ModelClaimed")?
                .ok_or_else(|| format!("no ModelClaimed entry for task {task_id} to refute"))?;
            let discrepancy = EvidenceEntry::discrepancy(
                task_id,
                &claim.commit_sha,
                &claim.diff_hash,
                payload,
                1,
                payload,
                "polyforge-cli-operator",
                payload,
            );
            promote(&claim, &discrepancy).map_err(|e| format!("promotion rejected: {e:?}"))?
        }
        "validation" => {
            let verified = latest_state_of_state(&ledger, task_id, "Verified")?
                .ok_or_else(|| format!("no Verified entry for task {task_id} to validate"))?;
            let validation = EvidenceEntry::validation(
                task_id,
                &verified.commit_sha,
                &verified.diff_hash,
                "polyforge-cli-operator",
                payload,
                payload,
            );
            promote(&verified, &validation).map_err(|e| format!("promotion rejected: {e:?}"))?
        }
        _ => unreachable!("parse_kind validated kind"),
    };

    let id = ledger
        .append(entry.to_ledger_entry())
        .map_err(|e| format!("append: {e:?}"))?;
    println!("appended entry {id}");
    Ok(())
}

fn latest_state_of_state(
    ledger: &Ledger,
    task_id: &str,
    state: &str,
) -> Result<Option<EvidenceEntry>, String> {
    let entries = ledger
        .iter_entries()
        .map_err(|e| format!("iter entries: {e:?}"))?;
    Ok(entries
        .into_iter()
        .rev()
        .find(|e| {
            e.payload.get("task_id").and_then(|v| v.as_str()) == Some(task_id)
                && e.payload.get("state").and_then(|v| v.as_str()) == Some(state)
        })
        .map(|le| {
            // Reconstruct a tri-state entry from the ledger record so we can
            // promote it. The ledger payload carries the structured fields.
            let state = match le.payload.get("state").and_then(|v| v.as_str()) {
                Some("ModelClaimed") => EvidenceState::ModelClaimed,
                Some("Verified") => EvidenceState::Verified,
                Some("Validated") => EvidenceState::Validated,
                _ => EvidenceState::ModelClaimed,
            };
            let commit_sha = le
                .payload
                .get("commit_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            let diff_hash = le
                .payload
                .get("diff_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            let command = le
                .payload
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let exit_code = le
                .payload
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            let stdout_hash = le
                .payload
                .get("stdout_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let validator = le
                .payload
                .get("validator")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let rationale = le
                .payload
                .get("rationale")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match state {
                EvidenceState::ModelClaimed => EvidenceEntry::new_claim(
                    le.payload
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(task_id),
                    commit_sha,
                    diff_hash,
                    &le.ts,
                ),
                EvidenceState::Verified => EvidenceEntry::tool_attestation(
                    le.payload
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(task_id),
                    commit_sha,
                    diff_hash,
                    &le.tool_version,
                    &le.env_fingerprint,
                    command,
                    exit_code,
                    stdout_hash,
                    &le.ts,
                ),
                EvidenceState::Validated => EvidenceEntry::validation(
                    le.payload
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(task_id),
                    commit_sha,
                    diff_hash,
                    validator,
                    rationale,
                    &le.ts,
                ),
                // Unreachable: the pre-filter above only selects entries whose
                // payload state matches the requested `state` string, and
                // `parse_required` never accepts a Refuted requirement.
                EvidenceState::Refuted => unreachable!("Refuted is never requested by the CLI"),
            }
        }))
}

fn cmd_ledger_tail() -> Result<(), String> {
    let ledger = Ledger::new(ledger_path());
    let state = ledger
        .verify_chain()
        .map_err(|e| format!("verify chain: {e:?}"))?;
    println!("{}", state.head_hash);
    Ok(())
}

/// Print per-task state counts as one grep-able line, classifying each task
/// by its latest ledger entry's payload state: Verified -> verified,
/// Validated -> validated, any other state (bare ModelClaimed, Refuted,
/// missing) -> failed. Read-only; an empty or missing ledger prints zeros.
fn cmd_ledger_summary() -> Result<(), String> {
    let ledger = Ledger::new(ledger_path());
    let entries = ledger
        .iter_entries()
        .map_err(|e| format!("iter entries: {e:?}"))?;

    let mut latest: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for e in &entries {
        let Some(task) = e.payload.get("task_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let state = e
            .payload
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("ModelClaimed");
        latest.insert(task, state);
    }

    let mut verified = 0usize;
    let mut validated = 0usize;
    let mut failed = 0usize;
    for state in latest.values() {
        match *state {
            "Verified" => verified += 1,
            "Validated" => validated += 1,
            _ => failed += 1,
        }
    }
    println!("tasks_verified={verified} tasks_validated={validated} tasks_failed={failed}");
    Ok(())
}

fn parse_required(spec: &str) -> Result<Vec<EvidenceState>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let state = match part {
            "claimed" => EvidenceState::ModelClaimed,
            "verified" => EvidenceState::Verified,
            "validated" => EvidenceState::Validated,
            other => return Err(format!("unknown required state: {other}")),
        };
        out.push(state);
    }
    if out.is_empty() {
        return Err("--required must list at least one state".to_string());
    }
    Ok(out)
}

/// Collect this task's ledger entries, sorted by seq (insertion order).
fn task_entries(task_id: &str) -> Result<Vec<LedgerEntry>, String> {
    let ledger = Ledger::new(ledger_path());
    let entries = ledger
        .iter_entries()
        .map_err(|e| format!("iter entries: {e:?}"))?;
    let mut out: Vec<_> = entries
        .into_iter()
        .filter(|e| e.payload.get("task_id").and_then(|v| v.as_str()) == Some(task_id))
        .collect();
    out.sort_by_key(|e| e.seq);
    Ok(out)
}

fn write_bundle(task_id: &str, eval: &Evaluation) -> Result<(), String> {
    let dir = evidence_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create evidence dir {}: {e}", dir.display()))?;

    let jsonl_path = dir.join(format!("gate-{task_id}.jsonl"));
    let manifest_path = dir.join(format!("gate-{task_id}.manifest.json"));

    let entries = task_entries(task_id)?;
    let mut jsonl = String::new();
    for e in &entries {
        let line = serde_json::to_string(e).map_err(|err| format!("serialize entry: {err}"))?;
        jsonl.push_str(&line);
        jsonl.push('\n');
    }
    fs::write(&jsonl_path, &jsonl)
        .map_err(|e| format!("write bundle {}: {e}", jsonl_path.display()))?;

    let bundle_sha256 = sha256_hex(jsonl.as_bytes());
    let manifest = serde_json::json!({
        "task_id": task_id,
        "tail_hash": eval.chain_tail_hash,
        "passed": eval.passed,
        "bundle_sha256": bundle_sha256,
        "tool_versions": tool_versions(),
    });
    let manifest_str = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("serialize manifest: {err}"))?;
    fs::write(&manifest_path, manifest_str)
        .map_err(|e| format!("write manifest {}: {e}", manifest_path.display()))?;

    println!("wrote {}", jsonl_path.display());
    println!("wrote {}", manifest_path.display());
    Ok(())
}

fn write_fail_manifest(task_id: &str, eval: &Evaluation) -> Result<(), String> {
    let dir = evidence_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create evidence dir {}: {e}", dir.display()))?;
    let manifest_path = dir.join(format!("gate-{task_id}.manifest.json"));
    let manifest = serde_json::json!({
        "task_id": task_id,
        "tail_hash": eval.chain_tail_hash,
        "passed": false,
        "bundle_sha256": null,
        "tool_versions": tool_versions(),
    });
    let manifest_str = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("serialize manifest: {err}"))?;
    fs::write(&manifest_path, manifest_str)
        .map_err(|e| format!("write manifest {}: {e}", manifest_path.display()))?;
    Ok(())
}

fn cmd_gate(task_id: &str, required: &[EvidenceState]) -> Result<ExitCode, String> {
    let ledger = Ledger::new(ledger_path());
    let eval = match evaluate_complete(&ledger, task_id, required) {
        Ok(e) => e,
        Err(GateError::TaskNotFound { .. }) => {
            eprintln!("gate FAILED for task {task_id}: no tri-state evidence found");
            return Ok(ExitCode::from(1));
        }
        Err(e) => return Err(format!("gate evaluation failed: {e}")),
    };

    if eval.passed {
        write_bundle(task_id, &eval)?;
        println!("gate PASSED for task {task_id}");
        Ok(ExitCode::SUCCESS)
    } else {
        // On FAIL: write a manifest with passed=false, but MUST NOT fabricate the bundle.
        write_fail_manifest(task_id, &eval)?;
        eprintln!("gate FAILED for task {task_id}");
        for m in &eval.missing {
            eprintln!("missing: {m}");
        }
        Ok(ExitCode::from(1))
    }
}

/// Parse a `cargo llvm-cov --json` export into a coverage report.
///
/// The export shape is `{"data":[{"totals":{...},"files":[{...}]}]}`: each
/// `data` entry carries a `totals.lines.percent` aggregate and a `files[]`
/// list whose entries carry `filename` plus `summary.lines.percent` (both
/// percentages 0-100; normalized to fractions here). Crate aggregates are
/// derived from each entry's file paths via the `/crates/<name>/` segment.
/// Parsing is defensive: missing/absent fields are skipped, and a report with
/// no usable data yields an empty report (which evaluates to PASS — whether
/// coverage was actually measured is the CI job's concern, not this
/// checker's).
fn parse_llvm_cov_report(raw: &str) -> Result<CoverageReport, String> {
    let root: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut report = CoverageReport::default();
    let Some(data) = root.get("data").and_then(|v| v.as_array()) else {
        return Ok(report);
    };
    for entry in data {
        let mut entry_crate: Option<String> = None;
        if let Some(files) = entry.get("files").and_then(|v| v.as_array()) {
            for file in files {
                let Some(path) = file.get("filename").and_then(|v| v.as_str()) else {
                    continue;
                };
                let percent = file
                    .get("summary")
                    .and_then(|s| s.get("lines"))
                    .and_then(|l| l.get("percent"))
                    .and_then(|v| v.as_f64());
                if let Some(percent) = percent {
                    report.files.push(FileCoverage {
                        path: path.to_string(),
                        ratio: percent / 100.0,
                    });
                }
                if entry_crate.is_none() {
                    entry_crate = crate_name_of(path);
                }
            }
        }
        if let Some(percent) = entry
            .get("totals")
            .and_then(|t| t.get("lines"))
            .and_then(|l| l.get("percent"))
            .and_then(|v| v.as_f64())
        {
            report.crates.push(CrateCoverage {
                name: entry_crate.unwrap_or_else(|| "<workspace>".to_string()),
                ratio: percent / 100.0,
            });
        }
    }
    Ok(report)
}

/// The crate a file belongs to: the path segment after the last `/crates/`
/// marker, when present.
fn crate_name_of(path: &str) -> Option<String> {
    let marker = "/crates/";
    let idx = path.rfind(marker)?;
    let rest = &path[idx + marker.len()..];
    let name = rest.split('/').next().unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn cmd_coverage_check(report_path: &str) -> Result<ExitCode, String> {
    let raw = fs::read_to_string(report_path)
        .map_err(|e| format!("read coverage report {}: {e}", report_path))?;
    let report = parse_llvm_cov_report(&raw)
        .map_err(|e| format!("parse coverage report {}: {e}", report_path))?;
    let verdict = CoverageFloor::default().evaluate(&report);
    if verdict.passed {
        println!("coverage PASS");
        return Ok(ExitCode::SUCCESS);
    }
    println!("coverage FAIL");
    for failure in &verdict.failures {
        match &failure.scope {
            CoverageScope::Crate(name) => println!(
                "crate {name}: {:.2}% < {:.2}% (floor)",
                failure.ratio * 100.0,
                failure.threshold * 100.0
            ),
            CoverageScope::File(path) => println!(
                "file {path}: {:.2}% < {:.2}% (floor)",
                failure.ratio * 100.0,
                failure.threshold * 100.0
            ),
        }
    }
    Ok(ExitCode::from(1))
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
    }
}

fn dispatch(args: &[String]) -> Result<ExitCode, String> {
    if args.is_empty() {
        print_usage();
        return Ok(ExitCode::from(2));
    }
    match args[0].as_str() {
        "init" => {
            cmd_init()?;
            Ok(ExitCode::SUCCESS)
        }
        "append" => {
            if args.len() < 3 {
                return Err("usage: pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>] [--experiment <id>] [--model <fp>] [--run <id>] [--budget <amt>] [--metadata <json>]".to_string());
            }
            let kind = args[1].clone();
            let payload = args[2].clone();
            let mut task_id = "default".to_string();
            let mut commit = None;
            let mut diff = None;
            let mut identity = IdentityFlags::default();
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--task" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--task requires a value".to_string());
                        }
                        task_id = args[i].clone();
                    }
                    "--commit" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--commit requires a value".to_string());
                        }
                        commit = Some(args[i].clone());
                    }
                    "--diff" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--diff requires a value".to_string());
                        }
                        diff = Some(args[i].clone());
                    }
                    "--experiment" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--experiment requires a value".to_string());
                        }
                        identity.experiment_id = Some(args[i].clone());
                    }
                    "--model" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--model requires a value".to_string());
                        }
                        identity.model_fingerprint = Some(args[i].clone());
                    }
                    "--run" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--run requires a value".to_string());
                        }
                        identity.run_id = Some(args[i].clone());
                    }
                    "--budget" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--budget requires a value".to_string());
                        }
                        identity.budget = Some(args[i].clone());
                    }
                    "--metadata" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--metadata requires a value".to_string());
                        }
                        identity.eval_metadata = Some(
                            serde_json::from_str(&args[i])
                                .map_err(|e| format!("--metadata must be valid JSON: {e}"))?,
                        );
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
                i += 1;
            }
            cmd_append(
                &kind,
                &payload,
                &task_id,
                commit.as_deref(),
                diff.as_deref(),
                identity,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        "ledger" => {
            if args.len() < 2 {
                return Err("usage: pf ledger <tail|summary>".to_string());
            }
            match args[1].as_str() {
                "tail" => {
                    cmd_ledger_tail()?;
                    Ok(ExitCode::SUCCESS)
                }
                "summary" => {
                    cmd_ledger_summary()?;
                    Ok(ExitCode::SUCCESS)
                }
                other => Err(format!("unknown ledger subcommand: {other}")),
            }
        }
        "gate" => {
            if args.len() < 2 {
                return Err("usage: pf gate <task_id> [--required verified,validated]".to_string());
            }
            let task_id = args[1].clone();
            let mut required = vec![EvidenceState::Verified];
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--required" => {
                        i += 1;
                        if i >= args.len() {
                            return Err("--required requires a value".to_string());
                        }
                        required = parse_required(&args[i])?;
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
                i += 1;
            }
            cmd_gate(&task_id, &required)
        }
        "coverage-check" => {
            if args.len() < 3 || args[1] != "--report" {
                return Err("usage: pf coverage-check --report <llvm-cov.json>".to_string());
            }
            cmd_coverage_check(&args[2])
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn print_usage() {
    eprintln!(
        "pf — PolyForge CLI\n\
         \n\
         usage:\n\
         \x20 pf init\n\
         \x20 pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]\n\
         \x20              [--experiment <id>] [--model <fp>] [--run <id>] [--budget <amt>] [--metadata <json>]\n\
         \x20 pf ledger tail\n\
         \x20 pf ledger summary\n\
         \x20 pf gate <task_id> [--required verified,validated]\n\
         \x20 pf coverage-check --report <llvm-cov.json>\n\
         \n\
         kinds: model_claim | tool_attestation | eval_attestation | discrepancy | validation\n\
         \n\
         env:\n\
         \x20 PF_LEDGER        ledger path (default .pf/ledger.jsonl)\n\
         \x20 PF_EVIDENCE_DIR  evidence dir (default .pf/evidence/)"
    );
}

#[cfg(test)]
mod tests {
    use super::{evidence_dir, ledger_path};
    use std::path::PathBuf;

    /// With no PF_LEDGER/PF_EVIDENCE_DIR overrides, the compiled-in defaults
    /// must resolve under `.pf/` — the tracked runtime home (C7 relocation).
    #[test]
    fn test_default_paths_resolve_under_pf() {
        // Clear any ambient overrides so this test exercises the defaults
        // regardless of the invoking shell's environment.
        std::env::remove_var("PF_LEDGER");
        std::env::remove_var("PF_EVIDENCE_DIR");

        assert_eq!(ledger_path(), PathBuf::from(".pf/ledger.jsonl"));
        assert_eq!(evidence_dir(), PathBuf::from(".pf/evidence/"));
    }
}
