//! T5: allowlist + typed-args tool runner (no shell).
//!
//! The only way evidence becomes Verified is an allowlisted tool running with
//! typed args and its attestation being appended. No shell interpolation
//! anywhere: we spawn the binary directly via [`std::process::Command`], never
//! `sh -c` / `bash -c` / `/bin/sh`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use polyforge_core::evidence::EvidenceEntry;
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
    /// Build a `ToolAttestation` (state `Verified`) ready for `polyforge_core::promote`.
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
    /// The tool did not finish within the configured wall-clock budget; its
    /// process group was killed and its output discarded.
    TimedOut { timeout_secs: u64 },
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

/// Default wall-clock budget for a tool run, in seconds.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 3600;

/// Environment variable overriding the default tool timeout (seconds).
pub const PF_TOOL_TIMEOUT_SECS: &str = "PF_TOOL_TIMEOUT_SECS";

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
        // Mutation testing: `cargo-mutants` (install: `cargo install
        // cargo-mutants --locked`, v27.1.0). CI installs it via
        // `taiki-e/install-action@v2` with `tool: cargo-mutants`. Typed args
        // only — e.g. `--version` — passed through `run`/`spawn`; no shell.
        Tool {
            name: "cargo-mutants".into(),
            bin: PathBuf::from("cargo-mutants"),
            args: vec![],
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The tool leads its own process group so a wall-clock timeout can kill
    // the whole tree (grandchildren holding the output pipes included).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| RunnerError::Spawn(e.to_string()))
}

/// Run an allowlisted tool to completion and capture its output + attestation
/// fields, bounded by the `PF_TOOL_TIMEOUT_SECS` wall-clock budget (default
/// [`DEFAULT_TOOL_TIMEOUT_SECS`]). A tool that outlives the budget is killed
/// together with its process group and the run fails with
/// [`RunnerError::TimedOut`].
pub fn run(tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
    run_with_timeout(tool, args, parse_timeout())
}

/// Like [`run`], but with an explicit wall-clock budget.
pub fn run_with_timeout(
    tool: &Tool,
    args: &[String],
    timeout: Duration,
) -> Result<RunOutput, RunnerError> {
    let child = spawn(tool, args)?;
    let out = wait_with_timeout(child, timeout)?;
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

/// Wait for a spawned child to finish, but no longer than `timeout`. A
/// watchdog thread sleeps for the budget and, on expiry, SIGKILLs the child's
/// whole process group so grandchildren holding the output pipes cannot keep
/// the caller blocked past the deadline. Returns [`RunnerError::TimedOut`]
/// when the budget was exceeded; the child's output is discarded in that case.
pub fn wait_with_timeout(child: Child, timeout: Duration) -> Result<Output, RunnerError> {
    let pid = child.id();
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&timed_out);
    let (tx, rx) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || match rx.recv_timeout(timeout) {
        // The child finished first; the watchdog exits without killing.
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            flag.store(true, Ordering::SeqCst);
            kill_process_group(pid);
        }
    });
    let out = child
        .wait_with_output()
        .map_err(|e| RunnerError::Io(e.to_string()))?;
    // Cancel the watchdog so it never kills a recycled pid after a normal exit.
    let _ = tx.send(());
    let _ = watchdog.join();
    if timed_out.load(Ordering::SeqCst) {
        return Err(RunnerError::TimedOut {
            timeout_secs: timeout.as_secs(),
        });
    }
    Ok(out)
}

/// Read the `PF_TOOL_TIMEOUT_SECS` wall-clock budget (seconds). Missing,
/// unparsable, or non-positive values fall back to
/// [`DEFAULT_TOOL_TIMEOUT_SECS`].
pub fn parse_timeout() -> Duration {
    parse_timeout_from(std::env::var(PF_TOOL_TIMEOUT_SECS).ok().as_deref())
}

/// Pure core of [`parse_timeout`]: `None` or an invalid value yields the
/// default; a positive integer yields that many seconds.
fn parse_timeout_from(value: Option<&str>) -> Duration {
    match value.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS),
    }
}

