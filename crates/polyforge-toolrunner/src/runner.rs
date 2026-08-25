//! T5: allowlist + typed-args tool runner (no shell).
//!
//! The only way evidence becomes Verified is an allowlisted tool running with
//! typed args and its attestation being appended. No shell interpolation
//! anywhere: we spawn the binary directly via [`std::process::Command`], never
//! `sh -c` / `bash -c` / `/bin/sh`.
//!
//! # Trust boundary
//!
//! The allowlist guarantees a bounded set of fixed-name binaries, typed
//! arguments without a shell, a wall-clock timeout, and attribution (env
//! fingerprint, git state). It does NOT guarantee that attestations are
//! TRUE: allowlisted tools execute project code (conftest.py,
//! vite.config.ts, eslint.config.js, build.rs) written by the verified
//! agent. Attestation truth comes from mutation testing, keyed gates, and
//! the operator Validated stage.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use polyforge_core::evidence::EvidenceEntry;
use sha2::{Digest, Sha256};

/// A tool on the allowlist.
///
/// When handed to [`run`] / [`run_with_timeout`] / [`spawn`], only `name`
/// selects what executes: the canonical allowlist entry supplies the binary
/// and fixed args, and `bin` / `args` of a caller-built struct are ignored.
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
        // v2: Python and JavaScript/TypeScript attestation tools (roadmap 1.3).
        Tool {
            name: "pytest".into(),
            bin: PathBuf::from("pytest"),
            args: vec![],
        },
        Tool {
            name: "ruff check".into(),
            bin: PathBuf::from("ruff"),
            args: vec!["check".into()],
        },
        Tool {
            name: "ruff format --check".into(),
            bin: PathBuf::from("ruff"),
            args: vec!["format".into(), "--check".into()],
        },
        Tool {
            name: "mypy".into(),
            bin: PathBuf::from("mypy"),
            args: vec![],
        },
        Tool {
            name: "pyright".into(),
            bin: PathBuf::from("pyright"),
            args: vec![],
        },
        Tool {
            name: "uv --version".into(),
            bin: PathBuf::from("uv"),
            args: vec!["--version".into()],
        },
        Tool {
            name: "vitest run".into(),
            bin: PathBuf::from("vitest"),
            args: vec!["run".into()],
        },
        Tool {
            name: "tsc".into(),
            bin: PathBuf::from("tsc"),
            args: vec!["--noEmit".into()],
        },
        Tool {
            name: "eslint".into(),
            bin: PathBuf::from("eslint"),
            args: vec![],
        },
        Tool {
            name: "biome check".into(),
            bin: PathBuf::from("biome"),
            args: vec!["check".into()],
        },
    ]
}

/// Look up a tool by name in the allowlist.
pub fn lookup(name: &str) -> Option<Tool> {
    allowlist().into_iter().find(|t| t.name == name)
}

/// Spawn an allowlisted tool with typed args, no shell. Returns the live child
/// with piped stdio so callers can inspect `/proc/<pid>/cmdline` while it runs.
///
/// Invariant: the allowlist entry is the single source of truth for the
/// binary identity and fixed args; caller-supplied [`Tool`] fields other than
/// `name` are ignored. The canonical entry is resolved internally and only it
/// is executed, so a hand-built `Tool` cannot smuggle an unallowlisted binary
/// past the name gate.
pub fn spawn(tool: &Tool, args: &[String]) -> Result<Child, RunnerError> {
    let canonical = lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
    validate_tool_args(&canonical.name, args)?;
    let mut cmd = Command::new(&canonical.bin);
    cmd.args(&canonical.args).args(args);
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

/// Execution backend for allowlisted tool runs.
///
/// The trait mirrors the free [`run`] function's EXACT signature
/// (synchronous, no async addition) so call sites can route through a
/// backend without any behavioral change. Today there is exactly one
/// compiled-in implementation, [`ProcessExecutor`]; the indirection exists so
/// a sandbox backend can be selected behind the same seam.
pub(crate) trait Executor {
    /// Run an allowlisted tool to completion under the configured wall-clock
    /// budget. Contract is byte-identical to the free [`run`] function.
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError>;
}

/// The process backend: spawns the canonical allowlisted binary directly via
/// [`std::process::Command`] (no shell), enforces the typed-arg policy, and
/// captures output + attestation fields. This is the historical behavior,
/// moved here 1:1 from the former [`run`] body — every check, ordering, and
/// error variant is unchanged.
pub(crate) struct ProcessExecutor;

impl Executor for ProcessExecutor {
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
        run_with_timeout(tool, args, parse_timeout())
    }
}

