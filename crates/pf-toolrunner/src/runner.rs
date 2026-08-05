//! T5: allowlist + typed-args tool runner (no shell).
//!
//! The only way evidence becomes Verified is an allowlisted tool running with
//! typed args and its attestation being appended. No shell interpolation
//! anywhere: we spawn the binary directly via [`std::process::Command`], never
//! `sh -c` / `bash -c` / `/bin/sh`.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use pf_core::evidence::EvidenceEntry;
use sha2::{Digest, Sha256};

/// A tool on the allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// Canonical allowlist name, e.g. `"cargo --version"`.
    pub name: String,
    /// Absolute or PATH-resolved binary to spawn directly (no shell).
    pub bin: PathBuf,
    /// The tool's fixed argument vector (e.g. `["--version"]`).
    pub args: Vec<String>,
}

/// Output of a completed tool run, plus the fields needed to build a
/// `ToolAttestation` (state `Verified`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub stdout_hash: String,
    pub env_fingerprint: String,
    pub tool_version: String,
    pub command: String,
}

impl RunOutput {
    /// Build a `ToolAttestation` (state `Verified`) ready for `pf_core::promote`.
    pub fn to_attestation(
        &self,
        task_id: impl Into<String>,
        commit_sha: impl Into<String>,
        diff_hash: impl Into<String>,
        ts: impl Into<String>,
    ) -> EvidenceEntry {
        EvidenceEntry::tool_attestation(
            task_id,
            commit_sha,
            diff_hash,
            self.tool_version.clone(),
            self.env_fingerprint.clone(),
            self.command.clone(),
            self.exit_code,
            self.stdout_hash.clone(),
            ts,
        )
    }
}

/// Errors produced by the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// The tool name is not on the allowlist.
    NotAllowed(String),
    /// An argument contains a shell metacharacter (shape violation).
    InvalidArg { tool: String, arg: String },
    /// The binary could not be spawned.
    Spawn(String),
    /// Reading the child output failed.
    Io(String),
    /// No `ModelClaimed` entry exists at the requested ledger sequence (or it
    /// is not a claim for this task).
    ClaimNotFound(u64),
    /// The tool ran but exited non-zero; nothing was promoted or appended.
    ToolFailed { exit_code: i32, stderr: String },
    /// The tri-state promotion was rejected by the state machine.
    Promote(String),
    /// A ledger read / integrity / append operation failed.
    Ledger(String),
}

/// The v1 allowlist. Add more tools here as the project grows.
pub fn allowlist() -> Vec<Tool> {
    vec![
        Tool {
            name: "cargo build".into(),
            bin: PathBuf::from("cargo"),
            args: vec!["build".into()],
        },
        Tool {
            name: "cargo test".into(),
            bin: PathBuf::from("cargo"),
            args: vec!["test".into()],
        },
        Tool {
            name: "cargo --version".into(),
            bin: PathBuf::from("cargo"),
            args: vec!["--version".into()],
        },
        Tool {
            name: "rustc --version".into(),
            bin: PathBuf::from("rustc"),
            args: vec!["--version".into()],
        },
        Tool {
            name: "gcc -v".into(),
            bin: PathBuf::from("gcc"),
            args: vec!["-v".into()],
        },
    ]
}

/// Look up a tool by name in the allowlist.
pub fn lookup(name: &str) -> Option<Tool> {
    allowlist().into_iter().find(|t| t.name == name)
}

/// Spawn an allowlisted tool with typed args, no shell. Returns the live child
/// with piped stdio so callers can inspect `/proc/<pid>/cmdline` while it runs.
pub fn spawn(tool: &Tool, args: &[String]) -> Result<Child, RunnerError> {
    if lookup(&tool.name).is_none() {
        return Err(RunnerError::NotAllowed(tool.name.clone()));
    }
    for a in args {
        validate_arg(&tool.name, a)?;
    }
    let mut cmd = Command::new(&tool.bin);
    cmd.args(&tool.args).args(args);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.spawn().map_err(|e| RunnerError::Spawn(e.to_string()))
}