/// SIGKILL the process group led by `pid` (the child spawned with
/// `process_group(0)`). Errors are ignored: the only goal is to release the
/// output pipes so the blocked `wait_with_output` returns.
///
/// The function is deliberately NOT cfg-split: on Unix it runs a real
/// `killpg`, on every other platform it falls through to an unconditional
/// `/bin/kill -9` subprocess, whose failure it ignores. The structural reason
/// is mutation-testing observability (commit 519ae8b lesson): a statement
/// gated behind `cfg(not(unix))` never compiles on Linux CI, so its mutant is
/// never built, never run, and registers as MISSED, which fails the exact
/// `cargo-mutants --in-diff` gate this repo enforces. An always-compiled
/// executable fallback line IS compiled on Linux, its mutant IS runnable, and
/// the mutant guard test below kills it. On Unix the extra `kill -9` of a
/// group leader we already `killpg`'d is a harmless ESRCH no-op.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `pid` came from a live Child we spawned into its own process
        // group; killpg takes a pid_t and a signal. Failure (ESRCH etc.) is
        // ignored by design.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    // Always-compiled fallback so its mutant is runnable on Linux CI (see doc
    // comment). Best-effort: any spawn or kill failure is ignored by design.
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
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

/// Build-relevant environment variables whose VALUES are folded into the
/// fingerprint (in addition to the deterministic name-set). Only these names
/// are ever read from the process environment; arbitrary or secret variables
/// are never included. Each is folded only when set.
const ENV_FINGERPRINT_ALLOWLIST: &[&str] = &[
    "RUSTFLAGS",
    "RUSTC_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "CC",
    "CXX",
    "TARGET",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "RUSTDOCFLAGS",
];

/// Stable fingerprint over tool version + os + arch + sorted env var names +
/// PATH, with a fixed-order self-describing tail that folds in Nix/Devbox
/// identity and lockfile hashes when present:
///
/// `<base-hex>|nix=<none|digest>|devbox=<none|sha256>|cargo.lock=<sha256>`
///
/// `base-hex` is the pre-C2.1 formula unchanged (SHA-256 over tool version +
/// os + arch + sorted env var names + PATH). The tail is appended verbatim in
/// fixed order; the `cargo.lock` section is unconditional. Changes when the
/// tool version, PATH, Nix store paths, `devbox.lock`, or the repo-root
/// `Cargo.lock` change; byte-stable across identical runs.
pub fn env_fingerprint(tool_version: &str) -> String {
    let names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let path = std::env::var("PATH").unwrap_or_default();
    let nix_segments = nix_segments_from_path(&path);
    let root = repo_root();
    // Both lockfiles are read relative to the repo root (walked up from
    // CARGO_MANIFEST_DIR); a missing/unreadable file contributes `none`.
    let devbox_bytes = root
        .as_deref()
        .and_then(|dir| std::fs::read(dir.join("devbox.lock")).ok());
    let cargo_lock_bytes = root
        .as_deref()
        .and_then(|dir| std::fs::read(dir.join("Cargo.lock")).ok());
    let allowlist: Vec<(&str, String)> = ENV_FINGERPRINT_ALLOWLIST
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (*name, value)))
        .collect();
    let allowlist_refs: Vec<(&str, &str)> = allowlist
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    env_fingerprint_from_pairs(
        tool_version,
        &names,
        &path,
        &nix_segments,
        devbox_bytes.as_deref(),
        cargo_lock_bytes.as_deref(),
        &allowlist_refs,
    )
}

/// Pure core of [`env_fingerprint`]: identical inputs produce byte-identical
/// output. os/arch come from compile-time constants only; every environment
/// and lockfile input is passed in so tests can drive synthetic vectors
/// without touching the real environment or filesystem. `allowlist_pairs` are
/// the (name, value) pairs of the curated build-env allowlist, folded in
/// sorted order.
fn env_fingerprint_from_pairs(
    tool_version: &str,
    env_names: &[String],
    path: &str,
    nix_segments: &[String],
    devbox_bytes: Option<&[u8]>,
    cargo_lock_bytes: Option<&[u8]>,
    allowlist_pairs: &[(&str, &str)],
) -> String {
    let mut names = env_names.to_vec();
    names.sort();
    let mut pairs = allowlist_pairs.to_vec();
    pairs.sort();
    let mut h = Sha256::new();
    h.update(tool_version.as_bytes());
    h.update(std::env::consts::OS.as_bytes());
    h.update(std::env::consts::ARCH.as_bytes());
    for n in &names {
        h.update(n.as_bytes());
        h.update(b"\0");
    }
    h.update(path.as_bytes());
    for (name, value) in &pairs {
        h.update(name.as_bytes());
        h.update(b"=");
        h.update(value.as_bytes());
        h.update(b"\0");
    }
    let base = hex(&h.finalize());
    format!(
        "{base}|nix={}|devbox={}|cargo.lock={}",
        nix_digest(nix_segments),
        lock_section(devbox_bytes),
        lock_section(cargo_lock_bytes),
    )
}

