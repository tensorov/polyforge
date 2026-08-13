//! Integration tests for the `polyforge-cli ledger summary` command.
//!
//! Spawn the REAL binary (`env!("CARGO_BIN_EXE_polyforge-cli")`) against unique
//! per-test temp ledgers (AtomicU64 counter - polyforge-cli test pattern). The
//! tri-state chains are built through the CLI itself; assertions are on the
//! exact single grep-able summary line.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Env {
    ledger: PathBuf,
}

impl Env {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("polyforge-summary-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            ledger: dir.join("ledger.jsonl"),
        }
    }
}

fn pf(env: &Env, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(args)
        .env("PF_LEDGER", &env.ledger)
        .output()
        .expect("failed to spawn polyforge-cli binary")
}

fn assert_ok(out: &Output, what: &str) {
    assert_eq!(
        out.status.code().expect("no exit code"),
        0,
        "{what} should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn summary_line(env: &Env) -> String {
    let out = pf(env, &["ledger", "summary"]);
    assert_ok(&out, "pf ledger summary");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Append a claim-only entry: the task stays ModelClaimed (counts as failed).
fn seed_claim_only(env: &Env, task: &str) {
    assert_ok(
        &pf(
            env,
            &["append", "model_claim", "claim-datum", "--task", task],
        ),
        "pf append model_claim",
    );
}

/// Append claim + tool_attestation: latest state Verified (counts as verified).
fn seed_verified(env: &Env, task: &str) {
    seed_claim_only(env, task);
    assert_ok(
        &pf(env, &["append", "tool_attestation", "ran", "--task", task]),
        "pf append tool_attestation",
    );
}

/// Append claim + attestation + validation: latest state Validated.
fn seed_validated(env: &Env, task: &str) {
    seed_verified(env, task);
    assert_ok(
        &pf(env, &["append", "validation", "op check", "--task", task]),
        "pf append validation",
    );
}

/// Append claim + discrepancy: latest state Refuted (counts as failed).
fn seed_refuted(env: &Env, task: &str) {
    seed_claim_only(env, task);
    assert_ok(
        &pf(
            env,
            &["append", "discrepancy", "trace data", "--task", task],
        ),
        "pf append discrepancy",
    );
}

#[test]
fn test_summary_missing_ledger_prints_zeros() {
    // No init, no entries: the ledger file does not even exist.
    let env = Env::new();
    assert_eq!(
        summary_line(&env),
        "tasks_verified=0 tasks_validated=0 tasks_failed=0"
    );
}

#[test]
fn test_summary_empty_init_ledger_prints_zeros() {
    // init creates an empty ledger file; summary must still print zeros.
    let env = Env::new();
    assert_ok(&pf(&env, &["init"]), "pf init");
    assert_eq!(
        summary_line(&env),
        "tasks_verified=0 tasks_validated=0 tasks_failed=0"
    );
}

#[test]
fn test_summary_mixed_states() {
    let env = Env::new();
    seed_verified(&env, "T-verified");
    seed_validated(&env, "T-validated");
    seed_claim_only(&env, "T-claimed");
    seed_refuted(&env, "T-refuted");
    assert_eq!(
        summary_line(&env),
        "tasks_verified=1 tasks_validated=1 tasks_failed=2",
        "verified + validated + (claim-only, refuted) must count as 1+1+2"
    );
}

#[test]
fn test_summary_latest_state_wins() {
    let env = Env::new();
    // T-latest goes claim -> verified -> validated: the LATEST state is
    // Validated, so it must count as validated, NOT verified.
    seed_validated(&env, "T-latest");
    // T-twice is verified twice: latest state stays Verified.
    seed_verified(&env, "T-twice");
    seed_verified(&env, "T-twice");
    assert_eq!(
        summary_line(&env),
        "tasks_verified=1 tasks_validated=1 tasks_failed=0",
        "latest state per task must win: T-latest=validated, T-twice=verified"
    );
}

#[test]
fn test_summary_claim_only_counts_as_failed() {
    let env = Env::new();
    seed_claim_only(&env, "T-claimed");
    assert_eq!(
        summary_line(&env),
        "tasks_verified=0 tasks_validated=0 tasks_failed=1",
        "a bare ModelClaimed (never promoted) must count as failed"
    );
}
