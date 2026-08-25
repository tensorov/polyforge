//! CLI entry point for the optional Langfuse bridge (non-trust analytics tool).

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: langfuse-bridge <gate-manifest.json> [--dry-run]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest_path: Option<PathBuf> = None;
    let mut dry_run = false;
    for arg in &args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                println!("env: LF_BASE_URL, LF_PK, LF_SK");
                return ExitCode::SUCCESS;
            }
            other => {
                if manifest_path.is_some() {
                    eprintln!("error: unexpected extra argument: {other}");
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
                manifest_path = Some(PathBuf::from(other));
            }
        }
    }
    let Some(path) = manifest_path else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    match langfuse_bridge::prepare(&path) {
        Ok(langfuse_bridge::PrepareOutcome::Skip { warning }) => {
            eprintln!("{warning}");
            ExitCode::SUCCESS
        }
        Ok(langfuse_bridge::PrepareOutcome::Proceed { manifest, trace_id }) => {
            let payload = langfuse_bridge::score_payload(&manifest, &trace_id);
            if dry_run {
                println!("{payload}");
                ExitCode::SUCCESS
            } else {
                post(&manifest, &trace_id, &payload)
            }
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn require_env(name: &str) -> Result<String, ()> {
    std::env::var(name).map_err(|_| {
        eprintln!("error: {name} is not set");
    })
}

fn post(
    manifest: &langfuse_bridge::GateManifest,
    trace_id: &str,
    payload: &str,
) -> ExitCode {
    let (base_url, public_key, secret_key) =
        match (require_env("LF_BASE_URL"), require_env("LF_PK"), require_env("LF_SK")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (_, _, _) => return ExitCode::from(2),
        };
    match langfuse_bridge::post_score(&base_url, &public_key, &secret_key, payload) {
        Ok(()) => {
            println!(
                "posted gate score for task {} (trace {trace_id})",
                manifest.task_id
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