/// `none` when no `/nix/store/` segments are present; otherwise the SHA-256 of
/// the lexicographically-sorted segments joined with `|`. Sorting makes the
/// digest independent of PATH order; a single-segment PATH behaves identically
/// to the multi-segment algorithm on a one-element list.
fn nix_digest(segments: &[String]) -> String {
    if segments.is_empty() {
        return "none".into();
    }
    let mut sorted = segments.to_vec();
    sorted.sort();
    sha256_hex(sorted.join("|").as_bytes())
}

/// Every `/nix/store/<32-char-Nix32>-<name>` segment of a PATH-style string
/// (colon-separated). The 32-char hash field is validated against the Nix32
/// charset (base-32: lowercase a-z + 0-9, minus e/o/u/t); the name is whatever
/// follows the mandatory `-` and must be non-empty. Any segment failing the
/// shape is ignored — this is a pure PATH parse, no subprocess.
fn nix_segments_from_path(path: &str) -> Vec<String> {
    path.split(':')
        .filter(|seg| {
            let Some(rest) = seg.strip_prefix("/nix/store/") else {
                return false;
            };
            let Some((hash, name)) = rest.split_once('-') else {
                return false;
            };
            hash.len() == 32 && !name.is_empty() && hash.bytes().all(is_nix32)
        })
        .map(str::to_owned)
        .collect()
}

/// Nix32 charset membership: base-32 (digits `0-9` plus lowercase `a-z`) with
/// `e`, `o`, `u`, `t` excluded.
fn is_nix32(c: u8) -> bool {
    (c.is_ascii_digit() || c.is_ascii_lowercase()) && !matches!(c, b'e' | b'o' | b'u' | b't')
}

/// Literal `none` for an absent lockfile, else its SHA-256.
fn lock_section(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(b) => sha256_hex(b),
        None => "none".into(),
    }
}

/// Locate the workspace root: walk up from `CARGO_MANIFEST_DIR` (falling back
/// to the current directory) to the nearest ancestor whose `Cargo.toml` carries
/// a `[workspace]` section. Both lockfiles are read relative to it.
/// `CARGO_MANIFEST_DIR` is preferred over the process cwd because it is stable
/// regardless of where the caller invokes the runner.
fn repo_root() -> Option<PathBuf> {
    let start = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())?;
    repo_root_from(&start)
}

