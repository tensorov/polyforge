//! End-to-end "army" harness for `pf-cli`: drives the REAL binary
//! (`env!("CARGO_BIN_EXE_pf-cli")`) against unique per-test temp ledgers
//! (AtomicU64 counter, distinct `pf-cli-army-` prefix so parallel gate.rs
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
        let dir = std::env::temp_dir().join(format!("pf-cli-army-{}-{n}", std::process::id()));
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
    Command::new(env!("CARGO_BIN_EXE_pf-cli"))
        .args(args)
        .env("PF_LEDGER", &env.ledger)
        .env("PF_EVIDENCE_DIR", &env.evidence_dir)
        .output()
        .expect("failed to spawn pf-cli binary")
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