/// Singleton instance of the process backend handed out by [`executor`].
static PROCESS_EXECUTOR: ProcessExecutor = ProcessExecutor;

/// Which execution backend attestations run under.
///
/// Selected once per process via [`init_executor`] before any spawn; the
/// default (never initialized) is [`ExecutorKind::Process`], which keeps
/// legacy behavior byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorKind {
    /// Spawn the allowlisted binary directly in this process (historical
    /// behavior, always available).
    Process,
    /// Run inside a sandbox backend. Requires building with the
    /// `sandbox-mock` feature; without it [`init_executor`] rejects the
    /// selection with a clear feature-gate error before anything runs.
    Sandbox,
}

/// Internal codes stored in [`EXECUTOR_KIND`]. `0` means "never selected"
/// and resolves to the process backend.
const KIND_UNSET: u8 = 0;
const KIND_PROCESS: u8 = 1;
const KIND_SANDBOX: u8 = 2;

static EXECUTOR_KIND: AtomicU8 = AtomicU8::new(KIND_UNSET);

/// Select the execution backend for this process. Must be called BEFORE any
/// tool run; later calls are accepted only when they repeat the already
/// selected kind (idempotent), otherwise they are rejected.
///
/// Fail-closed ordering: the `sandbox-mock` feature gate is checked BEFORE
/// any state is written, so selecting [`ExecutorKind::Sandbox`] without the
/// feature leaves the process on the untouched default and returns a clear
/// error instead of silently falling back.
pub fn init_executor(kind: ExecutorKind) -> Result<(), String> {
    let code = match kind {
        ExecutorKind::Process => KIND_PROCESS,
        ExecutorKind::Sandbox => {
            if !cfg!(feature = "sandbox-mock") {
                return Err("sandbox executor requires feature sandbox-mock".to_string());
            }
            KIND_SANDBOX
        }
    };
    match EXECUTOR_KIND.compare_exchange(KIND_UNSET, code, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => Ok(()),
        Err(prev) if prev == code => Ok(()),
        Err(prev) => Err(format!(
            "executor already initialized to {}",
            kind_name_of_code(prev)
        )),
    }
}

/// Render an internal selection code as its flag-facing name (diagnostics
/// only; unknown codes render as `process` because that is the effective
/// fallback).
fn kind_name_of_code(code: u8) -> &'static str {
    match code {
        KIND_SANDBOX => "sandbox",
        _ => "process",
    }
}

/// The currently selected backend kind. An uninitialized process reports
/// [`ExecutorKind::Process`].
fn selected_executor_kind() -> ExecutorKind {
    match EXECUTOR_KIND.load(Ordering::SeqCst) {
        KIND_SANDBOX => ExecutorKind::Sandbox,
        _ => ExecutorKind::Process,
    }
}

/// Module-level accessor returning the selected execution backend. The
/// selection is fixed by [`init_executor`]; the default is the process
/// backend. A recorded Sandbox selection maps to the process backend until
/// T10 supplies the mock singleton; in practice that state is unreachable
/// without the feature because init rejects Sandbox before writing it.
pub(crate) fn executor() -> &'static dyn Executor {
    match selected_executor_kind() {
        ExecutorKind::Process => &PROCESS_EXECUTOR,
        ExecutorKind::Sandbox => &PROCESS_EXECUTOR,
    }
}

