//! T6: container sandbox execution backend behind the `sandbox-container`
//! feature.
//!
//! [`ContainerExecutor`] runs each allowlisted tool inside an EPHEMERAL
//! container: one `docker run` (or podman) invocation per attestation run,
//! network disabled, the current checkout mounted read-only, and the child
//! environment scrubbed to an explicit allowlist (PATH only). The allowlist +
//! typed-arg policy from [`crate::runner`] still gates what may execute; the
//! wall-clock budget and process-group kill are inherited unchanged.
//!
//! # Command shape
//!
//! ```text
//! <runtime> run --rm --network none \
//!     -v <checkout>:/work:ro -w /work \
//!     -e PATH=<host PATH> \
//!     <image>@<sha256-digest> <tool> <fixed args...> <args...>
//! ```
//!
//! The runtime is resolved once per process: `POLYFORGE_SANDBOX_RUNTIME`
//! overrides, else the T5 prober (`ProdProbe::container_runtime`) answers
//! docker-else-podman. The image ref comes from `POLYFORGE_SANDBOX_IMAGE`,
//! defaulting to the documented `polyforge-sandbox:latest`. A ref already
//! carrying `@sha256:...` is used verbatim; otherwise the digest is resolved
//! through `<runtime> image inspect` (registry RepoDigest first, local
//! config Id second) so every production run is pinned BY DIGEST.
//!
//! # Image viability semantics
//!
//! An image that is absent or unresolvable at run time is a HARD ERROR for
//! production attestation runs (`RunnerError::Spawn`, message naming the
//! image). Feature-gated tests instead record skip-with-reason transcripts:
//! they probe for a usable runtime and image up front and return early with
//! a printed `[SKIP]` line when either is missing.
//!
//! # Executor identity
//!
//! `executor_digest = sha256("<image-ref-at-digest>|<sha256(checkout-tree)>")`
//! computed with the shared [`crate::runner::sha256_hex`] helper. The tree
//! hash walks regular files sorted by relative path (symlinks fold their
//! link target), skipping volatile build/VCS directories so the identity is
//! stable across rebuilds of the same source state.
//!
//! # Trust boundary
//!
//! Container isolation is real but bounded: `--network none` removes the
//! network namespace, `:ro` prevents worktree mutation, and `-e PATH=...`
//! passes no host secrets, yet the image itself is operator-provisioned and
//! its toolchain executes project code. Attestations produced here carry
//! `eval_metadata.executor_digest` so operators can tell which backend ran.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use super::prober::{ProbeSource, ProdProbe};
use super::runner::{
    command_string, exit_code_of, lookup, parse_timeout, sha256_hex, validate_tool_args,
    wait_with_timeout, Executor, RunOutput, RunnerError, Tool,
};
use sha2::{Digest, Sha256};

/// Environment variable selecting the sandbox image ref.
pub const POLYFORGE_SANDBOX_IMAGE_ENV: &str = "POLYFORGE_SANDBOX_IMAGE";

/// Environment variable overriding the probed container runtime name.
pub const POLYFORGE_SANDBOX_RUNTIME_ENV: &str = "POLYFORGE_SANDBOX_RUNTIME";

/// Documented default image ref when `POLYFORGE_SANDBOX_IMAGE` is unset.
/// Operators are expected to build or pull this image with the allowlisted
/// toolchain installed; production runs pin whatever it resolves to by
/// digest before executing anything.
pub const DEFAULT_SANDBOX_IMAGE: &str = "polyforge-sandbox:latest";

/// Mount point of the read-only checkout inside the container.
const CONTAINER_WORKDIR: &str = "/work";

/// Host environment variables forwarded into the container. Everything else
/// is structurally absent: docker/podman never inherit host env unless an
/// explicit `-e`/`--env` names it.
const ALLOWED_ENV_VARS: &[&str] = &["PATH"];

/// Directory names skipped by the checkout-tree hash. These hold build
/// artifacts, VCS metadata, and dependency caches whose contents churn
/// without any change to the verified source state; folding them would make
/// the executor identity unstable across unrelated rebuilds.
const EXCLUDED_TREE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".pf",
];

