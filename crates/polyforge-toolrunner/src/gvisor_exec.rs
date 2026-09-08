//! T7: gVisor (runsc) execution backend behind the `sandbox-gvisor` feature.
//!
//! Runs allowlisted tools inside a gVisor sandbox so an attestation run is
//! isolated from the host by the runsc userspace kernel. Two routes exist:
//!
//! 1. PRIMARY: a dedicated container runtime (`runsc-nonet`) registered ONCE
//!    at install time with daemon-level `runtimeArgs --network=none`, then
//!    `docker run --runtime=runsc-nonet --network none -v <checkout>:/work:ro
//!    ...`. Daemon-level registration is mandatory because per-run runtime
//!    argument overrides are impossible otherwise; `--network none` is also
//!    passed per run as defense in depth.
//! 2. FALLBACK (no docker/podman, or runsc not registered with it): drive the
//!    runsc binary directly over an OCI bundle. `runsc spec` generates the
//!    default `config.json` (rootfs REQUIRED; generated spec defaults to
//!    nodev + ro mounts), we edit it via serde_json (process args, cwd,
//!    read-only /work bind mount, absolute rootfs path), then `runsc run <id>`
//!    which blocks and propagates the child exit code.
//!
//! # Why `runsc do` is FORBIDDEN for attestation runs
//!
//! `runsc do` shares the host filesystem by design (it is a convenience
//! shortcut that binds the host root into the sandbox). An attestation run
//! through it could read and mutate host state, so any isolation claim it
//! produced would be false. It exists for interactive TESTING ONLY and this
//! module never emits it.
//!
//! # Rootless tradeoff (default rootful)
//!
//! Rootless runsc is possible in three modes but sacrifices network
//! isolation: without root privileges the network namespace cannot be set up
//! with the same guarantees, so "no network" cannot be asserted honestly.
//! This backend defaults to the rootful posture where the dedicated runtime's
//! daemon-level `--network=none` holds. Operators who must run rootless
//! accept weaker isolation at their own risk; nothing here enables it
//! implicitly.
//!
//! # Install prerequisites (operator duty)
//!
//! gVisor from the apt repo or a release tarball (Linux 5.6+, x86_64/ARM64).
//! It coexists additively with runc: register the dedicated runtime once in
//! `/etc/docker/daemon.json` under `runtimes.runsc-nonet` with
//! `runtimeArgs: ["--network=none"]` and restart the daemon.
//!
//! # Executor digest
//!
//! NO official fingerprint mechanism exists for a runsc installation, so the
//! identity is composed per the librarian brief:
//! `sha256(runsc_version | platform | image_digest | config_json)` where
//! `config_json` is the canonical structural sandbox configuration of the
//! selected route (argv excluded on purpose so one executor has one stable
//! digest). The formula intentionally diverges from the other tiers; the tier
//! prefix recorded at dispatch disambiguates them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use super::prober::{ProbeSource, ProdProbe};
use super::runner::{
    command_string, exit_code_of, lookup, parse_timeout, sha256_hex, validate_tool_args,
    wait_with_timeout, Executor, RunOutput, RunnerError, Tool,
};

/// Dedicated runtime name registered once at install time with daemon-level
/// `--network=none`. Overridable per config for operators who registered a
/// differently named runtime.
pub const DEFAULT_RUNTIME_NAME: &str = "runsc-nonet";

/// Mount target of the read-only checkout inside the sandbox.
pub const WORK_MOUNT: &str = "/work";

/// Which concrete route a run takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GvisorRoute {
    /// Container runtime (docker/podman) through the dedicated runsc runtime.
    ContainerRuntime,
    /// Direct OCI bundle driven by the runsc binary itself.
    OciBundle,
}

impl GvisorRoute {
    /// Short route tag folded into diagnostics and structural config JSON.
    pub fn tag(self) -> &'static str {
        match self {
            GvisorRoute::ContainerRuntime => "docker-runtime",
            GvisorRoute::OciBundle => "oci-bundle",
        }
    }
}

/// Configuration for [`GvisorExecutor`].
#[derive(Debug, Clone)]
pub struct GvisorConfig {
    /// Image reference pinned BY DIGEST (`repo@sha256:<64 hex>`). Tags are
    /// rejected: an attestation must name exactly what executed.
    pub image: String,
    /// Host checkout mounted read-only at [`WORK_MOUNT`].
    pub checkout: PathBuf,
    /// Absolute path to a provisioned rootfs directory (OCI bundle route
    /// only). The rootfs must contain the tool binaries; the checkout is
    /// bind-mounted over it read-only at run time.
    pub rootfs: Option<PathBuf>,
    /// Registered runtime name used by the primary route.
    pub runtime_name: String,
}

impl GvisorConfig {
    /// Build a config with the default runtime name and no OCI-bundle rootfs.
    /// Fails closed when `image` is not digest-pinned.
    pub fn new(
        image: impl Into<String>,
        checkout: impl Into<PathBuf>,
    ) -> Result<Self, RunnerError> {
        let image = image.into();
        validate_image_pin(&image)?;
        Ok(Self {
            image,
            checkout: checkout.into(),
            rootfs: None,
            runtime_name: DEFAULT_RUNTIME_NAME.to_string(),
        })
    }
}

/// Shape check for digest pinning: `<repo>@sha256:<64 hex>`. A shape
/// violation check, not escaping: mirrors the typed-arg policy semantics.
pub(crate) fn validate_image_pin(image: &str) -> Result<(), RunnerError> {
    let pinned = image.split_once('@').is_some_and(|(_, digest)| {
        let hex_part = digest.strip_prefix("sha256:").unwrap_or("");
        hex_part.len() == 64 && hex_part.bytes().all(|b| b.is_ascii_hexdigit())
    });
    if pinned {
        Ok(())
    } else {
        Err(RunnerError::InvalidArg {
            tool: "gvisor-image".to_string(),
            arg: image.to_string(),
        })
    }
}