/// Record-only executor identity for attestation payloads: `None` for the
/// process backend (legacy payloads stay byte-identical, no metadata key),
/// `Some(digest)` only when a non-process executor performed the run. The
/// mock sandbox backend arrives in T10; until then every compiled
/// configuration reports None, so attestations never claim an executor they
/// did not use.
pub(crate) fn active_executor_digest() -> Option<String> {
    match selected_executor_kind() {
        ExecutorKind::Process => None,
        ExecutorKind::Sandbox => None,
    }
}

/// Run an allowlisted tool to completion and capture its output + attestation
/// fields, bounded by the `PF_TOOL_TIMEOUT_SECS` wall-clock budget (default
/// [`DEFAULT_TOOL_TIMEOUT_SECS`]). A tool that outlives the budget is killed
/// together with its process group and the run fails with
/// [`RunnerError::TimedOut`].
///
/// Invariant: the allowlist entry is the single source of truth for the
/// binary identity and fixed args; caller-supplied [`Tool`] fields other than
/// `name` are ignored, including in the attested `command` string and the
/// resolved `tool_version`.
///
/// Delegates to the module's selected [`Executor`] backend ([`ProcessExecutor`]).
pub fn run(tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
    executor().run(tool, args)
}

/// Like [`run`], but with an explicit wall-clock budget.
pub fn run_with_timeout(
    tool: &Tool,
    args: &[String],
    timeout: Duration,
) -> Result<RunOutput, RunnerError> {
    let canonical = lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
    let child = spawn(tool, args)?;
    let out = wait_with_timeout(child, timeout)?;
    let exit_code = exit_code_of(out.status);
    let stdout_hash = sha256_hex(&out.stdout);
    let tool_version = tool_version(&canonical.bin);
    let env_fingerprint = env_fingerprint(&tool_version);
    let command = command_string(&canonical, args);
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

/// Attested exit code: the child's numeric status when it exited normally,
/// or -1 when it was killed by a signal (`status.code() == None`).
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
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
    const META: &[char] = &[';', '|', '&', '`', '$', '>', '<', '\t', '\n', '\0'];
    if arg.chars().any(|c| META.contains(&c)) {
        return Err(RunnerError::InvalidArg {
            tool: tool.to_string(),
            arg: arg.to_string(),
        });
    }
    Ok(())
}

/// Attestation runs must never MUTATE the worktree (--fix/--update/
/// --apply class) nor load attacker-chosen code paths (--rulesdir,
/// custom formatters, `-p` plugin imports). Package runners (uv run,
/// npx, npm exec) stay off the allowlist entirely because their argv
/// resolves an unbounded binary set.
fn denied_arg(tool: &str, arg: &str) -> bool {
    match tool {
        "ruff check" => matches!(arg, "--fix" | "--unsafe-fixes"),
        "eslint" => {
            matches!(
                arg,
                "--fix" | "--rulesdir" | "--resolve-plugins-relative-to"
            )
        }
        "biome check" => matches!(arg, "--apply" | "--apply-unsafe" | "--write"),
        "vitest run" => matches!(arg, "-u" | "--update"),
        "pytest" => {
            if arg == "--pdb" {
                true
            } else if arg == "-p" {
                false // value checked by the caller via pair rule below
            } else {
                arg.starts_with("-p") && !arg.starts_with("-p no:")
            }
        }
        "gcc -v" => true, // pure version probe: any extra arg denied
        _ => false,
    }
}

