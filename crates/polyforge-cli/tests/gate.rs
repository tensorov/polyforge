//! Integration tests for the `polyforge-cli` gate command.
//!
//! Spawn the REAL binary (`env!("CARGO_BIN_EXE_polyforge-cli")`) against unique
//! per-test temp ledgers/evidence dirs (AtomicU64 counter — polyforge-core pattern,
//! no fixed paths). The honest tri-state chain is built through the CLI
//! itself: `model_claim` then a `tool_attestation` that promotes the claim
//! to `Verified`. Gates assert on manifest CONTENT (passed / bundle_sha256 /
//! tail_hash), never on exit codes alone.

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
        let dir = std::env::temp_dir().join(format!("polyforge-gate-{}-{n}", std::process::id()));
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
/// Returns the bundle/manifest paths via the env.
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

/// Append a second claim for `task` with the given commit/diff key.
fn seed_second_claim(env: &Env, task: &str, commit: &str, diff: &str) {
    assert_ok(
        &pf(
            env,
            &[
                "append",
                "model_claim",
                "claim-datum-2",
                "--task",
                task,
                "--commit",
                commit,
                "--diff",
                diff,
            ],
        ),
        "pf append second model_claim",
    );
}

fn read_json(path: &Path) -> serde_json::Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Assert a pass manifest: passed=true + bundle_sha256 + tail_hash present.
fn assert_pass_manifest(env: &Env, task: &str) {
    let manifest: serde_json::Value = read_json(&env.manifest(task));
    assert_eq!(manifest["task_id"], task, "manifest task_id");
    assert_eq!(manifest["passed"], true, "manifest passed must be true");
    let bundle_sha = manifest["bundle_sha256"]
        .as_str()
        .expect("bundle_sha256 present");
    assert!(
        is_hex64(bundle_sha),
        "bundle_sha256 must be 64 hex chars, got {bundle_sha}"
    );
    let tail = manifest["tail_hash"].as_str().expect("tail_hash present");
    assert!(is_hex64(tail), "tail_hash must be 64 hex chars, got {tail}");
    assert!(
        env.bundle(task).exists(),
        "bundle {} must exist on PASS",
        env.bundle(task).display()
    );
}

/// Every ledger entry for `task` (in seq order), as raw JSONL lines.
fn ledger_lines_for_task(env: &Env, task: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(&env.ledger).unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("ledger line is JSON"))
        .filter(|v| v["payload"]["task_id"].as_str() == Some(task))
        .map(|v| serde_json::to_string(&v).expect("reserialize"))
        .collect()
}

#[test]
fn test_gate_exit_0_on_pass() {
    let env = Env::new();
    seed_verified(&env, "T1");

    let out = pf(&env, &["gate", "T1", "--required", "verified"]);
    assert_ok(&out, "pf gate T1 --required verified");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("gate PASSED"),
        "stdout should report gate PASSED"
    );
    assert_pass_manifest(&env, "T1");
}

#[test]
fn test_gate_exit_1_on_missing_evidence() {
    let env = Env::new();
    assert_ok(&pf(&env, &["init"]), "pf init");
    assert_ok(
        &pf(&env, &["append", "model_claim", "x", "--task", "T2"]),
        "pf append model_claim",
    );

    let out = pf(&env, &["gate", "T2", "--required", "verified"]);
    assert_eq!(
        exit_code(&out),
        1,
        "ModelClaimed alone must fail a Verified gate\nstderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("missing: Verified"),
        "stderr must list missing Verified, got: {err}"
    );
    // No fabricated bundle on FAIL; the manifest records passed=false.
    assert!(
        !env.bundle("T2").exists(),
        "no bundle may be fabricated on a failed gate"
    );
    let manifest: serde_json::Value = read_json(&env.manifest("T2"));
    assert_eq!(
        manifest["passed"], false,
        "fail manifest must say passed=false"
    );
    assert!(
        manifest["bundle_sha256"].is_null(),
        "bundle_sha256 must be null on FAIL"
    );
}