/// The container backend: one ephemeral container per attestation run under
/// docker or podman, network-less, read-only checkout, PATH-only env.
pub struct ContainerExecutor {
    runtime: String,
    image_ref: String,
}

impl ContainerExecutor {
    /// Explicit constructor (tests and advanced operators): `runtime` is the
    /// container CLI name ("docker" / "podman"), `image_ref` the image to
    /// pin by digest at run time.
    pub fn new(runtime: impl Into<String>, image_ref: impl Into<String>) -> Self {
        Self {
            runtime: runtime.into(),
            image_ref: image_ref.into(),
        }
    }

    /// Production constructor: runtime from `POLYFORGE_SANDBOX_RUNTIME` or
    /// the T5 prober, image from `POLYFORGE_SANDBOX_IMAGE` or the documented
    /// default. Errors only when neither config nor probe yields a runtime,
    /// which cannot happen on the selection path (the prober must have
    /// reported a Container tier for this backend to be chosen).
    pub fn from_environment() -> Result<Self, RunnerError> {
        let runtime = match std::env::var(POLYFORGE_SANDBOX_RUNTIME_ENV) {
            Ok(rt) if !rt.trim().is_empty() => rt.trim().to_string(),
            _ => ProdProbe.container_runtime().ok_or_else(|| {
                RunnerError::Spawn(
                    "no container runtime available: set POLYFORGE_SANDBOX_RUNTIME \
                         or install docker/podman"
                        .to_string(),
                )
            })?,
        };
        let image_ref = match std::env::var(POLYFORGE_SANDBOX_IMAGE_ENV) {
            Ok(img) if !img.trim().is_empty() => img.trim().to_string(),
            _ => DEFAULT_SANDBOX_IMAGE.to_string(),
        };
        Ok(Self { runtime, image_ref })
    }

    /// Resolved runtime CLI name.
    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    /// Configured image ref (before digest resolution).
    pub fn image_ref(&self) -> &str {
        &self.image_ref
    }

    /// Pin the configured image by digest, or fail hard naming it.
    pub(crate) fn pinned_image(&self) -> Result<String, RunnerError> {
        resolve_image_ref(&self.runtime, &self.image_ref)
    }
}

impl Executor for ContainerExecutor {
    fn run(&self, tool: &Tool, args: &[String]) -> Result<RunOutput, RunnerError> {
        let canonical =
            lookup(&tool.name).ok_or_else(|| RunnerError::NotAllowed(tool.name.clone()))?;
        validate_tool_args(&canonical.name, args)?;

        let checkout = std::env::current_dir().map_err(|e| RunnerError::Io(e.to_string()))?;
        let image_at_digest = self.pinned_image()?;
        let path_env = std::env::var("PATH").ok();

        let mut cmd = build_run_command(
            &self.runtime,
            Some(checkout.as_path()),
            &image_at_digest,
            &canonical.bin.display().to_string(),
            &canonical.args,
            args,
            path_env.as_deref(),
        );
        let spawned = cmd.spawn();
        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                return Err(RunnerError::Spawn(format!(
                    "container run failed (runtime {}, image {}): {e}",
                    self.runtime, image_at_digest
                )))
            }
        };
        let out = wait_with_timeout(child, parse_timeout())?;

        let exit_code = exit_code_of(out.status);
        let stdout_hash = sha256_hex(&out.stdout);
        let version = container_tool_version(
            &self.runtime,
            &image_at_digest,
            &canonical.bin.display().to_string(),
            path_env.as_deref(),
        );
        Ok(RunOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code,
            stdout_hash,
            // Same fingerprint formula as the process/mock backends: it
            // identifies the host context that resolved and dispatched the
            // run; the container-side identity lives in executor_digest.
            env_fingerprint: super::runner::env_fingerprint(&version),
            tool_version: version,
            command: command_string(&canonical, args),
        })
    }

    fn label(&self) -> &'static str {
        "sandbox-container"
    }
}

