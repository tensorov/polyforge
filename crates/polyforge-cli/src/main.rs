//! polyforge-cli — PolyForge command-line interface.
//!
//! Global flag (before any subcommand):
//!   pf --executor <process|sandbox> <command> [args...]
//!                           select the execution backend used for tool
//!                           attestations; validated BEFORE any command runs,
//!                           so an invalid value can never reach a spawn or a
//!                           ledger write. Default `process` keeps legacy
//!                           behavior byte-identical.
//!
//! Subcommands:
//!   pf init                 create the ledger at `.pf/ledger.jsonl` if missing (idempotent)
//!   pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]
//!                           [--experiment <id>] [--model <fp>] [--run <id>]
//!                           [--budget <amt>] [--metadata <json>]
//!                           append an evidence entry to the ledger
//!   pf ledger tail          print the last entry's hash (ChainState.head_hash)
//!   pf ledger summary       print per-task counts (latest ledger state per task)
//!   pf ledger export --otel [--out <path>]
//!                           export the whole ledger as OTLP/JSON log records
//!                           (default stdout; --out writes the same bytes to a
//!                           file). Fail-closed: a corrupt chain is an error.
//!                           Timestamps are converted only when unambiguous:
//!                           all-digits ts is epoch millis, RFC3339 strict
//!                           subset is parsed by hand; anything else keeps its
//!                           raw string in attributes and omits time fields.
//!   pf gate <task_id> [--required verified,validated] [--commit <sha>] [--diff <hash>]
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
use polyforge_core::gate::{evaluate_complete_scoped, Evaluation, GateError, GateScope};
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

/// Convert a ledger `ts` string into epoch nanoseconds when unambiguous.
///
/// All-digits values are epoch milliseconds; RFC3339 strict-subset strings
/// are parsed by hand; anything else yields None and the caller omits both
/// OTLP time fields entirely.
fn ts_to_nanos(ts: &str) -> Option<String> {
    if ts.bytes().all(|b| b.is_ascii_digit()) {
        let millis = ts.parse::<u64>().ok()?;
        millis.checked_mul(1_000_000).map(|n| n.to_string())
    } else {
        parse_rfc3339_to_nanos(ts)
    }
}

