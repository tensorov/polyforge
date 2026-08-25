//! T10: mock sandbox execution backend behind the `sandbox-mock` feature.
//!
//! A test double proving the [`Executor`] seam: it models isolation
//! semantics cheaply (fresh per-run working directory, scrubbed child
//! environment) without any container, VM, or network machinery. The live
//! microVM adapter is a follow-up scope decision recorded in the plan.
//!
//! # Isolation model
//!
//! Each run gets a fresh empty directory under [`std::env::temp_dir`] as
//! the child's cwd, and the child environment is CLEARED then repopulated
//! with a minimal allowlisted set (PATH only). The allowlist + typed-arg
//! policy from [`crate::runner`] still gates what may execute; the wall
//! clock budget and process-group kill are inherited unchanged.
//!
//! # Trust boundary
//!
//! This is a MOCK: it does not provide real isolation guarantees. It exists
//! so the executor selection seam, digest plumbing, and downstream
//! attestation metadata can be exercised end to end before a real sandbox
//! backend lands. Attestations produced through it carry
//! `eval_metadata.executor_digest` so operators can tell which backend ran.
//!
//! Network freedom is structural: the module uses only `std::process`,
//! `std::fs`, `std::env`, `std::path`, and `std::time`. No socket types,
//! no HTTP crates, no async runtime.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use super::runner::{
    command_string, exit_code_of, lookup, parse_timeout, sha256_hex, tool_version,
    validate_tool_args, wait_with_timeout, Executor, RunOutput, RunnerError, Tool,
};

/// Stub image identity the mock pretends to boot. The executor digest is
/// derived from this fixed string so attestations name a stable backend
/// version instead of a machine-specific path.
pub const MOCK_IMAGE_ID: &str = "mock-sandbox-image-v1";

/// Fixed executor identity: first 16 hex chars of SHA-256 over
/// [`MOCK_IMAGE_ID`]. Byte-stable across calls, processes, and machines.
pub fn executor_digest() -> String {
    sha256_hex(MOCK_IMAGE_ID.as_bytes())[..16].to_string()
}

/// The mock sandbox backend: runs the canonical allowlisted binary directly
/// (no shell) inside a fresh temp cwd with a scrubbed environment.
pub struct MockSandboxExecutor;

impl Executor for MockSandboxExecutor {
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
        let canonical =
            lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
        validate_tool_args(&canonical.name, args)?;

        let workdir = fresh_workdir()?;
        let mut all_args = canonical.args.clone();
        all_args.extend(args.iter().cloned());
        let mut cmd = scrubbed_command(&canonical.bin, &all_args, &workdir);
        let spawned = cmd.spawn();
        if let Err(e) = spawned {
            let _ = std::fs::remove_dir_all(&workdir);
            return Err(RunnerError::Spawn(e.to_string()));
        }
        let result = wait_with_timeout(spawned.unwrap(), parse_timeout());
        let _ = std::fs::remove_dir_all(&workdir);
        let out = result?;

        let exit_code = exit_code_of(out.status);
        let stdout_hash = sha256_hex(&out.stdout);
        let version = tool_version(&canonical.bin);
        Ok(RunOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code,
            stdout_hash,
            // Same fingerprint formula as the process backend: it identifies
            // the host toolchain context that resolved and executed the
            // binary, which is unchanged by the mock's cwd/env pinning.
            env_fingerprint: super::runner::env_fingerprint(&version),
            tool_version: version,
            command: command_string(&canonical, args),
        })
    }

    fn label(&self) -> &'static str {
        "sandbox-mock"
    }
}

/// Fresh empty working directory for one run:
/// `<temp_dir>/pf-sandbox-mock-<pid>-<nanos>`.
fn fresh_workdir() -> Result<PathBuf, RunnerError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pf-sandbox-mock-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| RunnerError::Io(e.to_string()))?;
    Ok(dir)
}

