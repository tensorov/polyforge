//! Mutation-guard integration tests: each test pins a behavior that a
//! surviving cargo-mutants mutant in main.rs would break. See the kill-matrix
//! in .omo/notepads/roadmap-phase0-trust-hardening/learnings.md.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Env {
    dir: PathBuf,
    ledger: PathBuf,
    evidence_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("polyforge-mutguard-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self {
            ledger: dir.join("ledger.jsonl"),
            evidence_dir: dir.join("evidence"),
            dir,
        }
    }
}

/// Spawn the real CLI with explicit PF_LEDGER/PF_EVIDENCE_DIR values and the
/// temp dir as the working directory (defaults resolve under the cwd).
fn pf_with(env: &Env, args: &[&str], ledger_env: &str, evidence_env: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(args)
        .current_dir(&env.dir)
        .env("PF_LEDGER", ledger_env)
        .env("PF_EVIDENCE_DIR", evidence_env)
        .output()
        .expect("failed to spawn polyforge-cli binary")
}

fn pf_in(env: &Env, args: &[&str]) -> Output {
    pf_with(
        env,
        args,
        env.ledger.to_str().expect("ledger path is UTF-8"),
        env.evidence_dir.to_str().expect("evidence dir is UTF-8"),
    )
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
    assert_ok(&pf_in(env, &["init"]), "pf init");
    assert_ok(
        &pf_in(
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
        &pf_in(env, &["append", "tool_attestation", "ran", "--task", task]),
        "pf append tool_attestation",
    );
}

/// Mutant :45:18 (guard -> true): an EMPTY PF_LEDGER must fall back to the
/// compiled-in `.pf/ledger.jsonl` default under the cwd.
#[test]
fn test_empty_pf_ledger_falls_back_to_default() {
    let env = Env::new();
    let out = pf_with(&env, &["init"], "", "");
    assert_ok(&out, "pf init with empty PF_LEDGER");
    assert!(
        env.dir.join(".pf/ledger.jsonl").exists(),
        "default .pf/ledger.jsonl must be created under the cwd"
    );
}

/// Mutant :52:18 (guard -> true): an EMPTY PF_EVIDENCE_DIR must fall back to
/// the default `.pf/evidence/`; the bundle must never land in the cwd root.
#[test]
fn test_empty_pf_evidence_dir_falls_back_to_default() {
    let env = Env::new();
    seed_verified(&env, "T1");

    let out = pf_with(
        &env,
        &["gate", "T1", "--required", "verified"],
        env.ledger.to_str().expect("ledger path is UTF-8"),
        "",
    );
    assert_ok(&out, "pf gate with empty PF_EVIDENCE_DIR");
    assert!(
        env.dir.join(".pf/evidence/gate-T1.jsonl").exists(),
        "bundle must be written under the default .pf/evidence/ dir"
    );
    assert!(
        !env.dir.join("gate-T1.jsonl").exists(),
        "bundle must NOT be written to the cwd root"
    );
}

/// Mutant :77:5 (cmd_init -> Ok(())): init must create the ledger file and
/// report both the fresh and the already-existing cases.
#[test]
fn test_init_creates_ledger_and_reports_existing() {
    let env = Env::new();
    let out = pf_in(&env, &["init"]);
    assert_ok(&out, "pf init (fresh)");
    assert!(env.ledger.exists(), "ledger file must exist after init");
    assert!(
        stderr(&out).contains("created ledger at"),
        "stderr must report creation, got: {}",
        stderr(&out)
    );

    let again = pf_in(&env, &["init"]);
    assert_ok(&again, "pf init (second)");
    assert!(
        stderr(&again).contains("ledger already exists at"),
        "stderr must report the existing ledger, got: {}",
        stderr(&again)
    );
}

/// Mutant :602:27 (< -> == / <=): an append with EXACTLY three args (kind +
/// payload) must be accepted, not rejected as a usage error.
#[test]
fn test_append_with_exactly_three_args_succeeds() {
    let env = Env::new();
    assert_ok(&pf_in(&env, &["init"]), "pf init");
    let out = pf_in(&env, &["append", "model_claim", "datum"]);
    assert_ok(&out, "pf append with exactly three args");
}

/// Mutant :688:27 (< -> >): `pf ledger` with no subcommand must be a usage error.
#[test]
fn test_ledger_requires_subcommand() {
    let env = Env::new();
    let out = pf_in(&env, &["ledger"]);
    assert_ne!(
        exit_code(&out),
        0,
        "ledger without subcommand must not exit 0"
    );
    assert!(
        stderr(&out).contains("usage: pf ledger"),
        "stderr must show ledger usage, got: {}",
        stderr(&out)
    );
}

/// Mutant :704:27 (< -> ==): `pf gate` with no task id must be a usage error.
#[test]
fn test_gate_requires_task_id() {
    let env = Env::new();
    let out = pf_in(&env, &["gate"]);
    assert_ne!(exit_code(&out), 0, "gate without task id must not exit 0");
    assert!(
        stderr(&out).contains("usage: pf gate"),
        "stderr must show gate usage, got: {}",
        stderr(&out)
    );
}

/// Mutant :704:27 (< -> == / <=): `pf gate <task>` with exactly two args must run the gate.
#[test]
fn test_gate_with_task_only_runs_gate() {
    let env = Env::new();
    seed_verified(&env, "T2");
    let out = pf_in(&env, &["gate", "T2"]);
    assert_ok(&out, "pf gate with exactly two args");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("gate PASSED for task T2"),
        "gate must pass on the verified chain, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Mutant :710:21 (< -> == / >): the flag loop must process `--required`.
#[test]
fn test_gate_flag_loop_processes_required() {
    let env = Env::new();
    seed_verified(&env, "T3");
    let out = pf_in(&env, &["gate", "T3", "--required", "validated"]);
    assert_ne!(
        exit_code(&out),
        0,
        "validated gate must fail on a verified-only chain"
    );
    assert!(
        stderr(&out).contains("gate FAILED for task T3"),
        "stderr must report the failed gate, got: {}",
        stderr(&out)
    );
}

/// Mutant :726:27 (< -> >) and :726:31 (|| -> &&): `pf coverage-check --report` must be a usage error.
#[test]
fn test_coverage_check_requires_report_flag() {
    let env = Env::new();
    let out = pf_in(&env, &["coverage-check", "--report"]);
    assert_ne!(
        exit_code(&out),
        0,
        "coverage-check without report must not exit 0"
    );
    assert!(
        stderr(&out).contains("usage: pf coverage-check"),
        "stderr must show coverage-check usage, got: {}",
        stderr(&out)
    );
}