/// Run an allowlisted tool to completion and capture its output + attestation
/// fields.
pub fn run(tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
    let child = spawn(tool, args)?;
    let out = child
        .wait_with_output()
        .map_err(|e| RunnerError::Io(e.to_string()))?;
    let exit_code = out.status.code().unwrap_or(-1);
    let stdout_hash = sha256_hex(&out.stdout);
    let tool_version = tool_version(&tool.bin);
    let env_fingerprint = env_fingerprint(&tool_version);
    let command = command_string(tool, args);
    Ok(RunOutput {
        stdout: out.stdout,
        stderr: out.stderr,
        exit_code,
        stdout_hash,
        env_fingerprint,
        tool_version,
        command,
    })
}

/// Reject shell metacharacters in typed args. This is a shape check, not
/// escaping: we never pass through a shell, so a space is fine but command
/// substitution / redirection / backgrounding are not.
fn validate_arg(tool: &str, arg: &str) -> Result<(), RunnerError> {
    const META: &[char] = &[';', '|', '&', '`', '$', '>', '<', '\n', '\0'];
    if arg.chars().any(|c| META.contains(&c)) {
        return Err(RunnerError::InvalidArg {
            tool: tool.to_string(),
            arg: arg.to_string(),
        });
    }
    Ok(())
}

/// Stable SHA-256 over tool version + os + arch + sorted env var names + PATH.
/// Changes when the tool version or PATH changes; stable across identical runs.
pub fn env_fingerprint(tool_version: &str) -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let mut names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    names.sort();
    let path = std::env::var("PATH").unwrap_or_default();
    let mut h = Sha256::new();
    h.update(tool_version.as_bytes());
    h.update(os.as_bytes());
    h.update(arch.as_bytes());
    for n in &names {
        h.update(n.as_bytes());
        h.update(b"\0");
    }
    h.update(path.as_bytes());
    hex(&h.finalize())
}

/// Resolve a tool's version by running `<bin> --version`.
fn tool_version(bin: &PathBuf) -> String {
    match Command::new(bin).arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => format!("unknown-{}", bin.display()),
    }
}

fn command_string(tool: &Tool, args: &[String]) -> String {
    let mut parts = vec![tool.bin.display().to_string()];
    parts.extend(tool.args.iter().cloned());
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::evidence::{EvidenceKind, EvidenceState};

    fn tool(name: &str) -> Tool {
        lookup(name).expect("tool on allowlist")
    }

    #[test]
    fn test_allowlist_rejects_unknown_tool() {
        let evil = Tool {
            name: "evil".into(),
            bin: PathBuf::from("evil"),
            args: vec![],
        };
        let err = run(&evil, &[]).unwrap_err();
        assert!(matches!(err, RunnerError::NotAllowed(n) if n == "evil"));
    }

    #[test]
    fn test_args_are_passed_verbatim() {
        let t = tool("cargo --version");
        let out = run(&t, &["some arg".to_string()]).unwrap();
        // The command string is built as `bin args...` joined by space, so a
        // space inside a typed arg is preserved verbatim (no shell split).
        assert!(out.command.ends_with("some arg"), "arg not verbatim: {}", out.command);
        assert!(out.command.starts_with("cargo"));
        assert!(out.exit_code == 0);
    }

    #[test]
    fn test_no_shell_used() {
        let t = tool("cargo --version");
        let out = run(&t, &[]).unwrap();
        // argv[0] is the allowlisted bin (cargo), never a shell.
        assert!(out.command.starts_with("cargo"), "no shell in command: {}", out.command);
        assert!(!out.command.contains("sh -c"));
        assert!(out.exit_code == 0);
    }

    #[test]
    fn test_attestation_has_tool_version_and_fingerprint() {
        let t = tool("cargo --version");
        let out = run(&t, &[]).unwrap();
        assert!(!out.tool_version.is_empty());
        assert!(!out.env_fingerprint.is_empty());
        let att = out.to_attestation("T5", "abc", "diff", "ts");
        assert_eq!(att.kind, EvidenceKind::ToolAttestation);
        assert_eq!(att.state, EvidenceState::Verified);
        assert_eq!(att.exit_code, 0);
        assert_eq!(att.stdout_hash.len(), 64);
    }

    #[test]
    fn test_env_fingerprint_changes_with_tool_version() {
        let a = env_fingerprint("cargo-1.95.0");
        let b = env_fingerprint("cargo-1.96.0");
        assert_ne!(a, b);
    }

    #[test]
    fn test_env_fingerprint_stable_across_runs() {
        let a = env_fingerprint("cargo-1.95.0");
        let b = env_fingerprint("cargo-1.95.0");
        assert_eq!(a, b);
    }
}