#[test]
fn test_bundle_reproducible() {
    let env = Env::new();
    seed_verified(&env, "T3");

    assert_ok(
        &pf(&env, &["gate", "T3", "--required", "verified"]),
        "first gate run",
    );
    let bundle_first = std::fs::read(env.bundle("T3")).unwrap();
    let manifest_first = read_json(&env.manifest("T3"));
    let sha_first = manifest_first["bundle_sha256"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ok(
        &pf(&env, &["gate", "T3", "--required", "verified"]),
        "second gate run",
    );
    let bundle_second = std::fs::read(env.bundle("T3")).unwrap();
    let manifest_second = read_json(&env.manifest("T3"));
    let sha_second = manifest_second["bundle_sha256"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        bundle_first, bundle_second,
        "bundle .jsonl must be byte-identical across runs"
    );
    assert_eq!(
        sha_first, sha_second,
        "bundle_sha256 must be identical across runs"
    );
    assert_eq!(
        manifest_first, manifest_second,
        "manifests must be identical across runs"
    );
}

#[test]
fn test_bundle_snapshot_matches_ledger() {
    let env = Env::new();
    seed_verified(&env, "T4");

    assert_ok(
        &pf(&env, &["gate", "T4", "--required", "verified"]),
        "pf gate T4",
    );

    // manifest tail_hash == `pf ledger tail` (head of the whole chain)
    let tail_out = pf(&env, &["ledger", "tail"]);
    assert_ok(&tail_out, "pf ledger tail");
    let tail_hash = String::from_utf8_lossy(&tail_out.stdout).trim().to_string();
    let manifest: serde_json::Value = read_json(&env.manifest("T4"));
    assert_eq!(
        manifest["tail_hash"].as_str().unwrap(),
        tail_hash,
        "manifest tail_hash must equal `pf ledger tail`"
    );

    // bundle .jsonl == the ledger entries for that task (subset, in seq order)
    let bundle = std::fs::read_to_string(env.bundle("T4")).unwrap();
    let bundle_lines: Vec<&str> = bundle.lines().filter(|l| !l.trim().is_empty()).collect();
    let ledger_lines = ledger_lines_for_task(&env, "T4");
    assert_eq!(
        bundle_lines.len(),
        ledger_lines.len(),
        "bundle must contain exactly this task's ledger entries"
    );
    for (i, b) in bundle_lines.iter().enumerate() {
        let bv: serde_json::Value = serde_json::from_str(b).unwrap();
        let lv: serde_json::Value = serde_json::from_str(&ledger_lines[i]).unwrap();
        assert_eq!(bv, lv, "bundle entry {i} must match the ledger record");
        assert_eq!(bv["payload"]["task_id"].as_str(), Some("T4"));
    }
}

#[test]
fn test_gate_keyed_scope_filters_by_commit_diff() {
    let env = Env::new();
    seed_verified(&env, "T5"); // claim abc123/d1 + verified
    seed_second_claim(&env, "T5", "def456", "d2");

    // Keyed on the verified key: passes.
    let out = pf(
        &env,
        &[
            "gate",
            "T5",
            "--required",
            "verified",
            "--commit",
            "abc123",
            "--diff",
            "d1",
        ],
    );
    assert_ok(&out, "pf gate T5 --commit abc123 --diff d1");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("gate PASSED"),
        "keyed gate on the verified key must pass"
    );
    assert_pass_manifest(&env, "T5");

    // Keyed on the unverified key: fails with missing Verified.
    let out = pf(
        &env,
        &[
            "gate",
            "T5",
            "--required",
            "verified",
            "--commit",
            "def456",
            "--diff",
            "d2",
        ],
    );
    assert_eq!(
        exit_code(&out),
        1,
        "keyed gate on the unverified key must fail\nstderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("missing: Verified"),
        "stderr must list missing Verified, got: {}",
        stderr(&out)
    );
    let manifest: serde_json::Value = read_json(&env.manifest("T5"));
    assert_eq!(
        manifest["passed"], false,
        "keyed FAIL manifest must say passed=false"
    );
}

#[test]
fn test_gate_latest_claim_rejects_stale_pass() {
    let env = Env::new();
    seed_verified(&env, "T7"); // claim abc123/d1 + verified
    seed_second_claim(&env, "T7", "abc123", "d1"); // same key: Verified is stale

    let out = pf(&env, &["gate", "T7", "--required", "verified"]);
    assert_eq!(
        exit_code(&out),
        1,
        "a stale Verified must fail the default LatestClaim gate\nstderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("missing: Verified"),
        "stderr must list missing Verified, got: {}",
        stderr(&out)
    );
    let manifest: serde_json::Value = read_json(&env.manifest("T7"));
    assert_eq!(
        manifest["passed"], false,
        "stale-pass manifest must say passed=false"
    );
}

#[test]
fn test_gate_requires_commit_and_diff_together() {
    let env = Env::new();
    seed_verified(&env, "T8");

    let out = pf(
        &env,
        &["gate", "T8", "--required", "verified", "--commit", "abc123"],
    );
    assert_eq!(
        exit_code(&out),
        2,
        "a partial key must be a usage error\nstderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("--commit and --diff must be provided together"),
        "stderr must explain the partial-key error, got: {}",
        stderr(&out)
    );
}
