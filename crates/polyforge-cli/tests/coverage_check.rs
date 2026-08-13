//! Integration tests for the `polyforge-cli coverage-check` command.
//!
//! Spawn the REAL binary against crafted `cargo llvm-cov --json` exports:
//! exit 0 + `coverage PASS` when every crate aggregate and file clears the
//! 80/80 floor; exit 1 + a naming `coverage FAIL` line when any scope falls
//! below it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn report_path(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "polyforge-cov-{}-{n}-{name}.json",
        std::process::id()
    ))
}

fn write_report(name: &str, body: &str) -> PathBuf {
    let path = report_path(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn coverage_check(report: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(["coverage-check", "--report"])
        .arg(report)
        .output()
        .expect("failed to spawn polyforge-cli binary")
}

/// A realistic `cargo llvm-cov --json` export with one data entry.
fn llvm_cov_export(totals_percent: f64, files: &[(&str, f64)]) -> String {
    let files_json: Vec<String> = files
        .iter()
        .map(|(path, percent)| {
            format!(
                r#"{{"filename":"{path}","summary":{{"lines":{{"count":10,"covered":5,"percent":{percent}}}}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{
  "type": "llvm.coverage.json.export",
  "version": "3.1.0",
  "data": [
    {{
      "totals": {{"lines": {{"count": 100, "covered": 80, "percent": {totals_percent}}}}},
      "files": [{}]
    }}
  ]
}}"#,
        files_json.join(",")
    )
}

#[test]
fn coverage_check_exits_1_and_names_file_below_floor() {
    let report = write_report(
        "below-file",
        &llvm_cov_export(95.0, &[("/repo/crates/polyforge-core/src/ledger.rs", 50.0)]),
    );
    let out = coverage_check(&report);
    let code = out.status.code().expect("no exit code");
    assert_eq!(code, 1, "a file below the 80% floor must exit 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("coverage FAIL"), "stdout: {stdout}");
    assert!(
        stdout.contains("/repo/crates/polyforge-core/src/ledger.rs"),
        "stdout must name the offending file: {stdout}"
    );
    // The crate aggregate (95%) clears the floor — no crate failure expected.
    assert!(!stdout.contains("crate "), "stdout: {stdout}");
}

#[test]
fn coverage_check_exits_1_and_names_crate_below_floor() {
    let report = write_report(
        "below-crate",
        &llvm_cov_export(
            70.0,
            &[("/repo/crates/polyforge-core/src/evidence.rs", 99.0)],
        ),
    );
    let out = coverage_check(&report);
    let code = out.status.code().expect("no exit code");
    assert_eq!(code, 1, "a crate aggregate below the 80% floor must exit 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("crate polyforge-core"),
        "stdout must name the offending crate: {stdout}"
    );
}

#[test]
fn coverage_check_exits_0_when_everything_clears_floor() {
    let report = write_report(
        "pass",
        &llvm_cov_export(
            92.0,
            &[
                ("/repo/crates/polyforge-core/src/evidence.rs", 97.0),
                ("/repo/crates/polyforge-core/src/gate.rs", 85.0),
            ],
        ),
    );
    let out = coverage_check(&report);
    let code = out.status.code().expect("no exit code");
    assert_eq!(code, 0, "all scopes above the floor must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("coverage PASS"), "stdout: {stdout}");
}

#[test]
fn coverage_check_exits_0_on_empty_report() {
    let report = write_report(
        "empty",
        r#"{"type":"llvm.coverage.json.export","version":"3.1.0","data":[]}"#,
    );
    let out = coverage_check(&report);
    let code = out.status.code().expect("no exit code");
    assert_eq!(code, 0, "an empty report passes vacuously");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("coverage PASS"), "stdout: {stdout}");
}

#[test]
fn coverage_check_rejects_unreadable_report() {
    let out = Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(["coverage-check", "--report", "/nonexistent/cov.json"])
        .output()
        .expect("failed to spawn polyforge-cli binary");
    let code = out.status.code().expect("no exit code");
    assert_eq!(
        code, 2,
        "an unreadable report is a usage/IO error, not a FAIL"
    );
}