/// Parse a strict RFC3339 subset by hand (no chrono/time dependency):
/// `YYYY-MM-DD[T|t| ]HH:MM:SS[.digits](Z | z | +HH:MM | -HH:MM)`.
///
/// Leap seconds (`:60`) are rejected; fractional digits beyond 9 are
/// truncated, shorter fractions are zero-padded on the right. Returns epoch
/// nanoseconds as a decimal String, or None when unparseable.
fn parse_rfc3339_to_nanos(s: &str) -> Option<String> {
    fn num(b: &[u8], start: usize, end: usize) -> Option<u32> {
        if end > b.len() || start > end {
            return None;
        }
        let mut v: u32 = 0;
        for &c in &b[start..end] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + u32::from(c - b'0');
        }
        Some(v)
    }

    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let year = i64::from(num(b, 0, 4)?);
    let month = num(b, 5, 7)?;
    let day = num(b, 8, 10)?;
    let hour = i64::from(num(b, 11, 13)?);
    let minute = i64::from(num(b, 14, 16)?);
    let second = num(b, 17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let mut pos = 19;
    let mut nanos: u32 = 0;
    if pos < b.len() && b[pos] == b'.' {
        pos += 1;
        let frac_start = pos;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
        let ndigits = pos - frac_start;
        if ndigits == 0 {
            return None;
        }
        let take = ndigits.min(9);
        let mut scaled: u32 = 0;
        for &c in &b[frac_start..frac_start + take] {
            scaled = scaled * 10 + u32::from(c - b'0');
        }
        nanos = scaled * 10u32.pow(9 - take as u32);
    }

    // Zone offset: Z | z | +HH:MM | -HH:MM (nothing else).
    let offset_minutes: i64 = if pos < b.len() && (b[pos] == b'Z' || b[pos] == b'z') {
        pos += 1;
        0
    } else if pos + 6 <= b.len() && (b[pos] == b'+' || b[pos] == b'-') && b[pos + 3] == b':' {
        let sign: i64 = if b[pos] == b'+' { 1 } else { -1 };
        let oh = i64::from(num(b, pos + 1, pos + 3)?);
        let om = i64::from(num(b, pos + 4, pos + 6)?);
        if oh > 23 || om > 59 {
            return None;
        }
        pos += 6;
        sign * (oh * 60 + om)
    } else {
        return None;
    };
    if pos != b.len() {
        return None;
    }

    let days = civil_to_days(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + i64::from(second) - offset_minutes * 60;
    let total = secs
        .checked_mul(1_000_000_000)?
        .checked_add(i64::from(nanos))?;
    Some(total.to_string())
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date
/// (Howard Hinnant's days_from_civil algorithm; valid for any year).
fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Build one OTLP/JSON log record for a ledger entry. `timeUnixNano` /
/// `observedTimeUnixNano` carry the same converted value and are omitted
/// entirely when the raw `ts` is unparseable; the raw `ts` always travels
/// verbatim in attributes so no information is lost either way.
fn otlp_record_for(e: &LedgerEntry) -> serde_json::Value {
    let attr = |key: &str, value: String| serde_json::json!({ "key": key, "value": { "stringValue": value } });
    let payload_str = |key: &str| {
        e.payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut rec = serde_json::Map::new();
    if let Some(nanos) = ts_to_nanos(&e.ts) {
        rec.insert(
            "timeUnixNano".to_string(),
            serde_json::Value::String(nanos.clone()),
        );
        rec.insert(
            "observedTimeUnixNano".to_string(),
            serde_json::Value::String(nanos),
        );
    }
    rec.insert(
        "attributes".to_string(),
        serde_json::json!([
            attr("task_id", payload_str("task_id")),
            attr("kind", e.kind.clone()),
            attr("state", payload_str("state")),
            attr("commit_sha", payload_str("commit_sha")),
            attr("diff_hash", payload_str("diff_hash")),
            attr("ts", e.ts.clone()),
            attr("hash", e.hash.clone()),
        ]),
    );
    serde_json::Value::Object(rec)
}

fn build_otlp_export(entries: &[LedgerEntry]) -> serde_json::Value {
    let records: Vec<serde_json::Value> = entries.iter().map(otlp_record_for).collect();
    serde_json::json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": "polyforge" } }
                ]
            },
            "scopeLogs": [{
                "logRecords": records
            }]
        }]
    })
}

/// Fail-closed OTLP/JSON export of the whole ledger.
fn cmd_ledger_export_otel(out_path: Option<&str>) -> Result<(), String> {
    let ledger = Ledger::new(ledger_path());
    ledger
        .verify_chain()
        .map_err(|e| format!("verify chain: {e:?}"))?;
    let entries = ledger
        .iter_entries()
        .map_err(|e| format!("iter entries: {e:?}"))?;
    let doc = build_otlp_export(&entries);
    let bytes = serde_json::to_vec(&doc).map_err(|e| format!("serialize export: {e}"))?;
    match out_path {
        Some(path) => fs::write(path, &bytes).map_err(|e| format!("write export {path}: {e}"))?,
        None => {
            use std::io::Write as _;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| format!("write stdout: {e}"))?;
        }
    }
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

fn cmd_gate(
    task_id: &str,
    required: &[EvidenceState],
    scope: GateScope,
) -> Result<ExitCode, String> {
    let ledger = Ledger::new(ledger_path());
    let eval = match evaluate_complete_scoped(&ledger, task_id, required, scope) {
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
    let args = match apply_executor_flag(args) {
        Ok(rest) => rest,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(2);
        }
    };
    match dispatch(&args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
    }
}

/// Global executor selection: `pf --executor <process|sandbox> <command> ...`.
/// The flag is only honored as the FIRST argument and is validated + applied
/// before dispatch, so an invalid value exits 2 before any spawn or ledger
/// write. Absent flag returns the arguments untouched (legacy behavior).
fn apply_executor_flag(mut args: Vec<String>) -> Result<Vec<String>, String> {
    if args.first().map(String::as_str) != Some("--executor") {
        return Ok(args);
    }
    if args.len() < 2 {
        return Err(EXECUTOR_USAGE.to_string());
    }
    let kind = parse_executor_kind(&args[1])?;
    polyforge_toolrunner::init_executor(kind)?;
    args.drain(..2);
    Ok(args)
}