/// Walk up from `start` to the nearest ancestor whose `Cargo.toml` carries a
/// `[workspace]` section.
fn repo_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            let manifest = dir.join("Cargo.toml");
            std::fs::read_to_string(manifest)
                .map(|text| text.contains("[workspace]"))
                .unwrap_or(false)
        })
        .map(PathBuf::from)
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

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
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
    use polyforge_core::evidence::{EvidenceKind, EvidenceState};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        assert!(
            out.command.ends_with("some arg"),
            "arg not verbatim: {}",
            out.command
        );
        assert!(out.command.starts_with("cargo"));
        assert!(out.exit_code == 0);
    }

    #[test]
    fn test_no_shell_used() {
        let t = tool("cargo --version");
        let out = run(&t, &[]).unwrap();
        // argv[0] is the allowlisted bin (cargo), never a shell.
        assert!(
            out.command.starts_with("cargo"),
            "no shell in command: {}",
            out.command
        );
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

    /// Synthetic-input helper for the pure core: identical args → identical
    /// fingerprint, with no real environment or filesystem access.
    fn fp(
        env_names: Vec<&str>,
        path: &str,
        nix: &[String],
        devbox: Option<&[u8]>,
        cargo: Option<&[u8]>,
    ) -> String {
        env_fingerprint_from_pairs(
            "cargo-1.95.0",
            &env_names.into_iter().map(String::from).collect::<Vec<_>>(),
            path,
            nix,
            devbox,
            cargo,
            &[],
        )
    }

    /// RED-first: two synthetic env sets with identical names but different
    /// RUSTFLAGS values must produce different fingerprints. This drives the
    /// value-folding of the allowlist through the pure core.
    #[test]
    fn test_env_fingerprint_folds_allowlist_values() {
        let base = env_fingerprint_from_pairs(
            "cargo-1.95.0",
            &[],
            "/usr/bin",
            &[],
            None,
            None,
            &[("RUSTFLAGS", "-Ctarget-cpu=native")],
        );
        let other = env_fingerprint_from_pairs(
            "cargo-1.95.0",
            &[],
            "/usr/bin",
            &[],
            None,
            None,
            &[("RUSTFLAGS", "-Copt-level=3")],
        );
        assert_ne!(
            base, other,
            "different RUSTFLAGS values must fold into different fingerprints"
        );
    }

    #[test]
    fn test_env_fingerprint_allowlist_values_deterministic() {
        let pairs = &[("RUSTFLAGS", "-Ctarget-cpu=native"), ("CC", "clang")];
        let a = env_fingerprint_from_pairs("cargo-1.95.0", &[], "/usr/bin", &[], None, None, pairs);
        let b = env_fingerprint_from_pairs("cargo-1.95.0", &[], "/usr/bin", &[], None, None, pairs);
        assert_eq!(a, b, "identical allowlist pairs must hash identically");
    }

    const NIX32_HASH: &str = "00000000000000000000000000000000";

    fn nix_seg(name: &str) -> String {
        format!("/nix/store/{NIX32_HASH}-{name}")
    }

    #[test]
    fn test_fingerprint_changes_when_nix_devbox_cargo_added() {
        let baseline = fp(
            vec![],
            "/usr/bin",
            &[],
            Some(b"devbox-lock-v1"),
            Some(b"cargo-lock-v1"),
        );
        let seg = nix_seg("cargo-1.95.0");
        let extended = fp(
            vec![],
            &format!("/usr/bin:{}", nix_seg("other")),
            &[seg],
            Some(b"devbox-lock-v2"),
            Some(b"cargo-lock-v2"),
        );
        assert_ne!(
            baseline, extended,
            "nix+devbox+cargo change must alter the fingerprint"
        );
    }

    #[test]
    fn test_fingerprint_identical_for_identical_inputs() {
        let seg = nix_seg("a");
        let a = fp(
            vec!["CARGO_HOME", "RUSTUP_HOME"],
            &format!("/usr/bin:{}", nix_seg("a")),
            std::slice::from_ref(&seg),
            Some(b"devbox-lock"),
            Some(b"cargo-lock"),
        );
        let b = fp(
            vec!["CARGO_HOME", "RUSTUP_HOME"],
            &format!("/usr/bin:{}", nix_seg("a")),
            std::slice::from_ref(&seg),
            Some(b"devbox-lock"),
            Some(b"cargo-lock"),
        );
        assert_eq!(
            a, b,
            "identical inputs must produce byte-equal fingerprints"
        );
    }

    #[test]
    fn test_absent_nix_devbox_render_none_token_and_stay_byte_stable() {
        let a = fp(vec![], "/usr/bin", &[], None, Some(b"cargo-lock"));
        let b = fp(vec![], "/usr/bin", &[], None, Some(b"cargo-lock"));
        assert_eq!(a, b, "structure is fixed; absent sections must not wobble");
        let cargo_hex = sha256_hex(b"cargo-lock");
        assert!(
            a.ends_with(&format!("|nix=none|devbox=none|cargo.lock={cargo_hex}")),
            "expected literal none tokens, got: {a}"
        );
        // base + tail: fingerprint is strictly longer than a bare 64-hex hash.
        assert!(a.len() > 64);
    }

    #[test]
    fn test_nix_digest_order_independent() {
        let seg_a = nix_seg("aaa");
        let seg_b = nix_seg("bbb");
        // Same two segments in different PATH order → same parsed set → same digest.
        let from_ab = nix_segments_from_path(&format!("{seg_a}:{seg_b}"));
        let from_ba = nix_segments_from_path(&format!("{seg_b}:{seg_a}"));
        assert_eq!(nix_digest(&from_ab), nix_digest(&from_ba));
        // Multi-segment digest is symmetric under argument order too.
        assert_eq!(
            nix_digest(&[seg_a.clone(), seg_b.clone()]),
            nix_digest(&[seg_b.clone(), seg_a.clone()])
        );
        // Single-segment PATH hashes identically to the multi-segment
        // algorithm on a one-element list: sha256 of that one segment.
        assert_eq!(
            nix_digest(std::slice::from_ref(&seg_a)),
            sha256_hex(seg_a.as_bytes())
        );
        // Full fingerprint: the nix section is byte-identical across PATH
        // orders; the base hash differs only because PATH itself is hashed
        // verbatim (pre-existing base-formula behavior).
        let ab = fp(
            vec![],
            &format!("{seg_a}:{seg_b}"),
            &from_ab,
            None,
            Some(b"cargo-lock"),
        );
        let ba = fp(
            vec![],
            &format!("{seg_b}:{seg_a}"),
            &from_ba,
            None,
            Some(b"cargo-lock"),
        );
        let nix_ab = ab
            .split("|nix=")
            .nth(1)
            .unwrap()
            .split("|devbox=")
            .next()
            .unwrap();
        let nix_ba = ba
            .split("|nix=")
            .nth(1)
            .unwrap()
            .split("|devbox=")
            .next()
            .unwrap();
        assert_eq!(
            nix_ab, nix_ba,
            "nix section must be independent of PATH order"
        );
    }

    #[test]
    fn test_nix_segment_parser_validates_nix32_shape() {
        let valid = nix_seg("cargo-1.95.0");
        // 'e' is excluded from the Nix32 charset → invalid hash field.
        let bad_char = "/nix/store/0000000000000000000000000000000e-ignored";
        // Hash field too short to be a store path.
        let too_short = "/nix/store/short-name";
        // No '-' delimiter / empty name → not a store path.
        let no_name = "/nix/store/00000000000000000000000000000000";
        let empty_name = "/nix/store/00000000000000000000000000000000-";
        let segments = nix_segments_from_path(&format!(
            "/usr/bin:{valid}:{bad_char}:{too_short}:{no_name}:{empty_name}"
        ));
        assert_eq!(
            segments,
            vec![valid],
            "only well-formed Nix32 segments are collected"
        );
    }

    #[test]
    fn test_attestation_carries_longer_composite_fingerprint() {
        let t = tool("cargo --version");
        let out = run(&t, &[]).unwrap();
        let fp = &out.env_fingerprint;
        assert!(
            fp.len() > 64,
            "fingerprint must be the composite base+tail, got len {}: {fp}",
            fp.len()
        );
        assert!(fp.contains("|nix="), "fixed nix section present: {fp}");
        assert!(
            fp.contains("|devbox="),
            "fixed devbox section present: {fp}"
        );
        assert!(
            fp.contains("|cargo.lock="),
            "fixed cargo.lock section present: {fp}"
        );
        // ToolAttestation JSON shows the longer fingerprint hash verbatim.
        let att = out.to_attestation("C2.1", "abc", "diff", "ts");
        let json = serde_json::to_string(&att).unwrap();
        assert!(
            json.contains(fp),
            "attestation JSON must embed the fingerprint: {json}"
        );
    }

    #[test]
    fn test_repo_root_from_finds_workspace_ancestor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = repo_root_from(manifest_dir).expect("workspace root found");
        let text = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("workspace Cargo.toml readable");
        assert!(
            text.contains("[workspace]"),
            "root {root:?} must carry a [workspace] section"
        );
        assert!(
            manifest_dir.starts_with(&root),
            "root {root:?} must be an ancestor of {manifest_dir:?}"
        );
    }

    #[test]
    fn test_repo_root_from_returns_none_without_workspace() {
        let tmp = std::env::temp_dir().join(format!("pf-noroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).expect("temp subdir created");
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"x\"\n")
            .expect("temp manifest written");
        assert_eq!(
            repo_root_from(&tmp.join("sub")),
            None,
            "no [workspace] ancestor must yield None"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_repo_root_finds_real_workspace() {
        let root = repo_root().expect("CARGO_MANIFEST_DIR walk-up finds the workspace");
        let text = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("workspace Cargo.toml readable");
        assert!(
            text.contains("[workspace]"),
            "root {root:?} must carry a [workspace] section"
        );
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            manifest_dir.starts_with(&root),
            "root {root:?} must be an ancestor of {manifest_dir:?}"
        );
    }

    #[test]
    fn test_env_fingerprint_reads_real_workspace_lockfile() {
        let fp = env_fingerprint("cargo-1.95.0");
        let tail = fp
            .split("|cargo.lock=")
            .nth(1)
            .expect("cargo.lock section present");
        assert_eq!(tail.len(), 64, "real Cargo.lock sha256, not `none`: {fp}");
        assert!(
            tail.bytes().all(|b| b.is_ascii_hexdigit()),
            "hex digest expected: {fp}"
        );
    }

    #[test]
    fn test_lookup_cargo_mutants() {
        let t = lookup("cargo-mutants").expect("cargo-mutants on allowlist");
        assert_eq!(t.name, "cargo-mutants");
        assert_eq!(t.bin, PathBuf::from("cargo-mutants"));
    }

    #[test]
    fn test_cargo_mutants_accepts_version_arg() {
        // Shape check only: `--version` is a clean typed arg (no metacharacters).
        assert!(validate_arg("cargo-mutants", "--version").is_ok());
    }

    #[test]
    fn test_cargo_mutants_rejects_shell_interpolation() {
        let pwned = std::path::Path::new("/tmp/pwned");
        let _ = std::fs::remove_file(pwned);

        // Semicolon chains commands; rejected outright, never passed to a shell.
        let err = validate_arg("cargo-mutants", "--version; rm -rf /").unwrap_err();
        assert!(matches!(err, RunnerError::InvalidArg { tool, .. } if tool == "cargo-mutants"));

        // Command substitution is rejected in `spawn` before the binary is ever
        // executed, so the substituted command must not run.
        let t = tool("cargo-mutants");
        let err = spawn(&t, &["$(touch /tmp/pwned)".to_string()]).unwrap_err();
        assert!(
            matches!(
                err,
                RunnerError::InvalidArg { ref tool, ref arg }
                    if tool == "cargo-mutants" && arg == "$(touch /tmp/pwned)"
            ),
            "unexpected error: {err:?}"
        );
        assert!(!pwned.exists(), "shell-interpolated arg must never execute");
    }

    #[test]
    fn test_parse_timeout_defaults_and_overrides() {
        let default = Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS);
        assert_eq!(parse_timeout_from(None), default);
        assert_eq!(parse_timeout_from(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_timeout_from(Some(" 10 ")), Duration::from_secs(10));
        assert_eq!(parse_timeout_from(Some("0")), default);
        assert_eq!(parse_timeout_from(Some("-3")), default);
        assert_eq!(parse_timeout_from(Some("abc")), default);
    }

    /// Spawn a raw child (bypassing the allowlist) into its own process group,
    /// mirroring what `spawn` does in production.
    #[cfg(unix)]
    fn spawn_raw(args: &[&str]) -> Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(args[0]);
        cmd.args(&args[1..]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.process_group(0);
        cmd.spawn().expect("raw child spawned")
    }

    #[test]
    fn test_hanging_tool_times_out() {
        let child = spawn_raw(&["sleep", "6"]);
        let start = std::time::Instant::now();
        let err = wait_with_timeout(child, Duration::from_millis(500)).unwrap_err();
        assert!(
            matches!(err, RunnerError::TimedOut { .. }),
            "expected TimedOut, got {err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout must fire well before the tool would exit: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_timeout_kills_process_tree() {
        // The direct child `sh` waits on a background `sleep` that inherits the
        // output pipes; without a process-group kill the pipes would stay open
        // and wait_with_timeout would block past the deadline.
        let pid_file = std::env::temp_dir().join(format!("pf-killpg-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let script = format!("sleep 6 & echo $! > {}; wait", pid_file.display());
        let child = spawn_raw(&["sh", "-c", &script]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild pid never recorded"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let start = std::time::Instant::now();
        let err = wait_with_timeout(child, Duration::from_millis(500)).unwrap_err();
        assert!(matches!(err, RunnerError::TimedOut { .. }));
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid_text = std::fs::read_to_string(&pid_file).expect("grandchild pid recorded");
        let grandchild: u32 = pid_text.trim().parse().expect("numeric pid");
        let gone = std::time::Instant::now();
        loop {
            if !std::path::Path::new(&format!("/proc/{grandchild}")).exists() {
                break;
            }
            assert!(
                gone.elapsed() < Duration::from_secs(5),
                "grandchild {grandchild} survived the process-group kill"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&pid_file);
    }

    // Mutant guard for the always-compiled fallback inside kill_process_group.
    // The child inherits this test runner's process group (deliberately NOT
    // process_group(0)), so killpg(child_pid) is an ESRCH no-op: only the
    // unconditional `kill -9` fallback can kill it. A mutant that drops or
    // breaks the fallback leaves the child alive and the assertion trips
    // within ~1s, far under the 500s cargo-mutants --timeout.
    #[cfg(unix)]
    #[test]
    fn test_kill_process_group_fallback_kills_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("sleep spawned");
        let pid = child.id();
        kill_process_group(pid);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => break,
                None => assert!(
                    std::time::Instant::now() < deadline,
                    "child {pid} survived kill_process_group: the always-compiled fallback is dead"
                ),
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_wait_with_timeout_normal_completion() {
        let child = spawn_raw(&["printf", "hello"]);
        let out = wait_with_timeout(child, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }

    #[test]
    fn test_run_with_timeout_allowlisted_tool() {
        let t = tool("cargo --version");
        let out = run_with_timeout(&t, &[], Duration::from_secs(60)).unwrap();
        assert!(out.exit_code == 0);
        assert_eq!(out.stdout_hash.len(), 64);
    }

    // Mutant 10 (runner.rs:182:49, delete `-`): a child killed by a signal has
    // `status.code() == None`; the original maps that to exit_code -1, the
    // mutant to +1. A self-killing script (kills its own process group, of
    // which it is the leader via process_group(0)) produces exactly that.
    #[cfg(unix)]
    #[test]
    fn test_signal_killed_child_reports_minus_one() {
        use std::os::unix::fs::PermissionsExt;
        let n = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let script =
            std::env::temp_dir().join(format!("pf-selfkill-{}-{n}.sh", std::process::id()));
        let _ = std::fs::remove_file(&script);
        std::fs::write(&script, "#!/bin/sh\nkill -KILL -$$\n").expect("script written");
        let mut perms = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("script made executable");
        let t = Tool {
            name: "cargo --version".into(), // allowlisted name passes the gate
            bin: script.clone(),
            args: vec![],
        };
        let out = run_with_timeout(&t, &[], Duration::from_secs(30)).unwrap();
        assert_eq!(
            out.exit_code, -1,
            "signal-killed child must map to -1, got {}",
            out.exit_code
        );
        let _ = std::fs::remove_file(&script);
    }

    // Mutant 12 (runner.rs:422:5, tool_version -> "xyzzy"): the resolved
    // version of a real tool is the tool's own `--version` output, never a
    // hardcoded constant.
    #[test]
    fn test_tool_version_is_real_output() {
        let v = tool_version(&PathBuf::from("/bin/ls"));
        assert!(!v.is_empty(), "ls --version must produce output");
        assert_ne!(v, "xyzzy", "tool_version must not be a constant");
    }

    // Mutant 13 (runner.rs:423:18, guard success -> true): a failing tool must
    // still resolve to the `unknown-` fallback, not to its stdout.
    #[test]
    fn test_tool_version_falls_back_for_failing_tool() {
        let v = tool_version(&PathBuf::from("/bin/false"));
        assert!(
            v.starts_with("unknown-"),
            "failing tool must use unknown- fallback, got: {v}"
        );
    }

    // Mutant 14 (runner.rs:423:18, guard success -> false): a succeeding tool
    // must resolve to its real version, never to the `unknown-` fallback.
    #[test]
    fn test_tool_version_reports_successful_tool_version() {
        let v = tool_version(&PathBuf::from("/bin/ls"));
        assert!(!v.is_empty(), "ls --version must produce output");
        assert!(
            !v.starts_with("unknown-"),
            "succeeding tool must not use unknown- fallback, got: {v}"
        );
    }
}
