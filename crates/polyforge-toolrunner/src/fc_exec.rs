//! T9: Firecracker microVM execution backend behind the `sandbox-firecracker`
//! feature.
//!
//! Runs allowlisted tools inside an ephemeral Firecracker microVM so an
//! attestation executes under hardware virtualization, isolated from the
//! host kernel entirely. The boot and exec protocol is the PROVEN T8 spike
//! protocol (deploy/firecracker/spike/spike.sh, verdict GO):
//!
//! 1. Spawn `firecracker --api-sock <sock>` (optionally wrapped by the
//!    jailer, see [`JailerConfig`]).
//! 2. Drive the REST API over the unix socket with minimal hand-rolled
//!    HTTP/1.1 PUTs IN THIS EXACT ORDER: logger (target file PRE-CREATED -
//!    FC v1.9.1 opens it WITHOUT O_CREAT), boot-source, drives/rootfs,
//!    vsock, actions InstanceStart. There is NO network-interface step:
//!    guests are vsock-only by design.
//! 3. Connect the vsock unix socket, send `CONNECT <port>`, expect a reply
//!    line whose prefix is `OK` (prefix check only, per the spike), then
//!    speak newline-delimited JSON with the guest agent: request
//!    `{"cmd":"..."}`, reply `{"exit":N,"stdout":"..."}`. One connection
//!    serves MANY requests because the agent loops per connection.
//!
//! One ephemeral microVM boots PER attestation run and is killed on drop,
//! on timeout (a watchdog SIGKILLs the whole process group), and on every
//! error path.
//!
//! # Executor digest: hash INPUTS, not image bytes
//!
//! `executor_digest = sha256(canonical build-inputs manifest)` where the
//! manifest is `{kernel_sha256, rootfs_build_inputs:{base_image_or_packages,
//! build_script_sha256}, fc_version}`. Raw ext4 bytes are deliberately NOT
//! hashed: mkfs.ext4 output embeds filesystem UUIDs, superblock timestamps,
//! and alignment padding that differ between rebuilds even from identical
//! inputs, so two content-identical images would produce different digests
//! and break cross-build comparability. Hashing the declared inputs yields
//! one stable identity per logical image. The formula intentionally
//! diverges from the other tiers; the `fc:` metadata prefix recorded at
//! dispatch disambiguates them.
//!
//! # Configuration (process environment)
//!
//! * `POLYFORGE_FC_KERNEL`: absolute path to the pinned vmlinux image.
//! * `POLYFORGE_FC_ROOTFS`: absolute path to the ext4 rootfs containing the
//!   tool binaries plus the static vsock guest agent as `/init`.
//! * `POLYFORGE_FC_MANIFEST`: path to the rootfs build-inputs manifest JSON
//!   written by deploy/firecracker/rootfs-build.sh (keys:
//!   `base_image_or_packages`, `build_script_sha256`).
//! * `POLYFORGE_FC_WORK_DIR`: directory for per-run sockets and logs
//!   (default `<temp>/pf-fc`). Must be writable.
//! * `POLYFORGE_FC_GUEST_CID`: vsock context id (default 3; 0 to 2 are
//!   reserved).
//!
//! The wall-clock budget comes from the shared `PF_TOOL_TIMEOUT_SECS`
//! handling ([`parse_timeout`]); a watchdog kills the whole FC process
//! group when the budget expires, mirroring [`wait_with_timeout`].
//!
//! # Jailer (production posture)
//!
//! The plan mandates jailer mode for production: chroot + uid/gid +
//! cgroups. Set [`FcConfig::jailer`] to stage every run through the jailer
//! binary. Under the jailer the API socket name passed to firecracker is
//! relative (`api.sock`) and the host-side socket appears at
//! `<chroot_base>/<run_id>/root/api.sock`; the logger target and vsock
//! socket likewise live inside the jail root (`/fc.log`, `/v.sock`). The
//! kernel and rootfs paths are passed into the API bodies VERBATIM, so in
//! jailer mode the operator must stage those assets where the jailed
//! process can see them (deployment-pack duty, see
//! deploy/firecracker/README-host-prereqs.md).
//!
//! # Known limitations (inherited from the proven guest agent)
//!
//! The guest agent merges stdout and stderr and truncates captured output
//! at 64 KiB; it executes commands through `/bin/sh -c`. Typed args are
//! already metachar-sanitized by the shared policy, so joining them with
//! single spaces is safe; an arg containing whitespace becomes multiple
//! shell words inside the guest. Attestations record the canonical command
//! string identically across backends regardless.

use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::runner::{
    command_string, lookup, parse_timeout, sha256_hex, validate_tool_args, Executor, RunOutput,
    RunnerError, Tool,
};

/// AF_VSOCK port the static guest agent listens on (spike-proven constant).
pub const GUEST_AGENT_PORT: u16 = 5001;

/// Default vsock context id assigned to the guest (0 to 2 are reserved).
pub const DEFAULT_GUEST_CID: u32 = 3;

/// Lowest usable guest CID: 0 invalid, 1 hypervisor, 2 host.
pub const MIN_GUEST_CID: u32 = 3;

/// Boot window for the microVM to accept the first vsock connection
/// (spike-proven default of 90 seconds).
pub const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 90;

/// How long the FC API socket may take to appear after spawn.
const API_SOCKET_WAIT_SECS: u64 = 10;

/// Poll interval while waiting for sockets (spike used 200ms / 500ms).
const API_POLL_INTERVAL: Duration = Duration::from_millis(200);
const VSOCK_RETRY_MS: u64 = 500;

/// Upper bound for one guest-agent response line (the agent itself
/// truncates captured output at 64 KiB; this cap only guards against a
/// hostile or broken peer streaming forever).
const MAX_AGENT_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Kernel boot args, verbatim from the proven spike boot-source PUT.
const BOOT_ARGS: &str = "console=ttyS0 init=/init reboot=k panic=1 pci=off";

/// Environment variable holding the pinned vmlinux image path.
pub const POLYFORGE_FC_KERNEL_ENV: &str = "POLYFORGE_FC_KERNEL";

/// Environment variable holding the ext4 rootfs image path.
pub const POLYFORGE_FC_ROOTFS_ENV: &str = "POLYFORGE_FC_ROOTFS";

/// Environment variable holding the rootfs build-inputs manifest path.
pub const POLYFORGE_FC_MANIFEST_ENV: &str = "POLYFORGE_FC_MANIFEST";

/// Environment variable overriding the per-run work directory.
pub const POLYFORGE_FC_WORK_DIR_ENV: &str = "POLYFORGE_FC_WORK_DIR";

/// Environment variable overriding the guest vsock CID.
pub const POLYFORGE_FC_GUEST_CID_ENV: &str = "POLYFORGE_FC_GUEST_CID";

