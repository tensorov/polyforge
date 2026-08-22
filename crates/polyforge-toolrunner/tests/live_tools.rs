//! Live-run integration tests for the v2 Python/JS-TS allowlist tools.
//!
//! These are SKIP-IF-ABSENT tests: every test first probes whether its binary
//! can be spawned at all (`<bin> --version`) and silently skips, with an
//! eprintln note, when the tool is not installed. Rust-only CI machines stay
//! green without installing any of these tools.
//!
//! Exit-code semantics are deliberately UNASSERTED. A bare invocation in
//! whatever working directory the test runner happens to use legitimately
//! exits non-zero across tool versions and project layouts (there is no
//! pytest.ini, tsconfig.json, or eslint config here). Each test asserts ONLY
//! that the runner's spawn+wait mechanics completed and captured a full
//! RunOutput; outcomes belong to real attestations, not to this suite.

use std::path::Path;
use std::process::{Command, Stdio};

use polyforge_toolrunner::{lookup, run};

/// Spawn-ability probe only: can the OS locate and exec `<bin> --version`?
/// The probe's own exit code is ignored on purpose - every allowlisted
/// binary prints a version harmlessly, and we care solely about whether the
/// binary resolves on PATH.
fn spawnable(bin: &Path) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok()
}

/// Shared body of every live test: look the canonical allowlist name up,
/// skip when the binary is absent, otherwise run it with NO extra args and
/// assert only the mechanics (spawn succeeded, wait completed, RunOutput was
/// fully constructed). Exit codes are deliberately not examined.
///
/// `run` executes with the CURRENT cwd of the test process. That is
/// acceptable here because we assert mechanics, not outcomes: a bare
/// invocation may legitimately fail differently per tool version and per
/// directory contents, and chdir-ing from parallel tests would mutate
/// process-global state anyway.
fn run_mechanics_only(name: &str) {
    let tool = lookup(name).unwrap_or_else(|| panic!("{name} must stay on the v2 allowlist"));
    if !spawnable(&tool.bin) {
        eprintln!("skip {name}: not on PATH");
        return;
    }
    let out = run(&tool, &[]).expect("spawn+wait mechanics must complete");
    assert_eq!(
        out.stdout_hash.len(),
        64,
        "RunOutput must carry the sha256 of captured stdout"
    );
    assert!(
        !out.env_fingerprint.is_empty(),
        "RunOutput must carry the environment fingerprint"
    );
}

#[test]
fn live_pytest_completes_when_on_path() {
    run_mechanics_only("pytest");
}

#[test]
fn live_ruff_completes_when_on_path() {
    // Both ruff allowlist entries share one binary; exercise both so every
    // v2 name gets a live run whenever ruff is installed.
    run_mechanics_only("ruff check");
    run_mechanics_only("ruff format --check");
}

#[test]
fn live_mypy_completes_when_on_path() {
    run_mechanics_only("mypy");
}

#[test]
fn live_pyright_completes_when_on_path() {
    run_mechanics_only("pyright");
}

#[test]
fn live_uv_completes_when_on_path() {
    run_mechanics_only("uv --version");
}

#[test]
fn live_vitest_completes_when_on_path() {
    run_mechanics_only("vitest run");
}

#[test]
fn live_tsc_completes_when_on_path() {
    run_mechanics_only("tsc");
}

#[test]
fn live_eslint_completes_when_on_path() {
    run_mechanics_only("eslint");
}

#[test]
fn live_biome_completes_when_on_path() {
    run_mechanics_only("biome check");
}

/// Always-run shape guard: pins the v2 table's fixed args against accidental
/// edits. Pure lookup, so it passes on machines with none of the tools
/// installed.
#[test]
fn v2_allowlist_shape_pins_fixed_args() {
    let pytest = lookup("pytest").expect("pytest on the allowlist");
    assert_eq!(pytest.name, "pytest");
    assert!(pytest.args.is_empty(), "pytest fixed args must stay []");

    let tsc = lookup("tsc").expect("tsc on the allowlist");
    assert_eq!(tsc.name, "tsc");
    assert_eq!(tsc.args, ["--noEmit"]);
}
