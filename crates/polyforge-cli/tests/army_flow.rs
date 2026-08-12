//! End-to-end "army" harness for `polyforge-cli`: drives the REAL binary
//! (`env!("CARGO_BIN_EXE_polyforge-cli")`) against unique per-test temp ledgers
//! (AtomicU64 counter, distinct `polyforge-army-` prefix so parallel gate.rs
//! tests never collide). Nothing is mocked: the ledger, the tri-state chain
//! and the toolrunner path all run through the actual subprocesses.
//!
//! `test_army_full_loop` covers the complete cycle: init -> model_claim ->
//! tool_attestation (the verify-and-append path that promotes the claim to
//! Verified — there is no separate `pf verify` subcommand) -> `pf ledger
//! tail` -> gate -> manifest tail_hash cross-check -> bundle reproducibility.
//! `test_army_rewind_fails_cycle` tampers one byte of the tail entry's
//! payload and asserts the gate fails with LedgerIntegrity and fabricates
//! no bundle.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-test isolation: unique temp dir for the ledger + evidence dir.
struct Env {
    ledger: PathBuf,
    evidence_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("polyforge-army-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            ledger: dir.join("ledger.jsonl"),
            evidence_dir: dir.join("evidence"),
        }
    }

    fn bundle(&self, task: &str) -> PathBuf {
        self.evidence_dir.join(format!("gate-{task}.jsonl"))
    }

    fn manifest(&self, task: &str) -> PathBuf {
        self.evidence_dir.join(format!("gate-{task}.manifest.json"))
    }
}

fn pf(env: &Env, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(args)
        .env("PF_LEDGER", &env.ledger)
        .env("PF_EVIDENCE_DIR", &env.evidence_dir)
        .output()
        .expect("failed to spawn polyforge-cli binary")
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("no exit code")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_ok(out: &Output, what: &str) {
    assert_eq!(
        exit_code(out),
        0,
        "{what} should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        stderr(out)
    );
}

/// Seed a ledger with an honest claim -> verified chain for `task`.
fn seed_verified(env: &Env, task: &str) {
    assert_ok(&pf(env, &["init"]), "pf init");
    assert_ok(
        &pf(
            env,
            &[
                "append",
                "model_claim",
                "claim-datum",
                "--task",
                task,
                "--commit",
                "abc123",
                "--diff",
                "d1",
            ],
        ),
        "pf append model_claim",
    );
    assert_ok(
        &pf(env, &["append", "tool_attestation", "ran", "--task", task]),
        "pf append tool_attestation",
    );
}

fn read_json(path: &Path) -> serde_json::Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Every ledger entry as parsed JSON, in seq order.
fn ledger_entries(env: &Env) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(&env.ledger).expect("read ledger");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("ledger line is JSON"))
        .collect()
}