/// Digest portion of a validated image reference (`sha256:<hex>`).
fn image_digest_of(image: &str) -> String {
    image
        .split_once('@')
        .map(|(_, digest)| digest.to_string())
        .unwrap_or_default()
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

/// The runsc binary location, probed fresh each call (cheap stat lookups).
pub fn find_runsc_binary() -> Option<PathBuf> {
    find_binary("runsc")
}

/// The container runtime binary for the primary route: docker preferred,
/// podman fallback (same order as the T5 probe).
fn find_container_binary() -> Option<PathBuf> {
    find_binary("docker").or_else(|| find_binary("podman"))
}

/// Route selection against a probe plus an explicit runsc-binary override so
/// every branch is unit-testable without touching the real host.
///
/// - Primary when a container runtime exists AND runsc is registered with it.
/// - Fallback (direct OCI bundle) whenever the runsc binary itself resolves,
///   even when no container runtime exists or runsc is not registered with
///   the detected runtime.
/// - Fail closed otherwise: attestations never silently downgrade to the
///   process backend.
pub(crate) fn select_route_with(
    probe: &dyn ProbeSource,
    runsc_bin: Option<&Path>,
) -> Result<GvisorRoute, RunnerError> {
    if probe.container_runtime().is_some() && probe.runsc_registered() {
        return Ok(GvisorRoute::ContainerRuntime);
    }
    if runsc_bin.is_some() {
        return Ok(GvisorRoute::OciBundle);
    }
    Err(RunnerError::Spawn(
        "gVisor tier unavailable: need a container runtime (docker/podman) with a \
         registered runsc runtime, or the runsc binary itself"
            .to_string(),
    ))
}

/// Production route selection reading the real host.
pub fn select_route(probe: &dyn ProbeSource) -> Result<GvisorRoute, RunnerError> {
    select_route_with(probe, find_runsc_binary().as_deref())
}

/// Compose the executor digest per the librarian brief. Identical inputs are
/// byte-stable; changing ANY component changes the output (unit-tested).
pub fn compose_digest(
    runsc_version: &str,
    platform: &str,
    image_digest: &str,
    config_json: &str,
) -> String {
    sha256_hex(format!("{runsc_version}|{platform}|{image_digest}|{config_json}").as_bytes())
}

/// Compile-time platform token folded into the digest.
fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// First line of `runsc --version`; best-effort `unknown-runsc` on failure so
/// digest composition never panics on a broken install.
fn runsc_version(runsc_bin: &Path) -> String {
    match Command::new(runsc_bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines().next().unwrap_or("").trim().to_string()
        }
        _ => "unknown-runsc".to_string(),
    }
}

/// argv for the primary route: `docker run --rm --runtime=<name> --network
/// none -v <checkout>:<WORK_MOUNT>:ro -w <WORK_MOUNT> <image> <tool argv...>`.
/// No environment variables are forwarded: docker forwards none by default,
/// which IS the scrubbed-env contract.
pub fn primary_run_args(cfg: &GvisorConfig, argv: &[String]) -> Result<Vec<String>, RunnerError> {
    validate_image_pin(&cfg.image)?;
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        format!("--runtime={}", cfg.runtime_name),
        "--network".to_string(),
        "none".to_string(),
        "-v".to_string(),
        format!("{}:{WORK_MOUNT}:ro", cfg.checkout.display()),
        "-w".to_string(),
        WORK_MOUNT.to_string(),
        cfg.image.clone(),
    ];
    args.extend(argv.iter().cloned());
    Ok(args)
}

/// argv generating the default OCI spec into `bundle_dir/config.json`.
pub fn oci_spec_args(bundle_dir: &Path) -> Vec<String> {
    vec![
        "spec".to_string(),
        format!("--bundle={}", bundle_dir.display()),
    ]
}

/// argv running the bundle to completion; `runsc run` blocks and propagates
/// the child exit code, so `Output.status.code()` is honest.
pub fn oci_run_args(bundle_dir: &Path, id: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        format!("--bundle={}", bundle_dir.display()),
        id.to_string(),
    ]
}

/// Edit a generated OCI spec for one attestation run: absolute rootfs path,
/// headless process (terminal off) with our argv and cwd, and a read-only
/// bind mount of the checkout at [`WORK_MOUNT`] appended to the defaults
/// (which already carry nodev + ro posture). Pure string-to-string so tests
/// drive it without runsc installed.
pub(crate) fn render_bundle_config(
    base_spec: &str,
    argv: &[String],
    workdir: &str,
    checkout: &Path,
    rootfs: &Path,
) -> Result<String, RunnerError> {
    let mut spec: serde_json::Value =
        serde_json::from_str(base_spec).map_err(|e| RunnerError::Io(e.to_string()))?;
    spec["root"]["path"] = serde_json::Value::String(rootfs.display().to_string());
    spec["process"]["terminal"] = serde_json::Value::Bool(false);
    spec["process"]["args"] = serde_json::Value::Array(
        argv.iter()
            .map(|a| serde_json::Value::String(a.clone()))
            .collect(),
    );
    spec["process"]["cwd"] = serde_json::Value::String(workdir.to_string());
    let work_mount = serde_json::json!({
        "destination": workdir,
        "type": "none",
        "source": checkout.display().to_string(),
        "options": ["rbind", "ro"],
    });
    match spec["mounts"].as_array_mut() {
        Some(mounts) => mounts.push(work_mount),
        None => spec["mounts"] = serde_json::Value::Array(vec![work_mount]),
    }
    serde_json::to_string_pretty(&spec).map_err(|e| RunnerError::Io(e.to_string()))
}

/// Nanosecond-resolution uniqueness suffix for per-run names.
fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Unique container id for one bundle run; runsc deletes state on exit.
fn fresh_container_id() -> String {
    format!("pf-gvisor-{}-{}", std::process::id(), unique_suffix())
}