/// Build the sandboxed command: direct binary spawn (no shell), cwd pinned
/// to `workdir`, environment cleared then repopulated with the minimal
/// allowlisted set (PATH only), piped output, own process group on Unix so
/// the shared watchdog can kill the whole tree on timeout.
fn scrubbed_command(bin: &Path, args: &[String], workdir: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.current_dir(workdir);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Output;
    use std::time::Duration;

    /// Drive the real scrubbed-spawn path with an arbitrary binary (the
    /// Executor impl restricts itself to the allowlist; these tests target
    /// the isolation mechanics themselves).
    fn run_scrubbed(bin: &str, args: &[&str]) -> Output {
        let workdir = fresh_workdir().expect("workdir created");
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut cmd = scrubbed_command(Path::new(bin), &owned, &workdir);
        let child = cmd.spawn().expect("child spawned");
        let out = wait_with_timeout(child, Duration::from_secs(30)).expect("child finished");
        let _ = std::fs::remove_dir_all(&workdir);
        out
    }

    /// QA happy path from the plan: a sentinel variable must be invisible
    /// to the scrubbed child, while PATH survives. The sentinel is
    /// CARGO_MANIFEST_DIR, which the cargo-test process always carries:
    /// reading an existing var avoids mutating the shared process
    /// environment (setenv races with parallel env readers). The full
    /// printenv dump additionally proves the repopulated set is exactly
    /// the allowlist (PATH only), which subsumes any single-sentinel check.
    #[test]
    fn sentinel_env_var_is_hidden_from_child() {
        let hidden = run_scrubbed("/bin/sh", &["-c", "printenv CARGO_MANIFEST_DIR"]);
        assert_eq!(
            String::from_utf8_lossy(&hidden.stdout).trim(),
            "",
            "sentinel var must be scrubbed from the child env"
        );
        let path_kept = run_scrubbed("/bin/sh", &["-c", "printenv PATH"]);
        assert!(
            !String::from_utf8_lossy(&path_kept.stdout).trim().is_empty(),
            "allowlisted PATH must be repopulated"
        );
        // Full-dump proof driven through printenv DIRECTLY (no shell): a
        // shell would inject its own runtime vars (PWD, SHLVL, _) into the
        // child environment nondeterministically, which would say nothing
        // about our scrubbing. With printenv as the direct child the
        // environment is byte-for-byte what scrubbed_command built.
        let dump = run_scrubbed("/usr/bin/printenv", &[]);
        let lines: Vec<String> = String::from_utf8_lossy(&dump.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 1, "child env must hold exactly one var");
        assert!(
            lines[0].starts_with("PATH="),
            "child env must be the PATH-only allowlist, got: {}",
            lines[0]
        );
    }

    #[test]
    fn child_cwd_is_fresh_temp_dir_under_temp_root() {
        let pwd = run_scrubbed("/bin/sh", &["-c", "pwd"]);
        let cwd = String::from_utf8_lossy(&pwd.stdout).trim().to_string();
        let root = std::env::temp_dir();
        let pinned = PathBuf::from(&cwd);
        assert!(
            pinned.starts_with(&root),
            "cwd must live under temp_dir(): {cwd}"
        );
        assert_ne!(
            pinned,
            std::env::current_dir().unwrap(),
            "cwd must not be the parent process cwd"
        );
        assert!(cwd.contains("pf-sandbox-mock"), "marker dir name: {cwd}");
    }

    #[test]
    fn failing_child_propagates_exit_code_unchanged() {
        let out = run_scrubbed("/bin/false", &[]);
        assert_eq!(out.status.code(), Some(1));
    }

    #[test]
    fn digest_is_fixed_sha256_prefix_of_stub_image() {
        let d = executor_digest();
        assert_eq!(d.len(), 16);
        assert!(d.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(d, sha256_hex(MOCK_IMAGE_ID.as_bytes())[..16]);
        assert_eq!(d, executor_digest());
    }

    /// Happy path through the trait: allowlisted tool runs in the mock and
    /// produces the full RunOutput shape.
    #[test]
    fn allowlisted_tool_runs_happy_in_mock() {
        let t = lookup("cargo --version").expect("tool on allowlist");
        let out = MockSandboxExecutor.run(&t, &[]).expect("mock run");
        assert_eq!(out.exit_code, 0);
        assert!(String::from_utf8_lossy(&out.stdout).contains("cargo"));
        assert_eq!(out.stdout_hash.len(), 64);
        assert!(!out.tool_version.is_empty());
        assert!(!out.env_fingerprint.is_empty());
        assert!(out.command.starts_with("cargo"));
    }

    /// QA failure path from the plan: a tool failing inside the mock
    /// propagates its non-zero outcome unchanged (Ok(RunOutput), real code).
    #[test]
    fn failing_tool_propagates_non_zero_unchanged() {
        let t = lookup("cargo build").expect("tool on allowlist");
        let out = MockSandboxExecutor
            .run(&t, &["--definitely-not-a-flag".to_string()])
            .expect("run completes even when the tool fails");
        assert_ne!(out.exit_code, 0);
        assert!(!out.stderr.is_empty());
    }

    /// The allowlist gate stays active in the mock: unknown names are
    /// rejected before anything spawns.
    #[test]
    fn mock_rejects_unallowlisted_tool() {
        let evil = Tool {
            name: "evil".into(),
            bin: PathBuf::from("evil"),
            args: vec![],
        };
        let err = MockSandboxExecutor.run(&evil, &[]).unwrap_err();
        assert!(matches!(err, RunnerError::NotAllowed(n) if n == "evil"));
    }
}