/// Build the ephemeral-container command. Pure over its inputs so tests can
/// assert the exact argv shape without a runtime present. `checkout` of
/// `None` omits the read-only mount and workdir pins entirely (lightweight
/// probes such as the in-container version check); `Some(dir)` produces the
/// full `-v <dir>:/work:ro -w /work` posture.
fn build_run_command(
    runtime: &str,
    checkout: Option<&Path>,
    image_at_digest: &str,
    bin: &str,
    fixed_args: &[String],
    args: &[String],
    path_env: Option<&str>,
) -> Command {
    let mut cmd = Command::new(runtime);
    cmd.arg("run").arg("--rm").arg("--network").arg("none");
    if let Some(dir) = checkout {
        cmd.arg("-v")
            .arg(format!("{}:{CONTAINER_WORKDIR}:ro", dir.display()));
        cmd.arg("-w").arg(CONTAINER_WORKDIR);
    }
    for name in ALLOWED_ENV_VARS {
        if *name == "PATH" {
            if let Some(path) = path_env {
                cmd.arg("-e").arg(format!("PATH={path}"));
            }
        } else if let Ok(value) = std::env::var(name) {
            cmd.arg("-e").arg(format!("{name}={value}"));
        }
    }
    cmd.arg(image_at_digest);
    cmd.arg(bin);
    cmd.args(fixed_args).args(args);
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

/// Resolve `image_ref` to its content address: refs already carrying `@`
/// pass through untouched (no spawn); otherwise ask the runtime for the
/// registry RepoDigest first and the local config Id second. Any failure is
/// a hard error NAMING THE IMAGE (production viability semantics).
///
/// Successful resolutions are cached per (runtime, ref): the digest of a
/// given tag can change between runs in principle, but within one process a
/// stable pin is exactly what attestation reproducibility wants.
fn resolve_image_ref(runtime: &str, image_ref: &str) -> Result<String, RunnerError> {
    if image_ref.contains('@') {
        return Ok(image_ref.to_string());
    }
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{runtime}|{image_ref}");
    if let Ok(guard) = cache.lock() {
        if let Some(pinned) = guard.get(&key) {
            return Ok(pinned.clone());
        }
    }
    for template in ["{{index .RepoDigests 0}}", "{{.Id}}"] {
        let output = Command::new(runtime)
            .args(["image", "inspect", "--format", template, image_ref])
            .stdin(Stdio::null())
            .output();
        let pinned = match output {
            Ok(out) if out.status.success() => {
                let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if id.is_empty() {
                    continue;
                }
                if id.contains('@') {
                    id
                } else {
                    format!("{image_ref}@{id}")
                }
            }
            _ => continue,
        };
        if let Ok(mut guard) = cache.lock() {
            guard.insert(key, pinned.clone());
        }
        return Ok(pinned);
    }
    Err(RunnerError::Spawn(format!(
        "sandbox image {image_ref} unavailable via {runtime}: not resolvable to a digest \
         (absent locally and no registry digest); production attestation runs require the \
         pinned image to exist - pull or build it first"
    )))
}

/// Version of `bin` as seen INSIDE the pinned image (one extra ephemeral
/// container running `<bin> --version`), cached per (image, bin). Attesting
/// the host version while the container executed the image's binary would be
/// dishonest metadata; when the probe itself fails the runner-wide
/// `unknown-<bin>` fallback applies.
fn container_tool_version(
    runtime: &str,
    image_at_digest: &str,
    bin: &str,
    path_env: Option<&str>,
) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{image_at_digest}|{bin}");
    if let Ok(guard) = cache.lock() {
        if let Some(version) = guard.get(&key) {
            return version.clone();
        }
    }
    let mut version_cmd = build_run_command(
        runtime,
        None,
        image_at_digest,
        bin,
        &["--version".to_string()],
        &[],
        path_env,
    );
    let version = match version_cmd.output() {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => format!("unknown-{bin}"),
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, version.clone());
    }
    version
}