fn entry_by_kind(env: &Env, kind: &str) -> serde_json::Value {
    ledger_entries(env)
        .into_iter()
        .find(|e| e["kind"].as_str() == Some(kind))
        .unwrap_or_else(|| panic!("no {kind} entry in ledger"))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn test_army_full_loop() {
    let env = Env::new();
    seed_verified(&env, "T11");

    // `pf ledger tail` captures the chain head; the gate manifest must agree.
    let tail_out = pf(&env, &["ledger", "tail"]);
    assert_ok(&tail_out, "pf ledger tail");
    let tail_hash = String::from_utf8_lossy(&tail_out.stdout).trim().to_string();
    assert!(
        is_hex64(&tail_hash),
        "ledger tail must be 64 hex, got {tail_hash}"
    );

    let first = pf(&env, &["gate", "T11", "--required", "verified"]);
    assert_ok(&first, "pf gate T11 --required verified (first run)");
    let stdout_first = String::from_utf8_lossy(&first.stdout).into_owned();
    assert!(
        stdout_first.contains("gate PASSED"),
        "stdout should report gate PASSED, got: {stdout_first}"
    );

    // Manifest: passed=true, bundle_sha256 = 64 hex, tail_hash cross-checks
    // against the `pf ledger tail` output captured above.
    let manifest: serde_json::Value = read_json(&env.manifest("T11"));
    assert_eq!(manifest["task_id"], "T11", "manifest task_id");
    assert_eq!(manifest["passed"], true, "manifest passed must be true");
    let bundle_sha = manifest["bundle_sha256"]
        .as_str()
        .expect("bundle_sha256 present");
    assert!(
        is_hex64(bundle_sha),
        "bundle_sha256 must be 64 hex, got {bundle_sha}"
    );
    assert_eq!(
        manifest["tail_hash"].as_str().unwrap(),
        tail_hash,
        "manifest tail_hash must equal `pf ledger tail`"
    );

    // The bundle itself must exist on PASS.
    assert!(
        env.bundle("T11").exists(),
        "bundle {} must exist on PASS",
        env.bundle("T11").display()
    );

    // Reproducibility: a second gate run must produce byte-identical bundle
    // and sha256 (deterministic bundle writing).
    assert_ok(
        &pf(&env, &["gate", "T11", "--required", "verified"]),
        "pf gate T11 --required verified (second run)",
    );
    let bundle_first = std::fs::read(env.bundle("T11")).unwrap();
    let bundle_second = std::fs::read(env.bundle("T11")).unwrap();
    assert_eq!(
        bundle_first, bundle_second,
        "bundle .jsonl must be byte-identical across runs"
    );
    let manifest_second: serde_json::Value = read_json(&env.manifest("T11"));
    assert_eq!(
        manifest_second["bundle_sha256"].as_str().unwrap(),
        bundle_sha,
        "bundle_sha256 must be identical across runs"
    );
}

#[test]
fn test_army_rewind_fails_cycle() {
    let env = Env::new();
    seed_verified(&env, "T11B");

    // Rewind attack: flip ONE byte of the tail entry's payload (a letter in
    // the promoted state string, "Verified" -> "Verifled" — same shape as the
    // client_smoke "ModelClaimed"->"ModelClailed" pattern). JSON stays valid,
    // so only the merkle chain can catch it.
    let raw = std::fs::read_to_string(&env.ledger).expect("read ledger");
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let last = lines.pop().expect("ledger must have a tail line");
    assert!(
        last.contains("\"Verified\""),
        "tail entry must carry the promoted Verified state for the tamper"
    );
    lines.push(last.replacen("\"Verified\"", "\"Verifled\"", 1));
    std::fs::write(&env.ledger, lines.join("\n") + "\n").expect("write tampered ledger");

    // The gate must fail: stderr carries the rendered GateError::LedgerIntegrity
    // Display text ("ledger integrity broken at seq N: expected .., found ..").
    // Exit code is 2: cmd_gate maps only TaskNotFound to exit 1; every other
    // GateError bubbles up through main's `error: ...` path (ExitCode::from(2)).
    let out = pf(&env, &["gate", "T11B", "--required", "verified"]);
    let err = stderr(&out);
    assert_eq!(
        exit_code(&out),
        2,
        "tampered chain must fail the gate\nstderr: {err}"
    );
    assert!(
        err.contains("ledger integrity broken at seq"),
        "stderr must render LedgerIntegrity, got: {err}"
    );

    // No fabricated bundle (and no manifest) on the integrity-failure path.
    assert!(
        !env.bundle("T11B").exists(),
        "no bundle may be fabricated on a failed gate"
    );
    assert!(
        !env.manifest("T11B").exists(),
        "no manifest may be fabricated on an integrity failure"
    );
}

#[test]
fn test_army_eval_attestation_identity_roundtrip() {
    let env = Env::new();
    assert_ok(&pf(&env, &["init"]), "pf init");
    assert_ok(
        &pf(
            &env,
            &[
                "append",
                "model_claim",
                "claim-datum",
                "--task",
                "T4E",
                "--commit",
                "abc123",
                "--diff",
                "d1",
                "--experiment",
                "exp-1",
                "--model",
                "mf-1",
                "--run",
                "run-1",
                "--budget",
                "0.50 usd",
                "--metadata",
                r#"{"pass@1":0.8}"#,
            ],
        ),
        "pf append model_claim with identity flags",
    );
    assert_ok(
        &pf(
            &env,
            &[
                "append",
                "eval_attestation",
                "eval ran",
                "--task",
                "T4E",
                "--experiment",
                "exp-1",
                "--model",
                "mf-1",
                "--run",
                "run-1",
                "--budget",
                "0.50 usd",
                "--metadata",
                r#"{"pass@1":0.8}"#,
            ],
        ),
        "pf append eval_attestation with identity flags",
    );

    // The claim carries the identity fields verbatim.
    let claim = entry_by_kind(&env, "ModelClaim");
    assert_eq!(claim["payload"]["experiment_id"], "exp-1");
    assert_eq!(claim["payload"]["model_fingerprint"], "mf-1");
    assert_eq!(claim["payload"]["run_id"], "run-1");
    assert_eq!(claim["payload"]["budget"], "0.50 usd");
    assert_eq!(
        claim["payload"]["eval_metadata"],
        serde_json::json!({ "pass@1": 0.8 })
    );

    // The promoted EvalAttestation is Verified and round-trips the identity
    // fields (promote copies them from the attestation).
    let eval = entry_by_kind(&env, "EvalAttestation");
    assert_eq!(eval["payload"]["state"], "Verified");
    assert_eq!(eval["payload"]["task_id"], "T4E");
    assert_eq!(eval["payload"]["experiment_id"], "exp-1");
    assert_eq!(eval["payload"]["model_fingerprint"], "mf-1");
    assert_eq!(eval["payload"]["run_id"], "run-1");
    assert_eq!(eval["payload"]["budget"], "0.50 usd");
    assert_eq!(
        eval["payload"]["eval_metadata"],
        serde_json::json!({ "pass@1": 0.8 })
    );

    // An eval-attested task is gateable as verified.
    assert_ok(
        &pf(&env, &["gate", "T4E", "--required", "verified"]),
        "pf gate T4E --required verified",
    );
}

#[test]
fn test_army_discrepancy_promotes_to_refuted() {
    let env = Env::new();
    assert_ok(&pf(&env, &["init"]), "pf init");
    assert_ok(
        &pf(
            &env,
            &[
                "append",
                "model_claim",
                "claim-datum",
                "--task",
                "T4D",
                "--commit",
                "abc123",
                "--diff",
                "d1",
            ],
        ),
        "pf append model_claim",
    );
    assert_ok(
        &pf(
            &env,
            &["append", "discrepancy", "trace data", "--task", "T4D"],
        ),
        "pf append discrepancy",
    );

    let disc = entry_by_kind(&env, "Discrepancy");
    assert_eq!(disc["payload"]["state"], "Refuted");
    assert_eq!(disc["payload"]["task_id"], "T4D");
    assert_eq!(disc["payload"]["rationale"], "trace data");
    assert_eq!(disc["payload"]["validator"], "polyforge-cli-operator");
    assert_eq!(disc["payload"]["exit_code"], 1);

    // A Refuted task must NOT pass a verified gate (record-only in M1).
    let out = pf(&env, &["gate", "T4D", "--required", "verified"]);
    assert_eq!(
        exit_code(&out),
        1,
        "Refuted task must fail a verified gate\nstderr: {}",
        stderr(&out)
    );
}

#[test]
fn test_army_identity_flags_absent_are_null() {
    let env = Env::new();
    assert_ok(&pf(&env, &["init"]), "pf init");
    assert_ok(
        &pf(
            &env,
            &["append", "model_claim", "claim-datum", "--task", "T4N"],
        ),
        "pf append model_claim",
    );
    assert_ok(
        &pf(
            &env,
            &["append", "eval_attestation", "eval ran", "--task", "T4N"],
        ),
        "pf append eval_attestation",
    );

    for e in ledger_entries(&env) {
        assert_eq!(e["payload"]["experiment_id"], serde_json::Value::Null);
        assert_eq!(e["payload"]["model_fingerprint"], serde_json::Value::Null);
        assert_eq!(e["payload"]["run_id"], serde_json::Value::Null);
        assert_eq!(e["payload"]["budget"], serde_json::Value::Null);
        assert_eq!(e["payload"]["eval_metadata"], serde_json::Value::Null);
    }
}

#[test]
fn test_army_usage_shows_new_kinds_and_pf_defaults() {
    let env = Env::new();
    // No args -> print_usage on stderr, exit 2.
    let out = pf(&env, &[]);
    assert_eq!(exit_code(&out), 2, "no args must print usage and exit 2");
    let err = stderr(&out);
    assert!(
        err.contains("eval_attestation"),
        "usage must list eval_attestation, got: {err}"
    );
    assert!(
        err.contains("discrepancy"),
        "usage must list discrepancy, got: {err}"
    );
    assert!(
        err.contains(".pf/ledger.jsonl"),
        "usage must show the .pf/ ledger default, got: {err}"
    );
    assert!(
        err.contains(".pf/evidence/"),
        "usage must show the .pf/ evidence default, got: {err}"
    );
}