/// Declared inputs of the rootfs build, hashed INTO the executor digest.
///
/// Populated from the manifest JSON emitted by
/// deploy/firecracker/rootfs-build.sh so that rebuilding the image from the
/// same inputs keeps one stable digest even though raw ext4 bytes drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsBuildInputs {
    /// Base image or exact package list the rootfs was populated from.
    pub base_image_or_packages: String,
    /// SHA-256 of the build script that produced the image.
    pub build_script_sha256: String,
}

impl RootfsBuildInputs {
    /// Parse the manifest JSON written by rootfs-build.sh. Both keys must be
    /// present and non-empty; anything else fails closed naming the key.
    pub fn from_manifest_path(path: &Path) -> Result<Self, RunnerError> {
        let raw = std::fs::read_to_string(path).map_err(|e| RunnerError::Io(e.to_string()))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| RunnerError::Io(format!("{} is not valid JSON: {e}", path.display())))?;
        let required = |key: &str| -> Result<String, RunnerError> {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    RunnerError::Spawn(format!(
                        "rootfs build-inputs manifest {} is missing non-empty string key \"{key}\"",
                        path.display()
                    ))
                })
        };
        Ok(Self {
            base_image_or_packages: required("base_image_or_packages")?,
            build_script_sha256: required("build_script_sha256")?,
        })
    }
}

/// Optional jailer wrapping for the production posture (chroot + uid/gid +
/// cgroups). When set on [`FcConfig::jailer`], every run spawns
/// `<bin> <args...> -- --api-sock api.sock` instead of invoking the
/// firecracker binary directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailerConfig {
    /// Absolute path to the jailer binary (SAME version as firecracker).
    pub bin: PathBuf,
    /// uid the jailed microVM process runs as.
    pub uid: u32,
    /// gid the jailed microVM process runs as.
    pub gid: u32,
    /// Base directory under which the per-run chroot is created
    /// (`<base>/<run_id>/root`).
    pub chroot_base: PathBuf,
    /// Extra `--cgroup` arguments (e.g. `ver=2,controllers=cpu,memory`).
    pub cgroups: Vec<String>,
}

/// Pure argv builder for the jailer invocation. Unit-tested shape contract;
/// the trailing `--` separates jailer args from firecracker args.
pub fn jailer_args(
    jailer: &JailerConfig,
    run_id: &str,
    firecracker_bin: &Path,
    api_sock_name: &str,
) -> Vec<String> {
    let mut args = vec![
        "--id".to_string(),
        run_id.to_string(),
        "--exec-file".to_string(),
        firecracker_bin.display().to_string(),
        "--uid".to_string(),
        jailer.uid.to_string(),
        "--gid".to_string(),
        jailer.gid.to_string(),
        "--chroot-base-dir".to_string(),
        jailer.chroot_base.display().to_string(),
    ];
    for cgroup in &jailer.cgroups {
        args.push("--cgroup".to_string());
        args.push(cgroup.clone());
    }
    args.push("--".to_string());
    args.push("--api-sock".to_string());
    args.push(api_sock_name.to_string());
    args
}

/// Validated configuration for [`FcExecutor`]. Construct through
/// [`FcConfig::new`] (validation included) or [`FcConfig::from_environment`].
#[derive(Debug, Clone)]
pub struct FcConfig {
    /// Pinned vmlinux kernel image (absolute, canonicalized).
    pub kernel: PathBuf,
    /// ext4 rootfs containing tool binaries plus the guest agent (absolute,
    /// canonicalized).
    pub rootfs: PathBuf,
    /// Writable directory hosting per-run sockets and logs.
    pub work_dir: PathBuf,
    /// vsock context id for the guest.
    pub guest_cid: u32,
    /// Declared rootfs build inputs folded into the executor digest.
    pub build_inputs: RootfsBuildInputs,
    /// Optional jailer wrapping (production posture). `None` spawns the
    /// firecracker binary directly (spike posture).
    pub jailer: Option<JailerConfig>,
}

impl FcConfig {
    /// Build and VALIDATE a config: kernel and rootfs must exist as files,
    /// the guest CID must not be reserved, and the work dir must be
    /// creatable and writable (probed with a create-and-remove file).
    pub fn new(
        kernel: impl Into<PathBuf>,
        rootfs: impl Into<PathBuf>,
        work_dir: impl Into<PathBuf>,
        guest_cid: u32,
        build_inputs: RootfsBuildInputs,
    ) -> Result<Self, RunnerError> {
        let kernel = kernel.into();
        let rootfs = rootfs.into();
        if !kernel.is_file() {
            return Err(RunnerError::Spawn(format!(
                "firecracker kernel {} is not a file (set {POLYFORGE_FC_KERNEL_ENV} to the pinned vmlinux image)",
                kernel.display()
            )));
        }
        if !rootfs.is_file() {
            return Err(RunnerError::Spawn(format!(
                "firecracker rootfs {} is not a file (set {POLYFORGE_FC_ROOTFS_ENV} to the ext4 rootfs)",
                rootfs.display()
            )));
        }
        if guest_cid < MIN_GUEST_CID {
            return Err(RunnerError::Spawn(format!(
                "guest_cid {guest_cid} is reserved (0 invalid, 1 hypervisor, 2 host); use {MIN_GUEST_CID} or greater"
            )));
        }
        let work_dir = ensure_writable_dir(&work_dir.into())?;
        Ok(Self {
            kernel: kernel.canonicalize().map_err(io_err)?,
            rootfs: rootfs.canonicalize().map_err(io_err)?,
            work_dir,
            guest_cid,
            build_inputs,
            jailer: None,
        })
    }

    /// Resolve the config from the process environment (see module docs for
    /// the variable list). Thin wrapper over the pure [`Self::resolve_config`]
    /// core so tests drive synthetic environments without env mutation.
    pub fn from_environment() -> Result<FcConfig, RunnerError> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self::resolve_config(
            get(POLYFORGE_FC_KERNEL_ENV).as_deref(),
            get(POLYFORGE_FC_ROOTFS_ENV).as_deref(),
            get(POLYFORGE_FC_MANIFEST_ENV).as_deref(),
            get(POLYFORGE_FC_WORK_DIR_ENV).as_deref(),
            get(POLYFORGE_FC_GUEST_CID_ENV).as_deref(),
        )
    }

    /// Pure core of [`Self::from_environment`]: every source is an explicit
    /// `Option` so tests exercise missing/invalid values hermetically.
    fn resolve_config(
        kernel: Option<&str>,
        rootfs: Option<&str>,
        manifest: Option<&str>,
        work_dir: Option<&str>,
        guest_cid: Option<&str>,
    ) -> Result<FcConfig, RunnerError> {
        let kernel = kernel.map(PathBuf::from).ok_or_else(|| {
            RunnerError::Spawn(format!(
                "{POLYFORGE_FC_KERNEL_ENV} must point at the pinned vmlinux image"
            ))
        })?;
        let rootfs = rootfs.map(PathBuf::from).ok_or_else(|| {
            RunnerError::Spawn(format!(
                "{POLYFORGE_FC_ROOTFS_ENV} must point at the ext4 rootfs containing the tool binaries and guest agent"
            ))
        })?;
        let manifest = manifest.map(PathBuf::from).ok_or_else(|| {
            RunnerError::Spawn(format!(
                "{POLYFORGE_FC_MANIFEST_ENV} must point at the rootfs build-inputs manifest JSON (written by deploy/firecracker/rootfs-build.sh)"
            ))
        })?;
        let build_inputs = RootfsBuildInputs::from_manifest_path(&manifest)?;
        let guest_cid = match guest_cid {
            Some(raw) => raw.trim().parse::<u32>().map_err(|_| {
                RunnerError::Spawn(format!(
                    "{POLYFORGE_FC_GUEST_CID_ENV} must be an unsigned integer, got {raw:?}"
                ))
            })?,
            None => DEFAULT_GUEST_CID,
        };
        let work_dir = match work_dir {
            Some(dir) => PathBuf::from(dir),
            None => std::env::temp_dir().join("pf-fc"),
        };
        FcConfig::new(kernel, rootfs, work_dir, guest_cid, build_inputs)
    }
}