/// Fresh empty bundle directory under temp_dir for one run.
fn fresh_bundle_dir() -> Result<PathBuf, RunnerError> {
    let dir = std::env::temp_dir().join(format!(
        "pf-gvisor-bundle-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| RunnerError::Io(e.to_string()))?;
    Ok(dir)
}

/// Piped stdio contract shared by both routes.
fn pipe_stdio(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// The spawned sandbox leads its own process group so the shared watchdog
/// can kill the whole tree on timeout. Deliberately NOT cfg-split (mutation
/// observability lesson): on non-unix the extra statement compiles as a
/// harmless borrow-and-drop instead of hiding behind a cfg gate.
fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// The gVisor backend: runs the canonical allowlisted entry inside a runsc
/// sandbox via the primary container-runtime route or the direct OCI-bundle
/// fallback. Same Executor contract as the mock: allowlist gate first, typed
/// args second, wall-clock budget inherited, host environment untouched.
pub struct GvisorExecutor {
    pub config: GvisorConfig,
}

impl GvisorExecutor {
    pub fn new(config: GvisorConfig) -> Self {
        Self { config }
    }

    /// Route selection against the real host.
    pub fn route(&self) -> Result<GvisorRoute, RunnerError> {
        select_route(&ProdProbe)
    }

    /// Structural (argv-free) sandbox configuration rendered canonically for
    /// the digest. One executor has ONE stable digest regardless of which
    /// tool ran; argv intentionally excluded.
    fn structural_config_json(&self, route: GvisorRoute) -> Result<String, RunnerError> {
        let value = match route {
            GvisorRoute::ContainerRuntime => serde_json::json!({
                "route": GvisorRoute::ContainerRuntime.tag(),
                "runtime": self.config.runtime_name,
                "network": "none",
                "mounts": [{
                    "target": WORK_MOUNT,
                    "source": self.config.checkout.display().to_string(),
                    "readonly": true,
                }],
                "image": self.config.image,
            }),
            GvisorRoute::OciBundle => {
                let rootfs = self.required_rootfs()?;
                serde_json::json!({
                    "route": GvisorRoute::OciBundle.tag(),
                    "rootfs": rootfs.display().to_string(),
                    "cwd": WORK_MOUNT,
                    "mounts": [{
                        "destination": WORK_MOUNT,
                        "source": self.config.checkout.display().to_string(),
                        "options": ["rbind", "ro"],
                    }],
                })
            }
        };
        serde_json::to_string(&value).map_err(|e| RunnerError::Io(e.to_string()))
    }

    /// Record-only executor identity composed from live inputs:
    /// `sha256(runsc_version | platform | image_digest | structural_config)`.
    /// Thin environment-reading wrapper over [`Self::executor_digest_with`].
    pub fn executor_digest(&self, route: GvisorRoute) -> Result<String, RunnerError> {
        let runsc_bin = find_runsc_binary();
        let version = match (&route, runsc_bin.as_deref()) {
            (_, Some(bin)) => runsc_version(bin),
            // No runsc binary but a registered runtime: the version lives in
            // the runtime-managed installation; record unknown rather than
            // guessing.
            (GvisorRoute::ContainerRuntime, None) => "unknown-runsc".to_string(),
            (GvisorRoute::OciBundle, None) => {
                return Err(RunnerError::Spawn("runsc binary not found".to_string()))
            }
        };
        self.executor_digest_with(&version, route)
    }

    /// Pure core of [`Self::executor_digest`]: the caller supplies the runsc
    /// version so tests drive both routes hermetically without a runsc
    /// install. Production goes through [`Self::executor_digest`].
    fn executor_digest_with(
        &self,
        runsc_version: &str,
        route: GvisorRoute,
    ) -> Result<String, RunnerError> {
        Ok(compose_digest(
            runsc_version,
            &platform(),
            &image_digest_of(&self.config.image),
            &self.structural_config_json(route)?,
        ))
    }

    fn required_rootfs(&self) -> Result<&Path, RunnerError> {
        let rootfs = self.config.rootfs.as_deref().ok_or_else(|| {
            RunnerError::Spawn(
                "gVisor OCI bundle route requires a provisioned rootfs (config.rootfs)".to_string(),
            )
        })?;
        if !rootfs.is_dir() {
            return Err(RunnerError::Spawn(format!(
                "gVisor rootfs {} is not a directory",
                rootfs.display()
            )));
        }
        Ok(rootfs)
    }

    /// One sandboxed invocation of `argv`, capturing stdout/stderr/status.
    /// Both routes reuse the shared wall-clock watchdog and process-group
    /// kill through [`wait_with_timeout`].
    pub(crate) fn sandboxed_output(
        &self,
        route: GvisorRoute,
        argv: &[String],
    ) -> Result<Output, RunnerError> {
        match route {
            GvisorRoute::ContainerRuntime => {
                let bin = find_container_binary().unwrap_or_else(|| PathBuf::from("docker"));
                let mut cmd = Command::new(bin);
                cmd.args(primary_run_args(&self.config, argv)?);
                pipe_stdio(&mut cmd);
                own_process_group(&mut cmd);
                let child = cmd.spawn().map_err(|e| RunnerError::Spawn(e.to_string()))?;
                wait_with_timeout(child, parse_timeout())
            }
            GvisorRoute::OciBundle => {
                let runsc = find_runsc_binary()
                    .ok_or_else(|| RunnerError::Spawn("runsc binary not found".to_string()))?;
                self.required_rootfs()?;
                let bundle = fresh_bundle_dir()?;
                let result = self.run_oci_bundle(&runsc, &bundle, argv);
                let _ = std::fs::remove_dir_all(&bundle);
                result
            }
        }
    }

    /// Fallback route mechanics: generate the default spec, edit it, run it.
    fn run_oci_bundle(
        &self,
        runsc: &Path,
        bundle: &Path,
        argv: &[String],
    ) -> Result<Output, RunnerError> {
        // 1. Default spec generation (rootfs placeholder, nodev+ro defaults).
        let mut spec_cmd = Command::new(runsc);
        spec_cmd.args(oci_spec_args(bundle));
        let spec_out = spec_cmd
            .output()
            .map_err(|e| RunnerError::Spawn(e.to_string()))?;
        if !spec_out.status.success() {
            return Err(RunnerError::ToolFailed {
                exit_code: exit_code_of(spec_out.status),
                stderr: String::from_utf8_lossy(&spec_out.stderr).into_owned(),
            });
        }
        // 2. Read, edit, write back.
        let raw = std::fs::read_to_string(bundle.join("config.json"))
            .map_err(|e| RunnerError::Io(e.to_string()))?;
        let edited = render_bundle_config(
            &raw,
            argv,
            WORK_MOUNT,
            &self.config.checkout,
            self.config.rootfs.as_deref().expect("rootfs checked"),
        )?;
        std::fs::write(bundle.join("config.json"), edited)
            .map_err(|e| RunnerError::Io(e.to_string()))?;
        // 3. Blocking run propagating the child exit code.
        let id = fresh_container_id();
        let mut cmd = Command::new(runsc);
        cmd.args(oci_run_args(bundle, &id));
        pipe_stdio(&mut cmd);
        own_process_group(&mut cmd);
        let child = cmd.spawn().map_err(|e| RunnerError::Spawn(e.to_string()))?;
        wait_with_timeout(child, parse_timeout())
    }
}

/// Full guest argv for one allowlisted run: the canonical BINARY first,
/// then its fixed args, then the caller's typed args. Omitting the binary
/// makes a zero-fixed-args tool execute the image default CMD instead of
/// the allowlisted command — a vacuous attestation.
pub(crate) fn primary_argv(canonical: &Tool, args: &[String]) -> Vec<String> {
    let mut argv = vec![canonical.bin.display().to_string()];
    argv.extend(canonical.args.iter().cloned());
    argv.extend(args.iter().cloned());
    argv
}

impl Executor for GvisorExecutor {
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
        // Same gate order as every backend: allowlist, then typed args,
        // BEFORE anything probes or spawns.
        let canonical =
            lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
        validate_tool_args(&canonical.name, args)?;
        let route = self.route()?;

        // Version probe INSIDE the sandbox so the recorded version describes
        // the isolated environment, not the host toolchain.
        let bin = canonical.bin.display().to_string();
        let ver_argv = vec![bin.clone(), "--version".to_string()];
        let tool_version = match self.sandboxed_output(route, &ver_argv) {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => format!("unknown-{bin}"),
        };

        let out = self.sandboxed_output(route, &primary_argv(&canonical, args))?;
        let stdout_hash = sha256_hex(&out.stdout);

        Ok(RunOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: exit_code_of(out.status),
            stdout_hash,
            env_fingerprint: super::runner::env_fingerprint(&tool_version),
            tool_version,
            command: command_string(&canonical, args),
        })
    }

    fn label(&self) -> &'static str {
        "sandbox-gvisor"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prober::ProbeSource;
    use crate::PF_TOOL_TIMEOUT_SECS;

    /// Fixture implementing [`ProbeSource`] with plain fields so every route
    /// matrix cell is constructed explicitly.
    #[derive(Default)]
    struct FakeProbe {
        runtime: Option<String>,
        runsc: bool,
    }

    impl ProbeSource for FakeProbe {
        fn kvm_available(&self) -> bool {
            false
        }
        fn fc_binaries_present(&self) -> bool {
            false
        }
        fn container_runtime(&self) -> Option<String> {
            self.runtime.clone()
        }
        fn runsc_registered(&self) -> bool {
            self.runsc
        }
    }

    const RUNSC_BIN: &str = "/usr/local/bin/runsc";

    /// Real empty directory under temp_dir: `required_rootfs()` performs an
    /// `is_dir()` check against the actual filesystem, so synthetic paths
    /// would fail on any host.
    fn temp_rootfs(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pf-gvisor-test-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).expect("temp rootfs created");
        dir
    }

    fn pinned_image() -> String {
        format!("rust@sha256:{}", "a".repeat(64))
    }

    fn test_config() -> GvisorConfig {
        GvisorConfig::new(pinned_image(), "/repo").expect("valid config")
    }

    #[test]
    fn digest_is_64_hex_and_stable_for_identical_inputs() {
        let a = compose_digest("release-20240101", "linux-x86_64", "sha256:ab", "{}");
        let b = compose_digest("release-20240101", "linux-x86_64", "sha256:ab", "{}");
        assert_eq!(a, b, "identical inputs must hash identically");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// Acceptance requirement: digest folding verified by unit test. Each of
    /// the four components MUST move the digest when changed.
    #[test]
    fn digest_folds_every_component() {
        let base = compose_digest("v1", "linux-x86_64", "sha256:aa", "{\"k\":1}");
        assert_ne!(
            compose_digest("v2", "linux-x86_64", "sha256:aa", "{\"k\":1}"),
            base
        );
        assert_ne!(
            compose_digest("v1", "darwin-arm64", "sha256:aa", "{\"k\":1}"),
            base
        );
        assert_ne!(
            compose_digest("v1", "linux-x86_64", "sha256:bb", "{\"k\":1}"),
            base
        );
        assert_ne!(
            compose_digest("v1", "linux-x86_64", "sha256:aa", "{\"k\":2}"),
            base
        );
    }

    #[test]
    fn image_pin_accepts_digest_rejects_tag_and_short_hash() {
        assert!(validate_image_pin(&pinned_image()).is_ok());
        assert!(validate_image_pin("rust:1.85").is_err());
        assert!(validate_image_pin("rust@sha256:abcd").is_err());
        assert!(validate_image_pin("rust").is_err());
        // Non-hex digest characters rejected.
        assert!(validate_image_pin("rust@sha256:zzzz").is_err());
    }

    #[test]
    fn config_new_rejects_unpinned_image() {
        assert!(GvisorConfig::new("rust:latest", "/repo").is_err());
        assert!(GvisorConfig::new(pinned_image(), "/repo").is_ok());
    }

    #[test]
    fn primary_run_args_exact_shape() {
        let cfg = test_config();
        let args =
            primary_run_args(&cfg, &["cargo".to_string(), "test".to_string()]).expect("args built");
        let image = pinned_image();
        let expected = vec![
            "run",
            "--rm",
            "--runtime=runsc-nonet",
            "--network",
            "none",
            "-v",
            "/repo:/work:ro",
            "-w",
            "/work",
            image.as_str(),
            "cargo",
            "test",
        ];
        assert_eq!(args, expected, "primary argv shape is contractual");
    }

    #[test]
    fn primary_run_args_honor_runtime_override() {
        let mut cfg = test_config();
        cfg.runtime_name = "my-runsc".to_string();
        let args = primary_run_args(&cfg, &[]).expect("args built");
        assert!(args.contains(&"--runtime=my-runsc".to_string()));
    }

    // ---- primary_argv: the binary MUST lead the guest argv -------------------

    #[test]
    fn primary_argv_prepends_binary_for_zero_fixed_args_tool() {
        // "pytest" has no fixed args: without the prepend the container
        // would execute the image default CMD instead of pytest.
        let t = lookup("pytest").expect("allowlisted");
        let argv = primary_argv(&t, &[]);
        assert_eq!(argv, vec!["pytest".to_string()]);
    }

    #[test]
    fn primary_argv_prepends_binary_for_fixed_args_tool() {
        let t = lookup("cargo --version").expect("allowlisted");
        let argv = primary_argv(&t, &[]);
        assert_eq!(
            argv,
            vec!["cargo".to_string(), "--version".to_string()],
            "binary first, then fixed args"
        );
    }

    #[test]
    fn primary_argv_appends_user_args_after_fixed_args() {
        let t = lookup("cargo build").expect("allowlisted");
        let argv = primary_argv(&t, &["--offline".to_string()]);
        assert_eq!(
            argv,
            vec![
                "cargo".to_string(),
                "build".to_string(),
                "--offline".to_string()
            ],
            "binary, fixed args, then typed user args"
        );
    }

    #[test]
    fn oci_spec_and_run_args_shapes() {
        let bundle = Path::new("/tmp/bundle");
        assert_eq!(
            oci_spec_args(bundle),
            vec!["spec".to_string(), "--bundle=/tmp/bundle".to_string()]
        );
        assert_eq!(
            oci_run_args(bundle, "pf-1"),
            vec![
                "run".to_string(),
                "--bundle=/tmp/bundle".to_string(),
                "pf-1".to_string()
            ]
        );
    }

    const BASE_SPEC: &str = r#"{
        "ociVersion": "1.0.0",
        "root": {"path": "rootfs", "readonly": true},
        "process": {"terminal": true, "args": ["/bin/sh"], "cwd": "/"},
        "mounts": [{"destination": "/proc", "type": "proc", "source": "proc"}]
    }"#;

    #[test]
    fn render_bundle_config_edits_root_process_and_appends_work_mount() {
        let edited = render_bundle_config(
            BASE_SPEC,
            &["cargo".to_string(), "test".to_string()],
            WORK_MOUNT,
            Path::new("/repo"),
            Path::new("/opt/rootfs"),
        )
        .expect("spec rendered");
        let v: serde_json::Value = serde_json::from_str(&edited).expect("valid json");
        assert_eq!(
            v["root"]["path"], "/opt/rootfs",
            "absolute rootfs substituted"
        );
        assert_eq!(v["process"]["terminal"], false, "headless attestation run");
        assert_eq!(
            v["process"]["args"],
            serde_json::json!(["cargo", "test"]),
            "argv replaced verbatim"
        );
        assert_eq!(v["process"]["cwd"], WORK_MOUNT);
        let mounts = v["mounts"].as_array().expect("mounts array kept");
        assert_eq!(
            mounts.len(),
            2,
            "default mount preserved + work mount appended"
        );
        let last = mounts.last().expect("appended mount present");
        assert_eq!(last["destination"], WORK_MOUNT);
        assert_eq!(last["source"], "/repo");
        let options: Vec<&str> = last["options"]
            .as_array()
            .expect("options array")
            .iter()
            .filter_map(|o| o.as_str())
            .collect();
        assert!(options.contains(&"rbind"), "bind options: {options:?}");
        assert!(options.contains(&"ro"), "read-only enforced: {options:?}");
        assert_eq!(v["ociVersion"], "1.0.0", "untouched fields preserved");
    }

    #[test]
    fn render_bundle_config_creates_missing_mounts_array() {
        let bare = r#"{"root":{"path":"rootfs"},"process":{"args":["sh"],"cwd":"/"}}"#;
        let edited = render_bundle_config(
            bare,
            &[],
            WORK_MOUNT,
            Path::new("/repo"),
            Path::new("/opt/rootfs"),
        )
        .expect("rendered");
        let v: serde_json::Value = serde_json::from_str(&edited).expect("valid json");
        let mounts = v["mounts"].as_array().expect("mounts array created");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["destination"], WORK_MOUNT);
    }

    #[test]
    fn render_bundle_config_rejects_invalid_base_spec() {
        let err = render_bundle_config(
            "not-json",
            &[],
            WORK_MOUNT,
            Path::new("/repo"),
            Path::new("/opt/rootfs"),
        );
        assert!(matches!(err, Err(RunnerError::Io(_))));
    }

    #[test]
    fn route_primary_when_runtime_and_runsc_registered() {
        let probe = FakeProbe {
            runtime: Some("docker".to_string()),
            runsc: true,
        };
        assert_eq!(
            select_route_with(&probe, Some(Path::new(RUNSC_BIN))),
            Ok(GvisorRoute::ContainerRuntime)
        );
    }

    #[test]
    fn route_fallback_when_no_runtime_but_runsc_binary() {
        let probe = FakeProbe::default();
        assert_eq!(
            select_route_with(&probe, Some(Path::new(RUNSC_BIN))),
            Ok(GvisorRoute::OciBundle)
        );
    }

    #[test]
    fn route_fallback_when_runtime_present_but_runsc_not_registered() {
        // Docker exists, runsc NOT registered with it, but the runsc binary
        // itself resolves: the direct OCI bundle route stays viable.
        let probe = FakeProbe {
            runtime: Some("docker".to_string()),
            runsc: false,
        };
        assert_eq!(
            select_route_with(&probe, Some(Path::new(RUNSC_BIN))),
            Ok(GvisorRoute::OciBundle)
        );
    }

    #[test]
    fn route_fails_closed_without_any_prerequisite() {
        let probe = FakeProbe::default();
        let err = select_route_with(&probe, None).unwrap_err();
        assert!(matches!(err, RunnerError::Spawn(_)));
        let msg = match err {
            RunnerError::Spawn(m) => m,
            other => panic!("expected Spawn, got {other:?}"),
        };
        assert!(msg.contains("gVisor tier unavailable"), "actionable: {msg}");
        assert!(msg.contains("runsc"), "names prerequisite: {msg}");
    }

    #[test]
    fn route_tags_are_stable_labels() {
        assert_eq!(GvisorRoute::ContainerRuntime.tag(), "docker-runtime");
        assert_eq!(GvisorRoute::OciBundle.tag(), "oci-bundle");
    }

    #[test]
    fn structural_config_differs_per_route_and_is_argv_free() {
        let exec = GvisorExecutor::new(test_config());
        let primary = exec
            .structural_config_json(GvisorRoute::ContainerRuntime)
            .expect("primary json");
        assert!(primary.contains("\"docker-runtime\""));
        assert!(primary.contains("\"network\":\"none\""));
        assert!(primary.contains("/repo"), "checkout folded");

        let rootfs = temp_rootfs("structural");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        let fallback = exec
            .structural_config_json(GvisorRoute::OciBundle)
            .expect("fallback json");
        assert!(fallback.contains("\"oci-bundle\""));
        assert!(
            fallback.contains(&rootfs.display().to_string()),
            "real rootfs path folded: {fallback}"
        );
        assert_ne!(primary, fallback, "routes must produce distinct identities");
        let _ = std::fs::remove_dir_all(&rootfs);
    }

    #[test]
    fn structural_config_requires_rootfs_on_bundle_route() {
        let exec = GvisorExecutor::new(test_config());
        let err = exec
            .structural_config_json(GvisorRoute::OciBundle)
            .unwrap_err();
        assert!(
            matches!(err, RunnerError::Spawn(ref m) if m.contains("rootfs")),
            "rootfs required: {err:?}"
        );
    }

    #[test]
    fn executor_digest_stable_and_route_sensitive() {
        // The live executor_digest wrapper resolves runsc via PATH.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let rootfs = temp_rootfs("digest");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        // Hermetic core: identical inputs are byte-stable on any host.
        let d1 = exec
            .executor_digest_with("runsc-v1", GvisorRoute::OciBundle)
            .expect("digest");
        let d2 = exec
            .executor_digest_with("runsc-v1", GvisorRoute::OciBundle)
            .expect("digest");
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
        // Route changes the structural config, hence the digest.
        let d3 = exec
            .executor_digest_with("runsc-v1", GvisorRoute::ContainerRuntime)
            .expect("digest");
        assert_ne!(d1, d3);
        // Version folding through the core.
        let d4 = exec
            .executor_digest_with("runsc-v2", GvisorRoute::OciBundle)
            .expect("digest");
        assert_ne!(d1, d4);

        // Live wrapper: ContainerRuntime succeeds even with no runsc binary
        // (unknown-runsc fallback); OciBundle outcome tracks binary presence,
        // so both branches are valid depending on the host.
        assert!(exec.executor_digest(GvisorRoute::ContainerRuntime).is_ok());
        match exec.executor_digest(GvisorRoute::OciBundle) {
            Ok(d) => assert_eq!(d.len(), 64),
            Err(RunnerError::Spawn(m)) => {
                assert!(m.contains("runsc binary not found"), "{m}")
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
        let _ = std::fs::remove_dir_all(&rootfs);
    }

    /// Contract suite without any sandbox: the allowlist gate fires BEFORE
    /// route probing, so these pass on hosts without runsc too.
    #[test]
    fn executor_rejects_unallowlisted_tool_before_probing() {
        let exec = GvisorExecutor::new(test_config());
        let evil = Tool {
            name: "evil".into(),
            bin: PathBuf::from("evil"),
            args: vec![],
        };
        let err = exec.run(&evil, &[]).unwrap_err();
        assert!(matches!(err, RunnerError::NotAllowed(n) if n == "evil"));
    }

    #[test]
    fn executor_rejects_metachar_args_before_probing() {
        let exec = GvisorExecutor::new(test_config());
        let t = lookup("cargo --version").expect("allowlisted");
        let err = exec.run(&t, &["bad;arg".to_string()]).unwrap_err();
        assert!(matches!(err, RunnerError::InvalidArg { .. }));
    }

    #[test]
    fn label_identifies_backend() {
        let exec = GvisorExecutor::new(test_config());
        assert_eq!(exec.label(), "sandbox-gvisor");
    }

    // ---- e2e skip-clean section ------------------------------------------
    //
    // Every e2e test below prints a SKIP line and returns cleanly when its
    // prerequisite is absent, so `cargo test --features sandbox-gvisor` is
    // green on hosts without gVisor while still recording WHY it skipped.

    fn skip(reason: &str) {
        eprintln!("SKIP(sandbox-gvisor): {reason}");
    }

    fn require_gvisor_route(route: GvisorRoute) -> bool {
        match select_route(&ProdProbe) {
            Ok(selected) if selected == route => true,
            Ok(other) => {
                skip(&format!(
                    "host selects {other:?}, not the requested {route:?}"
                ));
                false
            }
            Err(e) => {
                skip(&format!("gVisor prerequisites absent: {e:?}"));
                false
            }
        }
    }

    fn digest_pinned_env_image() -> Option<String> {
        std::env::var("POLYFORGE_SANDBOX_IMAGE")
            .ok()
            .filter(|image| validate_image_pin(image).is_ok())
    }

    /// Sentinel contract through the PRIMARY route (same asserts as the T2
    /// mock suite, through a real runsc container): cwd pinned to /work,
    /// sentinel env var scrubbed, outbound network dead.
    #[test]
    fn e2e_primary_sentinel_cwd_env_network() {
        // require_gvisor_route probes PATH (ProdProbe) and may spawn docker.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        if !require_gvisor_route(GvisorRoute::ContainerRuntime) {
            return;
        }
        let Some(image) = digest_pinned_env_image() else {
            skip("POLYFORGE_SANDBOX_IMAGE (digest-pinned) not set; set it to run the primary-route e2e");
            return;
        };
        let cfg = GvisorConfig::new(
            image,
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
        )
        .expect("config valid");
        let exec = GvisorExecutor::new(cfg);
        let route = GvisorRoute::ContainerRuntime;

        // cwd sentinel: pwd inside the sandbox must be /work.
        let out = exec
            .sandboxed_output(route, &["/bin/sh".into(), "-c".into(), "pwd".into()])
            .expect("sandboxed pwd ran");
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            WORK_MOUNT,
            "cwd sentinel"
        );

        // env sentinel: no host variable may leak into the sandbox.
        let out = exec
            .sandboxed_output(
                route,
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    "printenv CARGO_MANIFEST_DIR".into(),
                ],
            )
            .expect("sandboxed printenv ran");
        assert_eq!(out.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "sentinel env var must be scrubbed"
        );

        // network sentinel: ANY successful outbound connect fails the check;
        // both "blocked" and "no such device" outcomes are acceptable proof
        // of --network none.
        let out = exec
            .sandboxed_output(
                route,
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo > /dev/tcp/1.1.1.1/443".into(),
                ],
            )
            .expect("network probe ran");
        assert_ne!(
            out.status.code(),
            Some(0),
            "outbound TCP must fail under --network none"
        );

        // Executor identity recorded for the transcript.
        if let Ok(digest) = exec.executor_digest(route) {
            eprintln!("gvisor e2e executor_digest={digest}");
        }
    }

    /// Sentinel contract through the FALLBACK direct-OCI route. Requires a
    /// provisioned rootfs containing /bin/sh; opt in via
    /// PF_GVISOR_TEST_ROOTFS=<abs path>.
    #[test]
    fn e2e_fallback_bundle_sentinel_exit_code() {
        // find_runsc_binary resolves runsc via PATH.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let Ok(rootfs) = std::env::var("PF_GVISOR_TEST_ROOTFS") else {
            skip("PF_GVISOR_TEST_ROOTFS not set; point it at a provisioned rootfs dir to run the OCI-bundle e2e");
            return;
        };
        if find_runsc_binary().is_none() {
            skip("runsc binary not found on PATH or well-known dirs");
            return;
        }
        let mut cfg = GvisorConfig::new(
            digest_pinned_env_image().unwrap_or_else(pinned_image),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
        )
        .expect("config valid");
        cfg.rootfs = Some(PathBuf::from(rootfs));
        let exec = GvisorExecutor::new(cfg);

        // Exit-code propagation through `runsc run` (blocks and propagates).
        let ok = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["/bin/true".to_string()])
            .expect("bundle true ran");
        assert_eq!(ok.status.code(), Some(0));

        let fail = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["/bin/false".to_string()])
            .expect("bundle false ran");
        assert_eq!(fail.status.code(), Some(1), "exit code propagated verbatim");

        // cwd sentinel through the bundle route too.
        let pwd = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["/bin/pwd".to_string()])
            .expect("bundle pwd ran");
        assert_eq!(String::from_utf8_lossy(&pwd.stdout).trim(), WORK_MOUNT);

        if let Ok(digest) = exec.executor_digest(GvisorRoute::OciBundle) {
            eprintln!("gvisor fallback e2e executor_digest={digest}");
        }
    }

    /// Full Executor::run happy path when gVisor IS available: allowlisted
    /// tool runs end to end and produces the complete RunOutput shape with
    /// an in-sandbox tool version.
    #[test]
    fn e2e_executor_run_happy_path_when_gvisor_present() {
        // require_gvisor_route probes PATH (ProdProbe) and may spawn docker.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        if !require_gvisor_route(GvisorRoute::ContainerRuntime) {
            return;
        }
        let Some(image) = digest_pinned_env_image() else {
            skip("POLYFORGE_SANDBOX_IMAGE (digest-pinned) not set");
            return;
        };
        let cfg =
            GvisorConfig::new(image, std::env::current_dir().unwrap_or_default()).expect("valid");
        let exec = GvisorExecutor::new(cfg);
        let t = lookup("cargo --version").expect("allowlisted");
        match exec.run(&t, &[]) {
            Ok(out) => {
                assert_eq!(out.exit_code, 0);
                assert!(String::from_utf8_lossy(&out.stdout).contains("cargo"));
                assert_eq!(out.stdout_hash.len(), 64);
                assert!(!out.tool_version.is_empty());
                assert!(!out.env_fingerprint.is_empty());
                assert!(out.command.starts_with("cargo"));
            }
            Err(RunnerError::Spawn(e)) => {
                // Image pull failures and unregistered runtimes surface here;
                // they are environment gaps, not code defects.
                skip(&format!("spawn failed (environment gap): {e}"));
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    // ---- offline run-path coverage (fake runsc / fake docker) ---------------
    //
    // The e2e section above skip-cleans without a real runsc install; these
    // tests drive the SAME production paths (sandboxed_output, run_oci_bundle,
    // Executor::run) through hermetic shell-script binaries placed on PATH.
    // The fake must match the REAL parsing contract: run_oci_bundle runs
    // `runsc spec --bundle=<dir>` (must WRITE config.json and exit 0), then
    // `runsc run --bundle=<dir> <id>` with piped stdio whose stdout and exit
    // code become the RunOutput. PATH mutation + bare-name spawns hold
    // TOOL_SPAWN_LOCK like every other env test in this crate.

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    /// Write a fake `runsc` (and optionally a fake `docker`) into `dir`.
    ///
    /// The script has three branches keyed on $1:
    /// - `spec`: writes a minimal single-line OCI config.json into the
    ///   --bundle dir (printf, no heredoc) and exits 0;
    /// - `run`: cats the rendered config.json (proving the edit round-trip)
    ///   and exits with the baked-in RUN_EXIT;
    /// - anything else (e.g. `--version`): prints the baked-in version line.
    ///
    /// Every invocation appends "$@" to the REC file for argv assertions.
    /// Built with .replace() placeholders — never format! — so the JSON
    /// braces need no escaping.
    fn write_fake_runsc(dir: &Path, rec: &Path, version_line: &str, run_exit: &str) {
        let script = r#"#!/bin/sh
echo "$@" >> "__REC__"
case "$1" in
  spec)
    bundle_dir=$(echo "$2" | sed 's/^--bundle=//')
    printf '%s' '{"root":{"path":"rootfs"},"process":{"args":["sh"],"cwd":"/"}}' > "$bundle_dir/config.json"
    ;;
  run)
    bundle_dir=$(echo "$2" | sed 's/^--bundle=//')
    cat "$bundle_dir/config.json"
    exit "__RUN_EXIT__"
    ;;
  *)
    echo "__VERSION__"
    ;;