const EXECUTOR_USAGE: &str = "usage: pf [--executor <process|sandbox>] <command> [args...]";

fn parse_executor_kind(name: &str) -> Result<polyforge_toolrunner::ExecutorKind, String> {
    match name {
        "process" => Ok(polyforge_toolrunner::ExecutorKind::Process),
        "sandbox" => Ok(polyforge_toolrunner::ExecutorKind::Sandbox),
        other => Err(format!("{EXECUTOR_USAGE}; unknown executor: {other}")),
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
            let mut iter = args.iter().skip(3);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--task" => {
                        task_id = iter
                            .next()
                            .ok_or_else(|| "--task requires a value".to_string())?
                            .clone();
                    }
                    "--commit" => {
                        commit = Some(
                            iter.next()
                                .ok_or_else(|| "--commit requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--diff" => {
                        diff = Some(
                            iter.next()
                                .ok_or_else(|| "--diff requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--experiment" => {
                        identity.experiment_id = Some(
                            iter.next()
                                .ok_or_else(|| "--experiment requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--model" => {
                        identity.model_fingerprint = Some(
                            iter.next()
                                .ok_or_else(|| "--model requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--run" => {
                        identity.run_id = Some(
                            iter.next()
                                .ok_or_else(|| "--run requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--budget" => {
                        identity.budget = Some(
                            iter.next()
                                .ok_or_else(|| "--budget requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--metadata" => {
                        let raw = iter
                            .next()
                            .ok_or_else(|| "--metadata requires a value".to_string())?;
                        identity.eval_metadata = Some(
                            serde_json::from_str(raw)
                                .map_err(|e| format!("--metadata must be valid JSON: {e}"))?,
                        );
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
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
                return Err("usage: pf ledger <tail|summary|export>".to_string());
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
                "export" => {
                    if args.len() < 3 || args[2] != "--otel" {
                        return Err("usage: pf ledger export --otel [--out <path>]".to_string());
                    }
                    let mut out = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--out" => {
                                out = Some(
                                    args.get(i + 1)
                                        .ok_or_else(|| "--out requires a value".to_string())?
                                        .clone(),
                                );
                                i += 2;
                            }
                            other => {
                                return Err(format!("unknown flag for ledger export: {other}"))
                            }
                        }
                    }
                    cmd_ledger_export_otel(out.as_deref())?;
                    Ok(ExitCode::SUCCESS)
                }
                other => Err(format!("unknown ledger subcommand: {other}")),
            }
        }
        "gate" => {
            if args.len() < 2 {
                return Err("usage: pf gate <task_id> [--required verified,validated] [--commit <sha>] [--diff <hash>]".to_string());
            }
            let task_id = args[1].clone();
            let mut required = vec![EvidenceState::Verified];
            let mut commit = None;
            let mut diff = None;
            let mut iter = args.iter().skip(2);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--required" => {
                        required = parse_required(
                            iter.next()
                                .ok_or_else(|| "--required requires a value".to_string())?,
                        )?;
                    }
                    "--commit" => {
                        commit = Some(
                            iter.next()
                                .ok_or_else(|| "--commit requires a value".to_string())?
                                .clone(),
                        );
                    }
                    "--diff" => {
                        diff = Some(
                            iter.next()
                                .ok_or_else(|| "--diff requires a value".to_string())?
                                .clone(),
                        );
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
            }
            let scope = match (commit, diff) {
                (Some(commit_sha), Some(diff_hash)) => GateScope::Keyed {
                    commit_sha,
                    diff_hash,
                },
                (None, None) => GateScope::LatestClaim,
                _ => return Err("--commit and --diff must be provided together".to_string()),
            };
            cmd_gate(&task_id, &required, scope)
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
         \x20 pf [--executor <process|sandbox>] <command> [args...]\n\
         \x20 pf init\n\
         \x20 pf append <kind> <payload> [--task <id>] [--commit <sha>] [--diff <hash>]\n\
         \x20              [--experiment <id>] [--model <fp>] [--run <id>] [--budget <amt>] [--metadata <json>]\n\
         \x20 pf ledger tail\n\
         \x20 pf ledger summary\n\
         \x20 pf ledger export --otel [--out <path>]\n\
         \x20 pf gate <task_id> [--required verified,validated] [--commit <sha>] [--diff <hash>]\n\
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
    use super::{evidence_dir, latest_state_of_state, ledger_path, parse_kind, tool_versions};
    use polyforge_core::evidence::{promote, EvidenceEntry, EvidenceState};
    use polyforge_core::ledger::Ledger;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_ledger_path() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "polyforge-cli-unit-{}-{n}.jsonl",
            std::process::id()
        ))
    }

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

    /// Mutant :70:5 (tool_versions -> Default).
    #[test]
    fn test_tool_versions_reports_rustc_and_cargo() {
        let versions = tool_versions();
        assert!(
            versions.get("rustc").is_some(),
            "tool_versions must include rustc, got {versions}"
        );
        assert!(
            versions.get("cargo").is_some(),
            "tool_versions must include cargo, got {versions}"
        );
    }

    /// Mutant :91:5 (parse_kind -> Ok("") / Ok("xyzzy")).
    #[test]
    fn test_parse_kind_accepts_all_kinds_and_rejects_unknown() {
        for kind in [
            "model_claim",
            "tool_attestation",
            "eval_attestation",
            "discrepancy",
            "validation",
        ] {
            assert_eq!(parse_kind(kind), Ok(kind), "parse_kind must accept {kind}");
        }
        assert!(
            parse_kind("bogus").is_err(),
            "parse_kind must reject unknown kinds"
        );
    }

    /// Mutant :233:17 (&& -> ||).
    #[test]
    fn test_latest_state_of_state_matches_task_and_state() {
        let path = temp_ledger_path();
        let mut ledger = Ledger::new(&path);
        ledger
            .append(EvidenceEntry::new_claim("A", "c1", "d1", "p1").to_ledger_entry())
            .expect("append claim A");
        ledger
            .append(EvidenceEntry::new_claim("B", "c2", "d2", "p2").to_ledger_entry())
            .expect("append claim B");

        let found = latest_state_of_state(&ledger, "A", "ModelClaimed")
            .expect("query must succeed")
            .expect("task A must have a ModelClaimed entry");
        assert_eq!(found.task_id, "A", "must return task A's entry, not B's");
        assert_eq!(found.state, EvidenceState::ModelClaimed);

        let _ = std::fs::remove_file(&path);
    }

    /// Mutant :241:17 (delete the Validated arm).
    #[test]
    fn test_latest_state_of_state_reconstructs_validated() {
        let path = temp_ledger_path();
        let mut ledger = Ledger::new(&path);

        let claim = EvidenceEntry::new_claim("V", "c1", "d1", "p1");
        let attestation = EvidenceEntry::tool_attestation(
            "V",
            "c1",
            "d1",
            "polyforge-cli-1.95.0",
            "cli",
            "ran",
            0,
            "none",
            "ran",
        );
        let verified = promote(&claim, &attestation).expect("claim -> verified");
        let validation = EvidenceEntry::validation("V", "c1", "d1", "op", "check", "check");
        let validated = promote(&verified, &validation).expect("verified -> validated");
        ledger
            .append(validated.to_ledger_entry())
            .expect("append validated entry");

        let found = latest_state_of_state(&ledger, "V", "Validated")
            .expect("query must succeed")
            .expect("task V must have a Validated entry");
        assert_eq!(
            found.state,
            EvidenceState::Validated,
            "state must be Validated"
        );

        let _ = std::fs::remove_file(&path);
    }

    use super::{build_otlp_export, dispatch, parse_rfc3339_to_nanos, ts_to_nanos, LedgerEntry};

    #[test]
    fn test_parse_rfc3339_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("1970-01-01T00:00:00Z", Some("0")),
            ("2026-08-22T11:38:29Z", Some("1787398709000000000")),
            ("2026-08-22T13:38:29+02:00", Some("1787398709000000000")),
            ("2026-08-22t11:38:29z", Some("1787398709000000000")),
            ("2026-08-22 11:38:29Z", Some("1787398709000000000")),
            (
                "2026-08-22T11:38:29.1234567891Z",
                Some("1787398709123456789"),
            ),
            ("2026-08-22T11:38:29.5Z", Some("1787398709500000000")),
            ("2026-08-22T11:38:60Z", None),
            ("not-a-timestamp", None),
            ("2026-13-22T11:38:29Z", None),
            ("2026-08-32T11:38:29Z", None),
            ("2026-08-22T11:38:29", None),
            ("2026-08-22T11:38:29Zx", None),
            ("2026-08-22T24:00:00Z", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_rfc3339_to_nanos(input).as_deref(),
                *expected,
                "parse_rfc3339_to_nanos({input:?}) mismatch"
            );
        }
    }

    #[test]
    fn test_ts_to_nanos_table() {
        assert_eq!(
            ts_to_nanos("1755849509000").as_deref(),
            Some("1755849509000000000"),
            "all-digits ts is epoch millis"
        );
        assert_eq!(ts_to_nanos("99999999999999999999999"), None);
        assert_eq!(ts_to_nanos("garbage!!"), None);
        assert_eq!(ts_to_nanos(""), None);
    }

    #[test]
    fn test_build_otlp_export_shape_and_ts_policy() {
        let mk = |kind: &str, task: &str, state: &str, ts: &str| LedgerEntry {
            seq: 0,
            prev_hash: String::new(),
            kind: kind.to_string(),
            payload: serde_json::json!({
                "task_id": task,
                "state": state,
                "commit_sha": "c",
                "diff_hash": "d"
            }),
            tool_version: String::new(),
            env_fingerprint: String::new(),
            ts: ts.to_string(),
            hash: "ab".repeat(32),
            hash_version: 2,
        };
        let entries = vec![
            mk("model_claim", "A", "ModelClaimed", "1755849509000"),
            mk("validation", "B", "Validated", "weird-ts"),
        ];
        let doc = build_otlp_export(&entries);

        let rl = doc
            .get("resourceLogs")
            .and_then(|v| v.as_array())
            .expect("resourceLogs array");
        assert_eq!(rl.len(), 1);
        assert_eq!(
            rl[0]
                .pointer("/resource/attributes/0/key")
                .and_then(|v| v.as_str()),
            Some("service.name")
        );
        assert_eq!(
            rl[0]
                .pointer("/resource/attributes/0/value/stringValue")
                .and_then(|v| v.as_str()),
            Some("polyforge")
        );

        let recs = rl[0]
            .pointer("/scopeLogs/0/logRecords")
            .and_then(|v| v.as_array())
            .expect("logRecords array");
        assert_eq!(recs.len(), 2);

        assert_eq!(
            recs[0].get("timeUnixNano").and_then(|v| v.as_str()),
            Some("1755849509000000000")
        );
        assert_eq!(
            recs[0].get("observedTimeUnixNano").and_then(|v| v.as_str()),
            Some("1755849509000000000")
        );

        assert!(recs[1].get("timeUnixNano").is_none());
        assert!(recs[1].get("observedTimeUnixNano").is_none());

        let attrs = recs[1]
            .get("attributes")
            .and_then(|v| v.as_array())
            .expect("attributes array");
        let keys: Vec<_> = attrs
            .iter()
            .filter_map(|a| a.get("key").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                "task_id",
                "kind",
                "state",
                "commit_sha",
                "diff_hash",
                "ts",
                "hash"
            ]
        );
        let find = |k: &str| {
            attrs
                .iter()
                .find(|a| a.get("key").and_then(|v| v.as_str()) == Some(k))
                .and_then(|a| a.pointer("/value/stringValue"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        assert_eq!(find("task_id").as_deref(), Some("B"));
        assert_eq!(find("kind").as_deref(), Some("validation"));
        assert_eq!(find("state").as_deref(), Some("Validated"));
        assert_eq!(find("commit_sha").as_deref(), Some("c"));
        assert_eq!(find("diff_hash").as_deref(), Some("d"));
        assert_eq!(find("ts").as_deref(), Some("weird-ts"));
        assert_eq!(find("hash").as_deref().map(str::len), Some(64));
    }

    // ---- RFC3339 parser + dispatch mutation guards (2026-08-22 wave) ----

    /// Boundary guards for parse_rfc3339_to_nanos: field range checks (:429),
    /// fraction-digit loop bound (:438), zone-sign and offset arithmetic
    /// (:458-:466), and civil_to_days year-0000 era math (:486). Each row
    /// pins exact nanos or an explicit rejection.
    #[test]
    fn test_parse_rfc3339_boundary_guards() {
        let cases: &[(&str, Option<&str>)] = &[
            // hour 23 stays valid (:429:13 >= mutant would reject it)
            ("2026-08-22T23:14:15Z", Some("1787440455000000000")),
            // minute 59 + second 59 stay valid (:429:28/:429:43 >= mutants)
            ("2026-08-22T12:59:59Z", Some("1787403599000000000")),
            // minute 60 rejected (:429:28 == mutant would accept it)
            ("2026-08-22T12:60:00Z", None),
            // fraction digits running to end of input: no zone suffix, so
            // the original rejects; the :438 <= loop-bound mutant reads
            // b[len] out of bounds first and panics
            ("2026-08-22T12:34:56.123", None),
            // positive offset with nonzero minutes (:466 + -> - mutant folds
            // 5h30m into 4h30m and shifts every value)
            ("2026-08-22T12:34:56+05:30", Some("1787382296000000000")),
            // negative offset keeps its sign (:459 deleted -1 arm flips it)
            ("2026-08-22T12:34:56-05:00", Some("1787420096000000000")),
            // fraction before a numeric offset (:458 pos+3 -> pos-3 mutant
            // probes the wrong colon and rejects the whole timestamp)
            ("2026-08-22T12:34:56.5+05:30", Some("1787382296500000000")),
            // offset-hour bounds: 23 ok, 24 not (:462 ==/>=/||->&& mutants)
            ("2026-08-22T00:00:00+23:00", Some("1787274000000000000")),
            ("2026-08-22T00:00:00+24:00", None),
            // offset-minute bounds: 59 ok, 60 not (:462 ==/>= mutants)
            ("2026-08-22T00:00:00+00:59", Some("1787353260000000000")),
            ("2026-08-22T00:00:00+00:60", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_rfc3339_to_nanos(input).as_deref(),
                *expected,
                "parse_rfc3339_to_nanos({input:?}) mismatch"
            );
        }
    }

    /// Separator checks (:414 ||-> && mutants): exactly one wrong separator
    /// must reject; each mutated conjunct chain would fall through and parse
    /// the rest as if the layout were intact.
    #[test]
    fn test_parse_rfc3339_rejects_each_bad_separator() {
        for input in [
            "2026X08-22T12:34:56Z", // b[4] != '-'
            "2026-08X22T12:34:56Z", // b[7] != '-'
            "2026-08-22T12:34X56Z", // b[16] != ':'
        ] {
            assert!(
                parse_rfc3339_to_nanos(input).is_none(),
                "{input:?} with one bad separator must be rejected"
            );
        }
    }

    /// Short-input floor (:411 < -> == mutant): inputs shorter than the
    /// mandatory 19 bytes must reject cleanly, never index past the buffer.
    #[test]
    fn test_parse_rfc3339_rejects_short_input() {
        assert_eq!(parse_rfc3339_to_nanos("2026-08-2"), None);
        assert_eq!(parse_rfc3339_to_nanos("2026"), None);
    }

    /// civil_to_days era branch for y < 0 (:486 - -> + / - -> / mutants):
    /// only year 0000 January/February reach a negative y through the
    /// m <= 2 adjustment, and every such timestamp overflows i64 nanos in
    /// the final checked_mul, so all variants observably agree on None.
    /// This pin doubles as a tripwire: widening the nanos type would make
    /// the era branch observable and require real coverage here.
    #[test]
    fn test_parse_rfc3339_year_zero_overflows_to_none() {
        assert_eq!(parse_rfc3339_to_nanos("0000-02-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_to_nanos("0000-01-01T00:00:00Z"), None);
    }

    /// dispatch ledger-export usage guards (:906 ||-> && mutant): a missing
    /// --otel argument and a non-otel third argument must both yield the
    /// usage error, never fall through to the exporter.
    #[test]
    fn test_dispatch_ledger_export_usage_errors() {
        let err = dispatch(&["ledger".into(), "export".into()]).unwrap_err();
        assert_eq!(err, "usage: pf ledger export --otel [--out <path>]");
        let err = dispatch(&["ledger".into(), "export".into(), "--json".into()]).unwrap_err();
        assert_eq!(err, "usage: pf ledger export --otel [--out <path>]");
    }

    /// dispatch --out loop step (:919 += -> *= mutant): i *= 2 jumps from 3
    /// to 6, silently skipping the trailing unknown flag that must error.
    #[test]
    fn test_dispatch_ledger_export_out_flag_consumes_value() {
        let err = dispatch(&[
            "ledger".to_string(),
            "export".to_string(),
            "--otel".to_string(),
            "--out".to_string(),
            "/tmp/pf-mt-guard-export.jsonl".to_string(),
            "extra".to_string(),
        ])
        .unwrap_err();
        assert_eq!(err, "unknown flag for ledger export: extra");
    }

    // ---- T9 --executor flag plumbing ----

    use super::{apply_executor_flag, parse_executor_kind, EXECUTOR_USAGE};

    #[test]
    fn executor_kind_table() {
        assert!(matches!(
            parse_executor_kind("process"),
            Ok(polyforge_toolrunner::ExecutorKind::Process)
        ));
        assert!(matches!(
            parse_executor_kind("sandbox"),
            Ok(polyforge_toolrunner::ExecutorKind::Sandbox)
        ));
        let err = parse_executor_kind("bogus").unwrap_err();
        assert_eq!(err, format!("{EXECUTOR_USAGE}; unknown executor: bogus"));
    }

    #[test]
    fn apply_executor_flag_absent_leaves_args_untouched() {
        let args = vec!["gate".to_string(), "demo".to_string()];
        let rest = apply_executor_flag(args.clone()).expect("passthrough");
        assert_eq!(rest, args, "no flag means byte-identical legacy dispatch");
    }

    #[test]
    fn apply_executor_flag_process_consumes_pair_and_dispatches_rest() {
        let rest = apply_executor_flag(vec![
            "--executor".to_string(),
            "process".to_string(),
            "ledger".to_string(),
            "tail".to_string(),
        ])
        .expect("process selection");
        assert_eq!(rest, vec!["ledger".to_string(), "tail".to_string()]);
    }

    #[test]
    fn apply_executor_flag_unknown_value_is_usage_error() {
        let err = apply_executor_flag(vec![
            "--executor".to_string(),
            "firecracker".to_string(),
            "init".to_string(),
        ])
        .unwrap_err();
        assert!(
            err.contains(EXECUTOR_USAGE) && err.contains("unknown executor: firecracker"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn apply_executor_flag_missing_value_is_usage_error() {
        let err = apply_executor_flag(vec!["--executor".to_string()]).unwrap_err();
        assert_eq!(err, EXECUTOR_USAGE);
    }

    /// Feature-gate failure mode through the CLI surface: `sandbox` without
    /// the `sandbox-mock` feature must fail with the exact gate message and
    /// never reach dispatch (so no command, spawn, or ledger write happens).
    #[test]
    fn apply_executor_flag_sandbox_requires_feature() {
        match apply_executor_flag(vec![
            "--executor".to_string(),
            "sandbox".to_string(),
            "init".to_string(),
        ]) {
            Err(msg) => {
                assert_eq!(msg, "sandbox executor requires feature sandbox-mock");
            }
            Ok(rest) => {
                // Feature-on build (T10): validation passed, so dispatch must
                // receive the remaining arguments untouched.
                assert_eq!(rest, vec!["init".to_string()]);
            }
        }
    }

    /// The flag is a global FIRST-argument option only; after a subcommand it
    /// falls through to that subcommand's own unknown-flag rejection.
    #[test]
    fn apply_executor_flag_after_subcommand_is_not_global() {
        let args = vec![
            "gate".to_string(),
            "--executor".to_string(),
            "sandbox".to_string(),
        ];
        let rest = apply_executor_flag(args.clone()).expect("passthrough");
        assert_eq!(rest, args);
    }
}
