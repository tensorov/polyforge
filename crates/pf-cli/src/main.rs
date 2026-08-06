//! pf-cli — PolyForge command-line interface.
//!
//! Subcommands:
//!   pf init                 create the ledger at `.omo/ledger.jsonl` if missing (idempotent)
//!   pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]
//!                           append an evidence entry to the ledger
//!   pf ledger tail          print the last entry's hash (ChainState.head_hash)
//!   pf gate <task_id> [--required verified,validated]
//!                           run evaluate_complete; on PASS write a reproducible bundle
//!
//! Ledger path: default `.omo/ledger.jsonl`, overridable via `PF_LEDGER`.
//! Evidence dir: default `.omo/evidence/`, overridable via `PF_EVIDENCE_DIR`.
//!
//! Tri-state honesty: `pf append tool_attestation` and `pf append validation`
//! do NOT fabricate state. They locate the latest eligible entry for the task
//! (ModelClaimed for an attestation, Verified for a validation) and promote it
//! through `pf_core::evidence::promote` — the single gatekeeper enforcing the
//! claim -> verified -> validated chain. A bare attestation with no prior claim
//! is rejected.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use pf_core::evidence::{promote, EvidenceEntry, EvidenceState};
use pf_core::gate::{evaluate_complete, Evaluation, GateError};
use pf_core::ledger::{EvidenceEntry as LedgerEntry, Ledger};

const DEFAULT_LEDGER: &str = ".omo/ledger.jsonl";
const DEFAULT_EVIDENCE_DIR: &str = ".omo/evidence/";

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
        "validation" => Ok("validation"),
        other => Err(format!("unknown evidence kind: {other}")),
    }
}

fn cmd_append(
    kind: &str,
    payload: &str,
    task_id: &str,
    commit: Option<&str>,
    diff: Option<&str>,
) -> Result<(), String> {
    parse_kind(kind)?;
    let mut ledger = Ledger::new(&ledger_path());

    let entry = match kind {
        "model_claim" => {
            let commit_sha = commit.unwrap_or("none");
            let diff_hash = diff.unwrap_or("none");
            // The CLI operator supplies the claim datum (payload) as the opaque
            // caller-supplied `ts` field. It is DATA only — never executed.
            EvidenceEntry::new_claim(task_id, commit_sha, diff_hash, payload)
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
                "pf-cli-1.95.0",
                env::var("PF_ENV_FINGERPRINT").unwrap_or_else(|_| "cli".to_string()),
                payload,
                0,
                "none",
                payload,
            );
            promote(&claim, &attestation).map_err(|e| format!("promotion rejected: {e:?}"))?
        }
        "validation" => {
            let verified = latest_state_of_state(&ledger, task_id, "Verified")?
                .ok_or_else(|| format!("no Verified entry for task {task_id} to validate"))?;
            let validation = EvidenceEntry::validation(
                task_id,
                &verified.commit_sha,
                &verified.diff_hash,
                "pf-cli-operator",
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
            }
        }))
}

fn cmd_ledger_tail() -> Result<(), String> {
    let ledger = Ledger::new(&ledger_path());
    let state = ledger
        .verify_chain()
        .map_err(|e| format!("verify chain: {e:?}"))?;
    println!("{}", state.head_hash);
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
    let ledger = Ledger::new(&ledger_path());
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
    let ledger = Ledger::new(&ledger_path());
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
                return Err("usage: pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]".to_string());
            }
            let kind = args[1].clone();
            let payload = args[2].clone();
            let mut task_id = "default".to_string();
            let mut commit = None;
            let mut diff = None;
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
            )?;
            Ok(ExitCode::SUCCESS)
        }
        "ledger" => {
            if args.len() < 2 || args[1] != "tail" {
                return Err("usage: pf ledger tail".to_string());
            }
            cmd_ledger_tail()?;
            Ok(ExitCode::SUCCESS)
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
         \x20 pf ledger tail\n\
         \x20 pf gate <task_id> [--required verified,validated]\n\
         \n\
         env:\n\
         \x20 PF_LEDGER        ledger path (default .omo/ledger.jsonl)\n\
         \x20 PF_EVIDENCE_DIR  evidence dir (default .omo/evidence/)"
    );
}