esac
"#
        .replace("__REC__", &rec.display().to_string())
        .replace("__RUN_EXIT__", run_exit)
        .replace("__VERSION__", version_line);
        let bin = dir.join("runsc");
        std::fs::write(&bin, script).expect("fake runsc written");
        make_executable(&bin);
    }

    fn write_fake_docker(dir: &Path, rec: &Path) {
        let script = r#"#!/bin/sh
echo "$@" >> "__REC__"
echo "fake-docker-run-ok"
"#
        .replace("__REC__", &rec.display().to_string());
        let bin = dir.join("docker");
        std::fs::write(&bin, script).expect("fake docker written");
        make_executable(&bin);
    }

    /// Temp dir for the fake binaries + the argv recording file.
    fn fake_bin_dir(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pf-gvisor-fakebin-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).expect("fake bin dir created");
        let rec = dir.join("argv.log");
        (dir, rec)
    }

    /// Prepend `dir` to PATH, returning the saved value for restoration.
    fn push_path(dir: &Path) -> Option<String> {
        let saved = std::env::var("PATH").ok();
        let new_path = match &saved {
            Some(v) => format!("{}:{v}", dir.display()),
            None => dir.display().to_string(),
        };
        std::env::set_var("PATH", new_path);
        saved
    }

    fn restore_path(saved: Option<String>) {
        match saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }

    /// OCI-bundle route through a fake runsc: the run branch cats the
    /// RENDERED config.json, so the stdout proves the full spec-edit
    /// round-trip (argv strings, absolute rootfs, /work cwd) while the exit
    /// code propagates verbatim.
    #[test]
    fn bundle_route_round_trips_rendered_config_and_exit_code() {
        // Mutates PATH and spawns the fake runsc found through it.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("roundtrip");
        write_fake_runsc(&bin_dir, &rec, "runsc fake 20980101.0", "0");
        let rootfs = temp_rootfs("roundtrip");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        let saved_path = push_path(&bin_dir);

        let argv = vec!["cargo".to_string(), "--version".to_string()];
        let out = exec
            .sandboxed_output(GvisorRoute::OciBundle, &argv)
            .expect("bundle run through fake runsc");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(out.status.code(), Some(0), "fake runsc exits 0");
        assert!(stdout.contains(r#""cargo""#), "argv folded: {stdout}");
        assert!(stdout.contains(r#""--version""#), "argv folded: {stdout}");
        assert!(
            stdout.contains(&rootfs.display().to_string()),
            "absolute rootfs substituted: {stdout}"
        );
        assert!(stdout.contains(r#""cwd": "/work""#), "cwd pinned: {stdout}");
        assert!(stdout.contains(r#""readonly":true"#) || stdout.contains(r#""ro""#));

        // The spec and run invocations both recorded their argv.
        let recorded = std::fs::read_to_string(&rec).expect("argv log");
        assert!(
            recorded.contains("--bundle="),
            "spec/run argv recorded: {recorded}"
        );

        // Non-zero exit propagates verbatim through the same path.
        let (bin_dir2, rec2) = fake_bin_dir("exit7");
        write_fake_runsc(&bin_dir2, &rec2, "runsc fake 20980101.0", "7");
        let mut cfg2 = test_config();
        cfg2.rootfs = Some(rootfs.clone());
        let exec2 = GvisorExecutor::new(cfg2);
        let saved_path2 = push_path(&bin_dir2);
        let out2 = exec2
            .sandboxed_output(GvisorRoute::OciBundle, &["true".to_string()])
            .expect("run completes; exit code is data");
        assert_eq!(out2.status.code(), Some(7), "exit code propagates verbatim");

        restore_path(saved_path2);
        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir2);
        let _ = std::fs::remove_dir_all(&bin_dir);
        let _ = std::fs::remove_dir_all(&rootfs);
    }

    /// The wall-clock watchdog fires on a sleeping fake runsc: with
    /// PF_TOOL_TIMEOUT_SECS=1 the `run` branch sleeps past the budget and
    /// the run surfaces TimedOut naming the budget.
    #[test]
    fn bundle_route_timeout_kills_sleeping_runsc() {
        // Mutates PATH and PF_TOOL_TIMEOUT_SECS; spawns the fake runsc.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("timeout");
        // run branch sleeps 30 (far past the 1s budget), then would exit 0.
        let script = r#"#!/bin/sh
echo "$@" >> "__REC__"
case "$1" in
  spec)
    bundle_dir=$(echo "$2" | sed 's/^--bundle=//')
    printf '%s' '{"root":{"path":"rootfs"},"process":{"args":["sh"],"cwd":"/"}}' > "$bundle_dir/config.json"
    ;;
  run)
    sleep 30
    ;;
  *)
    echo "runsc fake 20980101.0"
    ;;
esac
"#
        .replace("__REC__", &rec.display().to_string());
        let bin = bin_dir.join("runsc");
        std::fs::write(&bin, script).expect("sleeping fake runsc written");
        make_executable(&bin);

        let rootfs = temp_rootfs("timeout");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        let saved_path = push_path(&bin_dir);
        let saved_timeout = std::env::var(PF_TOOL_TIMEOUT_SECS).ok();
        std::env::set_var(PF_TOOL_TIMEOUT_SECS, "1");

        let err = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["sleep".to_string()])
            .expect_err("sleeping run must hit the wall-clock budget");
        assert!(
            matches!(err, RunnerError::TimedOut { timeout_secs: 1 }),
            "budget must surface as TimedOut(1), got: {err:?}"
        );

        match saved_timeout {
            Some(v) => std::env::set_var(PF_TOOL_TIMEOUT_SECS, v),
            None => std::env::remove_var(PF_TOOL_TIMEOUT_SECS),
        }
        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);
        let _ = std::fs::remove_dir_all(&rootfs);
    }

    /// A fake runsc whose `spec` branch fails surfaces ToolFailed naming
    /// the spec step's exit code and stderr.
    #[test]
    fn bundle_route_spec_failure_is_tool_failed() {
        // Mutates PATH and spawns the fake runsc found through it.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("specfail");
        let script = r#"#!/bin/sh
echo "$@" >> "__REC__"
case "$1" in
  spec)
    echo "spec exploded" >&2
    exit 9
    ;;
  *)
    echo "runsc fake 20980101.0"
    ;;
esac
"#
        .replace("__REC__", &rec.display().to_string());
        let bin = bin_dir.join("runsc");
        std::fs::write(&bin, script).expect("spec-failing fake runsc written");
        make_executable(&bin);

        let rootfs = temp_rootfs("specfail");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        let saved_path = push_path(&bin_dir);

        let err = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["true".to_string()])
            .expect_err("spec failure must surface");
        match err {
            RunnerError::ToolFailed { exit_code, stderr } => {
                assert_eq!(exit_code, 9, "spec exit code propagates: {stderr}");
                assert!(stderr.contains("spec exploded"), "stderr folded: {stderr}");
            }
            other => panic!("expected ToolFailed, got {other:?}"),
        }

        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);
        let _ = std::fs::remove_dir_all(&rootfs);
    }

    /// Without any runsc reachable (PATH narrowed to an empty dir and no
    /// well-known install), the bundle route fails closed naming runsc.
    /// On a host with a real /usr/local/bin or /usr/bin runsc the route
    /// legitimately succeeds, so that arm asserts the happy shape instead
    /// (mirrors the fc_exec well-known-install conditional).
    #[test]
    fn bundle_route_without_runsc_fails_closed() {
        // Mutates PATH; the route probes it for the runsc binary.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (empty_dir, _rec) = fake_bin_dir("norunsc");
        let rootfs = temp_rootfs("norunsc");
        let mut cfg = test_config();
        cfg.rootfs = Some(rootfs.clone());
        let exec = GvisorExecutor::new(cfg);
        let saved_path = push_path(&empty_dir);

        let result = exec.sandboxed_output(GvisorRoute::OciBundle, &["true".to_string()]);

        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&empty_dir);
        let _ = std::fs::remove_dir_all(&rootfs);

        let well_known_runsc =
            Path::new("/usr/local/bin/runsc").exists() || Path::new("/usr/bin/runsc").exists();
        if !well_known_runsc {
            let err = result.expect_err("no runsc anywhere must fail closed");
            assert!(
                matches!(&err, RunnerError::Spawn(m) if m.contains("runsc binary not found")),
                "error names the missing prerequisite: {err:?}"
            );
        } else {
            // A real install answers from the well-known dir: the run either
            // succeeds or reports an environment gap, never a silent fallback.
            match result {
                Ok(out) => assert!(out.status.code().is_some()),
                Err(RunnerError::Spawn(_)) | Err(RunnerError::ToolFailed { .. }) => {}
                Err(e) => panic!("unexpected error shape: {e:?}"),
            }
        }
    }

    /// The bundle route requires a provisioned rootfs BEFORE anything spawns:
    /// a config without one fails closed naming the prerequisite. A fake
    /// runsc on PATH satisfies the binary probe so the rootfs check is the
    /// one that fires (on this host no real runsc exists, but the fake makes
    /// the test hermetic regardless).
    #[test]
    fn bundle_route_without_rootfs_names_prerequisite() {
        // Mutates PATH; the route probes it for runsc before the rootfs check.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("norootfs");
        write_fake_runsc(&bin_dir, &rec, "runsc fake 20980101.0", "0");
        let exec = GvisorExecutor::new(test_config()); // rootfs: None
        let saved_path = push_path(&bin_dir);
        let err = exec
            .sandboxed_output(GvisorRoute::OciBundle, &["true".to_string()])
            .expect_err("missing rootfs must fail closed");
        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains("requires a provisioned rootfs")),
            "error names the rootfs prerequisite: {err:?}"
        );
    }

    /// Primary route through a fake docker: the full `docker run --rm
    /// --runtime=... --network none ... <image> <argv>` command executes the
    /// fake, whose stdout becomes the run output.
    #[test]
    fn primary_route_round_trips_through_fake_docker() {
        // Mutates PATH and spawns the fake docker found through it.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("primary");
        write_fake_docker(&bin_dir, &rec);
        let exec = GvisorExecutor::new(test_config());
        let saved_path = push_path(&bin_dir);

        let argv = vec!["cargo".to_string(), "--version".to_string()];
        let out = exec
            .sandboxed_output(GvisorRoute::ContainerRuntime, &argv)
            .expect("primary run through fake docker");
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "fake-docker-run-ok"
        );

        // The recorded argv proves the contractual docker command shape.
        let recorded = std::fs::read_to_string(&rec).expect("argv log");
        for expected in [
            "run",
            "--rm",
            "--runtime=runsc-nonet",
            "--network",
            "none",
            "-v",
            "/repo:/work:ro",
            "-w",
            "/work",
        ] {
            assert!(
                recorded.contains(expected),
                "argv must contain {expected}: {recorded}"
            );
        }
        assert!(
            recorded.contains(&pinned_image()),
            "digest-pinned image leads the payload: {recorded}"
        );

        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);
    }

    /// Full Executor::run happy path offline: fake docker + fake runsc on
    /// PATH make route selection pick the primary route, the in-sandbox
    /// version probe answers through the fake runsc, and the RunOutput
    /// carries the complete attestation shape.
    #[test]
    fn executor_run_happy_path_through_fake_runtimes() {
        // Mutates PATH; route selection probes it and spawns both fakes.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("fullrun");
        write_fake_runsc(&bin_dir, &rec, "runsc fake 20980101.0", "0");
        write_fake_docker(&bin_dir, &rec);
        let exec = GvisorExecutor::new(test_config());
        let saved_path = push_path(&bin_dir);

        let t = lookup("cargo --version").expect("allowlisted");
        let out = exec.run(&t, &[]).expect("full run through fakes");

        assert_eq!(out.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "fake-docker-run-ok"
        );
        assert_eq!(out.stdout_hash.len(), 64);
        // The version probe runs INSIDE the sandbox through the selected
        // route (ContainerRuntime here), so the fake docker's stdout IS the
        // recorded tool_version — runsc --version is only read by
        // executor_digest, never by run().
        assert_eq!(out.tool_version, "fake-docker-run-ok");
        assert!(!out.env_fingerprint.is_empty());
        assert!(out.command.starts_with("cargo"));

        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);
    }

    /// executor_digest reads the runsc version from the binary on PATH: a
    /// fake runsc answering `--version` yields a digest that differs from
    /// the unknown-runsc fallback for the same route.
    #[test]
    fn executor_digest_reads_fake_runsc_version() {
        // Mutates PATH; executor_digest resolves runsc through it.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let (bin_dir, rec) = fake_bin_dir("digest");
        write_fake_runsc(&bin_dir, &rec, "runsc fake 20980101.0", "0");
        let exec = GvisorExecutor::new(test_config());
        let saved_path = push_path(&bin_dir);

        let with_fake = exec
            .executor_digest(GvisorRoute::ContainerRuntime)
            .expect("digest through fake runsc");

        restore_path(saved_path);
        let _ = std::fs::remove_dir_all(&bin_dir);

        assert_eq!(with_fake.len(), 64);
        assert!(
            with_fake.bytes().all(|b| b.is_ascii_hexdigit()),
            "digest is sha256 hex: {with_fake}"
        );
        let unknown = exec
            .executor_digest_with("unknown-runsc", GvisorRoute::ContainerRuntime)
            .expect("hermetic digest");
        assert_ne!(
            with_fake, unknown,
            "a real version answer must move the digest"
        );
    }

    /// required_rootfs rejects a rootfs path that exists but is a FILE,
    /// not a directory (structural_config_json surfaces the same error on
    /// the bundle route). Pure filesystem check — no PATH, no spawn.
    #[test]
    fn structural_config_rejects_rootfs_that_is_not_a_directory() {
        let file_rootfs = std::env::temp_dir().join(format!(
            "pf-gvisor-notdir-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&file_rootfs, b"i am a file").expect("rootfs file written");
        let mut cfg = test_config();
        cfg.rootfs = Some(file_rootfs.clone());
        let exec = GvisorExecutor::new(cfg);

        let err = exec
            .structural_config_json(GvisorRoute::OciBundle)
            .expect_err("file rootfs must be rejected");
        assert!(
            matches!(&err, RunnerError::Spawn(m) if m.contains("is not a directory")),
            "error names the not-a-directory rootfs: {err:?}"
        );
        let _ = std::fs::remove_file(&file_rootfs);
    }
}