/// Full typed-arg policy: metachars per arg, then the per-tool denylist,
/// then the eslint --format value rule (custom formatter paths are
/// loadable JS). Slice-level so `--format <value>` can be inspected as a
/// pair.
fn validate_tool_args(tool: &str, args: &[String]) -> Result<(), RunnerError> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        validate_arg(tool, a)?;
        if denied_arg(tool, a) {
            return Err(RunnerError::InvalidArg {
                tool: tool.to_string(),
                arg: a.clone(),
            });
        }
        if tool == "eslint" && a == "--format" {
            if let Some(v) = args.get(i + 1) {
                if v.contains('/') || v.ends_with(".js") {
                    return Err(RunnerError::InvalidArg {
                        tool: tool.to_string(),
                        arg: v.clone(),
                    });
                }
            }
        }
        if tool == "pytest" && a == "-p" {
            match args.get(i + 1) {
                Some(v) if v.starts_with("no:") => {}
                _ => {
                    return Err(RunnerError::InvalidArg {
                        tool: tool.to_string(),
                        arg: a.clone(),
                    });
                }
            }
        }
        i += 1;
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
/// `<base-hex>|nix=<none|digest>|devbox=<none|sha256>|cargo.lock=<sha256>|uv.lock=<sha256>|pnpm-lock.yaml=<sha256>|package-lock.json=<sha256>|yarn.lock=<sha256>`
///
/// `base-hex` is the pre-C2.1 formula unchanged (SHA-256 over tool version +
/// os + arch + sorted env var names + PATH). The tail is appended verbatim in
/// fixed order; every section is unconditional. Each lockfile name resolves
/// against candidate roots up-tree from the base directory (see
/// `candidate_roots`) and contributes the SHA-256 of the first match, or
/// literal `none` when no candidate contains the file. Changes when the tool
/// version, PATH, Nix store paths, or any tracked lockfile changes;
/// byte-stable across identical runs.
pub fn env_fingerprint(tool_version: &str) -> String {
    let base = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_default();
    env_fingerprint_at(tool_version, &base)
}

/// Environment-facing builder rooted at an explicit base directory: names,
/// PATH, and Nix segments come from the real process environment while
/// lockfiles are discovered by scanning [`candidate_roots`] built from
/// `base_dir`. Production callers go through [`env_fingerprint`] (base =
/// CARGO_MANIFEST_DIR, falling back to the current directory); tests pass a
/// synthetic root.
fn env_fingerprint_at(tool_version: &str, base_dir: &Path) -> String {
    let names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let path = std::env::var("PATH").unwrap_or_default();
    let nix_segments = nix_segments_from_path(&path);
    let roots = candidate_roots(base_dir);
    let read_first = |name: &str| {
        roots
            .iter()
            .find_map(|dir| std::fs::read(dir.join(name)).ok())
    };
    let devbox_bytes = read_first("devbox.lock");
    let cargo_lock_bytes = read_first("Cargo.lock");
    let uv_lock_bytes = read_first("uv.lock");
    let pnpm_lock_bytes = read_first("pnpm-lock.yaml");
    let package_lock_bytes = read_first("package-lock.json");
    let yarn_lock_bytes = read_first("yarn.lock");
    let allowlist: Vec<(&str, String)> = ENV_FINGERPRINT_ALLOWLIST
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (*name, value)))
        .collect();
    let allowlist_refs: Vec<(&str, &str)> = allowlist
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let locks = LockfileBytes {
        devbox: devbox_bytes.as_deref(),
        cargo_lock: cargo_lock_bytes.as_deref(),
        uv_lock: uv_lock_bytes.as_deref(),
        pnpm_lock: pnpm_lock_bytes.as_deref(),
        package_lock: package_lock_bytes.as_deref(),
        yarn_lock: yarn_lock_bytes.as_deref(),
    };
    env_fingerprint_from_pairs(
        tool_version,
        &names,
        &path,
        &nix_segments,
        &locks,
        &allowlist_refs,
    )
}