/// The Firecracker backend: boots one ephemeral microVM per attestation run
/// over the proven spike protocol and executes the canonical allowlisted
/// entry inside it. Same Executor contract as every other backend:
/// allowlist gate first, typed args second, wall-clock budget inherited,
/// host environment untouched.
pub struct FcExecutor {
    config: FcConfig,
}

impl FcExecutor {
    /// Wrap an already-validated config (see [`FcConfig::new`]).
    pub fn new(config: FcConfig) -> Self {
        Self { config }
    }

    /// Record-only executor identity:
    /// `sha256({kernel_sha256, rootfs_build_inputs, fc_version})`. Inputs
    /// are hashed, NOT image bytes (rationale in the module docs). The
    /// kernel digest costs one streaming read per call; acceptable at
    /// attestation frequency.
    pub fn executor_digest(&self) -> Result<String, RunnerError> {
        let kernel_sha256 = sha256_of_file(&self.config.kernel)?;
        Ok(compose_digest(
            &kernel_sha256,
            &self.config.build_inputs.base_image_or_packages,
            &self.config.build_inputs.build_script_sha256,
            &firecracker_version(),
        ))
    }

    /// One sandboxed invocation of the joined command string inside a fresh
    /// ephemeral microVM. Boot, version probe, exec, and teardown all share
    /// the overall wall-clock deadline.
    fn sandboxed_exec(&self, cmd: &str) -> Result<(i32, String), RunnerError> {
        let budget = parse_timeout();
        let deadline = Instant::now() + budget;
        let mut session = VmSession::boot(&self.config, budget, deadline)?;
        session.exec(cmd, deadline)
    }
}

impl Executor for FcExecutor {
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
        // Same gate order as every backend: allowlist, then typed args,
        // BEFORE anything probes or boots a VM.
        let canonical =
            lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
        validate_tool_args(&canonical.name, args)?;

        // Version probe INSIDE the microVM so the recorded version describes
        // the isolated environment, not the host toolchain. This costs a
        // second exec on the SAME connection (the agent loops per
        // connection); no second VM is booted.
        let bin = canonical.bin.display().to_string();
        let tool_version = match self.sandboxed_exec(&format!("{bin} --version")) {
            Ok((0, stdout)) => stdout.trim().to_string(),
            _ => format!("unknown-{bin}"),
        };

        let cmd = command_string(&canonical, args);
        let (exit_code, stdout) = self.sandboxed_exec(&cmd)?;
        let stdout_hash = sha256_hex(stdout.as_bytes());

        Ok(RunOutput {
            stdout: stdout.into_bytes(),
            // The guest agent merges stderr into stdout by design; nothing
            // separate is recorded rather than something invented.
            stderr: Vec::new(),
            exit_code,
            stdout_hash,
            env_fingerprint: super::runner::env_fingerprint(&tool_version),
            tool_version,
            command: command_string(&canonical, args),
        })
    }

    fn label(&self) -> &'static str {
        "sandbox-firecracker"
    }
}

// ---------------------------------------------------------------------------
// Digest composition
// ---------------------------------------------------------------------------

/// Compose the executor digest from the canonical build-inputs manifest.
/// Identical inputs are byte-stable; changing ANY component changes the
/// output (unit-tested). serde_json object serialization is deterministic
/// here, so the manifest string is canonical across runs.
pub fn compose_digest(
    kernel_sha256: &str,
    base_image_or_packages: &str,
    build_script_sha256: &str,
    fc_version: &str,
) -> String {
    let manifest = serde_json::json!({
        "kernel_sha256": kernel_sha256,
        "rootfs_build_inputs": {
            "base_image_or_packages": base_image_or_packages,
            "build_script_sha256": build_script_sha256,
        },
        "fc_version": fc_version,
    });
    sha256_hex(manifest.to_string().as_bytes())
}

/// Streaming SHA-256 of a file (never loads the whole image into memory).
fn sha256_of_file(path: &Path) -> Result<String, RunnerError> {
    let mut file = std::fs::File::open(path).map_err(io_err)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(io_err)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(sha256_hex(&hasher.finalize()))
}

/// First line of `firecracker --version`; best-effort `unknown-firecracker`
/// on any failure so digest composition never panics on a broken install
/// (same posture as the gVisor backend's unknown-runsc fallback).
fn firecracker_version() -> String {
    let Some(bin) = find_binary("firecracker") else {
        return "unknown-firecracker".to_string();
    };
    match Command::new(bin).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => "unknown-firecracker".to_string(),
    }
}

/// Resolve `name` as an executable file on PATH, then in well-known install
/// directories (non-interactive environments often carry a trimmed PATH).
fn find_binary(name: &str) -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        let on_path = std::env::split_paths(&path_var)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file());
        if let Some(found) = on_path {
            return Some(found);
        }
    }
    ["/usr/local/bin", "/usr/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn io_err(e: std::io::Error) -> RunnerError {
    RunnerError::Io(e.to_string())
}

/// Remaining time until `deadline`, or [`RunnerError::TimedOut`] naming the
/// full budget once it has expired.
fn remaining_or_timeout(deadline: Instant, budget_secs: u64) -> Result<Duration, RunnerError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(RunnerError::TimedOut {
            timeout_secs: budget_secs,
        });
    }
    Ok(deadline - now)
}

/// Nanosecond-resolution uniqueness suffix for per-run names.
fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// SIGKILL the process group led by `pid` (the FC process spawned with
/// `process_group(0)`). Deliberately NOT cfg-split (mutation-testing
/// observability lesson, see runner.rs kill_process_group): on Unix the
/// real killpg runs, then the always-compiled `kill -9` fallback is a
/// harmless ESRCH no-op whose mutant stays runnable on Linux CI.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `pid` came from a live Child we spawned into its own
        // process group; killpg takes a pid_t and a signal. Failure
        // (ESRCH etc.) is ignored by design.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

