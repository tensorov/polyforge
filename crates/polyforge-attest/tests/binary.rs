//! Surface tests spawning the REAL `polyforge-attest` binary
//! (`env!("CARGO_BIN_EXE_polyforge-attest")`).
//!
//! These pin the process boundary that in-process `dispatch()` unit tests
//! cannot see: the actual process exit code of `main` (usage errors must exit
//! 2, success must exit 0) and the bytes `print_usage` writes to stderr.
//! Mutation context (T3, polyforge-v040-hardening): kills the
//! `main -> ExitCode` replaced-with-Default survivor (Default is SUCCESS, so
//! every error path would silently exit 0) and the `print_usage -> ()`
//! survivor (stderr would go empty on the no-args/help paths).

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_polyforge-attest")
}

/// Verbatim copy of this repository's real `.pf/ledger.jsonl`; passes
/// `read_ledger` Merkle verification.
fn fixture_ledger() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ledger.jsonl")
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin_path())
        .args(args)
        .output()
        .expect("spawn polyforge-attest");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn task_happy_path_exits_zero_with_json_statement_on_stdout() {
    let ledger = fixture_ledger();
    let (code, stdout, stderr) = run(&[
        "task",
        "--ledger",
        ledger.to_str().expect("utf8 fixture path"),
        "--task",
        "bootstrap",
    ]);
    assert_eq!(code, 0, "happy path must exit SUCCESS; stderr: {stderr}");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must parse as one JSON statement");
    assert_eq!(v["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(
        v["predicateType"],
        "https://polyforge.dev/attestations/evidence/v1"
    );
    assert_eq!(
        v["subject"][0]["name"],
        "polyforge/task/bootstrap@bfe611b4b91e"
    );
}

#[test]
fn no_args_exits_two_with_usage_on_stderr() {
    let (code, stdout, stderr) = run(&[]);
    assert_eq!(code, 2, "no-args usage error must exit 2, not {code}");
    assert!(
        stdout.is_empty(),
        "usage goes to stderr, got stdout: {stdout}"
    );
    assert!(
        stderr.contains("usage:") && stderr.contains("polyforge-attest task"),
        "stderr must carry the usage text, got: {stderr}"
    );
}

#[test]
fn help_flag_exits_two_with_usage_on_stderr() {
    for flag in ["--help", "-h", "help"] {
        let (code, _stdout, stderr) = run(&[flag]);
        assert_eq!(code, 2, "{flag} usage error must exit 2");
        assert!(
            stderr.contains("usage:") && stderr.contains("polyforge-attest chain"),
            "{flag} must print usage to stderr, got: {stderr}"
        );
    }
}

#[test]
fn unknown_flag_exits_two_with_error_on_stderr() {
    let ledger = fixture_ledger();
    let (code, stdout, stderr) = run(&[
        "task",
        "--ledger",
        ledger.to_str().expect("utf8 fixture path"),
        "--task",
        "bootstrap",
        "--wat",
    ]);
    assert_eq!(code, 2, "unknown flag must exit 2, not {code}");
    assert!(stdout.is_empty(), "no statement may be emitted on error");
    assert!(
        stderr.contains("error:") && stderr.contains("unknown flag: --wat"),
        "stderr must name the offending flag, got: {stderr}"
    );
}

#[test]
fn unknown_command_exits_two_with_error_on_stderr() {
    let (code, _stdout, stderr) = run(&["bogus"]);
    assert_eq!(code, 2, "unknown command must exit 2, not {code}");
    assert!(
        stderr.contains("error: unknown command: bogus"),
        "got: {stderr}"
    );
}