/// Contents of the six environment lockfiles folded into the fingerprint
/// tail, held in fixed tail order. `None` means no candidate root contained
/// the file (or it was unreadable) and renders as literal `none`.
struct LockfileBytes<'a> {
    devbox: Option<&'a [u8]>,
    cargo_lock: Option<&'a [u8]>,
    uv_lock: Option<&'a [u8]>,
    pnpm_lock: Option<&'a [u8]>,
    package_lock: Option<&'a [u8]>,
    yarn_lock: Option<&'a [u8]>,
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
    locks: &LockfileBytes<'_>,
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
        "{base}|nix={}|devbox={}|cargo.lock={}|uv.lock={}|pnpm-lock.yaml={}|package-lock.json={}|yarn.lock={}",
        nix_digest(nix_segments),
        lock_section(locks.devbox),
        lock_section(locks.cargo_lock),
        lock_section(locks.uv_lock),
        lock_section(locks.pnpm_lock),
        lock_section(locks.package_lock),
        lock_section(locks.yarn_lock),
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

/// Directories scanned for each environment lockfile, in order: the enclosing
/// git repository root when one exists (nearest ancestor carrying a `.git`
/// entry), then `base` itself and every ancestor directory walking upward.
/// Per lockfile name the first candidate containing the file wins; later
/// candidates never override an earlier match. A `[workspace]` Cargo.toml is
/// not required, so foreign Python/TS repositories resolve their own locks.
fn candidate_roots(base: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(git_root) = base
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(PathBuf::from)
    {
        roots.push(git_root);
    }
    roots.extend(base.ancestors().map(PathBuf::from));
    roots
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

    /// Mutation/audit guard: run must execute the CANONICAL allowlist entry,
    /// never caller-supplied bin/args. A hand-built Tool carrying an
    /// allowlisted name but a hostile bin must be neutralized to the real
    /// allowlisted command.
    #[test]
    fn spawn_uses_canonical_allowlist_entry_not_caller_bin() {
        let hostile = Tool {
            name: "cargo --version".into(),
            bin: PathBuf::from("echo"),
            args: vec!["PWNED".into()],
        };
        let out = run(&hostile, &[]).expect("canonical cargo --version must run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("PWNED"),
            "caller-supplied bin/args must be ignored"
        );
        assert!(stdout.contains("cargo"), "canonical binary must execute");
        assert!(
            out.command.starts_with("cargo"),
            "attested command string must reflect the canonical entry: {}",
            out.command
        );
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

    /// All-absent lockfile vector for the pure core.
    fn no_locks() -> LockfileBytes<'static> {
        LockfileBytes {
            devbox: None,
            cargo_lock: None,
            uv_lock: None,
            pnpm_lock: None,
            package_lock: None,
            yarn_lock: None,
        }
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
            &LockfileBytes {
                devbox,
                cargo_lock: cargo,
                ..no_locks()
            },
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
            &no_locks(),
            &[("RUSTFLAGS", "-Ctarget-cpu=native")],
        );
        let other = env_fingerprint_from_pairs(
            "cargo-1.95.0",
            &[],
            "/usr/bin",
            &[],
            &no_locks(),
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
        let a =
            env_fingerprint_from_pairs("cargo-1.95.0", &[], "/usr/bin", &[], &no_locks(), pairs);
        let b =
            env_fingerprint_from_pairs("cargo-1.95.0", &[], "/usr/bin", &[], &no_locks(), pairs);
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
            a.contains(&format!(
                "|nix=none|devbox=none|cargo.lock={cargo_hex}|uv.lock=none|pnpm-lock.yaml=none|package-lock.json=none|yarn.lock=none"
            )),
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
    fn test_candidate_roots_walk_base_and_ancestors() {
        let tmp = std::env::temp_dir().join(format!("pf-cands-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).expect("temp subdir created");
        let roots = candidate_roots(&tmp.join("sub"));
        assert_eq!(
            roots.first(),
            Some(&tmp.join("sub")),
            "base dir itself is the first candidate: {roots:?}"
        );
        assert!(
            roots.contains(&tmp),
            "immediate parent must be a candidate: {roots:?}"
        );
        assert_eq!(
            roots.last(),
            Some(&PathBuf::from("/")),
            "walk terminates at the filesystem root: {roots:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_candidate_roots_prepends_git_root() {
        let tmp = std::env::temp_dir().join(format!("pf-gitroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".git")).expect(".git marker created");
        std::fs::create_dir_all(tmp.join("sub")).expect("sub created");
        let roots = candidate_roots(&tmp.join("sub"));
        assert_eq!(
            roots.first(),
            Some(&tmp),
            "git repo root is prepended ahead of base: {roots:?}"
        );
        assert_eq!(
            roots.get(1),
            Some(&tmp.join("sub")),
            "base dir follows the git root: {roots:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Negative control: with no lockfile of any kind in the base directory
    /// or anywhere up-tree, every foreign-lockfile section renders literal
    /// `none` and nothing is invented.
    #[test]
    fn test_env_fingerprint_no_lockfiles_up_tree_renders_none() {
        let tmp = std::env::temp_dir().join(format!("pf-nolocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp dir created");
        let fp = env_fingerprint_at("cargo-1.95.0", &tmp);
        assert_eq!(tail_section(&fp, "uv.lock"), "none");
        assert_eq!(tail_section(&fp, "pnpm-lock.yaml"), "none");
        assert_eq!(tail_section(&fp, "package-lock.json"), "none");
        assert_eq!(tail_section(&fp, "yarn.lock"), "none");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_env_fingerprint_reads_real_workspace_lockfile() {
        let fp = env_fingerprint("cargo-1.95.0");
        let tail = tail_section(&fp, "cargo.lock");
        assert_eq!(tail.len(), 64, "real Cargo.lock sha256, not `none`: {fp}");
        assert!(
            tail.bytes().all(|b| b.is_ascii_hexdigit()),
            "hex digest expected: {fp}"
        );
    }

    /// Extracts one self-describing tail section (`|<key>=<value>`) from a
    /// fingerprint string.
    fn tail_section(fp: &str, key: &str) -> String {
        let marker = format!("|{key}=");
        let rest = fp.split(&marker).nth(1).expect("tail section present");
        rest.split('|')
            .next()
            .expect("section value present")
            .to_string()
    }

    /// Foreign-repository discovery guard: a directory holding ONLY a
    /// foreign lockfile with no `[workspace]` ancestor anywhere up-tree must
    /// still fold that lockfile into the fingerprint tail via candidate-root
    /// scanning (Python/TS repositories without Cargo.toml are first-class).
    #[test]
    fn test_env_fingerprint_discovers_uv_lock_in_non_cargo_repo() {
        let tmp = std::env::temp_dir().join(format!("pf-noncargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp dir created");
        std::fs::write(tmp.join("uv.lock"), b"uv-lock-v1").expect("uv.lock written");
        let fp = env_fingerprint_at("cargo-1.95.0", &tmp);
        assert!(
            fp.contains(&format!("|uv.lock={}", sha256_hex(b"uv-lock-v1"))),
            "foreign-repo uv.lock must contribute its sha256: {fp}"
        );
        assert_ne!(tail_section(&fp, "uv.lock"), "none");
        assert_eq!(tail_section(&fp, "devbox"), "none");
        assert_eq!(tail_section(&fp, "cargo.lock"), "none");
        assert_eq!(tail_section(&fp, "pnpm-lock.yaml"), "none");
        assert_eq!(tail_section(&fp, "package-lock.json"), "none");
        assert_eq!(tail_section(&fp, "yarn.lock"), "none");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Non-Cargo lockfile types ARE folded once a `[workspace]` root exists
    /// up-tree: uv.lock contributes its SHA-256 while absent siblings stay
    /// literal `none`.
    #[test]
    fn test_env_fingerprint_discovers_uv_lock_under_workspace_root() {
        let tmp = std::env::temp_dir().join(format!("pf-uvroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp dir created");
        std::fs::write(
            tmp.join("Cargo.toml"),
            "[workspace]\nmembers = []\nresolver = \"2\"\n",
        )
        .expect("workspace manifest written");
        std::fs::write(tmp.join("uv.lock"), b"uv-lock-v1").expect("uv.lock written");
        let fp = env_fingerprint_at("cargo-1.95.0", &tmp);
        assert_eq!(
            tail_section(&fp, "uv.lock"),
            sha256_hex(b"uv-lock-v1"),
            "discovered uv.lock must contribute its sha256"
        );
        assert_eq!(tail_section(&fp, "devbox"), "none");
        assert_eq!(tail_section(&fp, "cargo.lock"), "none");
        assert_eq!(tail_section(&fp, "pnpm-lock.yaml"), "none");
        assert_eq!(tail_section(&fp, "package-lock.json"), "none");
        assert_eq!(tail_section(&fp, "yarn.lock"), "none");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_lookup_cargo_mutants() {
        let t = lookup("cargo-mutants").expect("cargo-mutants on allowlist");
        assert_eq!(t.name, "cargo-mutants");
        assert_eq!(t.bin, PathBuf::from("cargo-mutants"));
    }

    /// All v2 Python/JS-TS tools must be resolvable by exact name.
    #[test]
    fn allowlist_v2_tools_are_lookupable() {
        for name in [
            "pytest",
            "ruff check",
            "ruff format --check",
            "mypy",
            "pyright",
            "uv --version",
            "vitest run",
            "tsc",
            "eslint",
            "biome check",
        ] {
            assert!(lookup(name).is_some(), "{name} must be on the allowlist");
        }
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

    // Mutant 10 (exit_code_of, delete `-` in unwrap_or(-1)): a child killed
    // by a signal has status.code() == None; correct mapping is -1, the
    // mutant yields +1. The old spawn-based vehicle (hand-built Tool with a
    // self-killing script bin) is impossible by design now: canonical
    // resolution ignores caller-supplied bins, so the mapping is unit-tested
    // directly on exit_code_of.
    #[cfg(unix)]
    #[test]
    fn test_signal_killed_child_reports_minus_one() {
        use std::os::unix::process::ExitStatusExt;
        let signaled = std::process::ExitStatus::from_raw(9); // SIGKILL, no core
        assert!(
            signaled.code().is_none(),
            "raw wait status 9 must be signal-terminated"
        );
        assert_eq!(
            super::exit_code_of(signaled),
            -1,
            "signal-killed child must map to -1"
        );
        assert_eq!(
            super::exit_code_of(std::process::ExitStatus::from_raw(0)),
            0,
            "clean exit must stay 0"
        );
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

    // ---- ARG-DENYLIST policy walk ----

    /// Denial matrix: each argv must be rejected with InvalidArg before any
    /// process is spawned.
    #[test]
    fn arg_denylist_rejects_mutating_and_code_loading_args() {
        let cases: &[(&str, &[&str])] = &[
            ("ruff check", &["--fix"]),
            ("ruff check", &["--unsafe-fixes"]),
            ("eslint", &["--fix"]),
            ("eslint", &["--rulesdir", "x"]),
            ("eslint", &["--resolve-plugins-relative-to", "x"]),
            ("eslint", &["--format", "./evil.js"]),
            ("eslint", &["--format", "a/b"]),
            ("biome check", &["--apply"]),
            ("biome check", &["--write"]),
            ("vitest run", &["-u"]),
            ("vitest run", &["--update"]),
            ("pytest", &["-p", "evil_plugin"]),
            ("pytest", &["--pdb"]),
            ("gcc -v", &["anything"]),
        ];
        for (name, raw) in cases {
            let t = tool(name);
            let args: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
            let err = spawn(&t, &args).unwrap_err();
            assert!(
                matches!(err, RunnerError::InvalidArg { .. }),
                "{name} {raw:?} must be denied with InvalidArg, got {err:?}"
            );
        }
    }

    /// Positive controls: benign typed args pass the full policy walk.
    #[test]
    fn arg_denylist_allows_benign_typed_args() {
        let pytest_ok = vec!["-p".to_string(), "no:cacheprovider".to_string()];
        assert!(validate_tool_args("pytest", &pytest_ok).is_ok());
        let eslint_ok = vec!["--format".to_string(), "json".to_string()];
        assert!(validate_tool_args("eslint", &eslint_ok).is_ok());
    }

    /// Mutant runner.rs:390:42 (delete ! in the "-p no:" exemption): the
    /// single-token form "-p no:<plugin>" must stay allowed exactly like the
    /// pair form above; deleting the negation denies it instead.
    #[test]
    fn pytest_single_token_no_plugin_arg_is_allowed() {
        let args = vec!["-p no:cacheprovider".to_string()];
        assert!(
            validate_tool_args("pytest", &args).is_ok(),
            "single-token -p no: exemption must pass validation"
        );
    }

    /// Mutant runner.rs:413:29 (&& -> ||): the eslint --format value rule
    /// must stay scoped to eslint; pytest carrying --format with a .js value
    /// sits outside that rule and must pass.
    #[test]
    fn format_value_rule_is_eslint_scoped() {
        let args = vec!["--format".to_string(), "fmt.js".to_string()];
        assert!(
            validate_tool_args("pytest", &args).is_ok(),
            "non-eslint tools are not subject to the eslint --format rule"
        );
    }

    /// Tab is a shell word separator and must be rejected like newline/NUL.
    #[test]
    fn validate_arg_rejects_tab_metachar() {
        let err = validate_arg("cargo-mutants", "--foo\tbar").unwrap_err();
        assert!(matches!(err, RunnerError::InvalidArg { .. }));
    }

    // ---- T9 executor selection seam ----
    //
    // All writers below may only ever record KIND_PROCESS in this build
    // (Sandbox is rejected before any state write), so these tests are
    // race-free under cargo's parallel test runner: concurrent Process
    // selections are idempotent by design.

    #[test]
    fn selection_defaults_to_process_without_init() {
        assert_eq!(selected_executor_kind(), ExecutorKind::Process);
        assert!(active_executor_digest().is_none());
    }

    #[test]
    fn init_executor_process_is_idempotent() {
        init_executor(ExecutorKind::Process).expect("first process selection");
        init_executor(ExecutorKind::Process).expect("repeat process selection");
        assert_eq!(selected_executor_kind(), ExecutorKind::Process);
    }

    /// Feature-gate failure mode: selecting Sandbox without `sandbox-mock`
    /// must fail with the exact gate message and leave the selection
    /// untouched (no silent fallback to a half-initialized state).
    #[test]
    fn init_executor_sandbox_requires_feature() {
        match init_executor(ExecutorKind::Sandbox) {
            Err(msg) => {
                assert_eq!(msg, "sandbox executor requires feature sandbox-mock");
                assert_eq!(
                    selected_executor_kind(),
                    ExecutorKind::Process,
                    "rejected selection must not mutate process state"
                );
            }
            Ok(()) => {
                // Feature-on build (T10): the selection succeeded and must be
                // observable, never silently dropped.
                assert_eq!(selected_executor_kind(), ExecutorKind::Sandbox);
            }
        }
    }

    /// Conflicting re-selection renders the recorded kind by name. The full
    /// conflict path (recorded Sandbox vs requested Process) is only
    /// reachable with `sandbox-mock` compiled in, because in this build a
    /// Sandbox request dies at the feature gate before the set-once check.
    #[test]
    fn kind_code_names_render_for_conflict_message() {
        assert_eq!(kind_name_of_code(KIND_PROCESS), "process");
        assert_eq!(kind_name_of_code(KIND_SANDBOX), "sandbox");
        assert_eq!(kind_name_of_code(KIND_UNSET), "process");
    }

    /// Digest contract at the verify choke point: the process backend never
    /// contributes an executor_digest metadata key.
    #[test]
    fn process_backend_yields_no_executor_digest() {
        assert_eq!(active_executor_digest(), None);
    }
}