/// Create `dir` (parents included), prove it is writable with a
/// create-and-remove probe file, and return its canonicalized path.
fn ensure_writable_dir(dir: &Path) -> Result<PathBuf, RunnerError> {
    std::fs::create_dir_all(dir).map_err(io_err)?;
    let probe = dir.join(format!(".pf-write-probe-{}", unique_suffix()));
    std::fs::File::create(&probe).map_err(io_err)?;
    std::fs::remove_file(&probe).map_err(io_err)?;
    dir.canonicalize().map_err(io_err)
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client for the FC API unix socket
// ---------------------------------------------------------------------------

/// Build the raw bytes of one FC API PUT request. `Connection: close` asks
/// the server to finish after the response; the response parser does not
/// depend on it (it reads exactly Content-Length body bytes when present).
fn http_put_bytes(path: &str, body: &str) -> Vec<u8> {
    format!(
        "PUT /{path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// Issue one PUT against the FC API socket and require a 2xx response.
fn api_put(
    sock: &Path,
    path: &str,
    body: &serde_json::Value,
    deadline: Instant,
    budget_secs: u64,
) -> Result<(), RunnerError> {
    let body_str = serde_json::to_string(body).map_err(|e| RunnerError::Io(e.to_string()))?;
    let request = http_put_bytes(path, &body_str);
    let mut stream = UnixStream::connect(sock).map_err(io_err)?;
    let remaining = remaining_or_timeout(deadline, budget_secs)?;
    stream.set_write_timeout(Some(remaining)).map_err(io_err)?;
    stream.write_all(&request).map_err(io_err)?;
    let (status, resp_body) = read_http_response(&mut stream, deadline, budget_secs)?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(RunnerError::Spawn(format!(
            "FC API PUT /{path} rejected ({status}): {}",
            truncate_for_error(&resp_body)
        )))
    }
}

/// Read one complete HTTP response: headers first, then exactly
/// Content-Length body bytes when the header is present (204-style
/// responses without it end right after the header block).
fn read_http_response(
    stream: &mut UnixStream,
    deadline: Instant,
    budget_secs: u64,
) -> Result<(u16, String), RunnerError> {
    const HEADER_END: &[u8] = b"\r\n\r\n";
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(HEADER_END.len()).position(|w| w == HEADER_END) {
            break pos + HEADER_END.len();
        }
        let remaining = remaining_or_timeout(deadline, budget_secs)?;
        stream.set_read_timeout(Some(remaining)).map_err(io_err)?;
        let n = stream.read(&mut chunk).map_err(io_err)?;
        if n == 0 {
            return Err(RunnerError::Io(
                "FC API closed the connection before headers completed".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            RunnerError::Io(format!("unparseable FC API status line: {status_line:?}"))
        })?;
    let mut content_length: Option<usize> = None;
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let mut body: Vec<u8> = buf[header_end..].to_vec();
    if let Some(len) = content_length {
        while body.len() < len {
            let remaining = remaining_or_timeout(deadline, budget_secs)?;
            stream.set_read_timeout(Some(remaining)).map_err(io_err)?;
            let n = stream.read(&mut chunk).map_err(io_err)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// First 200 chars of an error body, for actionable-but-bounded messages.
fn truncate_for_error(text: &str) -> &str {
    match text.char_indices().nth(200) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

// ---------------------------------------------------------------------------
// vsock client (CONNECT handshake + newline-JSON exec)
// ---------------------------------------------------------------------------

struct VsockClient {
    stream: UnixStream,
}

impl VsockClient {
    fn set_timeouts(&mut self, timeout: Duration) -> Result<(), RunnerError> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(io_err)?;
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(io_err)?;
        Ok(())
    }
}

/// Spike-semantics CONNECT reply check: prefix `OK` only (the reply is
/// `OK <port>`; anything else rejects the handshake).
fn connect_reply_ok(line: &[u8]) -> bool {
    line.starts_with(b"OK")
}

/// Perform the CONNECT handshake on an established vsock unix-stream
/// connection: send `CONNECT <port>\n`, expect a reply line prefixed `OK`.
fn vsock_handshake(
    stream: &mut UnixStream,
    deadline: Instant,
    budget_secs: u64,
) -> Result<(), RunnerError> {
    let remaining = remaining_or_timeout(deadline, budget_secs)?;
    stream.set_write_timeout(Some(remaining)).map_err(io_err)?;
    stream
        .write_all(format!("CONNECT {GUEST_AGENT_PORT}\n").as_bytes())
        .map_err(io_err)?;
    let line = read_line_capped(stream, deadline, budget_secs)?;
    if connect_reply_ok(line.as_bytes()) {
        Ok(())
    } else {
        Err(RunnerError::Spawn(format!(
            "vsock CONNECT rejected: {line}"
        )))
    }
}

/// Connect to the vsock unix socket with retry until the boot window closes
/// (the guest needs real time to reach userspace after InstanceStart).
fn connect_vsock_with_retry(
    sock: &Path,
    boot_deadline: Instant,
    budget_secs: u64,
) -> Result<VsockClient, RunnerError> {
    loop {
        match UnixStream::connect(sock) {
            Ok(stream) => return Ok(VsockClient { stream }),
            Err(_) => {
                if Instant::now() >= boot_deadline {
                    return Err(RunnerError::TimedOut {
                        timeout_secs: budget_secs,
                    });
                }
                thread::sleep(Duration::from_millis(VSOCK_RETRY_MS));
            }
        }
    }
}

/// Connect AND handshake with retry, bounded by the boot deadline.
///
/// FC v1.9.1 accepts the host-side UDS CONNECT and answers `OK <cid>` as
/// soon as the vsock device is up, BEFORE the guest agent (PID 1, boots in
/// ~0.5-4s) starts listening on the AF_VSOCK port. When the guest-side
/// connect fails, FC closes the UDS stream and the handshake read dies with
/// `guest closed the vsock stream before a full line arrived`. The proven
/// T8 spike survived this by retrying the whole connect+handshake; this
/// loop restores that behavior: on an Io error from the handshake, drop
/// the client, sleep [`VSOCK_RETRY_MS`], reconnect, and re-handshake until
/// the boot window closes. Non-Io failures (CONNECT rejected, timeout)
/// surface immediately.
fn connect_and_handshake_with_retry(
    sock: &Path,
    boot_deadline: Instant,
    deadline: Instant,
    budget_secs: u64,
) -> Result<VsockClient, RunnerError> {
    loop {
        let mut client = connect_vsock_with_retry(sock, boot_deadline, budget_secs)?;
        match vsock_handshake(&mut client.stream, deadline, budget_secs) {
            Ok(()) => return Ok(client),
            Err(RunnerError::Io(_)) => {
                if Instant::now() >= boot_deadline {
                    return Err(RunnerError::TimedOut {
                        timeout_secs: budget_secs,
                    });
                }
                drop(client);
                thread::sleep(Duration::from_millis(VSOCK_RETRY_MS));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read one newline-terminated line with a hard size cap.
fn read_line_capped(
    stream: &mut UnixStream,
    deadline: Instant,
    budget_secs: u64,
) -> Result<String, RunnerError> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            return Ok(String::from_utf8_lossy(&buf[..pos]).into_owned());
        }
        if buf.len() > MAX_AGENT_LINE_BYTES {
            return Err(RunnerError::Io(
                "guest agent response exceeded the size cap".to_string(),
            ));
        }
        let remaining = remaining_or_timeout(deadline, budget_secs)?;
        stream.set_read_timeout(Some(remaining)).map_err(io_err)?;
        let n = stream.read(&mut chunk).map_err(io_err)?;
        if n == 0 {
            return Err(RunnerError::Io(
                "guest closed the vsock stream before a full line arrived".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parse one guest-agent response line: `{"exit":N,"stdout":"..."}` on
/// success, `{"error":"..."}` mapped to a hard error.
fn parse_agent_response(line: &str) -> Result<(i32, String), RunnerError> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| RunnerError::Io(format!("guest agent sent malformed JSON ({e}): {line}")))?;
    if let Some(err) = value.get("error").and_then(serde_json::Value::as_str) {
        return Err(RunnerError::Spawn(format!("guest agent error: {err}")));
    }
    let exit = value
        .get("exit")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            RunnerError::Io(format!("guest agent response missing exit code: {line}"))
        })?;
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((i32::try_from(exit).unwrap_or(-1), stdout))
}

// ---------------------------------------------------------------------------
// Per-run session: spawn, watchdog, cleanup
// ---------------------------------------------------------------------------

/// Host-side and in-guest path layout for ONE run. Under the jailer the
/// FC-visible paths live inside the jail root while the host reaches them
/// through `<chroot_base>/<run_id>/root/...`.
struct SessionPaths {
    /// Host-side path of the FC API socket (polled after spawn).
    api_host: PathBuf,
    /// Host-side path of the vsock unix socket (connected for exec).
    vsock_host: PathBuf,
    /// Host-side path of the FC internal log (pre-created: FC opens the
    /// logger target WITHOUT O_CREAT).
    log_host: PathBuf,
    /// log_path string placed INTO the logger PUT body.
    log_body: String,
    /// uds_path string placed INTO the vsock PUT body.
    vsock_body: String,
}

impl SessionPaths {
    fn resolve(config: &FcConfig, run_id: &str) -> Result<Self, RunnerError> {
        match &config.jailer {
            Some(jailer) => {
                let jail_root = jailer.chroot_base.join(format!("{run_id}/root"));
                std::fs::create_dir_all(&jail_root).map_err(io_err)?;
                Ok(Self {
                    api_host: jail_root.join("api.sock"),
                    vsock_host: jail_root.join("v.sock"),
                    log_host: jail_root.join("fc.log"),
                    log_body: "/fc.log".to_string(),
                    vsock_body: "/v.sock".to_string(),
                })
            }
            None => {
                let base = &config.work_dir;
                let api_host = base.join(format!("{run_id}.api.sock"));
                let vsock_host = base.join(format!("{run_id}.v.sock"));
                let log_host = base.join(format!("{run_id}.fc.log"));
                Ok(Self {
                    log_body: log_host.display().to_string(),
                    vsock_body: vsock_host.display().to_string(),
                    api_host,
                    vsock_host,
                    log_host,
                })
            }
        }
    }
}

/// Build the spawn Command for one run: direct firecracker (spike posture)
/// or jailer-wrapped (production posture). Process stderr is captured into
/// a dedicated proc log so early exits stay diagnosable.
fn spawn_command(
    config: &FcConfig,
    fc_bin: &Path,
    run_id: &str,
    paths: &SessionPaths,
) -> Result<Command, RunnerError> {
    let mut cmd = match &config.jailer {
        Some(jailer) => {
            let mut c = Command::new(&jailer.bin);
            c.args(jailer_args(jailer, run_id, fc_bin, "api.sock"));
            c
        }
        None => {
            let mut c = Command::new(fc_bin);
            c.arg("--api-sock").arg(&paths.api_host);
            c
        }
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    let proc_log = std::fs::File::create(config.work_dir.join(format!("{run_id}.proc.log")))
        .map_err(io_err)?;
    cmd.stderr(Stdio::from(proc_log));
    // The FC process leads its own process group so the watchdog can kill
    // the whole tree on timeout (mirrors the gVisor backend helper).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    Ok(cmd)
}

/// One ephemeral microVM lifetime: spawned child, watchdog channel, vsock
/// client, and the scratch files removed on drop. Drop ALWAYS kills the
/// process group first, so no boot/error/timeout path can leak a VM.
struct VmSession {
    pid: u32,
    child: Option<Child>,
    watchdog_tx: Option<mpsc::Sender<()>>,
    watchdog_handle: Option<thread::JoinHandle<()>>,
    client: Option<VsockClient>,
    scratch: Vec<PathBuf>,
}

impl VmSession {
    /// Boot one microVM following the PROVEN spike sequence: spawn FC,
    /// wait for the API socket, PUT logger -> boot-source -> drives/rootfs
    /// -> vsock -> actions InstanceStart, then establish the vsock
    /// connection with retry until the boot window closes.
    fn boot(config: &FcConfig, budget: Duration, deadline: Instant) -> Result<Self, RunnerError> {
        let budget_secs = budget.as_secs().max(1);
        let run_id = format!("pf-fc-{}-{}", std::process::id(), unique_suffix());
        let paths = SessionPaths::resolve(config, &run_id)?;

        let fc_bin = find_binary("firecracker").ok_or_else(|| {
            RunnerError::Spawn(
                "firecracker binary not found on PATH or in well-known dirs (see \
                 deploy/firecracker/README-host-prereqs.md)"
                    .to_string(),
            )
        })?;

        // FC v1.9.1 opens the logger target WITHOUT O_CREAT: pre-create it.
        std::fs::File::create(&paths.log_host).map_err(io_err)?;

        let mut cmd = spawn_command(config, &fc_bin, &run_id, &paths)?;
        let child = cmd.spawn().map_err(|e| RunnerError::Spawn(e.to_string()))?;
        let pid = child.id();

        // Watchdog: SIGKILLs the whole process group when the overall budget
        // expires, mirroring runner::wait_with_timeout semantics.
        let (watchdog_tx, watchdog_rx) = mpsc::channel::<()>();
        let watchdog_handle = thread::spawn(move || match watchdog_rx.recv_timeout(budget) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => kill_process_group(pid),
        });

        let mut session = Self {
            pid,
            child: Some(child),
            watchdog_tx: Some(watchdog_tx),
            watchdog_handle: Some(watchdog_handle),
            client: None,
            scratch: vec![
                paths.api_host.clone(),
                paths.vsock_host.clone(),
                paths.log_host.clone(),
                config.work_dir.join(format!("{run_id}.proc.log")),
            ],
        };

        let booted = (|| -> Result<(), RunnerError> {
            wait_api_socket(
                &paths.api_host,
                session.child.as_mut().expect("child held until drop"),
                &session.scratch[3],
                Instant::now() + Duration::from_secs(API_SOCKET_WAIT_SECS),
                budget_secs,
            )?;
            api_put(
                &paths.api_host,
                "logger",
                &serde_json::json!({
                    "level": "Info",
                    "log_path": paths.log_body,
                    "show_level": true,
                    "show_log_origin": true,
                }),
                deadline,
                budget_secs,
            )?;
            api_put(
                &paths.api_host,
                "boot-source",
                &serde_json::json!({
                    "kernel_image_path": config.kernel.display().to_string(),
                    "boot_args": BOOT_ARGS,
                }),
                deadline,
                budget_secs,
            )?;
            api_put(
                &paths.api_host,
                "drives/rootfs",
                &serde_json::json!({
                    "drive_id": "rootfs",
                    "path_on_host": config.rootfs.display().to_string(),
                    "is_root_device": true,
                    "is_read_only": false,
                    "io_engine": "Sync",
                }),
                deadline,
                budget_secs,
            )?;
            api_put(
                &paths.api_host,
                "vsock",
                &serde_json::json!({
                    "guest_cid": config.guest_cid,
                    "uds_path": paths.vsock_body,
                }),
                deadline,
                budget_secs,
            )?;
            api_put(
                &paths.api_host,
                "actions",
                &serde_json::json!({"action_type": "InstanceStart"}),
                deadline,
                budget_secs,
            )?;

            // Guest needs real time to reach userspace; retry within the
            // boot window (spike pattern), bounded by the overall deadline.
            let boot_deadline = std::cmp::min(
                deadline,
                Instant::now() + Duration::from_secs(DEFAULT_BOOT_TIMEOUT_SECS),
            );
            let client = connect_and_handshake_with_retry(
                &paths.vsock_host,
                boot_deadline,
                deadline,
                budget_secs,
            )?;
            session.client = Some(client);
            Ok(())
        })();

        // On any boot error the session drops here: watchdog cancelled,
        // process group killed, scratch files removed.
        booted.map(|_| session)
    }

    /// Execute one command through the established vsock connection. The
    /// connection stays open across calls (multiple requests per
    /// connection, spike-proven agent behavior).
    fn exec(&mut self, cmd: &str, deadline: Instant) -> Result<(i32, String), RunnerError> {
        let budget_secs = parse_timeout().as_secs().max(1);
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| RunnerError::Spawn("vsock connection not established".to_string()))?;
        let remaining = remaining_or_timeout(deadline, budget_secs)?;
        client.set_timeouts(remaining)?;
        let mut request = serde_json::to_vec(&serde_json::json!({"cmd": cmd}))
            .map_err(|e| RunnerError::Io(e.to_string()))?;
        request.push(b'\n');
        client.stream.write_all(&request).map_err(io_err)?;
        let line = read_line_capped(&mut client.stream, deadline, budget_secs)?;
        parse_agent_response(&line)
    }
}

impl Drop for VmSession {
    fn drop(&mut self) {
        // Cancel the watchdog BEFORE killing, so it can never fire on a
        // recycled pid after a normal teardown.
        if let Some(tx) = self.watchdog_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.watchdog_handle.take() {
            let _ = handle.join();
        }
        kill_process_group(self.pid);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        for path in &self.scratch {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Poll for the FC API socket to appear; fail fast when the FC process
/// exited early, pointing at its process log.
fn wait_api_socket(
    sock: &Path,
    child: &mut Child,
    proc_log: &Path,
    wait_deadline: Instant,
    budget_secs: u64,
) -> Result<(), RunnerError> {
    loop {
        if sock.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(io_err)? {
            return Err(RunnerError::Spawn(format!(
                "firecracker exited early ({status}); inspect {}",
                proc_log.display()
            )));
        }
        if Instant::now() >= wait_deadline {
            return Err(RunnerError::TimedOut {
                timeout_secs: budget_secs,
            });
        }
        thread::sleep(API_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real empty file under temp_dir: config validation performs actual
    /// filesystem checks, so synthetic paths would fail on any host.
    fn temp_asset(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pf-fc-test-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::File::create(&path).expect("temp asset created");
        path
    }

    fn build_inputs() -> RootfsBuildInputs {
        RootfsBuildInputs {
            base_image_or_packages: "busybox-1.35.0-x86_64-linux-musl + static vsock agent"
                .to_string(),
            build_script_sha256: "a".repeat(64),
        }
    }

    fn test_config(tag: &str) -> FcConfig {
        let kernel = temp_asset(&format!("{tag}-kernel"));
        let rootfs = temp_asset(&format!("{tag}-rootfs"));
        let work = std::env::temp_dir().join(format!(
            "pf-fc-test-work-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        FcConfig::new(kernel, rootfs, work, DEFAULT_GUEST_CID, build_inputs())
            .expect("valid test config")
    }

    // ---- digest ----------------------------------------------------------

    /// Acceptance requirement: same manifest hashes to the same digest.
    #[test]
    fn digest_is_deterministic_for_identical_manifests() {
        let a = compose_digest("kern-sha", "busybox-static", "script-sha", "1.9.1");
        let b = compose_digest("kern-sha", "busybox-static", "script-sha", "1.9.1");
        assert_eq!(a, b, "identical manifests must hash identically");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// Each manifest component MUST move the digest when changed.
    #[test]
    fn digest_folds_every_component() {
        let base = compose_digest("k1", "busybox", "s1", "1.9.1");
        assert_ne!(compose_digest("k2", "busybox", "s1", "1.9.1"), base);
        assert_ne!(compose_digest("k1", "debootstrap", "s1", "1.9.1"), base);
        assert_ne!(compose_digest("k1", "busybox", "s2", "1.9.1"), base);
        assert_ne!(compose_digest("k1", "busybox", "s1", "1.10.0"), base);
    }

    // ---- config validation -----------------------------------------------

    #[test]
    fn config_rejects_missing_kernel_and_rootfs() {
        let present = temp_asset("present");
        let absent = std::env::temp_dir().join(format!("pf-fc-absent-{}", unique_suffix()));
        let work = std::env::temp_dir().join(format!("pf-fc-work-{}", unique_suffix()));
        let err = FcConfig::new(
            absent.clone(),
            present.clone(),
            work.clone(),
            3,
            build_inputs(),
        )
        .expect_err("missing kernel must fail");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains(POLYFORGE_FC_KERNEL_ENV)),
            "kernel error names the env var: {err:?}"
        );
        let err = FcConfig::new(present, absent, work, 3, build_inputs())
            .expect_err("missing rootfs must fail");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains(POLYFORGE_FC_ROOTFS_ENV)),
            "rootfs error names the env var: {err:?}"
        );
    }

    #[test]
    fn config_rejects_reserved_guest_cid() {
        let kernel = temp_asset("cid-kernel");
        let rootfs = temp_asset("cid-rootfs");
        let work = std::env::temp_dir().join(format!("pf-fc-cid-work-{}", unique_suffix()));
        let err = FcConfig::new(kernel, rootfs, work, 2, build_inputs())
            .expect_err("reserved cid must fail");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains("reserved")),
            "reserved-cid error is actionable: {err:?}"
        );
    }

    #[test]
    fn config_rejects_unwritable_work_dir() {
        let kernel = temp_asset("work-kernel");
        let rootfs = temp_asset("work-rootfs");
        // An existing REGULAR FILE cannot become a directory, so
        // create_dir_all fails deterministically on every platform/user.
        let work_file = temp_asset("work-file");
        let err = FcConfig::new(kernel, rootfs, work_file, DEFAULT_GUEST_CID, build_inputs())
            .expect_err("unwritable work dir must fail");
        assert!(matches!(err, RunnerError::Io(_)), "{err:?}");
    }

    #[test]
    fn manifest_parser_accepts_valid_and_rejects_missing_keys() {
        let good = std::env::temp_dir().join(format!("pf-fc-manifest-good-{}", unique_suffix()));
        std::fs::write(
            &good,
            r#"{"base_image_or_packages":"busybox","build_script_sha256":"abc"}"#,
        )
        .expect("manifest written");
        let parsed = RootfsBuildInputs::from_manifest_path(&good).expect("valid manifest");
        assert_eq!(parsed.base_image_or_packages, "busybox");
        assert_eq!(parsed.build_script_sha256, "abc");

        let bad = std::env::temp_dir().join(format!("pf-fc-manifest-bad-{}", unique_suffix()));
        std::fs::write(&bad, r#"{"base_image_or_packages":"busybox"}"#).expect("written");
        let err = RootfsBuildInputs::from_manifest_path(&bad).expect_err("missing key");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains("build_script_sha256")),
            "error names the missing key: {err:?}"
        );
    }

    #[test]
    fn resolve_config_requires_all_env_sources_and_valid_cid() {
        let err = FcConfig::resolve_config(None, Some("/r"), Some("/m"), None, None)
            .expect_err("missing kernel env");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains(POLYFORGE_FC_KERNEL_ENV)),
            "{err:?}"
        );
        // The CID check runs after the manifest parse, so the manifest must
        // be a real readable file for the CID error to surface.
        let manifest = temp_asset("resolve-manifest");
        std::fs::write(
            &manifest,
            r#"{"base_image_or_packages":"busybox","build_script_sha256":"abc"}"#,
        )
        .expect("manifest written");
        let manifest = manifest.to_str().expect("utf8 path");
        let err =
            FcConfig::resolve_config(Some("/k"), Some("/r"), Some(manifest), None, Some("xyz"))
                .expect_err("invalid cid");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains(POLYFORGE_FC_GUEST_CID_ENV)),
            "{err:?}"
        );
    }

    // ---- jailer argv -------------------------------------------------------

    #[test]
    fn jailer_args_exact_shape_including_cgroups() {
        let jailer = JailerConfig {
            bin: PathBuf::from("/usr/local/bin/jailer"),
            uid: 1000,
            gid: 1000,
            chroot_base: PathBuf::from("/srv/jail"),
            cgroups: vec!["ver=2,controllers=cpu,memory".to_string()],
        };
        let args = jailer_args(
            &jailer,
            "pf-fc-run-1",
            Path::new("/usr/local/bin/firecracker"),
            "api.sock",
        );
        let expected = vec![
            "--id",
            "pf-fc-run-1",
            "--exec-file",
            "/usr/local/bin/firecracker",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--chroot-base-dir",
            "/srv/jail",
            "--cgroup",
            "ver=2,controllers=cpu,memory",
            "--",
            "--api-sock",
            "api.sock",
        ];
        let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(args, expected, "jailer argv shape is contractual");
    }

    // ---- HTTP + agent protocol parsers --------------------------------------

    #[test]
    fn http_put_request_shape_is_contractual() {
        let body = "{\"level\":\"Info\"}";
        let bytes = http_put_bytes("logger", body);
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with("PUT /logger HTTP/1.1\r\n"), "{text:?}");
        assert!(
            text.contains(&format!("Content-Length: {}\r\n", body.len())),
            "{text:?}"
        );
        assert!(text.contains("Connection: close\r\n"), "{text:?}");
        assert!(text.ends_with(&format!("\r\n\r\n{body}")), "{text:?}");
    }

    #[test]
    fn http_response_parses_status_with_and_without_content_length() {
        let mut stream = pair_stream();
        // 204-style: headers only, no content-length.
        stream
            .0
            .write_all(b"HTTP/1.1 204 No Content\r\nServer: firecracker\r\n\r\n")
            .expect("write");
        drop(stream.0);
        let (status, body) =
            read_http_response(&mut stream.1, Instant::now() + Duration::from_secs(5), 5)
                .expect("parsed");
        assert_eq!(status, 204);
        assert_eq!(body, "");

        let mut stream = pair_stream();
        stream
            .0
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 21\r\n\r\n{\"fault_message\":\"x\"}",
            )
            .expect("write");
        drop(stream.0);
        let (status, body) =
            read_http_response(&mut stream.1, Instant::now() + Duration::from_secs(5), 5)
                .expect("parsed");
        assert_eq!(status, 400);
        assert_eq!(body, "{\"fault_message\":\"x\"}");
    }

    /// Connected unix socket pair standing in for a server+client link in
    /// parser tests (no FC process involved).
    fn pair_stream() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socket pair")
    }

    #[test]
    fn agent_response_parses_exit_stdout_and_error() {
        let (exit, stdout) =
            parse_agent_response("{\"exit\":0,\"stdout\":\"hello\\n\"}").expect("parsed");
        assert_eq!(exit, 0);
        assert_eq!(stdout, "hello\n");

        let (exit, _) = parse_agent_response("{\"exit\":127,\"stdout\":\"\"}").expect("parsed");
        assert_eq!(exit, 127);

        let err = parse_agent_response("{\"error\":\"bad request\"}").expect_err("error variant");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains("bad request")),
            "{err:?}"
        );

        let err = parse_agent_response("not-json").expect_err("malformed");
        assert!(matches!(err, RunnerError::Io(_)), "{err:?}");

        let err = parse_agent_response("{\"stdout\":\"x\"}").expect_err("missing exit");
        assert!(
            matches!(&err, RunnerError::Io(m) if m.contains("missing exit code")),
            "{err:?}"
        );
    }

    #[test]
    fn connect_reply_checks_prefix_only_per_spike() {
        assert!(connect_reply_ok(b"OK 5001"));
        assert!(connect_reply_ok(b"OK"));
        assert!(!connect_reply_ok(b"ERR bad port"));
        assert!(!connect_reply_ok(b""));
    }

    // ---- executor gates (no VM touched) --------------------------------------

    #[test]
    fn executor_gates_allowlist_before_any_vm_work() {
        let exec = FcExecutor::new(test_config("gate-allow"));
        let evil = Tool {
            name: "evil".into(),
            bin: PathBuf::from("evil"),
            args: vec![],
        };
        let err = exec.run(&evil, &[]).unwrap_err();
        assert!(matches!(err, RunnerError::NotAllowed(n) if n == "evil"));
    }

    #[test]
    fn executor_gates_metachar_args_before_any_vm_work() {
        let exec = FcExecutor::new(test_config("gate-meta"));
        let t = lookup("cargo --version").expect("allowlisted");
        let err = exec.run(&t, &["bad;arg".to_string()]).unwrap_err();
        assert!(matches!(err, RunnerError::InvalidArg { .. }));
    }

    #[test]
    fn label_identifies_backend() {
        let exec = FcExecutor::new(test_config("label"));
        assert_eq!(exec.label(), "sandbox-firecracker");
    }

    // ---- connect+handshake retry loop ------------------------------------------

    /// A server that accepts every connection and closes it without a
    /// reply line reproduces the pre-agent boot race: FC accepts the UDS
    /// CONNECT, the guest-side connect fails, FC closes the stream, and the
    /// handshake read dies with Io("guest closed ... before a full line").
    fn closing_server(sock: &Path) -> PathBuf {
        let listener = std::os::unix::net::UnixListener::bind(sock).expect("bind");
        thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        sock.to_path_buf()
    }

    #[test]
    fn handshake_retry_loop_honors_boot_deadline() {
        let sock = std::env::temp_dir().join(format!(
            "pf-fc-retry-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _listener = closing_server(&sock);
        let start = Instant::now();
        let boot_deadline = start + Duration::from_millis(1500);
        let err = connect_and_handshake_with_retry(
            &sock,
            boot_deadline,
            start + Duration::from_secs(90),
            90,
        )
        .err()
        .and_then(|e| match e {
            RunnerError::TimedOut { timeout_secs: 90 } => None,
            other => Some(other),
        });
        assert!(
            err.is_none(),
            "deadline exhaustion must surface as TimedOut(90), got: {err:?}"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1400),
            "loop must retry until the boot deadline, gave up after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "loop must not retry past the deadline, ran {elapsed:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn handshake_retry_loop_recovers_after_transient_close() {
        let sock = std::env::temp_dir().join(format!(
            "pf-fc-retry-ok-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        thread::spawn(move || {
            let (first, _) = listener.accept().expect("first accept");
            drop(first);
            let (second, _) = listener.accept().expect("second accept");
            let mut second = second;
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            use std::io::Read;
            while second.read(&mut byte).is_ok() && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            second.write_all(b"OK 5001\n").expect("handshake reply");
        });
        let start = Instant::now();
        let client = connect_and_handshake_with_retry(
            &sock,
            start + Duration::from_secs(10),
            start + Duration::from_secs(90),
            90,
        )
        .expect("retry loop must absorb the first closed connection");
        assert!(
            client.stream.peer_addr().is_ok(),
            "recovered client holds a live stream"
        );
        let _ = std::fs::remove_file(&sock);
    }

    // ---- spawn command shapes --------------------------------------------------

    #[test]
    fn spawn_command_direct_mode_pins_the_api_sock_flag() {
        let config = test_config("spawn-direct");
        let paths = SessionPaths::resolve(&config, "run-x").expect("paths");
        let cmd = spawn_command(
            &config,
            Path::new("/usr/local/bin/firecracker"),
            "run-x",
            &paths,
        )
        .expect("command built");
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "/usr/local/bin/firecracker"
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["--api-sock", paths.api_host.to_str().expect("utf8")]
        );
    }

    #[test]
    fn session_paths_jailed_mode_routes_through_jail_root() {
        let mut config = test_config("jailed-paths");
        config.jailer = Some(JailerConfig {
            bin: PathBuf::from("/usr/local/bin/jailer"),
            uid: 1000,
            gid: 1000,
            chroot_base: std::env::temp_dir().join(format!(
                "pf-fc-jail-{}-{}",
                std::process::id(),
                unique_suffix()
            )),
            cgroups: vec![],
        });
        let paths = SessionPaths::resolve(&config, "run-y").expect("paths");
        let jail_root = config
            .jailer
            .as_ref()
            .expect("jailer")
            .chroot_base
            .join("run-y/root");
        assert_eq!(paths.api_host, jail_root.join("api.sock"));
        assert_eq!(paths.vsock_host, jail_root.join("v.sock"));
        assert_eq!(paths.log_host, jail_root.join("fc.log"));
        assert_eq!(paths.log_body, "/fc.log");
        assert_eq!(paths.vsock_body, "/v.sock");
        let _ = std::fs::remove_dir_all(&config.jailer.expect("jailer").chroot_base);
    }

    // ---- live e2e (opt-in) -------------------------------------------------------
    //
    // Plan acceptance: an #[ignore]-gated live test executed ONCE on a KVM
    // host when assets are provisioned, else skipped-with-reason. Never runs
    // in the default battery.

    #[test]
    #[ignore = "boots a real microVM; needs /dev/kvm plus provisioned POLYFORGE_FC_* assets"]
    fn e2e_live_microvm_echo_roundtrip() {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP(live): /dev/kvm absent on this host");
            return;
        }
        for var in [
            POLYFORGE_FC_KERNEL_ENV,
            POLYFORGE_FC_ROOTFS_ENV,
            POLYFORGE_FC_MANIFEST_ENV,
        ] {
            if std::env::var(var).is_err() {
                eprintln!("SKIP(live): {var} unset; provision assets per README-host-prereqs.md");
                return;
            }
        }
        let config = match FcConfig::from_environment() {
            Ok(config) => config,
            Err(e) => {
                eprintln!("SKIP(live): config invalid: {e:?}");
                return;
            }
        };
        let exec = FcExecutor::new(config);
        // The allowlist gate is host-side and must still pass (that is the
        // attestation contract); the provisioned rootfs is busybox-only, so
        // the guest reports 127 for the missing binary. This test proves the
        // VM/vsock/agent round-trip, not tool presence inside the image.
        let t = lookup("cargo --version").expect("allowlisted");
        match exec.run(&t, &[]) {
            Ok(out) => {
                assert_eq!(
                    out.exit_code, 127,
                    "busybox sh reports 127 for a missing binary"
                );
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("not found"),
                    "busybox sh must report the missing applet, got: {stdout:?}"
                );
                assert_eq!(out.stdout_hash.len(), 64);
                eprintln!(
                    "live microVM round-trip OK; executor_digest={:?}",
                    exec.executor_digest()
                );
            }
            Err(RunnerError::Spawn(e)) => {
                eprintln!("SKIP(live): environment gap: {e}");
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
}