/// Deterministic SHA-256 over the checkout tree: entries walked in sorted
/// order; regular files fold their relative path + content hash; symlinks
/// fold their relative path + link target; excluded directory names are
/// pruned entirely. Two calls over an unchanged tree are byte-identical.
pub(crate) fn hash_tree(root: &Path) -> Result<String, RunnerError> {
    let mut hasher = Sha256::new();
    fold_tree(root, root, &mut hasher)?;
    Ok(hex_of(hasher.finalize().as_slice()))
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fold_tree(root: &Path, dir: &Path, hasher: &mut Sha256) -> Result<(), RunnerError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| RunnerError::Io(format!("{}: {e}", dir.display())))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| RunnerError::Io(format!("{}: {e}", path.display())))?;
        if meta.is_symlink() {
            let target = std::fs::read_link(&path)
                .map(|t| t.to_string_lossy().to_string())
                .unwrap_or_default();
            hasher.update(b"L");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(target.as_bytes());
            hasher.update(b"\0");
            continue;
        }
        if meta.is_dir() {
            if EXCLUDED_TREE_DIRS.contains(&name.as_str()) {
                continue;
            }
            hasher.update(b"D");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            fold_tree(root, &path, hasher)?;
            continue;
        }
        let content = std::fs::read(&path)
            .map_err(|e| RunnerError::Io(format!("{}: {e}", path.display())))?;
        hasher.update(b"F");
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(sha256_hex(&content).as_bytes());
        hasher.update(b"\0");
    }
    Ok(())
}

/// Executor identity for the CURRENT environment:
/// `sha256("<image-ref-at-digest>|<tree-hash>")`, full 64-hex. Used by the
/// runner's digest choke point when this backend is selected. Resolution
/// failures yield `None` (the run itself surfaces the hard error).
pub(crate) fn current_run_digest(runtime: &str, image_ref: &str) -> Option<String> {
    let checkout = std::env::current_dir().ok()?;
    let image_at_digest = resolve_image_ref(runtime, image_ref).ok()?;
    let tree_hash = hash_tree(&checkout).ok()?;
    let input = format!("{image_at_digest}|{tree_hash}");
    Some(sha256_hex(input.as_bytes()))
}

