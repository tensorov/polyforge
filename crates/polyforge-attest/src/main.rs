//! polyforge-attest: thin binary that emits deterministic in-toto statements
//! from a PolyForge evidence ledger.
//!
//! Subcommands:
//!   polyforge-attest task --ledger <path> --task <id> [--bundle <file>]
//!       Print the task's evidence subchain statement as canonical JSON on
//!       stdout. With --bundle the subject digest covers the bundle file bytes
//!       instead of the task's ledger subchain.
//!   polyforge-attest chain --ledger <path> [--out <file>]
//!       Print the whole-chain statement as canonical JSON on stdout. With
//!       --out, write a DSSE envelope (base64 canonical statement payload,
//!       unsigned) to the file instead of printing.
//!
//! Usage errors exit with code 2, mirroring polyforge-cli conventions.

use std::env;
use std::path::Path;
use std::process::ExitCode;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

use polyforge_attest::{
    canonical_json, emit_chain_statement, emit_task_statement, read_ledger, DsseEnvelope,
};

const TASK_USAGE: &str =
    "usage: polyforge-attest task --ledger <path> --task <id> [--bundle <file>]";
const CHAIN_USAGE: &str = "usage: polyforge-attest chain --ledger <path> [--out <file>]";

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
        "task" => cmd_task(&args[1..]),
        "chain" => cmd_chain(&args[1..]),
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(ExitCode::from(2))
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn cmd_task(args: &[String]) -> Result<ExitCode, String> {
    let mut ledger: Option<&str> = None;
    let mut task_id: Option<&str> = None;
    let mut bundle: Option<&str> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--ledger" => {
                ledger = Some(
                    iter.next()
                        .ok_or_else(|| "--ledger requires a value".to_string())?,
                );
            }
            "--task" => {
                task_id = Some(
                    iter.next()
                        .ok_or_else(|| "--task requires a value".to_string())?,
                );
            }
            "--bundle" => {
                bundle = Some(
                    iter.next()
                        .ok_or_else(|| "--bundle requires a value".to_string())?,
                );
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let ledger = ledger.ok_or_else(|| TASK_USAGE.to_string())?;
    let task_id = task_id.ok_or_else(|| TASK_USAGE.to_string())?;

    let entries = read_ledger(Path::new(ledger)).map_err(|e| e.to_string())?;
    let statement = emit_task_statement(&entries, task_id, bundle).map_err(|e| e.to_string())?;
    println!("{}", statement_json(&statement)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_chain(args: &[String]) -> Result<ExitCode, String> {
    let mut ledger: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--ledger" => {
                ledger = Some(
                    iter.next()
                        .ok_or_else(|| "--ledger requires a value".to_string())?,
                );
            }
            "--out" => {
                out = Some(
                    iter.next()
                        .ok_or_else(|| "--out requires a value".to_string())?,
                );
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let ledger = ledger.ok_or_else(|| CHAIN_USAGE.to_string())?;

    let entries = read_ledger(Path::new(ledger)).map_err(|e| e.to_string())?;
    let payload = statement_json(&emit_chain_statement(&entries))?;
    match out {
        Some(path) => {
            let envelope =
                DsseEnvelope::new(BASE64_STANDARD.encode(payload.as_bytes()), Vec::new());
            let text = statement_json(&envelope)?;
            std::fs::write(path, text).map_err(|e| format!("write envelope {path}: {e}"))?;
            eprintln!("wrote DSSE envelope to {path}");
            Ok(ExitCode::SUCCESS)
        }
        None => {
            println!("{payload}");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Canonical JSON for any serializable attestation structure. Canonical form
/// (sorted keys, compact separators) keeps identical inputs byte-identical.
fn statement_json<T: serde::Serialize>(value: &T) -> Result<String, String> {
    let v = serde_json::to_value(value).map_err(|e| e.to_string())?;
    Ok(canonical_json(&v))
}

fn print_usage() {
    eprintln!(
        "polyforge-attest: deterministic in-toto statement emitter\n\
         \n\
         usage:\n\
         \x20 polyforge-attest task --ledger <path> --task <id> [--bundle <file>]\n\
         \x20 polyforge-attest chain --ledger <path> [--out <file>]\n\
         \n\
         task:  print the task evidence statement JSON on stdout; --bundle switches\n\
         \x20      the subject digest to the bundle file bytes\n\
         chain: print the whole-chain statement JSON on stdout; with --out write an\n\
         \x20      unsigned DSSE envelope (base64 canonical payload) to the file"
    );
}

#[cfg(test)]
mod tests {
    use super::{dispatch, CHAIN_USAGE, TASK_USAGE};
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "pf-attest-bin-{tag}-{}-{n}.tmp",
            std::process::id()
        ))
    }

    fn write_temp(tag: &str, content: &str) -> PathBuf {
        let path = temp_path(tag);
        std::fs::write(&path, content).expect("write temp file");
        path
    }

    const SAMPLE_LEDGER: &str = concat!(
        r#"{"seq":0,"prev_hash":"","kind":"ModelClaim","payload":{"task_id":"t1","commit_sha":"abc123def456","state":"ModelClaimed"},"hash":"aa11","env_fingerprint":"cli"}"#,
        "\n",
        r#"{"seq":1,"prev_hash":"aa11","kind":"ToolAttestation","payload":{"task_id":"t1","commit_sha":"abc123def456","state":"Verified"},"hash":"bb22","env_fingerprint":"cli"}"#,
        "\n"
    );

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_args_print_usage_and_exit_2() {
        assert_eq!(dispatch(&[]), Ok(ExitCode::from(2)));
    }

    #[test]
    fn help_variants_exit_2_with_usage() {
        for flag in ["--help", "-h", "help"] {
            assert_eq!(dispatch(&args(&[flag])), Ok(ExitCode::from(2)));
        }
    }

    #[test]
    fn unknown_command_is_an_error() {
        let err = dispatch(&args(&["bogus"])).expect_err("must fail");
        assert!(err.contains("unknown command: bogus"), "{err}");
    }

    #[test]
    fn unknown_flag_is_an_error() {
        let err = dispatch(&args(&["task", "--wat", "x"])).expect_err("must fail");
        assert_eq!(err, "unknown flag: --wat");
    }

    #[test]
    fn missing_flag_value_is_an_error() {
        let err = dispatch(&args(&["task", "--ledger"])).expect_err("must fail");
        assert_eq!(err, "--ledger requires a value");
    }

    #[test]
    fn task_without_required_flags_reports_usage() {
        let err = dispatch(&args(&["task"])).expect_err("must fail");
        assert_eq!(err, TASK_USAGE);
        let err = dispatch(&args(&["task", "--ledger", "x.jsonl"])).expect_err("must fail");
        assert_eq!(err, TASK_USAGE);
    }

    #[test]
    fn chain_without_ledger_reports_usage() {
        let err = dispatch(&args(&["chain"])).expect_err("must fail");
        assert_eq!(err, CHAIN_USAGE);
    }

    #[test]
    fn task_happy_path_succeeds_against_temp_ledger() {
        let path = write_temp("task-ok", SAMPLE_LEDGER);
        let ledger = path.to_str().expect("utf8 path").to_string();
        let result = dispatch(&args(&["task", "--ledger", &ledger, "--task", "t1"]));
        assert_eq!(result, Ok(ExitCode::SUCCESS));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn task_unknown_task_id_fails_closed() {
        let path = write_temp("task-missing", SAMPLE_LEDGER);
        let ledger = path.to_str().expect("utf8 path").to_string();
        let err = dispatch(&args(&["task", "--ledger", &ledger, "--task", "nope"]))
            .expect_err("must fail");
        assert!(err.contains("no ledger entries for task 'nope'"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chain_happy_path_writes_unsigned_dsse_envelope() {
        let path = write_temp("chain-ok", SAMPLE_LEDGER);
        let out_path = temp_path("chain-out");
        let ledger = path.to_str().expect("utf8 path").to_string();
        let out = out_path.to_str().expect("utf8 out path").to_string();
        let result = dispatch(&args(&["chain", "--ledger", &ledger, "--out", &out]));
        assert_eq!(result, Ok(ExitCode::SUCCESS));

        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let raw = std::fs::read_to_string(&out_path).expect("read envelope");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("envelope parses");
        assert_eq!(
            v["payloadType"], "application/vnd.in-toto+json",
            "payloadType must be pinned"
        );
        assert_eq!(v["signatures"], serde_json::json!([]), "unsigned envelope");
        let decoded = STANDARD
            .decode(v["payload"].as_str().expect("payload string"))
            .expect("base64 decodes");
        let stmt: serde_json::Value = serde_json::from_slice(&decoded).expect("statement parses");
        assert_eq!(
            stmt["_type"], "https://in-toto.io/Statement/v1",
            "payload must be the in-toto statement"
        );

        // Determinism: a second run produces byte-identical output.
        let raw2 = {
            let _ = dispatch(&args(&["chain", "--ledger", &ledger, "--out", &out]));
            std::fs::read_to_string(&out_path).expect("read envelope again")
        };
        assert_eq!(raw, raw2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn chain_empty_ledger_hashes_the_empty_input() {
        let path = write_temp("chain-empty", "");
        let ledger = path.to_str().expect("utf8 path").to_string();
        let result = dispatch(&args(&["chain", "--ledger", &ledger]));
        assert_eq!(result, Ok(ExitCode::SUCCESS));
        let _ = std::fs::remove_file(&path);
    }
}