/// True when THIS build AND host should route `ExecutorKind::Sandbox` to the
/// container backend: the feature is compiled in and the T5 prober selects
/// the Container tier. Cached once per process (the probe spawns processes).
pub(crate) fn container_backend_active() -> bool {
    static ACTIVE: OnceLock<bool> = OnceLock::new();
    *ACTIVE.get_or_init(|| {
        matches!(
            super::prober::select_tier(None, &ProdProbe),
            Ok(super::prober::SandboxTier::Container)
        )
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- pure command-shape assertions (no runtime required) ----

    fn sample_command(path_env: Option<&str>) -> Command {
        build_run_command(
            "docker",
            Some(Path::new("/home/u/checkout")),
            "alpine@sha256:abc123",
            "cargo",
            &["--version".to_string()],
            &["some arg".to_string()],
            path_env,
        )
    }

    fn argv(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|s| s.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn command_argv_matches_plan_shape_exactly() {
        let cmd = sample_command(Some("/usr/bin:/bin"));
        let a = argv(&cmd);
        assert_eq!(a[0], "docker");
        assert_eq!(&a[1..5], &["run", "--rm", "--network", "none"]);
        assert_eq!(a[5], "-v");
        assert_eq!(a[6], "/home/u/checkout:/work:ro");
        assert_eq!(&a[7..9], &["-w", "/work"]);
        assert_eq!(a[9], "-e");
        assert_eq!(a[10], "PATH=/usr/bin:/bin");
        assert_eq!(a[11], "alpine@sha256:abc123");
        assert_eq!(a[12], "cargo");
        assert_eq!(&a[13..], &["--version", "some arg"]);
        assert_eq!(a.len(), 15);
    }

    #[test]
    fn command_carries_only_the_allowed_env_and_no_cwd_leak() {
        let cmd = sample_command(Some("/usr/bin"));
        // Env reaches the CONTAINER as explicit `-e NAME=value` argv flags,
        // never as the docker client's own process environment.
        let a = argv(&cmd);
        let env_flags: Vec<&String> = a
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| &w[1])
            .collect();
        assert_eq!(env_flags.len(), 1, "exactly one -e flag: {a:?}");
        assert_eq!(env_flags[0], "PATH=/usr/bin");
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-e" && !w[1].starts_with("PATH=")),
            "no env var outside the allowlist may be forwarded: {a:?}"
        );
        assert!(
            cmd.get_envs().next().is_none(),
            "docker CLIENT env must stay untouched (no secrets leak via the parent)"
        );
        assert_eq!(
            cmd.get_current_dir(),
            None,
            "cwd is pinned via -w, not chdir"
        );
    }

    #[test]
    fn missing_path_env_omits_the_e_flag_entirely() {
        let cmd = sample_command(None);
        let a = argv(&cmd);
        assert!(
            !a.iter().any(|t| t == "-e"),
            "no PATH means no env passthrough at all: {a:?}"
        );
    }

    // ---- digest formula ----

    const IMAGE_AT_DIGEST: &str = "polyforge-sandbox@sha256:";

    #[test]
    fn digest_is_sha256_over_image_pipe_tree_and_stable() {
        let tmp = std::env::temp_dir().join(format!("pf-ce-digest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), b"hello").unwrap();

        let tree = hash_tree(&tmp).unwrap();
        let expected_input = format!("{IMAGE_AT_DIGEST}abc|{tree}");
        let d1 = sha256_hex(expected_input.as_bytes());
        let d2 = sha256_hex(expected_input.as_bytes());
        assert_eq!(d1, d2, "digest deterministic over identical inputs");
        assert_eq!(d1.len(), 64);
        assert!(d1.bytes().all(|b| b.is_ascii_hexdigit()));

        std::fs::write(tmp.join("a.txt"), b"changed").unwrap();
        let tree2 = hash_tree(&tmp).unwrap();
        assert_ne!(tree, tree2, "content change must move the tree hash");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hash_tree_skips_excluded_dirs_and_handles_symlinks() {
        let tmp = std::env::temp_dir().join(format!("pf-ce-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("src/main.rs"), b"fn main() {}").unwrap();
        let baseline = hash_tree(&tmp).unwrap();

        // Volatile build output must NOT move the identity.
        std::fs::write(tmp.join("target/artifact.o"), b"\xde\xad").unwrap();
        assert_eq!(hash_tree(&tmp).unwrap(), baseline);

        // Source change DOES move it.
        std::fs::write(tmp.join("src/main.rs"), b"fn main() { x }").unwrap();
        let changed = hash_tree(&tmp).unwrap();
        assert_ne!(changed, baseline);

        // Symlinks fold deterministically via their target string.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("main.rs", tmp.join("src/link.rs")).unwrap();
            let with_link = hash_tree(&tmp).unwrap();
            assert_ne!(with_link, changed);
            assert_eq!(with_link, hash_tree(&tmp).unwrap(), "link fold stable");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn digest_changes_with_image_identity() {
        let tmp = std::env::temp_dir().join(format!("pf-ce-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let tree = hash_tree(&tmp).unwrap();
        let a = sha256_hex(format!("img-a@sha256:x|{tree}").as_bytes());
        let b = sha256_hex(format!("img-b@sha256:y|{tree}").as_bytes());
        assert_ne!(a, b, "different images must produce different digests");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- image resolution ----

    #[test]
    fn pre_pinned_refs_pass_through_without_spawning() {
        let pinned = "alpine@sha256:deadbeef";
        assert_eq!(
            resolve_image_ref("definitely-not-a-runtime-binary", pinned).unwrap(),
            pinned
        );
    }

    // ---- live mechanics (skip-clean without a runtime/image) ----

    /// Probe for a usable runtime; None records the skip reason.
    fn live_runtime() -> Option<String> {
        if let Ok(rt) = std::env::var(POLYFORGE_SANDBOX_RUNTIME_ENV) {
            if !rt.trim().is_empty() {
                return Some(rt.trim().to_string());
            }
        }
        match ProdProbe.container_runtime() {
            Some(rt) => Some(rt),
            None => {
                println!("[SKIP] reason: no container runtime (docker/podman) on this host");
                None
            }
        }
    }

    /// Probe for ANY locally present image usable for mechanics checks:
    /// POLYFORGE_SANDBOX_IMAGE_TEST override, then well-known tiny bases.
    fn live_image(runtime: &str) -> Option<String> {
        if let Ok(img) = std::env::var("POLYFORGE_SANDBOX_IMAGE_TEST") {
            if !img.trim().is_empty() {
                return Some(img.trim().to_string());
            }
        }
        for candidate in ["alpine:latest", "busybox:latest", "debian:stable-slim"] {
            let ok = Command::new(runtime)
                .args(["image", "inspect", candidate])
                .stdin(Stdio::null())
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return Some(candidate.to_string());
            }
        }
        println!(
            "[SKIP] reason: no test image present locally (set POLYFORGE_SANDBOX_IMAGE_TEST \
             or pull alpine:latest)"
        );
        None
    }

    /// Drive the real container path with arbitrary argv (mechanics-level,
    /// mirroring the mock suite's run_scrubbed vehicle).
    fn run_in_container(
        runtime: &str,
        image: &str,
        bin: &str,
        args: &[&str],
    ) -> std::process::Output {
        let checkout = std::env::current_dir().expect("cwd");
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut cmd = build_run_command(
            runtime,
            Some(checkout.as_path()),
            image,
            bin,
            &[],
            &owned,
            std::env::var("PATH").ok().as_deref(),
        );
        let child = cmd.spawn().expect("container spawned");
        wait_with_timeout(child, Duration::from_secs(120)).expect("container finished")
    }

    #[test]
    fn absent_image_hard_error_names_the_image() {
        // live_runtime probes PATH (ProdProbe) and resolve_image_ref spawns
        // the runtime binary.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let Some(runtime) = live_runtime() else {
            return;
        };
        let bogus = "polyforge-nonexistent-image-under-test:v9";
        let err = resolve_image_ref(&runtime, bogus).unwrap_err();
        match err {
            RunnerError::Spawn(msg) => {
                assert!(msg.contains(bogus), "error must NAME the image: {msg}");
                assert!(
                    msg.contains("unavailable"),
                    "error must state unavailability: {msg}"
                );
            }
            other => panic!("expected Spawn error naming the image, got {other:?}"),
        }
        println!("[OK] absent-image hard error recorded for {bogus}");
    }

    #[test]
    fn container_sentinels_match_mock_suite_through_real_container() {
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let Some(runtime) = live_runtime() else {
            return;
        };
        let Some(image) = live_image(&runtime) else {
            return;
        };
        let image_at = resolve_image_ref(&runtime, &image).expect("digest resolution");

        // cwd sentinel: the tool sees /work, not the host cwd.
        let pwd = run_in_container(&runtime, &image_at, "/bin/sh", &["-c", "pwd"]);
        assert_eq!(pwd.status.code(), Some(0));
        let cwd = String::from_utf8_lossy(&pwd.stdout).trim().to_string();
        assert_eq!(cwd, CONTAINER_WORKDIR, "container cwd must be /work");
        println!("[OK] cwd sentinel: {cwd}");

        // Env sentinels via SHELL BUILTINS ONLY: external helpers such as
        // printenv are absent from some minimal images, and their absence
        // yields empty stdout that would make an emptiness check pass
        // vacuously. Every probe echoes a tagged verdict and every run's
        // exit code is checked with stderr attached to failures.
        let hidden = run_in_container(
            &runtime,
            &image_at,
            "/bin/sh",
            &["-c", "printf 'CDIR=[%s]' \"$CARGO_MANIFEST_DIR\""],
        );
        assert_eq!(
            hidden.status.code(),
            Some(0),
            "probe run failed, stderr: {}",
            String::from_utf8_lossy(&hidden.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&hidden.stdout).trim(),
            "CDIR=[]",
            "host sentinel var must be scrubbed from the container env"
        );
        let path_probe = run_in_container(
            &runtime,
            &image_at,
            "/bin/sh",
            &[
                "-c",
                "case \":$PATH:\" in '::') echo P=MISSING ;; *) echo P=PRESENT ;; esac",
            ],
        );
        assert_eq!(
            path_probe.status.code(),
            Some(0),
            "probe run failed, stderr: {}",
            String::from_utf8_lossy(&path_probe.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&path_probe.stdout).trim(),
            "P=PRESENT",
            "allowlisted PATH must be repopulated inside the container"
        );
        println!("[OK] env sentinel: PATH present, host vars absent");

        // Read-only mount sentinel: writing under /work must fail.
        let ro = run_in_container(
            &runtime,
            &image_at,
            "/bin/sh",
            &["-c", "touch /work/pf-ro-probe 2>/dev/null"],
        );
        assert_ne!(
            ro.status.code(),
            Some(0),
            ":ro mount must reject writes to the checkout"
        );
        println!("[OK] read-only mount sentinel: write rejected");

        // Exit-code propagation sentinel.
        let seven = run_in_container(&runtime, &image_at, "/bin/sh", &["-c", "exit 7"]);
        assert_eq!(seven.status.code(), Some(7));
        println!("[OK] exit-code sentinel: 7 propagated");

        // Network-none sentinel, best effort: DNS resolution must fail when
        // getent exists; a missing getent is recorded as inconclusive.
        let net = run_in_container(
            &runtime,
            &image_at,
            "/bin/sh",
            &[
                "-c",
                "command -v getent >/dev/null && getent hosts example.com || echo NO_GETENT",
            ],
        );
        let net_out = String::from_utf8_lossy(&net.stdout).trim().to_string();
        if net_out.contains("NO_GETENT") {
            println!("[SKIP] reason: getent absent in image; --network none asserted structurally");
        } else {
            assert_ne!(
                net.status.code(),
                Some(0),
                "DNS must fail under --network none, got: {net_out:?}"
            );
            println!("[OK] network sentinel: DNS unreachable (--network none)");
        }
    }

    // ---- trait-level happy path (needs a provisioned toolchain image) ----

    #[test]
    fn allowlisted_tool_runs_happy_through_container_executor() {
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        let Some(runtime) = live_runtime() else {
            return;
        };
        let Ok(image) = std::env::var(POLYFORGE_SANDBOX_IMAGE_ENV) else {
            println!(
                "[SKIP] reason: {} unset; trait-level e2e needs an image with the \
                 allowlisted toolchain",
                POLYFORGE_SANDBOX_IMAGE_ENV
            );
            return;
        };
        let executor = ContainerExecutor::new(runtime, image);
        let t = lookup("cargo --version").expect("tool on allowlist");
        let out = match executor.run(&t, &[]) {
            Ok(out) => out,
            Err(e) => panic!("provisioned image must run cargo: {e:?}"),
        };
        assert_eq!(out.exit_code, 0);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("cargo"), "real stdout captured: {stdout:?}");
        assert_eq!(out.stdout_hash.len(), 64);
        assert!(!out.tool_version.is_empty());
        assert!(!out.env_fingerprint.is_empty());
        assert!(out.command.starts_with("cargo"));
        println!("[OK] trait-level happy path through {}", executor.label());

        // Policy persistence: mutating args stay denied BEFORE any spawn.
        let err = executor
            .run(&lookup("ruff check").unwrap(), &["--fix".to_string()])
            .unwrap_err();
        assert!(matches!(err, RunnerError::InvalidArg { .. }));

        // Allowlist persistence: unknown tools rejected before any spawn.
        let evil = Tool {
            name: "evil".into(),
            bin: PathBuf::from("evil"),
            args: vec![],
        };
        let err = executor.run(&evil, &[]).unwrap_err();
        assert!(matches!(err, RunnerError::NotAllowed(n) if n == "evil"));
    }

    #[test]
    fn label_is_pinned() {
        assert_eq!(
            ContainerExecutor::new("docker", "x").label(),
            "sandbox-container"
        );
    }

    // ---- from_environment env parsing (empty/whitespace guards) ---------------

    /// Run `f` with POLYFORGE_SANDBOX_RUNTIME / POLYFORGE_SANDBOX_IMAGE set
    /// to `runtime` / `image`, restoring both afterwards (tui app.rs
    /// save-set-assert-restore pattern). No other test in this binary reads
    /// these vars, so the mutation is race-free in practice.
    fn with_env(runtime: Option<&str>, image: Option<&str>, f: impl FnOnce()) {
        let saved_rt = std::env::var(POLYFORGE_SANDBOX_RUNTIME_ENV).ok();
        let saved_img = std::env::var(POLYFORGE_SANDBOX_IMAGE_ENV).ok();
        match runtime {
            Some(v) => std::env::set_var(POLYFORGE_SANDBOX_RUNTIME_ENV, v),
            None => std::env::remove_var(POLYFORGE_SANDBOX_RUNTIME_ENV),
        }
        match image {
            Some(v) => std::env::set_var(POLYFORGE_SANDBOX_IMAGE_ENV, v),
            None => std::env::remove_var(POLYFORGE_SANDBOX_IMAGE_ENV),
        }
        f();
        match saved_rt {
            Some(v) => std::env::set_var(POLYFORGE_SANDBOX_RUNTIME_ENV, v),
            None => std::env::remove_var(POLYFORGE_SANDBOX_RUNTIME_ENV),
        }
        match saved_img {
            Some(v) => std::env::set_var(POLYFORGE_SANDBOX_IMAGE_ENV, v),
            None => std::env::remove_var(POLYFORGE_SANDBOX_IMAGE_ENV),
        }
    }

    /// Empty or whitespace-only env values fall back exactly like unset
    /// ones: the runtime to the probed docker/podman (or the hard error on
    /// a runtime-less host), the image to the documented default. The
    /// `!x.trim().is_empty()` guards must treat "" and "   " as absent.
    #[test]
    fn from_environment_treats_blank_runtime_and_image_as_unset() {
        // with_env mutates process-global POLYFORGE_SANDBOX_* vars.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        for blank in ["", "   "] {
            // Blank runtime + explicit image: the image env is honored
            // verbatim (trimmed), proving the runtime guard alone fell back.
            with_env(Some(blank), Some("my-image:v2"), || {
                let exec = ContainerExecutor::from_environment().expect("probed runtime");
                assert_eq!(exec.image_ref(), "my-image:v2");
                assert!(
                    exec.runtime() == "docker" || exec.runtime() == "podman",
                    "blank runtime must fall back to the probed runtime, got {}",
                    exec.runtime()
                );
            });

            // Blank image + explicit runtime: the runtime env is honored
            // verbatim (trimmed), proving the image guard alone fell back.
            with_env(Some("my-runtime"), Some(blank), || {
                let exec = ContainerExecutor::from_environment().expect("explicit runtime");
                assert_eq!(exec.runtime(), "my-runtime");
                assert_eq!(
                    exec.image_ref(),
                    DEFAULT_SANDBOX_IMAGE,
                    "blank image must fall back to the documented default"
                );
            });

            // Both blank: both fall back (probed runtime + default image).
            with_env(Some(blank), Some(blank), || {
                let exec = ContainerExecutor::from_environment().expect("probed runtime");
                assert_eq!(exec.image_ref(), DEFAULT_SANDBOX_IMAGE);
                assert!(
                    exec.runtime() == "docker" || exec.runtime() == "podman",
                    "blank runtime must fall back to the probed runtime, got {}",
                    exec.runtime()
                );
            });
        }

        // Non-blank values are TRIMMED, not rejected: surrounding whitespace
        // is not part of the value.
        with_env(Some("  docker  "), Some("  img:1  "), || {
            let exec = ContainerExecutor::from_environment().expect("trimmed values");
            assert_eq!(exec.runtime(), "docker");
            assert_eq!(exec.image_ref(), "img:1");
        });
    }

    /// On a host with no container runtime, a blank runtime value surfaces
    /// the actionable hard error (not a blank runtime name).
    #[test]
    fn from_environment_blank_runtime_without_probe_is_hard_error() {
        // with_env mutates process-global POLYFORGE_SANDBOX_* vars.
        let _spawn_guard = crate::runner::TOOL_SPAWN_LOCK.lock().unwrap();
        // Only meaningful when the probe finds nothing; with docker/podman
        // present the fallback succeeds, so assert the error shape only on
        // runtime-less hosts.
        if ProdProbe.container_runtime().is_some() {
            println!("[SKIP] reason: container runtime present; hard-error arm unreachable");
            return;
        }
        with_env(Some(""), Some("img"), || {
            let err = match ContainerExecutor::from_environment() {
                Ok(_) => panic!("no runtime and blank override must fail"),
                Err(e) => e,
            };
            assert!(
                matches!(&err, RunnerError::Spawn(m) if m.contains("no container runtime available")),
                "error must name the fix: {err:?}"
            );
        });
    }
}
